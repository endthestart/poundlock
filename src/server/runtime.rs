use super::AppState;
use crate::domain::{ModelProfile, NodeAction};
use anyhow::{Context, Result};
use serde_json::json;
use std::time::{Duration, Instant};

pub(super) async fn prepare_profile(state: &AppState, lease: &mut RuntimeLease) -> Result<String> {
    let profile = lease.profile.clone();
    let lease_id = lease.lease_id.clone();
    let Some(node_id) = profile.node_id.as_deref() else {
        return Ok(profile.model_id);
    };
    let node = state
        .store
        .node(node_id, state.config.node_offline_seconds)?
        .ok_or_else(|| anyhow::anyhow!("configured node does not exist"))?;
    anyhow::ensure!(
        !matches!(node.status.as_str(), "maintenance" | "revoked"),
        "Machine is paused for maintenance or has been removed"
    );
    if node.status == "online"
        && let Some(problem) = node.network_problem()
    {
        anyhow::bail!(problem);
    }

    if profile.wake_policy == "never" && profile.runtime_profile.as_deref() != Some("ollama") {
        enqueue_ensure(state, &profile, &lease_id)?;
        return Ok(profile.model_id);
    }

    let deadline = Instant::now() + Duration::from_secs(state.config.external_timeout_seconds);
    let endpoint = state
        .store
        .endpoint_secret(&profile.endpoint_id)?
        .ok_or_else(|| anyhow::anyhow!("endpoint does not exist"))?;

    if node.status != "online" {
        wait_for_node(state, &profile, node_id, deadline).await?;
    }
    let action = enqueue_ensure(state, &profile, &lease_id)?;
    // A gateway's model list can advertise an offline upstream. The node
    // must acknowledge its runtime lease before the request can proceed.
    let result = wait_for_action(state, &profile, &action, deadline).await?;
    let model_id = prepared_model_id(&profile, &result)?;

    wait_for_model(state, &endpoint, &model_id, deadline).await?;
    Ok(model_id)
}

/// Side-effect-free view of the node/runtime admission checks performed by
/// `prepare_profile`. Returns `(availability, reason)` when the runtime would
/// not be immediately usable, so preflight and admission errors can tell an
/// offline or paused machine apart from credential or capacity problems.
pub(super) fn runtime_availability(
    state: &AppState,
    profile: &ModelProfile,
) -> Result<Option<(&'static str, &'static str)>> {
    let Some(node_id) = profile.node_id.as_deref() else {
        return Ok(None);
    };
    let Some(node) = state
        .store
        .node(node_id, state.config.node_offline_seconds)?
    else {
        return Ok(Some(("blocked", "runtime_node_missing")));
    };
    Ok(match node.status.as_str() {
        "maintenance" | "revoked" => Some(("blocked", "runtime_node_unavailable")),
        "online" if node.network_problem().is_some() => {
            Some(("blocked", "runtime_node_network_unavailable"))
        }
        "online" => None,
        // Mirrors prepare_profile: a never-wake profile without a managed
        // runtime is sent to its endpoint without waiting for the node.
        _ if profile.wake_policy == "never"
            && profile.runtime_profile.as_deref() != Some("ollama") =>
        {
            None
        }
        _ if profile.wake_policy == "on-demand" => {
            if node.wol_mac.is_some() && node.wol_broadcast.is_some() {
                Some(("preparation_required", "runtime_wake_required"))
            } else {
                Some(("blocked", "runtime_wake_unavailable"))
            }
        }
        _ => Some(("blocked", "runtime_node_offline")),
    })
}

/// What to tell a client whose request could not reach the model server.
/// "When a request arrives: I keep it running" forwards without starting or
/// waking anything, so this is where such a model says the server is down,
/// and whether its machine is known to be asleep.
pub(super) fn unreachable_server_message(state: &AppState, profile: &ModelProfile) -> &'static str {
    let asleep = profile.node_id.as_deref().is_some_and(|node_id| {
        state
            .store
            .node(node_id, state.config.node_offline_seconds)
            .ok()
            .flatten()
            .is_some_and(|node| node.status != "online")
    });
    if asleep {
        "could not connect to the model server: its machine is asleep or offline"
    } else {
        "could not connect to the model server: it is not running or cannot be reached"
    }
}

pub(crate) fn prepared_model_id(
    profile: &ModelProfile,
    result: &serde_json::Value,
) -> Result<String> {
    if profile.runtime_profile.as_deref() == Some("ollama") {
        Ok(result["resolved_model_id"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .context("Ollama node did not return its managed context model; upgrade the node")?
            .to_owned())
    } else {
        Ok(profile.model_id.clone())
    }
}

async fn wait_for_node(
    state: &AppState,
    profile: &ModelProfile,
    node_id: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        let node = state
            .store
            .node(node_id, state.config.node_offline_seconds)?
            .ok_or_else(|| anyhow::anyhow!("configured node does not exist"))?;
        anyhow::ensure!(
            !matches!(node.status.as_str(), "maintenance" | "revoked"),
            "Machine is paused for maintenance or has been removed"
        );
        if node.status == "online" {
            if let Some(problem) = node.network_problem() {
                anyhow::bail!(problem);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("node did not reconnect before the deadline");
        }
        if profile.wake_policy == "on-demand" {
            if let (Some(mac), Some(broadcast)) = (&node.wol_mac, &node.wol_broadcast) {
                crate::wake::request_magic_packet(
                    state.config.wake_socket.as_deref(),
                    mac,
                    broadcast,
                )
                .await?;
            } else {
                anyhow::bail!("node is offline and has no Wake-on-LAN adapter");
            }
        } else {
            anyhow::bail!("the machine is asleep or offline, and this model does not wake it");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The node ran the runtime's ensure and it failed. Distinct from the machine
/// being offline or paused: the node answered, and a later ensure starts
/// clean, so the caller can retry. Typed so the relay can say so without
/// matching on message text.
#[derive(Debug)]
pub(crate) struct EnsureFailed(pub serde_json::Value);

impl std::fmt::Display for EnsureFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime ensure failed: {}", self.0)
    }
}

impl std::error::Error for EnsureFailed {}

async fn wait_for_action(
    state: &AppState,
    profile: &ModelProfile,
    action: &NodeAction,
    deadline: Instant,
) -> Result<serde_json::Value> {
    let signal = state.store.node_signal(&action.node_id);
    loop {
        // Subscribe before reading, so a completion in between still wakes us.
        let changed = signal.settled.notified();
        let current = state
            .store
            .action(&action.id)?
            .ok_or_else(|| anyhow::anyhow!("runtime action disappeared"))?;
        match current.status.as_str() {
            "completed" => return Ok(current.result.unwrap_or(json!({}))),
            "failed" => {
                return Err(EnsureFailed(current.result.unwrap_or(json!({}))).into());
            }
            _ if Instant::now() < deadline => {
                // The heartbeat can still look fresh while a machine enters sleep.
                // Keep the same action, but recover connectivity before waiting again.
                wait_for_node(state, profile, &action.node_id, deadline).await?;
                // The node's completion wakes this at once; the second only
                // paces the checks above while nothing arrives.
                let _ = tokio::time::timeout(Duration::from_secs(1), changed).await;
            }
            _ => anyhow::bail!("runtime ensure did not complete before the deadline"),
        }
    }
}

async fn wait_for_model(
    state: &AppState,
    endpoint: &crate::domain::EndpointSecret,
    model_id: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        match state.inference.probe(endpoint, model_id).await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(error) => return Err(error),
        }
    }
}

fn enqueue_ensure(state: &AppState, profile: &ModelProfile, lease_id: &str) -> Result<NodeAction> {
    let node_id = profile
        .node_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("node is not configured"))?;
    let runtime = profile
        .runtime_profile
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("runtime profile missing"))?;
    state.store.enqueue_action(
        node_id,
        None,
        "ensure",
        json!({
            "runtime_profile": runtime,
            "model_id": profile.model_id, "context_tokens": profile.context_tokens,
            "idle_seconds": profile.idle_seconds.unwrap_or(0),
            "lease_id": lease_id,
            "operation_id": uuid::Uuid::new_v4()
        }),
    )
}

fn release_profile(state: &AppState, profile: &ModelProfile, lease_id: &str) {
    if let (Some(node), Some(runtime)) = (&profile.node_id, &profile.runtime_profile)
        && let Err(error) = state.store.enqueue_action(
            node,
            None,
            "release",
            json!({
                "runtime_profile": runtime,
                "lease_id": lease_id,
                "operation_id": uuid::Uuid::new_v4()
            }),
        )
    {
        tracing::error!(%error, "could not release relay runtime lease");
    }
}

pub(super) struct RuntimeLease {
    state: AppState,
    profile: ModelProfile,
    lease_id: String,
}

impl RuntimeLease {
    pub(super) fn new(state: AppState, profile: ModelProfile, lease_id: String) -> Self {
        Self {
            state,
            profile,
            lease_id,
        }
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        // Renewal can be queued as soon as occupancy is claimed, before ensure.
        release_profile(&self.state, &self.profile, &self.lease_id);
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

// A restarted managed node releases its old local leases. Do not let a running
// worker silently auto-load an unowned model through the inference endpoint.
pub(crate) fn managed_job_runtime_valid(
    state: &AppState,
    package: &crate::domain::RunnerPackage,
) -> Result<bool> {
    if package.model.runtime_profile.as_deref() != Some("ollama") {
        return Ok(true);
    }
    let Some(node_id) = package.model.node_id.as_deref() else {
        return Ok(false);
    };
    let Some(node) = state
        .store
        .node(node_id, state.config.node_offline_seconds)?
    else {
        return Ok(false);
    };
    if node.status != "online" || node.network_problem().is_some() {
        return Ok(false);
    }
    let Some(action) = state.store.action_for_job(&package.job.id, "ensure")? else {
        return Ok(false);
    };
    if prepared_model_id(&package.model, action.result.as_ref().unwrap_or(&json!({}))).is_err() {
        return Ok(false);
    }
    let expected = action.result.as_ref().and_then(|r| r["boot_id"].as_str());
    let observed = node.capabilities["model_memory"]["boot_id"].as_str();
    Ok(action.status == "completed" && expected.is_some() && expected == observed)
}

pub(crate) fn managed_job_model_id(
    state: &AppState,
    package: &crate::domain::RunnerPackage,
) -> Result<String> {
    if package.model.runtime_profile.as_deref() != Some("ollama") {
        return Ok(package.model.model_id.clone());
    }
    let action = state
        .store
        .action_for_job(&package.job.id, "ensure")?
        .context("managed runtime action is unavailable")?;
    anyhow::ensure!(action.status == "completed", "managed runtime is not ready");
    prepared_model_id(&package.model, action.result.as_ref().unwrap_or(&json!({})))
}
