use crate::domain::{Job, ModelProfile};
use crate::server::AppState;
use crate::server::capacity::{Occupancy, Occupied};
use crate::store::Store;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[path = "scheduler_actions.rs"]
mod actions;
use actions::queue_runtime_action;
pub(crate) use actions::release_job;

const RENEW_INTERVAL: Duration = Duration::from_secs(60);

pub struct Scheduler;

impl Scheduler {
    pub fn spawn(state: AppState) {
        let preparation_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                interval.tick().await;
                if let Err(error) = prepare_queue(&preparation_state).await {
                    tracing::error!(%error, "queue preparation failed");
                }
            }
        });
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            let mut renewals = HashMap::<String, Instant>::new();
            loop {
                interval.tick().await;
                if let Err(error) = maintain_leases(&state, &mut renewals) {
                    tracing::error!(%error, "scheduler tick failed");
                }
            }
        });
    }
}

fn maintain_leases(state: &AppState, renewals: &mut HashMap<String, Instant>) -> Result<()> {
    for id in state.store.fail_expired_runners()? {
        release_job(state, &id, "stop")?;
    }
    let active = state.store.active_jobs()?;
    let interactive = state.occupancy.interactive_leases();
    renewals.retain(|id, _| {
        active.iter().any(|job| &job.id == id)
            || interactive.iter().any(|lease| &lease.lease_id == id)
    });
    for job in active {
        if matches!(job.status.as_str(), "ready" | "running")
            && let Some(package) = state.store.runner_package(&job.id)?
            && !crate::server::runtime::managed_job_runtime_valid(state, &package)?
        {
            state.store.fail_job(&job.id,json!({"layer":"runtime","code":"runtime_lease_lost","message":"The model node disconnected or restarted. Submit a new job after it is ready."}),false)?;
            release_job(state, &job.id, "stop")?;
            continue;
        }
        let due = renewals
            .get(&job.id)
            .is_none_or(|last| last.elapsed() >= RENEW_INTERVAL);
        if due {
            queue_runtime_action(state, &job, "renew")?;
            renewals.insert(job.id, Instant::now());
        }
    }
    enqueue_interactive_renewals(&state.store, &interactive, renewals)?;
    Ok(())
}

async fn prepare_queue(state: &AppState) -> Result<()> {
    // Readiness I/O must not serialize across the entire queue or starve leases.
    use futures_util::StreamExt;
    let jobs = state.store.schedulable_jobs()?;
    let results = futures_util::stream::iter(jobs)
        .map(|job| prepare_job(state, job))
        .buffer_unordered(16);
    tokio::pin!(results);
    while let Some(result) = results.next().await {
        if let Err(error) = result {
            tracing::error!(%error, "job preparation failed");
        }
    }
    Ok(())
}

fn enqueue_interactive_renewals(
    store: &Store,
    interactive: &[crate::server::capacity::InteractiveLease],
    renewals: &mut HashMap<String, Instant>,
) -> Result<()> {
    for lease in interactive {
        let due = renewals
            .get(&lease.lease_id)
            .is_none_or(|last| last.elapsed() >= RENEW_INTERVAL);
        if due {
            store.enqueue_action(
                &lease.node_id,
                None,
                "renew",
                json!({
                    "runtime_profile": lease.runtime_profile,
                    "lease_id": lease.lease_id,
                    "operation_id": uuid::Uuid::new_v4()
                }),
            )?;
            renewals.insert(lease.lease_id.clone(), Instant::now());
        }
    }
    Ok(())
}

async fn prepare_job(state: &AppState, job: Job) -> Result<()> {
    if job.cancel_requested {
        state.store.fail_job(
            &job.id,
            json!({"layer":"controller","code":"cancelled_before_run",
            "message":"Cancelled before a runner started."}),
            true,
        )?;
        release_job(state, &job.id, "stop")?;
        return Ok(());
    }
    let Some(package) = state.store.runner_package(&job.id)? else {
        state
            .store
            .fail_job(&job.id, missing("model profile"), false)?;
        release_job(state, &job.id, "stop")?;
        return Ok(());
    };
    let profile = package.model;
    let mut runtime_model_id = profile.model_id.clone();
    // What the node's own identity probe saw right after this job's model was
    // ready. Recorded with `model_ready` so provenance names this job's model,
    // not whichever one the node's periodic inventory last saw.
    let mut ensured_identity: Option<serde_json::Value> = None;
    if let Some(node_id) = &profile.node_id {
        let Some(node) = state
            .store
            .node(node_id, state.config.node_offline_seconds)?
        else {
            state.store.fail_job(&job.id, missing("node"), false)?;
            release_job(state, &job.id, "stop")?;
            return Ok(());
        };
        if node.status == "revoked" {
            state
                .store
                .fail_job(&job.id, missing("node registration"), false)?;
            release_job(state, &job.id, "stop")?;
            return Ok(());
        }
        if node.status == "maintenance" {
            if job.status != "waiting_node" {
                state.store.set_job_status(
                    &job.id,
                    "waiting_node",
                    "node_maintenance",
                    "Machine is paused for maintenance.",
                )?;
            }
            return Ok(());
        }
        if node.status != "online" {
            if job.status != "waiting_node" {
                state.store.set_job_status(
                    &job.id,
                    "waiting_node",
                    "node_offline",
                    "Waiting for the machine to come online.",
                )?;
            }
            if profile.wake_policy == "on-demand"
                && state.store.wake_due(&job.id, 30)?
                && let (Some(mac), Some(broadcast)) = (&node.wol_mac, &node.wol_broadcast)
            {
                match crate::wake::request_magic_packet(
                    state.config.wake_socket.as_deref(),
                    mac,
                    broadcast,
                )
                .await
                {
                    Ok(()) => state.store.add_event(
                        &job.id,
                        "wake_requested",
                        "Wake request sent; waiting for the machine to respond.",
                        json!({}),
                    )?,
                    Err(error) => state.store.add_event(
                        &job.id,
                        "wake_failed",
                        "Wake-on-LAN adapter failed.",
                        json!({"error":error.to_string()}),
                    )?,
                }
                state.store.mark_wake_sent(&job.id)?;
            }
            if profile.wake_policy != "manual"
                && preparation_expired(&job, state.config.external_timeout_seconds)
            {
                state.store.fail_job(&job.id, json!({"layer":"node","code":"node_unavailable",
                    "message":"Node did not reconnect before the availability deadline.","retry_safe":true}), false)?;
                release_job(state, &job.id, "stop")?;
            }
            return Ok(());
        }
        let runtime = profile.runtime_profile.as_deref().unwrap_or_default();
        if let Some(problem) = node.network_problem() {
            state.store.fail_job(
                &job.id,
                json!({"layer":"node","code":"selected_network_unavailable",
                "message":problem,"retry_safe":true}),
                false,
            )?;
            release_job(state, &job.id, "stop")?;
            return Ok(());
        }
        if !claim_job_slot(state, &job, &profile)? {
            return Ok(());
        }
        let action = state.store.ensure_action(node_id, &job.id, "ensure", json!({
            "runtime_profile":runtime,"model_id":profile.model_id,"context_tokens":profile.context_tokens,
            "idle_seconds":profile.idle_seconds.unwrap_or(0),"lease_id":job.attempt_id,"operation_id":uuid::Uuid::new_v4()}))?;
        match action.status.as_str() {
            "failed" => {
                // Retry-safe: the slot has been released and the next ensure
                // starts clean. A consumer saw a resubmission succeed at once
                // (an unmanaged server had held the port) but had no field
                // saying it might, so an automated caller would have escalated.
                state.store.fail_job(
                    &job.id,
                    json!({"layer":"runtime","code":"ensure_failed",
                "message":"Node could not prepare the configured runtime.","detail":action.result,
                "retry_safe":true}),
                    false,
                )?;
                release_job(state, &job.id, "stop")?;
                return Ok(());
            }
            "completed" => {
                ensured_identity = action
                    .result
                    .as_ref()
                    .and_then(|result| result.get("identity"))
                    .cloned();
                match crate::server::runtime::prepared_model_id(
                    &profile,
                    action.result.as_ref().unwrap_or(&json!({})),
                ) {
                    Ok(model_id) => runtime_model_id = model_id,
                    Err(error) => {
                        state.store.fail_job(
                            &job.id,
                            json!({"layer":"runtime","code":"ensure_failed",
                                "message":"Node did not return a usable managed model.",
                                "detail":error.to_string(),"retry_safe":true}),
                            false,
                        )?;
                        release_job(state, &job.id, "stop")?;
                        return Ok(());
                    }
                }
            }
            _ => {
                if job.status != "preparing" {
                    state.store.set_job_status(
                        &job.id,
                        "preparing",
                        "runtime_starting",
                        "Preparing the model.",
                    )?;
                }
                return Ok(());
            }
        }
    } else if job.status != "preparing" {
        state.store.set_job_status(
            &job.id,
            "preparing",
            "endpoint_check",
            "Checking whether the model server lists the requested model.",
        )?;
        return Ok(());
    }
    let endpoint = package.endpoint;
    match state.inference.probe(&endpoint, &runtime_model_id).await {
        Ok(mut evidence) => {
            if let (Some(identity), Some(fields)) = (ensured_identity, evidence.as_object_mut()) {
                fields.insert("serving_identity".into(), identity);
            }
            state.store.add_event(
                &job.id,
                "model_ready",
                "The model server lists the requested model.",
                evidence,
            )?;
            state.store.set_job_status(
                &job.id,
                "ready",
                "queued_for_runner",
                "Waiting for an available runner.",
            )?;
        }
        Err(error) if preparation_expired(&job, state.config.external_timeout_seconds) => {
            state.store.fail_job(
                &job.id,
                json!({"layer":"inference","code":"readiness_timeout",
                "message":error.to_string(),"retry_safe":true}),
                false,
            )?;
            release_job(state, &job.id, "stop")?;
        }
        Err(_) => {}
    }
    Ok(())
}

fn preparation_expired(job: &Job, timeout: u64) -> bool {
    DateTime::parse_from_rfc3339(&job.updated_at)
        .ok()
        .is_some_and(|start| {
            Utc::now()
                .signed_duration_since(start.with_timezone(&Utc))
                .num_seconds()
                >= timeout as i64
        })
}

fn missing(value: &str) -> serde_json::Value {
    json!({"layer":"controller","code":"missing_configuration","message":format!("Resolved {value} no longer exists.")})
}

fn claim_job_slot(state: &AppState, job: &Job, profile: &ModelProfile) -> Result<bool> {
    let Some(slot) = Occupancy::exclusive_slot(profile) else {
        return Ok(true);
    };
    match state.occupancy.try_claim_job(&slot, &job.id) {
        Ok(()) => Ok(true),
        Err(Occupied::Interactive) => {
            if job.status != "waiting_capacity" {
                state.store.set_job_status(
                    &job.id,
                    "waiting_capacity",
                    "capacity_in_use",
                    "Waiting for an interactive session to finish.",
                )?;
            }
            Ok(false)
        }
        Err(Occupied::Job { .. }) => {
            if job.status != "waiting_capacity" {
                state.store.set_job_status(
                    &job.id,
                    "waiting_capacity",
                    "capacity_queued",
                    "Waiting for another job to finish.",
                )?;
            }
            Ok(false)
        }
        Err(Occupied::Reserved) => {
            if job.status != "waiting_capacity" {
                state.store.set_job_status(
                    &job.id,
                    "waiting_capacity",
                    "capacity_queued",
                    "Waiting in line for the model; requests ahead of it go first.",
                )?;
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
