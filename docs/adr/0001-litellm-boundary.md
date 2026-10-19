# ADR 0001: Keep LiteLLM optional and operator-owned

Status: Accepted for 0.1.1

## Decision

Poundlock works directly with OpenAI-compatible model servers without LiteLLM.
When LiteLLM is connected, its operators continue to own provider definitions,
model routes, equivalent-deployment retries, fallbacks, prices, and organization
limits. Poundlock publishes only profiles an operator deliberately saves.

Poundlock mutates LiteLLM only when `managed_applications` is selected. Those
mutations create, inspect, update, and revoke application-scoped virtual keys.
Poundlock does not create or rewrite LiteLLM model routes or fallback policy.

A LiteLLM endpoint has one explicit mode:

- `gateway_only` uses the saved inference credential and offers no
  Poundlock-visible per-application accounting.
- `managed_applications` validates a database-backed LiteLLM management API,
  uses the saved administrative credential only for key lifecycle operations,
  and relays inference with the generated scoped key.

The optional reference Compose files are for evaluation and small installations.
They do not make LiteLLM part of Poundlock's lifecycle or prevent an operator
from connecting an existing deployment.

## Consequences

Poundlock remains useful on one private machine. Larger installations can reuse
LiteLLM rather than duplicating its gateway, provider, accounting, or retry
features. Route changes inside LiteLLM remain invisible to Poundlock, so the
operator must revalidate and resave the affected profile to change its routing
revision.
