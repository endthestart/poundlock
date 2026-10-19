"""Command line contract with the node agent: arguments in, one JSON object out."""

from __future__ import annotations

import contextlib
import io
import json
import os
import plistlib
import signal
import sys
from typing import Any, Dict, Optional, Tuple
from unittest import mock
from uuid import uuid4

from poundlock_llamacpp import __version__, cli
from poundlock_llamacpp.errors import ManagerError

from .fakes import ALIAS, LifecycleCase, manifest_dict


class CliCase(LifecycleCase):
    def run_cli(self, *argv: str, env: Optional[Dict[str, str]] = None) -> Tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        base = {k: v for k, v in os.environ.items() if not k.startswith("POUNDLOCK_LLAMACPP")}
        with mock.patch.dict(os.environ, {**base, **(env or {})}, clear=True):
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = cli.main(["--home", str(self.home), "--manifest", str(self.manifest_path), *argv],
                                system=self.system)
        return code, out.getvalue(), err.getvalue()

    def ok(self, *argv: str) -> Dict[str, Any]:
        code, out, err = self.run_cli(*argv)
        self.assertEqual(0, code, err)
        return json.loads(out)

    def fails(self, *argv: str) -> Tuple[int, Dict[str, Any]]:
        code, out, err = self.run_cli(*argv)
        self.assertEqual("", out, "a failure must not print a result")
        return code, json.loads(err)


class ActionTests(CliCase):
    def test_the_node_call_sequence_returns_json_objects(self) -> None:
        lease, ensure_op, renew_op, release_op, stop_op = (str(uuid4()) for _ in range(5))
        ready = self.ok("ensure", ALIAS, lease, ensure_op)
        self.assertEqual(("ready", lease), (ready["status"], ready["lease_id"]))
        self.assertEqual("renewed", self.ok("renew", lease, renew_op)["status"])
        self.assertEqual("released", self.ok("release", lease, release_op)["status"])
        self.assertEqual("stopped", self.ok("stop", lease, stop_op)["phase"])

    def test_the_model_argument_is_accepted_on_every_command(self) -> None:
        lease = str(uuid4())
        self.ok("ensure", ALIAS, lease, str(uuid4()))
        self.assertEqual("renewed", self.ok("renew", ALIAS, lease, str(uuid4()))["status"])
        self.assertEqual("released", self.ok("release", ALIAS, lease, str(uuid4()))["status"])
        self.assertEqual("stopped", self.ok("stop", ALIAS, lease, str(uuid4()))["phase"])
        self.assertEqual("Example-7B Q4_K_M", self.ok("identity", ALIAS)["model"])
        code, error = self.fails("identity", "not-listed")
        self.assertEqual((2, "ManagerError"), (code, error["error"]))

    def test_lease_commands_check_their_argument_shape(self) -> None:
        lease, operation = str(uuid4()), str(uuid4())
        code, error = self.fails("stop", lease)
        self.assertEqual(2, code)
        self.assertIn("takes [MODEL] LEASE_ID OPERATION_ID", error["message"])
        code, error = self.fails("stop", "a", "b", lease, operation)
        self.assertEqual(2, code)
        code, error = self.fails("stop", "--bad model", lease, operation)
        self.assertIn("not a valid model id", error["message"])

    def test_read_only_commands(self) -> None:
        self.assertEqual([ALIAS], self.ok("identity")["servable"])
        self.assertEqual("stopped", self.ok("status")["phase"])
        self.assertEqual("idle", self.ok("sweep")["status"])
        self.assertEqual("verified", self.ok("preflight")["status"])
        self.assertEqual("verified", self.ok("preflight", ALIAS)["status"])

    def test_ids_must_be_canonical_uuids(self) -> None:
        good = str(uuid4())
        for bad_lease, message in (("not-a-uuid", "lease_id must be a UUID"),
                                   (good.upper(), "canonical lowercase")):
            with self.subTest(bad_lease):
                code, error = self.fails("ensure", ALIAS, bad_lease, good)
                self.assertEqual(2, code)
                self.assertEqual("ManagerError", error["error"])
                self.assertIn(message, error["message"])
        code, error = self.fails("release", good, "../../etc/passwd")
        self.assertIn("operation_id", error["message"])
        self.assertEqual([], self.system.spawns)

    def test_exit_codes_and_error_shape(self) -> None:
        self.ok("ensure", ALIAS, str(uuid4()), str(uuid4()))
        code, error = self.fails("ensure", ALIAS, str(uuid4()), str(uuid4()))
        self.assertEqual((3, "ConflictError"), (code, error["error"]))
        self.assertEqual(1, error["schema_version"])
        code, error = self.fails("ensure", "unknown-model", str(uuid4()), str(uuid4()))
        self.assertEqual((2, "ManagerError"), (code, error["error"]))
        self.system.loading_polls = 0
        self.manifest_path.unlink()
        code, error = self.fails("identity")
        self.assertEqual((4, "InfrastructureError"), (code, error["error"]))

    def test_unexpected_exceptions_become_clean_json_with_exit_4(self) -> None:
        with mock.patch("poundlock_llamacpp.manager.Manager.identity", side_effect=RuntimeError("boom")):
            code, error = self.fails("identity")
        self.assertEqual((4, "RuntimeError", "boom"), (code, error["error"], error["message"]))

    def test_a_terminating_signal_ends_cleanly_without_output(self) -> None:
        with mock.patch("poundlock_llamacpp.cli.dispatch", side_effect=cli.Terminated("signal 15")):
            code, error = self.fails("ensure", ALIAS, str(uuid4()), str(uuid4()))
        self.assertEqual(143, code)
        self.assertEqual("Terminated", error["error"])

    def test_signal_handlers_are_restored_after_each_run(self) -> None:
        before = {n: signal.getsignal(n) for n in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)}
        self.ok("ensure", ALIAS, str(uuid4()), str(uuid4()))
        self.assertEqual(before, {n: signal.getsignal(n) for n in before})

    def test_sweep_that_finds_the_lock_busy_still_exits_zero(self) -> None:
        with self.manager.store.lock():
            self.assertEqual({"status": "busy"}, self.ok("sweep"))


class ConfigurationTests(CliCase):
    def test_home_and_manifest_come_from_the_environment(self) -> None:
        out, err = io.StringIO(), io.StringIO()
        env = {
            "POUNDLOCK_LLAMACPP_HOME": str(self.home),
            "POUNDLOCK_LLAMACPP_MANIFEST": str(self.manifest_path),
        }
        with mock.patch.dict(os.environ, env):
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = cli.main(["status"], system=self.system)
        self.assertEqual(0, code, err.getvalue())

    def test_the_manifest_defaults_to_the_home_directory(self) -> None:
        (self.home).mkdir(parents=True, exist_ok=True)
        (self.home / "manifest.json").write_text(json.dumps(manifest_dict()))
        with mock.patch.dict(os.environ, {}, clear=False):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = cli.main(["--home", str(self.home), "identity"], system=self.system)
        self.assertEqual(0, code)
        self.assertEqual([ALIAS], json.loads(out.getvalue())["servable"])

    def test_the_hidden_sweeper_command_is_not_advertised_but_works(self) -> None:
        self.assertNotIn("sweeper", cli.build_parser().format_help().replace("launchd-plist", ""))
        lease = str(uuid4())
        self.ok("ensure", ALIAS, lease, str(uuid4()))
        pid = self.system.server_pid
        self.ok("stop", lease, str(uuid4()))
        self.system.sleep_hook = None
        self.assertEqual(
            "sweeper_exited",
            self.ok("sweeper", "--watch-pid", str(pid), "--interval", "0.01")["status"],
        )

    def test_version_flag(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as caught:
            cli.main(["--version"], system=self.system)
        self.assertEqual(0, caught.exception.code)
        self.assertIn(__version__, out.getvalue())

    def test_launchd_plist_command_prints_the_job(self) -> None:
        buffer = io.BytesIO()
        wrapper = io.TextIOWrapper(buffer, write_through=True)
        with mock.patch.object(sys, "stdout", wrapper):
            code = cli.main(
                ["--home", str(self.home), "--manifest", str(self.manifest_path),
                 "launchd-plist", "--executable", "/usr/local/bin/poundlock-llamacpp"],
                system=self.system,
            )
        self.assertEqual(0, code)
        job = plistlib.loads(buffer.getvalue())
        self.assertEqual("com.poundlock.llamacpp", job["Label"])
        self.assertEqual(str(self.home), job["ProgramArguments"][2])
        self.assertEqual("sweep", job["ProgramArguments"][-1])

    def test_require_uuid_returns_the_value(self) -> None:
        value = str(uuid4())
        self.assertEqual(value, cli.require_uuid(value, "lease_id"))
        with self.assertRaises(ManagerError):
            cli.require_uuid("", "lease_id")

