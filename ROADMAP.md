# Poundlock roadmap

This is the plan for the Poundlock family after the first public releases:
Poundlock 0.3.0, Dispatch 0.3.0 and Toolkit 0.5.0. It replaces every earlier
backlog and research queue.

It has three horizons. **Now** is what gets built next, in order. **Next** follows
once Now's items land. **Later** is direction: worth doing, not yet designed. There
are no dates. Items move between horizons as evidence comes in, and an item is done
when its gate passes, not when its code merges.

## What we're building

Your own AI development team, on hardware you already own.

Poundlock turns the machines in your house into one private compute plane. It
wakes them when there's work, serves your models, queues requests fairly and runs
code in isolation. Dispatch, or any agent you trust, plans the work, researches it,
reviews it and tests it. Local models write the code inside sandboxes, behind
checks that can't be talked past. Frontier models are used where judgement matters
most, and only with your consent and within your budget.

## Who it's for

1. **A home lab with one operator.** This comes first. Every item is judged by
   whether it makes one person with a GPU or two more capable.
2. **Small teams sharing one controller.** Named people, their own jobs, their own
   budgets.

Medium and large teams with datacenter hardware are beyond what the maintainer can
build and test. Contributions in that direction are welcome; see the end of this
page.

## Principles

- **Local first.** Anything that leaves your network (a frontier model, a web
  search, a catalog lookup) is explicit, consented and budgeted.
- **Managers don't write code.** The manager, the Planner and any outside agent
  design, research, review and test. Workers write the code, in isolation.
- **Evidence, not opinions.** Acceptance means tests that really ran, a mutation
  check that a test noticed, frozen tests nobody edited, and a verdict against the
  requirements. A model saying "done" is not evidence.
- **Measured, not declared.** Features that depend on model quality unlock from
  Toolkit measurements, not from a model's name.
- **One engine, many surfaces.** The GUI, TUI, web UI, phone and outside agents
  read the same state and use the same contracts. No interface gets its own task
  lifecycle.
- **Fail closed, claim honestly.** If isolation can't be established, the work
  doesn't run. Support and isolation claims say exactly what was tested.
- **Three products, clear boundaries.** Poundlock owns machines, models, admission
  and server-side execution. Dispatch owns projects, planning, permissions and
  review. Toolkit measures and never controls. Each keeps its own version, and a
  published table lists the combination tested together.

## Now

In order within each product. Dispatch and Poundlock items can proceed side by
side. The first item is the flagship.

### 1. Autopilot, supported (Dispatch)

From an agreed spec to finished software, and you can pause, talk and continue at
any time. Dispatch 0.3.0 ships Autopilot as a preview. Going supported means:

- **Spec and inventory.** `expand_inventory` turns a short goal into candidate
  requirements for you to trim. Each requirement has user scenarios and a source.
- **You in the loop without stopping it.** Your messages are handled from the run's
  inbox at the next step boundary. Pause hands the project back to the attended UI;
  Continue reconciles your spec edits: removed requirements drop their tasks,
  changed ones lose their verdicts, new ones join the gap analysis.
- **One worker contract** for attended work and Autopilot, with one check runner and
  one sandbox interface.
- **macOS projects in a VM.** A per-project macOS VM backend for native projects. The
  host sandbox remains, labelled reduced isolation.
- **Stronger gates.** A frozen test that skips, or a test-runner hook that turns
  failures into skips, fails the task. Approved dependency installs go through
  registry-only egress, as attended installs already do. A plan-level run
  environment for checks and worker commands. Toolchain images prove the full check
  offline before a run starts.
- **Packaged.** The app ships its own matching Autopilot runner. No separate
  command-line install.

Gate: benchmark goals in the Frontier tier verify at least 80% of in-scope
requirements, with at most two interventions, only allowed stops, a mutation catch
rate of at least 70% and spend within the cap. Local-tier goals complete with
advisory verification. Existing benchmarks don't regress. Then the preview label
comes off.

### 2. Research from the internet (Dispatch)

The Planner and a research stage before the spec can look things up instead of
relying on what a model remembers.

- **A task-scoped tool broker.** A plan names the tools it needs, and you grant them
  per project or per run. Credentials stay in the broker, never in a prompt or a
  worker. Every call is recorded with the grant that allowed it. Large results come
  back as a bounded preview plus a handle to page through the rest.
- **First tools:** web search, page and documentation fetch, and GitHub repository
  and code search, for finding existing projects and example code. Search providers
  are pluggable, including a self-hostable one (SearXNG) and paid APIs.
- **Research before the spec.** An optional stage turns cited sources into candidate
  requirements, keeping what a source says apart from what the model infers. You
  approve the inventory; the Planner and workers get a compact reference packet.
  Imported reference bundles work too, with no network at all.
- **Rules.** Search queries and fetched excerpts are data leaving your network,
  under the same consent as an external model. Fetched content is untrusted
  reference material: it can't grant itself tools or change the plan. Time,
  request, size and cost caps, with cancel. Links and summaries, not copied text,
  artwork or proprietary code.

Gate: on a small open-source app with public docs, the research-backed inventory
misses and invents fewer requirements than a model-only one, and its citations are
accurate.

### 3. Durable jobs for any agent (Poundlock)

Durable jobs stay a first-class Poundlock feature, so Poundlock is useful without
Dispatch. Any client can submit work that keeps running after it disconnects. The
use case that drives this: ask an agent such as Claude Code to build something. It
designs, researches, reviews and tests, and every line of code comes from Poundlock
jobs.

- **Autopilot's gates on the server.** The job runner refuses a check run that ran
  no tests, freezes only files named as tests, runs the mutation check, keeps the
  scope, symlink and environment-tamper guards, treats outages and a busy model as
  waits, and challenges a worker's first "blocked" claim. Dependency installs need
  approval and go through registry-only egress.
- **A task contract for outside managers.** A bounded brief goes in: files,
  acceptance checks, frozen tests, budget. A patch or branch, check results and
  evidence come out.
- **Adapters for agents:** the existing CLI, an MCP server over the same job API,
  and a short guide to running an outside agent as manager with Poundlock as its
  only way to write code.

Gate: an outside agent finishes a small multi-task feature in which all code came
from jobs and every acceptance rests on job evidence. Existing job clients see no
regression.

### 4. The job runner in its own VM (Poundlock)

Today the runner drives the host's Docker, and that socket is root on the host. The
runner moves into a small Linux VM with its own Docker and gVisor (`runsc`). Jobs
need no GPU; models stay on the nodes. This protects the host from the runner.
Isolating workers from each other is a Next item.

Gate: jobs run unchanged in the VM, the runner has no path to the host's Docker or
files, and abandoned containers are cleaned up.

### 5. One frontier budget for the whole install (Poundlock)

One weekly and one monthly ceiling across every application token, enforced by
Poundlock. Clients show what's left and warn before the cap. LiteLLM stays optional
and keeps pricing. Per-person limits nest under this ceiling when teams arrive.

## Next

Roughly in order. Remote control depends on Teams.

### Parallel workers inside Autopilot (Dispatch, Poundlock)

Several tasks of one goal run at once, each in its own worktree inside a
Dispatch-owned mirror, never in your `.git`. Accepted tasks join an integration
branch through a merge queue: fast-forward or rebase, then re-run the checks.
Conflicts become repair tasks for review; nothing is resolved silently. Opt-in per
project, starting at two lanes. Tasks declare the files they touch, and overlapping
tasks never run together. Your folder changes only when you apply the result.

Poundlock supports it with:

- a read-only **lanes view** for clients: machine, slot, size class, typed GPU and
  memory, occupancy;
- **multi-slot llama.cpp**: more than one slot per runtime, a host-memory prompt
  cache and session affinity in the queue, so a manager and a worker stop evicting
  each other's cache;
- **preemption and priority**: your turn or the Planner's no longer waits behind a
  long worker request, and jobs claim the model per request instead of holding a
  slot for their whole run.

### Teams (Poundlock)

- **Named operators and roles** instead of one admin token, with separate roles for
  publishing models, spending and administering nodes. Passkey sign-in.
- **Per-person ownership.** Jobs, results and usage belong to whoever submitted
  them. Per-application profile allowlists. Fair queuing between people.
- **Per-person and per-application budgets** under the install ceiling.

### Remote control through Poundlock (Poundlock, Dispatch)

An opt-in Dispatch setting relays its GUI and TUI sessions through your Poundlock
server. Sign in to Poundlock's web UI from any browser to watch sessions, answer
questions, read the morning report, pause, continue and talk to a running goal as
if you were at the computer running Dispatch. Autopilot's front ends already only
read state and write to the run's inbox, so a remote client is one more front end,
not a second engine.

- Needs named operators (Teams) and TLS. Remote access to something that runs code
  is the riskiest surface on this page. New authority (a folder, a network host,
  more spend) is never granted from a notification alone.
- **Attention that reaches you.** Milestones, caps, parked tasks and decisions reach
  the web UI even when the desktop app is closed. Desktop notifications and the
  dock badge exist today.
- **Fleet health in clients.** Machine state, wake, occupancy and queue position in
  Dispatch, with Toolkit evidence beside each model in the picker.

### Stronger isolation for workers (Dispatch, Poundlock)

- **A fresh microVM per worker attempt**, as an optional backend behind the same
  sandbox interface: Firecracker on Linux with KVM, Virtualization.framework on
  macOS.
- **Linux Dispatch parity.** Project network switches enforced and container or VM
  backends qualified, so Dispatch on Linux becomes supported rather than preview.

### Platforms (Dispatch, Poundlock)

macOS and Linux, on ARM and x86_64, come first.

- **A notarized macOS Dispatch:** Developer ID signing, hardened runtime,
  notarization.
- **macOS nodes (Apple silicon and Intel) and Linux ARM64 nodes supported**, with
  signed node updates from a project-held offline key.
- **An MLX runtime** for Apple silicon nodes, alongside llama.cpp.

### Privacy and your data (all products)

- **Verified offline mode.** Nothing leaves your network (no catalog lookups,
  remote fonts or telemetry), checked by a test that watches egress. External
  models, search and research are explicit opt-ins.
- **Retention and deletion.** Configurable retention for prompts, transcripts,
  results and source snapshots. Export. Delete a project's data. An uninstall that
  says what remains.
- **Tokens in the OS keychain** (macOS Keychain, Linux Secret Service), with an
  explicit cutover and no silent plaintext fallback.
- **Read-only data roots.** Durable jobs and Autopilot can read folders you approve,
  such as a NAS share, read-only, with grants and manifests.

### Measurement (Toolkit)

- **Planner capability measurements** (`spec_planning`,
  `requirement_verification`). A local model that qualifies unlocks full Local-tier
  Autopilot with no code change. Fully local Autopilot is a goal reached through
  evidence, not a promise.
- **Recommended configurations.** Quantization, context, KV precision, sampling,
  reasoning and slots for your hardware, in a versioned format that Poundlock
  imports as a profile only after you approve it.
- **Benchmark suites.** The end-to-end coding benchmarks that gate Autopilot,
  published with synthetic projects, plus a harness-comparison mode, so anyone can
  reproduce a quality claim.
- **Key-protected endpoints.** Qualify a model server that needs a key through a
  small relay, so the worker running model-written code never holds the key.
- **Long-run tooling.** Loop and stall detection for long runs, and a fake
  OpenAI-compatible server for deterministic tests.

### Delivery as branches and pull requests (Dispatch, Poundlock)

Results can arrive as a local integration branch, or be pushed with a pull request
opened on GitHub or Gitea, only after you authorize that for the project. Today
results arrive as a patch, a fast-forward or a branch to fetch.

## Later

- **Goals as server-side jobs.** Hand Poundlock a whole spec and walk away: the
  Autopilot engine runs as a job, for any client.
- **A computer per agent.** A persistent VM per project or agent that sleeps between
  sessions, keeps its tools and workspace, and has disk checkpoints for rollback.
  The hard parts are lifecycle: reset, retention, credentials that pile up, and
  keeping the task ledger and accepted source outside the VM. A checkpoint never
  undoes anything that happened outside it.
- **iOS and Android apps** with native push notifications, built on remote control.
- **Toolkit on a schedule.** Re-qualify models and machines periodically so drift
  and regressions surface by themselves. Toolkit stays a measurement tool; Poundlock
  or a scheduler runs it and reads the results.
- **A shared evidence base.** Opt-in, published qualification results normalized by
  hardware, model and runtime, so you can see what works on a card like yours
  before downloading anything.
- **Model watch.** Tell the operator when a new model is likely to fit and beat a
  current profile. It never downloads anything by itself.
- **A reverse-engineering adapter.** Optional REA integration for inspecting
  software you're authorized to inspect, before specifying something similar. It
  runs in a disposable environment, and static inspection and running a target are
  separate permissions.
- **Windows** nodes and Dispatch.

## Contributions welcome

Good ideas the maintainer can't build or test on home-lab hardware. If you have the
hardware and want to add one, open an issue first.

- vLLM or SGLang runtimes with continuous batching, for team-sized GPU servers.
- Features for medium and large teams, such as SSO and multi-tenant isolation.

## Not planned

- A new agent harness. Pi stays the worker harness.
- Skill-pack catalogs.
- Several people editing one Dispatch project together, and a full audit trail.
  Per-person ownership is planned.
- Toolkit as a scheduler or production controller.
- Sending your source to hosted sandboxes by default.
- Enterprise multi-tenancy or support agreements from the maintainer.

## How this roadmap changes

Picking up an item opens a GitHub issue that links back here. Larger items get an
ADR in the product that owns them. To propose an item or argue with one, open an
issue. When evidence moves an item between horizons, this page says so.
