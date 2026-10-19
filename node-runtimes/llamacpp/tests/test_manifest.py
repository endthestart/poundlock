"""Manifest validation: it names programs to run, so mistakes must fail loudly."""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict
from unittest import mock

from poundlock_llamacpp.errors import InfrastructureError, ManagerError
from poundlock_llamacpp.manifest import (
    default_manifest_path,
    load_manifest,
    parse_profile,
)

from .fakes import ALIAS, SHA, manifest_dict, profile_dict

EXAMPLES = Path(__file__).resolve().parents[1] / "examples"


class ManifestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="llamacpp-manifest-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def write(self, document: Dict[str, Any], name: str = "manifest.json") -> Path:
        path = self.tmp / name
        path.write_text(json.dumps(document), encoding="utf-8")
        return path

    def profile(self, **overrides: Any):
        return load_manifest(self.write(manifest_dict({ALIAS: profile_dict(**overrides)}))).profile(ALIAS)

    def assert_invalid(self, message: str, **overrides: Any) -> None:
        with self.assertRaisesRegex(InfrastructureError, message):
            self.profile(**overrides)

    def server(self, **changes: Any) -> Dict[str, Any]:
        return {**profile_dict()["server"], **changes}


class ProfileTests(ManifestCase):
    def test_a_valid_profile_parses_with_defaults(self) -> None:
        raw = profile_dict()
        for key in ("warm_seconds", "lease_timeout_seconds", "startup_timeout_seconds"):
            del raw[key]
        profile = load_manifest(self.write(manifest_dict({ALIAS: raw}))).profile(ALIAS)
        self.assertEqual((600, 300, 420), (
            profile.warm_seconds, profile.lease_timeout_seconds, profile.startup_timeout_seconds
        ))
        self.assertIsNone(profile.warmup)
        self.assertEqual((), profile.conflicts)
        self.assertEqual("http://127.0.0.1:8080", profile.endpoint)

    def test_the_manager_owns_model_alias_context_host_and_port(self) -> None:
        profile = self.profile()
        command = profile.command
        self.assertEqual(
            ["--model", "--alias", "--ctx-size", "--host", "--port"], command[1:11:2]
        )
        self.assertEqual(["--parallel", "1", "--jinja"], command[11:])

    def test_repeating_a_managed_option_is_rejected(self) -> None:
        for argument in ("--port", "-m", "--model", "--alias", "-a", "--ctx-size", "-c", "--host"):
            with self.subTest(argument):
                self.assert_invalid(
                    "repeats", server=self.server(arguments=[argument, "x"])
                )
        self.assert_invalid("repeats --port", server=self.server(arguments=["--port=9"]))

    def test_the_key_file_and_the_slots_switch_shape_the_command(self) -> None:
        default = self.profile().command
        self.assertNotIn("--api-key-file", default)
        self.assertNotIn("--no-slots", default)
        key_file = "/etc/poundlock/llamacpp.key"
        profile = self.profile(server=self.server(api_key_file=key_file, slots_endpoint=False))
        self.assertEqual(
            ["--api-key-file", key_file, "--no-slots", "--parallel", "1", "--jinja"],
            profile.command[11:],
        )
        self.assertTrue(self.profile(server=self.server(slots_endpoint=True)).slots_endpoint)
        self.assert_invalid("api_key_file must be an absolute path", server=self.server(api_key_file="x.key"))
        self.assert_invalid("slots_endpoint must be true or false", server=self.server(slots_endpoint="no"))

    def test_the_key_and_the_slots_switch_cannot_bypass_the_profile(self) -> None:
        for argument, key in (
            ("--api-key", "api_key_file"),
            ("--api-key=x", "api_key_file"),
            ("--api-key-file", "api_key_file"),
            ("--slots", "slots_endpoint"),
            ("--no-slots", "slots_endpoint"),
        ):
            with self.subTest(argument):
                self.assert_invalid(
                    f"has {argument.split('=')[0]}; set server.{key} instead",
                    server=self.server(arguments=[argument, "x"]),
                )
        self.assert_invalid(
            "sets LLAMA_API_KEY; set server.api_key_file instead",
            server=self.server(environment={"LLAMA_API_KEY": "x"}),
        )

    def test_options_that_download_a_model_are_rejected(self) -> None:
        for argument in ("-hf", "--hf-repo", "--model-url", "-mu"):
            with self.subTest(argument):
                self.assert_invalid("local file", server=self.server(arguments=[argument, "x"]))

    def test_unknown_keys_are_typos_and_rejected(self) -> None:
        self.assert_invalid("unknown keys: warm_secs", warm_secs=5)
        self.assert_invalid("unknown keys: exe", server=self.server(exe="/x"))
        self.assert_invalid("unknown keys: sha", model={**profile_dict()["model"], "sha": "x"})
        with self.assertRaisesRegex(InfrastructureError, "unknown keys: extra"):
            load_manifest(self.write({**manifest_dict(), "extra": 1}))
        with self.assertRaisesRegex(InfrastructureError, "runtime has unknown keys: sweeep"):
            load_manifest(self.write(manifest_dict(sweeep="detached")))

    def test_sha256_must_be_lowercase_hex(self) -> None:
        for value in (SHA.upper(), "abc", "g" * 64, ""):
            with self.subTest(value):
                self.assert_invalid("sha256", model={**profile_dict()["model"], "sha256": value})

    def test_paths_must_be_absolute(self) -> None:
        self.assert_invalid("absolute", model={**profile_dict()["model"], "path": "models/x.gguf"})
        self.assert_invalid("absolute", server=self.server(executable="llama-server"))

    def test_host_must_be_an_address(self) -> None:
        for host in ("0.0.0.0", "::", "::1", "192.168.1.5", "localhost"):
            with self.subTest(host):
                self.assertEqual(host, self.profile(server=self.server(host=host)).host)
        self.assert_invalid("IP address", server=self.server(host="model.example.com"))

    def test_ipv6_endpoints_are_bracketed(self) -> None:
        self.assertEqual("http://[::1]:8080", self.profile(server=self.server(host="::")).endpoint)

    def test_wildcard_binds_are_probed_on_loopback(self) -> None:
        self.assertEqual("127.0.0.1", self.profile(server=self.server(host="0.0.0.0")).probe_host)

    def test_numbers_are_bounded_and_not_booleans(self) -> None:
        self.assert_invalid("port", server=self.server(port=0))
        self.assert_invalid("port", server=self.server(port=True))
        self.assert_invalid("lease_timeout_seconds", lease_timeout_seconds=10)
        self.assert_invalid("startup_timeout_seconds", startup_timeout_seconds=600)
        self.assert_invalid("warm_seconds", warm_seconds=-1)
        self.assert_invalid("size_bytes", model={**profile_dict()["model"], "size_bytes": 0})

    def test_conflicts_are_an_allowlist(self) -> None:
        self.assertEqual(("ollama",), self.profile(conflicts=["ollama"]).conflicts)
        self.assert_invalid("unknown \\['lmstudio'\\]", conflicts=["lmstudio"])

    def test_environment_names_and_values_are_checked(self) -> None:
        self.assert_invalid("invalid name", server=self.server(environment={"BAD NAME": "1"}))
        self.assert_invalid("non-empty string", server=self.server(environment={"OK": 1}))

    def test_warmup_settings(self) -> None:
        request = {"reasoning_effort": "none"}
        profile = self.profile(warmup={"min_tokens_per_second": 25.5, "request": request})
        self.assertEqual(25.5, profile.warmup.min_tokens_per_second)
        self.assertEqual((16, 256, 300), (
            profile.warmup.prime_tokens, profile.warmup.max_tokens, profile.warmup.timeout_seconds
        ))
        self.assert_invalid("positive number", warmup={"min_tokens_per_second": 0})
        self.assert_invalid("may not override", warmup={"request": {"messages": []}})
        self.assert_invalid("unknown keys", warmup={"min_tps": 3})

    def test_n_ctx_accepts_the_context_or_one_slots_share(self) -> None:
        one = self.profile(server=self.server(arguments=["--parallel", "1"]))
        self.assertEqual(frozenset({4096}), one.accepted_n_ctx)
        four = self.profile(server=self.server(arguments=["-np", "4"]))
        self.assertEqual(frozenset({4096, 1024}), four.accepted_n_ctx)

    def test_digest_changes_with_the_profile_and_with_the_alias(self) -> None:
        base = self.profile().digest
        self.assertEqual(base, self.profile().digest)
        self.assertNotEqual(base, self.profile(warm_seconds=1).digest)
        renamed = load_manifest(self.write(manifest_dict({"other": profile_dict()}))).profile("other")
        self.assertNotEqual(base, renamed.digest)

    def test_model_label_falls_back_to_the_file_name(self) -> None:
        model = {k: v for k, v in profile_dict()["model"].items() if k not in ("family", "quantization")}
        self.assertEqual("example", self.profile(model=model).model_label)


class ManifestFileTests(ManifestCase):
    def test_model_ids_must_be_safe_for_the_node_placeholder(self) -> None:
        for alias in ("-flag", "a b", "a/b", "x;y", ""):
            with self.subTest(alias):
                with self.assertRaisesRegex(InfrastructureError, "not a valid model id"):
                    load_manifest(self.write(manifest_dict({alias: profile_dict()})))
        for alias in ("qwen3.8-27b-mtp", "org_model:q4", "A1"):
            load_manifest(self.write(manifest_dict({alias: profile_dict()})))

    def test_unknown_model_is_a_refusal_not_an_infrastructure_error(self) -> None:
        manifest = load_manifest(self.write(manifest_dict()))
        with self.assertRaisesRegex(ManagerError, "not in the manifest"):
            manifest.profile("nope")
        with self.assertRaises(ManagerError) as caught:
            manifest.profile("--help")
        self.assertEqual(2, caught.exception.exit_code)

    def test_schema_version_and_shape(self) -> None:
        for document, message in (
            ({**manifest_dict(), "schema_version": 2}, "schema_version"),
            ({**manifest_dict(), "profiles": {}}, "profiles is empty"),
            ({**manifest_dict(), "profiles": []}, "must be an object"),
            ([], "must be an object"),
        ):
            with self.subTest(message), self.assertRaisesRegex(InfrastructureError, message):
                load_manifest(self.write(document))  # type: ignore[arg-type]

    def test_unreadable_and_invalid_json(self) -> None:
        with self.assertRaisesRegex(InfrastructureError, "unreadable"):
            load_manifest(self.tmp / "missing.json")
        path = self.tmp / "broken.json"
        path.write_text("{not json")
        with self.assertRaisesRegex(InfrastructureError, "not valid json"):
            load_manifest(path)

    @unittest.skipIf(os.name != "posix", "POSIX permissions")
    def test_a_manifest_others_can_write_is_refused(self) -> None:
        path = self.write(manifest_dict())
        path.chmod(0o664)
        with self.assertRaisesRegex(InfrastructureError, "writable by group or others"):
            load_manifest(path)
        path.chmod(0o644)
        load_manifest(path)

    def test_runtime_options(self) -> None:
        options = load_manifest(self.write(manifest_dict())).runtime
        self.assertIsNone(options.sweeper)
        self.assertEqual(30, options.sweep_interval_seconds)
        self.assertFalse(options.caffeinate)
        self.assertEqual("http://127.0.0.1:11434", options.ollama_url)
        with self.assertRaisesRegex(InfrastructureError, "sweeper"):
            load_manifest(self.write(manifest_dict(sweeper="cron")))
        for url in ("http://example.com:11434", "https://127.0.0.1", "http://user:pw@127.0.0.1"):
            with self.subTest(url), self.assertRaisesRegex(InfrastructureError, "loopback"):
                load_manifest(self.write(manifest_dict(ollama_url=url)))

    def test_default_manifest_path_prefers_json(self) -> None:
        self.assertEqual(self.tmp / "manifest.json", default_manifest_path(self.tmp))
        (self.tmp / "manifest.toml").write_text("")
        self.assertEqual(self.tmp / "manifest.toml", default_manifest_path(self.tmp))
        (self.tmp / "manifest.json").write_text("{}")
        self.assertEqual(self.tmp / "manifest.json", default_manifest_path(self.tmp))

    @unittest.skipUnless(sys.version_info >= (3, 11), "tomllib is Python 3.11+")
    def test_toml_manifests_load_on_python_3_11_and_newer(self) -> None:
        path = self.tmp / "manifest.toml"
        path.write_text(
            f"""
schema_version = 1
[runtime]
sweeper = "detached"
[profiles.{ALIAS}]
warm_seconds = 5
[profiles.{ALIAS}.model]
path = "/opt/models/example.gguf"
size_bytes = 1234
sha256 = "{SHA}"
[profiles.{ALIAS}.server]
executable = "/opt/llama/llama-server"
port = 8080
context_tokens = 4096
arguments = ["--jinja"]
""",
            encoding="utf-8",
        )
        profile = load_manifest(path).profile(ALIAS)
        self.assertEqual(5, profile.warm_seconds)
        self.assertEqual(("--jinja",), profile.arguments)

    def test_toml_on_an_older_python_says_what_to_do(self) -> None:
        path = self.tmp / "manifest.toml"
        path.write_text("schema_version = 1\n")
        with mock.patch.dict(sys.modules, {"tomllib": None}):
            with self.assertRaisesRegex(InfrastructureError, "Python 3.11 or newer; use a .json"):
                load_manifest(path)


class ExampleTests(unittest.TestCase):
    def test_examples_are_valid_manifests(self) -> None:
        examples = sorted(EXAMPLES.glob("*.json"))
        self.assertEqual(
            ["linux-amd-vulkan-mtp.json", "macos-metal.json"], [path.name for path in examples]
        )
        for path in examples:
            with self.subTest(path.name):
                manifest = load_manifest(path)
                for alias in manifest.aliases:
                    profile = manifest.profile(alias)
                    self.assertTrue(profile.command[0].startswith("/"))

    def test_the_linux_example_carries_the_proven_gates(self) -> None:
        profile = load_manifest(EXAMPLES / "linux-amd-vulkan-mtp.json").profile("example-27b-mtp")
        self.assertEqual(("creating MTP draft context",), profile.required_log_markers)
        self.assertEqual(("ollama",), profile.conflicts)
        self.assertEqual(30.0, profile.warmup.min_tokens_per_second)
        self.assertTrue(profile.warmup.require_no_reasoning)
        self.assertEqual({"GGML_VK_VISIBLE_DEVICES": "0"}, profile.environment)
        self.assertIn("draft-mtp", profile.arguments)

    def test_the_examples_turn_on_the_key_and_turn_off_slots(self) -> None:
        for path in sorted(EXAMPLES.glob("*.json")):
            manifest = load_manifest(path)
            for alias in manifest.aliases:
                with self.subTest(path.name):
                    profile = manifest.profile(alias)
                    self.assertIsNotNone(profile.api_key_file)
                    self.assertIn("--no-slots", profile.command)

    def test_the_macos_example_uses_caffeinate_and_the_launchd_sweep(self) -> None:
        manifest = load_manifest(EXAMPLES / "macos-metal.json")
        self.assertTrue(manifest.runtime.caffeinate)
        self.assertEqual("external", manifest.runtime.sweeper)

    def test_examples_and_package_have_no_personal_paths(self) -> None:
        root = Path(__file__).resolve().parents[1]
        forbidden = re.compile(r"/Users/[a-z]|/home/[a-z]", re.IGNORECASE)
        files = [*EXAMPLES.glob("*"), *(root / "src").rglob("*.py"), *(root / "deploy").glob("*"),
                 root / "README.md"]
        for path in files:
            if path.is_file():
                with self.subTest(path.name):
                    match = forbidden.search(path.read_text(encoding="utf-8"))
                    self.assertIsNone(match, match and match.group(0))

    def test_parse_profile_is_usable_directly(self) -> None:
        self.assertEqual(ALIAS, parse_profile(ALIAS, profile_dict()).alias)
