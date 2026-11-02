# Changelog

## 0.4.0 (2 November 2026)

Tested with Toolkit 0.5.0 and Dispatch 0.3.1.

- The console and `docs/linux-node.md` now give the same recovery from a node's authentication
  failure, the one that works: move the old credential aside, then enroll again. The console's old
  steps left the node refused and could break a machine whose credential was fine.
- The console, the node installer and one API error use today's names for models, application keys
  and Test inference.
- A console field that fails validation is marked `aria-invalid` until it is corrected, so a
  screen reader says so too.
- When a proxy answers with a non-JSON error, the console says it can't reach the controller and
  gives the status, not "Invalid server response".
- The Linux node guide is linked from the machine setup guide and included in the release bundle.
- The controller image runs on Debian 13. Debian 12 left Critical findings in zlib and sqlite
  that it will not fix. The builder stays on Debian 12, so the Linux node binary still needs only
  glibc 2.36.
- With the model drawer open, a console error shows inside it, above the footer: it covers no
  field or button, and a keyboard can reach Dismiss.
- Pi's minimatch is overridden to 11.0.1, which closes GHSA-g3hv-75wg-m249.
- Verify reasoning no longer drops a level just because an adaptive model didn't think about
  its short probe. A level the upstream accepted, below one that reasoned on the same route,
  now counts as verified: the route carries the setting. Listing only the levels that reasoned
  had made Poundlock refuse low and medium for every client of Claude Sonnet 5.5.
- A main build is a dev prerelease, `0.4.0-dev.<commit time>` (`scripts/dev_version.py`),
  so nodes, which install only a higher version, take every main deploy instead of
  waiting for the next release. Tags and branch builds keep Cargo.toml's version,
  which on main names the latest published release, 0.3.0.
- A node action whose poll reply was cut, as by a proxy restart, goes out again on the
  node's next poll instead of after 11 minutes. A lost release had kept a machine's
  slot held for its lease's five minutes, refusing every new request for that model.
  Nodes now log the actions that fail, and capabilities carry the caller's own
  permissions (`caller_permissions`).
- From the console re-check: agent output renders even with the runner's own notes in
  it, the OpenRouter card's output default follows the context until one is typed, the
  job model is a deliberate choice, Add and test warns before a load, refreshes keep
  focus and scroll, the job detail has Close and a `#jobs/<id>` link, the header shows
  the build commit, and macOS nodes report GPU memory in bytes.
- Access can edit a token's name and permissions (`PATCH /api/v1/api-tokens/{id}`)
  without replacing its secret or losing its jobs. The controller publishes each
  permission's label in capabilities (`permission_labels`, `token_permissions`) and
  the operator overview, and Access shows those labels instead of codes. Any active
  application token may now read capabilities, whatever its permissions. An empty
  permission list says to choose one.
- A new token's secret shows even when the refresh after creating it fails, and
  Cancel job asks first and reports a refused cancel.
- The Add-model drawer no longer changes its server: listing a server's models uses
  `GET /api/v1/endpoints/{id}/models`, which records no health check, and declaring an
  undeclared server's destination goes with the model in one change
  (`declare_server`), so a refused model leaves the server as it was.
- Removing a gateway that model profiles still use is refused before its client
  keys are revoked, not after.
- The job list shows each finished job's check result, the job form offers only
  models that can run jobs, and Access refreshes when opened. Poundlock's own check
  token is marked Internal, with nothing to revoke.
- The model checks are named for what they do, Check listing and Test inference, and
  Test inference asks before it may wake a machine and replace its loaded model.
- The console keeps its token for the browser tab unless Remember on this device is
  ticked, drops a saved token that stops working with a message, and keeps error
  messages up until they are clicked away.
- Refreshes and polls no longer rebuild controls under the operator: an open dropdown
  stays open, a running job's detail keeps what was expanded, and the OpenRouter card
  keeps focus while typing. A catalog model with no listed context is left for the
  operator to fill in rather than given 131072.
- Less clutter at production scale: revoked tokens are hidden until asked for, job
  titles drop Markdown heading marks and a full job list says it shows the newest
  only (`jobs_limit` in the overview and job options), machines group system and
  virtual network links, and In use names each profile once.
- Fewer contradictions: a test older than a day reads as history, machines without
  Ollama drop the Ollama memory panel, a server its runtime reports reads as running,
  and Discover shows a published route's saved price as its model card does.
- One name per concept: application and administrator tokens, application keys for
  LiteLLM's per-application identities, servers and gateways, and the node version.
  Secret fields, catalog badges and the Models view toggle are named for assistive
  technology, and Latte's focus ring meets 3:1.
- A job's agent output reads as what the agent said and did, with failed tool calls
  marked; Pi's raw event stream is one click away.
- A nightly database backup (`POUNDLOCK_BACKUP_DIR`, `POUNDLOCK_BACKUP_AT`): a
  consistent `VACUUM INTO` copy taken while the controller runs, checked, gzipped
  with a checksum, kept for 14 copies plus each month's first for six months, and
  reported in `last-backup.json`. `master-key` is never copied. `poundlock backup
  --to DIR` makes one copy by hand. In a Compose install, `compose.backup.yaml`
  mounts the folder named by `POUNDLOCK_BACKUP_PATH` and turns it on.
- `compose.yaml` now forwards `POUNDLOCK_GIT_HOSTS`,
  `POUNDLOCK_CAPACITY_DEFAULT_WAIT_SECONDS` and `POUNDLOCK_NODE_OFFLINE_SECONDS`
  from `.env`; before, setting them there had no effect.
- `GET /api/v1/harness-profiles` is gone. Nothing called it; the harness list is
  in `/api/v1/overview` and `/api/v1/job-options`.
- The ACP agent names itself Poundlock, not the pre-release Smart Local AI.
- `poundlock node` no longer takes `--enrollment-token`. Enroll with
  `node-enroll`, which reads the token from standard input; a container can still
  set `POUNDLOCK_ENROLLMENT_TOKEN` for its first start.
- A job's model requests check `reasoning_effort` as the relay does: `xhigh` is
  accepted, a profile with verified levels accepts only those, and a refusal has
  the code `unsupported_reasoning_effort`.
- The image build no longer signs node binaries from a build secret. Sign after
  each deploy with `scripts/sign-node-updates.py`, as since 0.2.0.
- A 0.1.x database is no longer upgraded: the 0.1.x schema changes and the
  one-time secret re-seal are gone. Start 0.2.0 or 0.2.1 on such a database first.
- The subcommands that drive a controller (`status`, `endpoint-add`, `model-add`,
  `project-add`, `node-token`, `node-wol`, `node-config-example` and `job-*`)
  share one HTTP client and say plainly what went wrong: an unreachable
  controller, a refused token or the controller's own message. They take
  `--token-file`. `node-config-example` prints the controller's example, not the
  local binary's; `node-token` takes `--node-name` and the controller's default
  validity; `endpoint-add` takes `--public-url` and `--data-destination`, and
  reads its key from `POUNDLOCK_ENDPOINT_API_KEY`, no longer `OPENAI_API_KEY`.
- `POST /api/v1/model-profiles` fills what a request leaves out (name, context,
  output limit, uses, wake policy, thinking and label) from the draft check the
  console shows, and `model-add` sends only what it is given, so the two cannot
  disagree. A request with neither `uses` nor `workload_class` now gets agent jobs
  and interactive requests instead of a 400. The console asks the draft check for
  a newly chosen machine's wake policy rather than keeping its own copy of the rule.

## 0.3.0 (19 October 2026)

The first public release. Tested with Toolkit 0.5.0 and Dispatch 0.3.0.

- Models > Discover reads OpenRouter's public catalog only when an operator chooses
  **Look up**. `POUNDLOCK_OPENROUTER_CATALOG` is `on-request` (default), `always`
  or `never`; set `always` to keep the old behaviour.
- `scripts/toolkit-capabilities.py` makes capability evidence from Toolkit 0.5.0's
  `recommend --format json` summary instead of importing Toolkit. It records a run
  that sent nothing about thinking as the model's default and an explicit off as
  off, and refuses an explicit effort level a profile cannot express. The old
  sibling-checkout contract check is gone.
- Upstream error messages no longer pass an echoed `sk-` key or a `key=`/`token=`
  value through to applications.
- Release node binaries and images are unsigned: nodes trust the controller's
  checksum over HTTPS or loopback. Self-builders can still embed their own key and
  sign; see docs/node-update-signing.md.
- macOS node binaries come only from native macOS builds; the Linux image no longer
  cross-compiles them against a third-party macOS SDK.
- `LICENSE` is the unmodified Apache-2.0 text; `NOTICE` added. Archives, the
  Compose bundle and the image carry the full texts for Rust crates, Pi 1.1.0 and
  the fonts and palette compiled into the binary.
- GitHub CI runs every check on pull requests, forks included, and a version tag
  publishes only after maintainer approval. `tests/e2e/run.py` exercises the
  Compose install end to end with a mock model server, and nightly with a tiny
  llama.cpp model.

- A nightly database backup (`POUNDLOCK_BACKUP_DIR`, `POUNDLOCK_BACKUP_AT`): a
  consistent `VACUUM INTO` copy taken while the controller runs, checked, gzipped
  with a checksum, kept for 14 copies plus each month's first for six months, and
  reported in `last-backup.json`. `master-key` is never copied. `poundlock backup
  --to DIR` makes one copy by hand.

- A request for a busy exclusive model can wait in line instead of an
  immediate 409 (contract revision 5, `capacity_queue`). `Poundlock-Wait`
  holds it up to 110 s; `Poundlock-Priority` orders the line (`owner` and
  `planner` need the token's new `inference:priority` permission); a session
  that just let go of the slot comes back first for 10 s; a waiter rises one
  class a minute. A wait that runs out answers 409 with the request's place and
  a ticket that keeps it for 60 s. Durable jobs wait in the same line, so a
  burst of requests no longer starves them. `GET /v1/capacity` shows each line.
  `POUNDLOCK_CAPACITY_DEFAULT_WAIT_SECONDS` lets clients that send no header
  wait too; it defaults to 110 s (0 restores answering at once).
- A chat app that cannot send a queue ticket, such as Open WebUI, no longer
  hears "This model is busy": a streamed request without `Poundlock-Wait` or a
  ticket gets its event stream at once and waits on it for up to 10 minutes,
  an SSE comment every 15 s keeping the connection open, then streams its
  answer. A token with `inference:priority` that names no priority waits as
  the owner, so its chats go next after the request in flight.

## 0.2.1 (October 2026)

- Gateway model discovery guides operators to LiteLLM's model screen, keeps the
  selected upstream ID and alias visible, checks the saved route and flags
  published profiles whose served model changed. Explicit refresh clears old
  reasoning verification without changing labels, destinations or limits.
- rustls 0.23.46 replaces 0.23.45 for the four upstream security advisories
  addressed in that patch release; no advisory gate is suppressed.
- Jobs run on Pi 1.1.0, with MCP turned off (`--no-mcp`).
- A job's retained output keeps one copy of each tool result. Pi repeats each one in up to
  five events, so a few large file reads used to fill the output budget
  (`output_limit_mid_turn`). The copy kept is `tool_execution_end`;
  `provenance.execution.output_bytes.compacted_events` counts the events trimmed.
- The llama.cpp runtime reports Poundlock's version.
- quinn-proto 0.11.19 in the lockfile (RUSTSEC-2026-0338, 0340, 0343; reqwest does not
  compile it). CI scanners: Trivy 0.75.0, osv-scanner 2.7.0, Semgrep 1.180.0; refreshed
  Node base image.
- The llama.cpp runtime no longer reports "exited while loading" for a server that is
  still starting. On a cold page cache the first check could see the new process before
  its command line was set.

## 0.2.0 (October 2026)

The first release for daily use. API major 1, contract revision 4 (unchanged).
Upgrade notes are in [Operations → Upgrade and rollback](docs/guide/operations.md#upgrade-and-rollback).

### Security

- **Git sources are fetched in a container of their own** (L13), without the Docker
  socket, straight into the job's volumes; it reaches only the repository's host on
  port 443, so a Gitea on the operator's network still works.
- **Stored secrets are bound to their row** (L7). The first start re-seals older ones,
  after saving the database beside itself as `poundlock.sqlite3.pre-secret-binding`
  for a rollback.
- **A limit on requests in flight** (L4): over `POUNDLOCK_MAX_REQUESTS_IN_FLIGHT`
  (512) the controller answers 503 `controller_busy` at once.
- **Enrollment tokens are bound to a machine name** (L8); only a token made for an
  existing machine's name can re-enroll it.
- **Ollama is off on a node unless `node.toml` enables it** (L10).
- **Linux nodes run as their own system account** (L11), with no home access and
  systemd hardening; `poundlock-node.sh account` moves an existing node. Enrollment
  asks for the token without echo instead of taking it on the command line.
- **Runner capacity is sized to the host's memory** (L15), and the runner's model
  proxy refuses requests over 64 in flight.
- **Job containers can run under a chosen Docker runtime** (L16), such as gVisor's
  `runsc`, with `POUNDLOCK_WORKER_RUNTIME`.
- **Node update signatures are made on the signer's machine** and uploaded with
  `scripts/sign-node-updates.py`; CI no longer needs the private key.

### Changed

- Pi 1.0.3, installed in the image from a committed lockfile
  (`pi-runtime/package-lock.json`).
- The llama.cpp node runtime documents that it drives llama.cpp forks with the same
  `llama-server` interface.

## 0.1.3 (5 October 2026)

Pi 1.0.0. Durable jobs reach only public addresses through the supervisor's egress
proxy (M5); job sources are deleted when jobs end and each token may hold 10
unfinished jobs (M2); the llama.cpp runtime takes an API key file (M4); signed node
updates; streaming and authentication fixes from the 30 September security review.

## 0.1.2

llama.cpp reasoning levels and thinking budgets, the model setup flow, the generic
llama.cpp node runtime, contract revision 4.

## 0.1.1

The first internal release: the controller, nodes, the relay, durable jobs and the
operator console.
