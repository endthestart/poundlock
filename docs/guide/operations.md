# Installation and operations

Use the Compose bundle from a release. Its `.env` contains an immutable image
reference; ordinary installation needs Docker and Compose, not a compiler.
The controller and supervisor are always on. Inference machines are separate.

## Remote access

The default port binds to `127.0.0.1`. An SSH tunnel is sufficient for remote
browser administration. Nodes and external clients need a reachable controller
URL. Set `POUNDLOCK_LISTEN_ADDRESS` in `.env` to the desired private interface
address, then recreate the services with `docker compose up -d`.
Use HTTPS or a trusted private network for credentials and node updates. The
operator configures TLS/VPN and firewall rules; Poundlock does not configure them.
A node configured for one network interface stays offline if that interface fails.

## Workers and capacity

Only the supervisor has access to the Docker socket. A read-only socket mount
still permits Docker API operations; the supervisor is trusted with that authority.
Workers receive no socket, administrator token, upstream API key or host directory.

Set `POUNDLOCK_RUNNER_CAPACITY=2` in `.env` to permit two independent jobs, then
run `docker compose up -d`. Capacity limits job containers, not GPU allocation.
Each running job can hold up to about 11 GiB of memory (three 2 GiB RAM-backed
volumes and a 4 GiB container), so the runner lowers its capacity to what fits
in half of the host's memory, at least one, and logs when it does. Set
`POUNDLOCK_RUNNER_MEMORY_BYTES` to plan for a different amount, for example when the
jobs you run are known to be small.

Job containers get Docker's default seccomp and AppArmor profiles. For a
stronger boundary, install [gVisor](https://gvisor.dev/docs/user_guide/install/)
on the Docker host, register its `runsc` runtime with Docker, and set
`POUNDLOCK_WORKER_RUNTIME=runsc` in `.env`. The runner refuses to start if
Docker has no runtime by that name.
Each application token may have 10 unfinished jobs; set
`POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN` in `.env` to change that. An unfinished job
keeps its encrypted source, up to about 90 MiB, in the controller database, so
the limit also bounds how much one token can store.
The controller serves at most 512 requests at once, open streams included, and
answers `503 controller_busy` with `Retry-After: 1` beyond that; set
`POUNDLOCK_MAX_REQUESTS_IN_FLIGHT` to change it.
Models > Discover reads your gateways' routes locally. Comparing them with
OpenRouter's public catalog sends one request from the controller to
openrouter.ai, with no key or prompt, and only when an operator chooses **Look
up**. Set `POUNDLOCK_OPENROUTER_CATALOG=never` to refuse it entirely, or
`always` to read it on every Discover load.
Profiles sharing a node/runtime use the same exclusive inference slot. Configure
separate runtime profiles only when the underlying servers can safely run together.

Jobs default to inference-only networking. To offer outbound access, set
`POUNDLOCK_ALLOW_JOB_NETWORK=true`; each job must also select it explicitly.
Those jobs reach public addresses on ports 80 and 443 through the supervisor's
proxy, and nothing on the local network. It is not a domain allowlist.
The supervisor uses a bounded `/work` tmpfs sized by `POUNDLOCK_WORK_SIZE`
(default 2g). That setting does not change per-job workspace volumes, which
remain 2 GiB, or the worker CPU/memory ceilings.

## Wake-on-LAN

On a Linux controller host, enable the private wake relay:

```sh
docker compose -f compose.yaml -f compose.wake.yaml up -d
```

Use both files for subsequent Compose operations on this installation. The relay
uses host networking to send LAN broadcasts and accepts requests only through a
private Unix socket shared with the controller. It exposes no TCP API.
Configure the machine's wake interface and test real sleep followed by Generate.
A heartbeat alone does not establish working inference. Avoid SSH probes during a
wake test because some machines wake on connection attempts.

## Optional registered repositories

Uploaded folders and HTTPS repository jobs need no host project mount. If an
operator wants to register existing committed server repositories, set
`POUNDLOCK_PROJECTS_PATH` to their parent directory and add
`-f compose.repositories.yaml` to Compose commands. Mounts are read-only.
Only administrator submissions can select these registered repositories.

## Credentials

Read the initial administrator token with:

```sh
docker compose exec controller cat /data/admin-token
```

Use Access to create and revoke named application tokens. Tokens are shown once.
Revoking a token also stops its inference streams and cancels the jobs it
submitted that have not finished. An application token cannot recover the
administrator token. Sign out in the
browser to clear the token stored on that device; that does not rotate the
server secret. To revoke the administrator token, replace `/data/admin-token`
with a new secret and restart the controller. Reenrollment of an offline or
removed name rotates a node's credential, and needs a token created for that
name (the Machines page creates every token for the name you enter). A
still-online or paused name cannot be taken over with a new enrollment token. Do not wipe controller state to
replace one computer.

## Backup and restore

### Nightly database backup

The controller can copy its database every night while it runs. Mount a folder for
the copies and set:

| Setting | Meaning |
|---|---|
| `POUNDLOCK_BACKUP_DIR` | The folder inside the controller container, for example `/backup`. Unset, there is no nightly backup. |
| `POUNDLOCK_BACKUP_AT` | Time of day in UTC, `HH:MM`; default `08:30`. |

Each run takes a consistent copy with SQLite's `VACUUM INTO` (writers keep
working), checks it (`PRAGMA integrity_check`, every table present, row counts of
tokens, profiles, machines and jobs), and writes `poundlock-YYYY-MM-DD.sqlite3.gz`
with a `.sha256` beside it. It keeps the 14 newest copies and the first copy of each
of the last six months, and writes `last-backup.json` (`ok`, the file, its size and
checksum, the counts, or the error) for monitoring; `GET /api/v1/backup` (administrator)
returns the same. A controller that starts without a good copy from the last day makes
one a minute after starting. The folder must belong to the
controller's user (UID 10001) and be writable by it alone (for example mode 0700): a folder
that is a symlink, belongs to someone else or that others may write is refused, and the
uncompressed copy is made beside the database, never in the backup folder. `poundlock backup --to DIR` makes one copy by
hand, for example before an upgrade.

The copies hold the database only. **`master-key` is never copied**, because the
copies usually go offsite: keep one copy of it somewhere safe, such as a password
manager. Without it, a restored database keeps everything except the stored
endpoint and gateway keys, which you enter again.

To restore a nightly copy, stop the controller, check the file against its
`.sha256`, unzip it over `data/poundlock.sqlite3` (removing any
`poundlock.sqlite3-wal` and `-shm` beside it), put `master-key` back if the data
folder lost it, and start the controller; `/readyz` reports ready once it has
opened the database.

### Full backup

Finish or cancel active jobs before a planned backup. Stop the services and copy
both controller state and persistent supervisor results:

```sh
docker compose stop
docker compose cp controller:/data ./poundlock-data-backup
docker compose cp runner:/results ./poundlock-results-backup
docker compose start
```

Keep the Compose files and exact image reference with the backup. Protect it:
controller state includes the administrator token and encryption key; pending
results can contain source code. Preserve the controller database and its
`master-key` together. Model weights and source repositories need separate backups.

To restore on the same host, stop the original installation first. Use a new
Compose project name so the original volumes remain intact. With the saved image
and configuration, create stopped containers and copy the backup contents:

```sh
docker compose -p poundlock-restored create
docker compose -p poundlock-restored cp ./poundlock-data-backup/. controller:/data/
docker compose -p poundlock-restored cp ./poundlock-results-backup/. runner:/results/
docker compose -p poundlock-restored run --rm --no-deps --user 0:0 --cap-add CHOWN --entrypoint chown controller -R 10001:10001 /data
docker compose -p poundlock-restored run --rm --no-deps --user 0:0 --cap-add CHOWN --entrypoint chown runner -R 10001:10001 /results
docker compose -p poundlock-restored up -d
```

The ownership commands use the image's default UID/GID. Adjust the runner's owner
if you configured a different one. Continue using `-p poundlock-restored` for this
restored installation. Never merge two databases or restore newer state into an
older binary. Reconnect nodes only after checking the restored controller.

## Upgrade and rollback

Drain active work and make a stopped backup. Replace `.env`'s `POUNDLOCK_IMAGE`
with the new release's digest, retaining your other settings. Run
`docker compose pull` and `docker compose up -d`, then check login, Machines,
Generate and a small verified job.

Upgrade the controller before its nodes. It answers an older node's polls as it
always did, while a newer node against an older controller polls every
`heartbeat_seconds` and gets faster model requests only once the controller is
upgraded.

### From 0.1.x to 0.2.0

1. Deploy the 0.2.0 image. On its first start the controller saves the database as
   `poundlock.sqlite3.pre-secret-binding` and re-seals stored secrets.
2. If your build embeds an update key, sign the node binaries it serves:
   `scripts/sign-node-updates.py` ([Node update signing](../node-update-signing.md)).
   Nodes then update themselves; a 0.1.x node takes the signed build by checksum.
3. If a model profile uses the built-in Ollama runtime, add `[ollama]` with
   `enabled = true` to that node's `node.toml` before the node updates.
4. Optionally move each Linux node to its own account:
   `sudo bash poundlock-node.sh account`, with the installer from Machines. Its
   runtime, models and key files must then live outside home folders.
5. Give each llama.cpp server an API key: create the key file, set `api_key_file` in
   the runtime manifest, and enter the same key on the endpoint in Setup.

Rollback uses the prior image together with the matching pre-upgrade backup.
From 0.2.0 the controller binds every stored secret to its database row and
re-seals older secrets the first time it starts, so a 0.1.x image cannot read a
database a 0.2.0 controller has opened. Before re-sealing it saves the database
as it was beside the original, as `/data/poundlock.sqlite3.pre-secret-binding`;
to roll back, stop the services, put that file in place of
`/data/poundlock.sqlite3` (removing its `-wal` and `-shm` files), and start the
older image. Changes made since the upgrade are lost. The copy holds the same secrets
as the database; delete it once you no longer need the rollback.
Database migrations may prevent simply starting an older image on newer state.
Linux node upgrades and rollback are guided in Machines; they preserve enrollment.
This first public release does not run automated previous-image upgrade in the
GitHub workflow; later releases should pass the prior image to the install
rehearsal. GitHub pull-request validation uses read-only permissions. Image
publication, attestations and the draft GitHub release run only for version
tags, from the image that validation already tested.

## Failure recovery

- Identity only lists models. Generate waits 90 seconds for an answer and does
  not retry. Retry Generate if it times out. The first request after idle can
  be slower than a later retry; a later success is not proof that every first
  request will finish in time.
- Interactive chat uses `POUNDLOCK_EXTERNAL_TIMEOUT_SECONDS` after the machine
  is prepared. FIM completions use a 30-second inference budget. Wake and
  runtime prepare have a separate deadline and are not added back into that
  inference budget. A stalled upstream fails the request instead of appearing
  complete.
- A durable job's model requests use `POUNDLOCK_JOB_INFERENCE_TIMEOUT_SECONDS`
  (default 1200): the longest the model may stay silent, both before its
  response starts and between streamed chunks. It is longer than the
  interactive budget because a job may prefill its whole context on a slow
  local model. When it expires the request fails with `504 upstream_timeout`,
  which the job's harness retries; the job itself keeps running.
- A disconnected browser can reopen the same job. It does not need to resubmit.
- A failed job keeps its error, output, checks and any collected patch. Review the
  evidence before retrying. Repair attempts are bounded by the submitted policy.
- The supervisor cleans up only resources carrying its own identity and storage
  labels. Cleanup failures stop new dispatch. Check `docker compose logs runner`.
- Reports not acknowledged by the controller remain in `/results/outbox`.
  Preserve conflicting reports; do not delete them or rerun a job to hide a failure.
- A node's default 300-second lease expiry releases ownership after loss of contact.
  Supported grace periods are 180–3600 seconds. It does not force the OS to sleep.
- An interrupted interactive stream is not a durable job. After a controller
  restart, the old upstream stream may briefly overlap a new request until local
  expiry; avoid restarting the controller during interactive inference.

## Removal

`docker compose down` removes services and keeps data. Add `-v` only to delete
controller history, credentials and pending results permanently. This does not
remove models or external repositories.

Use the Linux node installer's `remove` command to revoke enrollment and remove
its service, configuration, binary and polkit rule. For a wiped/unavailable machine,
use Remove in Machines. Removing a model profile or endpoint hides it from new
work; previously accepted jobs retain their saved settings and history.
