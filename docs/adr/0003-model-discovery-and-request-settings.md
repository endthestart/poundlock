# ADR 0003: Model discovery, reasoning settings, and retirement

Status: Accepted for 0.1.3 (contract revision 4)

## Decision

Three states are kept apart everywhere an operator or client sees a model. A
**catalog listing** means a provider's public catalog lists it. That proves it
exists and what it advertises, not that this account may use it. A
**configured route** means LiteLLM routes it with a credential. A **published
profile** means Poundlock offers it to applications. Discovery reads LiteLLM's
`/model/info` with the endpoint's administrative key and the public OpenRouter
catalog. It publishes only configured routes. A route that does not exist yet
is created in LiteLLM's own Admin UI (ADR 0001: Poundlock does not write routes
or credentials). Testing a route uses LiteLLM's `/health`, so the
administrative key never carries inference.

Reasoning is chosen per request by the client, and Poundlock never rewrites it.
Precedence, most specific first:

1. an explicit request value;
2. the client's role default (Dispatch);
3. the profile's `thinking` default;
4. the upstream model's own default.

Omitting the field and sending `none` are different choices. A profile
advertises the explicit levels that a live request showed taking effect.
`none` must produce no reasoning tokens, and every other level must produce
some. A level that merely did not fail is not verified, because LiteLLM's
`drop_params` discards a parameter it does not map. `max` is never offered,
because LiteLLM's OpenRouter adapter sends it as `xhigh`. A value outside the
vocabulary, or outside a profile's verified list, is refused with
`unsupported_reasoning_effort`. A profile with no verified levels passes a
valid value through as before, so existing clients keep working. Durable jobs
keep the profile default.

llama.cpp is the one engine where the level is carried differently: its server
default beats a top-level `reasoning_effort`, so for a profile on the `llamacpp`
runtime the relay sends the same level as `chat_template_kwargs`
(`enable_thinking` and `reasoning_effort`). That is a translation of the
client's choice, not a change to it, and keys the client set are kept. Such a
profile also takes a bounded `thinking_budget_tokens`, and is verified from the
returned reasoning text when the server reports no token count.

A profile is **disabled** after three consecutive authoritative not-found
answers from its upstream, with the first and last at least thirty minutes
apart. An authoritative answer is a 404 or 400 that names the model or route
as missing. Rate limits, timeouts, server errors, busy slots and sleeping
machines never count and never reset the count. Any success resets it. A
disabled profile stays listed with its reason, source and last check. It is
refused for inference and new jobs, and only an operator re-enables it.

Served model, provider and price are recorded per profile with their source.
The relay reports the served model and the gateway's per-call cost estimate in
`x-poundlock-served-model` and `x-poundlock-response-cost`. They are for
display only and never used for routing. LiteLLM stays authoritative for spend.

## Consequences

The catalog-to-gateway handoff opens Models + Endpoints rather than the generic
Admin UI landing page. It keeps the selected upstream ID and alias visible, lets
the operator choose a gateway or existing route, and reads back the saved route
before offering publication. Discovery also compares the current served model
with a published profile and flags drift without mutating the profile. Explicit
refresh updates identity and price and discards old-model reasoning verification;
labels, destinations and limits remain operator-owned. Neither action infers.

Operators can tell "listed", "routable" and "offered" apart, and cannot publish
something no route serves. Clients offer only settings shown to work, and a
setting that would be silently dropped is refused instead. A model that
disappears upstream stops failing requests one by one, but a flaky provider
does not lose its profile.

Operator actions (discovery, publish, verify, enable) sit behind the single
administrative token. Verification sends its probes through a controller-owned
application token named "Poundlock operator checks", whose secret is discarded
at creation. On a managed gateway, it gets its own scoped virtual key.
Application tokens never reach operator routes or gateway credentials.

Multi-user operation is deferred to a public follow-up. It will add named
operator identities, roles that separate publishing from spending, and an audit
trail of who published, verified, disabled or re-enabled which profile.
