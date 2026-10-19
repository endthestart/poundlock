"""Manifest of llama-server profiles, keyed by the model id Poundlock requests.

The manifest names executables to run, so it is validated strictly: unknown keys are
errors, paths must be absolute, and the options the manager owns (model, alias,
context, host, port, API key, slots endpoint) cannot be repeated in the extra arguments.
"""

from __future__ import annotations

import errno
import hashlib
import ipaddress
import json
import os
import re
import stat
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, FrozenSet, List, Mapping, Optional, Set, Tuple
from urllib.parse import urlsplit

from .errors import InfrastructureError, ManagerError

SCHEMA_VERSION = 1

# Matches what the node agent accepts for {model}: alphanumerics and . _ - : only.
ALIAS_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
ENV_NAME_PATTERN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
# One key on one line. No whitespace, so llama-server (which reads each line of the file as
# a key) and the runtime read the same key; no leading '#', which newer builds treat as a comment.
API_KEY_PATTERN = re.compile(rb"[A-Za-z0-9._~+/=-]{32,1024}")
API_KEY_HINT = (
    "one key of 32 to 1024 characters from A-Z a-z 0-9 . _ ~ + / = - on a single line, "
    "for example: openssl rand -hex 32"
)

# Options the manager writes itself; repeating them would create two sources of truth.
MANAGED_OPTIONS: FrozenSet[str] = frozenset(
    {"-m", "--model", "-a", "--alias", "-c", "--ctx-size", "--host", "--port"}
)
# Options a profile key sets. `--api-key` would also put the key on the command line.
KEYED_OPTIONS: Dict[str, str] = {
    "--api-key": "server.api_key_file",
    "--api-key-file": "server.api_key_file",
    "--slots": "server.slots_endpoint",
    "--no-slots": "server.slots_endpoint",
}
# Options that make llama-server fetch a model, bypassing the size and SHA-256 check.
REMOTE_MODEL_OPTIONS: FrozenSet[str] = frozenset(
    {
        "-hf", "-hfr", "--hf-repo", "-hff", "--hf-file", "-hfv", "--hf-repo-v",
        "-hffv", "--hf-file-v", "-mu", "--model-url", "-dr", "--docker-repo",
    }
)
KNOWN_CONFLICTS: FrozenSet[str] = frozenset({"ollama"})
SWEEPERS: FrozenSet[str] = frozenset({"detached", "external"})
LOOPBACK_NAMES = frozenset({"127.0.0.1", "localhost", "::1"})

DEFAULT_OLLAMA_URL = "http://127.0.0.1:11434"
DEFAULT_WARMUP_PROMPT = (
    "Write a Python function that returns the n-th Fibonacci number, with a docstring."
)
# The node agent stops an ensure command after 600 seconds; leave room for hashing,
# unloading conflicts and cleanup around the startup window.
MAX_STARTUP_SECONDS = 570


@dataclass(frozen=True)
class Warmup:
    prompt: str
    prime_tokens: int
    max_tokens: int
    timeout_seconds: int
    min_tokens_per_second: Optional[float]
    require_no_reasoning: bool
    request: Dict[str, Any]


@dataclass(frozen=True)
class RuntimeOptions:
    sweeper: Optional[str]  # None means the platform default
    sweep_interval_seconds: int
    caffeinate: bool
    ollama_url: str
    foreign_process_names: Tuple[str, ...]
    # Homes of other instances on this machine (a second slot): their servers are not foreign.
    peer_homes: Tuple[Path, ...] = ()


@dataclass(frozen=True)
class Profile:
    alias: str
    model_path: Path
    model_size: int
    model_sha256: str
    model_family: Optional[str]
    model_quantization: Optional[str]
    model_source: Optional[str]
    executable: Path
    min_build: int
    host: str
    port: int
    context_tokens: int
    arguments: Tuple[str, ...]
    environment: Dict[str, str]
    api_key_file: Optional[Path]
    slots_endpoint: bool
    required_log_markers: Tuple[str, ...]
    warmup: Optional[Warmup]
    conflicts: Tuple[str, ...]
    max_idle_vram_bytes: Optional[int]
    vram_settle_seconds: int
    warm_seconds: int
    lease_timeout_seconds: int
    startup_timeout_seconds: int
    digest: str

    @property
    def probe_host(self) -> str:
        if self.host in {"0.0.0.0", ""}:
            return "127.0.0.1"
        if self.host == "::":
            return "::1"
        return self.host

    @property
    def endpoint(self) -> str:
        host = f"[{self.probe_host}]" if ":" in self.probe_host else self.probe_host
        return f"http://{host}:{self.port}"

    @property
    def command(self) -> List[str]:
        command = [
            str(self.executable),
            "--model", str(self.model_path),
            "--alias", self.alias,
            "--ctx-size", str(self.context_tokens),
            "--host", self.host,
            "--port", str(self.port),
        ]
        if self.api_key_file is not None:
            command += ["--api-key-file", str(self.api_key_file)]
        if not self.slots_endpoint:
            command.append("--no-slots")
        return [*command, *self.arguments]

    @property
    def identity_fragments(self) -> Tuple[str, ...]:
        """Command-line pieces a process must carry to count as this profile's server."""
        return (
            f"--model {self.model_path}",
            f"--alias {self.alias}",
            f"--port {self.port}",
        )

    @property
    def accepted_n_ctx(self) -> FrozenSet[int]:
        """Values /props may report: the whole context, or one slot's share of it."""
        accepted = {self.context_tokens}
        parallel = argument_int(self.arguments, ("-np", "--parallel"))
        if parallel and parallel > 1:
            accepted.add(self.context_tokens // parallel)
        return frozenset(accepted)

    @property
    def model_label(self) -> str:
        parts = [part for part in (self.model_family, self.model_quantization) if part]
        return " ".join(parts) if parts else self.model_path.stem


@dataclass(frozen=True)
class Manifest:
    path: Path
    runtime: RuntimeOptions
    raw_profiles: Mapping[str, Any]

    @property
    def aliases(self) -> List[str]:
        return sorted(self.raw_profiles)

    def profile(self, alias: str) -> Profile:
        if not ALIAS_PATTERN.match(alias) or alias not in self.raw_profiles:
            raise ManagerError(f"model is not in the manifest: {alias[:80]}")
        return parse_profile(alias, self.raw_profiles[alias])


def argument_int(arguments: Tuple[str, ...], options: Tuple[str, ...]) -> Optional[int]:
    for index, value in enumerate(arguments[:-1]):
        if value in options:
            try:
                return int(arguments[index + 1])
            except ValueError:
                return None
    return None


def default_manifest_path(home: Path) -> Path:
    toml_path = home / "manifest.toml"
    json_path = home / "manifest.json"
    return toml_path if toml_path.exists() and not json_path.exists() else json_path


def _read_raw(path: Path) -> Mapping[str, Any]:
    try:
        mode = path.stat().st_mode
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise InfrastructureError(f"manifest is unreadable: {exc}") from exc
    if mode & (stat.S_IWGRP | stat.S_IWOTH):
        raise InfrastructureError(
            f"manifest {path} is writable by group or others; it names programs to run, "
            "so run: chmod go-w on it"
        )
    try:
        if path.suffix == ".toml":
            try:
                import tomllib  # type: ignore[import-not-found,import-untyped,unused-ignore]  # 3.11+
            except ImportError as exc:
                raise InfrastructureError(
                    "TOML manifests need Python 3.11 or newer; use a .json manifest instead"
                ) from exc
            raw = tomllib.loads(text)
        else:
            raw = json.loads(text)
    except (ValueError, json.JSONDecodeError) as exc:
        raise InfrastructureError(f"manifest is not valid {path.suffix[1:] or 'JSON'}: {exc}") from exc
    if not isinstance(raw, dict):
        raise InfrastructureError("manifest must be an object")
    return raw


def load_manifest(path: Path) -> Manifest:
    raw = _read_raw(path)
    try:
        _only_keys(raw, {"schema_version", "runtime", "profiles"}, "manifest")
        if raw.get("schema_version") != SCHEMA_VERSION:
            raise ValueError(f"schema_version must be {SCHEMA_VERSION}")
        profiles = _mapping(raw.get("profiles"), "profiles")
        if not profiles:
            raise ValueError("profiles is empty")
        for alias in profiles:
            if not ALIAS_PATTERN.match(str(alias)):
                raise ValueError(
                    f"profile key {str(alias)[:40]!r} is not a valid model id "
                    "(letters, digits, . _ - : and no leading dash)"
                )
        runtime = _parse_runtime(_mapping(raw.get("runtime", {}), "runtime"))
    except ValueError as exc:
        raise InfrastructureError(f"manifest is invalid: {exc}") from exc
    return Manifest(path=path, runtime=runtime, raw_profiles=profiles)


def _parse_runtime(raw: Mapping[str, Any]) -> RuntimeOptions:
    _only_keys(
        raw,
        {
            "sweeper",
            "sweep_interval_seconds",
            "caffeinate",
            "ollama_url",
            "foreign_process_names",
            "peer_homes",
        },
        "runtime",
    )
    sweeper = raw.get("sweeper")
    if sweeper is not None and sweeper not in SWEEPERS:
        raise ValueError(f"runtime.sweeper must be one of {sorted(SWEEPERS)}")
    url = _string(raw.get("ollama_url", DEFAULT_OLLAMA_URL), "runtime.ollama_url")
    parts = urlsplit(url)
    if parts.scheme != "http" or (parts.hostname or "") not in LOOPBACK_NAMES or parts.username:
        raise ValueError("runtime.ollama_url must be a plain http:// loopback address")
    return RuntimeOptions(
        sweeper=sweeper,
        sweep_interval_seconds=_integer(
            raw.get("sweep_interval_seconds", 30), "runtime.sweep_interval_seconds", 1, 3600
        ),
        caffeinate=_boolean(raw.get("caffeinate", False), "runtime.caffeinate"),
        ollama_url=url.rstrip("/"),
        foreign_process_names=tuple(
            _string_list(raw.get("foreign_process_names", ["llama-server"]), "runtime.foreign_process_names")
        ),
        peer_homes=tuple(
            _absolute_path(value, "runtime.peer_homes")
            for value in _string_list(raw.get("peer_homes", []), "runtime.peer_homes")
        ),
    )


def _absolute_path(value: str, where: str) -> Path:
    path = Path(value).expanduser()
    if not path.is_absolute():
        raise ValueError(f"{where} entries must be absolute paths")
    return path


def parse_profile(alias: str, raw_value: Any) -> Profile:
    where = f"profile {alias}"
    try:
        raw = _mapping(raw_value, where)
        _only_keys(
            raw,
            {
                "model", "server", "required_log_markers", "warmup", "conflicts",
                "max_idle_vram_bytes", "vram_settle_seconds", "warm_seconds",
                "lease_timeout_seconds", "startup_timeout_seconds",
            },
            where,
        )
        model = _mapping(raw.get("model"), f"{where}.model")
        _only_keys(
            model, {"path", "size_bytes", "sha256", "family", "quantization", "source"},
            f"{where}.model",
        )
        server = _mapping(raw.get("server"), f"{where}.server")
        _only_keys(
            server,
            {
                "executable", "min_build", "host", "port", "context_tokens",
                "arguments", "environment", "api_key_file", "slots_endpoint",
            },
            f"{where}.server",
        )
        sha256 = _string(model.get("sha256"), f"{where}.model.sha256")
        if not SHA256_PATTERN.match(sha256):
            raise ValueError(f"{where}.model.sha256 must be 64 lowercase hex characters")
        arguments = _string_list(server.get("arguments", []), f"{where}.server.arguments")
        _check_arguments(arguments, where)
        conflicts = _string_list(raw.get("conflicts", []), f"{where}.conflicts")
        unknown = sorted(set(conflicts) - KNOWN_CONFLICTS)
        if unknown:
            raise ValueError(f"{where}.conflicts: unknown {unknown}; known: {sorted(KNOWN_CONFLICTS)}")
        environment = _mapping(server.get("environment", {}), f"{where}.server.environment")
        for name, value in environment.items():
            if not ENV_NAME_PATTERN.match(str(name)):
                raise ValueError(f"{where}.server.environment has an invalid name")
            if name == "LLAMA_API_KEY":
                raise ValueError(
                    f"{where}.server.environment sets LLAMA_API_KEY; set server.api_key_file instead"
                )
            _string(value, f"{where}.server.environment.{name}")
        host = _string(server.get("host", "127.0.0.1"), f"{where}.server.host")
        _check_host(host, where)
        idle_vram = raw.get("max_idle_vram_bytes")
        api_key_file = server.get("api_key_file")
        profile = Profile(
            alias=alias,
            model_path=_absolute(model.get("path"), f"{where}.model.path"),
            model_size=_integer(model.get("size_bytes"), f"{where}.model.size_bytes", 1),
            model_sha256=sha256,
            model_family=_optional_label(model, "family", where),
            model_quantization=_optional_label(model, "quantization", where),
            model_source=_optional_label(model, "source", where),
            executable=_absolute(server.get("executable"), f"{where}.server.executable"),
            min_build=_integer(server.get("min_build", 0), f"{where}.server.min_build", 0),
            host=host,
            port=_integer(server.get("port"), f"{where}.server.port", 1, 65535),
            context_tokens=_integer(
                server.get("context_tokens"), f"{where}.server.context_tokens", 1
            ),
            arguments=tuple(arguments),
            environment={str(name): str(value) for name, value in environment.items()},
            api_key_file=(
                None if api_key_file is None
                else _absolute(api_key_file, f"{where}.server.api_key_file")
            ),
            slots_endpoint=_boolean(
                server.get("slots_endpoint", True), f"{where}.server.slots_endpoint"
            ),
            required_log_markers=tuple(
                _string_list(raw.get("required_log_markers", []), f"{where}.required_log_markers")
            ),
            warmup=_parse_warmup(raw.get("warmup"), where),
            conflicts=tuple(conflicts),
            max_idle_vram_bytes=(
                None if idle_vram is None
                else _integer(idle_vram, f"{where}.max_idle_vram_bytes", 1)
            ),
            vram_settle_seconds=_integer(
                raw.get("vram_settle_seconds", 30), f"{where}.vram_settle_seconds", 0, 300
            ),
            warm_seconds=_integer(raw.get("warm_seconds", 600), f"{where}.warm_seconds", 0, 86400),
            lease_timeout_seconds=_integer(
                raw.get("lease_timeout_seconds", 300), f"{where}.lease_timeout_seconds", 60, 3600
            ),
            startup_timeout_seconds=_integer(
                raw.get("startup_timeout_seconds", 420),
                f"{where}.startup_timeout_seconds", 1, MAX_STARTUP_SECONDS,
            ),
            # The alias is part of the identity: two profiles that differ only by alias
            # start servers that answer to different model ids.
            digest=hashlib.sha256(
                json.dumps(
                    {"alias": alias, "profile": raw},
                    sort_keys=True, separators=(",", ":"), default=str,
                ).encode()
            ).hexdigest(),
        )
    except ValueError as exc:
        raise InfrastructureError(f"manifest is invalid: {exc}") from exc
    return profile


def _parse_warmup(value: Any, where: str) -> Optional[Warmup]:
    if value is None:
        return None
    raw = _mapping(value, f"{where}.warmup")
    _only_keys(
        raw,
        {
            "prompt", "prime_tokens", "max_tokens", "timeout_seconds",
            "min_tokens_per_second", "require_no_reasoning", "request",
        },
        f"{where}.warmup",
    )
    minimum = raw.get("min_tokens_per_second")
    if minimum is not None and (
        isinstance(minimum, bool) or not isinstance(minimum, (int, float)) or minimum <= 0
    ):
        raise ValueError(f"{where}.warmup.min_tokens_per_second must be a positive number")
    request = _mapping(raw.get("request", {}), f"{where}.warmup.request")
    if {"model", "messages", "max_tokens", "stream"} & set(request):
        raise ValueError(f"{where}.warmup.request may not override model, messages, max_tokens or stream")
    return Warmup(
        prompt=_string(raw.get("prompt", DEFAULT_WARMUP_PROMPT), f"{where}.warmup.prompt"),
        prime_tokens=_integer(raw.get("prime_tokens", 16), f"{where}.warmup.prime_tokens", 1, 4096),
        max_tokens=_integer(raw.get("max_tokens", 256), f"{where}.warmup.max_tokens", 1, 8192),
        timeout_seconds=_integer(
            raw.get("timeout_seconds", 300), f"{where}.warmup.timeout_seconds", 1, MAX_STARTUP_SECONDS
        ),
        min_tokens_per_second=None if minimum is None else float(minimum),
        require_no_reasoning=_boolean(
            raw.get("require_no_reasoning", False), f"{where}.warmup.require_no_reasoning"
        ),
        request=dict(request),
    )


def _check_arguments(arguments: List[str], where: str) -> None:
    for argument in arguments:
        option = argument.split("=", 1)[0]
        if option in MANAGED_OPTIONS:
            raise ValueError(
                f"{where}.server.arguments repeats {option}; the manager sets the model, "
                "alias, context size, host and port from the profile"
            )
        if option in KEYED_OPTIONS:
            raise ValueError(
                f"{where}.server.arguments has {option}; set {KEYED_OPTIONS[option]} instead"
            )
        if option in REMOTE_MODEL_OPTIONS:
            raise ValueError(
                f"{where}.server.arguments has {option}; profiles use a local file whose "
                "size and SHA-256 are verified"
            )


def read_api_key(profile: Profile) -> Optional[str]:
    """The key in `server.api_key_file`, read at each use so it is never kept in state.

    The file is refused unless it is a private regular file: it must not be a symlink, and
    group and others get no access. Messages name the file, never its contents.
    """
    path = profile.api_key_file
    if path is None:
        return None
    where = f"manifest profile {profile.alias}: server.api_key_file {path}"
    try:
        # O_NONBLOCK: opening a FIFO must not wait for a writer before it can be refused.
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as exc:
        if exc.errno == errno.ELOOP:
            raise InfrastructureError(f"{where} is a symlink; name the file itself") from None
        raise InfrastructureError(f"{where} is unreadable: {exc.strerror}") from None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise InfrastructureError(f"{where} is not a regular file")
        if info.st_mode & (stat.S_IRWXG | stat.S_IRWXO):
            raise InfrastructureError(
                f"{where} is accessible to group or others; it holds the server's API key, "
                "so run: chmod 600 on it"
            )
        data = os.read(fd, 2048)
    finally:
        os.close(fd)
    key = data[:-1] if data.endswith(b"\n") else data
    if not API_KEY_PATTERN.fullmatch(key):
        raise InfrastructureError(f"{where} must hold {API_KEY_HINT}")
    return key.decode("ascii")


def _check_host(host: str, where: str) -> None:
    if host == "localhost":
        return
    try:
        ipaddress.ip_address(host)
    except ValueError:
        raise ValueError(f"{where}.server.host must be an IP address or localhost") from None


def _only_keys(raw: Mapping[str, Any], allowed: Set[str], where: str) -> None:
    unknown = sorted(str(key) for key in set(raw) - allowed)
    if unknown:
        raise ValueError(f"{where} has unknown keys: {', '.join(unknown)}")


def _mapping(value: Any, where: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{where} must be an object")
    return value


def _string(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{where} must be a non-empty string")
    return value


def _optional_label(raw: Mapping[str, Any], key: str, where: str) -> Optional[str]:
    return None if raw.get(key) is None else _string(raw[key], f"{where}.model.{key}")


def _string_list(value: Any, where: str) -> List[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) and item for item in value):
        raise ValueError(f"{where} must be a list of non-empty strings")
    return list(value)


def _boolean(value: Any, where: str) -> bool:
    if not isinstance(value, bool):
        raise ValueError(f"{where} must be true or false")
    return value


def _integer(value: Any, where: str, minimum: int, maximum: Optional[int] = None) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{where} must be an integer")
    if value < minimum or (maximum is not None and value > maximum):
        bound = f"at least {minimum}" if maximum is None else f"between {minimum} and {maximum}"
        raise ValueError(f"{where} must be {bound}")
    return value


def _absolute(value: Any, where: str) -> Path:
    path = Path(_string(value, where))
    if not path.is_absolute():
        raise ValueError(f"{where} must be an absolute path")
    return path
