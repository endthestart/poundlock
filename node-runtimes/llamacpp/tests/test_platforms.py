"""Platform adapters: Linux against a fake /proc and /sys, macOS against canned `ps`."""

from __future__ import annotations

import os
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Dict, List, Optional
from unittest import mock

from poundlock_llamacpp import platforms
from poundlock_llamacpp.errors import ManagerError
from poundlock_llamacpp.launchd import LAUNCHD_LABEL, render_plist
from poundlock_llamacpp.linux import LinuxSystem
from poundlock_llamacpp.macos import MacSystem, parse_ps_line
from poundlock_llamacpp.system import ProcessInfo, script_interpreter_runs


class LinuxCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="llamacpp-linux-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.proc = self.tmp / "proc"
        self.sys = self.tmp / "sys"
        self.proc.mkdir()
        self.binary = self.tmp / "opt" / "llama-server"
        self.binary.parent.mkdir()
        self.binary.write_bytes(b"\x7fELF fake")
        self.system = LinuxSystem(proc_root=self.proc, sysfs_root=self.sys)

    def add_process(
        self,
        pid: int,
        argv: List[str],
        *,
        state: str = "S",
        comm: str = "llama-server",
        exe: Optional[str] = None,
        start: int = 4242,
    ) -> None:
        base = self.proc / str(pid)
        base.mkdir()
        # Field 2 is the command in parentheses and may itself contain ") " and spaces.
        fields = [state, "1", "1", "1", "0", "-1", "0", "0", "0", "0", "0", "0", "0", "0", "0",
                  "20", "0", "1", "0", str(start), "0"]
        (base / "stat").write_text(f"{pid} ({comm} (x) y) " + " ".join(fields) + "\n")
        (base / "comm").write_text(comm + "\n")
        (base / "cmdline").write_bytes(b"\0".join(part.encode() for part in argv) + b"\0")
        if exe is not None:
            os.symlink(exe, base / "exe")


class LinuxProcessTests(LinuxCase):
    def test_inspect_reads_argv_exe_and_the_start_time_token(self) -> None:
        self.add_process(10, [str(self.binary), "--port", "8080"], exe=str(self.binary), start=777)
        info = self.system.inspect(10)
        assert info is not None
        self.assertEqual("777", info.started)
        self.assertEqual(f"{self.binary} --port 8080", info.command)
        self.assertEqual((str(self.binary), "--port", "8080"), info.argv)

    def test_a_new_process_on_a_reused_pid_has_a_different_token(self) -> None:
        self.add_process(10, [str(self.binary)], exe=str(self.binary), start=100)
        first = self.system.inspect(10)
        shutil.rmtree(self.proc / "10")
        self.add_process(10, [str(self.binary)], exe=str(self.binary), start=200)
        self.assertNotEqual(first.started, self.system.inspect(10).started)  # type: ignore[union-attr]

    def test_missing_zombie_and_invalid_pids_are_not_processes(self) -> None:
        self.add_process(11, [str(self.binary)], state="Z", exe=None)
        self.assertIsNone(self.system.inspect(11))
        self.assertIsNone(self.system.inspect(99999))
        self.assertIsNone(self.system.inspect(0))
        self.assertIsNone(self.system.inspect(-5))

    def test_runs_binary_by_exe_link_symlink_and_replaced_file(self) -> None:
        link = self.tmp / "opt" / "llama-server-current"
        os.symlink(self.binary, link)
        self.add_process(12, [str(link)], exe=str(self.binary))
        info = self.system.inspect(12)
        assert info is not None
        self.assertTrue(self.system.runs_binary(info, link))
        self.assertTrue(self.system.runs_binary(info, self.binary))
        deleted = ProcessInfo(12, "1", "x", ("x",), str(self.binary) + " (deleted)")
        self.assertTrue(self.system.runs_binary(deleted, self.binary), "an upgraded binary is still ours")

    def test_a_different_executable_is_not_the_binary(self) -> None:
        other = self.tmp / "opt" / "other"
        other.write_bytes(b"\x7fELF other")
        self.add_process(13, [str(self.binary)], exe=str(other))
        info = self.system.inspect(13)
        assert info is not None
        self.assertFalse(self.system.runs_binary(info, self.binary))

    def test_a_script_binary_is_recognised_by_its_interpreter_command(self) -> None:
        script = self.tmp / "opt" / "server-script"
        script.write_text("#!/usr/bin/env python3\n")
        info = ProcessInfo(14, "1", f"/usr/bin/python3 {script} --port 1",
                           ("/usr/bin/python3", str(script), "--port", "1"), "/usr/bin/python3")
        self.assertTrue(self.system.runs_binary(info, script))
        self.assertFalse(self.system.runs_binary(info, self.binary))
        elf_info = ProcessInfo(15, "1", f"/usr/bin/python3 {self.binary}",
                               ("/usr/bin/python3", str(self.binary)), "/usr/bin/python3")
        self.assertFalse(self.system.runs_binary(elf_info, self.binary), "only #! files count as scripts")

    def test_find_processes_matches_the_name_exactly_and_skips_zombies(self) -> None:
        self.add_process(20, ["a"], comm="llama-server", exe=str(self.binary))
        self.add_process(21, ["b"], comm="llama-server-x", exe=str(self.binary))
        self.add_process(22, ["c"], comm="llama-server", state="Z")
        self.add_process(23, ["d"], comm="python3", exe=str(self.binary))
        (self.proc / "self").mkdir()
        self.assertEqual([20], self.system.find_processes("llama-server"))

    def test_long_names_match_the_kernel_truncation(self) -> None:
        self.add_process(24, ["a"], comm="llama-server-vu", exe=str(self.binary))
        self.assertEqual([24], self.system.find_processes("llama-server-vulkan"))

    def test_find_processes_without_a_proc_tree(self) -> None:
        self.assertEqual([], LinuxSystem(proc_root=self.tmp / "nope").find_processes("llama-server"))


class LinuxSettingsTests(LinuxCase):
    def add_card(
        self, name: str, used: Optional[int], total: Optional[int] = None, gtt: Optional[int] = None
    ) -> None:
        device = self.sys / "class" / "drm" / name / "device"
        device.mkdir(parents=True)
        for filename, value in (("mem_info_vram_used", used), ("mem_info_vram_total", total),
                                ("mem_info_gtt_used", gtt)):
            if value is not None:
                (device / filename).write_text(f"{value}\n")

    def test_gpu_memory_reads_amd_sysfs_per_card(self) -> None:
        self.add_card("card0", None)  # an integrated or non-AMD card: no readings
        self.add_card("card1", 2_000, total=16_000, gtt=500)
        (self.sys / "class" / "drm" / "card1-DP-1").mkdir()
        self.assertEqual(
            [{"card": "card1", "vram_used": 2_000, "vram_total": 16_000, "gtt_used": 500}],
            self.system.gpu_memory(),
        )

    def test_gpu_memory_without_sysfs_is_empty(self) -> None:
        self.assertEqual([], self.system.gpu_memory())

    def test_garbled_readings_are_skipped(self) -> None:
        self.add_card("card1", None)
        (self.sys / "class" / "drm" / "card1" / "device" / "mem_info_vram_used").write_text("n/a")
        self.assertEqual([], self.system.gpu_memory())

    def test_shader_cache_goes_under_the_runtime_home(self) -> None:
        home = Path("/var/lib/poundlock/llamacpp")
        self.assertEqual({"XDG_CACHE_HOME": str(home / "cache")}, self.system.server_environment(home))

    def test_default_home_follows_xdg_state(self) -> None:
        with mock.patch.dict(os.environ, {"XDG_STATE_HOME": "/srv/state"}):
            self.assertEqual(Path("/srv/state/poundlock-llamacpp"), self.system.default_home())
        env = {k: v for k, v in os.environ.items() if k != "XDG_STATE_HOME"}
        with mock.patch.dict(os.environ, env, clear=True):
            self.assertEqual(
                Path.home() / ".local" / "state" / "poundlock-llamacpp", self.system.default_home()
            )

    def test_linux_sweeps_with_a_detached_process_and_never_caffeinates(self) -> None:
        self.assertEqual("detached", self.system.default_sweeper)
        self.assertFalse(self.system.supports_caffeinate)
        self.assertIsNone(self.system.caffeinate_command(1))


class MacTests(unittest.TestCase):
    PS = "Ss   Tue Sep 29 14:58:00 2026 /opt/homebrew/bin/llama-server --model /m.gguf --port 8080"

    def test_parse_ps_line(self) -> None:
        info = parse_ps_line(5, self.PS + "\n")
        assert info is not None
        self.assertEqual("Tue Sep 29 14:58:00 2026", info.started)
        self.assertEqual("/opt/homebrew/bin/llama-server --model /m.gguf --port 8080", info.command)

    def test_single_digit_days_and_spaces_in_paths(self) -> None:
        line = "S    Tue Sep  9 04:05:06 2026 /Users/x/Library/Application Support/llama-server --port 1"
        info = parse_ps_line(5, line)
        assert info is not None
        self.assertEqual("Tue Sep 9 04:05:06 2026", info.started)
        self.assertTrue(info.command.startswith("/Users/x/Library/Application Support/llama-server"))

    def test_zombies_and_short_output_are_not_processes(self) -> None:
        self.assertIsNone(parse_ps_line(5, "Z    Tue Sep 29 14:58:00 2026 /x"))
        self.assertIsNone(parse_ps_line(5, ""))
        self.assertIsNone(parse_ps_line(5, "S Tue"))

    def test_inspect_uses_ps_and_a_missing_pid_is_none(self) -> None:
        system = MacSystem()
        with mock.patch.object(system, "_run", return_value=self.PS) as run:
            info = system.inspect(5)
        assert info is not None
        self.assertEqual(["/bin/ps", "-ww", "-p", "5", "-o", "stat=", "-o", "lstart=", "-o", "command="],
                         run.call_args[0][0])
        with mock.patch.object(system, "_run", return_value=None):
            self.assertIsNone(system.inspect(5))
        self.assertIsNone(system.inspect(0))

    def test_ps_runs_in_utc_so_the_start_token_never_depends_on_the_callers_timezone(self) -> None:
        completed = mock.Mock(returncode=0, stdout=self.PS.encode())
        with mock.patch("poundlock_llamacpp.macos.subprocess.run", return_value=completed) as run:
            MacSystem().inspect(5)
        env = run.call_args.kwargs["env"]
        self.assertEqual(("C", "UTC"), (env["LC_ALL"], env["TZ"]))

    def test_runs_binary_matches_the_command_prefix_only(self) -> None:
        system = MacSystem()
        binary = Path("/Users/x/Library/Application Support/llama/llama-server")
        yes = ProcessInfo(1, "t", f"{binary} --port 1")
        bare = ProcessInfo(1, "t", str(binary))
        other = ProcessInfo(1, "t", f"{binary}-old --port 1")
        later = ProcessInfo(1, "t", f"/bin/sh -c echo {binary}")
        self.assertTrue(system.runs_binary(yes, binary))
        self.assertTrue(system.runs_binary(bare, binary))
        self.assertFalse(system.runs_binary(other, binary))
        self.assertFalse(system.runs_binary(later, binary))

    def test_find_processes_uses_pgrep_and_drops_zombies(self) -> None:
        system = MacSystem()
        outputs: Dict[str, Optional[str]] = {"/usr/bin/pgrep": "30\n31\n"}

        def fake_run(argv: List[str]) -> Optional[str]:
            if argv[0] == "/usr/bin/pgrep":
                return outputs["/usr/bin/pgrep"]
            return self.PS if argv[3] == "30" else "Z    Tue Sep 29 14:58:00 2026 /x"

        with mock.patch.object(system, "_run", side_effect=fake_run):
            self.assertEqual([30], system.find_processes("llama-server"))
        outputs["/usr/bin/pgrep"] = None
        with mock.patch.object(system, "_run", side_effect=fake_run):
            self.assertEqual([], system.find_processes("llama-server"))

    def test_mac_settings(self) -> None:
        system = MacSystem()
        self.assertEqual("external", system.default_sweeper)
        self.assertEqual(["/usr/bin/caffeinate", "-dimsu", "-w", "99"], system.caffeinate_command(99))
        self.assertEqual({}, system.server_environment(Path("/x")))
        self.assertTrue(str(system.default_home()).endswith("Library/Application Support/poundlock-llamacpp"))
        self.assertEqual([], system.gpu_memory())


class ScriptTests(unittest.TestCase):
    def test_missing_file_is_not_a_script(self) -> None:
        info = ProcessInfo(1, "t", "python /nope")
        self.assertFalse(script_interpreter_runs(info, Path("/nope")))


class PlatformSelectionTests(unittest.TestCase):
    def test_detects_the_running_platform(self) -> None:
        with mock.patch.object(sys, "platform", "linux"):
            self.assertIsInstance(platforms.detect_system(), LinuxSystem)
        with mock.patch.object(sys, "platform", "darwin"):
            self.assertIsInstance(platforms.detect_system(), MacSystem)
        with mock.patch.object(sys, "platform", "win32"):
            with self.assertRaisesRegex(ManagerError, "Linux and macOS only"):
                platforms.detect_system()


class LaunchdTests(unittest.TestCase):
    def test_plist_uses_the_neutral_label_and_sweeps_every_minute(self) -> None:
        import plistlib

        data = plistlib.loads(
            render_plist(
                Path("/usr/local/bin/poundlock-llamacpp"),
                Path("/Users/x/Library/Application Support/poundlock-llamacpp"),
                Path("/etc/manifest & more.json"),
            )
        )
        self.assertEqual("com.poundlock.llamacpp", data["Label"])
        self.assertEqual(LAUNCHD_LABEL, data["Label"])
        self.assertEqual(
            [
                "/usr/local/bin/poundlock-llamacpp",
                "--home", "/Users/x/Library/Application Support/poundlock-llamacpp",
                "--manifest", "/etc/manifest & more.json",
                "sweep",
            ],
            data["ProgramArguments"],
        )
        self.assertEqual(60, data["StartInterval"])
        self.assertTrue(data["RunAtLoad"])
        self.assertEqual("/dev/null", data["StandardOutPath"])
        self.assertTrue(data["StandardErrorPath"].endswith("logs/launchd-sweep.error.log"))


class FilePermissionSanity(unittest.TestCase):
    def test_deploy_script_is_executable(self) -> None:
        script = Path(__file__).resolve().parents[1] / "deploy" / "install.sh"
        self.assertTrue(script.stat().st_mode & stat.S_IXUSR)
