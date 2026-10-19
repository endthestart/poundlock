# ADR 0002: Require explicit external access and managed budgets

Status: Accepted for 0.1.1

## Decision

`inference:invoke` authorizes ordinary inference. A separate `external:invoke`
permission authorizes sending request data through profiles declared
`external`. Poundlock enforces it before wake, managed-key provisioning, or an
upstream request. For the single-user MVP, it covers every deliberately
published external profile; per-application profile allowlists are deferred.
Profiles with an `unknown` destination remain visible but are blocked until
the operator confirms the route as `operator_network` or `external`.

Managed application access to an external profile also requires a positive
maximum spend and one of the supported reset periods: daily, weekly, or monthly.
The endpoint policy is copied independently to every application virtual key.
LiteLLM remains authoritative for spend, reset enforcement, prices, and global
limits. Poundlock reports application-scoped usage and freshness without keeping
a second billing ledger.

Blocked profiles remain visible in discovery with an availability state and
machine-readable reason. `POST /api/v1/access/preflight` validates or creates the
calling application's managed identity without inference. Inference retains the
same idempotent preparation as a recovery path.

## Consequences

Local permission never silently authorizes external disclosure or cost. Clients
can explain a denial before work begins. Missing budget, permission, or managed
identity state is visible instead of becoming an unexplained fallback or generic
upstream failure.
