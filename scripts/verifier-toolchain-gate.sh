#!/usr/bin/env bash
# Build the verifier-toolchain stage and run a uv / CPython 3.14 project check.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
tag=${POUNDLOCK_TOOLCHAIN_TAG:-poundlock-verifier-toolchain:local}

if ! docker info >/dev/null 2>&1; then
  echo "Docker engine is not running. Start Colima or Docker Desktop, then retry." >&2
  exit 1
fi

docker build --target verifier-toolchain -t "$tag" "$root"

tmp_opts="rw,exec,nosuid,nodev,size=256m,uid=0,gid=0,mode=0755"
work_opts="rw,exec,nosuid,nodev,size=256m,uid=0,gid=0,mode=0755"

echo "image python 3.14, uv, and writable exec"
docker run --rm --read-only --network none \
  --tmpfs "/tmp:${tmp_opts}" \
  --tmpfs "/work:${work_opts}" \
  --tmpfs "/home/worker:${tmp_opts}" \
  --user 0:0 \
  --entrypoint bash "$tag" -lc '
    set -euo pipefail
    python3.14 -c "import sys; assert sys.version_info[:2] == (3, 14), sys.version"
    uv --version
    printf "#!/bin/sh\nexit 0\n" > /tmp/ok && chmod +x /tmp/ok && /tmp/ok
    printf "#!/bin/sh\nexit 0\n" > /work/ok && chmod +x /work/ok && /work/ok
    echo toolchain-exec-ok
  '

echo "uv.lock project: uv sync and uv run pytest"
docker run --rm \
  --tmpfs "/tmp:${tmp_opts}" \
  --tmpfs "/work:${work_opts}" \
  --tmpfs "/home/worker:${tmp_opts}" \
  --user 0:0 \
  --workdir /work \
  --entrypoint bash "$tag" -lc '
    set -euo pipefail
    export HOME=/home/worker
    export PATH=/usr/local/bin:/usr/bin:/bin
    cat > /work/pyproject.toml << "EOF"
[project]
name = "poundlock-verifier-gate"
version = "0.0.1"
requires-python = ">=3.14"
dependencies = ["pytest>=8"]
EOF
    mkdir -p /work/tests
    cat > /work/tests/test_python.py << "EOF"
import sys

def test_cpython_series():
    assert sys.version_info[:2] == (3, 14)
EOF
    uv lock
    uv sync --frozen
    uv run pytest -q
    echo uv-project-check-ok
  '
