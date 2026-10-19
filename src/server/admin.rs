use super::{AdminAuth, ApiError, ApiResult, AppState, admin_auth};
use crate::domain::*;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

/// The nightly database backup: whether it is on, and how its last run went.
async fn backup_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    Ok(Json(match &state.config.backup {
        Some(backup) => {
            let dir = backup.dir.clone();
            let last = tokio::task::spawn_blocking(move || crate::backup::last_status(&dir))
                .await
                .ok()
                .flatten();
            json!({
                "configured": true,
                "dir": backup.dir,
                "at_utc": backup.at.format("%H:%M").to_string(),
                "last": last,
            })
        }
        None => json!({"configured": false}),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/backup", get(backup_status))
        .route(
            "/api/v1/endpoints",
            get(list_endpoints).post(create_endpoint),
        )
        .route(
            "/api/v1/endpoints/{id}",
            axum::routing::patch(patch_endpoint).delete(delete_endpoint),
        )
        .route("/api/v1/endpoints/{id}/test", post(test_endpoint))
        .route(
            "/api/v1/endpoints/{id}/managed-identities/rotate",
            post(rotate_managed_identities),
        )
        .route(
            "/api/v1/managed-identities/reconcile",
            post(reconcile_managed_identities),
        )
        .route(
            "/api/v1/model-profiles",
            get(list_models).post(create_model),
        )
        .route(
            "/api/v1/model-profiles/{id}",
            axum::routing::patch(patch_model).delete(delete_model),
        )
        .route("/api/v1/model-profiles/{id}/test", post(test_model))
        .route("/api/v1/harness-profiles", get(list_harnesses))
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{id}",
            axum::routing::patch(patch_project).delete(delete_project),
        )
}

async fn overview(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"git_hosts":state.config.git_hosts,"outbound_network_allowed":state.config.allow_job_network,
        "endpoints":state.store.list_endpoints()?, "models":state.store.list_model_profiles()?,
        "harnesses":state.store.list_harness_profiles()?, "projects":state.store.list_projects()?,
        "nodes":state.store.list_nodes(state.config.node_offline_seconds)?,
        "api_tokens":state.store.list_api_tokens()?,
        "managed_identities":state.store.managed_litellm_key_statuses()?,
        "jobs":state.store.list_jobs(100)?,
        "occupancy":state.occupancy.named_snapshot(&state.store.list_model_profiles()?)}),
    ))
}

async fn list_endpoints(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Endpoint>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(state.store.list_endpoints()?))
}

#[derive(Deserialize)]
struct NewEndpoint {
    #[serde(flatten)]
    endpoint: CreateEndpoint,
    #[serde(default)]
    public_url: Option<String>,
    /// A model server's declaration of where it sends prompts, inherited by
    /// every model on it. Gateways declare per route instead.
    #[serde(default)]
    data_destination: Option<String>,
}

async fn create_endpoint(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(NewEndpoint {
        endpoint: mut input,
        public_url,
        data_destination,
    }): Json<NewEndpoint>,
) -> ApiResult<Json<Endpoint>> {
    input.name = required(input.name, "endpoint name")?;
    input.base_url = input.base_url.trim_end_matches('/').into();
    validate_endpoint(&input)?;
    validate_server_destination(&input.kind, data_destination.as_deref())?;
    let public_url = public_url_input(public_url)?;
    if input.managed_identity.enabled {
        validate_management_mode(&state, &input).await?;
    }
    let mut endpoint = state.store.create_endpoint(&input)?;
    if public_url.is_some() {
        state
            .store
            .set_endpoint_public_url(&endpoint.id, public_url.as_deref())?;
        endpoint.public_url = public_url;
    }
    if let Some(destination) = data_destination.filter(|_| endpoint.data_destination.is_some()) {
        state
            .store
            .set_endpoint_destination(&endpoint.id, &destination)?;
        endpoint.data_destination = Some(destination);
    }
    Ok(Json(endpoint))
}

/// A gateway's routes each declare where prompts go; only a model server
/// declares it once for all its models.
fn validate_server_destination(kind: &str, destination: Option<&str>) -> ApiResult<()> {
    match destination {
        Some(value) if !crate::domain::DATA_DESTINATIONS.contains(&value) => Err(ApiError::bad(
            "data_destination must be unknown, operator_network or external",
        )),
        Some(value) if kind == "litellm" && value != "unknown" => Err(ApiError::bad(
            "a gateway declares where prompts go for each route, when the route is published",
        )),
        _ => Ok(()),
    }
}

/// An operator-facing address: HTTP(S), no credentials, query or fragment.
/// Blank clears it.
fn public_url_input(value: Option<String>) -> ApiResult<Option<String>> {
    let Some(value) = value.map(|value| value.trim().trim_end_matches('/').to_owned()) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = url::Url::parse(&value)
        .map_err(|_| ApiError::bad("public address must be an absolute URL"))?;
    if !["http", "https"].contains(&parsed.scheme())
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || value.len() > 512
    {
        return Err(ApiError::bad(
            "Use an HTTP or HTTPS public address without credentials, query or fragment",
        ));
    }
    Ok(Some(value))
}

#[derive(Deserialize)]
struct EndpointPatch {
    name: Option<String>,
    kind: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    managed_identity: Option<ManagedIdentityPolicy>,
    /// Absent keeps it; blank clears it.
    public_url: Option<String>,
    /// A model server's declaration; every model on it inherits it.
    data_destination: Option<String>,
}

async fn patch_endpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminAuth,
    Json(input): Json<EndpointPatch>,
) -> ApiResult<Json<Endpoint>> {
    let existing = state
        .store
        .endpoint_secret(&id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    let public_url = input
        .public_url
        .clone()
        .map(|value| public_url_input(Some(value)))
        .transpose()?;
    if let Some(public_url) = &public_url {
        state
            .store
            .set_endpoint_public_url(&id, public_url.as_deref())?;
    }
    validate_server_destination(
        input.kind.as_deref().unwrap_or(&existing.endpoint.kind),
        input.data_destination.as_deref(),
    )?;
    // Only the public address or the destination changed: neither is a
    // re-point, so the endpoint is not re-saved. A new destination moves the
    // routing revision of its models through their own declaration.
    if input.name.is_none()
        && input.kind.is_none()
        && input.api_key.is_none()
        && input.base_url.is_none()
        && input.managed_identity.is_none()
    {
        if let Some(destination) = input
            .data_destination
            .as_deref()
            .filter(|_| existing.endpoint.data_destination.is_some())
        {
            state.store.set_endpoint_destination(&id, destination)?;
        }
        return state
            .store
            .list_endpoints()?
            .into_iter()
            .find(|item| item.id == id)
            .map(Json)
            .ok_or_else(|| ApiError::not_found("endpoint not found"));
    }
    let updated = CreateEndpoint {
        name: required(
            input.name.unwrap_or_else(|| existing.endpoint.name.clone()),
            "endpoint name",
        )?,
        kind: input.kind.unwrap_or_else(|| existing.endpoint.kind.clone()),
        base_url: input
            .base_url
            .unwrap_or_else(|| existing.endpoint.base_url.clone())
            .trim_end_matches('/')
            .into(),
        api_key: input
            .api_key
            .clone()
            .unwrap_or_else(|| existing.api_key.clone()),
        managed_identity: input
            .managed_identity
            .unwrap_or_else(|| existing.endpoint.managed_identity.clone()),
    };
    validate_endpoint(&updated)?;
    require_key_for_new_origin(&existing, &updated, input.api_key.is_some())?;
    let credentials_changed =
        existing.endpoint.base_url != updated.base_url || existing.api_key != updated.api_key;
    if updated.managed_identity.enabled
        && (!existing.endpoint.managed_identity.enabled || credentials_changed)
    {
        validate_management_mode(&state, &updated).await?;
    }
    if existing.endpoint.managed_identity.enabled
        && (credentials_changed || !updated.managed_identity.enabled)
    {
        state
            .managed_identities
            .revoke_endpoint_keys(&existing)
            .await
            .map_err(|error| {
                tracing::warn!(endpoint_id=%id, %error, "could not revoke managed identities before endpoint change");
                ApiError::unavailable(
                    "managed client keys could not be revoked; endpoint settings were not changed",
                )
            })?;
    }
    Ok(Json(
        state
            .store
            .update_endpoint_with_destination(&id, &updated, input.data_destination.as_deref())?
            .ok_or_else(|| ApiError::not_found("endpoint not found"))?,
    ))
}

fn validate_endpoint(input: &CreateEndpoint) -> ApiResult<()> {
    if !["openai", "litellm"].contains(&input.kind.as_str()) {
        return Err(ApiError::bad("kind must be openai or litellm"));
    }
    if input.managed_identity.enabled && input.kind != "litellm" {
        return Err(ApiError::bad(
            "managed identities are supported only for LiteLLM endpoints",
        ));
    }
    if input.managed_identity.enabled && input.api_key.is_empty() {
        return Err(ApiError::bad(
            "managed identities require a LiteLLM administrative credential",
        ));
    }
    crate::litellm::validate_policy(&input.managed_identity)
        .map_err(|error| ApiError::bad(error.to_string()))?;
    let parsed = url::Url::parse(&input.base_url)
        .map_err(|_| ApiError::bad("base_url must be an absolute URL"))?;
    if !["http", "https"].contains(&parsed.scheme())
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ApiError::bad(
            "Use an HTTP or HTTPS API URL without credentials, query or fragment",
        ));
    }
    Ok(())
}

async fn validate_management_mode(state: &AppState, input: &CreateEndpoint) -> ApiResult<()> {
    state
        .managed_identities
        .validate_management_connection(&input.base_url, &input.api_key)
        .await
        .map_err(|error| {
            tracing::warn!(endpoint = %input.name, %error, "LiteLLM application identity management check failed");
            ApiError::unavailable(
                "LiteLLM application identity management is unavailable; verify the administrative key and database-backed proxy configuration",
            )
        })
}

async fn rotate_managed_identities(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let rotated = state.managed_identities.rotate_endpoint(&id).await?;
    Ok(Json(json!({"rotated": rotated})))
}

async fn reconcile_managed_identities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let reconciled = state.managed_identities.reconcile_pending().await?;
    let pending = state.store.pending_managed_litellm_revocations()?.len();
    Ok(Json(json!({"reconciled": reconciled, "pending": pending})))
}

async fn delete_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let endpoint = state
        .store
        .endpoint_secret(&id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    if endpoint.endpoint.managed_identity.enabled {
        state
            .managed_identities
            .revoke_endpoint_keys(&endpoint)
            .await
            .map_err(|error| {
                tracing::warn!(endpoint_id=%id, %error, "could not revoke managed identities before endpoint removal");
                ApiError::unavailable(
                    "managed client keys could not be revoked; endpoint was not removed",
                )
            })?;
    }
    if !state.store.remove_endpoint(&id)? {
        return Err(ApiError::not_found("endpoint not found"));
    }
    Ok(Json(json!({"removed":true})))
}
async fn delete_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if !state.store.remove_model_profile(&id)? {
        return Err(ApiError::not_found("model profile not found"));
    }
    Ok(Json(json!({"removed":true})))
}
async fn delete_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if !state.store.remove_project(&id)? {
        return Err(ApiError::not_found("project not found"));
    }
    Ok(Json(json!({"removed":true})))
}

async fn test_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let endpoint = state
        .store
        .endpoint_secret(&id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    match state.inference.probe(&endpoint, "").await {
        Ok(mut body) => {
            if endpoint.endpoint.managed_identity.enabled
                && let Err(error) = state
                    .managed_identities
                    .validate_management_connection(&endpoint.endpoint.base_url, &endpoint.api_key)
                    .await
            {
                tracing::warn!(endpoint = %endpoint.endpoint.name, %error, "LiteLLM application identity management check failed");
                let message = "Models are reachable, but application identity management is unavailable. Verify the administrative key and database-backed LiteLLM configuration.";
                state.store.record_endpoint_check(&id, false, message)?;
                return Err(ApiError::unavailable(message));
            }
            if endpoint.endpoint.managed_identity.enabled {
                body["poundlock"] = json!({
                    "connection_mode": "managed_applications",
                    "management": "available",
                });
            }
            let message = if endpoint.endpoint.managed_identity.enabled {
                "Listed models and verified application identity management"
            } else {
                "Listed models"
            };
            state.store.record_endpoint_check(&id, true, message)?;
            Ok(Json(body))
        }
        Err(error) => {
            tracing::warn!(endpoint = %endpoint.endpoint.name, %error, "endpoint compatibility check failed");
            let message = state.inference.check_failure_message(&error, false);
            state.store.record_endpoint_check(&id, false, &message)?;
            Err(ApiError::unavailable(message))
        }
    }
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ModelProfile>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(state.store.list_model_profiles()?))
}

async fn create_model(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(mut input): Json<CreateModelProfile>,
) -> ApiResult<Json<ModelProfile>> {
    input.name = required(input.name, "profile name")?;
    input.model_id = required(input.model_id, "model ID")?;
    if input.idle_seconds.is_some_and(|s| s > 3600) {
        return Err(ApiError::bad("Idle delay must be at most 60 minutes"));
    }
    if state.store.endpoint_secret(&input.endpoint_id)?.is_none() {
        return Err(ApiError::bad("endpoint_id does not exist"));
    }
    if input.context_tokens < 1024
        || input.max_output_tokens == 0
        || input.max_output_tokens > input.context_tokens
    {
        return Err(ApiError::bad("token limits are inconsistent"));
    }
    if !["never", "on-demand", "manual"].contains(&input.wake_policy.as_str()) {
        return Err(ApiError::bad("unknown wake_policy"));
    }
    if input.node_id.is_some() != input.runtime_profile.is_some() {
        return Err(ApiError::bad(
            "node_id and runtime_profile must be configured together",
        ));
    }
    check_servable(
        &state,
        input.node_id.as_deref(),
        input.runtime_profile.as_deref(),
        &input.model_id,
    )?;
    Ok(Json(state.store.create_model_profile(&input)?))
}

/// A runtime that reports which model ids it can serve must be able to serve
/// this one. A profile whose model no machine can load fails only later, at
/// its first lease, with a far less obvious error.
fn check_servable(
    state: &AppState,
    node_id: Option<&str>,
    runtime_profile: Option<&str>,
    model_id: &str,
) -> ApiResult<()> {
    let (Some(node_id), Some(runtime)) = (node_id, runtime_profile) else {
        return Ok(());
    };
    let Some(node) = state
        .store
        .node(node_id, state.config.node_offline_seconds)?
    else {
        return Ok(());
    };
    let Some(servable) = node
        .capabilities
        .get("inventory")
        .and_then(|inventory| inventory.get("runtimes"))
        .and_then(|runtimes| runtimes.get(runtime))
        .and_then(|identity| identity.get("servable"))
        .and_then(Value::as_array)
    else {
        return Ok(());
    };
    let ids: Vec<&str> = servable.iter().filter_map(Value::as_str).collect();
    if ids.is_empty() || ids.contains(&model_id) {
        return Ok(());
    }
    Err(ApiError::bad(format!(
        "{runtime} on this machine serves {}, not {model_id}",
        ids.join(", ")
    )))
}

async fn patch_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminAuth,
    Json(input): Json<PatchModelProfile>,
) -> ApiResult<Json<ModelProfile>> {
    if let Some(endpoint_id) = &input.endpoint_id
        && state.store.endpoint_secret(endpoint_id)?.is_none()
    {
        return Err(ApiError::bad("endpoint_id does not exist"));
    }
    if let (Some(context_tokens), Some(max_output_tokens)) =
        (input.context_tokens, input.max_output_tokens)
        && (context_tokens < 1024 || max_output_tokens == 0 || max_output_tokens > context_tokens)
    {
        return Err(ApiError::bad("token limits are inconsistent"));
    }
    if let Some(wake_policy) = &input.wake_policy
        && !["never", "on-demand", "manual"].contains(&wake_policy.as_str())
    {
        return Err(ApiError::bad("unknown wake_policy"));
    }
    let existing = state
        .store
        .model_profile(&id)?
        .ok_or_else(|| ApiError::not_found("model profile not found"))?;
    check_servable(
        &state,
        input.node_id.as_deref().or(existing.node_id.as_deref()),
        input
            .runtime_profile
            .as_deref()
            .or(existing.runtime_profile.as_deref()),
        input.model_id.as_deref().unwrap_or(&existing.model_id),
    )?;
    Ok(Json(
        state
            .store
            .patch_model_profile(&id, &input)?
            .ok_or_else(|| ApiError::not_found("model profile not found"))?,
    ))
}

async fn test_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminAuth,
    Json(input): Json<ModelTest>,
) -> ApiResult<Json<Value>> {
    let profile = state
        .store
        .model_profile(&id)?
        .ok_or_else(|| ApiError::not_found("model profile not found"))?;
    let endpoint = state
        .store
        .endpoint_secret(&profile.endpoint_id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    if input.generate && endpoint.endpoint.managed_identity.enabled {
        return Err(ApiError::bad(
            "Generate through an application token after access preflight; Poundlock does not use the LiteLLM administrative key for inference",
        ));
    }
    let lease_id = uuid::Uuid::new_v4().to_string();
    let _occupancy_guard = match super::capacity::Occupancy::exclusive_slot(&profile) {
        Some(slot) => match state
            .occupancy
            .try_claim_interactive(slot, lease_id.clone())
        {
            Ok(guard) => Some(guard),
            Err(_) => {
                return Err(ApiError::conflict(
                    "This model is busy. Wait for the current request to finish before retrying.",
                ));
            }
        },
        None => None,
    };
    let mut runtime_lease =
        super::runtime::RuntimeLease::new(state.clone(), profile.clone(), lease_id.clone());
    let runtime_model_id = match super::runtime::prepare_profile(&state, &mut runtime_lease).await {
        Ok(model_id) => model_id,
        Err(error) => {
            tracing::warn!(profile = %profile.name, %error, "model compatibility check could not prepare runtime");
            state
                .store
                .record_model_check(&id, false, "Runtime is unavailable")?;
            return Err(ApiError::unavailable(
                "the configured inference runtime is unavailable",
            ));
        }
    };
    let _runtime_lease = runtime_lease;
    let response = if input.generate && profile.uses.contains("completion") {
        state
            .inference
            .fim_completion_probe(&endpoint, &runtime_model_id)
            .await
    } else if input.generate {
        state
            .inference
            .completion_probe(&endpoint, &runtime_model_id, profile.max_output_tokens)
            .await
    } else {
        state.inference.probe(&endpoint, &runtime_model_id).await
    };
    match response {
        Ok(body) => {
            let message = if input.generate {
                "Generate check passed"
            } else {
                "Identity check passed"
            };
            state.store.record_model_check(&id, true, message)?;
            Ok(Json(body))
        }
        Err(error) => {
            tracing::warn!(profile = %profile.name, %error, "model compatibility check failed");
            let message = state
                .inference
                .check_failure_message(&error, input.generate);
            state.store.record_model_check(&id, false, &message)?;
            Err(ApiError::unavailable(message))
        }
    }
}

async fn list_harnesses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<HarnessProfile>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(state.store.list_harness_profiles()?))
}

async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Project>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(state.store.list_projects()?))
}

async fn create_project(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(mut input): Json<CreateProject>,
) -> ApiResult<Json<Project>> {
    input.name = required(input.name, "project name")?;
    Ok(Json(state.store.create_project(&input)?))
}

async fn patch_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminAuth,
    Json(input): Json<PatchProject>,
) -> ApiResult<Json<Project>> {
    Ok(Json(
        state
            .store
            .patch_project(&id, &input)?
            .ok_or_else(|| ApiError::not_found("project not found"))?,
    ))
}

#[derive(Deserialize)]
struct ModelTest {
    #[serde(default)]
    generate: bool,
}

fn required(value: String, name: &str) -> ApiResult<String> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        Err(ApiError::bad(format!("{name} is required")))
    } else {
        Ok(value)
    }
}

/// The saved key belongs to the host it was entered for. Pointing an endpoint
/// at another scheme, host or port without re-entering it would hand that key
/// (a provider key, or the LiteLLM master key) to whoever runs the new address
/// the next time the endpoint is tested or used.
fn require_key_for_new_origin(
    existing: &EndpointSecret,
    updated: &CreateEndpoint,
    key_supplied: bool,
) -> ApiResult<()> {
    let origin = |value: &str| {
        url::Url::parse(value).ok().map(|url| {
            (
                url.scheme().to_owned(),
                url.host_str().map(str::to_owned),
                url.port_or_known_default(),
            )
        })
    };
    let moved = origin(&existing.endpoint.base_url) != origin(&updated.base_url)
        || origin(&updated.base_url).is_none();
    if moved && !key_supplied && !existing.api_key.is_empty() {
        return Err(ApiError::bad_code(
            "api_key_required",
            "Enter the API key again to move this endpoint to another host, port or scheme. The saved key is only sent to the address it was entered for.",
        ));
    }
    Ok(())
}
