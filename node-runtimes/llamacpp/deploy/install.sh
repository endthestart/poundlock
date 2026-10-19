#!/usr/bin/env bash
# Install or remove poundlock-llamacpp as one self-contained executable (a Python zipapp).
#
#   deploy/install.sh [--prefix DIR] [--python PATH]
#                     [--launchd [--home DIR] [--manifest FILE] [--launch-agents DIR] [--no-load]]
#   deploy/install.sh --uninstall [--prefix DIR] [--launchd] [--launch-agents DIR]
#
# Installs DIR/bin/poundlock-llamacpp and DIR/share/poundlock-llamacpp/examples. It
# never writes a manifest and never touches state, logs or models: those are host
# configuration. --launchd (macOS) also installs and loads the sweep job
# com.poundlock.llamacpp; Linux needs no service because the runtime starts a
# detached sweeper beside its server.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SOURCE="$SCRIPT_DIR/../src/poundlock_llamacpp"
LABEL="com.poundlock.llamacpp"

PREFIX="$HOME/.local"
PYTHON=""
LAUNCHD=0
UNINSTALL=0
NO_LOAD=0
HOME_DIR=""
MANIFEST=""
LAUNCH_AGENTS="$HOME/Library/LaunchAgents"

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; }
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --python) PYTHON="$2"; shift 2 ;;
    --launchd) LAUNCHD=1; shift ;;
    --home) HOME_DIR="$2"; shift 2 ;;
    --manifest) MANIFEST="$2"; shift 2 ;;
    --launch-agents) LAUNCH_AGENTS="$2"; shift 2 ;;
    --no-load) NO_LOAD=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

BIN="$PREFIX/bin/poundlock-llamacpp"
PLIST="$LAUNCH_AGENTS/$LABEL.plist"
DOMAIN="gui/$(id -u)"

if [ "$LAUNCHD" = 1 ] && [ "$(uname -s)" != Darwin ] && [ "$NO_LOAD" != 1 ]; then
  echo "--launchd loads a launchd job and is macOS only; add --no-load to only write the plist" >&2
  exit 2
fi

if [ "$UNINSTALL" = 1 ]; then
  if [ "$LAUNCHD" = 1 ]; then
    [ "$NO_LOAD" = 1 ] || launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    rm -f "$PLIST"
  fi
  rm -f "$BIN"
  rm -rf "$PREFIX/share/poundlock-llamacpp"
  echo "Removed $BIN. State, logs, models and the manifest were left in place."
  exit 0
fi

PYTHON="${PYTHON:-$(command -v python3 || true)}"
[ -n "$PYTHON" ] && [ -x "$PYTHON" ] || { echo "python3 not found; pass --python PATH" >&2; exit 1; }
case "$PYTHON" in /*) ;; *) echo "--python must be an absolute path (the node runs without your PATH)" >&2; exit 2 ;; esac
case "$PYTHON" in *" "*) echo "--python must not contain spaces (it becomes a shebang)" >&2; exit 2 ;; esac
"$PYTHON" -c 'import sys; sys.exit(sys.version_info < (3, 9))' \
  || { echo "$PYTHON is older than Python 3.9" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir "$STAGE/poundlock_llamacpp"
cp "$SOURCE"/*.py "$STAGE/poundlock_llamacpp/"
install -d -m 0755 "$PREFIX/bin" "$PREFIX/share/poundlock-llamacpp/examples"
# Our own entry point: zipapp's -m discards main()'s return, so every exit would be 0.
printf 'from poundlock_llamacpp.cli import main\n\nraise SystemExit(main())\n' > "$STAGE/__main__.py"
"$PYTHON" -m zipapp "$STAGE" -p "$PYTHON" -c -o "$STAGE/poundlock-llamacpp"
install -m 0755 "$STAGE/poundlock-llamacpp" "$BIN"
install -m 0644 "$SCRIPT_DIR"/../examples/* "$PREFIX/share/poundlock-llamacpp/examples/"
echo "Installed $BIN ($("$BIN" --version))"

if [ "$LAUNCHD" = 1 ]; then
  args=()
  [ -z "$HOME_DIR" ] || args+=(--home "$HOME_DIR")
  [ -z "$MANIFEST" ] || args+=(--manifest "$MANIFEST")
  install -d -m 0755 "$LAUNCH_AGENTS"
  "$BIN" ${args[@]+"${args[@]}"} launchd-plist --executable "$BIN" > "$PLIST"
  chmod 0644 "$PLIST"
  if [ "$NO_LOAD" != 1 ]; then
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    launchctl bootstrap "$DOMAIN" "$PLIST"
    launchctl enable "$DOMAIN/$LABEL"
  fi
  echo "Sweep job: $PLIST"
fi

cat <<EOF
Next: copy an example from $PREFIX/share/poundlock-llamacpp/examples to your manifest,
edit the paths, size and SHA-256, then run:
  $BIN --home HOME_DIR --manifest MANIFEST preflight
EOF
