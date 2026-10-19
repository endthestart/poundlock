# Editors and agent integrations

Create a named application token in Access for each integration. Give it only the
permissions it needs. The administrator token is for the operator console and administration.

## OpenAI-compatible clients

Point the client to your controller URL plus `/v1`, use its application token as
the API key, and select a model profile name. Aliases are optional names for the
same profile. Chat uses `/v1/chat/completions`; fast completion uses
`/v1/completions`. `/v1/models` lists profile names and their configured limits.
`poundlock.uses` says what a profile is meant for (`interactive`, `completion`,
`durable`); clients choose by it, but it does not restrict which requests the
relay answers. Token permissions and the profile's data destination do.

Send the **model name** exactly as `/v1/models` lists it in `id`. The machine's
own string for the same weights is the **server model ID**
(`poundlock.upstream_model`). It is never accepted as a request name, because
several names can share one server model ID. A request whose `model` names
nothing gets 404 `unknown_model_profile` with `param: "model"`; the message says
which name to send. For a token that also has `models:read`,
`error.poundlock.did_you_mean` lists up to five names, exact server model ID
matches first (`reason: "server_model_id"`). The status and code do not change;
see `docs/reference/fixtures/unknown-model-profile-error.json`.

Testing a server or listing its model IDs only confirms what it reports; it
publishes nothing. Clients see a model once you add it in **Models**, where each
model shows both strings with a copy button: the model name clients send and the
server model ID. Each advertised profile includes `poundlock.endpoint_kind`
(`openai` or `litellm`). This identifies the configured adapter, not whether the
model is local, paid, more capable, or eligible for escalation. It also includes
`poundlock.routing_revision`, a stable hash of the Poundlock-known route and
policy. Preserve an approval only while the profile ID and revision still match.
Resaving a profile or endpoint deliberately changes the revision. After changing
a route only inside LiteLLM, revalidate and resave the Poundlock profile because
Poundlock cannot detect that internal change.

Call authenticated `GET /api/v1/capabilities` before relying on optional
features. Contract revision 4 advertises `verifier_toolchain` (CPython series,
uv, and writable exec paths on the worker/verifier image), `capability_evidence`
(measured capability evidence on `/v1/models` and job-options rows; see below),
`reasoning_levels`, `route_catalog` and `model_status` (ADR 0003),
and explicit availability reasons, access preflight, external permission,
managed application identities, and usage summary support. The application
OpenAPI document is available at `/api/openapi.yaml`. Clients must tolerate
additive fields and should show an actionable incompatibility when a required
feature is absent.

Exclusive interactive and durable
profiles also include `busy` and `occupancy` (`kind` is `interactive`, `job`, or
null, plus the machine and runtime slot) so a client can pick another profile
instead of retrying a busy slot. Those fields do not include endpoint URLs, API
keys, or operator overview data.

### Inference destination and client compatibility

Where prompts go is the operator's declaration, never inferred. A model server
declares it once, in **Connections** (or the first time a model is added to it),
and every model on it inherits it. A gateway declares it for each route when the
route is published, because one gateway can send some routes to local models and
others to external services. Application tokens receive each model's destination
as `poundlock.data_destination` on `/v1/models` and `data_destination` on durable
`/api/v1/job-options` rows:

| Value | Meaning |
|---|---|
| `unknown` | Destination has not been confirmed; the default for new and upgraded profiles. |
| `operator_network` | Prompts are processed within the operator's controlled machines/network, which may include multiple hosts. |
| `external` | Prompts are sent to a service or network outside that boundary. |

This declaration does not enforce routing, certify a gateway's configuration or
authorize disclosure. If fallback can leave the controlled network, declare
`external`; if the route cannot be confirmed, use `unknown`. Unknown profiles
remain visible for repair but cannot be invoked or submitted until the operator
confirms their destination.

In the administration API, a model server's endpoint carries `data_destination`
(set it on create or with PATCH, alone or with other changes), and each model
reports `data_destination_source`: `endpoint` when it inherits its server's,
`profile` when it carries its own. A model on a model server may restate its
server's value but not contradict it; the first model on a server nobody has
declared declares it. Declaring a server applies to every model on it.

Recheck declarations after upstream routing changes. A new address or adapter
makes an endpoint's destination `unknown` until it is declared again, which the
same save may do; rotating its credential does not, because it changes neither
where data goes nor how it gets there. An API edit that would change where a
model's prompts go without saying so, such as moving it to a server that
declares otherwise or re-pointing a gateway route, must state `data_destination`
in the same request, or it is refused with 400. The declaration never silently
becomes `unknown` and takes the profile offline, and the console says when an
edit needs a new one.

A database upgraded from an earlier release keeps every model's destination.
A model server whose live models all declared the same destination declares it
from then on, and they inherit it. Where they disagreed, the server stays
undeclared and each model keeps its own until the operator declares the server.

The pre-release `leaves_this_computer` field was removed because gateway type
cannot establish that fact. Clients must treat an absent or unrecognized
destination as unknown, not local. Use profile IDs to collapse aliases, keep
explicit user role choices, and require approved policy before changing models
or sending source to an external destination. Model names are not capabilities.
Testing a server or listing its models still publishes nothing.

Profiles remain in discovery when an application cannot currently use them.
Read `availability` and `availability_reason` (also present inside `poundlock`):

| Availability | Client behavior |
|---|---|
| `available` | Policy prerequisites are satisfied; admission can still race. |
| `preparation_required` | Call access preflight before starting the operation. |
| `busy` | Wait, poll, or make an explicitly approved selection. |
| `blocked` | Show `blocked_reason`; do not retry or reroute silently. |

### Thinking default

In Models, **Thinking** (under Advanced) records the operator's default reasoning
setting for each model profile: `model_default` (send nothing; the model's own
default applies) or `off`. A new model on a machine's `llamacpp` runtime starts
at `off`, and any other at `model_default`. Application tokens receive
`poundlock.thinking` on `/v1/models` and `thinking` on durable
`/api/v1/job-options` rows.

A profile whose thinking default is `off` is meant for local reasoning models
(for example Qwen 3.5/3.6 on Ollama or llama.cpp) that think unless the request
carries `reasoning_effort: "none"`. For that profile, Poundlock's relay injects
`"reasoning_effort": "none"` into interactive `/v1/chat/completions` requests
whose body does not already set `reasoning_effort`, a `reasoning` object, or
`chat_template_kwargs.enable_thinking`. It never overrides an explicit client
value, so a client can still force thinking on for a single request. It applies
to chat completions only; the fast-completion `/v1/completions` path is left
untouched. (A llama.cpp profile does this differently; see below.) Durable jobs
reach the model through the controller's job-scoped proxy, which applies the same
default.

#### llama.cpp profiles

llama-server keeps its own thinking default, and that default beats a top-level
`reasoning_effort`: the level is accepted and does nothing. Only the request's
`chat_template_kwargs` switches thinking. So for a profile on the `llamacpp`
node runtime (the name the node configuration gives it, `[runtimes.llamacpp]`),
the relay carries the choice there. It translates the choice and never changes
it: the level the client names is the level the template receives. Keys the
client already put in `chat_template_kwargs` are never replaced, and a client
that sets `enable_thinking` itself is left as sent.

| Level in the request | Profile thinking | Sent to llama.cpp |
| --- | --- | --- |
| `none` | any | `chat_template_kwargs: {enable_thinking: false}` |
| `minimal`, `low`, `medium`, `high`, `xhigh` | any | `{enable_thinking: true, reasoning_effort: <level>}` |
| none named | `off` | `{enable_thinking: false}` |
| none named | `model_default` | nothing; the server's default applies |

Durable jobs and the reasoning probe use the same rule. Every other runtime is
unchanged.

A request to a llama.cpp profile may also carry `thinking_budget_tokens`, a
whole number from 1 to the profile's `max_output_tokens`, which llama.cpp uses to
end reasoning early. The reasoning text shares the output limit with the answer,
so a budget at or near the limit bounds nothing. A value outside that range is
refused with `invalid_thinking_budget`, and any other profile refuses the field
by name as before. `poundlock.reasoning.thinking_budget_tokens` on `/v1/models`
(`max` and `default`) appears only on llama.cpp profiles. An operator can set
`thinking_budget_tokens` on the profile through the admin API
(`PATCH /api/v1/model-profiles/{id}`; `null` clears it). It fills in for a request
that turns thinking on and names no budget of its own, and it never applies when
thinking is off.

Verifying a level (`verify-reasoning`) needs to see reasoning. llama.cpp builds
that report no reasoning-token count are read from the returned
`reasoning_content` instead: each result carries `reasoning_chars`, the number of
characters of reasoning text (characters, not tokens), and a level is verified
when thinking is on and text comes back, or off and none does. Profiles on other
runtimes are verified only from a reported token count, as before.

Requests are checked against the options Poundlock understands, and an unknown
key is refused by name (`unsupported inference option: <key>`). For a profile
whose `data_destination` is `operator_network`, the local-runtime controls
`chat_template_kwargs`, `top_k`, `min_p` and `repeat_penalty` are also accepted,
on both the interactive relay and durable jobs. Hosted APIs generally reject
them, so they are refused for any other destination.

### Capability evidence

In Models, the profile editor’s Advanced section lets an operator paste a JSON block of
measured capability evidence for a profile. Make it from
[Poundlock Toolkit](https://github.com/endthestart/poundlock-toolkit) 0.5.0 or later
results:

```sh
poundlock-toolkit recommend --format json run.json [more runs] --output summary.json
python3 -m pip install 'jsonschema>=4.23,<5'
python3 scripts/toolkit-capabilities.py summary.json > block.json
```

The script reads Toolkit's versioned summary
(`urn:poundlock-toolkit:recommendation:1`) and does not import Toolkit. It refuses a
run that sent an explicit reasoning effort, because a profile's thinking default is
either the model's own or off, and a tool-schema run that measured the two APIs with
different thinking behaviour. `poundlock.capabilities` on `/v1/models` and `capabilities` on
durable `/api/v1/job-options` rows carry the list. Each entry names one of
`code_repair_verified`, `plan_tool_schema` or `sealed_investigation`, a
`status` (`qualified`, `smoke` or `insufficient`), `passed`/`attempted`
counts, and the settings it was measured under. **Evidence is measured, never
inferred: an empty list means unknown, not incapable.** There is no ingest
endpoint; scoring stays in the Toolkit, and Poundlock only stores and
republishes what it is given.

`/v1/models` augments each entry at read time with three fields Poundlock
never stores: `age_days`, `stale` (measured more than 30 days ago, mirroring
the Toolkit's own freshness rule) and `settings_match` (`exact` or
`different`, comparing `measured_settings.context_tokens` and reasoning
against the profile's live `context_tokens` and `thinking`). Treat a stale or
settings-mismatched entry as evidence about a configuration that may no
longer be this one, not as a current guarantee. Patching a profile's
`context_tokens` or `thinking` drops every measurement whose recorded
settings no longer match, for the same reason changing a route rotates
`routing_revision`: the measurement described a configuration that no longer
exists.

Known blocked reasons include `external_permission_required`,
`data_destination_confirmation_required`,
`managed_budget_required`, `managed_identity_provision_failed`, and
`managed_identity_reconciliation_required`. An external profile requires both
the ordinary operation permission and `external:invoke`.

Access preflight checks credentials and policy first. When those pass, an
inference preflight also reports the node/runtime state that admission would
hit: `runtime_node_missing`, `runtime_node_unavailable` (paused or removed),
`runtime_node_network_unavailable`, `runtime_node_offline` (the machine is
asleep and the model is set not to wake it) and `runtime_wake_unavailable` are
blocked reasons. Conditions the
relay resolves by itself do not change `availability`, so existing clients keep
working: a node that will be woken on demand adds `runtime_state:
"preparation_required"` with `runtime_reason: "runtime_wake_required"`, and an
occupied exclusive slot adds `capacity: "in_use"` with `capacity_reason:
"capacity_in_use"` (the request itself may then receive HTTP 409; retry the
same request). Job preflight never reports slot occupancy, because durable jobs
queue. A job preflight adds `runner_status`
(`connected` or `not_seen`) and warns `runner_not_connected` when no runner has
contacted the controller recently; queued jobs still wait for one. Failed
interactive requests use the same `runtime_*` codes when runtime preparation
fails, so clients can tell an unavailable machine from missing credentials or
a busy model. Interactive responses carry `x-poundlock-request-id`, which
matches controller diagnostics for that request.

`GET /readyz` reports controller/storage readiness only (`"scope":
"controller_storage"`): the controller is serving and its database answers.
It does not prove that a runner, node, endpoint, model or credential is usable;
use access preflight and job options for those.

`busy` only reports a current Poundlock-managed exclusive-slot claim. `false`
does not prove an endpoint is online, a model is loaded or a slot is reserved.
Profiles without an exclusive slot have no managed busyness measurement.
`occupancy.slot` names the exclusive slot a profile competes for
(`<node_id>/<runtime_profile>`), or is `null` when it has none. Profiles with
the same `slot` run one at a time across all their names and aliases, and
switching between two of them loads a different model. State can
change after discovery: interactive admission can return HTTP 409
`capacity_in_use`; durable jobs use the existing queue. Handle these outcomes
without duplicate submissions or unauthorized fallback. Poll without spending
model reasoning tokens. Job APIs require their jobs permissions independently
of `models:read`; application tokens cannot access operator configuration.

When a LiteLLM endpoint uses per-application identities, `POST
/api/v1/access/preflight` prepares access without inference and `GET /api/v1/usage`
returns that application token's configured gateway policy and safe usage
summary. It never returns gateway or provider keys. A gateway can report usage
as unavailable without making model discovery unavailable.

For Zed, configure Agent/Inline Assistant separately from Edit Prediction. See
[Zed's API provider guide](https://zed.dev/docs/ai/use-api-access) and
[Edit Prediction](https://zed.dev/docs/ai/edit-prediction). A starting configuration
is in `config/examples/zed-openai-provider.json` in the source repository.

An on-demand profile may wait while a node wakes and loads its model. Configure
client timeouts accordingly. Use durable jobs when work must continue after the
client disconnects; an interactive HTTP stream is not a durable job.

## Directing durable coding work

An orchestrator can submit a prompt, source and verification policy through
`POST /api/v1/jobs`, poll the returned job ID, download its patch and review the
result. It can then submit another job with reviewed source and a new prompt.
It does not need Docker access or knowledge of node addresses.

Use a token with `jobs:submit`, `jobs:read`, and optionally `jobs:cancel`.
`GET /api/v1/job-options` returns usable model profiles, harnesses and policy
limits. Model choice is supplied in every job. Independent jobs can target
separate machines concurrently; Poundlock does not automatically split a task or
merge conflicting patches. The orchestrator owns review and integration.

See [job API](jobs.md) for request examples and error handling.

## Optional ACP adapter

The native binary includes an experimental ACP adapter. Install Pi 1.1.0 locally
for attached execution, then configure an ACP-capable editor to launch:

```sh
poundlock acp --controller http://127.0.0.1:7331 \
  --token-file /absolute/private/path/poundlock-token \
  --execution attached --model-profile-id coding
```

The editor supplies a working directory. Attached mode runs Pi's file and shell
tools with your local user privileges and sends inference through Poundlock.
It is not containerized and does not upload the workspace. Cancellation stops
the process group owned by that turn. Keep the token file private.

Attached Pi trusts nothing the repository ships: it runs with project trust set
to `never` and `--no-approve`, and loads no extensions, skills, prompt templates
or context files, so a repository's `.pi` directory never runs code. Poundlock
still passes `AGENTS.md` and configured instruction files to the model as text.
Pi inherits only `PATH`, `LANG`, `TERM`, `TMPDIR` and `NODE_EXTRA_CA_CERTS` from
your environment; tokens and other secrets in your shell stay behind.

`--execution delegated --project-id PROJECT_ID` submits a durable job against an
operator-registered server repository. For uploaded files or read-only HTTPS
checkout, use the ordinary job API or web UI instead.

## Models and execution profiles

A profile is a request configuration, not necessarily a distinct model. One
configured model can have interactive and durable profiles with different limits.
Models → Ready groups profiles by machine or gateway; this is
configured routing identity, not a claim that a model is loaded or qualified.

`poundlock.model_label` on `/v1/models` and `model_label` on job-options rows is
an optional operator-declared display string, such as a model family, size and
quantization. Null means unspecified. It is not verified runtime identity, a
capability rating, or a request identifier. Continue routing with the canonical
profile name (interactive) or profile UUID (durable); `upstream_model` remains
the exact upstream routing ID and can itself be an alias.

Create accepts `model_label`; PATCH omission preserves it, null clears it. Labels
are limited to 256 UTF-8 bytes with no control characters. Changing the profile
route without a new declaration clears the label. Endpoint route or credential
changes also clear labels and reset destinations to unknown. Operators must
refresh declarations when an upstream alias changes its target outside Poundlock.

Renaming a profile preserves its UUID but can break consumers using its old
name. Inventory consumers, migrate saved bindings, and remove compatibility
aliases only after verification. Clients should collapse aliases using
`poundlock.profile_id`, preserve user role choices, and never infer destination,
readiness or performance from a model name. Toolkit qualification evidence is
published separately as `poundlock.capabilities` (see Capability evidence,
above); Poundlock still does not publish measured speed or quality scores.

Discovery returns one row per canonical profile. Compatibility aliases remain in
`poundlock.aliases` and continue to work as request names, but no longer appear
as duplicate `/v1/models` rows. When refreshing a saved alias binding, match it
against that metadata and retain the stable profile UUID.
