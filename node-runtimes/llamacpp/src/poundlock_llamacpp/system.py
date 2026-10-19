"""The host boundary: everything the manager does to files, processes and sockets.

`System` holds what is the same on Linux and macOS. `LinuxSystem` and `MacSystem`
supply process inspection, GPU readings and platform defaults. Tests replace the
whole thing with an in-memory fake.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from .errors import ManagerError

_NO_PROXY = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class RequestFailed(Exception):
    """An HTTP call failed; the message is safe to log and never carries a response body."""

    def __init__(self, message: str, status: Optional[int] = None) -> None:
        super().__init__(message)
        self.status = status


@dataclass(frozen=True)
class ProcessInfo:
    pid: int
    started: str  # opaque start-time token; it changes if the PID is reused
    command: str  # argv joined by single spaces
    argv: Optional[Tuple[str, ...]] = None
    exe: Optional[str] = None


def script_interpreter_runs(info: ProcessInfo, binary: Path) -> bool:
    """True when `binary` is a `#!` script and `info` is its interpreter running it."""
    try:
        with binary.open("rb") as stream:
            if stream.read(2) != b"#!":
                return False
    except OSError:
        return False
    named = {str(binary), os.path.realpath(binary)}
    if info.argv is not None:
        return any(part in named for part in info.argv[1:3])
    return any(f" {name} " in f" {info.command} " for name in named)


class System:
    name = "generic"
    default_sweeper = "external"
    supports_caffeinate = False

    def __init__(self) -> None:
        self._children: Dict[int, subprocess.Popen[bytes]] = {}

    # --- platform-specific ---------------------------------------------------

    def inspect(self, pid: int) -> Optional[ProcessInfo]:
        raise NotImplementedError

    def runs_binary(self, info: ProcessInfo, binary: Path) -> bool:
        raise NotImplementedError

    def find_processes(self, name: str) -> List[int]:
        """PIDs of live processes whose name is exactly `name`."""
        raise NotImplementedError

    def gpu_memory(self) -> List[Dict[str, Any]]:
        return []

    def default_home(self) -> Path:
        raise NotImplementedError

    def server_environment(self, home: Path) -> Dict[str, str]:
        """Variables set unless the profile sets them."""
        return {}

    def caffeinate_command(self, server_pid: int) -> Optional[List[str]]:
        return None

    # --- files ---------------------------------------------------------------

    def file_identity(self, path: Path) -> Tuple[int, int]:
        info = path.stat()
        return info.st_size, info.st_mtime_ns

    def sha256(self, path: Path) -> str:
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            while chunk := stream.read(8 * 1024 * 1024):
                digest.update(chunk)
        return digest.hexdigest()

    def is_executable(self, path: Path) -> bool:
        return path.is_file() and os.access(path, os.X_OK)

    def server_version(
        self, executable: Path, env: Optional[Dict[str, str]] = None, timeout: float = 30.0
    ) -> str:
        """`--version` output. Run with the server's environment, so a binary that needs
        `LD_LIBRARY_PATH` to load is judged as it will actually run."""
        try:
            completed = subprocess.run(
                [str(executable), "--version"],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                check=False,
                timeout=timeout,
                env=env,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            raise ManagerError(f"cannot run {executable} --version: {exc}") from exc
        output = (completed.stdout + completed.stderr)[:16_384].decode(errors="replace")
        if completed.returncode != 0:
            first = (output.strip().splitlines() or ["no output"])[0][:200]
            raise ManagerError(f"{executable} --version exited {completed.returncode}: {first}")
        return output

    def read_log(self, path: Path, limit: int = 4 * 1024 * 1024) -> str:
        with path.open("rb") as stream:
            return stream.read(limit).decode(errors="replace")

    # --- processes -----------------------------------------------------------

    def spawn_server(self, argv: Sequence[str], env: Dict[str, str], log_path: Path) -> int:
        """Start the server detached from our pipes but inside our process group.

        The node agent kills the command's process group when an ensure times out
        or fails, so a server that stays in the group cannot outlive a failed
        ensure. Its stdio goes to the log and its fds are not inherited, so the
        agent never waits on a pipe the server holds open.
        """
        return self._spawn(argv, env, log_path, new_session=False)

    def spawn_helper(self, argv: Sequence[str], env: Dict[str, str], log_path: Path) -> int:
        """Start a helper (sweeper, caffeinate) that must outlive the command."""
        return self._spawn(argv, env, log_path, new_session=True)

    def _spawn(
        self, argv: Sequence[str], env: Dict[str, str], log_path: Path, *, new_session: bool
    ) -> int:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        try:
            process = subprocess.Popen(
                list(argv),
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=fd,
                stderr=fd,
                close_fds=True,
                start_new_session=new_session,
            )
        finally:
            os.close(fd)
        self._children[process.pid] = process
        return process.pid

    def alive(self, pid: int) -> bool:
        child = self._children.get(pid)
        if child is not None:
            child.poll()  # reaps a zombie so it stops looking alive
        return self.inspect(pid) is not None

    def terminate(self, pid: int, grace_seconds: float = 20.0) -> None:
        """SIGTERM, wait, then SIGKILL. The caller has already verified ownership."""
        if pid <= 0 or pid == os.getpid():
            return
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            self.alive(pid)
            return
        if self._wait_gone(pid, grace_seconds):
            return
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        self._wait_gone(pid, 5.0)

    def _wait_gone(self, pid: int, seconds: float) -> bool:
        deadline = time.monotonic() + seconds
        while True:
            if not self.alive(pid):
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.05)

    def port_in_use(self, host: str, port: int) -> bool:
        family = socket.AF_INET6 if ":" in host else socket.AF_INET
        with socket.socket(family, socket.SOCK_STREAM) as probe:
            probe.settimeout(1.0)
            return probe.connect_ex((host, port)) == 0

    # --- http ----------------------------------------------------------------

    def http_json(
        self,
        method: str,
        url: str,
        body: Optional[Dict[str, Any]] = None,
        timeout: float = 3.0,
        api_key: Optional[str] = None,
    ) -> Any:
        data = None if body is None else json.dumps(body).encode()
        headers = {"Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        if api_key is not None:
            # Unredirected: a redirect never carries the key to another address.
            request.add_unredirected_header("Authorization", f"Bearer {api_key}")
        path = urllib.parse.urlsplit(url).path
        try:
            with _NO_PROXY.open(request, timeout=timeout) as response:
                return json.load(response)
        except urllib.error.HTTPError as exc:
            raise RequestFailed(f"HTTP {exc.code} from {path}", exc.code) from None
        except (OSError, ValueError, http.client.HTTPException) as exc:
            raise RequestFailed(f"{path}: {type(exc).__name__}: {str(exc)[:200]}") from None

    # --- helper launch -------------------------------------------------------

    def self_command(self) -> Tuple[List[str], Dict[str, str]]:
        """Command prefix and environment that re-run this package, also from a zipapp."""
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        existing = os.environ.get("PYTHONPATH")
        path = root if not existing else f"{root}{os.pathsep}{existing}"
        return [sys.executable, "-m", "poundlock_llamacpp"], {"PYTHONPATH": path}
