"""Real processes and sockets, with a fake llama-server. Never llama.cpp.

These run the CLI the way the node agent does: a subprocess with piped stdout and
stderr, its own process group, and a wait for the pipes to close.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional
from uuid import uuid4

from poundlock_llamacpp.platforms import detect_system
from poundlock_llamacpp.state import Store

SOURCE = Path(__file__).resolve().parents[1] / "src"
FAKE_SERVER = Path(__file__).with_name("fake_llama_server.py")
ALIAS = "fake-model"


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def wait_for(condition: Callable[[], Any], seconds: float = 15.0) -> Any:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = condition()
        if value:
            return value
        time.sleep(0.05)
    return condition()


class RealCase(unittest.TestCase):
    warm_seconds = 600
    startup_seconds = 30
    sweeper = "external"
    warmup: Optional[Dict[str, Any]] = None
    markers: List[str] = []
    server_env: Dict[str, str] = {}
    server_keys: Dict[str, Any] = {}

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="llamacpp-real-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.addCleanup(self.kill_stragglers)
        self.system = detect_system()
        # A distinctive name, so a llama-server on the developer's machine is never mistaken for it.
        self.binary = self.tmp / "bin" / "fake-llama-server"
        self.binary.parent.mkdir()
        self.binary.write_text(f"#!{sys.executable}\n" + FAKE_SERVER.read_text())
        self.binary.chmod(0o755)
        self.model = self.tmp / "model.gguf"
        self.model.write_bytes(b"not a real model\n" * 100)
        self.home = self.tmp / "home"
        self.manifest = self.tmp / "manifest.json"
        self.port = free_port()
        self.request_log = self.tmp / "requests.log"
        self.write_manifest()
        self.owned: List[subprocess.Popen] = []

    def write_manifest(self) -> None:
        profile: Dict[str, Any] = {
            "model": {
                "path": str(self.model),
                "size_bytes": self.model.stat().st_size,
                "sha256": hashlib.sha256(self.model.read_bytes()).hexdigest(),
            },
            "server": {
                "executable": str(self.binary),
                "min_build": 11000,
                "host": "127.0.0.1",
                "port": self.port,
                "context_tokens": 4096,
                "environment": {"FAKE_REQUEST_LOG": str(self.request_log), **self.server_env},
                "arguments": ["--parallel", "1"],
                **self.server_keys,
            },
            "required_log_markers": self.markers,
            "warm_seconds": self.warm_seconds,
            "lease_timeout_seconds": 300,
            "startup_timeout_seconds": self.startup_seconds,
        }
        if self.warmup is not None:
            profile["warmup"] = self.warmup
        self.manifest.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "runtime": {
                        "sweeper": self.sweeper,
                        "sweep_interval_seconds": 1,
                        "foreign_process_names": [],
                    },
                    "profiles": {ALIAS: profile},
                }
            )
        )

    # --- running the CLI ---
    def argv(self, *args: str) -> List[str]:
        return [sys.executable, "-m", "poundlock_llamacpp", "--home", str(self.home),
                "--manifest", str(self.manifest), *args]

    def env(self) -> Dict[str, str]:
        env = {k: v for k, v in os.environ.items() if not k.startswith("POUNDLOCK_LLAMACPP")}
        env["PYTHONPATH"] = str(SOURCE)
        return env

    def run_cli(self, *args: str, timeout: float = 60.0) -> subprocess.CompletedProcess:
        # Piped output plus communicate() only returns once every holder of the pipe has
        # closed it, so a server that kept the pipe would time this out.
        return subprocess.run(
            self.argv(*args), capture_output=True, text=True, timeout=timeout,
            env=self.env(), stdin=subprocess.DEVNULL, start_new_session=True,
        )

    def popen_cli(self, *args: str) -> subprocess.Popen:
        process = subprocess.Popen(
            self.argv(*args), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env=self.env(), stdin=subprocess.DEVNULL, start_new_session=True,
        )
        self.owned.append(process)
        return process

    def ensure(self, lease: Optional[str] = None) -> subprocess.CompletedProcess:
        return self.run_cli("ensure", ALIAS, lease or str(uuid4()), str(uuid4()))

    def result(self, completed: subprocess.CompletedProcess) -> Dict[str, Any]:
        self.assertEqual(0, completed.returncode, completed.stderr)
        return json.loads(completed.stdout)

    def error(self, completed: subprocess.CompletedProcess, code: int) -> Dict[str, Any]:
        self.assertEqual(code, completed.returncode, completed.stdout + completed.stderr)
        self.assertEqual("", completed.stdout)
        return json.loads(completed.stderr)

    # --- observing ---
    def state(self) -> Dict[str, Any]:
        return json.loads((self.home / "state.json").read_text())

    def alive(self, pid: int) -> bool:
        return self.system.alive(pid)

    def fake_server_processes(self) -> Dict[int, str]:
        """PID to command line of every process running this test's fake server, from `ps`."""
        listing = subprocess.run(
            ["ps", "-axww", "-o", "pid=,command="], capture_output=True, text=True
        ).stdout
        found = {}
        for line in listing.splitlines():
            pid, _, command = line.strip().partition(" ")
            if str(self.binary) in command and pid.isdigit():
                found[int(pid)] = command
        return found

    def fake_servers(self) -> List[str]:
        return list(self.fake_server_processes().values())

    def assert_no_fake_server(self) -> None:
        wait_for(lambda: not self.fake_servers(), 5)
        self.assertEqual([], self.fake_servers(), "a fake llama-server was left running")

    def kill_stragglers(self) -> None:
        for process in self.owned:
            if process.poll() is None:
                process.kill()
            process.communicate()
        try:
            state = self.state()
        except (OSError, ValueError):
            state = {}
        stray = set(self.fake_server_processes())
        sweeper = (state.get("sweeper") or {}).get("pid")
        if sweeper:
            stray.add(int(sweeper))
        for pid in stray:
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass


class LifecycleTests(RealCase):
    def test_the_node_call_sequence_with_piped_output(self) -> None:
        lease = str(uuid4())
        started = time.monotonic()
        ready = self.result(self.ensure(lease))
        self.assertLess(time.monotonic() - started, 30, "ensure must not wait on the server's pipes")
        self.assertEqual(("ready", lease), (ready["status"], ready["lease_id"]))
        pid = ready["server_pid"]
        self.assertTrue(self.alive(pid))
        self.assertEqual(f"http://127.0.0.1:{self.port}", ready["endpoint"])

        # The lock belongs to the command, not to the server it started.
        with Store(self.home, lock_timeout=0.5).lock():
            pass

        before = self.request_log.read_text().splitlines()
        renewed = self.result(self.run_cli("renew", lease, str(uuid4())))
        self.assertEqual("renewed", renewed["status"])
        self.assertEqual(before, self.request_log.read_text().splitlines(), "renew probed the server")

        released = self.result(self.run_cli("release", lease, str(uuid4())))
        self.assertEqual("warm_idle", released["phase"])
        self.assertTrue(self.alive(pid), "release keeps the server warm")

        second = str(uuid4())
        again = self.result(self.ensure(second))
        self.assertEqual(pid, again["server_pid"], "a warm server is reused, not restarted")

        stopped = self.result(self.run_cli("stop", second, str(uuid4())))
        self.assertEqual("stopped", stopped["phase"])
        wait_for(lambda: not self.alive(pid))
        self.assertFalse(self.alive(pid))
        self.assert_no_fake_server()

    def test_status_and_identity_observe_the_live_server(self) -> None:
        self.result(self.ensure())
        status = self.result(self.run_cli("status"))
        self.assertTrue(status["observed"]["process_owned"])
        self.assertTrue(status["observed"]["health_ok"])
        identity = self.result(self.run_cli("identity"))
        self.assertEqual([ALIAS], identity["servable"])
        self.assertEqual("build 11200", identity["build"])
        self.result(self.run_cli("stop", self.state()["lease_id"], str(uuid4())))

    def test_preflight_fills_the_hash_cache(self) -> None:
        self.assertEqual("verified", self.result(self.run_cli("preflight"))["status"])
        cache = json.loads((self.home / "artifact-cache.json").read_text())
        self.assertIn(str(self.model), cache["models"])
        self.assertIn(str(self.binary), cache["servers"])


class FailedEnsureTests(RealCase):
    """A failed ensure leaves no server behind, whatever made it fail."""

    def assert_clean_failure(self, message: str) -> None:
        error = self.error(self.ensure(), 4)
        self.assertRegex(error["message"], message)
        self.assertEqual("failed", self.state()["phase"])
        self.assert_no_fake_server()
        # The slot is free again: nothing holds the lock or the port.
        with Store(self.home, lock_timeout=0.5).lock():
            pass
        self.assertFalse(self.system.port_in_use("127.0.0.1", self.port))

    def run_with(self, **env: str) -> None:
        self.server_env = env
        self.write_manifest()

    def test_server_crashes_at_start(self) -> None:
        self.run_with(FAKE_EXIT_CODE="3")
        self.assert_clean_failure("exited while loading")

    def test_server_never_becomes_ready(self) -> None:
        self.startup_seconds = 3
        self.run_with(FAKE_STARTUP_DELAY="60")
        self.assert_clean_failure("not ready after 3s")

    def test_wrong_context(self) -> None:
        self.run_with(FAKE_CTX_DELTA="1")
        self.assert_clean_failure("context does not match")

    def test_slow_decode_is_a_vram_spill(self) -> None:
        self.warmup = {"min_tokens_per_second": 30}
        self.run_with(FAKE_TPS="5")
        self.assert_clean_failure("below the minimum 30.*VRAM spill")

    def test_thinking_on(self) -> None:
        self.warmup = {"require_no_reasoning": True}
        self.run_with(FAKE_REASONING="1")
        self.assert_clean_failure("thinking is on")

    def test_required_marker_missing(self) -> None:
        self.markers = ["creating MTP draft context"]
        self.run_with()
        self.assert_clean_failure("marker is absent")

    def test_a_server_that_wants_a_key_the_profile_lacks_fails_at_once(self) -> None:
        self.startup_seconds = 15
        key_file = self.tmp / "server.key"
        key_file.write_text(ApiKeyTests.KEY + "\n")
        self.run_with(FAKE_API_KEY_FILE=str(key_file))
        started = time.monotonic()
        self.assert_clean_failure(
            "HTTP 401 from /health: llama-server wants an API key; set server.api_key_file"
        )
        self.assertLess(time.monotonic() - started, 10, "a refused key is final; ensure must not wait it out")


class ApiKeyTests(RealCase):
    KEY = "test-key-" + "0" * 32  # a fixture, not a secret
    warmup = {"min_tokens_per_second": 1}

    def setUp(self) -> None:
        super().setUp()
        self.key_file = self.tmp / "llamacpp.key"
        self.key_file.write_text(self.KEY + "\n")
        self.key_file.chmod(0o600)
        self.server_keys = {"api_key_file": str(self.key_file), "slots_endpoint": False}
        self.write_manifest()

    def get(self, path: str, key: Optional[str] = None) -> int:
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}")
        if key:
            request.add_header("Authorization", f"Bearer {key}")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            with opener.open(request, timeout=5) as reply:
                return int(reply.status)
        except urllib.error.HTTPError as exc:
            exc.close()
            return exc.code

    def test_the_server_runs_behind_the_key_and_no_output_shows_it(self) -> None:
        lease = str(uuid4())
        runs = [self.ensure(lease)]
        ready = self.result(runs[0])
        runs.append(self.run_cli("status"))
        self.assertTrue(self.result(runs[-1])["observed"]["health_ok"])
        runs.append(self.run_cli("preflight"))
        self.assertEqual("verified", self.result(runs[-1])["status"])

        command = self.fake_server_processes()[ready["server_pid"]]
        self.assertIn(f"--api-key-file {self.key_file}", command)
        self.assertIn("--no-slots", command)
        self.assertNotIn(self.KEY, command)
        self.assertEqual(401, self.get("/props"), "the server answered without the key")
        self.assertEqual(200, self.get("/props", self.KEY))
        self.assertEqual(501, self.get("/slots", self.KEY))

        runs.append(self.run_cli("stop", lease, str(uuid4())))
        self.result(runs[-1])
        text = "".join(run.stdout + run.stderr for run in runs)
        for path in self.home.rglob("*"):
            if path.is_file():
                text += path.read_text(errors="replace")
        self.assertNotIn(self.KEY, text)
        self.assert_no_fake_server()

    def test_a_key_file_others_can_read_is_refused_and_nothing_starts(self) -> None:
        self.key_file.chmod(0o644)
        error = self.error(self.ensure(), 4)
        self.assertRegex(error["message"], "api_key_file .* is accessible to group or others.*chmod 600")
        self.assertNotIn(self.KEY, json.dumps(error))
        self.assertEqual([], self.fake_servers())


class InterruptionTests(RealCase):
    startup_seconds = 60
    server_env = {"FAKE_STARTUP_DELAY": "120"}

    def wait_until_starting(self, process: subprocess.Popen) -> Dict[str, Any]:
        def starting() -> Optional[Dict[str, Any]]:
            try:
                state = self.state()
            except (OSError, ValueError):
                return None
            return state if state.get("phase") == "starting" and self.alive(state["server_pid"]) else None

        state = wait_for(starting, 20)
        self.assertIsNotNone(state, "ensure never reached the starting phase")
        return state

    def test_the_node_killing_the_process_group_takes_the_server_with_it(self) -> None:
        process = self.popen_cli("ensure", ALIAS, str(uuid4()), str(uuid4()))
        state = self.wait_until_starting(process)
        os.killpg(process.pid, signal.SIGKILL)  # what the node does when an ensure times out
        process.communicate()
        wait_for(lambda: not self.alive(state["server_pid"]), 10)
        self.assertFalse(self.alive(state["server_pid"]))
        self.assert_no_fake_server()
        # The state still says "starting"; the next sweep recognises the abandoned start.
        swept = self.result(self.run_cli("sweep"))
        self.assertEqual(("stopped", "abandoned_start"), (swept["phase"], swept["last_stop_reason"]))

    def test_sigterm_during_startup_removes_the_server(self) -> None:
        process = self.popen_cli("ensure", ALIAS, str(uuid4()), str(uuid4()))
        state = self.wait_until_starting(process)
        process.send_signal(signal.SIGTERM)
        _, stderr = process.communicate(timeout=30)
        self.assertEqual(143, process.returncode, stderr)
        self.assertTrue(wait_for(lambda: not self.alive(state["server_pid"]), 10))
        self.assert_no_fake_server()
        self.assertEqual("failed", self.state()["phase"])


class SafetyTests(RealCase):
    def test_a_listener_on_the_port_is_refused_and_left_alone(self) -> None:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", self.port))
            listener.listen()
            error = self.error(self.ensure(), 3)
            self.assertIn(f"port {self.port}", error["message"])
            self.assertTrue(self.system.port_in_use("127.0.0.1", self.port), "the listener was disturbed")
        self.assert_no_fake_server()

    def test_an_unrelated_process_on_a_recorded_pid_is_never_signalled(self) -> None:
        bystander = subprocess.Popen(["sleep", "60"])
        self.addCleanup(bystander.wait)  # cleanups run last-in first: kill, then reap
        self.addCleanup(bystander.kill)
        info = self.system.inspect(bystander.pid)
        assert info is not None
        self.home.mkdir()
        (self.home / "state.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "revision": 1,
                    "phase": "warm_idle",
                    "model": ALIAS,
                    "server_pid": bystander.pid,
                    "release_deadline_epoch": 0,
                    "owner": {
                        "binary": str(self.binary),
                        "started": info.started,
                        "fragments": [f"--port {self.port}"],
                    },
                    "policy": {"warm_seconds": 600, "lease_timeout_seconds": 300},
                }
            )
        )
        swept = self.result(self.run_cli("sweep"))
        self.assertEqual("server_not_running", swept["last_stop_reason"])
        time.sleep(0.2)
        self.assertIsNone(bystander.poll(), "the bystander was killed")

    def test_the_process_this_runtime_started_is_recognised_and_stopped(self) -> None:
        lease = str(uuid4())
        pid = self.result(self.ensure(lease))["server_pid"]
        status = self.result(self.run_cli("status"))
        self.assertTrue(status["observed"]["process_owned"])
        self.result(self.run_cli("stop", lease, str(uuid4())))
        self.assertTrue(wait_for(lambda: not self.alive(pid)))


class SweeperTests(RealCase):
    warm_seconds = 1
    sweeper = "detached"

    def test_the_detached_sweeper_ends_a_warm_server_and_then_itself(self) -> None:
        lease = str(uuid4())
        ready = self.result(self.ensure(lease))
        state = self.state()
        sweeper_pid = state["sweeper"]["pid"]
        self.assertTrue(self.alive(sweeper_pid))
        self.result(self.run_cli("release", lease, str(uuid4())))

        self.assertTrue(wait_for(lambda: not self.alive(ready["server_pid"]), 12), "warm window not enforced")
        self.assertTrue(wait_for(lambda: not self.alive(sweeper_pid), 12), "the sweeper outlived its server")
        final = self.state()
        self.assertEqual(("stopped", "warm_window_over"), (final["phase"], final["last_stop_reason"]))
        self.assert_no_fake_server()

    def test_stopping_also_ends_the_sweeper(self) -> None:
        lease = str(uuid4())
        self.result(self.ensure(lease))
        sweeper_pid = self.state()["sweeper"]["pid"]
        self.result(self.run_cli("stop", lease, str(uuid4())))
        self.assertTrue(wait_for(lambda: not self.alive(sweeper_pid), 10))


class SystemTests(unittest.TestCase):
    def test_terminate_stops_and_reaps_a_child(self) -> None:
        system = detect_system()
        tmp = Path(tempfile.mkdtemp(prefix="llamacpp-term-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        pid = system.spawn_helper(["sleep", "60"], dict(os.environ), tmp / "helper.log")
        self.assertTrue(system.alive(pid))
        system.terminate(pid, grace_seconds=5)
        self.assertFalse(system.alive(pid))

    def test_terminate_escalates_to_sigkill(self) -> None:
        system = detect_system()
        tmp = Path(tempfile.mkdtemp(prefix="llamacpp-term-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        stubborn = (
            "import signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);"
            "print('up',flush=True);time.sleep(60)"
        )
        log = tmp / "stubborn.log"
        pid = system.spawn_helper([sys.executable, "-c", stubborn], dict(os.environ), log)
        wait_for(lambda: "up" in log.read_text())
        system.terminate(pid, grace_seconds=0.3)
        self.assertFalse(system.alive(pid))

    def test_a_binary_whose_version_command_fails_is_reported(self) -> None:
        from poundlock_llamacpp.errors import ManagerError

        tmp = Path(tempfile.mkdtemp(prefix="llamacpp-version-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        broken = tmp / "llama-server"
        broken.write_text(
            "#!/bin/sh\necho 'error while loading shared libraries: libllama.so' >&2\nexit 127\n"
        )
        broken.chmod(0o755)
        with self.assertRaisesRegex(ManagerError, "exited 127: error while loading shared libraries"):
            detect_system().server_version(broken)
        with self.assertRaisesRegex(ManagerError, "cannot run"):
            detect_system().server_version(tmp / "missing")

    def test_the_server_environment_reaches_the_version_command(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="llamacpp-version-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        probe = tmp / "llama-server"
        probe.write_text("#!/bin/sh\necho \"version: 11200 ($EXAMPLE_LIBS)\"\n")
        probe.chmod(0o755)
        output = detect_system().server_version(probe, {**os.environ, "EXAMPLE_LIBS": "abcdef012"})
        self.assertIn("(abcdef012)", output)

    def test_terminate_never_signals_itself_or_pid_zero(self) -> None:
        detect_system().terminate(os.getpid())
        detect_system().terminate(0)
        detect_system().terminate(-1)

    def test_port_in_use_sees_a_listener(self) -> None:
        system = detect_system()
        port = free_port()
        self.assertFalse(system.port_in_use("127.0.0.1", port))
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", port))
            listener.listen()
            self.assertTrue(system.port_in_use("127.0.0.1", port))

    def test_http_json_ignores_proxy_settings_and_reports_failures_briefly(self) -> None:
        from poundlock_llamacpp.system import RequestFailed

        system = detect_system()
        env = {"http_proxy": "http://127.0.0.1:9", "HTTP_PROXY": "http://127.0.0.1:9"}
        saved = {key: os.environ.get(key) for key in env}
        os.environ.update(env)
        self.addCleanup(lambda: [os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v)
                                 for k, v in saved.items()])
        with self.assertRaises(RequestFailed) as caught:
            system.http_json("GET", f"http://127.0.0.1:{free_port()}/health", timeout=1)
        self.assertNotIn("proxy", str(caught.exception).lower())
        self.assertLess(len(str(caught.exception)), 400)


if __name__ == "__main__":
    unittest.main()
