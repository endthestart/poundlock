use super::{ApiError, ApiResult, AppState, admin_auth, api_token_auth};
use crate::domain::{Event, Job, SubmitJob};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/jobs",
            get(list_jobs)
                .post(submit_job)
                .layer(DefaultBodyLimit::max(crate::source::MAX_SOURCE_JSON)),
        )
        .route("/api/v1/job-options", get(job_options))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/events", get(get_events))
        .route("/api/v1/jobs/{id}/patches/{index}", get(download_patch))
        .route("/api/v1/jobs/{id}/cancel", post(cancel_job))
        .route_layer(middleware::from_fn_with_state(state, authorize_job_request))
}

async fn authorize_job_request(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let permission = if request.method() == axum::http::Method::GET {
        "jobs:read"
    } else if request.uri().path().ends_with("/cancel") {
        "jobs:cancel"
    } else {
        "jobs:submit"
    };
    if admin_auth(&state, request.headers()).is_err() {
        match api_token_auth(&state, request.headers(), permission) {
            Ok(principal) => {
                request.extensions_mut().insert(principal);
            }
            Err(error) => return error.into_response(),
        }
    }
    next.run(request).await
}

async fn job_options(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
) -> ApiResult<Json<Value>> {
    let operator = admin_auth(&state, &headers).is_ok();
    let mut models = Vec::new();
    for model in state
        .store
        .list_model_profiles()?
        .into_iter()
        .filter(|model| model.uses.contains("durable"))
    {
        let endpoint = state
            .store
            .endpoint_secret(&model.endpoint_id)?
            .ok_or_else(|| ApiError::unavailable("configured endpoint is unavailable"))?;
        let revision = state.store.routing_revision(&model)?;
        let mut value =
            durable_job_model(&model, &endpoint.endpoint.kind, &revision, &state.occupancy);
        if let Some(Extension(principal)) = &principal {
            let mut access =
                state
                    .managed_identities
                    .access_for_job(principal, &model, &endpoint.endpoint)?;
            if value["busy"] == true && access["availability"] == "available" {
                access["availability"] = json!("busy");
                access["availability_reason"] = json!("capacity_in_use");
            }
            super::relay::apply_access_metadata(&mut value, &access);
        } else if operator {
            value["connection_mode"] = json!(crate::litellm::connection_mode(&endpoint.endpoint));
        }
        models.push(value);
    }
    Ok(Json(
        json!({"schema_version":crate::contract::SCHEMA_VERSION,"contract_revision":crate::contract::CONTRACT_REVISION,
        "role":if operator { "operator" } else { "developer" },
        "permissions":(crate::domain::API_TOKEN_PERMISSIONS.into_iter().filter(|permission| operator || api_token_auth(&state,&headers,permission).is_ok()).collect::<Vec<_>>()),
        "models":models,
        "harnesses":state.store.list_harness_profiles()?,
        "git_hosts":state.config.git_hosts,
        "outbound_network_allowed":state.config.allow_job_network,
        "runner_status":state.runners.status(),
        "source_limits":{"bytes":crate::source::MAX_SOURCE_BYTES,"files":10000},
        "active_jobs_per_token":state.config.max_active_jobs_per_token,
        "verifier":crate::verifier_toolchain::advertised(),
        "effective_budgets":effective_budgets(),
        "policy_limits":policy_limits()}),
    ))
}

/// The real per-attempt budgets `max_repairs` implies, computed from the
/// same functions the runner uses to enforce them (`job_policy.rs`) rather
/// than a second, hand-maintained table. `max_repairs` ranges 0-3
/// (`JobPolicy::validate`), so every possible value is listed; a caller
/// looks up the row matching the `max_repairs` it plans to submit.
///
/// Wall time is deliberately not included here: it is configured per runner
/// process (`POUNDLOCK_JOB_WALL_SECONDS`), and this table is shown before any
/// runner has been chosen for a specific job, so there is no real value to
/// surface without guessing at which runner will claim it. Once a runner
/// does claim a job, it reports its own `wall_seconds`, and the job resource
/// (`get_job`/`submit_job`, via `Store::job_wall_budget`) reports that value
/// alongside the computed `deadline_at` — `null` until claimed.
/// The bounds a submitted policy must satisfy, read from the constants that
/// enforce them. Every one of these was previously discoverable only by
/// submitting a job and reading the 400 — a consumer found the verification
/// timeout range exactly that way.
fn policy_limits() -> Value {
    use crate::job_policy as policy;
    json!({
        "max_repairs": policy::MAX_REPAIRS,
        "verification_commands": policy::MAX_VERIFICATION_COMMANDS,
        "protected_paths": policy::MAX_PROTECTED_PATHS,
        "policy_bytes": policy::MAX_POLICY_BYTES,
        "verification_timeout_seconds": {
            "min": policy::MIN_VERIFICATION_TIMEOUT_SECONDS,
            "max": policy::MAX_VERIFICATION_TIMEOUT_SECONDS,
            "default": policy::DEFAULT_VERIFICATION_TIMEOUT_SECONDS,
        },
    })
}

fn effective_budgets() -> Vec<Value> {
    (0..=3u8)
        .map(|max_repairs| {
            json!({
                "max_repairs":max_repairs,
                "attempts":crate::job_policy::effective_attempts(max_repairs),
                "output_bytes_per_stream":crate::job_policy::effective_output_bytes_per_stream(max_repairs),
                "patch_bytes":crate::job_policy::effective_patch_bytes(max_repairs),
            })
        })
        .collect()
}

pub(crate) fn durable_job_model(
    profile: &crate::domain::ModelProfile,
    endpoint_kind: &str,
    routing_revision: &str,
    occupancy: &super::capacity::Occupancy,
) -> Value {
    let mut value = serde_json::to_value(profile).unwrap_or_else(|_| json!({}));
    let (busy, occupancy_json) = super::relay::client_occupancy(profile, occupancy);
    value["endpoint_kind"] = json!(endpoint_kind);
    value["routing_revision"] = json!(routing_revision);
    value["busy"] = json!(busy);
    value["occupancy"] = occupancy_json;
    // Serializing the profile emits the stored measurements only. Publish the
    // same described view `/v1/models` does, so a reader never has to decide
    // for itself whether a measurement is stale or still describes this
    // profile's configuration.
    value["capabilities"] = json!(crate::domain::describe_capabilities(profile));
    value["status"] = json!(if profile.retirement.disabled.is_some() {
        "disabled"
    } else {
        "enabled"
    });
    value
}

/// A token sees its own jobs; an operator sees every job.
///
/// `api_token_id` was recorded at submission and never consulted again, so
/// holding `jobs:read` was enough to read any other token's job — including
/// its patches, which are that project's source. Per-token managed identities
/// and per-token budgets already treat a token as a tenant; this makes the job
/// routes agree.
fn tenant(
    state: &AppState,
    headers: &HeaderMap,
    principal: Option<&crate::domain::ApiTokenPrincipal>,
) -> Option<String> {
    if admin_auth(state, headers).is_ok() {
        return None;
    }
    principal.map(|principal| principal.id.clone())
}

/// `Ok(())` when this caller may act on `id`. A job belonging to someone else
/// is reported as missing rather than forbidden: whether a job id exists is
/// itself not this caller's business.
fn require_tenant_job(
    state: &AppState,
    headers: &HeaderMap,
    principal: Option<&crate::domain::ApiTokenPrincipal>,
    id: &str,
) -> ApiResult<()> {
    let owner = state
        .store
        .job_api_token_id(id)?
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    match tenant(state, headers, principal) {
        None => Ok(()),
        Some(caller) if owner.as_deref() == Some(caller.as_str()) => Ok(()),
        Some(_) => Err(ApiError::not_found("job not found")),
    }
}

async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
) -> ApiResult<Json<Vec<Job>>> {
    let principal = principal.map(|Extension(value)| value);
    let scope = tenant(&state, &headers, principal.as_ref());
    Ok(Json(state.store.list_jobs_for(250, scope.as_deref())?))
}

async fn submit_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
    Json(mut input): Json<SubmitJob>,
) -> ApiResult<Json<Value>> {
    if !input.project_id.is_empty() && admin_auth(&state, &headers).is_err() {
        return Err(ApiError::bad(
            "Registered server repositories require operator access. Supply files for this job instead.",
        ));
    }
    input
        .policy
        .validate()
        .map_err(|error| ApiError::bad(error.to_string()))?;
    if input.policy.network == crate::job_policy::JobNetwork::Outbound
        && !state.config.allow_job_network
    {
        return Err(ApiError::bad(
            "Outbound job network access is disabled by the operator.",
        ));
    }
    input.prompt = input.prompt.trim().to_owned();
    if input.prompt.is_empty() {
        return Err(ApiError::bad("prompt is required"));
    }
    if input.prompt.len() > 256 * 1024 {
        return Err(ApiError::bad("prompt exceeds 256 KiB"));
    }
    if input.idempotency_key.trim().is_empty() {
        return Err(ApiError::bad("idempotency_key is required"));
    }
    if input.project_id.is_empty() == input.source.is_none() {
        return Err(ApiError::bad(
            "Supply source or a registered project, not both.",
        ));
    }
    if let Some(crate::source::JobSource::Git { repository, .. }) = &input.source
        && !crate::git_source::host_allowed(repository, &state.config.git_hosts)
    {
        return Err(ApiError::bad(
            "Repository host is not approved by the operator. Use an approved host or upload a folder.",
        ));
    }
    if let Some(source) = &input.source {
        source
            .validate()
            .map_err(|error| ApiError::bad(error.to_string()))?;
    }
    if !input.project_id.is_empty() && state.store.project(&input.project_id)?.is_none() {
        return Err(ApiError::bad("project_id does not exist"));
    }
    let model = state
        .store
        .model_profile(&input.model_profile_id)?
        .ok_or_else(|| ApiError::bad("model_profile_id does not exist"))?;
    if !model.uses.contains("durable") {
        return Err(ApiError::bad(
            "model_profile_id is not configured for durable jobs",
        ));
    }
    if let Some(disabled) = &model.retirement.disabled {
        return Err(ApiError::unavailable_code(
            "model_disabled",
            format!(
                "this model was taken out of service: {}. An operator can retest or re-enable it",
                disabled.reason
            ),
        ));
    }
    let harness = state
        .store
        .harness_profile(&input.harness_profile_id)?
        .ok_or_else(|| ApiError::bad("harness_profile_id does not exist"))?;
    if harness.kind == "pi" {
        crate::harness::validate_pi_context(model.context_tokens)
            .map_err(|error| ApiError::bad(error.to_string()))?;
    }
    if let Some(Extension(principal)) = &principal {
        if let Some(code) = crate::litellm::destination_block_reason(principal, &model) {
            let message = if code == "data_destination_confirmation_required" {
                "the operator must confirm where this profile processes data before it can be used"
            } else {
                "this application token is not allowed to send data to external models"
            };
            return Err(ApiError::forbidden(code, message));
        }
        let endpoint = state
            .store
            .endpoint_secret(&model.endpoint_id)?
            .ok_or_else(|| ApiError::unavailable("configured endpoint is unavailable"))?;
        state
            .managed_identities
            .credential_for(principal, &endpoint, &model, "jobs:submit")
            .await
            .map_err(|error| {
                tracing::warn!(api_token_id=%principal.id, endpoint_id=%endpoint.endpoint.id, %error, "managed LiteLLM job identity is unavailable");
                ApiError::unavailable("managed gateway identity is unavailable; operator reconciliation is required")
            })?;
    }
    state
        .store
        .submit_job_for(
            &input,
            principal.as_ref().map(|Extension(value)| value.id.as_str()),
            state.config.max_active_jobs_per_token,
        )
        .and_then(|job| {
            // The same lookup a caller would otherwise have to make against
            // job-options, attached to the job actually submitted so it does
            // not have to cross-reference max_repairs itself.
            let max_repairs = job.policy.max_repairs;
            let mut value = serde_json::to_value(&job).unwrap_or_else(|_| json!({}));
            value["effective_budgets"] = json!({
                "max_repairs":max_repairs,
                "attempts":crate::job_policy::effective_attempts(max_repairs),
                "output_bytes_per_stream":crate::job_policy::effective_output_bytes_per_stream(max_repairs),
                "patch_bytes":crate::job_policy::effective_patch_bytes(max_repairs),
            });
            attach_wall_budget(&state, &job.id, &mut value)?;
            Ok(Json(value))
        })
        .map_err(|error: anyhow::Error| {
            if error.to_string().contains("idempotency key") {
                ApiError::conflict("idempotency key was reused for different input")
            } else {
                error.into()
            }
        })
}

/// Merges the job's runner-reported `wall_seconds` and the controller's
/// computed `deadline_at` into `value` (see `Store::job_wall_budget`). Both
/// are `null` before a runner has claimed the job and reported its
/// configured budget — this never guesses a value, unlike `effective_budgets`
/// which is knowable at submission time from policy alone.
fn attach_wall_budget(state: &AppState, id: &str, value: &mut Value) -> anyhow::Result<()> {
    let (wall_seconds, deadline_at) = state.store.job_wall_budget(id)?;
    value["wall_seconds"] = json!(wall_seconds);
    value["deadline_at"] = json!(deadline_at);
    Ok(())
}

async fn get_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_tenant_job(
        &state,
        &headers,
        principal.map(|Extension(v)| v).as_ref(),
        &id,
    )?;
    let job = state
        .store
        .job(&id)?
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    let mut value = serde_json::to_value(&job).unwrap_or_else(|_| json!({}));
    attach_wall_budget(&state, &id, &mut value)?;
    Ok(Json(value))
}

async fn get_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<Event>>> {
    require_tenant_job(
        &state,
        &headers,
        principal.map(|Extension(v)| v).as_ref(),
        &id,
    )?;
    if state.store.job(&id)?.is_none() {
        return Err(ApiError::not_found("job not found"));
    }
    Ok(Json(state.store.events(&id, query.after.unwrap_or(0))?))
}

async fn cancel_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_tenant_job(
        &state,
        &headers,
        principal.map(|Extension(v)| v).as_ref(),
        &id,
    )?;
    if state.store.job(&id)?.is_none() {
        return Err(ApiError::not_found("job not found"));
    }
    Ok(Json(
        json!({"cancel_requested":state.store.request_cancel(&id)?}),
    ))
}

#[derive(Deserialize)]
struct EventQuery {
    after: Option<i64>,
}

#[derive(Deserialize)]
struct PatchQuery {
    attempt: Option<usize>,
}

async fn download_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    principal: Option<Extension<crate::domain::ApiTokenPrincipal>>,
    Path((id, index)): Path<(String, usize)>,
    Query(query): Query<PatchQuery>,
) -> ApiResult<Response> {
    // A patch is the project's own source. This is the read that made the
    // missing tenant check matter.
    require_tenant_job(
        &state,
        &headers,
        principal.map(|Extension(v)| v).as_ref(),
        &id,
    )?;
    let job = state
        .store
        .job(&id)?
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    let result = job
        .result
        .ok_or_else(|| ApiError::not_found("No patch is available yet."))?;
    let previous = result["provenance"]["previous_attempts"].as_array();
    let final_attempt = previous.map_or(0, Vec::len) + 1;
    let attempt = query.attempt.unwrap_or(final_attempt);
    let patches = if attempt == final_attempt {
        &result["patches"]
    } else {
        &previous
            .and_then(|items| attempt.checked_sub(1).and_then(|n| items.get(n)))
            .ok_or_else(|| ApiError::not_found("attempt not found"))?["patches"]
    };
    let patch = patches
        .get(index)
        .and_then(|p| p["patch"].as_str())
        .ok_or_else(|| ApiError::not_found("patch not found"))?;
    Ok((
        [
            ("content-type", "text/x-diff; charset=utf-8".to_owned()),
            (
                "content-disposition",
                format!("attachment; filename=\"poundlock-attempt-{attempt}-root-{index}.patch\""),
            ),
            ("cache-control", "no-store".to_owned()),
            ("x-content-type-options", "nosniff".to_owned()),
        ],
        patch.to_owned(),
    )
        .into_response())
}
