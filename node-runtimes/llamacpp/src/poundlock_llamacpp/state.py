"""Durable state under the runtime home: state file, operation lock, hash cache, logs."""

from __future__ import annotations

import fcntl
import json
import os
import time
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List

from .errors import BusyError, InfrastructureError

STATE_VERSION = 1
MANAGER_LOG_LIMIT = 1024 * 1024
KEEP_SERVER_LOGS = 10


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def atomic_json(path: Path, payload: Dict[str, Any]) -> None:
    """Write a private file that readers see either whole or not at all."""
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(payload, stream, indent=2, sort_keys=True)
        stream.write("\n")
    os.replace(temporary, path)


class Store:
    def __init__(
        self,
        home: Path,
        *,
        lock_timeout: float = 60.0,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.home = home
        self.state_path = home / "state.json"
        self.cache_path = home / "artifact-cache.json"
        self.lock_path = home / "operation.lock"
        self.log_root = home / "logs"
        self.manager_log = self.log_root / "manager.log"
        self.lock_timeout = lock_timeout
        self._sleep = sleep

    def prepare(self) -> None:
        for directory in (self.home, self.log_root):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)

    @contextmanager
    def lock(self, *, wait: bool = True) -> Iterator[None]:
        """Exclusive operation lock. The fd is close-on-exec, so no child holds it."""
        self.prepare()
        with self.lock_path.open("a+") as stream:
            deadline = time.monotonic() + (self.lock_timeout if wait else 0.0)
            while True:
                try:
                    fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise BusyError("the runtime manager is busy with another operation") from None
                    self._sleep(0.1)
            try:
                yield
            finally:
                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)

    def read_state(self) -> Dict[str, Any]:
        if not self.state_path.exists():
            return {"schema_version": STATE_VERSION, "revision": 0, "phase": "stopped"}
        try:
            state = json.loads(self.state_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise InfrastructureError(f"manager state is unreadable: {exc}") from exc
        if not isinstance(state, dict) or state.get("schema_version") != STATE_VERSION:
            raise InfrastructureError(f"manager state schema_version must be {STATE_VERSION}")
        return state

    def write_state(self, state: Dict[str, Any]) -> Dict[str, Any]:
        state = dict(state)
        state["schema_version"] = STATE_VERSION
        state["revision"] = int(state.get("revision", 0)) + 1
        state["updated_at"] = utc_now()
        self.prepare()
        atomic_json(self.state_path, state)
        return state

    def read_cache(self) -> Dict[str, Any]:
        try:
            cache = json.loads(self.cache_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            cache = {}
        if not isinstance(cache, dict):
            cache = {}
        for section in ("models", "servers"):
            if not isinstance(cache.get(section), dict):
                cache[section] = {}
        return cache

    def write_cache(self, cache: Dict[str, Any]) -> None:
        self.prepare()
        atomic_json(self.cache_path, cache)

    def log(self, command: str, lease: str, message: str) -> None:
        """One line per event. Callers pass ids and counts, never environment or arguments."""
        try:
            self.prepare()
            if self.manager_log.exists() and self.manager_log.stat().st_size > MANAGER_LOG_LIMIT:
                os.replace(self.manager_log, self.manager_log.with_name("manager.log.1"))
            fd = os.open(self.manager_log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
            with os.fdopen(fd, "a", encoding="utf-8") as stream:
                stream.write(f"{utc_now()} {command} {lease or '-'} {message}\n")
        except OSError:
            pass  # logging must never fail an operation

    def prune_server_logs(self, keep_path: Path) -> List[Path]:
        removed = []
        logs = sorted(self.log_root.glob("*.server.log"), key=lambda entry: entry.stat().st_mtime)
        for entry in logs[:-KEEP_SERVER_LOGS]:
            if entry != keep_path:
                try:
                    entry.unlink()
                    removed.append(entry)
                except OSError:
                    pass
        return removed
