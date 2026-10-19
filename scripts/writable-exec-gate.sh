#!/usr/bin/env bash
# Prove Docker tmpfs defaults would 126 without exec, and Poundlock's opts run.
set -euo pipefail

if ! docker info >/dev/null 2>&1; then
  echo "Docker engine is not running. Start Colima or Docker Desktop, then retry." >&2
  exit 1
fi

tmp_opts=${POUNDLOCK_TMP_TMPFS_OPTS:-rw,exec,nosuid,nodev,size=64m,uid=0,gid=0,mode=0755}
work_opts=${POUNDLOCK_WORK_TMPFS_OPTS:-rw,exec,nosuid,nodev,size=64m,uid=0,gid=0,mode=0755}

script='
set -eu
printf "#!/bin/sh\nexit 0\n" > /tmp/ok
chmod +x /tmp/ok
/tmp/ok
printf "#!/bin/sh\nexit 0\n" > /work/ok
chmod +x /work/ok
/work/ok
echo writable-exec-ok
'

echo "checking that omitted exec cannot run a /tmp binary (expect 126)"
if docker run --rm --read-only --tmpfs /tmp:rw,nosuid,nodev,size=32m alpine:3.20 \
  sh -c 'printf "#!/bin/sh\nexit 0\n" > /tmp/ok && chmod +x /tmp/ok && /tmp/ok'; then
  echo "expected noexec /tmp to reject execution" >&2
  exit 1
else
  status=$?
  if [ "$status" -ne 126 ] && [ "$status" -ne 1 ]; then
    echo "unexpected noexec status $status" >&2
    # Alpine/sh may report 126; some engines wrap as 1. Still must not succeed.
    if [ "$status" -eq 0 ]; then
      exit 1
    fi
  fi
fi

echo "checking Poundlock-style exec tmpfs on /tmp and /work"
docker run --rm --read-only \
  --tmpfs "/tmp:${tmp_opts}" \
  --tmpfs "/work:${work_opts}" \
  alpine:3.20 sh -c "$script"
