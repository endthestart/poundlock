"""The macOS launchd job that runs `sweep` every minute."""

from __future__ import annotations

import plistlib
from pathlib import Path

LAUNCHD_LABEL = "com.poundlock.llamacpp"


def render_plist(executable: Path, home: Path, manifest: Path, interval_seconds: int = 60) -> bytes:
    """Property list for ~/Library/LaunchAgents/com.poundlock.llamacpp.plist.

    stdout goes nowhere (each sweep prints a status object); stderr keeps errors.
    """
    return plistlib.dumps(
        {
            "Label": LAUNCHD_LABEL,
            "ProgramArguments": [
                str(executable),
                "--home", str(home),
                "--manifest", str(manifest),
                "sweep",
            ],
            "RunAtLoad": True,
            "StartInterval": int(interval_seconds),
            "StandardOutPath": "/dev/null",
            "StandardErrorPath": str(home / "logs" / "launchd-sweep.error.log"),
        }
    )
