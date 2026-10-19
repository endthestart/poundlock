"""In-memory host for the lifecycle tests: processes, HTTP and files are all simulated."""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import urlsplit
from uuid import uuid4

from poundlock_llamacpp.errors import ManagerError
from poundlock_llamacpp.manager import Manager
from poundlock_llamacpp.system import ProcessInfo, RequestFailed, System

ALIAS = "example-model"
BINARY = "/opt/llama/llama-server"
MODEL = "/opt/models/example.gguf"
SHA = "ab" * 32


class FakeSystem(System):
    name = "fake"
    default_sweeper = "external"

    def __init__(self) -> None:
        super().__init__()
        self.procs: Dict[int, ProcessInfo] = {}
        self.next_pid = 1000
        self.spawns: List[List[str]] = []
        self.spawn_env: List[Dict[str, str]] = []
        self.helpers: List[List[str]] = []
        self.terminated: List[int] = []
        self.requests: List[str] = []
        self.foreign: Dict[int, str] = {}  # pid -> process name
        self.port_busy = False
        self.gpu: List[Dict[str, Any]] = []
        self.gpu_after_unload: Optional[List[Dict[str, Any]]] = None
        self.model_size = 1234
        self.model_hash = SHA
        self.hash_calls = 0
        self.version_calls = 0
        self.version_text = "version: 11200 (abcdef012)"
        self.executable_ok = True
        self.version_error = ""
        self.version_env: Optional[Dict[str, str]] = None
        self.spawn_error: Optional[OSError] = None
        self.log_text = "offloaded 33/33 layers to GPU\n"
        self.supports_caffeinate = False
        # server behaviour
        self.server_pid: Optional[int] = None
        self.loading_polls = 0  # /health answers 503 this many times first
        self.n_ctx = 4096  # what the running server reports
        self.spawn_n_ctx = 4096  # what a newly started server reports
        self.tps = 60.0
        self.reasoning = ""
        self.warmup_error = False
        self.crash_after_spawn = False
        self.exec_pending_inspects = 0  # inspects that see the server with no command line yet
        self.ollama_models: List[str] = []
        self.ollama_up = True
        self.required_key: Optional[str] = None  # a key the server wants that no profile names
        self.server_key: Optional[str] = None  # what the running server accepts
        self.sent_keys: List[Optional[str]] = []  # the key each server request carried
        self.unloaded: List[str] = []
        self.events: List[str] = []
        self.sleep_hook = None

    # --- platform surface ---
    def inspect(self, pid: int) -> Optional[ProcessInfo]:
        info = self.procs.get(pid)
        if info is not None and pid == self.server_pid and self.exec_pending_inspects > 0:
            self.exec_pending_inspects -= 1
            return ProcessInfo(pid=pid, started=info.started, command="", argv=("",), exe=info.exe)
        return info

    def runs_binary(self, info: ProcessInfo, binary: Path) -> bool:
        return info.exe == str(binary)

    def find_processes(self, name: str) -> List[int]:
        return sorted(pid for pid, proc_name in self.foreign.items() if proc_name == name)

    def gpu_memory(self) -> List[Dict[str, Any]]:
        return list(self.gpu)

    def default_home(self) -> Path:
        return Path("/nonexistent")

    def server_environment(self, home: Path) -> Dict[str, str]:
        return {"XDG_CACHE_HOME": str(home / "cache")}

    def caffeinate_command(self, server_pid: int) -> Optional[List[str]]:
        return ["caffeinate", "-dimsu", "-w", str(server_pid)] if self.supports_caffeinate else None

    def self_command(self):
        return ["python3", "-m", "poundlock_llamacpp"], {}

    # --- files ---
    def file_identity(self, path: Path):
        if str(path) == BINARY:
            return 10, 1
        if str(path) != MODEL:
            raise FileNotFoundError(str(path))
        return self.model_size, 7

    def sha256(self, path: Path) -> str:
        self.hash_calls += 1
        return self.model_hash

    def is_executable(self, path: Path) -> bool:
        return self.executable_ok

    def server_version(self, executable: Path, env=None, timeout: float = 30.0) -> str:
        self.version_calls += 1
        self.version_env = env
        if self.version_error:
            raise ManagerError(self.version_error)
        return self.version_text

    def read_log(self, path: Path, limit: int = 0) -> str:
        return self.log_text

    # --- processes ---
    def _add(self, argv: List[str], prefix: str = "t") -> int:
        self.next_pid += 1
        pid = self.next_pid
        self.procs[pid] = ProcessInfo(
            pid=pid, started=f"{prefix}{pid}", command=" ".join(argv), argv=tuple(argv), exe=argv[0]
        )
        return pid

    def spawn_server(self, argv, env, log_path) -> int:
        if self.spawn_error:
            raise self.spawn_error
        self.events.append("spawn")
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_text(self.log_text)
        self.spawns.append(list(argv))
        self.spawn_env.append(dict(env))
        pid = self._add(list(argv))
        self.server_pid = pid
        self.n_ctx = self.spawn_n_ctx
        self.server_key = self.required_key
        if "--api-key-file" in argv:  # read once at start, as llama-server does
            key_file = Path(argv[list(argv).index("--api-key-file") + 1])
            self.server_key = key_file.read_text().split("\n")[0]
        if self.crash_after_spawn:
            del self.procs[pid]
        return pid

    def spawn_helper(self, argv, env, log_path) -> int:
        self.helpers.append(list(argv))
        return self._add(list(argv), prefix="h")

    def terminate(self, pid: int, grace_seconds: float = 20.0) -> None:
        self.terminated.append(pid)
        self.procs.pop(pid, None)

    def port_in_use(self, host: str, port: int) -> bool:
        return self.port_busy

    def kill_externally(self, pid: int) -> None:
        self.procs.pop(pid, None)

    def running(self) -> List[ProcessInfo]:
        return list(self.procs.values())

    # --- http ---
    def http_json(self, method, url, body=None, timeout=3.0, api_key=None):
        self.requests.append(f"{method} {url}")
        if ":11434" in url:
            return self._ollama(method, url, body)
        if self.server_pid is None or self.server_pid not in self.procs:
            raise RequestFailed("connection refused")
        self.sent_keys.append(api_key)
        if self.server_key is not None and api_key != self.server_key:
            raise RequestFailed(f"HTTP 401 from {urlsplit(url).path}", 401)
        if url.endswith("/health"):
            if self.loading_polls > 0:
                self.loading_polls -= 1
                raise RequestFailed("HTTP 503 from /health")
            return {"status": "ok"}
        if url.endswith("/v1/models"):
            argv = self.procs[self.server_pid].argv or ()
            return {"data": [{"id": argv[argv.index("--alias") + 1]}]}
        if url.endswith("/props"):
            return {"default_generation_settings": {"n_ctx": self.n_ctx}}
        if url.endswith("/v1/chat/completions"):
            if self.warmup_error:
                raise RequestFailed("HTTP 400 from /v1/chat/completions")
            message: Dict[str, Any] = {"content": "ok"}
            if self.reasoning:
                message["reasoning_content"] = self.reasoning
            return {"choices": [{"message": message}], "timings": {"predicted_per_second": self.tps}}
        raise RequestFailed(f"HTTP 404 from {url}")

    def _ollama(self, method: str, url: str, body: Any):
        if not self.ollama_up:
            raise RequestFailed("connection refused")
        if url.endswith("/api/ps"):
            return {"models": [{"name": name} for name in self.ollama_models]}
        if url.endswith("/api/generate"):
            self.events.append(f"unload {body['model']}")
            self.unloaded.append(body["model"])
            self.ollama_models.remove(body["model"])
            if self.gpu_after_unload is not None:
                self.gpu = self.gpu_after_unload
            return {"done": True}
        raise RequestFailed("HTTP 404")


def profile_dict(**overrides: Any) -> Dict[str, Any]:
    profile: Dict[str, Any] = {
        "model": {
            "path": MODEL,
            "size_bytes": 1234,
            "sha256": SHA,
            "family": "Example-7B",
            "quantization": "Q4_K_M",
        },
        "server": {
            "executable": BINARY,
            "min_build": 11000,
            "host": "127.0.0.1",
            "port": 8080,
            "context_tokens": 4096,
            "arguments": ["--parallel", "1", "--jinja"],
            "environment": {"GGML_EXAMPLE": "1"},
        },
        "warm_seconds": 600,
        "lease_timeout_seconds": 300,
        "startup_timeout_seconds": 60,
    }
    for key, value in overrides.items():
        profile[key] = value
    return profile


def manifest_dict(profiles: Optional[Dict[str, Any]] = None, **runtime: Any) -> Dict[str, Any]:
    return {
        "schema_version": 1,
        "runtime": {"foreign_process_names": ["llama-server"], **runtime},
        "profiles": profiles if profiles is not None else {ALIAS: profile_dict()},
    }


class LifecycleCase(unittest.TestCase):
    """A Manager on a FakeSystem with a clock that only `sleep` advances."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="llamacpp-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.now = 1_000_000.0
        self.system = FakeSystem()
        self.home = self.tmp / "home"
        self.manifest_path = self.tmp / "manifest.json"
        self.write_manifest(manifest_dict())
        self.manager = self.make_manager()

    def advance(self, seconds: float) -> None:
        self.now += seconds

    def make_manager(self) -> Manager:
        def sleep(seconds: float) -> None:
            self.now += seconds
            if self.system.sleep_hook:
                self.system.sleep_hook()

        return Manager(
            home=self.home,
            manifest_path=self.manifest_path,
            system=self.system,
            clock=lambda: self.now,
            sleep=sleep,
            lock_timeout=0.3,
        )

    def write_manifest(self, manifest: Dict[str, Any]) -> None:
        self.manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    def edit_profile(self, **changes: Any) -> None:
        manifest = json.loads(self.manifest_path.read_text())
        manifest["profiles"][ALIAS].update(changes)
        self.write_manifest(manifest)

    def ids(self):
        return str(uuid4()), str(uuid4())

    def ensure(self, lease: Optional[str] = None, alias: str = ALIAS) -> Dict[str, Any]:
        return self.manager.ensure(alias, lease or str(uuid4()), str(uuid4()))

    def state(self) -> Dict[str, Any]:
        return json.loads((self.home / "state.json").read_text())

    def assert_nothing_running(self) -> None:
        self.assertEqual([], [proc.command for proc in self.system.running()])
