"""Model file, server binary and key file checks, with a cache so hashing happens once."""

from __future__ import annotations

import plistlib
import re
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from .errors import InfrastructureError
from .manifest import Profile, read_api_key
from .state import Store, utc_now
from .system import System

# llama.cpp: "version: 11192 (a1b2c3d4e)". Forks: "version: 0.4.0-dev (build 10830, commit 465e49b9c)".
BUILD_PATTERNS = (
    re.compile(r"\bbuild\s+(\d+)(?:,\s*commit\s+([0-9a-f]{7,40}))?"),
    re.compile(r"\bversion:\s*(\d+)\s*\(([0-9a-f]{7,40})\)"),
)


def parse_build(version_output: str) -> Tuple[Optional[int], Optional[str]]:
    for pattern in BUILD_PATTERNS:
        match = pattern.search(version_output)
        if match:
            return int(match.group(1)), match.group(2)
    return None, None


def version_line(version_output: str) -> str:
    """The line naming the version; some builds print GPU probe lines before it."""
    for line in version_output.splitlines():
        if "version" in line or "build" in line:
            return line.strip()[:500]
    return version_output.strip()[:500]


def application_version(executable: Path) -> Optional[Dict[str, Optional[str]]]:
    """Name and version of the macOS .app bundle that ships the server, if any."""
    for parent in executable.parents:
        if parent.suffix == ".app":
            try:
                with (parent / "Contents" / "Info.plist").open("rb") as stream:
                    info = plistlib.load(stream)
            except (OSError, plistlib.InvalidFileException, ValueError):
                return {"name": parent.stem, "version": None}
            version = info.get("CFBundleShortVersionString") or info.get("CFBundleVersion")
            return {"name": parent.stem, "version": str(version) if version else None}
    return None


def cached_server_version(
    system: System,
    store: Store,
    executable: Path,
    environment: Dict[str, str],
    *,
    timeout: float = 30.0,
) -> str:
    """`--version` output, cached per binary so identity probes stay cheap."""
    if not system.is_executable(executable):
        raise InfrastructureError(f"server binary is not an executable file: {executable}")
    try:
        size, mtime_ns = system.file_identity(executable)
    except OSError as exc:
        raise InfrastructureError(f"server binary is unavailable: {exc}") from exc
    cache = store.read_cache()
    entry = cache["servers"].get(str(executable), {})
    if entry.get("size_bytes") == size and entry.get("mtime_ns") == mtime_ns and entry.get("output"):
        return str(entry["output"])
    output = system.server_version(executable, environment, timeout)
    cache["servers"][str(executable)] = {"size_bytes": size, "mtime_ns": mtime_ns, "output": output}
    store.write_cache(cache)
    return output


def verify_artifacts(
    system: System, store: Store, profile: Profile, environment: Dict[str, str]
) -> Dict[str, Any]:
    """Check the key file, the model file and the server build; return what was verified."""
    read_api_key(profile)  # first: an unsafe key file is refused before any slow hashing
    try:
        size, mtime_ns = system.file_identity(profile.model_path)
    except OSError as exc:
        raise InfrastructureError(f"model file is unavailable: {exc}") from exc
    if size != profile.model_size:
        raise InfrastructureError(
            f"model size mismatch: expected {profile.model_size}, found {size}"
        )
    cache = store.read_cache()
    entry = cache["models"].get(str(profile.model_path), {})
    if not (
        entry.get("size_bytes") == size
        and entry.get("mtime_ns") == mtime_ns
        and entry.get("sha256") == profile.model_sha256
    ):
        found = system.sha256(profile.model_path)
        if found != profile.model_sha256:
            raise InfrastructureError(
                f"model sha256 mismatch: expected {profile.model_sha256}, found {found}"
            )
        cache["models"][str(profile.model_path)] = {
            "size_bytes": size,
            "mtime_ns": mtime_ns,
            "sha256": found,
            "verified_at": utc_now(),
        }
        store.write_cache(cache)
    version = cached_server_version(system, store, profile.executable, environment)
    build, commit = parse_build(version)
    if build is not None and build < profile.min_build:
        raise InfrastructureError(
            f"server build {build} is older than the profile minimum {profile.min_build}"
        )
    evidence: Dict[str, Any] = {
        "model_file": profile.model_path.name,
        "model_size_bytes": size,
        "model_sha256": profile.model_sha256,
        "context_tokens": profile.context_tokens,
        "server_build": version_line(version),
        "server_build_number": build,
        "server_commit": commit,
        "server_min_build": profile.min_build,
        # An unrecognised version string is recorded, not fatal: readiness still gates
        # the lease, and upgrading the server is the operator's call.
        "server_compatibility": "unknown_version" if build is None else "compatible",
        "application": application_version(profile.executable),
        "api_key_required": profile.api_key_file is not None,
    }
    for key, value in (
        ("model_family", profile.model_family),
        ("model_quantization", profile.model_quantization),
        ("model_source", profile.model_source),
    ):
        if value:
            evidence[key] = value
    return evidence

