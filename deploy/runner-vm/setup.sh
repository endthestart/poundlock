#!/bin/bash
# Poundlock job runner on its own Debian or Ubuntu VM, with gVisor.
# Run on the VM, from an unpacked release bundle (it uses ../../compose.runner.yaml).
set -euo pipefail
umask 077
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export DEBIAN_FRONTEND=noninteractive
fail() { printf '%s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }
usage() {
  cat <<'HELP'
Install: sudo bash setup.sh install --controller URL --image REF --runner-token-file FILE [--capacity N]
Upgrade: sudo bash setup.sh upgrade --image REF
Diagnose: sudo bash setup.sh doctor
Remove: sudo bash setup.sh remove

URL is how this VM reaches the controller, for example http://192.0.2.10:7331.
REF is the controller's own image, pinned by digest (POUNDLOCK_IMAGE in its .env).
FILE is a copy of the controller's runner token (runtime-secrets volume,
/run/poundlock/runner-token in the controller container). It is installed
readable only by the runner and never printed.

If the image is in a private registry, run docker login for it first.

Install adds Docker Engine from Docker's apt repository when this VM has no
Docker 28 or newer, and gVisor's runsc from gVisor's apt repository, then
registers runsc with Docker and starts the runner with job containers under
gVisor. Give the VM no access to the controller host's Docker or files.
HELP
}
operation=${1:-help}; shift || true
case "$operation" in help|--help|-h) usage; exit 0;; install|upgrade|doctor|remove) ;; *) usage; exit 1;; esac
[[ $(uname -s) == Linux ]] || fail 'This script runs on the Linux VM that will run jobs.'
[[ $EUID == 0 ]] || fail 'Run it with sudo.'
for tool in apt-get curl install stat grep sed; do command -v "$tool" >/dev/null || fail "Required command missing: $tool"; done
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
home=/opt/poundlock-runner
secrets=/etc/poundlock-runner
token_path=$secrets/runner-token
env_file=$home/runner.env
runner_uid=10001
controller= image= token_file= capacity=1
while (( $# )); do
  [[ $# -ge 2 ]] || fail "Missing value for $1"
  case "$1" in
    --controller) controller=$2;; --image) image=$2;; --runner-token-file) token_file=$2;; --capacity) capacity=$2;;
    *) fail "Unknown option: $1";;
  esac
  shift 2
done

compose() { docker compose -f "$home/compose.runner.yaml" --env-file "$env_file" "$@"; }

check_image() {
  [[ $1 =~ ^[a-z0-9][a-z0-9./_:-]*@sha256:[0-9a-f]{64}$ ]] || fail 'The image must be a registry reference pinned by @sha256: digest.'
}

# Docker's runtimes, read whole: `grep -q` on a pipe can end the writer early,
# and under pipefail that reads as a failure even when it matched.
has_runsc() {
  local runtimes
  runtimes=$(docker info --format '{{json .Runtimes}}' 2>/dev/null || true)
  [[ $runtimes == *'"runsc"'* ]]
}

docker_major() {
  local version
  version=$(docker version --format '{{.Server.Version}}' 2>/dev/null || true)
  printf '%s\n' "${version%%.*}"
}

install_docker() {
  local major
  # Packages and their scripts expect the usual umask, not this script's 077.
  umask 022
  major=$(docker_major)
  if [[ $major =~ ^[0-9]+$ ]] && (( major >= 28 )); then
    say "Docker Engine $major is already installed."
    docker compose version >/dev/null 2>&1 || fail 'Docker Compose v2 is missing: install the docker-compose-plugin package and rerun.'
    umask 077
    return
  fi
  # shellcheck disable=SC1091
  . /etc/os-release
  case "${ID:-}" in debian|ubuntu) ;; *) fail "Docker installation supports Debian and Ubuntu, not ${ID:-unknown}. Install Docker Engine 28+ yourself and rerun.";; esac
  say "Installing Docker Engine from download.docker.com ($ID ${VERSION_CODENAME:-})."
  apt-get update -q
  apt-get install -y -q ca-certificates curl gnupg
  install -d -m 0755 /etc/apt/keyrings
  curl -fsSL "https://download.docker.com/linux/$ID/gpg" -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  printf 'deb [arch=%s signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/%s %s stable\n' \
    "$(dpkg --print-architecture)" "$ID" "$VERSION_CODENAME" > /etc/apt/sources.list.d/docker.list
  chmod a+r /etc/apt/sources.list.d/docker.list
  apt-get update -q
  apt-get install -y -q docker-ce docker-ce-cli containerd.io docker-compose-plugin
  systemctl enable --now docker
  major=$(docker_major)
  [[ $major =~ ^[0-9]+$ ]] && (( major >= 28 )) || fail 'Docker Engine 28 or newer did not start.'
  umask 077
}

install_gvisor() {
  umask 022
  if ! command -v runsc >/dev/null; then
    say 'Installing gVisor (runsc) from gvisor.dev.'
    apt-get install -y -q ca-certificates curl gnupg
    curl -fsSL https://gvisor.dev/archive.key | gpg --dearmor --yes -o /usr/share/keyrings/gvisor-archive-keyring.gpg
    # apt reads keyrings as its own unprivileged user.
    chmod a+r /usr/share/keyrings/gvisor-archive-keyring.gpg
    printf 'deb [arch=%s signed-by=/usr/share/keyrings/gvisor-archive-keyring.gpg] https://storage.googleapis.com/gvisor/releases release main\n' \
      "$(dpkg --print-architecture)" > /etc/apt/sources.list.d/gvisor.list
    chmod a+r /etc/apt/sources.list.d/gvisor.list
    apt-get update -q
    apt-get install -y -q runsc
  fi
  if ! has_runsc; then
    runsc install
    systemctl restart docker
  fi
  has_runsc || fail 'Docker does not list the runsc runtime after installing gVisor.'
  umask 077
}

write_env() {
  local docker_gid
  docker_gid=$(stat -c '%g' /var/run/docker.sock)
  {
    printf 'POUNDLOCK_IMAGE=%s\n' "$image"
    printf 'POUNDLOCK_CONTROLLER_URL=%s\n' "$controller"
    printf 'POUNDLOCK_RUNNER_TOKEN_PATH=%s\n' "$token_path"
    printf 'POUNDLOCK_DOCKER_GID=%s\n' "$docker_gid"
    printf 'POUNDLOCK_RUNNER_CAPACITY=%s\n' "$capacity"
    printf 'POUNDLOCK_WORKER_RUNTIME=runsc\n'
  } > "$env_file.new"
  chmod 0600 "$env_file.new"
  mv "$env_file.new" "$env_file"
}

wait_connected() {
  local waited=0
  while (( waited < 90 )); do
    if compose logs --no-color runner 2>/dev/null | grep -F 'runner connected' >/dev/null; then
      say 'The runner is connected to the controller.'
      return
    fi
    sleep 3; waited=$((waited + 3))
  done
  compose logs --no-color --tail 40 runner >&2 || true
  fail 'The runner did not connect within 90 seconds; see its log above.'
}

case "$operation" in
  install)
    [[ -n $controller && -n $image && -n $token_file ]] || { usage; exit 1; }
    [[ $controller =~ ^https?://[^[:space:]]+$ ]] || fail 'The controller URL must start with http:// or https://.'
    check_image "$image"
    [[ $capacity =~ ^[1-9][0-9]?$ ]] || fail 'Capacity must be a whole number from 1 to 99.'
    [[ -f $token_file && -s $token_file ]] || fail "No runner token in $token_file."
    [[ -f $here/../../compose.runner.yaml ]] || fail 'Run this script from an unpacked release bundle; compose.runner.yaml is missing.'
    curl -fsS --max-time 10 "$controller/healthz" >/dev/null || fail "This VM cannot reach the controller at $controller/healthz."
    install_docker
    install_gvisor
    install -d -m 0755 "$home"
    # chown, not install -o: uutils' install (Ubuntu 26.04) refuses a numeric
    # owner that has no account, and the runner's UID has none on this VM.
    install -d -m 0700 "$secrets"
    chown "$runner_uid:$runner_uid" "$secrets"
    install -m 0400 "$token_file" "$token_path"
    chown "$runner_uid:$runner_uid" "$token_path"
    install -m 0644 "$here/../../compose.runner.yaml" "$home/compose.runner.yaml"
    write_env
    docker pull -q "$image" >/dev/null || fail "Could not pull $image; if its registry is private, run docker login first."
    # Prove gVisor runs this image before handing it jobs.
    docker run --rm --runtime=runsc --network none --entrypoint true "$image" || fail 'The image does not start under gVisor on this VM.'
    compose up -d
    wait_connected
    say "Jobs now run on this VM under gVisor. On the controller host set POUNDLOCK_LOCAL_RUNNERS=0"
    say "and POUNDLOCK_REQUIRED_WORKER_RUNTIME=runsc in .env, then run: docker compose up -d"
    ;;
  upgrade)
    [[ -n $image ]] || { usage; exit 1; }
    check_image "$image"
    [[ -f $env_file ]] || fail 'The runner is not installed here.'
    sed -i "s|^POUNDLOCK_IMAGE=.*|POUNDLOCK_IMAGE=$image|" "$env_file"
    docker pull -q "$image" >/dev/null
    docker run --rm --runtime=runsc --network none --entrypoint true "$image" || fail 'The image does not start under gVisor on this VM.'
    compose up -d
    wait_connected
    ;;
  doctor)
    say "Docker Engine: $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo 'not running')"
    say "Runtimes: $(docker info --format '{{range $name, $_ := .Runtimes}}{{$name}} {{end}}' 2>/dev/null)"
    say "runsc: $(command -v runsc >/dev/null && runsc --version 2>/dev/null | sed -n 1p || echo 'not installed')"
    if [[ -f $env_file ]]; then
      say "Controller: $(grep '^POUNDLOCK_CONTROLLER_URL=' "$env_file" | cut -d= -f2-)"
      compose ps
      compose logs --no-color --tail 20 runner || true
    else
      say 'The runner is not installed here.'
    fi
    ;;
  remove)
    if [[ -f $env_file ]]; then
      say 'Stopping the runner. Finish or cancel its jobs first; a stopped runner fails jobs it was running.'
      compose down
    fi
    rm -f "$token_path" "$env_file" "$home/compose.runner.yaml"
    rmdir "$secrets" "$home" 2>/dev/null || true
    say 'Removed the runner. Its result volume (poundlock-runner_runner-results) is kept; remove it with docker volume rm once nothing needs it.'
    ;;
esac
