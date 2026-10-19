"""The installed artifact: a zipapp built by deploy/install.sh, run like the node runs it."""

from __future__ import annotations

import json
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from uuid import uuid4

import poundlock_llamacpp

from .test_processes import ALIAS, RealCase, wait_for

DEPLOY = Path(__file__).resolve().parents[1] / "deploy"
INSTALL = DEPLOY / "install.sh"


def run_install(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(INSTALL), "--python", sys.executable, *args],
        capture_output=True, text=True, timeout=60,
    )


class InstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="llamacpp-install-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.prefix = self.tmp / "prefix"
        self.binary = self.prefix / "bin" / "poundlock-llamacpp"

    def test_installs_one_executable_and_the_examples_and_nothing_else(self) -> None:
        result = run_install("--prefix", str(self.prefix))
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertTrue(os.access(self.binary, os.X_OK))
        self.assertEqual(
            ["linux-amd-vulkan-mtp.json", "macos-metal.json"],
            sorted(p.name for p in (self.prefix / "share/poundlock-llamacpp/examples").iterdir()),
        )
        self.assertEqual(["bin", "share"], sorted(p.name for p in self.prefix.iterdir()))
        version = subprocess.run([str(self.binary), "--version"], capture_output=True, text=True)
        self.assertIn(f"poundlock-llamacpp {poundlock_llamacpp.__version__}", version.stdout)
        self.assertEqual("#!" + sys.executable, self.binary.read_bytes().split(b"\n")[0].decode())

    def test_the_installed_command_keeps_its_exit_codes(self) -> None:
        # The node judges each command by its exit status, never by its output.
        run_install("--prefix", str(self.prefix))
        manifest = self.tmp / "manifest.json"
        manifest.write_text('{"schema_version": 1, "profiles": {}}')
        manifest.chmod(0o600)
        result = subprocess.run(
            [str(self.binary), "--home", str(self.tmp / "home"), "--manifest", str(manifest),
             "ensure", "model", "not-a-uuid", str(uuid4())],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(2, result.returncode, result.stderr)
        self.assertEqual("ManagerError", json.loads(result.stderr)["error"])

    def test_uninstall_removes_only_what_it_installed(self) -> None:
        run_install("--prefix", str(self.prefix))
        state = self.tmp / "home" / "state.json"
        state.parent.mkdir()
        state.write_text("{}")
        result = run_install("--uninstall", "--prefix", str(self.prefix))
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertFalse(self.binary.exists())
        self.assertFalse((self.prefix / "share/poundlock-llamacpp").exists())
        self.assertTrue(state.exists())

    def test_launchd_job_is_written_with_the_neutral_label(self) -> None:
        agents = self.tmp / "agents"
        result = run_install(
            "--prefix", str(self.prefix), "--launchd", "--no-load",
            "--launch-agents", str(agents),
            "--home", str(self.tmp / "home dir"), "--manifest", str(self.tmp / "m.json"),
        )
        self.assertEqual(0, result.returncode, result.stderr)
        job = plistlib.loads((agents / "com.poundlock.llamacpp.plist").read_bytes())
        self.assertEqual("com.poundlock.llamacpp", job["Label"])
        self.assertEqual(str(self.binary), job["ProgramArguments"][0])
        self.assertEqual(str(self.tmp / "home dir"), job["ProgramArguments"][2])
        removed = run_install("--uninstall", "--prefix", str(self.prefix), "--launchd",
                              "--no-load", "--launch-agents", str(agents))
        self.assertEqual(0, removed.returncode, removed.stderr)
        self.assertFalse((agents / "com.poundlock.llamacpp.plist").exists())

    def test_relative_or_missing_python_is_refused(self) -> None:
        result = subprocess.run(
            ["bash", str(INSTALL), "--prefix", str(self.prefix), "--python", "python3"],
            capture_output=True, text=True,
        )
        self.assertNotEqual(0, result.returncode)
        self.assertFalse(self.binary.exists())

    @unittest.skipIf(sys.platform == "darwin", "Linux refuses to load a launchd job")
    def test_launchd_is_refused_off_macos_unless_only_writing(self) -> None:
        result = run_install("--prefix", str(self.prefix), "--launchd")
        self.assertEqual(2, result.returncode)


class MetadataTests(unittest.TestCase):
    def test_pyproject_version_matches_the_package(self) -> None:
        import re

        from poundlock_llamacpp import __version__

        text = (DEPLOY.parent / "pyproject.toml").read_text()
        self.assertEqual(__version__, re.search(r'^version = "([^"]+)"', text, re.M).group(1))  # type: ignore[union-attr]
        self.assertIn('poundlock-llamacpp = "poundlock_llamacpp.cli:main"', text)


class InstalledRuntimeTests(RealCase):
    """The zipapp must start its own detached sweeper: `python -m` cannot see a .pyz."""

    warm_seconds = 1
    sweeper = "detached"

    def test_zipapp_runs_the_lifecycle_and_starts_its_sweeper(self) -> None:
        prefix = self.tmp / "prefix"
        installed = run_install("--prefix", str(prefix))
        self.assertEqual(0, installed.returncode, installed.stderr)
        binary = prefix / "bin" / "poundlock-llamacpp"
        base = [str(binary), "--home", str(self.home), "--manifest", str(self.manifest)]
        env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}  # nothing but the .pyz

        def run(*args: str) -> subprocess.CompletedProcess:
            return subprocess.run([*base, *args], capture_output=True, text=True, timeout=60,
                                  env=env, stdin=subprocess.DEVNULL, start_new_session=True)

        lease = str(uuid4())
        ready = run("ensure", ALIAS, lease, str(uuid4()))
        self.assertEqual(0, ready.returncode, ready.stderr)
        state = self.state()
        sweeper_pid = state["sweeper"]["pid"]
        self.assertTrue(self.alive(sweeper_pid), "the sweeper did not survive its parent")
        command = self.system.inspect(sweeper_pid).command  # type: ignore[union-attr]
        self.assertIn("-m poundlock_llamacpp", command)

        released = run("release", lease, str(uuid4()))
        self.assertEqual(0, released.returncode, released.stderr)
        # Enforcing the warm window proves the sweeper imported the package from the .pyz.
        self.assertTrue(wait_for(lambda: not self.alive(state["server_pid"]), 12))
        self.assertTrue(wait_for(lambda: not self.alive(sweeper_pid), 12))
        final = json.loads((self.home / "state.json").read_text())
        self.assertEqual("warm_window_over", final["last_stop_reason"])
