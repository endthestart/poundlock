mod client;
mod inhibitor;
mod inventory;
mod ollama;
mod operations;
mod poll_loop;
#[cfg(test)]
pub(crate) mod testing;
pub use operations::{
    check_config, check_config_identity, doctor, enroll_once, maintenance, revoke,
};
pub mod network;
mod update;

use crate::crypto::write_private;
use crate::domain::{EnrollNode, NodeAction};
use anyhow::{Context, Result, bail};
use client::NodeClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

pub use update::default_data_dir;

pub struct NodeOptions {
    pub controller: String,
    pub name: String,
    pub enrollment_token: Option<String>,
    pub credential_file: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

pub(crate) const MAX_LEASE_TIMEOUT_SECONDS: u64 = 3600;
pub(crate) const RUNTIME_ACTION_TIMEOUT_SECONDS: u64 = 600;
pub(crate) const MAX_HEARTBEAT_SECONDS: u64 = 10;
/// Longest a node may ask the controller to hold an idle action poll. Well
/// under the idle timeouts of common reverse proxies; a node asks for its
/// heartbeat interval, which is at most half of this.
pub(crate) const MAX_POLL_WAIT_SECONDS: u64 = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(default)]
    controller: String,
    #[serde(default)]
    name: String,
    /// On unless the configuration turns it off. What a node agrees to install
    /// is decided in `update`: a signed newer binary when this build embeds the
    /// release key, otherwise a newer one over HTTPS or loopback only.
    #[serde(default = "enabled")]
    auto_update: bool,
    /// The operator's explicit acceptance of a plain `http://` controller on
    /// another machine. Reported to the controller so Machines can show it.
    #[serde(default)]
    allow_insecure_http: bool,
    #[serde(default)]
    network: network::NetworkPolicy,
    #[serde(default = "default_heartbeat")]
    heartbeat_seconds: u64,
    #[serde(default = "default_lease_timeout")]
    lease_timeout_seconds: u64,
    #[serde(default)]
    runtimes: HashMap<String, RuntimeCommands>,
    #[serde(default)]
    ollama: ollama::Config,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RuntimeCommands {
    #[serde(default)]
    ensure: Vec<String>,
    #[serde(default)]
    renew: Vec<String>,
    #[serde(default)]
    release: Vec<String>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    identity: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeCredential {
    node_id: String,
    credential: String,
}

pub async fn run(options: NodeOptions) -> Result<()> {
    let (options, config) = operations::load(options)?;
    if !options.credential_file.exists() {
        config.network.validate()?;
    }
    let api = NodeClient::new(options.controller.clone(), &config.network)?;
    let capabilities = capabilities(&config, &options.controller, &options.data_dir).await;
    let mut models = ollama::Manager::open(config.ollama.clone(), &options.data_dir)?;
    models
        .tick(Duration::from_secs(config.lease_timeout_seconds))
        .await;
    let (models_tx, models_rx) = tokio::sync::watch::channel(models.report());
    let credential = load_or_enroll(&api, &options, &capabilities).await?;
    let (inventory_tx, inventory_rx) =
        tokio::sync::watch::channel(capabilities["inventory"].clone());
    let data_dir = options.data_dir.clone();
    let identities = identity_commands(&config);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(inventory::REFRESH_SECONDS)).await;
            if inventory_tx
                .send(inventory::collect(&data_dir, &identities).await)
                .is_err()
            {
                break;
            }
        }
    });
    let heartbeat_api = api.clone();
    let heartbeat_credential = credential.clone();
    let mut heartbeat_capabilities = capabilities.clone();
    let network_policy = config.network.clone();
    let heartbeat_seconds = config.heartbeat_seconds;
    tokio::spawn(async move {
        loop {
            heartbeat_capabilities["network"] = network_policy.report();
            heartbeat_capabilities["inventory"] = inventory_rx.borrow().clone();
            heartbeat_capabilities["model_memory"] = models_rx.borrow().clone();
            if let Err(error) = heartbeat_api
                .heartbeat(&heartbeat_credential, &heartbeat_capabilities)
                .await
            {
                tracing::warn!(%error, "node heartbeat failed");
            }
            tokio::time::sleep(Duration::from_secs(heartbeat_seconds)).await;
        }
    });
    let mut host = Local {
        config: &config,
        options: &options,
        inhibitors: inhibitor::Inhibitors::new(),
        models,
        models_tx,
        last_update_check: Instant::now()
            .checked_sub(Duration::from_secs(300))
            .unwrap_or_else(Instant::now),
    };
    poll_loop::serve(
        &api,
        &credential,
        Duration::from_secs(config.heartbeat_seconds),
        &mut host,
    )
    .await
}

/// What this machine does for the poll loop: run runtime actions, do the
/// periodic upkeep, and replace its own binary when idle.
struct Local<'a> {
    config: &'a NodeConfig,
    options: &'a NodeOptions,
    inhibitors: inhibitor::Inhibitors,
    models: ollama::Manager,
    models_tx: tokio::sync::watch::Sender<Value>,
    last_update_check: Instant,
}

impl poll_loop::Host for Local<'_> {
    async fn execute(&mut self, action: &NodeAction) -> Result<Value> {
        execute_action(self.config, action, &mut self.inhibitors, &mut self.models).await
    }

    async fn idle(&mut self, api: &NodeClient, credential: &NodeCredential) {
        if self.config.auto_update
            && self.inhibitors.is_empty()
            && !self.models.busy()
            && self.last_update_check.elapsed() >= Duration::from_secs(300)
        {
            self.last_update_check = Instant::now();
            match update::maybe_replace(
                api,
                credential,
                &self.options.data_dir,
                &self.options.config_file,
            )
            .await
            {
                Ok(true) => {
                    tracing::info!("node binary replaced; exiting so the supervisor restarts");
                    std::process::exit(0);
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "node update check failed"),
            }
        }
    }

    async fn maintain(&mut self) {
        self.models
            .tick(Duration::from_secs(self.config.lease_timeout_seconds))
            .await;
        let _ = self.models_tx.send(self.models.report());
        for lease_id in inhibitor::expire(
            &mut self.inhibitors,
            Duration::from_secs(self.config.lease_timeout_seconds),
        )
        .await
        {
            tracing::warn!(%lease_id, "expired unrenewed node sleep inhibitor");
        }
    }
}

async fn load_or_enroll(
    api: &NodeClient,
    options: &NodeOptions,
    capabilities: &Value,
) -> Result<NodeCredential> {
    if options.credential_file.exists() {
        return Ok(serde_json::from_slice(
            &tokio::fs::read(&options.credential_file).await?,
        )?);
    }
    let token = options
        .enrollment_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--enrollment-token is required for first enrollment"))?;
    let enrolled = api
        .enroll(EnrollNode {
            token: token.into(),
            name: options.name.clone(),
            platform: std::env::consts::OS.into(),
            capabilities: capabilities.clone(),
        })
        .await?;
    let credential = NodeCredential {
        node_id: enrolled.node_id,
        credential: enrolled.credential,
    };
    write_private(
        &options.credential_file,
        &serde_json::to_string_pretty(&credential)?,
    )?;
    Ok(credential)
}

async fn capabilities(config: &NodeConfig, controller: &str, data_dir: &std::path::Path) -> Value {
    let transport = match operations::transport(controller) {
        Ok(operations::Transport::Https) => "https",
        Ok(operations::Transport::Loopback) => "loopback",
        _ => "insecure_http",
    };
    json!({"controller_transport":transport,"runtimes":config.runtimes.keys().map(String::as_str).chain(config.ollama.enabled.then_some("ollama")).collect::<Vec<_>>(),
        "sleep_inhibitor":sleep_inhibitor_name(),"typed_actions":["ensure","renew","release","stop"],
        "action_long_poll":true,
        "agent_version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "auto_update":config.auto_update,
        "update_verification":if update::embedded_key_text().is_some() {"signature"} else {"checksum"},
        "network":config.network.report(),"inventory":inventory::collect(data_dir, &identity_commands(config)).await})
}

/// Per-runtime identity probe commands, keyed by runtime name. A runtime with
/// no `identity` command configured maps to an empty argument list and is
/// skipped when the inventory is collected.
fn identity_commands(config: &NodeConfig) -> HashMap<String, Vec<String>> {
    config
        .runtimes
        .iter()
        .map(|(name, commands)| (name.clone(), commands.identity.clone()))
        .collect()
}

async fn execute_action(
    config: &NodeConfig,
    action: &NodeAction,
    inhibitors: &mut inhibitor::Inhibitors,
    models: &mut ollama::Manager,
) -> Result<Value> {
    if !["ensure", "renew", "release", "stop"].contains(&action.kind.as_str()) {
        bail!("controller requested unsupported action kind");
    }
    if action.kind == "ensure" {
        config.network.validate()?;
    }
    let profile = field(&action.payload, "runtime_profile")?;
    let (lease, operation) = action_ids(&action.payload)?;
    if profile == "ollama" {
        if action.kind == "ensure"
            && !inhibitors.contains_key(lease)
            && let Some(held) = inhibitor::acquire(lease).await?
        {
            inhibitors.insert(lease.into(), held);
        }
        let result = models.action(&action.kind, lease, &action.payload).await;
        if action.kind == "renew" && result.is_ok() {
            inhibitor::renew(inhibitors, lease);
        }
        if matches!(action.kind.as_str(), "release" | "stop")
            || (action.kind == "ensure" && result.is_err())
        {
            inhibitor::release(inhibitors, lease).await;
        }
        return result;
    }
    let runtime = config.runtimes.get(profile).ok_or_else(|| {
        anyhow::anyhow!("runtime profile '{profile}' is not configured on this node")
    })?;
    if action.kind == "ensure"
        && !inhibitors.contains_key(lease)
        && let Some(inhibitor) = inhibitor::acquire(lease).await?
    {
        inhibitors.insert(lease.into(), inhibitor);
    }
    let command = match action.kind.as_str() {
        "ensure" => &runtime.ensure,
        "renew" => &runtime.renew,
        "release" => &runtime.release,
        "stop" => &runtime.stop,
        _ => unreachable!(),
    };
    let model = action
        .payload
        .get("model_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut result = run_allowlisted(command, profile, model, lease, operation).await;
    // What is serving now, taken the moment the model is ready. The periodic
    // inventory is up to five minutes old, so a job claimed right after its own
    // ensure was being credited with whichever model the previous job loaded.
    if action.kind == "ensure"
        && let Ok(Value::Object(fields)) = &mut result
        && let Some(identity) = inventory::runtime_identity(&runtime.identity).await
    {
        fields.insert("identity".into(), identity);
    }
    if action.kind == "renew" && result.is_ok() {
        inhibitor::renew(inhibitors, lease);
    }
    if (matches!(action.kind.as_str(), "release" | "stop")
        || (action.kind == "ensure" && result.is_err()))
    {
        inhibitor::release(inhibitors, lease).await;
    }
    result
}

/// The lease and operation ids a runtime command may receive. The controller
/// only ever sends UUIDs; anything else (`-h`, `../x`, a shell fragment) is a
/// forged action, refused before any command sees it.
fn action_ids(payload: &Value) -> Result<(&str, &str)> {
    let lease = field(payload, "lease_id")?;
    let operation = field(payload, "operation_id")?;
    let uuid = |value: &str| value.len() == 36 && uuid::Uuid::try_parse(value).is_ok();
    anyhow::ensure!(
        uuid(lease) && uuid(operation),
        "controller sent a lease or operation id that is not a UUID"
    );
    Ok((lease, operation))
}

fn is_model_argument(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

async fn run_allowlisted(
    template: &[String],
    profile: &str,
    model: &str,
    lease: &str,
    operation: &str,
) -> Result<Value> {
    run_owned_command(
        template,
        profile,
        model,
        lease,
        operation,
        Duration::from_secs(RUNTIME_ACTION_TIMEOUT_SECONDS),
    )
    .await
}

async fn run_owned_command(
    template: &[String],
    profile: &str,
    model: &str,
    lease: &str,
    operation: &str,
    timeout: Duration,
) -> Result<Value> {
    if template.is_empty() {
        return Ok(json!({"adapter":"noop","status":"ready"}));
    }
    // {model} lets one runtime (one hardware slot) serve several model profiles.
    // It comes from the controller, so only a conservative model-id shape passes.
    if template.iter().any(|part| part.contains("{model}")) && !is_model_argument(model) {
        anyhow::bail!("model id is missing or not a safe command argument");
    }
    let values = template
        .iter()
        .map(|part| {
            part.replace("{profile}", profile)
                .replace("{model}", model)
                .replace("{lease_id}", lease)
                .replace("{operation_id}", operation)
        })
        .collect::<Vec<_>>();
    let mut command = Command::new(&values[0]);
    command
        .args(&values[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate(&mut command);
    let child = command.spawn().context("start runtime command")?;
    let mut group = crate::process::ProcessGroup::new(&child)?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("runtime command exceeded {timeout:?}"))??;
    if output.status.success() {
        // A successful adapter may have started a long-lived server in this
        // group. Timeout and failure keep the group armed so Drop stops it.
        group.disarm();
    }
    let stdout = bounded(&output.stdout);
    let stderr = bounded(&output.stderr);
    if !output.status.success() {
        bail!(
            "runtime command exited {:?}: {stderr}",
            output.status.code()
        );
    }
    let parsed =
        serde_json::from_str(&stdout).unwrap_or_else(|_| json!({"stdout":stdout,"stderr":stderr}));
    Ok(parsed)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("action field '{name}' is missing"))
}

fn bounded(bytes: &[u8]) -> String {
    let bytes = if bytes.len() > 64 * 1024 {
        &bytes[bytes.len() - 64 * 1024..]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).into_owned()
}

fn default_heartbeat() -> u64 {
    3
}

fn default_lease_timeout() -> u64 {
    300
}

fn sleep_inhibitor_name() -> &'static str {
    inhibitor::name()
}

pub fn example_config() -> &'static str {
    include_str!("../config/examples/node.toml")
}

fn enabled() -> bool {
    true
}

#[cfg(all(test, unix))]
mod runtime_command_tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn read_pid(path: &Path) -> i32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    async fn wait_pid(path: &Path) -> i32 {
        for _ in 0..50 {
            if path.exists() {
                return read_pid(path);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("runtime command did not write a pid file");
    }

    #[tokio::test]
    async fn model_placeholder_selects_the_model_and_rejects_unsafe_ids() {
        let echo = vec![
            "sh".into(),
            "-c".into(),
            "printf '{\"model\":\"%s\"}' \"$1\"".into(),
            "ensure".into(),
            "{model}".into(),
        ];
        let result = run_owned_command(
            &echo,
            "coding",
            "qwen3.8-27b:q4",
            "lease",
            "op",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(result["model"], "qwen3.8-27b:q4");
        for unsafe_id in [
            "", "--help", "a b", "../x", "x;y", "..", ".hidden", ":x", "_x",
        ] {
            assert!(
                run_owned_command(
                    &echo,
                    "coding",
                    unsafe_id,
                    "lease",
                    "op",
                    Duration::from_secs(5)
                )
                .await
                .is_err(),
                "{unsafe_id:?} should be rejected"
            );
        }
    }

    #[test]
    fn only_uuid_lease_and_operation_ids_reach_a_runtime_command() {
        let lease = "5f0c6b1e-8a4f-4f7e-9a53-2c1d7e6b9a10";
        let operation = "0b7f3c2a-1d4e-4c6b-8f9a-7e2d5c1b3a49";
        let ids = json!({"lease_id":lease,"operation_id":operation});
        assert_eq!(action_ids(&ids).unwrap(), (lease, operation));
        for forged in [
            "-h",
            "--help",
            "../../etc/passwd",
            "x; rm -rf ~",
            "",
            "5f0c6b1e8a4f4f7e9a532c1d7e6b9a10",
            "{5f0c6b1e-8a4f-4f7e-9a53-2c1d7e6b9a10}",
        ] {
            assert!(
                action_ids(&json!({"lease_id":forged,"operation_id":operation})).is_err(),
                "{forged:?} lease"
            );
            assert!(
                action_ids(&json!({"lease_id":lease,"operation_id":forged})).is_err(),
                "{forged:?} operation"
            );
        }
        assert!(action_ids(&json!({"lease_id":lease})).is_err());
    }

    #[tokio::test]
    async fn successful_startup_leaves_a_background_server_until_release() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("server.pid");
        let ensure = vec![
            "sh".into(),
            "-c".into(),
            "nohup sleep 8 >/dev/null 2>&1 & printf '%s' \"$!\" > \"$1\"; echo '{\"status\":\"ready\"}'".into(),
            "ensure".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let result =
            run_owned_command(&ensure, "coding", "", "lease", "op", Duration::from_secs(2))
                .await
                .unwrap();
        assert_eq!(result["status"], "ready");
        let pid = read_pid(&pid_file);
        assert!(pid_alive(pid), "successful ensure must not kill its server");
        let release = vec![
            "sh".into(),
            "-c".into(),
            "kill \"$1\"; echo '{\"status\":\"stopped\"}'".into(),
            "release".into(),
            pid.to_string(),
        ];
        run_owned_command(
            &release,
            "coding",
            "",
            "lease",
            "op",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!pid_alive(pid), "release must stop the started server");
    }

    #[tokio::test]
    async fn failed_startup_stops_command_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("server.pid");
        let ensure = vec![
            "sh".into(),
            "-c".into(),
            "sleep 8 & printf '%s' \"$!\" > \"$1\"; exit 1".into(),
            "ensure".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        assert!(
            run_owned_command(&ensure, "coding", "", "lease", "op", Duration::from_secs(2))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let pid = read_pid(&pid_file);
        assert!(!pid_alive(pid), "failed ensure must stop descendants");
    }

    #[tokio::test]
    async fn timed_out_startup_stops_command_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("server.pid");
        let ensure = vec![
            "sh".into(),
            "-c".into(),
            "sleep 8 & printf '%s' \"$!\" > \"$1\"; wait".into(),
            "ensure".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let error = run_owned_command(
            &ensure,
            "coding",
            "",
            "lease",
            "op",
            Duration::from_millis(200),
        )
        .await
        .expect_err("timeout");
        assert!(error.to_string().contains("exceeded"), "{error:#}");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let pid = wait_pid(&pid_file).await;
        assert!(!pid_alive(pid), "timeout must stop descendants");
    }

    #[tokio::test]
    async fn cancelled_startup_stops_command_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("server.pid");
        let ensure = vec![
            "sh".into(),
            "-c".into(),
            "sleep 8 & printf '%s' \"$!\" > \"$1\"; wait".into(),
            "ensure".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let run = run_owned_command(
            &ensure,
            "coding",
            "",
            "lease",
            "op",
            Duration::from_secs(10),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), run)
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let pid = wait_pid(&pid_file).await;
        assert!(!pid_alive(pid), "cancellation must stop descendants");
    }
}
