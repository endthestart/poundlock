# Install a Linux node

The guide is also available directly in **Machines → Linux installation guide**.

Install the node before choosing a model server. It registers the machine,
reports hardware, software and network details, handles wake/readiness actions,
and keeps the machine awake during leased work. Drivers and models can be
installed afterward using the upstream links in Machines → Model server setup
help. Existing model servers can be connected directly.

## Requirements

- Linux with systemd and glibc 2.36 or newer (for example Debian 12, Ubuntu 24.04,
  or a current CachyOS installation). Check with `systemctl --version` and
  `getconf GNU_LIBC_VERSION`.
- A published binary matching `uname -m`: x86_64 or aarch64. Machines reports
  an unavailable architecture before issuing its installation command.
- An existing non-root login with sudo, Bash, coreutils, util-linux,
  systemd-logind, and polkit. Use your distribution packages if any are missing.
- Access from this machine to the controller URL shown in Machines. Remote
  installations should use HTTPS or a trusted private network. The controller
  separately needs to reach the model server's configured endpoint.

## Installation

1. Open Machines. Enter a unique machine name, select Linux and its processor.
2. Choose System routing (any available network interface), or One
   interface. For a specific interface, run `poundlock node-interfaces` after
   downloading and making the binary executable to find the interface name.
3. Download the binary, `node.toml` and `poundlock-node.sh` into the same folder
   on the Linux machine, for example `~/Downloads/poundlock`. Any folder you own
   works; if downloading elsewhere, copy all three files to Linux first. The
   installer places files in their permanent locations. Keep the installer for
   diagnostics, upgrades and removal. The configuration contains the controller URL and name.
   The default `always-ready` runtime assumes your model server already runs.
4. Create a token and run the displayed `sudo bash ... install` command. It
   verifies the binary checksum and configuration, installs a systemd service
   under the login that invoked sudo, and prompts for the token with input hidden.
   `--user LOGIN` is an optional override; root cannot be the service account. The token is
   single-use and expires in 15 minutes; create another if it has expired.
5. Confirm Online in Machines and review **Technical details**. Enrollment
   reports inventory immediately; the service refreshes it every five minutes.
   Use **Model server setup help** for relevant upstream installation links.
   When the server is running, choose **Connect a model server**. Add the model server
   endpoint in Connections and a model profile in Models, using this machine and runtime
   `always-ready`. Use Generate to verify actual inference. A model listing or
   an Online node is insufficient on its own.
6. If the machine can sleep, configure its Wake-on-LAN adapter and test from
   real sleep. Do not probe with SSH during that test; SSH can wake some hosts.

The installer creates `/etc/poundlock/node.toml`, a private
`/var/lib/poundlock` directory, `poundlock-node.service`, and one polkit rule
allowing the selected login to inhibit sleep. It does not change routes, network
interfaces, firewall rules, suspend settings, model files or model services.

The service runs without root privileges. Runtime command arrays in node.toml
are advanced configuration: use absolute executable paths, no shell strings.
Their writable state must fit the service's `/var/lib/poundlock` access, or an
operator must explicitly add a narrow `ReadWritePaths` systemd override. Keep
configuration free of passwords and API keys: it is readable by the service;
node credentials are stored separately with mode 0600.

## Diagnose or repair

Run `sudo bash poundlock-node.sh doctor`. It checks configuration, the selected
connection, authenticated controller access, and sleep inhibition. Inspect
`journalctl -u poundlock-node.service --since '10 minutes ago'` for service logs.
The diagnostic does not advertise a heartbeat or execute runtime commands.

- Missing executable or unknown configuration key: repair node.toml and run
  `/var/lib/poundlock/poundlock node-check --config /etc/poundlock/node.toml`.
- Authentication failure: verify you are using the same controller. To enroll
  again, stop the service while idle, preserve then remove the old credential
  file as its owner, create a new token, and repeat install. Reusing the same
  machine name rotates its credential. Never enroll over a machine doing work.
- Connection failure: check the controller URL and the chosen interface. The
  node does not switch from One interface to another connection automatically.
- Plain HTTP refused: a controller address on another machine must be
  `https://`. To accept plain `http://` on a network you trust, add
  `allow_insecure_http = true` to `node.toml` and run node-check again.
- Sleep inhibitor denied: check systemd-logind, polkit, and
  `/etc/polkit-1/rules.d/60-poundlock-node.rules`. Run doctor under the service
  account to test its permission. Do not disable sleep system-wide as a workaround.
- Interrupted install: correct the reported problem and repeat the command.
  An existing managed install keeps its credential. Unmanaged paths and symlinks
  are refused; migrate an earlier manual install while idle by stopping its
  service and preserving its files before moving them out of the installer paths.

## Upgrade and rollback

New downloaded configurations use explicit upgrades (`auto_update = false`). In
Machines, select Linux and the installed processor type in Enroll a machine.
Download the node binary and Linux installer. The binary download displays the
complete upgrade command with the published SHA-256; run it in the download folder.
No new token or configuration download is needed. Its form is:

    sudo bash poundlock-node.sh upgrade --binary ./poundlock-linux-x86_64 --sha256 HASH

The controller atomically pauses an idle node before replacement. Active work
blocks maintenance; wait for it to finish or cancel it first. Queued work waits
while maintenance is paused. The installer validates the staged binary, keeps
`poundlock.previous`, replaces the binary, restarts and resumes the node.

    sudo bash poundlock-node.sh rollback
    sudo bash poundlock-node.sh resume

Rollback uses the previous binary even if the new binary cannot start. Resume
recovers an interrupted maintenance operation after its cause has been fixed.
Neither operation changes the controller database or model files. Retain the
installer alongside the binary for recovery. Re-check Generate after an upgrade.

Configurations without `auto_update` keep automatic idle updates; set
`auto_update = false` to use explicit upgrades. A node built with the
release's update key installs only a signed, newer binary for its platform. A
node built without one refuses automatic updates unless the controller address
is `https://` or loopback. Both verify SHA-256, refuse an older or equal version,
run configuration preflight before replacement and retain a previous binary.
See [Node update signing](node-update-signing.md).

## How the node polls

The node asks the controller for work with a request the controller holds open
for up to `heartbeat_seconds` (never more than 20 seconds) and answers the moment
work is queued, so a model request does not wait for the node's next poll. A
reverse proxy in front of the controller must let that request stay open for
that long; if it cuts polls short, the node logs failed polls and waits its
interval between them. Heartbeats, model upkeep and sleep-inhibitor expiry keep running
about once per `heartbeat_seconds` while a poll is held. A node that does this
reports `action_long_poll` in Technical details. Against a controller that
predates it, the node polls every `heartbeat_seconds`, as before.

## Remove

Finish or cancel active work, then run:

    sudo bash poundlock-node.sh remove

This revokes the node credential, disables the service, and removes the node's
configuration, binaries, private credential, and polkit rule. The controller
retains the Removed machine and historical job evidence. New use requires a new
enrollment token. Model files, GPU drivers, and model services remain installed.
If the controller is unreachable, restore connectivity before removing the
credential so that revocation can be confirmed.

For a machine already wiped or no longer available, use **Remove machine** in
Machines. Operator removal revokes its credential and keeps job history. It
cannot uninstall files remotely and refuses while work is active or a recent
runtime reservation has not been released. Offline alone does not mean idle.
Abandoned reservations stop blocking removal after the maximum node lease and
in-flight action windows have elapsed, provided no current job or inference
request still owns the machine. Removed
registrations are shown under **Removed machines**; a new installation requires
a new enrollment token. A still-online or paused name cannot be claimed with a
new token; wait until it is offline, or remove it first. The operator API is
`DELETE /api/v1/nodes/{id}` (401
without operator access, 409 for active work, 404 for an unknown machine).

## Clean-machine acceptance

On a Linux machine that has never had this node installed:

- Complete the installation using only Machines and this guide.
- Confirm Online, the expected network connection, and Generate success.
- Submit a small job in the UI and inspect its patch and verification evidence.
- Restart the computer; confirm the service reconnects without a token prompt.
- Test sleep, wake, inference, and cancellation if supported by the hardware.
- Upgrade, roll back, remove, confirm Removed, and enroll again with a new token.

Record any unclear step or unexplained failure. Automated fixtures do not replace
this physical installation and usability acceptance.

## Inventory and setup guidance

The node reports OS/version/kernel, CPU, memory, its data filesystem capacity,
GPU names or PCI IDs, available driver details, known software locations, and
network interfaces with MACs, addresses, broadcast addresses and available
driver/MTU details. Fields unavailable to the service account remain unknown.
GPU names may require the optional system `lspci` utility on Linux; PCI IDs
remain available without it.

The timestamped inventory is stored with the node and remains available while
it is offline, after controller restart, and after a rollback to an older node
that does not report inventory. A new inventory replaces the previous snapshot;
this is the last reported state, not a historical monitoring database. Network
information updates with heartbeats. Slower hardware probes run separately so
they cannot hold up heartbeats or lease handling.

Technical details can be downloaded as JSON from Machines. Collection uses
allowlisted system metadata and bounded read-only diagnostic commands. It does
not start inference servers, inspect model files or containers, scan the
network, or collect credentials, environment dumps, hardware serials or
personal documents. Standard application locations are checked for presence;
custom paths and container installations may not be detected. Presence does
not establish server health or model readiness. `node-doctor` includes the
same inventory locally.

Model-server and driver links are selected from the reported OS, architecture
and GPU vendor. They point to upstream installation and compatibility guidance;
they are not claims that every linked combination has passed Poundlock testing.
The catalog includes Ollama, LM Studio, llama.cpp, vLLM, SGLang, oMLX for Apple
silicon and ToshLLM for Intel Macs with AMD graphics. New servers can also use
the existing compatible endpoint path without a dedicated integration.

The default runtime assumes the model server is already running. An adapter
may invoke locally configured lifecycle commands, but enrollment does not
install one or take over server start/stop. Timeout or failure stops that
command's process group; a successful start can leave a server running until
release or stop. Confirm API reachability, actual
generation and any needed tool-calling behavior before using unattended jobs.
For GUI servers, also check startup after logout/reboot.

Wake setup lists the reported interfaces and fills their MAC and subnet
broadcast address when available. Selecting an adapter does not change routing
or save wake policy. Save the settings explicitly; **Wake test** uses the saved
settings independently of any unsaved edits. Manual overrides remain available.
