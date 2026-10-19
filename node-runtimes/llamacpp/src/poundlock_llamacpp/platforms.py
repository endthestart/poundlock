"""Pick the platform adapter for the running OS."""

from __future__ import annotations

import sys

from .errors import ManagerError
from .linux import LinuxSystem
from .macos import MacSystem
from .system import System


def detect_system() -> System:
    if sys.platform.startswith("linux"):
        return LinuxSystem()
    if sys.platform == "darwin":
        return MacSystem()
    raise ManagerError(f"unsupported platform: {sys.platform}; Linux and macOS only")
