# Changelog

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
