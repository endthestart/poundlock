use super::{ApiError, ApiResult, AppState, runner_auth};
use crate::domain::{RunnerPackage, RunnerResult};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// How long after its last authenticated contact a runner is still reported
/// as connected. Idle runners poll for work every few seconds.
pub(crate) const RUNNER_PRESENCE_SECONDS: u64 = 60;

/// In-memory record of the most recent authenticated runner contact. It is a
/// diagnostic signal for clients, not an admission gate: queued jobs wait for
/// a runner either way.
#[derive(Clone, Default)]
pub struct RunnerPresence(Arc<std::sync::Mutex<Option<std::time::Instant>>>);

impl RunnerPresence {
    pub fn record(&self) {
        if let Ok(mut last) = self.0.lock() {
            *last = Some(std::time::Instant::now());
        }
    }

    pub fn status(&self) -> &'static str {
        let recent = self
            .0
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|last| {
                last.elapsed() <= std::time::Duration::from_secs(RUNNER_PRESENCE_SECONDS)
            });
        if recent { "connected" } else { "not_seen" }
    }
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/runner/v1/claim", post(claim))
        .route("/runner/v1/jobs/{id}/source", get(source))
        .route("/runner/v1/jobs/{id}/heartbeat", post(heartbeat))
        .route("/runner/v1/jobs/{id}/events", post(event))
        .route(
            "/runner/v1/jobs/{id}/complete",
            post(complete)
                // 10 MiB patches + 4 MiB output can expand sixfold in JSON.
                .layer(DefaultBodyLimit::max(90 * 1024 * 1024)),
        )
        .route(
            "/runner/v1/jobs/{id}/fail",
            post(fail).layer(DefaultBodyLimit::max(90 * 1024 * 1024)),
        )
        .route_layer(middleware::from_fn_with_state(
            (state, Arc::new(Semaphore::new(2))),
            authorize_request,
        ))
}

async fn authorize_request(
    State((state, uploads)): State<(AppState, Arc<Semaphore>)>,
    request: Request,
    next: Next,
) -> Response {
    if let Err(error) = runner_auth(&state, request.headers()) {
        return error.into_response();
    }
    state.runners.record();
    // Admit bounded large reports before any body is buffered or deserialized.
    // Other runner routes remain available while reports are being uploaded.
    let _permit = if request.uri().path().ends_with("/complete")
        || request.uri().path().ends_with("/fail")
    {
        match uploads.try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return ApiError::unavailable("Result uploads busy; retry shortly").into_response();
            }
        }
    } else {
        None
    };
    next.run(request).await
}

async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ClaimRequest>,
) -> ApiResult<Json<Option<RunnerPackage>>> {
    runner_auth(&state, &headers)?;
    let Some(id) = state.store.claim_runner_job(input.wall_seconds)? else {
        return Ok(Json(None));
    };
    let mut package = state.store.runner_package(&id)?;
    if let Some(package) = &mut package {
        // The harness reaches only the controller's job-scoped inference route.
        // Operator endpoint credentials and network addresses stay here.
        package.endpoint.api_key = state.store.issue_job_inference_token(&id)?;
        package.endpoint.endpoint.base_url = format!("/job/v1/{id}");
        // The runner has no database access and so cannot resolve this
        // itself; do it once, here, at the freshest point before execution.
        package.serving_identity = serving_identity(&state, &id, &package.model);
    }
    if package.is_none() {
        state.store.fail_job(
            &id,
            json!({"layer":"controller","code":"missing_configuration",
            "message":"A resolved project, model, endpoint, or harness no longer exists."}),
            false,
        )?;
        crate::scheduler::release_job(&state, &id, "stop")?;
    }
    Ok(Json(package))
}

/// Best-effort resolved serving identity for job provenance: the operator's
/// display label plus what the node reported about the runtime this job was
/// prepared on. The alias (`model.model_id`) is always in provenance
/// separately and is left alone; this only adds what is actually known.
/// Returns `None` when nothing is — provenance must never invent a value.
///
/// The node's report must describe *this* job's model. The periodic inventory
/// refreshes every five minutes, and a job is claimed seconds after its own
/// ensure, so reading it here would name whichever model the previous job
/// loaded, recording one model's run under another's name. The identity
/// taken right after this job's ensure is preferred. The periodic report is
/// used only when it was taken after this job's model was ready; otherwise the
/// field says why it is absent rather than carrying an older model's name.
fn serving_identity(
    state: &AppState,
    job_id: &str,
    model: &crate::domain::ModelProfile,
) -> Option<Value> {
    let mut identity = serde_json::Map::new();
    if let Some(label) = &model.model_label {
        identity.insert("model_label".into(), json!(label));
    }
    let ready = state.store.events(job_id, 0).ok().and_then(|events| {
        events
            .into_iter()
            .rev()
            .find(|event| event.kind == "model_ready")
    });
    if let Some(reported) = ready
        .as_ref()
        .and_then(|event| event.payload.get("serving_identity"))
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
    {
        identity.insert("node_reported".into(), reported.clone());
        identity.insert("node_reported_at".into(), json!("model_ready"));
    } else if let Some(node_id) = &model.node_id
        && let Some(runtime) = &model.runtime_profile
        && let Ok(Some(node)) = state.store.node(node_id, state.config.node_offline_seconds)
        && let Some(reported) = node
            .capabilities
            .get("inventory")
            .and_then(|value| value.get("runtimes"))
            .and_then(|value| value.get(runtime))
            .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
    {
        if observed_after(
            reported,
            ready.as_ref().map(|event| event.created_at.as_str()),
        ) {
            identity.insert("node_reported".into(), reported.clone());
            identity.insert("node_reported_at".into(), json!("inventory"));
        } else {
            identity.insert(
                "node_reported_unavailable".into(),
                json!("the node's last report predates this job's model preparation"),
            );
        }
    }
    (!identity.is_empty()).then(|| Value::Object(identity))
}

/// Whether a periodic identity report was taken after `ready`. Unknown on
/// either side counts as not after: a missing timestamp cannot prove it.
fn observed_after(reported: &Value, ready: Option<&str>) -> bool {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    match (
        reported
            .get("checked_at")
            .and_then(Value::as_str)
            .and_then(parse),
        ready.and_then(parse),
    ) {
        (Some(checked), Some(ready)) => checked >= ready,
        _ => false,
    }
}

async fn source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::source::JobSource>> {
    runner_auth(&state, &headers)?;
    Ok(Json(state.store.runner_source(&id)?.ok_or_else(|| {
        ApiError::not_found("source unavailable or runner lease inactive")
    })?))
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    runner_auth(&state, &headers)?;
    Ok(Json(
        json!({"cancel_requested":state.store.runner_heartbeat(&id)?}),
    ))
}

async fn event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<RunnerEvent>,
) -> ApiResult<Json<Value>> {
    runner_auth(&state, &headers)?;
    state
        .store
        .add_event(&id, &input.kind, &input.message, input.payload)?;
    Ok(Json(json!({"accepted":true})))
}

async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(result): Json<RunnerResult>,
) -> ApiResult<Json<Value>> {
    runner_auth(&state, &headers)?;
    if result
        .patches
        .iter()
        .map(|patch| patch.patch.len())
        .sum::<usize>()
        > 10 * 1024 * 1024
        || result.output.len() > 4 * 1024 * 1024
    {
        return Err(ApiError::bad("result exceeds patch or output limit"));
    }
    state.store.complete_job(&id, &result)?;
    crate::scheduler::release_job(&state, &id, "release")?;
    Ok(Json(json!({"accepted":true})))
}

async fn fail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<RunnerFailure>,
) -> ApiResult<Json<Value>> {
    runner_auth(&state, &headers)?;
    if state.store.job(&id)?.is_none() {
        return Err(ApiError::not_found("job not found"));
    }
    if let Some(result) = &input.result
        && (result
            .patches
            .iter()
            .map(|patch| patch.patch.len())
            .sum::<usize>()
            > 10 * 1024 * 1024
            || result.output.len() > 4 * 1024 * 1024)
    {
        return Err(ApiError::bad("result exceeds patch or output limit"));
    }
    state
        .store
        .fail_job_with_result(&id, input.error, input.cancelled, input.result.as_ref())?;
    crate::scheduler::release_job(&state, &id, "stop")?;
    Ok(Json(json!({"accepted":true})))
}

/// `wall_seconds` is the claiming runner's own configured wall budget
/// (`POUNDLOCK_JOB_WALL_SECONDS`); absent (or omitted entirely, for an
/// older runner build) leaves the job's stored budget and deadline unknown
/// rather than guessed (`Store::claim_runner_job`).
#[derive(Deserialize, Default)]
struct ClaimRequest {
    #[serde(default)]
    wall_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct RunnerEvent {
    kind: String,
    message: String,
    #[serde(default)]
    payload: Value,
}
#[derive(Deserialize)]
struct RunnerFailure {
    error: Value,
    result: Option<RunnerResult>,
    #[serde(default)]
    cancelled: bool,
}

#[cfg(test)]
mod serving_identity_tests {
    use super::*;
    use crate::domain::EnrollNode;
    use crate::server::ops_tests::state_with_artifacts;

    /// A node whose periodic report still describes the model the previous
    /// job loaded, and a job about to be claimed on it.
    fn fixture(
        reported_at: &str,
    ) -> (
        tempfile::TempDir,
        AppState,
        String,
        crate::domain::ModelProfile,
    ) {
        let (directory, state) = state_with_artifacts(None);
        let token = state.store.create_enrollment_token(15, None).unwrap();
        let node_id = state
            .store
            .enroll_node(&EnrollNode {
                token,
                name: "vega".into(),
                platform: "macos".into(),
                capabilities: json!({"runtimes":["local-general"],"inventory":{"runtimes":{
                    "local-general":{"model":"previous-model","checked_at":reported_at}}}}),
            })
            .unwrap()
            .node_id;
        let endpoint = state.store.create_endpoint(&serde_json::from_value(json!({"name":"mac","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""})).unwrap()).unwrap();
        let model = state.store.create_model_profile(&serde_json::from_value(json!({"name":"vega-manager","endpoint_id":endpoint.id,"model_id":"vega-manager","context_tokens":65536,"max_output_tokens":8192,"workload_class":"durable-coding-agent","wake_policy":"never","node_id":node_id,"runtime_profile":"local-general"})).unwrap()).unwrap();
        let job = state.store.submit_job(&serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":reported_at,"model_profile_id":model.id,"harness_profile_id":"pi","prompt":"test"})).unwrap()).unwrap();
        (directory, state, job.id, model)
    }

    #[test]
    fn names_the_model_this_job_loaded_not_the_one_before_it() {
        let (_directory, state, job, model) = fixture("2026-01-01T00:00:00+00:00");
        state
            .store
            .add_event(
                &job,
                "model_ready",
                "ready",
                json!({"serving_identity":{"model":"this-model"}}),
            )
            .unwrap();
        let identity = serving_identity(&state, &job, &model).unwrap();
        assert_eq!(identity["node_reported"]["model"], "this-model");
        assert_eq!(identity["node_reported_at"], "model_ready");
    }

    #[test]
    fn refuses_a_periodic_report_older_than_this_jobs_preparation() {
        // A node agent that predates the post-ensure identity. Its report was
        // taken before this job's model loaded, so it names someone else's.
        let (_directory, state, job, model) = fixture("2026-01-01T00:00:00+00:00");
        state
            .store
            .add_event(&job, "model_ready", "ready", json!({}))
            .unwrap();
        let identity = serving_identity(&state, &job, &model).unwrap();
        assert!(identity.get("node_reported").is_none(), "{identity}");
        assert!(identity["node_reported_unavailable"].is_string());
    }

    #[test]
    fn keeps_a_periodic_report_taken_after_the_model_was_ready() {
        let (_directory, state, job, model) = fixture("2999-01-01T00:00:00+00:00");
        state
            .store
            .add_event(&job, "model_ready", "ready", json!({}))
            .unwrap();
        let identity = serving_identity(&state, &job, &model).unwrap();
        assert_eq!(identity["node_reported"]["model"], "previous-model");
        assert_eq!(identity["node_reported_at"], "inventory");
    }
}
