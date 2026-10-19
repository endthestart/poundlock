//! The supervisor's small adapter to the standard Docker Engine API. Job input
//! never supplies Docker options, image names, mount paths or resource labels.
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const OWNER_LABEL: &str = "net.poundlock.supervisor";
const ATTEMPT_LABEL: &str = "net.poundlock.attempt";
const STORAGE_LABEL: &str = "net.poundlock.storage";
const API: &str = "http://docker/v1.48";

#[derive(Clone)]
pub(super) struct Docker {
    client: Client,
    pub supervisor: String,
    pub image: String,
    owner: String,
    storage: String,
    /// The attempt networks with outbound access, which the egress proxy serves.
    pub egress: super::egress::Egress,
    /// The Docker runtime job containers run under, when not the default (L16).
    runtime: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct ExecOutput {
    pub code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Tracked per stream. One shared flag meant a noisy stderr reported the
    /// agent's own transcript as cut off, which is a different failure with a
    /// different meaning: only a truncated *stdout* loses agent evidence.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Number of Pi `message_update` streaming events dropped from `stdout` by
    /// the agent-only filter in `frames`. Zero unless that filter ran.
    pub omitted_stream_events: usize,
    /// Pi events whose repeated copies of messages and tool results the same
    /// filter removed (see `compact_event`). Zero unless that filter ran.
    pub compacted_events: usize,
}

impl ExecOutput {
    /// Either stream lost bytes. Callers that only care whether *this exec's*
    /// output is complete — candidate inspection, a verification command —
    /// want this; callers classifying the agent's own run want
    /// `stdout_truncated`.
    pub fn truncated(&self) -> bool {
        self.stdout_truncated || self.stderr_truncated
    }
}

pub(super) type Capture = std::sync::Arc<std::sync::Mutex<ExecOutput>>;

pub(super) fn captured(capture: &Capture) -> ExecOutput {
    capture.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

impl Docker {
    pub async fn connect(
        socket: &Path,
        supervisor: &str,
        owner: String,
        results: &Path,
        runtime: Option<String>,
    ) -> Result<Self> {
        ensure!(
            !supervisor.is_empty(),
            "runner must run inside its Docker supervisor container"
        );
        let client = Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let mut docker = Self {
            client,
            supervisor: supervisor.into(),
            image: String::new(),
            owner,
            storage: String::new(),
            egress: Default::default(),
            runtime,
        };
        let version = docker.json(Method::GET, "/version", None).await?;
        ensure!(
            version["Os"] == "linux",
            "workers require a Linux Docker engine"
        );
        let major = version["Version"]
            .as_str()
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        ensure!(
            major >= 28,
            "workers require Docker Engine 28 or newer for isolated networking"
        );
        let own = docker
            .json(Method::GET, &format!("/containers/{supervisor}/json"), None)
            .await?;
        docker.supervisor = own["Id"]
            .as_str()
            .context("supervisor container ID missing")?
            .into();
        docker.image = own["Image"]
            .as_str()
            .context("supervisor image ID missing")?
            .into();
        docker.storage = storage_scope(&own, results)?;
        ensure!(
            docker.image.starts_with("sha256:"),
            "worker image must be immutable"
        );
        if let Some(runtime) = &docker.runtime {
            let info = docker.json(Method::GET, "/info", None).await?;
            ensure!(
                info["Runtimes"].get(runtime).is_some(),
                "POUNDLOCK_WORKER_RUNTIME names {runtime}, which this Docker engine does not have"
            );
        }
        Ok(docker)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(method, format!("{API}{path}"))
    }

    async fn json(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = self.request(method, path).timeout(Duration::from_secs(15));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(
            status.is_success(),
            "Docker {path} returned {status}: {}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(2048)])
        );
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }

    fn labels(&self, attempt: &str) -> Value {
        json!({OWNER_LABEL:self.owner, STORAGE_LABEL:self.storage, ATTEMPT_LABEL:attempt})
    }

    pub async fn volume(&self, name: &str, attempt: &str) -> Result<()> {
        self.json(Method::POST, "/volumes/create", Some(json!({
            "Name":name, "Driver":"local", "Labels":self.labels(attempt),
            "DriverOpts":{"type":"tmpfs","device":"tmpfs","o":crate::verifier_toolchain::work_volume_tmpfs_opts()}
        }))).await?;
        Ok(())
    }

    /// Every attempt network is internal. With `outbound`, its containers reach
    /// the internet only through the supervisor's egress proxy, which refuses
    /// the operator's network (M5).
    pub async fn network(&self, name: &str, attempt: &str, outbound: bool) -> Result<()> {
        self.internal_network(name, attempt, outbound.then_some(None))
            .await
    }

    /// A network whose containers reach only `host` on port 443, through the
    /// egress proxy: the Git fetch container's (L13).
    pub async fn fetch_network(&self, name: &str, attempt: &str, host: &str) -> Result<()> {
        self.internal_network(name, attempt, Some(Some(host))).await
    }

    /// `egress` is `None` for no way out, `Some(None)` for the public internet
    /// and `Some(Some(host))` for one host.
    async fn internal_network(
        &self,
        name: &str,
        attempt: &str,
        egress: Option<Option<&str>>,
    ) -> Result<()> {
        self.json(
            Method::POST,
            "/networks/create",
            Some(json!({
                "Name":name, "Driver":"bridge", "Internal":true, "EnableIPv6":false,
                "Labels":self.labels(attempt),
                "Options":{"com.docker.network.bridge.gateway_mode_ipv4":"isolated"}
            })),
        )
        .await?;
        if let Some(only) = egress {
            let details = self
                .json(Method::GET, &format!("/networks/{name}"), None)
                .await?;
            let subnet = details["IPAM"]["Config"][0]["Subnet"]
                .as_str()
                .context("the attempt network has no subnet")?;
            match only {
                Some(host) => self.egress.allow_only(name, subnet, host)?,
                None => self.egress.allow(name, subnet)?,
            }
        }
        self.json(
            Method::POST,
            &format!("/networks/{name}/connect"),
            Some(json!({
                "Container":self.supervisor,"EndpointConfig":{"Aliases":["poundlock-supervisor"]}
            })),
        )
        .await?;
        Ok(())
    }

    pub async fn create(
        &self,
        name: &str,
        attempt: &str,
        network: &str,
        volumes: &[(&str, &str, bool)],
        wall_seconds: u64,
    ) -> Result<()> {
        let mut env = vec![format!("PATH={}", crate::verifier_toolchain::WORKER_PATH)];
        if self.egress.outbound(network) {
            env.extend(super::egress::proxy_environment());
        }
        let mut tmpfs = json!({"/tmp":crate::verifier_toolchain::tmp_tmpfs_opts()});
        if !volumes
            .iter()
            .any(|(_, target, _)| *target == "/home/worker")
        {
            tmpfs["/home/worker"] = json!(crate::verifier_toolchain::home_tmpfs_opts());
        }
        let mounts: Vec<Value> = volumes
            .iter()
            .map(|(source, target, readonly)| {
                json!({
                    "Type":"volume","Source":source,"Target":target,"ReadOnly":readonly,
                    "VolumeOptions":{"NoCopy":true}
                })
            })
            .collect();
        let mut config = json!({
                "Image":self.image,"User":"10002:10002","Labels":self.labels(attempt),
                "Entrypoint":["/bin/sleep"],"Cmd":[wall_seconds.max(1).to_string()],
                "WorkingDir":"/","Env":env,
                "HostConfig":{"NetworkMode":network,"ReadonlyRootfs":true,"CapDrop":["ALL"],
                    "SecurityOpt":["no-new-privileges:true"],"PidsLimit":256,
                    // An init reaps what exec'd commands orphan; no core dumps of job memory (L16).
                    "Init":true,
                    "Ulimits":[{"Name":"nofile","Soft":65536,"Hard":65536},{"Name":"core","Soft":0,"Hard":0}],
                    "Memory":4294967296_u64,"MemorySwap":4294967296_u64,"NanoCpus":2000000000_u64,
                    "Tmpfs":tmpfs,
                    "Mounts":mounts,"LogConfig":{"Type":"none"},"RestartPolicy":{"Name":"no"}}
        });
        if let Some(runtime) = &self.runtime {
            config["HostConfig"]["Runtime"] = json!(runtime);
        }
        self.json(
            Method::POST,
            &format!("/containers/create?name={name}"),
            Some(config),
        )
        .await?;
        self.json(Method::POST, &format!("/containers/{name}/start"), None)
            .await?;
        Ok(())
    }

    pub async fn upload(&self, container: &str, target: &str, archive: Vec<u8>) -> Result<()> {
        let response = self
            .request(Method::PUT, &format!("/containers/{container}/archive"))
            .query(&[("path", target), ("noOverwriteDirNonDir", "true")])
            .timeout(Duration::from_secs(60))
            .header("Content-Type", "application/x-tar")
            .body(archive)
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "workspace upload failed: {}",
            response.status()
        );
        Ok(())
    }

    /// Run `command` in `container`, capturing at most `limit` bytes per stream
    /// into `capture`. When `filter_agent_stream_events` is set, `stdout` has
    /// Pi `message_update` streaming-delta lines dropped before they count
    /// against `limit` (see `frames`); `stderr` and every other caller are
    /// unaffected, so pass `true` only for the Pi agent's own exec.
    pub async fn exec(
        &self,
        container: &str,
        command: &std::process::Command,
        limit: usize,
        capture: &Capture,
        filter_agent_stream_events: bool,
    ) -> Result<ExecOutput> {
        let args: Vec<String> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|v| {
                v.to_str()
                    .context("worker command is not UTF-8")
                    .map(String::from)
            })
            .collect::<Result<_>>()?;
        let env: Vec<String> = command
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            })
            .collect();
        let created = self
            .json(
                Method::POST,
                &format!("/containers/{container}/exec"),
                Some(json!({
                    "AttachStdout":true,"AttachStderr":true,"AttachStdin":false,"Tty":false,
                    "User":"10001:10001","Privileged":false,"Cmd":args,"Env":env,
                    "WorkingDir":command.get_current_dir().and_then(Path::to_str).unwrap_or("/work")
                })),
            )
            .await?;
        let id = created["Id"].as_str().context("Docker exec ID missing")?;
        let response = self
            .request(Method::POST, &format!("/exec/{id}/start"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "tcp")
            .json(&json!({"Detach":false,"Tty":false}))
            .send()
            .await?;
        ensure!(
            response.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS,
            "Docker exec did not establish its output stream: {}",
            response.status()
        );
        frames(
            response.upgrade().await?,
            limit,
            capture,
            filter_agent_stream_events,
        )
        .await?;
        let status = self
            .json(Method::GET, &format!("/exec/{id}/json"), None)
            .await?;
        ensure!(
            status["Running"] == false,
            "Docker exec output ended while process was still running"
        );
        let mut output = captured(capture);
        output.code = status["ExitCode"]
            .as_i64()
            .context("Docker exec exit status missing")?;
        Ok(output)
    }

    /// Remove one volume; a volume Docker still holds is left for the attempt's cleanup.
    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        self.json(Method::DELETE, &format!("/volumes/{name}"), None)
            .await
            .map(|_| ())
    }

    pub async fn remove_container(&self, name: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let response = self
                    .request(
                        Method::DELETE,
                        &format!("/containers/{name}?force=true&v=true"),
                    )
                    .send()
                    .await?;
                let status = response.status();
                if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(());
                }
                ensure!(
                    status == reqwest::StatusCode::CONFLICT,
                    "worker removal failed: {status}"
                );
                // Docker can still be removing a worker after an earlier delete.
                // Require confirmed removal before inspecting its candidate or
                // removing volumes; a conflict alone never establishes cleanup.
                drop(response);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .context("worker removal was not confirmed within 15 seconds")?
    }

    /// Filter by an installation identity stored outside worker files. Never use
    /// global prune, caller-supplied selectors, or names belonging to other apps.
    pub async fn cleanup(&self, attempt: Option<&str>) -> Result<()> {
        let mut labels = vec![
            format!("{OWNER_LABEL}={}", self.owner),
            format!("{STORAGE_LABEL}={}", self.storage),
        ];
        if let Some(attempt) = attempt {
            labels.push(format!("{ATTEMPT_LABEL}={attempt}"));
        }
        let filters = json!({"label":labels}).to_string();
        self.remove_containers(&filters).await?;
        let networks = self
            .request(Method::GET, "/networks")
            .query(&[("filters", &filters)])
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Value>>()
            .await?;
        for network in networks {
            let id = network["Id"].as_str().context("network ID missing")?;
            let details = self
                .json(Method::GET, &format!("/networks/{id}"), None)
                .await?;
            if let Some(containers) = details["Containers"].as_object() {
                for member in containers.keys() {
                    // Only the current/previous supervisor remains after owned
                    // workers were removed. Refuse to detach unrelated containers.
                    let own = self
                        .json(Method::GET, &format!("/containers/{member}/json"), None)
                        .await?;
                    ensure!(
                        member == &self.supervisor
                            || own["Config"]["Labels"][OWNER_LABEL] == self.owner,
                        "an unrelated container is attached to a worker network; operator recovery required"
                    );
                    self.json(
                        Method::POST,
                        &format!("/networks/{id}/disconnect"),
                        Some(json!({"Container":member,"Force":true})),
                    )
                    .await?;
                }
            }
            self.json(Method::DELETE, &format!("/networks/{id}"), None)
                .await?;
            if let Some(name) = details["Name"].as_str() {
                self.egress.forget(name);
            }
        }
        self.remove_volumes(&filters).await
    }

    /// List then force-remove every container matching `filters`, waiting for
    /// each removal to be confirmed. Reused to retry a volume conflict caused by
    /// a holder that outlives its own container's confirmed removal (see
    /// `remove_volumes`), so it must stay scoped to the caller's label filter.
    async fn remove_containers(&self, filters: &str) -> Result<()> {
        let list = self.list_containers(filters).await?;
        for container in list {
            self.remove_container(container["Id"].as_str().context("container ID missing")?)
                .await?;
        }
        Ok(())
    }

    async fn list_containers(&self, filters: &str) -> Result<Vec<Value>> {
        Ok(self
            .request(Method::GET, "/containers/json")
            .query(&[("all", "true"), ("filters", filters)])
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<Value>>()
            .await?)
    }

    /// Docker can report a container's removal as confirmed (a 404 on inspect)
    /// before the volume driver has released that container's reference, so a
    /// volume delete right afterwards can still see a transient 409 "volume is
    /// in use". Re-listing and removing any holder that matches this
    /// installation's own label filter (never a caller-supplied selector) and
    /// retrying clears that race without ever touching unrelated resources.
    async fn remove_volumes(&self, filters: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let volumes = self
                    .request(Method::GET, "/volumes")
                    .query(&[("filters", filters)])
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await?;
                let names = volumes["Volumes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|volume| {
                        volume["Name"]
                            .as_str()
                            .map(String::from)
                            .context("volume name missing")
                    })
                    .collect::<Result<Vec<_>>>()?;
                if names.is_empty() {
                    return Ok(());
                }
                let mut conflict = false;
                for name in names {
                    let response = self
                        .request(Method::DELETE, &format!("/volumes/{name}"))
                        .send()
                        .await?;
                    let status = response.status();
                    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                        continue;
                    }
                    ensure!(
                        status == reqwest::StatusCode::CONFLICT,
                        "volume removal failed: {status}"
                    );
                    conflict = true;
                }
                if !conflict {
                    return Ok(());
                }
                self.remove_containers(filters).await?;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .context("volume removal was not confirmed within 30 seconds")?
    }
}

async fn frames(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    capture: &Capture,
    filter_stdout_stream_events: bool,
) -> Result<()> {
    let mut buffer = [0u8; 8192];
    // Only used when `filter_stdout_stream_events` is set: stdout bytes seen so
    // far that do not yet form a complete line, buffered across Docker frame
    // boundaries (a frame can split a JSON event anywhere, including mid-line).
    let mut pending_stdout_line: Vec<u8> = Vec::new();
    // Set once the current line has already grown past `limit` on its own, so
    // the rest of it is discarded without buffering: it can never fit, and this
    // bounds memory regardless of how long an unfiltered line runs.
    let mut discarding_oversized_line = false;
    loop {
        let mut header = [0u8; 8];
        if reader.read(&mut header[..1]).await? == 0 {
            break;
        }
        reader.read_exact(&mut header[1..]).await?;
        ensure!(
            header[1..4] == [0, 0, 0] && matches!(header[0], 1 | 2),
            "invalid Docker output frame"
        );
        let is_stdout = header[0] == 1;
        let mut remaining = u32::from_be_bytes(header[4..8].try_into()?) as usize;
        while remaining > 0 {
            let count = remaining.min(buffer.len());
            reader.read_exact(&mut buffer[..count]).await?;
            if filter_stdout_stream_events && is_stdout {
                feed_stdout_line_filter(
                    &buffer[..count],
                    &mut pending_stdout_line,
                    &mut discarding_oversized_line,
                    capture,
                    limit,
                );
            } else {
                let mut output = capture.lock().unwrap_or_else(|p| p.into_inner());
                let target = if is_stdout {
                    &mut output.stdout
                } else {
                    &mut output.stderr
                };
                let retained = count.min(limit.saturating_sub(target.len()));
                target.extend_from_slice(&buffer[..retained]);
                let lost = retained != count;
                if is_stdout {
                    output.stdout_truncated |= lost;
                } else {
                    output.stderr_truncated |= lost;
                }
            }
            remaining -= count;
        }
    }
    if filter_stdout_stream_events {
        // The stream ended without a trailing newline on whatever is left; emit
        // it like any other line rather than silently dropping it.
        emit_stdout_line(&pending_stdout_line, capture, limit);
        append_omitted_stream_events_summary(capture, limit);
    }
    Ok(())
}

/// Feed newly read stdout bytes through the line-buffered `message_update`
/// filter used for the Pi agent's own exec (see `Docker::exec`).
fn feed_stdout_line_filter(
    mut chunk: &[u8],
    pending: &mut Vec<u8>,
    discarding_oversized_line: &mut bool,
    capture: &Capture,
    limit: usize,
) {
    loop {
        if *discarding_oversized_line {
            match chunk.iter().position(|&b| b == b'\n') {
                Some(index) => {
                    *discarding_oversized_line = false;
                    chunk = &chunk[index + 1..];
                }
                None => return,
            }
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(index) => {
                pending.extend_from_slice(&chunk[..=index]);
                chunk = &chunk[index + 1..];
                emit_stdout_line(pending, capture, limit);
                pending.clear();
            }
            None => {
                pending.extend_from_slice(chunk);
                if pending.len() > limit {
                    // This line alone already exceeds the whole budget: it can
                    // never fit, so retain what fits as truncated evidence now
                    // and stop buffering the remainder of it.
                    write_stdout_bytes(pending, capture, limit);
                    pending.clear();
                    *discarding_oversized_line = true;
                }
                return;
            }
        }
    }
}

/// Drop `line` if it is a Pi `message_update` streaming event, compact it if
/// it repeats a message or tool result kept elsewhere, and otherwise retain it
/// byte-exact.
///
/// Pi's JSON stream repeats each tool result up to five times: in
/// `tool_execution_end`, the tool-result message's `message_start` and
/// `message_end`, `turn_end.toolResults`, and every message again in
/// `agent_end.messages`. A few large reads filled the output budget with
/// copies. The kept copies are `tool_execution_end` (the result next to the
/// tool's name) and each assistant `message_end` (Pi's authoritative final
/// message). The rest keep the fields Poundlock reads (`type`, and
/// `agent_end.willRetry`) and lose the repeats. `message_update` only ever
/// wraps an incremental piece of a message that `message_end` completes.
fn emit_stdout_line(line: &[u8], capture: &Capture, limit: usize) {
    let event = serde_json::from_slice::<Value>(line).ok();
    match event.as_ref().map(|event| (event["type"].as_str(), event)) {
        Some((Some("message_update"), _)) => {
            capture
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .omitted_stream_events += 1;
        }
        Some((_, event)) => match compact_event(event) {
            Some(compacted) => {
                capture
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .compacted_events += 1;
                if let Some(mut bytes) = compacted {
                    bytes.push(b'\n');
                    write_stdout_bytes(&bytes, capture, limit);
                }
            }
            None => write_stdout_bytes(line, capture, limit),
        },
        None => write_stdout_bytes(line, capture, limit),
    }
}

/// `None` keeps the event as it is; `Some(None)` drops it; `Some(Some(bytes))`
/// replaces it with a shorter event.
fn compact_event(event: &Value) -> Option<Option<Vec<u8>>> {
    let without = |keys: &[&str]| {
        let mut event = event.clone();
        let object = event.as_object_mut()?;
        let mut removed = false;
        for key in keys {
            removed |= object.remove(*key).is_some();
        }
        removed.then(|| serde_json::to_vec(&event).ok()).flatten()
    };
    match event["type"].as_str()? {
        "message_start" => Some(None),
        "message_end" if event["message"]["role"] == "toolResult" => Some(None),
        "turn_end" => without(&["message", "toolResults"]).map(Some),
        "agent_end" => without(&["messages"]).map(Some),
        _ => None,
    }
}

fn write_stdout_bytes(bytes: &[u8], capture: &Capture, limit: usize) {
    let mut output = capture.lock().unwrap_or_else(|p| p.into_inner());
    let retained = bytes.len().min(limit.saturating_sub(output.stdout.len()));
    output.stdout.extend_from_slice(&bytes[..retained]);
    output.stdout_truncated |= retained != bytes.len();
}

/// Record, as one trailing stdout line, how many `message_update` events the
/// filter dropped, so reviewers reading retained output know filtering
/// happened. Best-effort: if it does not fit in what remains of `limit`, it is
/// silently skipped rather than displacing real agent evidence or forcing a
/// truncation that would not otherwise have happened.
fn append_omitted_stream_events_summary(capture: &Capture, limit: usize) {
    let (omitted, compacted) = {
        let output = capture.lock().unwrap_or_else(|p| p.into_inner());
        (output.omitted_stream_events, output.compacted_events)
    };
    if omitted == 0 && compacted == 0 {
        return;
    }
    let mut summary = String::from("[poundlock] ");
    if omitted > 0 {
        summary.push_str(&format!(
            "omitted {omitted} streaming message_update event(s) from retained output"
        ));
    }
    if compacted > 0 {
        if omitted > 0 {
            summary.push_str("; ");
        }
        summary.push_str(&format!(
            "removed repeated messages and tool results from {compacted} event(s)"
        ));
    }
    summary.push_str("; see message_end events for the complete assistant output\n");
    let mut output = capture.lock().unwrap_or_else(|p| p.into_inner());
    let remaining = limit.saturating_sub(output.stdout.len());
    if remaining >= summary.len() {
        output.stdout.extend_from_slice(summary.as_bytes());
    }
}

pub(super) async fn archive(directory: &Path) -> Result<Vec<u8>> {
    // Only supervisor-prepared files enter tar here. Never unpack worker archives
    // on the supervisor, and never follow symlinks while packing source trees.
    let mut child = tokio::process::Command::new("tar")
        .args([
            "--owner=10001",
            "--group=10001",
            "--numeric-owner",
            "-cf",
            "-",
            "-C",
        ])
        .arg(directory)
        .arg(".")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("archive output missing")?
        .take(256 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > 256 * 1024 * 1024 {
        child.kill().await?;
        bail!("prepared workspace exceeds 256 MiB transfer limit");
    }
    if !child.wait().await?.success() {
        bail!("could not prepare workspace archive");
    }
    Ok(bytes)
}

fn storage_scope(container: &Value, results: &Path) -> Result<String> {
    ensure!(
        results.is_absolute(),
        "runner result directory must be absolute"
    );
    let mounts = container["Mounts"].as_array();
    let backing = mounts
        .into_iter()
        .flatten()
        .filter_map(|mount| {
            let destination = Path::new(mount["Destination"].as_str()?);
            let relative = results.strip_prefix(destination).ok()?;
            let source = mount["Name"]
                .as_str()
                .or_else(|| mount["Source"].as_str())?;
            Some((
                destination.as_os_str().len(),
                format!("{source}/{}", relative.display()),
            ))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, source)| source);
    // A restored copy on a different volume must never clean up the original
    // installation's active workers, even when its supervisor UUID was copied.
    let scope = backing.unwrap_or_else(|| {
        format!(
            "{}/{}",
            container["Id"].as_str().unwrap_or_default(),
            results.display()
        )
    });
    Ok(crate::crypto::token_hash(&scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_volumes_and_tmpfs_request_exec() {
        let volume = json!({
            "DriverOpts":{"type":"tmpfs","device":"tmpfs","o":crate::verifier_toolchain::work_volume_tmpfs_opts()}
        });
        let opts = volume["DriverOpts"]["o"].as_str().unwrap();
        assert!(opts.split(',').any(|part| part == "exec"));
        let tmpfs = json!({
            "/tmp": crate::verifier_toolchain::tmp_tmpfs_opts(),
            "/home/worker": crate::verifier_toolchain::home_tmpfs_opts(),
        });
        for path in ["/tmp", "/home/worker"] {
            let mount = tmpfs[path].as_str().unwrap();
            assert!(
                mount.split(',').any(|part| part == "exec"),
                "{path}: {mount}"
            );
            assert!(
                !mount.split(',').any(|part| part == "noexec"),
                "{path}: {mount}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn container_removal_requires_confirmation_and_bounds_conflict_retries() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::AsyncWriteExt;

        for (statuses, expected_ok, minimum_requests) in [
            (vec![409, 409, 204], true, 3),
            (vec![409, 404], true, 2),
            (vec![403], false, 1),
            (vec![409], false, 2),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let count = requests.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                    }
                    assert!(request.starts_with(
                        b"DELETE /v1.48/containers/owned-worker?force=true&v=true HTTP/1.1\r\n"
                    ));
                    let index = count.fetch_add(1, Ordering::SeqCst);
                    let status = statuses[index.min(statuses.len() - 1)];
                    stream.write_all(format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ).as_bytes()).await.unwrap();
                }
            });
            let docker = Docker {
                client: Client::builder()
                    .unix_socket(socket)
                    .no_proxy()
                    .build()
                    .unwrap(),
                supervisor: String::new(),
                image: String::new(),
                owner: String::new(),
                storage: String::new(),
                egress: Default::default(),
                runtime: None,
            };
            let result = docker.remove_container("owned-worker").await;
            server.abort();
            assert_eq!(result.is_ok(), expected_ok, "{result:?}");
            assert!(requests.load(Ordering::SeqCst) >= minimum_requests);
            if minimum_requests == 1 {
                assert_eq!(
                    requests.load(Ordering::SeqCst),
                    1,
                    "non-conflicts must fail immediately"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn volume_conflict_during_cleanup_removes_the_holder_and_retries() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::AsyncWriteExt;

        // A container removal can be confirmed (a 404 on later inspect) before
        // Docker's volume driver has released its reference, so a volume delete
        // right afterwards can still see a transient 409. `cleanup` must re-list
        // containers under its own label filter, remove any holder it finds, and
        // retry the volume delete rather than treating the conflict as fatal.
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let container_lists = Arc::new(AtomicUsize::new(0));
        let volume_lists = Arc::new(AtomicUsize::new(0));
        let volume_deletes = Arc::new(AtomicUsize::new(0));
        let holder_removed = Arc::new(AtomicUsize::new(0));
        let server = {
            let container_lists = container_lists.clone();
            let volume_lists = volume_lists.clone();
            let volume_deletes = volume_deletes.clone();
            let holder_removed = holder_removed.clone();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                    }
                    let line = String::from_utf8_lossy(&request);
                    let start = line.lines().next().unwrap();
                    let mut parts = start.split_whitespace();
                    let method = parts.next().unwrap();
                    let path = parts.next().unwrap();
                    let bare = path.split('?').next().unwrap();
                    let (status, body) = match (method, bare) {
                        ("GET", "/v1.48/containers/json") => {
                            match container_lists.fetch_add(1, Ordering::SeqCst) {
                                0 => (200, "[]".to_string()),
                                _ => (200, r#"[{"Id":"holder"}]"#.to_string()),
                            }
                        }
                        ("DELETE", "/v1.48/containers/holder") => {
                            holder_removed.fetch_add(1, Ordering::SeqCst);
                            (204, String::new())
                        }
                        ("GET", "/v1.48/networks") => (200, "[]".to_string()),
                        ("GET", "/v1.48/volumes") => {
                            match volume_lists.fetch_add(1, Ordering::SeqCst) {
                                0 => (200, r#"{"Volumes":[{"Name":"vol1"}]}"#.to_string()),
                                _ => (200, r#"{"Volumes":[]}"#.to_string()),
                            }
                        }
                        ("DELETE", "/v1.48/volumes/vol1") => {
                            volume_deletes.fetch_add(1, Ordering::SeqCst);
                            (
                                409,
                                r#"{"message":"remove vol1: volume is in use - [holder]"}"#
                                    .to_string(),
                            )
                        }
                        other => panic!("unexpected request: {other:?}"),
                    };
                    let reason = if status == 204 {
                        "No Content"
                    } else {
                        "Fixture"
                    };
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            })
        };
        let docker = Docker {
            client: Client::builder()
                .unix_socket(socket)
                .no_proxy()
                .build()
                .unwrap(),
            supervisor: String::new(),
            image: String::new(),
            owner: "owner".into(),
            storage: "storage".into(),
            egress: Default::default(),
            runtime: None,
        };
        let result = docker.cleanup(None).await;
        server.abort();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(container_lists.load(Ordering::SeqCst), 2);
        assert_eq!(holder_removed.load(Ordering::SeqCst), 1);
        assert_eq!(volume_deletes.load(Ordering::SeqCst), 1);
        assert_eq!(volume_lists.load(Ordering::SeqCst), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn volume_removal_fails_fast_on_a_non_conflict_status() {
        use tokio::io::AsyncWriteExt;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let body = r#"{"Volumes":[{"Name":"vol1"}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(
                    b"HTTP/1.1 403 Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let docker = Docker {
            client: Client::builder()
                .unix_socket(socket)
                .no_proxy()
                .build()
                .unwrap(),
            supervisor: String::new(),
            image: String::new(),
            owner: "owner".into(),
            storage: "storage".into(),
            egress: Default::default(),
            runtime: None,
        };
        let result = docker
            .remove_volumes(&json!({"label":["x"]}).to_string())
            .await;
        server.abort();
        assert!(result.is_err(), "a non-conflict status must not be retried");
    }

    #[test]
    fn restored_result_volume_cannot_adopt_live_worker_resources() {
        let original = json!({"Id":"old-container","Mounts":[{"Name":"original-results","Destination":"/results"}]});
        let redeployed = json!({"Id":"new-container","Mounts":[{"Name":"original-results","Destination":"/results"}]});
        let restored = json!({"Id":"restore-container","Mounts":[{"Name":"restored-results","Destination":"/results"}]});
        let path = Path::new("/results");
        assert_eq!(
            storage_scope(&original, path).unwrap(),
            storage_scope(&redeployed, path).unwrap()
        );
        assert_ne!(
            storage_scope(&original, path).unwrap(),
            storage_scope(&restored, path).unwrap()
        );
    }
    /// A noisy dependency writing to stderr must not be reported as the
    /// agent's own transcript being cut off. One shared flag meant it was:
    /// the job came back `output_limit_*` with a complete stdout, blaming the
    /// agent for a warning it did not emit.
    #[tokio::test]
    async fn a_noisy_stderr_does_not_truncate_the_agents_transcript() {
        // stdout well inside the budget; stderr four bytes over it.
        let bytes = [
            1, 0, 0, 0, 0, 0, 0, 2, b'o', b'k', 2, 0, 0, 0, 0, 0, 0, 6, b'w', b'a', b'r', b'n',
            b'!', b'!',
        ];
        let capture = Capture::default();
        frames(&bytes[..], 2, &capture, false).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.stdout, b"ok");
        assert!(!output.stdout_truncated, "stdout was complete");
        assert!(output.stderr_truncated, "stderr lost bytes");
        // `truncated()` still says this exec's output is incomplete, which is
        // what a verification command cares about.
        assert!(output.truncated());
    }

    #[tokio::test]
    async fn output_frames_are_bounded_and_truncation_is_explicit() {
        let bytes = [
            1, 0, 0, 0, 0, 0, 0, 4, b'a', b'b', b'c', b'd', 2, 0, 0, 0, 0, 0, 0, 1, b'e',
        ];
        let capture = Capture::default();
        frames(&bytes[..], 3, &capture, false).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.stdout, b"abc");
        assert_eq!(output.stderr, b"e");
        assert!(output.truncated());
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
        assert!(
            frames(&bytes[..7], 10, &Capture::default(), false)
                .await
                .is_err()
        );
        let partial = Capture::default();
        assert!(frames(&bytes[..14], 10, &partial, false).await.is_err());
        assert_eq!(
            captured(&partial).stdout,
            b"abcd",
            "a broken output stream discarded already captured evidence"
        );
    }

    fn docker_frame(stream: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![stream, 0, 0, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[tokio::test]
    async fn agent_stdout_filter_drops_message_update_split_across_frames() {
        let delta = br#"{"type":"message_update","usage":{},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":" the"}}"#;
        let split = delta.len() / 2;
        let mut stream = Vec::new();
        // Split a single message_update line across two separate stdout frames.
        stream.extend(docker_frame(1, &delta[..split]));
        stream.extend(docker_frame(1, &delta[split..]));
        stream.extend(docker_frame(1, b"\n"));
        let capture = Capture::default();
        frames(&stream[..], 4096, &capture, true).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.stdout, b"[poundlock] omitted 1 streaming message_update event(s) from retained output; see message_end events for the complete assistant output\n");
        assert_eq!(output.omitted_stream_events, 1);
        assert!(!output.truncated());
    }

    #[tokio::test]
    async fn agent_stdout_filter_retains_message_end_and_non_json_byte_exact() {
        let message_end =
            br#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}
"#;
        let non_json = b"not json output\n";
        let mut stream = Vec::new();
        stream.extend(docker_frame(1, non_json));
        stream.extend(docker_frame(1, message_end));
        let capture = Capture::default();
        frames(&stream[..], 4096, &capture, true).await.unwrap();
        let output = captured(&capture);
        let mut expected = Vec::new();
        expected.extend_from_slice(non_json);
        expected.extend_from_slice(message_end);
        assert_eq!(output.stdout, expected);
        assert_eq!(output.omitted_stream_events, 0);
        assert!(!output.truncated());
    }

    #[tokio::test]
    async fn agent_stdout_filter_keeps_one_copy_of_each_tool_result() {
        let big = "x".repeat(2000);
        let lines = [
            format!(
                r#"{{"type":"tool_execution_end","toolCallId":"t1","toolName":"read","isError":false,"result":{{"content":[{{"type":"text","text":"{big}"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"message_start","message":{{"role":"toolResult","content":[{{"type":"text","text":"{big}"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"message_end","message":{{"role":"toolResult","content":[{{"type":"text","text":"{big}"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"turn_end","message":{{"role":"assistant"}},"toolResults":[{{"content":"{big}"}}]}}"#
            ),
            format!(
                r#"{{"type":"agent_end","messages":[{{"content":"{big}"}}],"willRetry":true}}"#
            ),
        ];
        let mut stream = Vec::new();
        for line in &lines {
            stream.extend(docker_frame(1, format!("{line}\n").as_bytes()));
        }
        let capture = Capture::default();
        frames(&stream[..], 1 << 20, &capture, true).await.unwrap();
        let output = captured(&capture);
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.matches(&big).count(), 1, "{text}");
        assert!(
            text.contains(&format!("{}\n", lines[0])),
            "tool_execution_end is kept byte-exact"
        );
        assert!(text.contains(r#"{"type":"turn_end"}"#));
        assert!(text.contains(r#""type":"agent_end""#) && text.contains(r#""willRetry":true"#));
        assert!(
            text.contains(r#""toolName":"read""#),
            "tool_execution_end keeps its fields"
        );
        assert!(!text.contains(r#""type":"message_start""#));
        assert!(!text.contains(r#""role":"toolResult""#));
        assert_eq!(output.compacted_events, 4);
        assert!(text.ends_with("removed repeated messages and tool results from 4 event(s); see message_end events for the complete assistant output\n"));
        // What Poundlock reads still reads the same.
        assert!(!crate::harness::agent_turn_settled(&text));
    }

    #[tokio::test]
    async fn agent_stdout_filter_leaves_stderr_untouched() {
        let delta_shaped_stderr =
            br#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}
"#;
        let stream = docker_frame(2, delta_shaped_stderr);
        let capture = Capture::default();
        frames(&stream[..], 4096, &capture, true).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.stderr, delta_shaped_stderr);
        assert!(output.stdout.is_empty());
        assert_eq!(output.omitted_stream_events, 0);
    }

    #[tokio::test]
    async fn agent_stdout_filter_still_truncates_a_genuinely_oversized_line() {
        let mut message_end =
            br#"{"type":"message_end","message":{"role":"assistant","content":""#.to_vec();
        message_end.extend(std::iter::repeat_n(b'a', 100));
        message_end.extend_from_slice(b"\"}}\n");
        let stream = docker_frame(1, &message_end);
        let capture = Capture::default();
        frames(&stream[..], 32, &capture, true).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.stdout.len(), 32);
        assert_eq!(output.stdout, &message_end[..32]);
        assert!(output.truncated());
        assert_eq!(output.omitted_stream_events, 0);
    }

    #[tokio::test]
    async fn pi_failure_still_detects_failure_after_stdout_filtering() {
        let mut stream = Vec::new();
        stream.extend(docker_frame(
            1,
            br#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":" the"}}
"#,
        ));
        stream.extend(docker_frame(
            1,
            br#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"boom"}}
"#,
        ));
        let capture = Capture::default();
        frames(&stream[..], 4096, &capture, true).await.unwrap();
        let output = captured(&capture);
        assert_eq!(output.omitted_stream_events, 1);
        let bounded = crate::harness::bounded_output(output.stdout, output.stderr, 4 * 1024 * 1024);
        let failure = crate::harness::pi_failure(&bounded).expect("failure detected");
        assert_eq!(failure["code"], "model_request_failed");
    }
}
