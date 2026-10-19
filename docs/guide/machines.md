# Inference machines

Poundlock's node reports hardware and network information, reconnects to the
controller after wake, and keeps the machine awake while it owns active work.
It does not install GPU drivers, download model weights or replace your model
server. You can enroll first and choose inference software afterward.

## Linux

Use **Machines → Linux installation guide**. The page generates the configuration,
provides the matching binary and installer, and shows complete commands. Download
all files into any folder you own on the target machine.

The installer uses sudo to create a systemd service and a narrow polkit rule.
The node runs as its own system account, `poundlock-node`, with no login and no
access to home folders, in the `render` group for the GPU when it exists.
Its service is hardened with systemd's `Protect*` and `Restrict*` settings.
Give `--user LOGIN` only when the node's runtime or models live in that login's
home; that login's home then stays read-only to the node. Permanent paths are
`/etc/poundlock/node.toml` and `/var/lib/poundlock/`. The installer handles
permissions and enrollment; you do not need to move files there yourself.

An installation from 0.1.x runs as the login that installed it. Move it to the
system account with `sudo bash poundlock-node.sh account`, and back with
`sudo bash poundlock-node.sh account --user LOGIN` if its models need the home
folder; the command checks the node with `doctor` afterwards.

The supported baseline is systemd Linux with glibc 2.36 or newer. The UI reports
available binaries; do not select a different processor's binary to work around
an unavailable download. Diagnostics, upgrades, rollback, removal and reenrollment
are included in the guide. A new enrollment token cannot take over a name that
is still online or paused; wait until the machine misses heartbeats or remove it
first. Reinstall after a wipe reuses the same name once the old registration is
offline or removed.

## Controller address

A node's `controller` address must be `https://` unless the controller runs on
the same machine. Over plain `http://` to another machine, the node's credential
crosses the network unencrypted and anyone on the path can answer for the
controller. To accept that on a network you trust, add
`allow_insecure_http = true` to `node.toml`; `node-check` and the service refuse
the address otherwise. Machines marks a node that uses this setting. Prefer a
TLS reverse proxy in front of the controller.

## Model servers and GPUs

The machine card links to upstream installation guidance based on detected OS,
GPU and installed runtimes. Upstream publishers maintain their hardware support
lists. Useful starting points include:

- [Ollama](https://docs.ollama.com/linux) and its [GPU guidance](https://docs.ollama.com/gpu).
- [LM Studio](https://lmstudio.ai/docs/app) for a desktop interface and local API.
- [llama.cpp](https://github.com/ggml-org/llama.cpp) for CPU, CUDA, Vulkan and other backends.
- [vLLM](https://docs.vllm.ai/en/latest/getting_started/installation/) for supported serving hardware.

On Apple Silicon, an OpenAI-compatible MLX server is another option. Intel Macs
need a runtime that actually supports their CPU/GPU; Apple Silicon instructions
do not apply to them. Connect a server you already use through its network API.

## When a request arrives

Each model on a machine says what Poundlock does when a request for it arrives.
The API field is `wake_policy`; its stored values did not change.

| Choice | `wake_policy` | What happens |
|---|---|---|
| **I keep it running** | `never` | Poundlock forwards the request at once. With a machine, it asks the node to keep the machine awake for it but does not wait for the model. If the server does not answer, the client is told so (`upstream_connection_failed`). |
| **Start the model, don't wake the machine** | `manual` | When the machine is online, its runtime starts the model and the request waits until the model answers. When the machine is asleep or offline, the request fails at once with `runtime_node_offline`. |
| **Wake the machine and start the model** | `on-demand` | Poundlock sends Wake-on-LAN if the machine is asleep, waits for its node, then starts the model and waits for it. |

A model without a machine is always "I keep it running". A new model on a
machine defaults to the third choice when the machine has Wake-on-LAN settings,
and to the second otherwise. Choose the first only when the model is already
loaded whenever the machine is on. With a runtime that loads models (llama.cpp,
a model manager), a request can otherwise reach the server before the model is
ready. With Ollama management, Poundlock always loads the model and waits for it
while the machine is online, whatever the choice. Durable jobs queue instead of
failing: they wait for the machine, and only the third choice wakes it.

## Adding a model

**Models → Add model** asks where the model runs, which model, and what it is
for. Poundlock fills in the rest and says where each value came from; change any
of them under Advanced before adding the model.

- **Model name**, what clients send: `<machine>-<server model ID>` on a machine,
  `<server>-<server model ID>` on a server that is always running, and the route
  name for a gateway route. If it is taken, the runtime is appended (`-llamacpp`),
  then a number. The name cannot change once the model is added.
- **Context**: what the machine's runtime reports for that model. A node agent
  that does not report its model list, or a server without a machine, needs it
  typed from the server's settings.
- **Output limit**: a quarter of the context, at most 32,768.
- **What it is for**: agent jobs and interactive requests.
- **When a request arrives**: as above.
- **Thinking**: Off for a llama.cpp model a machine manages, otherwise the
  model's own default.

The console takes these from `POST /api/v1/model-profiles/draft` (administrator
token) with `endpoint_id` and `model_id`, and optionally `node_id`,
`runtime_profile`, `uses`, `name` and `context_tokens`. It stores nothing and
sends no inference request. Each value has a `source`, and `problems` lists what
would stop the model working: a name already in use, a model the runtime cannot
serve, a context below the 8,192 agent jobs need, or a paused machine. Publishing
a gateway route from Discover uses the same defaults.

## Ollama loading and unloading

Enable the built-in Ollama adapter in the node configuration (`[ollama]` with
`enabled = true`; it is off by default) and select its
reported `ollama` runtime in the Models profile editor. The managed adapter talks to numeric loopback
on that machine, default `http://127.0.0.1:11434`. The configured inference endpoint
must reach that same server.

The profile's idle delay controls models loaded by Poundlock. `0` unloads after
the final request/job releases its lease. Models loaded by another application
are not adopted or unloaded. If another application already has the requested
model loaded, the UI reports that conflict; stop or unload it there before retrying.
Machines → Model memory shows observed residency and available cleanup controls.
The node creates a small local `poundlock-context-*` model definition for each
configured context. It shares the installed model's weights and makes that
context effective through Ollama's OpenAI-compatible API; it is not another
model download.

Custom runtime commands remain an advanced local configuration. They are explicit
argument arrays for ensure, renew, release and stop, not arbitrary controller shell
commands. Timeout or failure stops the command's process group. A successful
ensure that starts a model server leaves that server running; release and stop
must end it. Restrict their filesystem and network permissions to what they need.

## Network and wake interfaces

System routing can use any available network interface. One-interface mode binds
the node's controller connection to the chosen interface; it does not change the
OS routing table. The inference server's listening interface is configured in
that server. A wake-capable interface may differ from the high-speed data interface.

Wake-on-LAN must be supported and enabled in the NIC, firmware and OS. On Linux,
`sudo ethtool INTERFACE` reports support and the current mode; `Wake-on: g` means
magic-packet wake is enabled. Persistence depends on the network manager. Follow
its documentation and test after reboot. Poundlock reports observations and sends
wake packets; it does not change driver, firmware or network-manager settings.

## macOS

Download and verify the matching native archive, then extract it in a temporary
folder. From that folder:

```sh
mkdir -p "$HOME/.local/share/poundlock/bin"
chmod 700 "$HOME/.local/share/poundlock"
install -m 755 poundlock "$HOME/.local/share/poundlock/bin/poundlock"
```

Download a node configuration from Machines, or copy `config/examples/node.toml`
from the Compose bundle. Save it as `~/.local/share/poundlock/node.toml` and set
`controller` to your reachable controller URL and `name` to this machine's name.
Create an enrollment token in Machines, then run:

```sh
"$HOME/.local/share/poundlock/bin/poundlock" node-enroll \
  --config "$HOME/.local/share/poundlock/node.toml" \
  --credential-file "$HOME/.local/share/poundlock/node-credential.json"
```

Paste the token into standard input and press Return, then Control-D. The token
is consumed once. Install `deploy/macos/com.poundlock.node.plist.example` from the
Compose bundle as `~/Library/LaunchAgents/com.poundlock.node.plist`, replacing every
`__PLACEHOLDER__` with an absolute path or the configured user, URL and name:

```sh
mkdir -p "$HOME/Library/LaunchAgents"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.poundlock.node.plist"
```

Confirm Online in Machines and test Generate. Enable “Wake for network access”
where supported. For removal, unload the agent with `launchctl bootout` using the
same domain and plist path, remove the machine in Machines, then delete the plist
and its `~/.local/share/poundlock` directory after preserving any needed settings.

MacOS setup is currently more manual than the Linux installer. Model-runtime
commands vary by server; the example node configuration documents their arguments.
The node releases its sleep inhibitor after work; the OS decides when to sleep.

### Self-update

An idle node updates itself unless `node.toml` sets `auto_update = false`.
Installing a binary is code execution as the node's user, which on macOS is your
own login, so what a node accepts depends on how it was built:

- **Signed builds.** A node built with the release's update key installs only a
  binary named by a manifest signed with that key, for its own platform and a
  newer version. The controller relays the signature but cannot forge one. See
  [Node update signing](../node-update-signing.md).
- **Unsigned builds.** A node built without a key trusts the controller's
  SHA-256 checksum. It refuses automatic updates unless the controller address
  is `https://` or loopback, because on plain HTTP anyone on the network could
  answer for the controller.

Either way the node refuses an older or equal version, verifies the download's
SHA-256, runs it once with `node-check` against its configuration before
committing, keeps the previous binary, and replaces
`~/.local/share/poundlock/bin/poundlock` in place. The node then exits so
launchd's `KeepAlive` restarts it on the new binary. That works only because
launchd (via `ProgramArguments[0]` in the plist above) runs that exact path. The
file the updater writes is not downloaded by the OS or moved across a Gatekeeper
boundary by a browser or `curl`, so no quarantine attribute is attached and no
notarization prompt appears. It is not Apple code-signed.

To go back to the previous binary, set `auto_update = false` in `node.toml` first,
or the node installs the newer one again within five minutes. Then, while the node
is idle:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.poundlock.node.plist"
mv "$HOME/.local/share/poundlock/bin/poundlock.previous" "$HOME/.local/share/poundlock/bin/poundlock"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.poundlock.node.plist"
```

### One-time migration for an existing machine

An earlier manual setup may have installed the binary at `~/.local/bin/poundlock`
instead, with launchd's `ProgramArguments[0]` pointing there. Self-update will not
help that installation, because the updater always writes to
`~/.local/share/poundlock/bin/poundlock`, a path launchd never runs. Migrate it
once, while the node is idle:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.poundlock.node.plist"
mkdir -p "$HOME/.local/share/poundlock/bin"
mv "$HOME/.local/bin/poundlock" "$HOME/.local/share/poundlock/bin/poundlock"
ln -sf "$HOME/.local/share/poundlock/bin/poundlock" "$HOME/.local/bin/poundlock"
```

The symlink keeps `~/.local/bin/poundlock` working for interactive use (for
example `node-enroll` or `doctor`) without touching `$PATH`. Edit
`~/Library/LaunchAgents/com.poundlock.node.plist` so `ProgramArguments[0]` reads
`/Users/<user>/.local/share/poundlock/bin/poundlock` — the current
`deploy/macos/com.poundlock.node.plist.example` already uses that path, so diff
against a freshly filled-in copy if unsure. Add `auto_update = true` to
`node.toml` to make the intent explicit (a configuration without the key already
updates itself). Reload the agent:

```sh
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.poundlock.node.plist"
```

Confirm Online in Machines. Do not run this migration while the node holds
active work.
