# Connect an existing LiteLLM gateway

Poundlock can use a working LiteLLM gateway alongside direct OpenAI-compatible
model servers. Keep direct local endpoints direct when that is the simplest
route. A gateway is useful when one place already manages provider credentials,
external model routes, accounting, or policy.

Provider configuration and provider keys stay at LiteLLM. Dispatch and other
clients receive one Poundlock application token and only the model profiles an
operator saves in Poundlock.

This example assumes the gateway exposes a route named `frontier-review`. It
does not install LiteLLM or require it for local models. Follow LiteLLM's
[configuration reference](https://docs.litellm.ai/docs/proxy/configs) and
[virtual-key documentation](https://docs.litellm.ai/docs/proxy/virtual_keys)
for the version you deploy; the end-to-end suite tests LiteLLM 1.102.1 in both
connection modes. Record the deployed version and verified policy in
private operational records.

## Publish a gateway route

1. Configure the upstream route in LiteLLM. Confirm its real provider,
   destination, context and output limits, tool support, and fallback behavior.
   A fallback that can leave the operator network makes the destination
   `external`.
2. In Poundlock **Connections**, add an endpoint with type **LiteLLM**, the gateway
   API base (for example `https://gateway.example.net/v1`), its gateway credential
   and, when you open LiteLLM through a reverse proxy, its public address. Never route the gateway back to the same Poundlock profile.
3. In **Models**, choose **Publish** on the route under Discover, or **Add model**
   with the gateway as where it runs. The review keeps the route name as the model
   name and takes the context from the route; check both against the limits you
   verified. Each route declares where its prompts go: choose **To an external
   service** when a request or its fallback can leave the operator network.
4. **Add model** publishes it. For managed application identities, run access
   preflight and Generate through an application token; Poundlock does not use
   the administrative key for inference.
5. Create a named application token in **Access**. The saved profile then appears
   in `GET /v1/models` with `endpoint_kind=litellm`, its declared destination,
   and a routing revision.

Listing routes or testing an endpoint publishes nothing. Poundlock publishes
only the models you add and does not copy LiteLLM's full catalog into client
discovery.

## Choose the connection mode

**Shared gateway key** (`gateway_only`) is the default. Poundlock authenticates to LiteLLM with the endpoint
credential for every request. This preserves existing gateways and is suitable
when accounting and limits do not need to distinguish Poundlock applications.

For separate accounting and policy, choose **Separate application identities and
budgets** (`managed_applications`) on the endpoint. This mode requires:

- LiteLLM proxy key-management APIs, including `key/generate`, `key/update`,
  `key/info`, and `key/block`;
- a LiteLLM database configuration that supports persistent virtual keys;
- an endpoint credential authorized to manage those keys; and
- at least one saved Poundlock profile on that gateway.

Saving a managed endpoint validates both model listing and the key-management
API. A successful model-list check alone does not prove that the administrative
credential can manage application identities.

Poundlock provisions one virtual key during application preflight or, as a
recovery path, the first time an authorized application
uses the gateway. The key is restricted to the upstream model IDs represented by
saved profiles on that endpoint and to LiteLLM's LLM API route class. External
profiles require the application token's `external:invoke` permission and a
configured maximum spend plus daily, weekly, or monthly reset. Key lifetime,
requests per minute, and tokens per minute remain optional advanced limits.
Poundlock sends configured values to LiteLLM; verify on your deployed LiteLLM
version that its database and edition enforce the policy you selected.

Managed mode fails closed when key management is unavailable. Poundlock does
not silently use the shared credential. The application receives a bounded
infrastructure error and the operator can see the failed identity under
**Access → LiteLLM client identities**. Correct the gateway and retry the
application request.

Poundlock encrypts managed virtual keys at rest. Application APIs and logs do
not return virtual keys, the gateway credential, or provider credentials.
Poundlock replaces caller-supplied identity fields with stable application-token,
request, profile, and routing-revision attribution before forwarding a LiteLLM
request.

## Budgets, usage, rotation, and revocation

An application token with `models:read` can request `GET /api/v1/usage`.
Poundlock returns configured policy plus the safe usage fields that LiteLLM
reports, such as spend, maximum budget, reset time, expiry, rate limits, and
blocked state. The response includes the calculated remaining amount, source,
and observation time when fresh data is available. An unavailable usage lookup
is reported as unavailable rather than estimated. No key material is included.

Before a client begins work that may use an external profile, it can call:

```http
POST /api/v1/access/preflight
Authorization: Bearer APPLICATION_TOKEN
Content-Type: application/json

{"model":"review-external","operation":"inference"}
```

Use `operation: "job"` for a durable profile; omission defaults to
`inference` for compatibility. The response reports `availability`,
`blocked_reason`, `connection_mode`, destination, and this application's
matching usage state. Preflight does not run inference or expose another
application's usage.

**Rotate client keys** replaces active keys for one gateway. Rotation never
returns plaintext keys to the browser. Revoking a Poundlock application token
blocks it immediately at the controller, stops its inference streams within a
few seconds, requests cancellation of every job it queued or is running, and
schedules its LiteLLM keys for blocking. If the gateway is unavailable, the pending revocation remains visible
and the controller retries it. Use **Retry pending revocations** in Access after
repairing the gateway for an immediate retry.

Changing or removing a managed endpoint first blocks its active client keys. If
LiteLLM cannot confirm that action, Poundlock leaves the endpoint unchanged so
the operator can recover safely.

When rotating the LiteLLM administrative credential, update Poundlock while the
old credential still works. Poundlock uses the saved credential to block its
existing client keys before accepting the replacement. If the old credential
was already disabled, restore it temporarily before retrying the endpoint
change.

## Routing changes and client approvals

### Add or update a model without losing the handoff

In **Models → Discover**, choose a catalog model and **Add route in LiteLLM**.
The guide keeps the upstream model ID and the gateway alias available to copy.
Choose the gateway explicitly when there is more than one. To upgrade an alias
clients already use, select its existing route rather than creating another one.
The link opens LiteLLM's **Models + Endpoints** screen, or the selected model
group. It never sends a provider credential in the URL and does not create a route.

Save the route in LiteLLM with its existing provider credential, then return and
choose **Check saved route**. That reads the selected gateway and requires both
the alias and upstream model to match. A missing or mismatched route remains
unpublished. A new route still needs **Publish** with an explicit destination.

Discovery flags a published profile when the gateway now serves a different
model. **Refresh profile from gateway** requires confirmation, updates its served
identity and price, changes its routing revision and clears reasoning verification
from the old model. It does not change the alias, display label, destination,
context/output limits or uses. Review those separately and verify only reasoning
settings supported by the new model. Route checking and refreshing send no model
requests; **Verify reasoning** does send small paid probes.

For LiteLLM versions without the current `/ui/models-and-endpoints/` route, use
its own navigation to reach Models. The guide's copied IDs and read-back check
still apply. Dismiss the guide to abandon it; no rollback is needed because it
never writes a gateway route or publishes a profile automatically.

Every saved profile advertises `poundlock.routing_revision`. Poundlock changes
that value when the profile, endpoint, credential, destination, node, runtime,
or managed-identity policy is resaved. Clients can use it to invalidate a prior
approval before sending data through a changed route.

Poundlock cannot observe a route changed only inside LiteLLM. After such a
change, recheck the route and resave its Poundlock profile. This deliberately
changes the routing revision. Keep the destination `unknown` until the new route
is verified.

For acceptance, first use a disposable OpenAI-compatible provider to verify
publication, authentication, request forwarding, attribution, and errors
without a bill. A real external-provider test must separately record the
authorized cost limit, route, provider, tools request, fallback behavior, and
outcome. Keep that evidence and all credentials outside the product repository.

For a small evaluation install, see the optional
[LiteLLM and PostgreSQL Compose example](../../examples/litellm/README.md). An
existing managed LiteLLM deployment remains the primary integration path.
