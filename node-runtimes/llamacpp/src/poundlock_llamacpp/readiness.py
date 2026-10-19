"""Readiness gates: HTTP probes, log markers and the warm-up request."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Dict, FrozenSet, List, Optional

from .errors import InfrastructureError, NotReady
from .manifest import Profile, Warmup, read_api_key
from .state import utc_now
from .system import RequestFailed, System

MARKER_ATTEMPTS = 4


def json_has_int(value: Any, key: str, accepted: FrozenSet[int]) -> bool:
    if isinstance(value, dict):
        if value.get(key) in accepted and not isinstance(value.get(key), bool):
            return True
        return any(json_has_int(child, key, accepted) for child in value.values())
    if isinstance(value, list):
        return any(json_has_int(child, key, accepted) for child in value)
    return False


def lists_alias(models: Any, alias: str) -> bool:
    """llama-server lists the served alias under data[].id and models[].model/name."""
    if not isinstance(models, dict):
        return False
    for group, keys in (("data", ("id",)), ("models", ("model", "name"))):
        for item in models.get(group) or []:
            if isinstance(item, dict) and any(item.get(key) == alias for key in keys):
                return True
    return False


def refused(profile: Profile, error: RequestFailed) -> Optional[InfrastructureError]:
    """A 401 or 403 is final: waiting does not change which key the server holds."""
    if error.status not in (401, 403):
        return None
    if profile.api_key_file is None:
        return InfrastructureError(f"{error}: llama-server wants an API key; set server.api_key_file")
    return InfrastructureError(
        f"{error}: llama-server refused the key in server.api_key_file; "
        "it reads the file only when it starts"
    )


def probe(system: System, profile: Profile) -> Dict[str, Any]:
    """Cheap HTTP checks. A failed call is NotReady; a wrong answer or a refused key is fatal."""
    api_key = read_api_key(profile)
    try:
        health = system.http_json("GET", f"{profile.endpoint}/health", api_key=api_key)
        models = system.http_json("GET", f"{profile.endpoint}/v1/models", api_key=api_key)
        props = system.http_json("GET", f"{profile.endpoint}/props", api_key=api_key)
    except RequestFailed as exc:
        raise (refused(profile, exc) or NotReady(str(exc))) from None
    if not lists_alias(models, profile.alias):
        raise InfrastructureError("the model id is not listed by the server's /v1/models")
    if not json_has_int(props, "n_ctx", profile.accepted_n_ctx):
        raise InfrastructureError(
            f"the server context does not match the profile ({profile.context_tokens} tokens)"
        )
    return {
        "health": health if isinstance(health, dict) else {},
        "model_id": profile.alias,
        "context_tokens": profile.context_tokens,
        "verified_at": utc_now(),
    }


def check_markers(
    system: System, profile: Profile, log_path: Path, sleep: Callable[[float], None]
) -> List[str]:
    """Every required marker must be in the server log. The log can lag health briefly."""
    if not profile.required_log_markers:
        return []
    missing: List[str] = list(profile.required_log_markers)
    for attempt in range(MARKER_ATTEMPTS):
        try:
            log = system.read_log(log_path)
        except OSError:
            log = ""
        missing = [marker for marker in profile.required_log_markers if marker not in log]
        if not missing:
            return list(profile.required_log_markers)
        if attempt + 1 < MARKER_ATTEMPTS:
            sleep(1.0)
    raise InfrastructureError("required server log marker is absent: " + ", ".join(missing))


def _completion(
    system: System, profile: Profile, warmup: Warmup, tokens: int, timeout: float
) -> Dict[str, Any]:
    body: Dict[str, Any] = {
        "model": profile.alias,
        "messages": [{"role": "user", "content": warmup.prompt}],
        "max_tokens": tokens,
        "temperature": 0,
        "stream": False,
    }
    body.update(warmup.request)
    try:
        result = system.http_json(
            "POST",
            f"{profile.endpoint}/v1/chat/completions",
            body,
            timeout=max(1.0, timeout),
            api_key=read_api_key(profile),
        )
    except RequestFailed as exc:
        raise (refused(profile, exc) or InfrastructureError(f"warm-up request failed: {exc}")) from None
    if not isinstance(result, dict):
        raise InfrastructureError("warm-up response is not a JSON object")
    return result


def run_warmup(
    system: System,
    profile: Profile,
    remaining: Callable[[], float],
    gpu_summary: Callable[[], str],
) -> Dict[str, Any]:
    """Prime the server, then measure one completion.

    The first request pays for lazy setup (Vulkan shader compilation, cache warm-up),
    so it is not measured. The gates: generation works, thinking is off when required,
    and decode speed clears the minimum, which catches a VRAM spill into system memory.
    """
    warmup = profile.warmup
    if warmup is None:
        return {}
    _completion(system, profile, warmup, warmup.prime_tokens, min(warmup.timeout_seconds, remaining()))
    result = _completion(
        system, profile, warmup, warmup.max_tokens, min(warmup.timeout_seconds, remaining())
    )
    choices = result.get("choices")
    message = choices[0].get("message") if isinstance(choices, list) and choices else None
    reasoning = ""
    if isinstance(message, dict):
        reasoning = str(message.get("reasoning_content") or "")
    timings = result.get("timings")
    try:
        tokens_per_second = float(
            (timings.get("predicted_per_second") if isinstance(timings, dict) else 0) or 0.0
        )
    except (TypeError, ValueError):
        tokens_per_second = 0.0
    if warmup.require_no_reasoning and reasoning:
        raise InfrastructureError(
            f"thinking is on: {len(reasoning)} characters of reasoning_content in the warm-up reply"
        )
    minimum = warmup.min_tokens_per_second
    if minimum is not None and tokens_per_second < minimum:
        raise InfrastructureError(
            f"warm-up decode {tokens_per_second:.1f} tok/s is below the minimum {minimum:g}; "
            f"a VRAM spill is likely ({gpu_summary()})"
        )
    return {
        "tokens_per_second": round(tokens_per_second, 1),
        "reasoning_characters": len(reasoning),
        "verified_at": utc_now(),
    }


def gpu_summary(readings: List[Dict[str, Any]]) -> str:
    if not readings:
        return "no GPU memory readings on this host"
    return "; ".join(
        f"{item['card']} vram_used={item.get('vram_used')} gtt_used={item.get('gtt_used')}"
        for item in readings
    )

