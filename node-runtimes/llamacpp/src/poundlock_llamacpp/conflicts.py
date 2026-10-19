"""Things that share the GPU and must let go before llama-server loads."""

from __future__ import annotations

from typing import Callable, List

from .errors import ConflictError
from .manifest import Profile
from .system import System


def unload_ollama(system: System, base_url: str, log: Callable[[str], None]) -> List[str]:
    """Ask Ollama to drop every loaded model. Not running, or a failed call, is fine:
    the idle-VRAM gate is what refuses to start on a busy GPU."""
    try:
        listing = system.http_json("GET", f"{base_url}/api/ps", timeout=5)
    except Exception:
        return []
    models = listing.get("models") if isinstance(listing, dict) else None
    names = [
        item["name"]
        for item in models or []
        if isinstance(item, dict) and isinstance(item.get("name"), str)
    ]
    for name in names:
        try:
            system.http_json("POST", f"{base_url}/api/generate", {"model": name, "keep_alive": 0}, timeout=30)
            log(f"asked ollama to unload {name}")
        except Exception as exc:
            log(f"ollama unload of {name} failed: {type(exc).__name__}")
    return names


def release_conflicts(
    system: System, profile: Profile, ollama_url: str, log: Callable[[str], None]
) -> List[str]:
    unloaded: List[str] = []
    if "ollama" in profile.conflicts:
        unloaded = unload_ollama(system, ollama_url, log)
    return unloaded


def wait_for_idle_vram(
    system: System,
    profile: Profile,
    clock: Callable[[], float],
    sleep: Callable[[float], None],
) -> int:
    """Wait until no card holds more than `max_idle_vram_bytes`. Returns the highest
    reading seen, or -1 when the host gives no readings (the gate is then skipped)."""
    limit = profile.max_idle_vram_bytes
    if limit is None:
        return -1
    deadline = clock() + profile.vram_settle_seconds
    while True:
        readings = system.gpu_memory()
        if not readings:
            return -1
        used = max(int(item["vram_used"]) for item in readings)
        if used < limit:
            return used
        if clock() >= deadline:
            raise ConflictError(
                f"VRAM still holds {used} bytes after {profile.vram_settle_seconds}s "
                f"(limit {limit}); something else is using the GPU"
            )
        sleep(1.0)
