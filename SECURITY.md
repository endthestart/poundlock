# Security

Poundlock is for a trusted operator and authorized users of a shared deployment.
It is not a tenant isolation service. Keep administration on loopback or a trusted
private connection; use HTTPS when credentials cross an untrusted network. Nodes
refuse a plain `http://` controller on another machine unless `node.toml` sets
`allow_insecure_http = true`, which Machines shows.

## Reporting

Report a vulnerability privately with GitHub's **Report a vulnerability** button on
this repository's Security tab. Do not put an exploit, credentials, private source
code or deployment addresses in a public issue. Include the affected version and
the component involved. Do not test against someone else's installation without
permission.

One maintainer looks after this project. Reports are acknowledged on a best-effort
basis, usually within seven days; there is no service-level agreement. Fixes go
into the latest release only.

## Trust boundaries

- The controller holds administrator and application credentials, encrypted
  upstream keys, node enrollment and job history. Its database and master key
  belong in protected backups. A job's encrypted source, including any Git read
  token, is deleted when the job ends; a backup keeps what the database held
  when it was made.
- The supervisor has Docker API authority. Treat it and its configuration as
  privileged. A read-only socket mount does not make the Docker API read-only.
  It never parses fetched Git data: a job's Git source is fetched by a short-lived
  container without the socket, straight into the job's volumes, reaching only the
  repository's host.
- Stored upstream keys, job contracts, sources and managed keys are sealed with the
  master key and bound to their table, row and column, so a ciphertext copied into
  another row does not open.
- Agent and verification containers process untrusted source, prompts and model
  output. They must not receive the Docker socket, administrator credentials,
  upstream secrets or arbitrary host mounts.
- Node enrollment authorizes the controller to request the locally configured
  runtime actions. It does not authorize arbitrary remote shell commands.
- Node self-update is on unless `node.toml` sets `auto_update = false`. Public
  release binaries embed no update key: they trust the controller's checksum and
  update only over HTTPS or loopback, so the controller chooses the code those
  nodes run. A node you build with your own key installs only newer binaries
  whose manifest that key signed; you sign on your own machine and upload the
  signatures, and neither CI nor the controller holds the private key. See
  [Node update signing](docs/node-update-signing.md).
- A Linux node runs as its own system account, which cannot read home folders,
  under systemd's `Protect*` and `Restrict*` settings, unless the operator installs
  it under a login. The built-in Ollama runtime is off unless `node.toml` enables
  it.
- Native ACP runs with the local user's permissions. It is a separate execution
  boundary from the restricted durable worker containers. Attached Pi loads no
  project settings, extensions, skills or context files and inherits only a
  minimal environment; the model's tool calls still run as the user.

## Required properties

Authenticate and authorize before performing protected work. Scope temporary
inference credentials to their job and revoke them on completion or cancellation.
Keep the accepted job's source, model, harness and verification policy stable.
Treat patches and agent claims as untrusted; independent checks determine the
verification result. Protected files must remain protected during repair.

Bound source sizes, request parsing, execution time, repair attempts and the
unfinished jobs each application token may hold. Delete a job's source when the
job ends. Cancellation and recovery must terminate owned processes and clean up
owned resources without touching another supervisor's resources. Failed cleanup
must remain visible.
Do not silently change a model, runtime, network policy or submitted contract.

## Limitations reviewers should consider

Each application token sees, cancels and downloads only the jobs it submitted,
and revoking it cancels them and stops its streams. A token may hold a limited
number of unfinished jobs, which bounds the source it can store, but finished
jobs keep their prompt, results and events with no retention period. Jobs still
share one queue, one set of runners and the same model capacity; this is not
tenant isolation.
Docker containers share the host kernel unless `POUNDLOCK_WORKER_RUNTIME` names a
stronger runtime such as gVisor's `runsc`, which the Docker host must provide.
Explicit dependency network access goes
through the supervisor's proxy, which reaches only public addresses on ports 80 and
443, never local services; it is not a destination allowlist. Operators are
responsible for model-server access control, TLS, network rules and host updates.
The operations guide documents lease expiry and interrupted-stream behavior.
The administrator token is a file-backed bearer: replace the file and restart
to revoke it; browser Sign out only clears that device. The browser keeps it in
local storage until Sign out. The UI is served with a strict content security
policy and may not be framed. Job list APIs omit
result bodies. Enrollment refuses a still-online or paused machine name, and an enrollment token created for one machine name cannot enroll another or take over an existing machine.
Release container images are attested. Base images are pinned by digest and Pi
and its dependencies are installed from a committed lockfile, but Debian packages
are upgraded at build time, so later rebuilds are not bit-for-bit reproducible.
The controller refuses requests beyond a limit in flight (`POUNDLOCK_MAX_REQUESTS_IN_FLIGHT`)
but has no per-client rate limit or header-read timeout; put a reverse proxy in
front of it when it is reachable from an untrusted network.
Model servers' error messages reach applications with URLs, addresses and
credential-like values replaced, but are otherwise passed through.
GitHub pull-request validation is read-only and holds no secrets; publishing runs
only for version tags, uses the image and archives CI tested, and waits for a
maintainer's approval.

These boundaries describe deployment and severity context, not blanket exclusions.
Report reachable violations of the stated controls, including those involving
authorized but less-privileged users. No finding class is excluded by this policy.
