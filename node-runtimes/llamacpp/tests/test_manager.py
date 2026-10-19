"""Lease lifecycle against a simulated host: no real processes, sockets or clocks."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import List
from uuid import uuid4

from poundlock_llamacpp.errors import ConflictError, InfrastructureError, ManagerError
from poundlock_llamacpp.manifest import load_manifest

from .fakes import (
    ALIAS,
    BINARY,
    MODEL,
    LifecycleCase,
    manifest_dict,
    profile_dict,
)

WARMUP = {
    "min_tokens_per_second": 30,
    "require_no_reasoning": True,
    "request": {"reasoning_effort": "none"},
}


class EnsureTests(LifecycleCase):
    def test_cold_start_builds_the_command_and_reports_evidence(self) -> None:
        self.edit_profile(required_log_markers=["layers to GPU"], warmup=WARMUP)
        lease = str(uuid4())
        result = self.ensure(lease)

        self.assertEqual("ready", result["status"])
        self.assertEqual(lease, result["lease_id"])
        self.assertEqual("http://127.0.0.1:8080", result["endpoint"])
        self.assertEqual(
            [
                BINARY, "--model", MODEL, "--alias", ALIAS, "--ctx-size", "4096",
                "--host", "127.0.0.1", "--port", "8080", "--parallel", "1", "--jinja",
            ],
            self.system.spawns[0],
        )
        evidence = result["evidence"]
        self.assertEqual("Example-7B", evidence["artifact"]["model_family"])
        self.assertEqual("compatible", evidence["artifact"]["server_compatibility"])
        self.assertEqual(11200, evidence["artifact"]["server_build_number"])
        self.assertEqual(["layers to GPU"], evidence["log_markers"])
        self.assertEqual(60.0, evidence["warmup"]["tokens_per_second"])
        self.assertEqual(self.now + 300, result["lease_deadline_epoch"])

    def test_environment_is_profile_env_over_platform_default(self) -> None:
        environment = {"XDG_CACHE_HOME": "/x", "A": "1"}
        self.edit_profile(server={**profile_dict()["server"], "environment": environment})
        self.ensure()
        env = self.system.spawn_env[0]
        self.assertEqual("/x", env["XDG_CACHE_HOME"])
        self.assertEqual("1", env["A"])

    def test_platform_default_cache_dir_is_used_when_profile_is_silent(self) -> None:
        self.ensure()
        self.assertEqual(str(self.home / "cache"), self.system.spawn_env[0]["XDG_CACHE_HOME"])

    def test_same_operation_id_is_a_replay(self) -> None:
        lease, operation = self.ids()
        first = self.manager.ensure(ALIAS, lease, operation)
        second = self.manager.ensure(ALIAS, lease, operation)
        self.assertEqual(first["revision"], second["revision"])
        self.assertEqual(1, len(self.system.spawns))

    def test_a_retry_after_the_server_was_swept_starts_again_instead_of_replaying(self) -> None:
        lease, operation = self.ids()
        self.manager.ensure(ALIAS, lease, operation)
        self.advance(301)
        self.manager.sweep()
        again = self.manager.ensure(ALIAS, lease, operation)
        self.assertEqual("ready", again["status"])
        self.assertEqual(2, len(self.system.spawns))

    def test_a_retry_after_a_refused_ensure_is_not_mistaken_for_a_completed_one(self) -> None:
        other = profile_dict(server={**profile_dict()["server"], "port": 8081})
        self.write_manifest(manifest_dict({ALIAS: profile_dict(), "other-model": other}))
        first = str(uuid4())
        self.ensure(first)
        self.manager.release(first, str(uuid4()))
        lease, operation = self.ids()
        self.system.port_busy = True
        with self.assertRaisesRegex(ConflictError, "port 8081"):
            self.manager.ensure("other-model", lease, operation)  # stopped the warm one first
        self.system.port_busy = False
        retried = self.manager.ensure("other-model", lease, operation)
        self.assertEqual(("ready", "other-model"), (retried["status"], retried["model"]))

    def test_a_second_lease_is_refused_while_the_first_is_live(self) -> None:
        self.ensure()
        with self.assertRaisesRegex(ConflictError, "held by another lease") as caught:
            self.ensure()
        self.assertEqual(3, caught.exception.exit_code)
        self.assertEqual([], self.system.terminated)

    def test_an_expired_lease_can_be_taken_over_without_a_restart(self) -> None:
        self.ensure()
        self.advance(301)
        newer = str(uuid4())
        result = self.ensure(newer)
        self.assertEqual(newer, result["lease_id"])
        self.assertEqual(1, len(self.system.spawns))

    def test_the_same_lease_may_ensure_again(self) -> None:
        lease = str(uuid4())
        first = self.ensure(lease)
        self.advance(100)
        second = self.ensure(lease)
        self.assertGreater(second["lease_deadline_epoch"], first["lease_deadline_epoch"])
        self.assertEqual(1, len(self.system.spawns))

    def test_the_same_lease_cannot_ensure_after_its_profile_was_edited(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.edit_profile(warm_seconds=900)
        with self.assertRaisesRegex(ConflictError, "profile changed while this lease is active"):
            self.ensure(lease)
        self.assertEqual([], self.system.terminated, "the leased server must not be restarted")

    def test_the_same_lease_cannot_switch_models(self) -> None:
        other = {"other-model": profile_dict()}
        self.write_manifest(manifest_dict({ALIAS: profile_dict(), **other}))
        lease = str(uuid4())
        self.ensure(lease)
        with self.assertRaisesRegex(ConflictError, "different model"):
            self.ensure(lease, alias="other-model")

    def test_unknown_model_is_refused_before_anything_starts(self) -> None:
        with self.assertRaisesRegex(ManagerError, "not in the manifest"):
            self.ensure(alias="not-listed")
        self.assertEqual([], self.system.spawns)

    def test_a_stale_ready_state_restarts_when_the_server_died(self) -> None:
        self.ensure()
        self.system.kill_externally(self.system.server_pid)
        self.advance(301)
        self.ensure()
        self.assertEqual(2, len(self.system.spawns))

    def test_ensure_recovers_from_an_abandoned_start(self) -> None:
        self.ensure()
        state = self.state()
        state["phase"] = "starting"
        (self.home / "state.json").write_text(json.dumps(state))
        result = self.ensure()
        self.assertEqual("ready", result["status"])
        self.assertIn(state["server_pid"], self.system.terminated)
        self.assertEqual(2, len(self.system.spawns))


class WarmIdleTests(LifecycleCase):
    def test_release_keeps_the_server_warm_then_sweep_stops_it(self) -> None:
        lease, operation = self.ids()
        self.manager.ensure(ALIAS, lease, operation)
        released = self.manager.release(lease, str(uuid4()))
        self.assertEqual("released", released["status"])
        self.assertEqual("warm_idle", released["phase"])
        self.assertIsNone(released["lease_id"])
        self.assertEqual(1, len(self.system.running()))

        self.advance(599)
        self.assertEqual("kept", self.manager.sweep()["status"])
        self.advance(2)
        stopped = self.manager.sweep()
        self.assertEqual("stopped", stopped["phase"])
        self.assertEqual("warm_window_over", stopped["last_stop_reason"])
        self.assert_nothing_running()

    def test_a_warm_server_is_reused_by_the_next_lease(self) -> None:
        first = str(uuid4())
        self.ensure(first)
        self.manager.release(first, str(uuid4()))
        second = str(uuid4())
        result = self.ensure(second)
        self.assertEqual("ready", result["phase"])
        self.assertEqual(second, result["lease_id"])
        self.assertEqual(1, len(self.system.spawns))
        self.assertIsNone(self.state()["release_deadline_epoch"])

    def test_warm_seconds_zero_stops_on_release(self) -> None:
        self.edit_profile(warm_seconds=0)
        lease = str(uuid4())
        self.ensure(lease)
        released = self.manager.release(lease, str(uuid4()))
        self.assertEqual("released", released["last_stop_reason"])
        self.assert_nothing_running()

    def test_a_different_model_replaces_the_warm_one(self) -> None:
        second = profile_dict(server={**profile_dict()["server"], "port": 8081})
        self.write_manifest(manifest_dict({ALIAS: profile_dict(), "other-model": second}))
        lease = str(uuid4())
        self.ensure(lease)
        first_pid = self.system.server_pid
        self.manager.release(lease, str(uuid4()))
        result = self.ensure(alias="other-model")
        self.assertEqual("other-model", result["model"])
        self.assertIn(first_pid, self.system.terminated)
        self.assertIn("--alias other-model", self.system.inspect(self.system.server_pid).command)

    def test_an_edited_profile_restarts_a_warm_server(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        self.edit_profile(warm_seconds=900)
        self.ensure()
        self.assertEqual(2, len(self.system.spawns))

    def test_an_unhealthy_warm_server_is_restarted(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        self.system.n_ctx = 999  # /props of the warm server no longer matches
        self.ensure()
        self.assertEqual(2, len(self.system.spawns))


class RenewReleaseStopTests(LifecycleCase):
    def test_renew_extends_the_lease_without_probing_the_server(self) -> None:
        lease = str(uuid4())
        first = self.ensure(lease)
        requests = list(self.system.requests)
        self.advance(240)
        renewed = self.manager.renew(lease, str(uuid4()))
        self.assertEqual("renewed", renewed["status"])
        self.assertGreater(renewed["lease_deadline_epoch"], first["lease_deadline_epoch"])
        self.assertEqual(requests, self.system.requests, "renew must not call the server")
        self.advance(61)
        self.assertEqual("kept", self.manager.sweep()["status"])

    def test_renew_for_another_lease_is_a_conflict(self) -> None:
        self.ensure()
        with self.assertRaisesRegex(ConflictError, "active lease"):
            self.manager.renew(str(uuid4()), str(uuid4()))

    def test_renew_reports_a_lost_server_and_cleans_up(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.system.kill_externally(self.system.server_pid)
        with self.assertRaisesRegex(InfrastructureError, "lease is lost"):
            self.manager.renew(lease, str(uuid4()))
        self.assertEqual("stopped", self.state()["phase"])
        self.assert_nothing_running()

    def test_release_of_an_unknown_lease_changes_nothing(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        result = self.manager.release(str(uuid4()), str(uuid4()))
        self.assertEqual("ignored", result["status"])
        self.assertEqual("ready", self.state()["phase"])
        self.assertEqual(lease, self.state()["lease_id"])

    def test_release_is_idempotent_across_retries(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        again = self.manager.release(lease, str(uuid4()))
        self.assertEqual("released", again["status"])
        self.assertEqual("warm_idle", again["phase"])

    def test_stop_ends_the_matching_lease_only(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        ignored = self.manager.stop(str(uuid4()), str(uuid4()))
        self.assertEqual("ignored", ignored["status"])
        self.assertEqual(1, len(self.system.running()))
        stopped = self.manager.stop(lease, str(uuid4()))
        self.assertEqual("stopped", stopped["phase"])
        self.assertEqual("explicit_stop", stopped["last_stop_reason"])
        self.assert_nothing_running()

    def test_stop_accepts_the_lease_that_released_a_warm_server(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        self.assertEqual("ignored", self.manager.stop(str(uuid4()), str(uuid4()))["status"])
        self.manager.stop(lease, str(uuid4()))
        self.assert_nothing_running()

    def test_stop_when_already_stopped_is_quiet(self) -> None:
        result = self.manager.stop(str(uuid4()), str(uuid4()))
        self.assertEqual("already_stopped", result["status"])

    def test_release_stop_and_renew_work_after_the_manifest_is_gone(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manifest_path.unlink()
        self.manager.renew(lease, str(uuid4()))
        self.manager.release(lease, str(uuid4()))
        self.manager.stop(lease, str(uuid4()))
        self.assert_nothing_running()


class SweepTests(LifecycleCase):
    def test_lease_watchdog_stops_a_server_nobody_renews(self) -> None:
        self.ensure()
        self.advance(299)
        self.assertEqual("kept", self.manager.sweep()["status"])
        self.advance(2)
        stopped = self.manager.sweep()
        self.assertEqual("lease_expired", stopped["last_stop_reason"])
        self.assert_nothing_running()

    def test_sweep_clears_state_when_the_server_died(self) -> None:
        self.ensure()
        self.system.kill_externally(self.system.server_pid)
        result = self.manager.sweep()
        self.assertEqual("server_not_running", result["last_stop_reason"])

    def test_sweep_stops_an_abandoned_start(self) -> None:
        self.ensure()
        state = self.state()
        state["phase"] = "starting"
        (self.home / "state.json").write_text(json.dumps(state))
        result = self.manager.sweep()
        self.assertEqual("abandoned_start", result["last_stop_reason"])
        self.assert_nothing_running()

    def test_sweep_is_idle_when_nothing_runs(self) -> None:
        self.assertEqual("idle", self.manager.sweep()["status"])

    def test_sweep_yields_to_an_operation_in_progress(self) -> None:
        self.ensure()
        self.advance(10_000)
        with self.manager.store.lock():
            self.assertEqual({"status": "busy"}, self.manager.sweep())
        self.assertEqual("stopped", self.manager.sweep()["phase"])

    def test_actions_report_busy_when_the_lock_stays_held(self) -> None:
        with self.manager.store.lock():
            other = self.make_manager()
            with self.assertRaisesRegex(ConflictError, "busy"):
                other.stop(str(uuid4()), str(uuid4()))

    def test_run_sweeper_exits_once_its_server_is_gone(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        pid = self.system.server_pid
        ticks = []
        self.system.sleep_hook = lambda: ticks.append(1)
        self.manager.stop(lease, str(uuid4()))
        self.manager.run_sweeper(pid, 30)
        self.assertEqual(1, len(ticks))


class OwnershipTests(LifecycleCase):
    def _plant(self, command: str, started: str = "x1") -> int:
        from poundlock_llamacpp.system import ProcessInfo

        pid = 4242
        self.system.procs[pid] = ProcessInfo(pid, started, command, tuple(command.split()), BINARY)
        return pid

    def _state_for(self, pid: int, started: str = "x1") -> None:
        self.home.mkdir(parents=True, exist_ok=True)
        (self.home / "state.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "revision": 1,
                    "phase": "ready",
                    "model": ALIAS,
                    "lease_id": str(uuid4()),
                    "lease_deadline_epoch": self.now - 1,
                    "server_pid": pid,
                    "owner": {
                        "binary": BINARY,
                        "started": started,
                        "fragments": [f"--model {MODEL}", f"--alias {ALIAS}", "--port 8080"],
                    },
                    "policy": {"warm_seconds": 600, "lease_timeout_seconds": 300},
                }
            )
        )

    def test_a_recorded_pid_running_something_else_is_never_signalled(self) -> None:
        pid = self._plant("/usr/bin/other-program --serve")
        self._state_for(pid)
        self.manager.sweep()
        self.assertEqual([], self.system.terminated)
        self.assertIn(pid, self.system.procs)
        self.assertEqual("stopped", self.state()["phase"])

    def test_a_reused_pid_with_a_matching_command_is_never_signalled(self) -> None:
        command = f"{BINARY} --model {MODEL} --alias {ALIAS} --port 8080"
        pid = self._plant(command, started="later-boot")
        self._state_for(pid, started="original")
        self.manager.sweep()
        self.assertEqual([], self.system.terminated)

    def test_the_process_this_runtime_started_is_signalled(self) -> None:
        command = f"{BINARY} --model {MODEL} --alias {ALIAS} --port 8080"
        pid = self._plant(command)
        self._state_for(pid)
        self.manager.sweep()
        self.assertEqual([pid], self.system.terminated)

    def test_a_llama_server_started_by_someone_else_is_refused_and_left_alone(self) -> None:
        self.system.foreign[777] = "llama-server"
        with self.assertRaisesRegex(ConflictError, "did not start"):
            self.ensure()
        self.assertEqual([], self.system.spawns)
        self.assertEqual([], self.system.terminated)

    def test_a_busy_port_is_refused(self) -> None:
        self.system.port_busy = True
        with self.assertRaisesRegex(ConflictError, "port 8080"):
            self.ensure()
        self.assertEqual([], self.system.spawns)

    def test_the_process_name_check_can_be_narrowed_in_the_manifest(self) -> None:
        self.write_manifest(manifest_dict(foreign_process_names=[]))
        self.system.foreign[777] = "llama-server"
        self.system.foreign[778] = "llama-server"
        # Only the profile's own binary name is always checked; here it is the same name.
        with self.assertRaisesRegex(ConflictError, "did not start"):
            self.ensure()


class PeerInstanceTests(LifecycleCase):
    """A second slot on one machine: another instance's server is not foreign."""

    PEER_COMMAND = f"{BINARY} --model /models/peer.gguf --alias peer --port 8081"

    def peer_state(self, pid: int, *, running: bool = True) -> Path:
        from poundlock_llamacpp.system import ProcessInfo

        peer = self.tmp / "peer-home"
        peer.mkdir()
        owner = {"binary": BINARY, "started": "p1", "fragments": ["--alias peer", "--port 8081"]}
        (peer / "state.json").write_text(
            json.dumps(
                {"schema_version": 1, "revision": 1, "phase": "ready", "server_pid": pid, "owner": owner}
            )
        )
        if running:
            command = self.PEER_COMMAND
            self.system.procs[pid] = ProcessInfo(pid, "p1", command, tuple(command.split()), BINARY)
        return peer

    def test_a_peer_instances_server_does_not_block_a_start(self) -> None:
        peer = self.peer_state(777)
        self.write_manifest(manifest_dict(peer_homes=[str(peer)]))
        self.system.foreign[777] = "llama-server"
        self.ensure()
        self.assertEqual(1, len(self.system.spawns))
        self.assertEqual([], self.system.terminated)

    def test_a_stale_peer_record_does_not_exempt_a_reused_pid(self) -> None:
        # The peer's state names pid 777, but 777 is now someone else's llama-server.
        peer = self.peer_state(777, running=False)
        self.write_manifest(manifest_dict(peer_homes=[str(peer)]))
        self.system.foreign[777] = "llama-server"
        with self.assertRaisesRegex(ConflictError, "pid 777"):
            self.ensure()
        self.assertEqual([], self.system.spawns)

    def test_a_server_no_peer_recorded_is_still_refused(self) -> None:
        peer = self.peer_state(777)
        self.write_manifest(manifest_dict(peer_homes=[str(peer)]))
        self.system.foreign[778] = "llama-server"
        with self.assertRaisesRegex(ConflictError, "pid 778"):
            self.ensure()
        self.assertEqual([], self.system.spawns)

    def test_peer_homes_must_be_absolute(self) -> None:
        self.write_manifest(manifest_dict(peer_homes=["relative/home"]))
        with self.assertRaisesRegex(Exception, "absolute"):
            load_manifest(self.manifest_path)


class FailedStartTests(LifecycleCase):
    """A failed ensure must leave no server, sweeper or helper behind."""

    def assert_failed_cleanly(self, message: str) -> None:
        with self.assertRaisesRegex(InfrastructureError, message):
            self.ensure()
        self.assert_nothing_running()
        state = self.state()
        self.assertEqual("failed", state["phase"])
        self.assertRegex(state["failure"], message)
        self.assertNotIn("lease_id", state)

    def test_never_ready(self) -> None:
        self.system.loading_polls = 10**9
        self.assert_failed_cleanly("not ready after 60s")

    def test_server_exits_while_loading(self) -> None:
        self.system.sleep_hook = lambda: self.system.kill_externally(self.system.server_pid)
        self.system.loading_polls = 1
        self.assert_failed_cleanly("exited while loading")

    def test_a_server_still_starting_is_not_taken_for_an_exit(self) -> None:
        # Seen on a Linux node after sleep: the first look finds the new process before
        # exec has set up its command line.
        self.system.exec_pending_inspects = 2
        result = self.ensure()
        self.assertEqual("ready", result["status"])

    def test_server_dies_at_once(self) -> None:
        self.system.crash_after_spawn = True
        self.assert_failed_cleanly("exited while loading")

    def test_missing_log_marker(self) -> None:
        self.edit_profile(required_log_markers=["creating MTP draft context"])
        self.assert_failed_cleanly("marker is absent: creating MTP draft context")

    def test_wrong_context(self) -> None:
        self.system.spawn_n_ctx = 2048
        self.assert_failed_cleanly("context does not match")

    def test_decode_speed_below_minimum(self) -> None:
        self.edit_profile(warmup=WARMUP)
        self.system.tps = 12.0
        self.assert_failed_cleanly(r"12\.0 tok/s is below the minimum 30.*VRAM spill")

    def test_thinking_left_on(self) -> None:
        self.edit_profile(warmup=WARMUP)
        self.system.reasoning = "let me think"
        self.assert_failed_cleanly("thinking is on")

    def test_warmup_request_fails(self) -> None:
        self.edit_profile(warmup=WARMUP)
        self.system.warmup_error = True
        self.assert_failed_cleanly("warm-up request failed")

    def test_interrupt_during_load(self) -> None:
        def interrupt() -> None:
            raise KeyboardInterrupt

        self.system.loading_polls = 5
        self.system.sleep_hook = interrupt
        with self.assertRaises(KeyboardInterrupt):
            self.ensure()
        self.assert_nothing_running()
        self.assertEqual("failed", self.state()["phase"])

    def test_detached_sweeper_and_caffeinate_die_with_a_failed_start(self) -> None:
        self.write_manifest(manifest_dict(sweeper="detached", caffeinate=True))
        self.system.supports_caffeinate = True
        self.system.loading_polls = 10**9
        with self.assertRaises(InfrastructureError):
            self.ensure()
        self.assertEqual(2, len(self.system.helpers))
        self.assert_nothing_running()

    def test_a_failed_start_does_not_block_the_next_one(self) -> None:
        self.system.loading_polls = 10**9
        with self.assertRaises(InfrastructureError):
            self.ensure()
        self.system.loading_polls = 0
        self.assertEqual("ready", self.ensure()["status"])

    def test_artifact_failures_start_nothing(self) -> None:
        for label, change, message in (
            ("size", lambda s: setattr(s, "model_size", 5), "size mismatch"),
            ("sha", lambda s: setattr(s, "model_hash", "cd" * 32), "sha256 mismatch"),
            ("old build", lambda s: setattr(s, "version_text", "version: 10000 (abc1234)"), "older than"),
            ("no binary", lambda s: setattr(s, "executable_ok", False), "not an executable"),
        ):
            with self.subTest(label):
                self.setUp()
                change(self.system)
                with self.assertRaisesRegex(InfrastructureError, message):
                    self.ensure()
                self.assertEqual([], self.system.spawns)
                self.assertEqual([], self.system.unloaded)

    def test_a_binary_that_cannot_run_is_reported_before_anything_starts(self) -> None:
        self.system.version_error = "/opt/llama/llama-server --version exited 127: libllama.so not found"
        with self.assertRaisesRegex(ManagerError, "libllama.so not found"):
            self.ensure()
        self.assertEqual([], self.system.spawns)

    def test_version_is_probed_with_the_servers_environment(self) -> None:
        self.ensure()
        assert self.system.version_env is not None
        self.assertEqual("1", self.system.version_env["GGML_EXAMPLE"])

    def test_a_server_that_cannot_be_executed_leaves_no_state_or_process(self) -> None:
        self.system.spawn_error = PermissionError("denied")
        with self.assertRaisesRegex(InfrastructureError, "cannot start .*denied"):
            self.ensure()
        self.assert_nothing_running()
        self.assertFalse((self.home / "state.json").exists())

    def test_caffeinate_is_refused_where_it_does_not_exist(self) -> None:
        self.write_manifest(manifest_dict(caffeinate=True))
        with self.assertRaisesRegex(InfrastructureError, "only available on macOS"):
            self.ensure()
        self.assertEqual([], self.system.spawns)


class ArtifactTests(LifecycleCase):
    def test_hash_is_computed_once_and_cached(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.stop(lease, str(uuid4()))
        self.ensure()
        self.assertEqual(1, self.system.hash_calls)
        self.assertEqual(1, self.system.version_calls)

    def test_a_changed_file_is_hashed_again(self) -> None:
        self.manager.preflight()
        (self.home / "artifact-cache.json").write_text(
            json.dumps({"models": {MODEL: {"size_bytes": 1234, "mtime_ns": 999, "sha256": "ab" * 32}}})
        )
        self.manager.preflight()
        self.assertEqual(2, self.system.hash_calls)

    def test_unrecognised_version_is_recorded_not_fatal(self) -> None:
        self.system.version_text = "custom build"
        evidence = self.ensure()["evidence"]["artifact"]
        self.assertEqual("unknown_version", evidence["server_compatibility"])

    def test_preflight_names_each_model_and_starts_nothing(self) -> None:
        result = self.manager.preflight(ALIAS)
        self.assertEqual("verified", result["status"])
        self.assertEqual(64, len(result["models"][ALIAS]["profile_digest"]))
        self.assertEqual([], self.system.spawns)


class ConflictTests(LifecycleCase):
    def test_ollama_models_are_unloaded_before_the_server_starts(self) -> None:
        self.edit_profile(conflicts=["ollama"])
        self.system.ollama_models = ["qwen3:8b", "llama3:8b"]
        result = self.ensure()
        self.assertEqual(["unload qwen3:8b", "unload llama3:8b", "spawn"], self.system.events)
        self.assertEqual(["qwen3:8b", "llama3:8b"], result["evidence"]["conflicts_unloaded"])

    def test_ollama_not_running_is_fine(self) -> None:
        self.edit_profile(conflicts=["ollama"])
        self.system.ollama_up = False
        self.assertEqual("ready", self.ensure()["status"])

    def test_ollama_is_left_alone_unless_the_profile_names_it(self) -> None:
        self.system.ollama_models = ["qwen3:8b"]
        self.ensure()
        self.assertEqual([], self.system.unloaded)

    def test_start_waits_for_vram_to_drain_and_gives_up_if_it_does_not(self) -> None:
        gib = 1024**3
        self.edit_profile(conflicts=["ollama"], max_idle_vram_bytes=3 * gib, vram_settle_seconds=5)
        busy = [{"card": "card1", "vram_used": 14 * gib, "vram_total": 16 * gib, "gtt_used": 0}]
        self.system.gpu = busy
        self.system.ollama_models = ["qwen3:8b"]
        self.system.gpu_after_unload = [{**busy[0], "vram_used": 1 * gib}]
        result = self.ensure()
        self.assertEqual(1 * gib, result["evidence"]["idle_vram_before_start"])

    def test_vram_that_never_drains_refuses_the_start(self) -> None:
        gib = 1024**3
        self.edit_profile(max_idle_vram_bytes=3 * gib, vram_settle_seconds=5)
        self.system.gpu = [{"card": "card1", "vram_used": 14 * gib, "vram_total": None, "gtt_used": 0}]
        with self.assertRaisesRegex(ConflictError, "VRAM still holds"):
            self.ensure()
        self.assertEqual([], self.system.spawns)

    def test_no_gpu_readings_skips_the_vram_gate(self) -> None:
        self.edit_profile(max_idle_vram_bytes=1)
        self.assertEqual("ready", self.ensure()["status"])


class HelperTests(LifecycleCase):
    def test_detached_sweeper_starts_once_and_ends_with_the_server(self) -> None:
        self.write_manifest(manifest_dict(sweeper="detached", sweep_interval_seconds=7))
        lease = str(uuid4())
        self.ensure(lease)
        self.assertEqual(1, len(self.system.helpers))
        command = self.system.helpers[0]
        self.assertEqual(["python3", "-m", "poundlock_llamacpp"], command[:3])
        self.assertIn("sweeper", command)
        self.assertEqual("7", command[command.index("--interval") + 1])
        self.assertEqual(str(self.system.server_pid), command[command.index("--watch-pid") + 1])

        self.manager.release(lease, str(uuid4()))
        self.ensure()
        self.assertEqual(1, len(self.system.helpers), "reuse must not start a second sweeper")

        self.manager.stop(self.state()["lease_id"], str(uuid4()))
        self.assert_nothing_running()

    def test_a_lost_sweeper_is_replaced_when_the_warm_server_is_reused(self) -> None:
        self.write_manifest(manifest_dict(sweeper="detached"))
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        self.system.kill_externally(self.state()["sweeper"]["pid"])
        self.ensure()
        self.assertEqual(2, len(self.system.helpers))

    def test_external_sweeping_starts_no_helper(self) -> None:
        self.write_manifest(manifest_dict(sweeper="external"))
        self.ensure()
        self.assertEqual([], self.system.helpers)

    def test_the_platform_default_decides_when_the_manifest_is_silent(self) -> None:
        self.system.default_sweeper = "detached"
        self.ensure()
        self.assertEqual(1, len(self.system.helpers))

    def test_caffeinate_holds_the_server_pid_and_stops_with_it(self) -> None:
        self.write_manifest(manifest_dict(caffeinate=True))
        self.system.supports_caffeinate = True
        lease = str(uuid4())
        self.ensure(lease)
        self.assertEqual(
            ["caffeinate", "-dimsu", "-w", str(self.system.server_pid)], self.system.helpers[0]
        )
        self.manager.stop(lease, str(uuid4()))
        self.assert_nothing_running()


class ReportingTests(LifecycleCase):
    def test_identity_lists_servable_models_and_the_only_profile(self) -> None:
        identity = self.manager.identity()
        self.assertEqual([ALIAS], identity["servable"])
        self.assertEqual("Example-7B Q4_K_M", identity["model"])
        self.assertEqual("build 11200", identity["build"])
        self.assertEqual("abcdef012", identity["commit"])

    def test_identity_describes_every_servable_model_from_the_manifest(self) -> None:
        other = profile_dict(model={**profile_dict()["model"], "family": "Other-70B"})
        other["server"] = {**other["server"], "port": other["server"]["port"] + 1, "context_tokens": 65536}
        self.write_manifest(manifest_dict({ALIAS: profile_dict(), "zz-other": other}))
        identity = self.manager.identity()
        default_context = profile_dict()["server"]["context_tokens"]
        self.assertEqual(
            [
                {"id": ALIAS, "label": "Example-7B Q4_K_M", "context_tokens": default_context},
                {"id": "zz-other", "label": "Other-70B Q4_K_M", "context_tokens": 65536},
            ],
            identity["models"],
        )

    def test_identity_reuses_the_cached_version(self) -> None:
        self.manager.identity()
        self.manager.identity()
        self.assertEqual(1, self.system.version_calls)

    def test_identity_with_several_profiles_names_the_loaded_one_only(self) -> None:
        second = profile_dict(model={**profile_dict()["model"], "family": "Other-70B"})
        self.write_manifest(manifest_dict({ALIAS: profile_dict(), "zz-other": second}))
        idle = self.manager.identity()
        self.assertEqual([ALIAS, "zz-other"], idle["servable"])
        self.assertNotIn("model", idle)
        self.ensure(alias="zz-other")
        self.assertEqual("Other-70B Q4_K_M", self.manager.identity()["model"])

    def test_identity_survives_a_binary_that_will_not_run(self) -> None:
        self.system.executable_ok = False
        identity = self.manager.identity()
        self.assertNotIn("build", identity)
        self.assertEqual([ALIAS], identity["servable"])

    def test_status_reports_the_live_observation(self) -> None:
        self.ensure()
        observed = self.manager.status()["observed"]
        self.assertTrue(observed["process_owned"])
        self.assertTrue(observed["health_ok"])
        self.system.kill_externally(self.system.server_pid)
        self.assertFalse(self.manager.status()["observed"]["process_owned"])

    def test_status_when_nothing_ever_ran(self) -> None:
        result = self.manager.status()
        self.assertEqual("stopped", result["phase"])
        self.assertFalse(result["observed"]["process_owned"])


class SecretTests(LifecycleCase):
    SECRET_ENV = "env-secret-7f3a"
    SECRET_ARG = "arg-secret-91bc"

    def test_secrets_in_the_profile_never_reach_output_state_or_logs(self) -> None:
        server = {
            **profile_dict()["server"],
            "environment": {"HF_TOKEN": self.SECRET_ENV},
            "arguments": ["--hf-token", self.SECRET_ARG],
        }
        self.edit_profile(server=server)
        lease = str(uuid4())
        outputs = [self.ensure(lease)]
        outputs.append(self.manager.renew(lease, str(uuid4())))
        outputs.append(self.manager.status())
        outputs.append(self.manager.identity())
        outputs.append(self.manager.release(lease, str(uuid4())))
        outputs.append(self.manager.sweep())
        outputs.append(self.manager.preflight())
        self.manager.stop(lease, str(uuid4()))

        self.assertIn(self.SECRET_ENV, self.system.spawn_env[0].get("HF_TOKEN", ""))
        self.assertIn(self.SECRET_ARG, self.system.spawns[0])
        text = json.dumps(outputs)
        for path in self.home.rglob("*"):
            if path.is_file():
                text += path.read_text(errors="replace")
        self.assertNotIn(self.SECRET_ENV, text)
        self.assertNotIn(self.SECRET_ARG, text)


class ApiKeyTests(LifecycleCase):
    KEY = "test-key-" + "0" * 32  # a fixture, not a secret
    NEW_KEY = "next-key-" + "0" * 32  # a fixture, not a secret

    def setUp(self) -> None:
        super().setUp()
        self.key_file = self.tmp / "llamacpp.key"
        self.write_key(self.KEY)
        self.use_key_file(self.key_file)

    def write_key(self, content: str, mode: int = 0o600) -> None:
        self.key_file.write_text(content + "\n")
        self.key_file.chmod(mode)

    def use_key_file(self, path: Path) -> None:
        self.edit_profile(
            server={**profile_dict()["server"], "api_key_file": str(path)}, warmup=WARMUP
        )

    def server_requests(self) -> List[str]:
        return [request for request in self.system.requests if ":8080" in request]

    def test_every_probe_sends_the_key(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.assertTrue(self.manager.status()["observed"]["health_ok"])
        self.manager.release(lease, str(uuid4()))
        self.ensure()  # the warm server is probed again before reuse
        self.assertEqual(1, len(self.system.spawns))
        paths = {request.rsplit(":8080", 1)[1] for request in self.server_requests()}
        self.assertEqual({"/health", "/v1/models", "/props", "/v1/chat/completions"}, paths)
        self.assertEqual(len(self.server_requests()), len(self.system.sent_keys))
        self.assertEqual({self.KEY}, set(self.system.sent_keys))

    def test_the_key_never_reaches_argv_output_state_or_logs(self) -> None:
        lease = str(uuid4())
        outputs = [self.ensure(lease)]
        outputs.append(self.manager.renew(lease, str(uuid4())))
        outputs.append(self.manager.status())
        outputs.append(self.manager.identity())
        outputs.append(self.manager.release(lease, str(uuid4())))
        outputs.append(self.manager.sweep())
        outputs.append(self.manager.preflight())
        outputs.append(self.manager.stop(lease, str(uuid4())))

        argv = self.system.spawns[0]
        self.assertEqual(str(self.key_file), argv[argv.index("--api-key-file") + 1])
        self.assertTrue(outputs[0]["evidence"]["artifact"]["api_key_required"])
        text = json.dumps([outputs, self.system.spawns, self.system.helpers, self.system.spawn_env])
        for path in self.home.rglob("*"):
            if path.is_file():
                text += path.read_text(errors="replace")
        self.assertNotIn(self.KEY, text)

    def test_a_server_that_wants_a_key_the_profile_lacks_fails_at_once(self) -> None:
        self.edit_profile(server=profile_dict()["server"])
        self.system.required_key = self.KEY
        started = self.now
        with self.assertRaisesRegex(
            InfrastructureError,
            "HTTP 401 from /health: llama-server wants an API key; set server.api_key_file",
        ):
            self.ensure()
        self.assertLess(self.now - started, 5, "a refused key is final; ensure must not wait it out")
        self.assertEqual("failed", self.state()["phase"])
        self.assert_nothing_running()

    def test_a_changed_key_is_reported_and_the_next_ensure_restarts_with_it(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        self.manager.release(lease, str(uuid4()))
        self.write_key(self.NEW_KEY)
        observed = self.manager.status()["observed"]
        self.assertFalse(observed["health_ok"])
        self.assertRegex(observed["health_error"], "HTTP 401 .*refused the key in server.api_key_file")
        self.assertNotIn(self.NEW_KEY, json.dumps(observed))
        self.ensure()
        self.assertEqual(2, len(self.system.spawns))
        self.assertEqual(self.NEW_KEY, self.system.server_key)
        self.assertEqual(self.NEW_KEY, self.system.sent_keys[-1])

    def test_an_unsafe_key_file_is_refused_before_anything_starts(self) -> None:
        directory = self.tmp / "keys"
        directory.mkdir()
        link = self.tmp / "link.key"
        link.symlink_to(self.key_file)
        fifo = self.tmp / "fifo.key"
        os.mkfifo(fifo)
        safe_text = "x" * 40
        for name, path, content, mode, message in (
            ("group-readable", self.key_file, self.KEY, 0o640, "is accessible to group or others.*chmod 600"),
            ("world-readable", self.key_file, self.KEY, 0o604, "is accessible to group or others.*chmod 600"),
            ("group-writable", self.key_file, self.KEY, 0o620, "is accessible to group or others.*chmod 600"),
            ("symlink", link, self.KEY, 0o600, "is a symlink"),
            ("directory", directory, self.KEY, 0o600, "is not a regular file"),
            ("fifo", fifo, self.KEY, 0o600, "is not a regular file"),
            ("missing", self.tmp / "missing.key", self.KEY, 0o600, "is unreadable"),
            ("short", self.key_file, "short-key", 0o600, "must hold one key"),
            ("two keys", self.key_file, f"{self.KEY}\n{safe_text}", 0o600, "must hold one key"),
            ("comment", self.key_file, "#" + safe_text, 0o600, "must hold one key"),
            ("CRLF", self.key_file, self.KEY + "\r", 0o600, "must hold one key"),
            ("spaces", self.key_file, safe_text + " " + safe_text, 0o600, "must hold one key"),
        ):
            with self.subTest(name):
                self.write_key(content, mode)
                self.use_key_file(path)
                with self.assertRaisesRegex(
                    InfrastructureError, f"server.api_key_file {re.escape(str(path))} {message}"
                ) as caught:
                    self.ensure()
                self.assertNotIn(content.strip()[:16], str(caught.exception))
                with self.assertRaisesRegex(InfrastructureError, message):
                    self.manager.preflight()
                self.assertEqual([], self.system.spawns)
                self.assertEqual(0, self.system.hash_calls, "refused before hashing the model")
        self.write_key(self.KEY)
        self.use_key_file(self.key_file)
        self.assertEqual("ready", self.ensure()["status"])


class DeploymentShapeTests(LifecycleCase):
    def test_state_and_logs_live_under_the_configured_home_only(self) -> None:
        self.ensure()
        for path in self.home.rglob("*"):
            self.assertTrue(str(path).startswith(str(self.home)))
        self.assertTrue((self.home / "state.json").exists())
        self.assertTrue((self.home / "operation.lock").exists())
        self.assertTrue((self.home / "logs" / "manager.log").exists())

    def test_state_file_is_private(self) -> None:
        self.ensure()
        mode = (self.home / "state.json").stat().st_mode & 0o777
        self.assertEqual(0o600, mode)

    def test_each_start_prunes_old_server_logs(self) -> None:
        logs = self.home / "logs"
        logs.mkdir(parents=True)
        for index in range(15):
            old = logs / f"old{index:02d}.server.log"
            old.write_text("x")
            os.utime(old, (1_000 + index, 1_000 + index))
        self.ensure()
        remaining = sorted(path.name for path in logs.glob("*.server.log"))
        self.assertEqual(10, len(remaining))
        self.assertNotIn("old00.server.log", remaining)
        self.assertIn("old14.server.log", remaining)
        self.assertTrue(any(not name.startswith("old") for name in remaining), "the new log is kept")

    def test_manager_log_records_events_without_arguments(self) -> None:
        lease = str(uuid4())
        self.ensure(lease)
        text = (self.home / "logs" / "manager.log").read_text()
        self.assertIn(lease, text)
        self.assertIn("ensure", text)
        self.assertNotIn("--jinja", text)

