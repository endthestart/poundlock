"""Command line for the Poundlock node agent: one JSON object on stdout per action.

    poundlock-llamacpp [--home DIR] [--manifest FILE] COMMAND ...

    ensure MODEL LEASE_ID OPERATION_ID    start or reuse the server for MODEL
    renew|release|stop [MODEL] LEASE_ID OPERATION_ID   (MODEL is accepted and ignored)
    identity [MODEL] | status | sweep | preflight [MODEL]
    launchd-plist [--executable PATH]     print the macOS sweep job for `launchctl`

Exit codes: 0 done, 2 invalid or refused, 3 conflict or busy, 4 could not be made ready.
Errors are one JSON object on stderr. Nothing printed includes arguments or environment.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional
from uuid import UUID

from . import __version__
from .errors import ManagerError
from .launchd import render_plist
from .manager import Manager
from .manifest import ALIAS_PATTERN, default_manifest_path
from .platforms import detect_system
from .system import System


class Terminated(BaseException):
    """Raised on SIGTERM/SIGINT so an ensure in progress can remove its server."""


def _terminate(signum: int, _frame: Any) -> None:
    raise Terminated(f"signal {signum}")


def require_uuid(value: str, label: str) -> str:
    try:
        parsed = UUID(value)
    except ValueError:
        raise ManagerError(f"{label} must be a UUID") from None
    if str(parsed) != value:
        raise ManagerError(f"{label} must use canonical lowercase UUID form")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="poundlock-llamacpp", description="Manage one llama-server for a Poundlock node."
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    parser.add_argument(
        "--home",
        type=Path,
        default=os.environ.get("POUNDLOCK_LLAMACPP_HOME"),
        help="state, lock and log directory (env POUNDLOCK_LLAMACPP_HOME)",
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=os.environ.get("POUNDLOCK_LLAMACPP_MANIFEST"),
        help="manifest (.json, or .toml on Python 3.11+); default HOME/manifest.json",
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")
    ensure = commands.add_parser("ensure", help="start or reuse the server for MODEL under LEASE_ID")
    ensure.add_argument("model")
    ensure.add_argument("lease_id")
    ensure.add_argument("operation_id")
    for name, text in (
        ("renew", "extend the lease"),
        ("release", "end the lease; keep the server warm for the warm window"),
        ("stop", "stop the server now if the lease matches"),
    ):
        sub = commands.add_parser(name, help=text)
        sub.add_argument("ids", nargs="+", metavar="ID", help="[MODEL] LEASE_ID OPERATION_ID")
    identity = commands.add_parser("identity", help="what this runtime serves, for the node inventory")
    identity.add_argument("model", nargs="?")
    commands.add_parser("status", help="state plus live process and health observation")
    commands.add_parser("sweep", help="end abandoned starts, expired leases and warm servers")
    preflight = commands.add_parser(
        "preflight", help="verify model files and server builds; start nothing"
    )
    preflight.add_argument("model", nargs="?")
    plist = commands.add_parser("launchd-plist", help="print the macOS launchd sweep job")
    plist.add_argument("--executable", type=Path, help="installed poundlock-llamacpp path")
    plist.add_argument("--interval", type=int, default=60, help="seconds between sweeps")
    # Internal: the detached sweeper the runtime starts beside its server (no help line).
    sweeper = commands.add_parser("sweeper")
    sweeper.add_argument("--watch-pid", type=int, required=True)
    sweeper.add_argument("--interval", type=float, default=30.0)
    return parser


def dispatch(manager: Manager, args: argparse.Namespace) -> Dict[str, Any]:
    command = args.command
    if command == "ensure":
        return manager.ensure(
            args.model,
            require_uuid(args.lease_id, "lease_id"),
            require_uuid(args.operation_id, "operation_id"),
        )
    if command in ("renew", "release", "stop"):
        if len(args.ids) not in (2, 3):
            raise ManagerError(f"{command} takes [MODEL] LEASE_ID OPERATION_ID")
        if len(args.ids) == 3 and not ALIAS_PATTERN.match(args.ids[0]):
            raise ManagerError("MODEL is not a valid model id")
        action = {"renew": manager.renew, "release": manager.release, "stop": manager.stop}[command]
        return action(
            require_uuid(args.ids[-2], "lease_id"),
            require_uuid(args.ids[-1], "operation_id"),
        )
    if command == "identity":
        return manager.identity(args.model)
    if command == "status":
        return manager.status()
    if command == "sweep":
        return manager.sweep()
    if command == "preflight":
        return manager.preflight(args.model)
    manager.run_sweeper(args.watch_pid, args.interval)
    return {"status": "sweeper_exited"}


def print_launchd_plist(manager: Manager, args: argparse.Namespace) -> None:
    executable = args.executable or Path(sys.argv[0])
    sys.stdout.buffer.write(
        render_plist(
            executable.expanduser().absolute(), manager.home, manager.manifest_path, args.interval
        )
    )


def build_manager(args: argparse.Namespace, system: System) -> Manager:
    home = (args.home or system.default_home()).expanduser().absolute()
    manifest = (args.manifest or default_manifest_path(home)).expanduser().absolute()
    return Manager(home=home, manifest_path=manifest, system=system)


def main(argv: Optional[List[str]] = None, *, system: Optional[System] = None) -> int:
    os.umask(0o077)
    args = build_parser().parse_args(argv)
    handlers = {}
    try:
        manager = build_manager(args, system or detect_system())
        if args.command in ("ensure", "renew", "release", "stop"):
            for number in (signal.SIGTERM, signal.SIGINT):
                handlers[number] = signal.signal(number, _terminate)
        if args.command == "ensure":
            # Ignored signals stay ignored across exec: the server then survives a
            # closing terminal the way `nohup` makes it.
            handlers[signal.SIGHUP] = signal.signal(signal.SIGHUP, signal.SIG_IGN)
        if args.command == "launchd-plist":
            print_launchd_plist(manager, args)
            return 0
        result = dispatch(manager, args)
    except ManagerError as exc:
        _report(type(exc).__name__, str(exc))
        return exc.exit_code
    except Terminated as exc:
        _report("Terminated", f"interrupted by {exc}; nothing was left running")
        return 143
    except Exception as exc:
        _report(type(exc).__name__, str(exc)[:500])
        return 4
    finally:
        for number, handler in handlers.items():
            signal.signal(number, handler)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


def _report(error: str, message: str) -> None:
    print(
        json.dumps({"schema_version": 1, "error": error, "message": message}, sort_keys=True),
        file=sys.stderr,
    )


if __name__ == "__main__":
    raise SystemExit(main())
