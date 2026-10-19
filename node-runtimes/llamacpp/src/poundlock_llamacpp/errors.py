"""Expected failures, each with a stable command exit code."""

from __future__ import annotations


class ManagerError(RuntimeError):
    """The request is invalid or refused."""

    exit_code = 2


class ConflictError(ManagerError):
    """The slot, the GPU or the port belongs to something else right now."""

    exit_code = 3


class BusyError(ConflictError):
    """Another operation holds the runtime lock."""


class InfrastructureError(ManagerError):
    """The requested model could not be made ready, or state is unusable."""

    exit_code = 4


class NotReady(Exception):
    """A readiness probe failed in a way that may pass on the next attempt."""
