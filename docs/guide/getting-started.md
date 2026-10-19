# From hardware to your first project

Poundlock Toolkit measures a machine and its model setup. Poundlock makes the
prepared models available through one authenticated service. Poundlock Dispatch
provides a project manager that plans, delegates work and helps you review results.
You can also use Poundlock directly from its browser or API.

## 1. Qualify a model before installing Poundlock

Install Poundlock Toolkit (`pip install poundlock-toolkit`; source at
[github.com/endthestart/poundlock-toolkit](https://github.com/endthestart/poundlock-toolkit))
and follow its **README**. Toolkit runs locally or
uses an explicitly configured SSH target; it does not require a Poundlock server.
Use its installation guidance for your operating system, GPU and chosen model
server. Drivers, runtime packages and model weights remain operator-managed.

Retain the report and its evidence identity privately. Check the actual runtime,
model revision/quantization, context, output limit, reasoning settings and harness
version tested. A successful connection or a GPU's memory capacity does not prove
coding quality. An endpoint-only check does not establish machine setup or wake
support. Recheck the exact combination after changing it.

Poundlock's bundled coding harness is Pi **1.1.0**. Evidence from another Pi
version is useful comparison evidence, not qualification of this combination.
Use the current Dispatch release's documented harness version for attached work.

## 2. Install the controller

Use a Linux x86_64 machine with Docker Engine 28+ and Docker Compose. Follow the
[download, checksum and first-start instructions](../../README.md#install).
Keep the controller's first-run administrator token private. For a remote server,
use HTTPS or an SSH tunnel; the default listener is local to the Docker host.

The Compose bundle pins the tested image. Docker Compose is sufficient; a
container-management UI is optional. Keep hostnames, credentials, overrides and
backups outside the source checkout. The supervisor needs Docker API access and
is a trusted component. Its agent containers do not receive that access.

## 3. Enroll machines and choose model ownership

An always-running model server can be added as an endpoint without enrolling its
host. Enroll a node when Poundlock should manage its runtime or wake it. Follow
**Machines → Linux installation guide**, or the [machine guide](machines.md) for
other supported setups. Use the release asset for the machine's operating system
and architecture, and verify its checksum before installation.

Enrollment reports hardware and interfaces. Choose the node's connection
explicitly if system routing is unsuitable. The model server must also listen on
an address reachable from the controller; selecting the node's interface does
not change the model server's listener or the host's routing table.

Configure the wake-capable NIC separately. It may differ from the data NIC.
Enable magic-packet wake in the operating system and firmware, then test from
sleep without first opening SSH. A successful packet send alone does not prove
the machine woke. See [wake setup and diagnosis](machines.md).

For an existing runtime, choose the always-ready path. For supported managed
Ollama loading, configure the node's Ollama adapter and select it in the profile.
**Unload after idle seconds = 0** releases owned models after work; a longer
interval keeps them warm between requests. Poundlock does not take ownership of
models another application already loaded. Custom runtime adapters are advanced
node-local configuration; Toolkit measurements do not install them automatically.

## 4. Map the Toolkit result into Connections and Models

This release uses a manual handoff. Keep the report identity with your private
deployment record; there is no report importer or automatic tier assignment.

| Qualified setting | Poundlock configuration |
|---|---|
| Runtime API address and authentication | Connections: endpoint type, API base URL and upstream key |
| Exact server model ID | **Server model ID** in Models, picked from what the server or machine reports; clients send the model name instead |
| Tested context and output limits | Profile context window and maximum output tokens; match the runtime too |
| Machine and runtime ownership | Enrolled node and its configured runtime profile, when managed |
| Network connection and wake NIC | Machines settings, separately from the endpoint's address |
| Desired model retention | Managed profile's unload-after-idle setting |
| Inference destination, including gateway fallback | Declared once per model server in Connections; per route for a gateway |
| Model/harness quality evidence | Capability evidence in Models, made with `scripts/toolkit-capabilities.py`; keep the result files in a private record |

Pi coding needs at least 8192 context tokens. In **Models**, **Add model**
publishes it; use **Generate** for an actual answer. Listing a server's model
IDs and testing a connection publish nothing. Repeat a warm request if diagnosing
cold-start latency; do not silently switch models after a failure.

Destination is `operator_network`, `external` or `unknown`. It describes the
operator's confirmed route, not what a model name or gateway type suggests.
Discovery metadata is advisory: a profile can become busy after it is listed.
See [model discovery and capacity behavior](integrations.md).

## 5. Connect Dispatch

Get the matching [Poundlock Dispatch](https://github.com/endthestart/poundlock-dispatch)
release and follow its README. In Poundlock **Access**, create one named
application token with model-list and inference permissions, plus job read, submit
and cancel permissions if you will also submit durable jobs. Keep the
administrator token and upstream provider keys out of Dispatch.

Enter the controller address and the application token in Dispatch, test the
connection, then bind its manager, worker and optional escalation roles to saved
Poundlock profiles. Confirm the external-disclosure and spending policy before
binding any External profile.

Dispatch runs work in three ways, and they differ in what keeps running:

- **Attached work**, the default: the manager plans, workers edit your project
  folder on your machine through Poundlock's inference API, and you review the
  result. It stops when you close Dispatch.
- **Autopilot** (preview): a detached runner on the same machine keeps
  working from an agreed plan in a private worktree after you close the window.
  It needs the matching terminal client installed and that machine awake.
- **Durable jobs**: submitted by hand (`dispatch jobs`, Poundlock's **Jobs** page
  or the API), they run on the Poundlock server in restricted containers and
  survive every client closing. Dispatch never turns its own tasks into durable
  jobs.

Remote inference is not remote execution: in attached work and Autopilot only the
model runs elsewhere; the code runs on your machine.

Start with a disposable project. Check the patch and verification output, and
revoke the test application token when finished. An unverified result is not
proof that an application works. Follow [job examples](jobs.md) for durable job
sources, pinned Git commits, protected tests and replay-safe submission.

## Optional: use an existing LiteLLM gateway

See the [gateway setup example](litellm.md). Local-only deployments do not need
LiteLLM, a provider account or a gateway database.

## Recovery and maintenance

If discovery is empty, check that profiles were saved and the application token
can list models. A busy response requires waiting or an explicitly permitted
choice; it is not permission to send work elsewhere. If a model cannot prepare,
check the node, runtime and endpoint before resubmitting. Preserve a durable job's
idempotency key when recovering a lost submission acknowledgement.

Use [operations](operations.md) for backed-up upgrades, restoration and removal,
and [machines](machines.md) for node upgrade/removal. Retain the tested image
digest and previous state backup for rollback. Stop or finish active work before
planned maintenance. Remove nodes and revoke their credentials when retiring them.
Each application token sees only its own jobs, but that is not tenant isolation; see [security](../../SECURITY.md).
