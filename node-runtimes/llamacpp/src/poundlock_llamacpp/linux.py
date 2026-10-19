"""Linux adapter: /proc ownership checks and sysfs GPU readings."""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

from .system import ProcessInfo, System, script_interpreter_runs

DELETED_SUFFIX = " (deleted)"
CARD = re.compile(r"^card\d+$")


class LinuxSystem(System):
    name = "linux"
    # The node agent runs as a hardened service; a detached sweeper needs no unit
    # file, and it shares the server's cgroup so a service restart ends both.
    default_sweeper = "detached"

    def __init__(self, proc_root: Path = Path("/proc"), sysfs_root: Path = Path("/sys")) -> None:
        super().__init__()
        self.proc_root = proc_root
        self.sysfs_root = sysfs_root

    def inspect(self, pid: int) -> Optional[ProcessInfo]:
        if pid <= 0:
            return None
        base = self.proc_root / str(pid)
        try:
            stat_text = (base / "stat").read_text(errors="replace")
            fields = stat_text[stat_text.rindex(")") + 2 :].split()
            state, started = fields[0], fields[19]
            if state in {"Z", "X"}:
                return None
            raw = (base / "cmdline").read_bytes()
        except (OSError, ValueError, IndexError):
            return None
        argv = tuple(part.decode(errors="replace") for part in raw.rstrip(b"\0").split(b"\0"))
        try:
            exe: Optional[str] = os.readlink(base / "exe")
        except OSError:
            exe = None
        return ProcessInfo(pid=pid, started=started, command=" ".join(argv), argv=argv, exe=exe)

    def runs_binary(self, info: ProcessInfo, binary: Path) -> bool:
        if info.exe:
            exe = info.exe[: -len(DELETED_SUFFIX)] if info.exe.endswith(DELETED_SUFFIX) else info.exe
            if os.path.realpath(exe) == os.path.realpath(binary):
                return True
        return script_interpreter_runs(info, binary)

    def find_processes(self, name: str) -> List[int]:
        found = []
        try:
            entries = list(self.proc_root.iterdir())
        except OSError:
            return []
        for entry in entries:
            if not entry.name.isdigit():
                continue
            try:
                comm = (entry / "comm").read_text(errors="replace").strip()
            except OSError:
                continue
            if comm == name[:15] and self.inspect(int(entry.name)) is not None:
                found.append(int(entry.name))
        return sorted(found)

    def gpu_memory(self) -> List[Dict[str, Any]]:
        """Best-effort AMD readings; other GPUs simply do not appear."""
        readings = []
        drm = self.sysfs_root / "class" / "drm"
        try:
            cards = sorted(entry for entry in drm.iterdir() if CARD.match(entry.name))
        except OSError:
            return []
        for card in cards:
            used = self._read_int(card / "device" / "mem_info_vram_used")
            if used is None:
                continue
            readings.append(
                {
                    "card": card.name,
                    "vram_used": used,
                    "vram_total": self._read_int(card / "device" / "mem_info_vram_total"),
                    "gtt_used": self._read_int(card / "device" / "mem_info_gtt_used"),
                }
            )
        return readings

    @staticmethod
    def _read_int(path: Path) -> Optional[int]:
        try:
            return int(path.read_text().strip())
        except (OSError, ValueError):
            return None

    def default_home(self) -> Path:
        state = os.environ.get("XDG_STATE_HOME")
        base = Path(state) if state else Path.home() / ".local" / "state"
        return base / "poundlock-llamacpp"

    def server_environment(self, home: Path) -> Dict[str, str]:
        # Mesa keeps its Vulkan shader cache here. A service's $HOME is often
        # read-only, which would recompile shaders on every start.
        return {"XDG_CACHE_HOME": str(home / "cache")}
