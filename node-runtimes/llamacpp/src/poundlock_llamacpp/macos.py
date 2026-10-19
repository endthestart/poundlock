"""macOS adapter: `ps`-based ownership checks and optional caffeinate."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import List, Optional

from .system import ProcessInfo, System, script_interpreter_runs

PS = "/bin/ps"
PGREP = "/usr/bin/pgrep"
CAFFEINATE = "/usr/bin/caffeinate"


class MacSystem(System):
    name = "macos"
    # launchd runs `sweep` every minute (deploy/com.poundlock.llamacpp.plist).
    default_sweeper = "external"
    supports_caffeinate = True

    def _run(self, argv: List[str]) -> Optional[str]:
        try:
            completed = subprocess.run(
                argv,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                check=False,
                timeout=10,
                # UTC keeps the start-time token identical for every caller, whatever TZ
                # its environment sets.
                env={**os.environ, "LC_ALL": "C", "TZ": "UTC"},
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return completed.stdout.decode(errors="replace") if completed.returncode == 0 else None

    def inspect(self, pid: int) -> Optional[ProcessInfo]:
        if pid <= 0:
            return None
        output = self._run([PS, "-ww", "-p", str(pid), "-o", "stat=", "-o", "lstart=", "-o", "command="])
        return None if output is None else parse_ps_line(pid, output)

    def runs_binary(self, info: ProcessInfo, binary: Path) -> bool:
        for candidate in {str(binary), os.path.realpath(binary)}:
            if info.command == candidate or info.command.startswith(candidate + " "):
                return True
        return script_interpreter_runs(info, binary)

    def find_processes(self, name: str) -> List[int]:
        output = self._run([PGREP, "-x", name])
        if not output:
            return []
        pids = [int(line) for line in output.split() if line.isdigit()]
        return sorted(pid for pid in pids if self.inspect(pid) is not None)

    def default_home(self) -> Path:
        return Path.home() / "Library" / "Application Support" / "poundlock-llamacpp"

    def caffeinate_command(self, server_pid: int) -> Optional[List[str]]:
        return [CAFFEINATE, "-dimsu", "-w", str(server_pid)]


def parse_ps_line(pid: int, output: str) -> Optional[ProcessInfo]:
    """Parse `ps -o stat= -o lstart= -o command=`: state, five start-time words, argv."""
    parts = output.strip().split(None, 6)
    if len(parts) < 7 or parts[0].startswith(("Z", "X")):
        return None
    return ProcessInfo(pid=pid, started=" ".join(parts[1:6]), command=parts[6])
