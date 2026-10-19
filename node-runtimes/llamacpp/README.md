# poundlock-llamacpp

A node runtime that starts, keeps warm and stops `llama-server` (llama.cpp) for
the Poundlock node agent. One manifest describes the models; one command
serves Linux and macOS. It uses only the Python standard library (3.9 or newer).
llama.cpp forks that keep `llama-server`'s flags and endpoints work the same way, such
as a patched build for older GPUs; `identity` reports the fork's app name and version.

A runtime is one hardware slot. `{model}` (the Poundlock profile's model id)
picks the manifest profile whose key matches. If a different model is warm and
idle, `ensure` replaces it; if it is leased, the slot is busy.

## Node configuration

In `node.toml` (see `config/examples/node.toml`):

```toml
[runtimes.llamacpp]
ensure   = ["/usr/local/bin/poundlock-llamacpp", "--home", "/var/lib/poundlock/llamacpp", "--manifest", "/etc/poundlock/llamacpp.json", "ensure", "{model}", "{lease_id}", "{operation_id}"]
renew    = ["/usr/local/bin/poundlock-llamacpp", "--home", "/var/lib/poundlock/llamacpp", "--manifest", "/etc/poundlock/llamacpp.json", "renew", "{lease_id}", "{operation_id}"]
release  = ["/usr/local/bin/poundlock-llamacpp", "--home", "/var/lib/poundlock/llamacpp", "--manifest", "/etc/poundlock/llamacpp.json", "release", "{lease_id}", "{operation_id}"]
stop     = ["/usr/local/bin/poundlock-llamacpp", "--home", "/var/lib/poundlock/llamacpp", "--manifest", "/etc/poundlock/llamacpp.json", "stop", "{lease_id}", "{operation_id}"]
identity = ["/usr/local/bin/poundlock-llamacpp", "--home", "/var/lib/poundlock/llamacpp", "--manifest", "/etc/poundlock/llamacpp.json", "identity"]
```

Then create a Poundlock model profile whose model id is a manifest key and whose
runtime is `llamacpp`. Keep that name: the controller recognises llama.cpp by it, and
sends reasoning levels and thinking budgets to such profiles in the form llama-server
honours.

- `--home` holds state, the lock, logs and the hash cache. On Linux the node
  service can only write under `/var/lib/poundlock`, so pass it explicitly.
  Defaults: `~/.local/state/poundlock-llamacpp` on Linux,
  `~/Library/Application Support/poundlock-llamacpp` on macOS.
- The model id may use letters, digits and `. _ - :` only.
- **Node agent version:** `{model}` and `identity` need an agent that supports
  them (0.1.2 and later). An older agent passes `{model}` literally and rejects
  `identity`: write the model id in place of `{model}` and leave `identity` out.
- The node stops any command after 600 seconds, so keep `startup_timeout_seconds`
  at 570 or less and run `preflight` after installing (below).

Commands, all printing one JSON object on stdout:
`ensure MODEL LEASE OP`, `renew|release|stop [MODEL] LEASE OP` (a leading MODEL is
accepted and ignored), `identity [MODEL]`, `status`, `sweep`, `preflight [MODEL]`.
`identity` lists the manifest's model ids in `servable` and, in `models`, each one's
label and context, so Poundlock offers them when you add a model on this machine.
An older node agent passes on `servable` only, and Poundlock then asks for the context.
Exit codes: 0 done, 2 invalid or refused, 3 conflict or busy, 4 could not be made
ready (a JSON error goes to stderr).

## Manifest

JSON (any Python 3.9+), or TOML on Python 3.11+ (`manifest.toml`). Start from
`examples/linux-amd-vulkan-mtp.json` or `examples/macos-metal.json`. Unknown keys are
errors, and the file must not be writable by group or others: it names programs to run.

The manager sets `--model`, `--alias`, `--ctx-size`, `--host` and `--port` from the
profile, and `--api-key-file` and `--no-slots` from `api_key_file` and `slots_endpoint`.
Repeating them in `arguments`, or using `--api-key`, `--slots` or `LLAMA_API_KEY`, is an error.

| Key (per profile unless noted) | Meaning |
| --- | --- |
| `model.path`, `size_bytes`, `sha256` | Absolute file, exact size, lowercase SHA-256. Checked on every cold start; the hash is cached by size and mtime. |
| `model.family`, `quantization`, `source` | Optional labels, reported by `identity`. |
| `server.executable`, `min_build` | The real `llama-server` (not a wrapper that does not `exec` it). An older build is refused; an unparsable version is recorded, not fatal. |
| `server.host`, `port`, `context_tokens` | Bind address (`0.0.0.0` is probed on `127.0.0.1`), port, context. `n_ctx` from `/props` must match; with `--parallel N` the per-slot share also passes. |
| `server.api_key_file` | Optional absolute path to the server's API key; see [API key and network](#api-key-and-network). Probes and the warm-up send it as `Authorization: Bearer`. |
| `server.slots_endpoint` | `false` passes `--no-slots`, which turns `/slots` off; absent or `true` leaves llama-server's default. Check that `llama-server --help` lists `--no-slots` first. |
| `server.arguments` | Extra `llama-server` options. |
| `server.environment` | Extra environment, for example GPU selection. Values are never printed. |
| `required_log_markers` | Strings that must appear in the server log, for example `creating MTP draft context`. |
| `warmup` | Optional priming request, then one measured request. `min_tokens_per_second` fails a slow decode (a VRAM spill); `require_no_reasoning` fails if thinking is on; `request` adds body fields such as `{"reasoning_effort": "none"}`. |
| `conflicts` | `["ollama"]` unloads Ollama's models before starting. |
| `max_idle_vram_bytes` | Refuse to start while a GPU holds more VRAM than this (AMD sysfs readings, when available), waiting up to `vram_settle_seconds`. |
| `warm_seconds` (600), `lease_timeout_seconds` (300), `startup_timeout_seconds` (420) | Warm window after release (0 stops at once); lease expiry without `renew`; time to become ready. |
| `runtime.sweeper` | `detached` or `external`; see below. |
| `runtime.sweep_interval_seconds` (30), `caffeinate` (false, macOS only), `ollama_url`, `foreign_process_names` | Sweeper tick; hold `caffeinate -dimsu` for the server's life; loopback Ollama address; process names refused as not ours (default `llama-server`). |
| `runtime.peer_homes` | Homes of other instances on this machine, for a second slot: the server each one's state records is not refused as foreign. Give each instance its own home, manifest, port and node runtime, and the second instance the `detached` sweeper (the launchd job's label is fixed). |

## API key and network

llama-server answers anyone who can reach its port, so give it a key whenever `host` is
`0.0.0.0` or a LAN address. llama-server may still answer `/health` and `/v1/models` without it.

1. As the login the node service runs as, create the key file (sudo only to create it under `/etc`):

   ```sh
   sudo install -m 600 -o "$USER" -g "$(id -gn)" /dev/null /etc/poundlock/llamacpp.key
   openssl rand -hex 32 > /etc/poundlock/llamacpp.key
   ```

   It must be a regular file, not a symlink, with no access for group or others, holding one
   key (32 to 1024 characters from `A-Z a-z 0-9 . _ ~ + / = -`) on one line. Anything else
   is refused before the server starts. The runtime reads the file each time it probes and
   never prints, logs or stores the key.
2. In Poundlock, open **Connections**, edit this server's endpoint and enter the same key as its
   **API key**. The controller sends it as `Authorization: Bearer` on every request. Do this first:
   a server without a key ignores the header.
3. Set `server.api_key_file` in each profile and run `preflight`. The changed profile restarts
   the server at the next `ensure`. If you later change the key, update the endpoint too; a warm
   server whose key no longer matches is restarted.

**Firewall.** A `0.0.0.0` bind listens on every interface. Allow the port only from the
controller's address (an nftables, firewalld or ufw rule), or bind the address the controller
uses. Job containers with outbound network can reach LAN ports too.

## Behaviour

- **ensure** refuses a foreign `llama-server` and a busy port, checks the model and
  build, unloads conflicts, starts the server, then requires `/health`, the model id in
  `/v1/models`, `n_ctx`, the log markers and the warm-up gates. Any failure kills the
  server it started and records why. A warm server for the same profile is reused
  without restarting or re-running the warm-up.
- **renew** extends the lease after checking only that the server is ours: no health
  probe, so a server busy with a long request is never judged dead. If it is gone,
  renew fails and the lease is lost.
- **release** keeps the server for `warm_seconds`. **stop** ends it if the lease matches.
  Both are safe to repeat.
- **Sweeping** ends abandoned starts, expired leases and expired warm servers.
  - Linux: a detached sweeper starts beside the server and exits with it. It needs no
    unit file, and it shares the server's cgroup, so restarting the node service ends
    both. No systemd timer is used. Set `"sweeper": "external"` and run `sweep` from your
    own timer if you prefer one.
  - macOS: a launchd job runs `sweep` every minute.
- **Sleep:** the node agent already holds an inhibitor for each lease, so the runtime
  adds none on Linux. On macOS, `caffeinate` (if configured) also covers the warm window.
- **Safety:** the manager signals only a process it can prove it started: same start-time
  token, same binary, and the profile's model, alias and port on the command line, even if
  `state.json` is edited. State and logs are private (`0700`/`0600`) under `--home`.
  Output and logs never include arguments, environment values or the API key.

## Install

From a checkout:

```sh
node-runtimes/llamacpp/deploy/install.sh                 # ~/.local/bin/poundlock-llamacpp
node-runtimes/llamacpp/deploy/install.sh --prefix /usr/local --python /usr/bin/python3
```

It builds one self-contained executable, copies the examples to `share/`, and touches
nothing else: no manifest, state or models. Then:

1. Copy an example to your manifest; set the paths, size, SHA-256 (`sha256sum`),
   markers and arguments for your build.
2. `poundlock-llamacpp --home HOME --manifest FILE preflight` checks the files and
   fills the hash cache, so the first `ensure` does not spend its time hashing.
3. macOS only: add `--launchd` to the install (with `--home` and `--manifest`) to load
   the sweep job `com.poundlock.llamacpp`.
4. Add the `[runtimes.llamacpp]` block and restart the node. `status` shows the live
   state, and `logs/manager.log` under `--home` has one line per event.

Remove with `install.sh --uninstall` (add `--launchd` on macOS). State, logs, models and the
manifest stay. Tests: `python3 -m unittest discover -s node-runtimes/llamacpp/tests -t
node-runtimes/llamacpp`; they use a fake server and never run llama.cpp.

## Troubleshooting

- **First request on Vulkan is slow.** Mesa compiles shaders on the first request. The
  priming request absorbs it, and the cache lives in `HOME/cache` (`XDG_CACHE_HOME`) so it
  survives restarts. After a driver or llama.cpp upgrade allow a longer
  `startup_timeout_seconds`.
- **"below the minimum ... a VRAM spill is likely".** Layers or KV cache spilled into system
  memory. Check `vram_used` and `gtt_used` in `status`; free the GPU (`conflicts`,
  `max_idle_vram_bytes`), then lower `context_tokens`, the KV cache type or the offloaded layers.
- **"a llama-server that this runtime did not start"** or **"port N is already in use".**
  Left alone on purpose: stop it yourself. Deleting `state.json` while a server runs causes this too.
- **"context does not match".** Set `--parallel` explicitly in `arguments`; llama.cpp splits
  the context across slots.
- **"required server log marker is absent".** Read the log named by `server_log` in `status`;
  the build or model probably lacks the feature (for example MTP).
- **"model sha256 mismatch" or "size mismatch".** The file is not the one in the manifest; the
  message shows the hash found.
- **"llama-server wants an API key".** The server holds a key that no profile names: set
  `server.api_key_file` to the file it should read.
- **"refused the key in server.api_key_file".** The file changed after the server started. The next
  `ensure` restarts it; update the endpoint's key in Poundlock.
- **"api_key_file ... is accessible to group or others"**, **"is a symlink"** or **"must hold one key"**.
  `chmod 600` the file, name the file itself, and write one key on one line.
- **Exit 3, "busy".** Another operation holds the lock. Retry.
- **"manifest ... is writable by group or others".** `chmod go-w` it.
