# ADR 0001: Keep LiteLLM optional and operator-owned

Status: Accepted for 0.1.1

## Decision

Poundlock works directly with OpenAI-compatible model servers without LiteLLM.
When LiteLLM is connected, its operators continue to own provider definitions,
model routes, equivalent-deployment retries, fallbacks, prices, and organization
limits. Poundlock publishes only profiles an operator deliberately saves.

Poundlock mutates LiteLLM only when `managed_applications` is selected. Those
mutations create, inspect, update, and revoke application-scoped virtual keys,
and add a model route the operator confirms (amended 27 Oct 2026, below).
Poundlock does not rewrite or delete LiteLLM model routes, and never touches
credentials or fallback policy.

A LiteLLM endpoint has one explicit mode:

- `gateway_only` uses the saved inference credential and offers no
  Poundlock-visible per-application accounting.
- `managed_applications` validates a database-backed LiteLLM management API,
  uses the saved administrative credential only for key lifecycle operations,
  and relays inference with the generated scoped key.

The optional reference Compose files are for evaluation and small installations.
They do not make LiteLLM part of Poundlock's lifecycle or prevent an operator
from connecting an existing deployment.

## Amendment, 27 Oct 2026: adding a route the operator confirms

Typing each new OpenRouter model into LiteLLM's Admin UI was slow and error
prone, and a route without prices counts no spend, so no budget stops it. The
owner chose a one-click add instead. Poundlock now creates a route in one case
only: the operator picks a model from the OpenRouter catalog and confirms a card
showing its route name, the existing named credential it will use, its prices,
context and maximum output, and where its prompts go. Poundlock then calls
LiteLLM's `/model/new` with the catalog's prices as the route's own cost per
token, and publishes the profile in the same step. It adds nothing without that
confirmation, never replaces an existing route of the same name, and never
creates or reads provider credentials beyond their names.

## Consequences

Poundlock remains useful on one private machine. Larger installations can reuse
LiteLLM rather than duplicating its gateway, provider, accounting, or retry
features. Route changes inside LiteLLM remain invisible to Poundlock, so the
operator must revalidate and resave the affected profile to change its routing
revision.
