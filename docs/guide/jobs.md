# Submit a durable job

Open the controller, connect with an operator token or an application token with
`jobs:read` and `jobs:submit`, and use **Jobs**. Add `jobs:cancel` when cancellation
is needed. A token sees, cancels and downloads only the jobs it submitted; jobs
still share one queue and the same runners, so this is not tenant isolation.
Application tokens cannot configure the controller or select registered server
repositories.

Describe the requested work, select a model, and choose starting files:

- **Upload a folder** captures the selected files, including uncommitted edits.
  Review included files before submitting. Git metadata is excluded. Common
  dependency/build directories and `.env`, `.pem` and `.key` files start unchecked;
  this is a convenience filter, not a secret scanner. Include any required files
  deliberately. Mark executable files in the review because browsers do not
  preserve Unix permissions. Symlinks are not supported by this upload format.
- **Read-only Git checkout** fetches an HTTPS repository at a full 40-character
  commit ID. The operator approves repository hosts; the default is `github.com`
  and `gitlab.com`. For private repositories supply a Git username and a token
  restricted to repository read access. The token is encrypted with the source
  request and supplied only to Git fetch, never to the agent workspace. It is
  deleted with the source when the job ends.
- **Start with an empty workspace** requires no repository and can produce new
  files or a text answer.
- **Advanced: server repository** uses repositories already registered on the
  controller. This option requires an operator token.

Snapshots support up to 10,000 files and 50 MiB of decoded content. The complete
snapshot is encrypted in the controller database until the job ends: completing,
failing or cancelling a job deletes its source. The prompt, results and events
are kept. Returned patches are relative to the uploaded files, including any
uncommitted edits, rather than your repository's HEAD.

Once the job appears in the queue, you can close the page and return later.
If submission fails with a connection error, retry without editing the form to
avoid creating a duplicate. If you edit or reload the form, check the queue
before resubmitting. Review output, patches and verification evidence.
Completion alone does not mean independently verified.

## API

Send an authenticated `GET /api/v1/job-options` for available durable models,
harnesses, permissions, source limits, the advertised verifier toolchain
(`python`, `uv`, `writable_exec_paths`), `effective_budgets`, `policy_limits`
and `active_jobs_per_token`. `contract_revision`
is 5. Job-options lists only profiles whose `uses` include `durable` (a profile may
serve several uses); interactive and completion profiles stay on `GET /v1/models`,
though a profile with both `durable` and `interactive` uses appears in both places.
Each durable row includes `endpoint_kind`, `routing_revision`, and the same
`busy` / `occupancy` view as advertised models. Testing an endpoint does not
create these rows; save a model profile first. Submit JSON to `POST /api/v1/jobs`:

```json
{
  "idempotency_key": "unique-request-id",
  "model_profile_id": "MODEL_ID",
  "harness_profile_id": "pi",
  "prompt": "Update greeting.txt to greet the reader by name.",
  "source": {
    "kind": "snapshot",
    "files": [{"path": "greeting.txt", "content_base64": "SGVsbG8hCg==", "executable": false}]
  }
}
```

Use `"source":{"kind":"empty"}` for a new workspace. Source and legacy
`project_id` are mutually exclusive. Paths must be relative regular files without
traversal, Git metadata, case aliases or file/directory collisions.

Poll `GET /api/v1/jobs/JOB_ID`; fetch ordered events at
`GET /api/v1/jobs/JOB_ID/events?after=SEQUENCE`. Results include patches, output,
provenance and cleanup evidence. `GET /api/v1/jobs` and the operator overview
return recent job summaries without those bodies. Cancel with
`POST /api/v1/jobs/JOB_ID/cancel`.
Replay the exact submission with its original key after a transport failure;
a different request with that key returns HTTP 409.

An application token may have 10 unfinished jobs at once; the operator sets this
with `POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN`, and job-options reports it as
`active_jobs_per_token`. A job is unfinished until it is completed, failed or
cancelled, including while a cancellation is pending. Past the limit,
submission returns HTTP 429 with code `too_many_active_jobs`; wait for a job to
finish or cancel one. Replaying an existing job's key still returns that job.
Operator submissions are not limited.

Jobs execute in separate containers. Patch collection runs after agent processes
are removed and uses original Git metadata. Review patches before applying them.

## Checks, permissions and repair

Open **Checks and permissions (optional)** in the job form. Supply shell commands
one per line. Prefer `uv sync` / `uv run` for Python (the worker image has uv and
CPython 3.14 on `PATH`; `/tmp`, `/work`, and `/home/worker` are executable). Node
checks can use `npm ci` then `npm test`. Commands run in order in a fresh copy
reconstructed from the original source and the proposed patch. Test writes and
build products cannot alter the downloadable
patch. Python 3, Python venv, Node.js, npm, Git, curl and ripgrep are installed;
other toolchains and custom worker images are not part of this release.

Use exact workspace-relative files or directories under **Protected files or
directories** to keep submitted tests unchanged. A directory protects additions,
deletions, contents, executable permissions and symlink targets beneath it in
`strict` mode (the default; see below for `existing_only`). These are paths, not
glob patterns. Every protected path must exist in the original source. Modified
protected files fail the gate before commands execute, including a change that
names a protected path with different letter case or Unicode normalization,
which a Mac applying the patch would write into the protected path. A changed
`.gitattributes` is listed in the report's `flagged_paths` for review, because it
changes how Git applies and checks out files on the receiving machine. Protection alone leaves
the result `unverified`; passing independent commands records `passed`. A passing
command is evidence to review, not automatic permission to merge.

The API accepts this optional `policy` on submission:

```json
{
  "verification": {
    "commands": [
      {"command": "python3 -m unittest discover -s tests", "cwd": "", "timeout_seconds": 300}
    ],
    "protected_paths": ["tests"]
  },
  "network": "inference_only",
  "max_repairs": 1,
  "dependency_freshness": false
}
```

`cwd` and protected paths are relative to the agent's starting folder. For an
advanced job with multiple server repositories, that folder contains the named
repository directories. Paths cannot escape it or refer to Git metadata. The UI
uses one working directory and timeout for all commands; the API can specify each.
Limits are 8 commands, 64 protected paths, 32 KiB total policy and 1–600 seconds per
command. Omitted commands default to a 300-second timeout.

Each protected path is either a bare string (`strict` mode, protecting the path
against every addition, deletion and modification beneath it — Poundlock's
original and still-default behaviour) or an object naming an explicit mode:

```json
{"path": "evaluation/bootstrap", "mode": "existing_only"}
```

`existing_only` protects only entries that already existed in the original
source against modification or deletion; new entries an agent creates beneath
the path (a virtualenv, a build cache) do not fail verification. Use it when a
directory needs to stay writable for the agent's own tooling but its existing
contents must not change. `strict` remains the right choice when nothing new
should appear there at all. This is an additive extension of the wire format:
existing bare-string policies keep working unchanged.

Set `dependency_freshness: true` to append a fixed instruction to the job
prompt telling the agent to resolve current package versions from the
registry (`npm view`, `pip index versions`, `cargo search`, and similar)
instead of a remembered, possibly stale, version literal. Default false keeps
today's behaviour. This matters most for outbound-network jobs that add or
upgrade dependencies; Pi can and will check the registry when told to, but
does not by default.

By default the agent can contact only its assigned model, and verification has no
network. Operators may enable `POUNDLOCK_ALLOW_JOB_NETWORK=true` in the controller
configuration. Each job must still explicitly request `network: "outbound"`.
The agent and checks then reach the internet through a proxy in the supervisor:
`HTTP_PROXY` and `HTTPS_PROXY` are set for them, and npm, pip, uv, git and curl use
it. It connects only to public addresses on ports 80 and 443, so package
registries work but the operator's local network, the host and Poundlock's own
services do not. It is not a domain allowlist. Workers still
have no host files, Docker access or supervisor credentials. Source checkout uses
the separate operator-approved Git host list and repository-read credential.

The check container can execute binaries written to `/tmp` or `/work`. Use the
snapshot’s lockfile rather than assuming image-baked test tools. Example:

```text
uv sync --frozen
uv run pytest
```

`python3 -m venv .venv && .venv/bin/pip install -r requirements.txt` still works
on the image’s Debian 3.11 when a job is not using 3.14. Prefer `uv run` for
projects that pin CPython 3.14; that interpreter has no pip of its own, though a venv made
with `python3.14 -m venv` gets one. For Node use `npm ci`, then `npm test`. Turn on
outbound access when those commands need downloads. Agent-installed dependencies
excluded from the patch are absent in verification. Commands share verification
files, but each command's processes are removed before the next starts.

Repairs default to 0 and support 0–3. Only failed commands or changed protected
files trigger repair. A fresh Pi process receives the original task, fixed policy,
candidate files and recorded failures. Each repair uses the same model and the
remaining original job deadline. Cancellation, lease loss, missing commands,
verification timeouts, output limits, infrastructure errors and supervisor
restarts stop execution; none automatically restart the agent. Shell exits 126/127
are treated as unavailable commands. Exhausted repairs return a failed job with
all retained attempts. No files are applied to your original repository.

The result starts with the outcome, checks and final patch. Earlier attempts,
command output, agent logs, provenance and cleanup remain available in expandable
sections. Provenance's `model` field remains the routing alias, never the
resolved backend; `serving_identity`, when known, adds the operator's
`model_label` and the node's `application`/`build`/`commit`/`model` for that
runtime. `node_reported_at` says where that came from: `model_ready` means the
node probed the runtime right after preparing this job's model; `inventory`
means its periodic report, used only when that report was taken after this
job's model was ready. When neither applies, `node_reported` is absent and
`node_reported_unavailable` says why — an older report would name the model a
previous job loaded. **Download patch** returns the exact diff, including for failed or
cancelled jobs. API clients use `GET /api/v1/jobs/JOB_ID/patches/0`, optionally
`?attempt=1` for a prior attempt (one-based). Root indices are zero-based; download
each root separately. The endpoint requires `jobs:read` and sends no-store headers.
Apply against the recorded base revision or original uploaded snapshot; run
`git apply --check change.patch` before `git apply change.patch`.

Patches share a 10 MiB budget divided evenly among the maximum number of attempts.
Exceeding it fails with `{"layer":"resources","code":"patch_limit"}` (or, when
the collector itself refuses to keep enlarging one root's diff, a
`patch_failed`/`patch_collection_error` from the same check) whose message
names the largest changed paths and their byte counts — a few entries, sorted
by size, never file contents — so "the agent installed a database into the
workspace" is visible directly instead of inferred from a total size alone.
Agent output has a 4 MiB job budget, split equally between streams and the
maximum number of attempts; jobs without repairs retain the previous 2 MiB per
stream. The agent's own stdout has its Pi `message_update` streaming-delta
events (superseded by each turn's `message_end`) dropped before they count
against that budget; a trailing line reports how many were omitted. Command
output retains 16 KiB per stream. Exceeding a limit fails visibly. Verification source copying and
protected-file comparison are bounded at 256 MiB and 50,000 entries. Interrupted
checks retain the candidate checkpoint; a supervisor restart reports interruption
and cleans up, rather than rerunning the job.

`GET /api/v1/job-options` also reports `policy_limits` — the bounds a submitted
policy must satisfy (max repairs, verification commands, protected paths, policy
bytes, and the verification-timeout range and default), read from the same
constants that enforce them so they cannot drift apart. They were previously
discoverable only by submitting a job and reading the 400.

`GET /api/v1/job-options` and the submit response report `effective_budgets`: for
each possible `max_repairs` (0–3), the real `attempts`, `output_bytes_per_stream`
and `patch_bytes` a job with that setting gets, computed from the same values the
runner enforces rather than a second, duplicated table — so raising `max_repairs`
from 0 no longer silently halves the usable output budget without warning. Wall
time is still not part of `effective_budgets`: it is configured per runner process
(`POUNDLOCK_JOB_WALL_SECONDS`, default 3600), so different runners can enforce
different wall budgets for jobs that otherwise look identical, and it is genuinely
unknown before a specific runner claims the job — job-options is asked before that
happens, so it would have to guess. Once a runner does claim the job, it reports
its own configured `wall_seconds` on the claim, the controller stores it on that
job and computes an absolute `deadline_at` anchored on the claim timestamp, and
both fields appear on the submit response and `GET /api/v1/jobs/JOB_ID` — `null`
until claimed, exactly like any other value this API would otherwise have to
invent. `deadline_at` is the controller's view of the deadline, computed once at
claim time; it does not move afterward, including through the grace window
described below.

Hitting the output budget or the wall-time deadline mid-run does not stop the
agent process itself — only the retained capture is cut, or the job is marked
failed while the process may still be exiting. The two cutoffs then differ,
because one of them still has time left and the other does not.

An **output** cutoff always verifies the retained patch. The wall clock still
bounds the run, so the checks cost seconds against work already done, and a
patch handed back unchecked only moves that work to you. The error code still
distinguishes `output_limit_after_turn` from `output_limit_mid_turn` — it
describes how the run ended, not whether the patch was checked — and
`verification.checked_after` names the cutoff the checks ran after, so
`verification.status: "passed"` can never be read as "the agent finished the
task". Verification is not gated on the turn having settled here, because it
cannot be: the capture keeps the beginning of the stream and drops the tail, so
when the agent's own stdout overflows, the terminal message proving the turn
settled is exactly what was discarded.

A **wall** cutoff still verifies only when the agent's final turn had already
settled, in a bounded window after the deadline sized from the job's own
declared verification-command timeouts (this does not change the reported
`deadline_at`). There the deadline has passed, so running checks for a turn
still in flight would extend the job on no evidence.

Only the agent's own stdout overflowing produces an `output_limit_*` code; a
noisy dependency filling stderr does not cut the transcript and is reported
separately. `provenance.execution.output_bytes` records what the attempt spent
against the limit, including how many streaming events the capture filtered, so
a task can be sized against the budget that actually binds. The agent's prompt
carries both budgets — remaining wall-clock seconds and the per-stream output
limit — so it can wrap up rather than start work it cannot finish.

Patch collection applies a default exclude floor (`__pycache__`, `.venv`,
`node_modules`, build and tool caches) through git's `core.excludesFile`, so
files an agent generated while working are not encoded into the diff as binary
literals. A `.gitignore` in your own snapshot outranks the floor and can
re-include anything with `!`. If the working tree is nonetheless too large to
diff, the failure names the largest paths instead of returning nothing.

## Pinned Git source

```json
{
  "kind": "git",
  "repository": "https://github.com/owner/repository.git",
  "revision": "0123456789abcdef0123456789abcdef01234567",
  "credentials": {"username": "reader", "token": "REPOSITORY_READ_TOKEN"}
}
```

Place this object in `source`; omit `credentials` for public repositories.
Use a repository-scoped, read-only credential. Poundlock only fetches; it cannot
prove that a supplied provider token lacks write permission. Do not give it one.
Supply a full 40-character commit ID; branch names and tags are not accepted.

Operators can set `POUNDLOCK_GIT_HOSTS` to comma-separated exact HTTPS hostnames,
including a self-hosted Gitea hostname; an empty value disables remote Git intake.
No wildcard hosts, arbitrary ports,
embedded URL credentials, redirects, SSH, credential helpers or submodule fetches
are used. Adding a host permits the runner to fetch repositories from it.
The UI shows the approved list.

The fetch runs in a short-lived container of its own, without the Docker
socket, that writes straight into the job's volumes and can reach only the
repository's host on port 443, at any address, so a Gitea on your own network
works. The supervisor never reads the fetched Git data.

The working tree limit remains 50 MiB and 10,000 files. Fetch has a five-minute
ceiling and also consumes the attempt wall budget. Symlinks, submodules and LFS
pointers are rejected; upload resolved regular files instead. Git object
downloads can exceed working-tree size; keep the runner's work-volume ceiling.
The same per-job container isolation applies to Git and uploaded sources.

The fetch container uses standard [Git fetch](https://git-scm.com/docs/git-fetch)
and [command-scoped configuration](https://git-scm.com/docs/git-config).
Checkout credentials are kept out of command arguments and persistent Git
configuration, and remote fetch stderr is withheld to prevent credential echoes.

## Inference access during a job

Model calls pass through the controller to the configured inference endpoint.
The runner must be able to reach the controller; it does not need direct access
to a gateway or model server. Each job receives an inference token instead of
the upstream API key.

That token grants access only to the accepted model, within its output limit,
with one request at a time. Cancellation, lease expiry, terminal completion or a
replacement token revokes it. It cannot access job source, other jobs, runner
control routes, or operator settings. Verification workers receive no inference
credential. Source and commands remain inside the job containers.
