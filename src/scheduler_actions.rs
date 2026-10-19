use crate::domain::Job;
use crate::server::AppState;
use anyhow::Result;
use serde_json::json;

pub(super) fn queue_runtime_action(state: &AppState, job: &Job, kind: &str) -> Result<()> {
    let Some(package) = state.store.runner_package(&job.id)? else {
        return Ok(());
    };
    let (Some(node_id), Some(runtime)) = (package.model.node_id, package.model.runtime_profile)
    else {
        return Ok(());
    };
    state.store.enqueue_action(
        &node_id,
        Some(&job.id),
        kind,
        json!({
            "runtime_profile": runtime,
            "lease_id": job.attempt_id,
            "operation_id": uuid::Uuid::new_v4()
        }),
    )?;
    Ok(())
}

pub(crate) fn release_job(state: &AppState, id: &str, kind: &str) -> Result<()> {
    state.occupancy.release_job(id);
    let Some(job) = state.store.job(id)? else {
        return Ok(());
    };
    if kind != "release" {
        return queue_runtime_action(state, &job, kind);
    }
    let Some(package) = state.store.runner_package(&job.id)? else {
        return Ok(());
    };
    let (Some(node_id), Some(runtime)) = (package.model.node_id, package.model.runtime_profile)
    else {
        return Ok(());
    };
    state.store.ensure_action(
        &node_id,
        &job.id,
        kind,
        json!({
            "runtime_profile": runtime,
            "lease_id": job.attempt_id,
            "operation_id": uuid::Uuid::new_v4()
        }),
    )?;
    Ok(())
}
