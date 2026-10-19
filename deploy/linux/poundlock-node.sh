#!/bin/bash
# poundlock Linux node installer. Download from your controller's Machines page.
set -euo pipefail
umask 077
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
fail() { printf '%s\n' "$*" >&2; exit 1; }
usage() {
  cat <<'HELP'
Install: sudo bash poundlock-node.sh install --binary FILE --sha256 HASH --config node.toml [--user LOGIN]
Upgrade: sudo bash poundlock-node.sh upgrade --binary FILE --sha256 HASH
Move an existing node to its own system account: sudo bash poundlock-node.sh account
  (or back to a login account: sudo bash poundlock-node.sh account --user LOGIN)
Diagnose: sudo bash poundlock-node.sh doctor
Rollback: sudo bash poundlock-node.sh rollback
Resume after interrupted maintenance: sudo bash poundlock-node.sh resume
Remove: sudo bash poundlock-node.sh remove
Install prompts for the one-use enrollment token without storing it in shell history.
The node runs as its own system account, poundlock-node, which cannot read home
folders. Pass --user LOGIN only when its models or runtime live in that login's home.
Stop submitting work and wait for existing requests/jobs before upgrade, rollback or removal.
HELP
}
operation=${1:-help}; shift || true
case "$operation" in help|--help|-h) usage; exit 0;; install|upgrade|doctor|rollback|remove|resume|account) ;; *) usage; exit 1;; esac
[[ $(uname -s) == Linux ]] || fail 'This installer requires Linux.'
[[ $EUID == 0 ]] || fail 'Run this installer with sudo.'
[[ -d /run/systemd/system ]] || fail 'A running systemd system is required.'
for tool in systemctl systemd-inhibit runuser sha256sum install realpath getent flock pkcheck useradd; do command -v "$tool" >/dev/null || fail "Required command missing: $tool"; done
exec 9>/run/lock/poundlock-node-install.lock
flock -n 9 || fail 'Another node installation operation is running.'
node_user= binary= digest= config=
while (( $# )); do
  [[ $# -ge 2 ]] || fail "Missing value for $1"
  case "$1" in --user) node_user=$2;; --binary) binary=$2;; --sha256) digest=$2;; --config) config=$2;; *) fail "Unknown option: $1";; esac
  shift 2
done
conf=/etc/poundlock
data=/var/lib/poundlock
system_user=poundlock-node
unit=/etc/systemd/system/poundlock-node.service
rule=/etc/polkit-1/rules.d/60-poundlock-node.rules
for path in "$conf" "$data" "$unit" "$rule"; do [[ ! -L $path ]] || fail "Refusing symlink: $path"; done
if [[ -f $conf/install-user ]]; then
  [[ $(stat -c %u "$conf") == 0 && $(stat -c %a "$conf") == 755 ]] || fail 'Installer configuration directory has unexpected ownership or permissions.'
  saved_user=$(cat "$conf/install-user")
  if [[ $operation == account ]]; then
    target_user=${node_user:-$system_user}
  else
    [[ -z $node_user || $node_user == "$saved_user" ]] || fail 'Existing installation uses a different account.'
  fi
  node_user=$saved_user
else
  [[ $operation == install ]] || fail 'No managed node installation found.'
  node_user=${node_user:-$system_user}
  for path in "$conf" "$data" "$unit" "$rule"; do [[ ! -e $path ]] || fail "Existing unmanaged path: $path. Follow the migration instructions first."; done
fi
# The node's own account: no login, no home folder of its own (L11).
ensure_system_user() {
  if ! getent passwd "$system_user" >/dev/null; then
    useradd --system --user-group --home-dir "$data" --no-create-home --shell /usr/sbin/nologin "$system_user"
  fi
}
[[ $node_user != "$system_user" ]] || ensure_system_user
[[ $node_user =~ ^[a-z_][a-z0-9_-]*\$?$ ]] || fail 'Run with sudo from your normal login, or provide an existing Linux login with --user.'
node_uid=$(id -u "$node_user")
node_gid=$(id -g "$node_user")
[[ $node_uid != 0 ]] || fail 'The node must run as a non-root account.'
as_node() { runuser -u "$node_user" -- "$@"; }
node_program=$data/poundlock
node() { as_node "$node_program" "$@" --config "$conf/node.toml" --credential-file "$data/node-credential.json"; }
idle() {
  # Atomically block new runtime actions only after all old work has released.
  node node-pause
}
# Maintenance stops and starts the service in quick succession; clear systemd's
# start limit first, so that alone cannot leave the node failed.
start_service() {
  systemctl reset-failed poundlock-node.service 2>/dev/null || true
  systemctl start poundlock-node.service
}
# The polkit rule and systemd unit for the current account. A login account keeps
# its home read-only; the system account cannot see home folders at all (L11).
write_service() {
  local home_access=read-only groups=
  if [[ $node_user == "$system_user" ]]; then
    home_access=true
    # Model servers open the GPU's render node; nothing else (cameras, framebuffers).
    if getent group render >/dev/null; then groups+=" render"; fi
  fi
  install -d -m 755 /etc/polkit-1/rules.d
  cat > "$rule" <<POLKIT
// Managed by poundlock-node.sh. Only permits sleep inhibition for the node account.
polkit.addRule(function(action, subject) {
  if (subject.user === "$node_user" &&
      (action.id === "org.freedesktop.login1.inhibit-block-sleep" ||
       action.id === "org.freedesktop.login1.inhibit-block-idle")) {
    return polkit.Result.YES;
  }
});
POLKIT
  chmod 644 "$rule"
  cat > "$unit" <<UNIT
[Unit]
Description=poundlock runtime node
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=$node_uid
Group=$node_gid
SupplementaryGroups=${groups# }
ExecStart=$data/poundlock node --config $conf/node.toml --credential-file $data/node-credential.json --data-dir $data
Restart=always
RestartSec=5
TimeoutStopSec=15
KillMode=control-group
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=$home_access
ReadWritePaths=$data
CapabilityBoundingSet=
AmbientCapabilities=
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
RestrictSUIDSGID=true
RestrictRealtime=true
RestrictNamespaces=true
LockPersonality=true
SystemCallArchitectures=native
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
[Install]
WantedBy=multi-user.target
UNIT
  chmod 644 "$unit"
}
if [[ $operation == account ]]; then
  [[ $target_user != "$node_user" ]] || fail "The node already runs as $node_user."
  [[ $target_user =~ ^[a-z_][a-z0-9_-]*\$?$ ]] || fail 'Supply an existing Linux login with --user.'
  # Resolve the account before stopping anything, so a typo leaves the node running.
  [[ $target_user != "$system_user" ]] || ensure_system_user
  target_uid=$(id -u "$target_user" 2>/dev/null) || fail "No account named $target_user."
  target_gid=$(id -g "$target_user")
  [[ $target_uid != 0 ]] || fail 'The node must run as a non-root account.'
  # A hard link the old account planted would hand its target to the new one.
  linked() { [[ -n $(find "$data" -xdev ! -type d -links +1 -print -quit) ]]; }
  ! linked || fail "A file under $data has more than one hard link; remove it before changing accounts."
  previous_uid=$node_uid
  idle
  systemctl stop poundlock-node.service
  ! linked || fail "A file under $data gained a hard link; the node is stopped. Remove it, then run account again."
  node_user=$target_user
  node_uid=$target_uid
  node_gid=$target_gid
  # Hand over only what the old account owned, never following a link.
  find "$data" -xdev -user "$previous_uid" -exec chown -h "$node_uid:$node_gid" {} +
  find "$conf" -xdev -user "$previous_uid" -exec chown -h "$node_uid:$node_gid" {} +
  printf '%s\n' "$node_user" > "$conf/install-user"
  write_service
  systemctl daemon-reload
  start_service
  node node-resume
  if ! node node-doctor; then
    printf 'The node does not pass doctor as %s. Models or a runtime under a home folder need a login account: sudo bash poundlock-node.sh account --user %s\n' "$node_user" "$saved_user" >&2
    exit 1
  fi
  printf 'The node now runs as %s. Test a model with Generate in Setup.\n' "$node_user"
  exit 0
fi
if [[ $operation == resume ]]; then
  start_service
  node node-resume
  node node-doctor
  exit 0
fi
if [[ $operation == doctor ]]; then
  systemctl --no-pager --full status poundlock-node.service || true
  node node-doctor
  exit 0
fi
if [[ $operation == remove ]]; then
  # Revocation is atomic with the controller's active-action check.
  node node-revoke
  systemctl disable --now poundlock-node.service
  rm -f "$unit" "$rule"
  # Delete user-owned state as that user, never through a privileged symlink traversal.
  as_node find "$data" -mindepth 1 -delete
  rmdir "$data"
  rm -f "$conf/node.toml" "$conf/install-user"
  rmdir "$conf"
  systemctl daemon-reload
  printf 'Node removed and credential revoked. Model files and model services were not removed.\n'
  exit 0
fi
if [[ $operation == rollback ]]; then
  [[ -f $data/poundlock.previous ]] || fail 'No previous binary is available.'
  node_program=$data/poundlock.previous
  idle
  as_node "$data/poundlock.previous" node-check --config "$conf/node.toml"
  systemctl stop poundlock-node.service
  as_node cp -- "$data/poundlock.previous" "$data/poundlock.new"
  as_node mv -f -- "$data/poundlock.new" "$data/poundlock"
  start_service
  node_program=$data/poundlock
  node node-resume
  node node-doctor
  printf 'Previous binary restored.\n'
  exit 0
fi
[[ -f $binary && $digest =~ ^[a-fA-F0-9]{64}$ ]] || fail 'Supply the downloaded binary and its SHA-256 from Machines.'
binary=$(realpath "$binary")
[[ $(sha256sum "$binary" | cut -d ' ' -f 1) == "${digest,,}" ]] || fail 'Binary SHA-256 does not match. Download it again.'
if [[ $operation == install ]]; then
  [[ -f $config ]] || fail 'Supply the downloaded node.toml with --config.'
  config=$(realpath "$config")
else
  config=$conf/node.toml
  idle
fi
# Stage with root ownership. Run every downloaded executable as the node account.
stage=$(mktemp -d /tmp/poundlock-install.XXXXXXXX)
trap 'rm -rf -- "$stage"' EXIT
chmod 755 "$stage"
install -m 755 "$binary" "$stage/poundlock"
[[ $(sha256sum "$stage/poundlock" | cut -d ' ' -f 1) == "${digest,,}" ]] || fail 'Binary changed while staging; download again.'
install -m 644 "$config" "$stage/node.toml"
as_node "$stage/poundlock" node-check --require-identity --config "$stage/node.toml"
if [[ ! -f $conf/install-user ]]; then
  install -d -m 755 "$conf"
  install -d -m 700 -o "$node_uid" -g "$node_gid" "$data"
  printf '%s\n' "$node_user" > "$conf/install-user"
  chmod 644 "$conf/install-user"
fi
if systemctl is-active --quiet poundlock-node.service; then idle; systemctl stop poundlock-node.service; fi
if [[ -f $data/poundlock ]]; then as_node cp -- "$data/poundlock" "$data/poundlock.previous"; fi
as_node install -m 755 "$stage/poundlock" "$data/poundlock.new"
as_node mv -f -- "$data/poundlock.new" "$data/poundlock"
install -m 644 "$stage/node.toml" "$conf/node.toml"
write_service
if [[ ! -f $data/node-credential.json ]]; then
  read -rsp 'Enrollment token (input hidden): ' enrollment_token </dev/tty
  printf '\n'
  printf '%s' "$enrollment_token" | node node-enroll
  unset enrollment_token
fi
systemctl daemon-reload
systemctl enable poundlock-node.service
start_service
node node-resume
if ! node node-doctor; then
  printf 'Installed, but validation failed. Run doctor and inspect journalctl -u poundlock-node.service. A failed upgrade can be reverted with rollback.\n' >&2
  exit 1
fi
printf 'Node installed. Confirm Online in Machines, then test the model using Generate in Setup.\n'
