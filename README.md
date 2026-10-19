# Poundlock

**Put your private inference infrastructure to work.**

Status: 0.3.0, the first public release. A small project with one maintainer;
see [support](#status-and-support).

Poundlock connects the model servers you already run to your editor and to
unattended coding jobs. Submit a prompt and source files, let a local agent work,
then review its patch and independently checked results.

- **One API for your models.** Connect OpenAI-compatible servers directly, or use
  an existing gateway such as LiteLLM.
- **Explicit external access.** Keep local inference separate from permission to
  send data or spend through external profiles, with optional LiteLLM budgets.
- **Jobs that survive disconnection.** Follow progress in the browser or API,
  cancel work, and download patches when it finishes.
- **Independent checks.** Run verification commands, protect selected files,
  choose dependency network access, and limit repair attempts.
- **Wake and release.** Enroll Linux or macOS machines, use Wake-on-LAN where
  supported, and load/unload Ollama models automatically.
- **Multiple machines.** Run independent jobs concurrently while keeping a fast
  completion model available for your editor.

Poundlock uses Pi 1.1.0 as its coding harness. You choose the model and its settings.
Model servers, GPU drivers and model downloads remain under your control.
The [architecture map](docs/maps/poundlock-release.architecture.html) shows the
controller, trusted supervisor, restricted agent containers and separate model
servers.

For the full Toolkit → Poundlock → Dispatch workflow, start with the
[family getting-started guide](docs/guide/getting-started.md). What changed in each
release, and how to upgrade, is in [CHANGELOG.md](CHANGELOG.md).

## Install

The server runs on **Linux x86_64 with Docker Engine 28+ and Docker Compose**.
You also need a working OpenAI-compatible model server, such as llama.cpp's
`llama-server`, Ollama or vLLM; LiteLLM is optional. See
[status and support](#status-and-support) for node platforms.

1. Download `poundlock-compose.tar.gz` and `SHA256SUMS` from this repository’s
   Releases page. Check the archive before extracting it:

   ```sh
   sha256sum --ignore-missing --check SHA256SUMS
   tar -xzf poundlock-compose.tar.gz
   ```

2. Give the supervisor access to Docker’s socket group, then start:

   ```sh
   printf '\nPOUNDLOCK_DOCKER_GID=%s\n' "$(stat -c '%g' /var/run/docker.sock)" >> .env
   docker compose up -d
   docker compose exec controller cat /data/admin-token
   ```

3. Open **http://127.0.0.1:7331** and enter that administrator token. Use
   **Sign out** when finished on a shared browser. The release bundle pins its
   image by digest; no source build is required.

For a remote server, use a private connection or HTTPS and configure its listening
address. See [installation and operations](docs/guide/operations.md).

## First useful job

In **Connections**, add your model server’s API URL (usually ending in `/v1`). In
**Models**, choose **Add model**, fetch its model IDs and create a profile using an exact ID and the context limit configured
in that server. Pi coding tasks need at least 8192 context tokens. Use **Generate**
to test an actual answer. Identity only lists models. Generate waits up to
90 seconds; the first request after idle can be slower than a later retry.

In **Jobs**, choose that profile, upload a folder or start with an empty workspace,
and describe the change. Add a verification command when you want a checked
result. Review the returned patch before applying it; a completed job without
verification commands is marked unverified.

[Job submission and API examples](docs/guide/jobs.md) cover repository checkout,
protected tests, network permissions, repair limits and patch downloads.

## Connect your editor

Create an application token in **Access** with permission to list models and send
inference requests. Configure your editor’s OpenAI-compatible provider with:

| Setting | Value |
|---|---|
| API URL | `http://127.0.0.1:7331/v1` |
| API key | Your application token |
| Model | A profile name from Models |

Use a fast-completion profile for autocomplete and a coding profile for chat.
Select **Allow prompts and files to leave the controlled network** only for an
application that may use profiles declared External. Managed LiteLLM access also
requires an operator-set spending limit and reset period.
Keep the administrator token out of editor settings. The optional native ACP
adapter is described in [integrations](docs/guide/integrations.md).

## Add a sleeping machine

Open **Machines → Linux installation guide** for enrollment, diagnostics,
upgrades and removal. Enrollment reports hardware, network interfaces and model
server setup links. Select the connection you want the node to use, and configure
its wake-capable interface separately if needed.

For Ollama, select **Ollama — managed loading and unloading** in the model profile.
Set **Unload after idle seconds** to `0` to unload immediately after work—useful
on a shared gaming GPU. Poundlock leaves externally loaded models alone.

Wake-on-LAN also requires support and configuration in the NIC, firmware and OS.
See [machine setup](docs/guide/machines.md), including macOS and runtime limits.

## Trust and limitations

This release targets a trusted personal or small-team deployment. Each
application token sees and cancels only its own jobs, but jobs share one queue,
one set of runners and the same model capacity: **it is not a multi-tenant
service**. Only the trusted supervisor receives Docker access. Agents run in
separate, restricted containers and receive job-scoped inference credentials.
A job allowed outbound access reaches public addresses on ports 80 and 443
through the supervisor's proxy, never the local network.

What leaves the machine: prompts and files go only to the model servers you
configure, and to external ones only for profiles you declare External. The
controller makes no other outbound request unless an operator looks up
OpenRouter's public catalog in Models > Discover, a job fetches its source from an
approved Git host, or a job allowed network access fetches dependencies. Prompts, results and job events stay in the controller
database until you remove them; a job's source is deleted when it ends.

Docker shares the host kernel. Review generated code before running it elsewhere.
Model and harness quality determine what a job can accomplish; Poundlock does not
guarantee that an agent can finish an arbitrary application.

Back up controller state and pending runner results together. Follow the
[operations guide](docs/guide/operations.md) for upgrades, recovery and removal.
See [SECURITY.md](SECURITY.md) for the security boundary and reporting guidance.

## Status and support

| Component | Platform | Status |
|---|---|---|
| Controller and job runner | Linux x86_64, Docker Engine 28+ with Compose | Supported |
| Node | Linux x86_64 with systemd | Supported |
| Node | Linux ARM64; macOS Apple silicon and Intel | Preview: built and unit-tested by CI, not acceptance-tested on hardware for this release |
| Anything on Windows | | Not supported |

Release node binaries and images are unsigned; see
[node update signing](docs/node-update-signing.md). Fixes go into the latest
release only. One maintainer looks after this project on a best-effort basis:
issues and pull requests are welcome, and security reports go through
[SECURITY.md](SECURITY.md). What comes next for all three products is in the
[roadmap](ROADMAP.md).

## Development

Clone the source repository before running these commands:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
POUNDLOCK_IMAGE=poundlock:dev docker compose -f compose.yaml -f compose.build.yaml up --build -d
```

[Contributing](CONTRIBUTING.md) explains the checks, the end-to-end suite and
release builds. [Ecosystem compatibility](ECOSYSTEM.md) says which Toolkit and
Dispatch versions were tested with this release. [HISTORY.md](HISTORY.md) says
where the code came from.

Licensed under [Apache-2.0](LICENSE); see [NOTICE](NOTICE) and
[third-party notices](THIRD_PARTY_NOTICES.md). Model weights and model servers
are not part of Poundlock.
