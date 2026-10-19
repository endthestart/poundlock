"""Lease lifecycle for one llama-server slot: ensure, renew, release, stop, sweep.

State lives in `<home>/state.json`; every mutating operation holds `<home>/operation.lock`.
Phases: stopped, starting, ready (leased), warm_idle (released, kept loaded), failed.

The manager only ever signals a process it can prove it started: same start-time
token, same binary, same model/alias/port on the command line. A recorded PID that
was reused, or a `llama-server` someone else launched, is left alone.
"""

from __future__ import annotations

import os
import signal
import threading
import time
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List, Optional, Set, Tuple

from .artifacts import (
    application_version,
    cached_server_version,
    parse_build,
    verify_artifacts,
    version_line,
)
from .conflicts import release_conflicts, wait_for_idle_vram
from .errors import BusyError, ConflictError, InfrastructureError, ManagerError, NotReady
from .manifest import Profile, RuntimeOptions, load_manifest
from .platforms import detect_system
from .readiness import check_markers, gpu_summary, probe, run_warmup
from .state import Store, utc_now
from .system import ProcessInfo, System

ACTIVE_PHASES = ("starting", "ready", "warm_idle")
PUBLIC_KEYS = (
    "schema_version", "revision", "updated_at", "phase", "model", "lease_id",
    "lease_deadline_epoch", "released_lease_id", "release_deadline_epoch",
    "last_operation_id", "last_stop_reason", "endpoint", "server_pid", "started_at",
    "server_log", "last_server_log", "evidence", "failure",
)
SERVER_STOP_GRACE = 20.0
HELPER_STOP_GRACE = 3.0


@contextmanager
def shielded() -> Iterator[None]:
    """Ignore SIGINT/SIGTERM while cleaning up, so a second signal cannot leave a server."""
    if threading.current_thread() is not threading.main_thread():
        yield
        return
    saved = {}
    for number in (signal.SIGINT, signal.SIGTERM):
        try:
            saved[number] = signal.signal(number, signal.SIG_IGN)
        except (ValueError, OSError):
            pass
    try:
        yield
    finally:
        for number, handler in saved.items():
            signal.signal(number, handler)


class Manager:
    def __init__(
        self,
        *,
        home: Path,
        manifest_path: Path,
        system: Optional[System] = None,
        clock: Callable[[], float] = time.time,
        sleep: Callable[[float], None] = time.sleep,
        lock_timeout: float = 60.0,
        poll_interval: float = 2.0,
    ) -> None:
        self.home = home
        self.manifest_path = manifest_path
        self.system = system or detect_system()
        self.clock = clock
        self.sleep = sleep
        self.poll_interval = poll_interval
        self.store = Store(home, lock_timeout=lock_timeout, sleep=sleep)
        self._command = "-"
        self._lease = "-"

    # --- small helpers -------------------------------------------------------

    def log(self, message: str) -> None:
        self.store.log(self._command, self._lease, message)

    def _begin(self, command: str, lease_id: str = "-") -> None:
        self._command, self._lease = command, lease_id

    def view(self, state: Dict[str, Any], status: Optional[str] = None) -> Dict[str, Any]:
        result = {key: state[key] for key in PUBLIC_KEYS if key in state}
        result["status"] = status or str(state.get("phase", "stopped"))
        return result

    def _server_environment(self, profile: Profile) -> Dict[str, str]:
        environment = dict(os.environ)
        environment.update(self.system.server_environment(self.home))
        environment.update(profile.environment)
        return environment

    def _owner_matches(self, info: Optional[ProcessInfo], owner: Dict[str, Any]) -> bool:
        return (
            info is not None
            and bool(owner.get("started"))
            and info.started == owner["started"]
            and self.system.runs_binary(info, Path(str(owner["binary"])))
            and all(fragment in info.command for fragment in owner.get("fragments", []))
        )

    def _owned(self, state: Dict[str, Any]) -> bool:
        pid = int(state.get("server_pid") or 0)
        owner = state.get("owner")
        if pid <= 0 or not isinstance(owner, dict):
            return False
        return self._owner_matches(self.system.inspect(pid), owner)

    def _terminate_if(
        self, pid: int, started: str, matches: Callable[[ProcessInfo], bool], grace: float
    ) -> None:
        info = self.system.inspect(pid) if pid > 0 else None
        if info is not None and started and info.started == started and matches(info):
            self.system.terminate(pid, grace)

    def _stop_server(self, state: Dict[str, Any], *, trusted: bool = False) -> None:
        """End the server and its helpers, but only processes proven ours.

        `trusted` is for a server this very operation just spawned: its PID cannot be
        reused while we still hold the child, so no proof is needed.
        """
        pid = int(state.get("server_pid") or 0)
        if pid > 0 and (trusted or self._owned(state)):
            self.system.terminate(pid, SERVER_STOP_GRACE)
        helper = state.get("caffeinate") or {}
        self._terminate_if(
            int(helper.get("pid") or 0),
            str(helper.get("started") or ""),
            lambda info: "caffeinate" in info.command and f"-w {pid}" in info.command,
            HELPER_STOP_GRACE,
        )
        sweeper = state.get("sweeper") or {}
        self._terminate_if(
            int(sweeper.get("pid") or 0),
            str(sweeper.get("started") or ""),
            lambda info: "poundlock_llamacpp" in info.command
            and " sweeper " in f" {info.command} ",
            HELPER_STOP_GRACE,
        )

    def _stopped(
        self, state: Dict[str, Any], reason: str, operation_id: Optional[str] = None
    ) -> Dict[str, Any]:
        """Record the stop. Only an operation that completed passes its id: a cleanup inside
        an operation that then fails must not make a retry of it look already done."""
        self.log(f"stopped: {reason}")
        fresh: Dict[str, Any] = {
            "revision": state.get("revision", 0),
            "phase": "stopped",
            "last_stop_reason": reason,
        }
        last = operation_id or state.get("last_operation_id")
        if last:
            fresh["last_operation_id"] = last
        log = state.get("server_log") or state.get("last_server_log")
        if log:
            fresh["last_server_log"] = log
        return self.store.write_state(fresh)

    def _reconcile(self, state: Dict[str, Any]) -> Dict[str, Any]:
        """An active phase whose server is gone (or was never ours) is stale."""
        if state.get("phase") in ("ready", "warm_idle") and not self._owned(state):
            self._stop_server(state)
            return self._stopped(state, "server_not_running")
        return state

    # --- ensure --------------------------------------------------------------

    def ensure(self, alias: str, lease_id: str, operation_id: str) -> Dict[str, Any]:
        self._begin("ensure", lease_id)
        manifest = load_manifest(self.manifest_path)
        profile = manifest.profile(alias)
        with self.store.lock():
            state = self.store.read_state()
            if (
                state.get("last_operation_id") == operation_id
                and state.get("phase") == "ready"
                and state.get("lease_id") == lease_id
            ):
                return self.view(state)  # a retry of an ensure that already completed
            state = self._reconcile(state)
            state, reused = self._reuse(state, profile, manifest.runtime, lease_id, operation_id)
            if reused is not None:
                return reused
            return self._start(state, profile, manifest.runtime, lease_id, operation_id)

    def _reuse(
        self,
        state: Dict[str, Any],
        profile: Profile,
        options: RuntimeOptions,
        lease_id: str,
        operation_id: str,
    ) -> Tuple[Dict[str, Any], Optional[Dict[str, Any]]]:
        phase = state.get("phase")
        if phase == "starting":
            # Only ensure holds the lock while starting, so this start was abandoned.
            self._stop_server(state)
            return self._stopped(state, "abandoned_start"), None
        if phase not in ("ready", "warm_idle"):
            return state, None
        now = self.clock()
        if phase == "ready":
            held_by_other = state.get("lease_id") != lease_id and now < float(
                state.get("lease_deadline_epoch") or 0
            )
            if held_by_other:
                raise ConflictError("the slot is held by another lease")
            if state.get("lease_id") == lease_id:
                if state.get("model") != profile.alias:
                    raise ConflictError("this lease already holds a different model")
                if state.get("profile_digest") != profile.digest:
                    raise ConflictError(
                        "the profile changed while this lease is active; release the lease first"
                    )
        reason = "profile_switch"
        if state.get("profile_digest") == profile.digest:
            try:
                ready = probe(self.system, profile)
            except (NotReady, InfrastructureError) as exc:
                reason = "warm_server_unhealthy"
                self.log(f"reuse refused: {str(exc)[:200]}")
            else:
                state = dict(state)
                state.update(
                    phase="ready",
                    lease_id=lease_id,
                    lease_deadline_epoch=now + profile.lease_timeout_seconds,
                    released_lease_id=None,
                    release_deadline_epoch=None,
                    last_operation_id=operation_id,
                    policy=self._policy(profile),
                    evidence={**state.get("evidence", {}), "ready": ready},
                )
                state = self.store.write_state(state)
                state = self._ensure_sweeper(state, options)
                self.log(f"warm server reused pid={state.get('server_pid')}")
                return state, self.view(state, "ready")
        self._stop_server(state)
        return self._stopped(state, reason), None

    @staticmethod
    def _policy(profile: Profile) -> Dict[str, int]:
        return {
            "warm_seconds": profile.warm_seconds,
            "lease_timeout_seconds": profile.lease_timeout_seconds,
        }

    def _refuse_foreign(self, profile: Profile, options: RuntimeOptions) -> None:
        names = sorted(set(options.foreign_process_names) | {profile.executable.name})
        peers = self._peer_servers(options)
        foreign = sorted(
            {pid for name in names for pid in self.system.find_processes(name)} - peers
        )
        if foreign:
            raise ConflictError(
                "a llama-server that this runtime did not start is running "
                f"(pid {', '.join(str(pid) for pid in foreign[:5])}); it is left alone"
            )
        if self.system.port_in_use(profile.probe_host, profile.port):
            raise ConflictError(
                f"port {profile.port} is already in use by something this runtime did not start"
            )

    def _peer_servers(self, options: RuntimeOptions) -> Set[int]:
        """Servers that another instance on this machine (a second slot) started, read
        from its state without its lock. Only a server proven to be that peer's (its
        start time, binary and command line, as `_owned` checks) is exempt: a stale
        state.json could name a reused pid. An unreadable peer contributes nothing, so
        its server stays foreign and the start is refused, as before peers existed."""
        pids: Set[int] = set()
        for home in options.peer_homes:
            try:
                state = Store(home).read_state()
                pid = int(state.get("server_pid") or 0)
            except (InfrastructureError, OSError, ValueError, TypeError):
                continue
            if pid > 0 and self._owned(state):
                pids.add(pid)
        return pids

    def _start(
        self,
        state: Dict[str, Any],
        profile: Profile,
        options: RuntimeOptions,
        lease_id: str,
        operation_id: str,
    ) -> Dict[str, Any]:
        if options.caffeinate and not self.system.supports_caffeinate:
            raise InfrastructureError("runtime.caffeinate is only available on macOS")
        self._refuse_foreign(profile, options)
        environment = self._server_environment(profile)
        artifacts = verify_artifacts(self.system, self.store, profile, environment)
        unloaded = release_conflicts(self.system, profile, options.ollama_url, self.log)
        idle_vram = wait_for_idle_vram(self.system, profile, self.clock, self.sleep)

        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        log_path = self.store.log_root / f"{stamp}-{operation_id}.server.log"
        started = self.clock()
        try:
            pid = self.system.spawn_server(profile.command, environment, log_path)
        except OSError as exc:
            raise InfrastructureError(f"cannot start {profile.executable}: {exc}") from exc
        running: Dict[str, Any] = {"server_pid": pid}
        try:
            info = self.system.inspect(pid)
            owner = {
                "binary": str(profile.executable),
                "fragments": list(profile.identity_fragments),
                "started": info.started if info else "",
            }
            running["owner"] = owner
            running = self.store.write_state(
                {
                    "revision": state.get("revision", 0),
                    "phase": "starting",
                    "model": profile.alias,
                    "profile_digest": profile.digest,
                    "lease_id": lease_id,
                    "lease_deadline_epoch": started
                    + profile.startup_timeout_seconds
                    + profile.lease_timeout_seconds,
                    "last_operation_id": operation_id,
                    "server_pid": pid,
                    "owner": owner,
                    "server_log": str(log_path),
                    "endpoint": profile.endpoint,
                    "started_at": utc_now(),
                    "policy": self._policy(profile),
                    "evidence": {"artifact": artifacts},
                }
            )
            self.log(f"start pid={pid} log={log_path.name} unloaded={len(unloaded)}")
            self.store.prune_server_logs(log_path)
            running = self._start_caffeinate(running, options)
            running = self._ensure_sweeper(running, options)
            evidence = self._await_ready(profile, running, log_path, started)
            gpu = self.system.gpu_memory()
            running.update(
                phase="ready",
                lease_deadline_epoch=self.clock() + profile.lease_timeout_seconds,
                evidence={
                    "artifact": artifacts,
                    **evidence,
                    "gpu": gpu,
                    "idle_vram_before_start": idle_vram if idle_vram >= 0 else None,
                    "conflicts_unloaded": unloaded,
                    "load_seconds": round(self.clock() - started, 1),
                },
            )
            running = self.store.write_state(running)
        except BaseException as exc:
            self._abort_start(running, profile, operation_id, exc)
            raise
        self.log(f"ready pid={pid} load_s={running['evidence']['load_seconds']}")
        return self.view(running, "ready")

    def _abort_start(
        self, running: Dict[str, Any], profile: Profile, operation_id: str, error: BaseException
    ) -> None:
        """Leave nothing behind: kill the server we spawned and record why."""
        with shielded():
            reason = str(error)[:500] if isinstance(error, ManagerError) else type(error).__name__
            self.log(f"FAILED: {reason}")
            try:
                self._stop_server(running, trusted=True)
            finally:
                try:
                    self.store.write_state(
                        {
                            "revision": running.get("revision", 0),
                            "phase": "failed",
                            "model": profile.alias,
                            "profile_digest": profile.digest,
                            "last_operation_id": operation_id,
                            "failure": reason,
                            "server_log": running.get("server_log"),
                        }
                    )
                except OSError:
                    pass

    def _start_caffeinate(self, state: Dict[str, Any], options: RuntimeOptions) -> Dict[str, Any]:
        if not options.caffeinate:
            return state
        command = self.system.caffeinate_command(int(state["server_pid"]))
        if command is None:
            return state
        pid = self.system.spawn_helper(command, dict(os.environ), self.store.log_root / "caffeinate.log")
        info = self.system.inspect(pid)
        state = dict(state)
        state["caffeinate"] = {"pid": pid, "started": info.started if info else ""}
        return self.store.write_state(state)

    def _ensure_sweeper(self, state: Dict[str, Any], options: RuntimeOptions) -> Dict[str, Any]:
        """Start the detached sweeper unless one is already running or sweeping is external."""
        if (options.sweeper or self.system.default_sweeper) != "detached":
            return state
        current = state.get("sweeper") or {}
        existing = self.system.inspect(int(current.get("pid") or 0))
        if existing is not None and existing.started == current.get("started"):
            return state
        prefix, extra = self.system.self_command()
        argv = prefix + [
            "--home", str(self.home),
            "--manifest", str(self.manifest_path),
            "sweeper",
            "--watch-pid", str(state["server_pid"]),
            "--interval", str(options.sweep_interval_seconds),
        ]
        try:
            pid = self.system.spawn_helper(
                argv, {**os.environ, **extra}, self.store.log_root / "sweeper.log"
            )
        except OSError as exc:
            self.log(f"sweeper did not start: {type(exc).__name__}; use the external sweep")
            return state
        info = self.system.inspect(pid)
        state = dict(state)
        state["sweeper"] = {"pid": pid, "started": info.started if info else ""}
        return self.store.write_state(state)

    def _await_ready(
        self, profile: Profile, state: Dict[str, Any], log_path: Path, started: float
    ) -> Dict[str, Any]:
        deadline = started + profile.startup_timeout_seconds
        pid = int(state["server_pid"])
        owner = state["owner"]
        last = "the server did not answer"
        pause = 0.25  # poll quickly at first so a warm-cache load is noticed at once
        while self.clock() < deadline:
            info = self.system.inspect(pid)
            if not self._owner_matches(info, owner):
                # Linux shows a new process's command line only once exec has set it
                # up, which can lag the spawn on a cold page cache. The same process
                # with an empty command line is still starting, not gone.
                if info is None or info.started != owner["started"] or any(info.argv):
                    raise InfrastructureError(f"llama-server exited while loading; see {log_path}")
                self.sleep(pause)
                continue
            try:
                ready = probe(self.system, profile)
            except NotReady as exc:
                last = str(exc)
                self.sleep(pause)
                pause = min(pause * 1.5, self.poll_interval)
                continue
            markers = check_markers(self.system, profile, log_path, self.sleep)
            warmup = run_warmup(
                self.system,
                profile,
                lambda: deadline - self.clock(),
                lambda: gpu_summary(self.system.gpu_memory()),
            )
            return {"ready": ready, "log_markers": markers, "warmup": warmup}
        raise InfrastructureError(
            f"not ready after {profile.startup_timeout_seconds}s ({last}); see {log_path}"
        )

    # --- renew / release / stop ----------------------------------------------

    def renew(self, lease_id: str, operation_id: str) -> Dict[str, Any]:
        """Extend the lease. No health probe: a server busy with a long request must not
        be judged unhealthy and killed."""
        self._begin("renew", lease_id)
        with self.store.lock():
            state = self.store.read_state()
            if state.get("phase") != "ready" or state.get("lease_id") != lease_id:
                raise ConflictError("renew does not match the active lease")
            if not self._owned(state):
                self._stop_server(state)
                self._stopped(state, "server_lost_during_lease")
                raise InfrastructureError("llama-server is gone; the lease is lost")
            timeout = int((state.get("policy") or {}).get("lease_timeout_seconds", 300))
            state = dict(state)
            state.update(
                last_operation_id=operation_id, lease_deadline_epoch=self.clock() + timeout
            )
            return self.view(self.store.write_state(state), "renewed")

    def release(self, lease_id: str, operation_id: str) -> Dict[str, Any]:
        self._begin("release", lease_id)
        with self.store.lock():
            state = self.store.read_state()
            if state.get("phase") == "warm_idle" and state.get("released_lease_id") == lease_id:
                return self.view(state, "released")
            if state.get("phase") != "ready" or state.get("lease_id") != lease_id:
                return self.view(state, "ignored")
            if not self._owned(state):
                self._stop_server(state)
                return self.view(self._stopped(state, "server_not_running", operation_id))
            warm = int((state.get("policy") or {}).get("warm_seconds", 0))
            if warm <= 0:
                self._stop_server(state)
                return self.view(self._stopped(state, "released", operation_id))
            state = dict(state)
            state.update(
                phase="warm_idle",
                lease_id=None,
                lease_deadline_epoch=None,
                released_lease_id=lease_id,
                release_deadline_epoch=self.clock() + warm,
                last_operation_id=operation_id,
            )
            self.log(f"warm idle for {warm}s")
            return self.view(self.store.write_state(state), "released")

    def stop(self, lease_id: str, operation_id: str) -> Dict[str, Any]:
        self._begin("stop", lease_id)
        with self.store.lock():
            state = self.store.read_state()
            phase = state.get("phase")
            if phase not in ACTIVE_PHASES:
                return self.view(state, "already_stopped")
            yours = lease_id in (state.get("lease_id"), state.get("released_lease_id"))
            if phase != "starting" and not yours:
                return self.view(state, "ignored")
            self._stop_server(state)
            return self.view(self._stopped(state, "explicit_stop", operation_id))

    # --- sweep ---------------------------------------------------------------

    def sweep(self, *, wait: bool = False) -> Dict[str, Any]:
        """End abandoned starts, expired leases and expired warm servers.

        Safe to run often, from a detached sweeper or launchd. If an operation holds
        the lock, it is left to finish and this run is skipped.
        """
        self._begin("sweep")
        try:
            with self.store.lock(wait=wait):
                return self._sweep_locked()
        except BusyError:
            return {"status": "busy"}

    def _sweep_locked(self) -> Dict[str, Any]:
        state = self.store.read_state()
        phase = state.get("phase")
        if phase not in ACTIVE_PHASES:
            return self.view(state, "idle")
        now = self.clock()
        reason: Optional[str] = None
        if phase == "starting":
            reason = "abandoned_start"
        elif not self._owned(state):
            reason = "server_not_running"
        elif phase == "ready" and now >= float(state.get("lease_deadline_epoch") or 0):
            reason = "lease_expired"
        elif phase == "warm_idle" and now >= float(state.get("release_deadline_epoch") or 0):
            reason = "warm_window_over"
        if reason is None:
            return self.view(state, "kept")
        self._stop_server(state)
        return self.view(self._stopped(state, reason))

    def run_sweeper(self, watch_pid: int, interval: float) -> None:
        """Body of the detached sweeper: sweep on a timer until its server is gone."""
        self._begin("sweeper")
        while True:
            self.sleep(interval)
            try:
                self.sweep()
                state = self.store.read_state()
            except ManagerError as exc:
                self.log(f"sweeper error: {str(exc)[:200]}")
                continue
            if state.get("server_pid") != watch_pid or state.get("phase") not in ACTIVE_PHASES:
                return

    # --- read-only views -----------------------------------------------------

    def status(self) -> Dict[str, Any]:
        state = self.store.read_state()
        observed: Dict[str, Any] = {
            "process_owned": False,
            "health_ok": False,
            "gpu": self.system.gpu_memory(),
            "checked_at": utc_now(),
        }
        if state.get("phase") in ACTIVE_PHASES and state.get("model"):
            observed["process_owned"] = self._owned(state)
            if observed["process_owned"]:
                try:
                    profile = load_manifest(self.manifest_path).profile(str(state["model"]))
                    probe(self.system, profile)
                    observed["health_ok"] = True
                except (ManagerError, NotReady) as exc:
                    observed["health_error"] = str(exc)[:300]
        return {**self.view(state), "observed": observed}

    def identity(self, model: Optional[str] = None) -> Dict[str, Any]:
        """Serving identity for the node inventory. The node allows ten seconds, so this
        never hashes files and reuses the cached `--version` output. Without `model`, it
        describes the loaded profile, or the only one; with several idle, none."""
        manifest = load_manifest(self.manifest_path)
        aliases = manifest.aliases
        result: Dict[str, Any] = {
            "servable": aliases[:32],
            "models": self._models(manifest, aliases[:32]),
            "checked_at": utc_now(),
        }
        state = self.store.read_state()
        alias = model or (state.get("model") if state.get("phase") in ACTIVE_PHASES else None)
        if model is None and alias not in aliases:
            alias = aliases[0] if len(aliases) == 1 else None
        if alias is None:
            return result
        profile = manifest.profile(str(alias))
        result["model"] = profile.model_label
        app = application_version(profile.executable)
        if app is not None:
            result["application"] = app
        try:
            version = cached_server_version(
                self.system, self.store, profile.executable, self._server_environment(profile), timeout=6.0
            )
        except ManagerError:
            return result
        build, commit = parse_build(version)
        result["build"] = f"build {build}" if build is not None else version_line(version)[:200]
        if commit:
            result["commit"] = commit
        return result

    @staticmethod
    def _models(manifest: Any, aliases: List[str]) -> List[Dict[str, Any]]:
        """Each servable model's id, label and context, read from the manifest only,
        so Poundlock can offer them without asking for the context by hand. A profile
        that does not parse is left out rather than failing the whole identity."""
        models = []
        for alias in aliases:
            try:
                profile = manifest.profile(alias)
            except ManagerError:
                continue
            models.append(
                {"id": alias, "label": profile.model_label, "context_tokens": profile.context_tokens}
            )
        return models

    def preflight(self, alias: Optional[str] = None) -> Dict[str, Any]:
        """Verify model files and server builds without starting anything. Run it once
        after installing: it fills the hash cache so the first ensure is fast."""
        manifest = load_manifest(self.manifest_path)
        targets: List[str] = [alias] if alias else manifest.aliases
        models = {}
        for name in targets:
            profile = manifest.profile(name)
            models[name] = {
                "profile_digest": profile.digest,
                "evidence": verify_artifacts(
                    self.system, self.store, profile, self._server_environment(profile)
                ),
            }
        return {"schema_version": 1, "status": "verified", "models": models}
