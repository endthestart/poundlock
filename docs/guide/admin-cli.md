# Admin CLI

The `poundlock` binary, in the controller image and on every machine with a native
node, can also drive a running controller from a terminal or a script. Each command
is a plain client of the controller's HTTP API: it does only what the API offers any
client, and never opens the database. Output is JSON on standard output, except
where a command below says otherwise. A failure prints the reason on standard error
and exits with status 1.

The commands serve two surfaces, and the split is deliberate:

- **Administration** sets up and runs the controller. These commands use the same
  operator API as the console, with the administrator token: `status`,
  `endpoint-add`, `model-add`, `project-add`, `node-token`, `node-wol` and
  `node-config-example`. Everything the console does goes through that API; the
  commands cover the common setup steps so far.
- **Consumption** uses the controller's services. `job-submit`, `job-watch` and
  `job-cancel` use the job API that Dispatch and other applications use. They take
  the administrator token or an application token with `jobs:submit`, `jobs:read`
  or `jobs:cancel`.

The node commands (`node`, `node-enroll`, `node-doctor`, `node-pause`,
`node-resume`, `node-revoke`, `node-check`) run on an inference machine with its own
credential; see [Inference machines](machines.md). `controller`, `runner`, `backup`
and `wake-relay` are the server's own processes and tools.

## Connecting

Every command takes:

- `--controller`: the controller's address, or `POUNDLOCK_CONTROLLER_URL`. The
  default is `http://127.0.0.1:7331`.
- `--token-file`: a file holding the token. It wins over the two below.
- `--token`, or `POUNDLOCK_TOKEN`. Prefer a file: other users of the machine can
  see a token on the command line.

On the Compose host, run the commands inside the controller container, where the
administrator token is already in a file:

```sh
docker compose exec controller poundlock status --token-file /data/admin-token
```

From another computer, set `--controller` to the address nodes use (see
[remote access](operations.md#remote-access)) and keep the token in a file only you
can read.

Errors say what to check. An unreachable controller names the address and the
setting to fix, a refused token names the token options, and anything else gives
the request, the controller's own message and the HTTP status:

```text
Error: POST /api/v1/model-profiles was refused: endpoint_id does not exist (HTTP 400)
```

A controller older than the command reports that it has no such route.

## Administration

### status

Prints `GET /api/v1/overview`: the version, endpoints, models, harnesses, projects,
machines, application tokens (never their secrets), managed gateway identities,
the last 100 jobs and occupancy.

```sh
poundlock status --token-file /data/admin-token
```

### endpoint-add

Adds a model server or gateway (`POST /api/v1/endpoints`) and prints it with its
`id`.

- `--name`: required.
- `--base-url`: required. The OpenAI-compatible API address, such as
  `http://10.20.0.20:8080/v1`.
- `--kind`: `openai` for a model server (the default) or `litellm` for a gateway.
- `--api-key`, or `POUNDLOCK_ENDPOINT_API_KEY`: the endpoint's key, if it needs
  one. Prefer the variable.
- `--data-destination`: where a model server sends prompts: `unknown`,
  `operator_network` or `external`. Every model on it inherits this. The console
  asks for it; until it is declared, models on the server are flagged. A gateway
  declares it for each route instead.
- `--public-url`: the address operators open for this server, if it has one.

```sh
POUNDLOCK_ENDPOINT_API_KEY=… poundlock endpoint-add --token-file /data/admin-token \
  --name workstation --base-url http://10.20.0.20:8080/v1 \
  --data-destination operator_network
```

Managed application identities for a LiteLLM gateway are set in the console.

### model-add

Adds a model profile (`POST /api/v1/model-profiles`) and prints it.
`--endpoint-id` and `--model-id` (what the server calls the model) are required.
Anything else left out is filled in by the controller exactly as the console's
**Add model** fills it in; [Adding a model](machines.md#adding-a-model) lists the
rules. Only a context that nothing reports has to be given.

- `--name`: what clients send. It cannot change later.
- `--context-tokens`, `--max-output-tokens`.
- `--uses`: `durable` (agent jobs), `interactive` or `completion`. Repeat the flag
  or separate with commas.
- `--node-id` and `--runtime-profile`, together: the machine that runs the model
  and the runtime it reports, such as `llamacpp` or `ollama`.
- `--wake-policy`: `never` (I keep it running), `manual` (start the model, don't
  wake the machine) or `on-demand` (wake the machine and start the model).
- `--thinking`: `model_default` or `off`.
- `--model-label`: a label for people; never used for routing.
- `--alias`: another name clients may send. Repeat for more.
- `--idle-seconds`: Ollama only, how long the model stays loaded after its last
  request (0 to 3600).
- `--data-destination`: a gateway route's own declaration of where it sends
  prompts. A model server's declaration applies to every model on it.

A model on a machine, with every other value from what the machine reports:

```sh
poundlock model-add --token-file /data/admin-token \
  --endpoint-id 6f1c… --model-id qwen3.8-27b-mtp \
  --node-id 2b7e… --runtime-profile llamacpp
```

A model on a server you keep running, which reports no context:

```sh
poundlock model-add --token-file /data/admin-token \
  --endpoint-id 6f1c… --model-id coder --context-tokens 32768
```

To see what would be filled in without saving anything, send the same answers to
`POST /api/v1/model-profiles/draft`. Publish a gateway route from **Models →
Discover**, which checks the route and records what the gateway reports about it.

### project-add

Registers committed repositories on the server for administrator job submissions
(`POST /api/v1/projects`); see [optional registered
repositories](operations.md#optional-registered-repositories).

- `--name`: required.
- `--root`: a repository folder relative to `POUNDLOCK_PROJECTS_PATH`. At least
  one; repeat for more.
- `--instruction-file`: a repository file passed to the model as instructions
  alongside `AGENTS.md`, such as `docs/conventions.md`. Repeat for more.

```sh
poundlock project-add --token-file /data/admin-token --name web --root web
```

### node-token

Creates a one-time enrollment token (`POST /api/v1/nodes/enrollment-tokens`) and
prints it once. Give it to `poundlock node-enroll` on the machine.

- `--node-name`: the machine this token may enroll. Re-enrolling an existing
  machine needs a token for its name.
- `--valid-minutes`: 1 to 1440. The controller's default is 15.

```sh
poundlock node-token --token-file /data/admin-token --node-name orion
```

### node-wol

Sets a machine's Wake-on-LAN address (`PUT /api/v1/nodes/{id}/wol`). The machine's
`id` is in `status`, under `nodes`.

- `--mac`: required. A machine that reports its interfaces accepts only one of
  their addresses.
- `--broadcast`: address and port, default `255.255.255.255:9`. The console
  proposes the wake interface's own broadcast address instead.

```sh
poundlock node-wol --token-file /data/admin-token 2b7e… \
  --mac 02:00:00:00:00:01 --broadcast 10.20.0.255:9
```

### node-config-example

Prints the controller's example `node.toml` (`GET /api/v1/nodes/config-example`),
as TOML rather than JSON.

```sh
poundlock node-config-example --token-file /data/admin-token > node.toml
```

## Consumption

### job-submit

Submits a durable job on a registered project (`POST /api/v1/jobs`) and prints it.
A registered project needs the administrator token. An application sends files or
a Git source through the job API itself; see [Submit a durable job](jobs.md).

- `--project-id`, `--model-profile-id` (a model for agent jobs) and `--prompt`:
  required.
- `--harness-profile-id`: default `pi`.
- `--idempotency-key`: a new UUID by default. Submitting again with the same key
  and input returns the same job instead of a duplicate, which recovers a
  submission whose answer was lost.
- `--watch`: then follow the job as `job-watch` does.

```sh
poundlock job-submit --token-file /data/admin-token \
  --project-id 0d4a… --model-profile-id 9c2e… \
  --prompt "Fix the failing date test" --watch
```

### job-watch

Follows a job: one `kind  message` line per event, every two seconds, then the job
as JSON once it has completed, failed or been cancelled.

```sh
poundlock job-watch --token-file /data/admin-token 5b1f…
```

### job-cancel

Asks the controller to cancel a job (`POST /api/v1/jobs/{id}/cancel`). It prints
`{"cancel_requested": true}` when the request was recorded, and `false` for a job
that had already finished.

```sh
poundlock job-cancel --token-file /data/admin-token 5b1f…
```
