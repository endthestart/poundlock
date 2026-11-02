#!/usr/bin/env python3
"""The version of a main build: the next minor release after Cargo.toml's, as a
prerelease ordered by commit time (UTC), for example 0.4.0-dev.20261030013000.

Nodes install only a higher version, so every main build has to sort above the
last release and the main build before it, and below the release it leads to.
"""
import datetime
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]


def cargo_version() -> str:
    found = re.search(r'^version = "(\d+\.\d+\.\d+)"', (ROOT / "Cargo.toml").read_text(), re.M)
    if not found:
        raise SystemExit("Cargo.toml has no package version")
    return found.group(1)


def dev_version(release: str, commit_time: int) -> str:
    major, minor, _ = (int(part) for part in release.split("."))
    stamp = datetime.datetime.fromtimestamp(commit_time, datetime.timezone.utc)
    return f"{major}.{minor + 1}.0-dev.{stamp:%Y%m%d%H%M%S}"


def is_dev_of(version: str, release: str) -> bool:
    major, minor, _ = (int(part) for part in release.split("."))
    return re.fullmatch(rf"{major}\.{minor + 1}\.0-dev\.\d{{14}}", version) is not None


if __name__ == "__main__":
    when = subprocess.run(["git", "-C", str(ROOT), "show", "-s", "--format=%ct", "HEAD"],
                          capture_output=True, text=True, check=True).stdout.strip()
    print(dev_version(cargo_version(), int(when)))
    sys.exit(0)
