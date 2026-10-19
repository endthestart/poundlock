#!/usr/bin/env bash
set -euo pipefail
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) platform=linux-x86_64 ;;
  Linux/aarch64) platform=linux-aarch64 ;;
  Darwin/arm64) platform=macos-aarch64 ;;
  Darwin/x86_64) platform=macos-x86_64 ;;
  *) echo 'Unsupported build platform' >&2; exit 1 ;;
esac
# Prevent local checkout and Cargo cache paths from entering distributed binaries.
# The bounded BuildKit release worker has observed rust-lld deadlock while linking
# dependency build scripts. Release images select GNU bfd explicitly; normal local
# builds retain the developer's configured linker.
if [[ "${POUNDLOCK_BUILD_LINKER:-}" == bfd ]]; then
  command -v ld.bfd >/dev/null || { echo 'GNU ld.bfd is required for this release build' >&2; exit 1; }
fi
CARGO_ENCODED_RUSTFLAGS=$(python3 - <<'PY'
import os, pathlib
separator = chr(31)
flags = os.environ['CARGO_ENCODED_RUSTFLAGS'].split(separator) if os.environ.get('CARGO_ENCODED_RUSTFLAGS') else os.environ.get('RUSTFLAGS', '').split()
if os.environ.get('POUNDLOCK_BUILD_LINKER') == 'bfd':
    flags += ['-C', 'link-arg=-fuse-ld=bfd']
flags += ['--remap-path-prefix=' + str(pathlib.Path.home()) + '=/build-home',
          '--remap-path-prefix=' + str(pathlib.Path(os.environ.get('RUSTUP_HOME', pathlib.Path.home() / '.rustup'))) + '=/rust',
          '--remap-path-prefix=' + str(pathlib.Path(os.environ.get('CARGO_HOME', pathlib.Path.home() / '.cargo'))) + '=/cargo',
          '--remap-path-prefix=' + str(pathlib.Path.cwd()) + '=.']
print(separator.join(flags))
PY
)
export CARGO_ENCODED_RUSTFLAGS
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
cargo build --release --locked
mkdir -p "dist/$platform"
cp target/release/poundlock "dist/$platform/poundlock"
cargo metadata --locked --format-version 1 --filter-platform "$(rustc -vV | sed -n 's/^host: //p')" > target/release/dependencies.json
python3 scripts/third-party-licenses.py target/release/dependencies.json "dist/$platform/THIRD_PARTY_LICENSES.md"
cp LICENSE NOTICE "dist/$platform/"
"dist/$platform/poundlock" --version
