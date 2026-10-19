use super::{AdminAuth, ApiError, ApiResult, AppState, admin_auth};
use crate::domain::{API_TOKEN_PERMISSIONS, ApiToken, CreateApiToken, IssuedApiToken};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/api-tokens", get(list_tokens).post(create_token))
        .route("/api/v1/api-tokens/{id}/revoke", post(revoke_token))
        .route("/api/v1/capabilities", get(capabilities))
        .route("/api/v1/access/preflight", post(preflight))
        .route("/api/v1/usage", get(usage))
}

async fn capabilities(
    State(_state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _principal = super::api_token_auth(&_state, &headers, "models:read")?;
    Ok(Json(crate::contract::capabilities_document(env!(
        "CARGO_PKG_VERSION"
    ))))
}

#[derive(Deserialize)]
struct PreflightRequest {
    model: String,
    #[serde(default)]
    operation: Option<String>,
}

async fn list_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ApiToken>>> {
    admin_auth(&state, &headers)?;
    Ok(Json(state.store.list_api_tokens()?))
}

async fn create_token(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(mut input): Json<CreateApiToken>,
) -> ApiResult<Json<IssuedApiToken>> {
    input.name = input.name.trim().to_owned();
    if input.name.is_empty() || input.name.len() > 80 {
        return Err(ApiError::bad("token name must contain 1 to 80 characters"));
    }
    let allowed = API_TOKEN_PERMISSIONS.into_iter().collect::<HashSet<_>>();
    input.permissions.sort();
    input.permissions.dedup();
    if input.permissions.is_empty()
        || input
            .permissions
            .iter()
            .any(|permission| !allowed.contains(permission.as_str()))
    {
        return Err(ApiError::bad(format!(
            "permissions must contain only: {}",
            API_TOKEN_PERMISSIONS.join(", ")
        )));
    }
    Ok(Json(state.store.create_api_token(&input)?))
}

async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    if !state.store.revoke_api_token(&id)? {
        return Err(ApiError::not_found("active API token not found"));
    }
    let pending = state
        .store
        .pending_managed_litellm_revocations()?
        .into_iter()
        .filter(|mapping| mapping.api_token_id == id)
        .count();
    let reconciliation = state.managed_identities.clone();
    tokio::spawn(async move {
        if let Err(error) = reconciliation.reconcile_pending().await {
            tracing::warn!(%error, "managed LiteLLM revocation remains pending");
        }
    });
    Ok(Json(json!({
        "revoked": true,
        "managed_identities_pending": pending,
    })))
}

async fn usage(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let principal = super::api_token_auth(&state, &headers, "models:read")?;
    let mut usage = state.managed_identities.usage_for(&principal).await?;
    usage["schema_version"] = json!(1);
    Ok(Json(usage))
}

async fn preflight(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreflightRequest>,
) -> ApiResult<Json<Value>> {
    let principal = super::api_token_auth(&state, &headers, "models:read")?;
    let model = input.model.trim();
    if model.is_empty() {
        return Err(ApiError::bad("model is required"));
    }
    let profile = state
        .store
        .model_profile(model)?
        .or(state.store.model_profile_by_name(model)?)
        .ok_or_else(|| ApiError::not_found("model profile not found"))?;
    let endpoint = state
        .store
        .endpoint_secret(&profile.endpoint_id)?
        .ok_or_else(|| ApiError::unavailable("configured endpoint is unavailable"))?;
    let (operation, required_permission) = match input.operation.as_deref().unwrap_or("inference") {
        "inference" => ("inference", "inference:invoke"),
        "job" => ("job", "jobs:submit"),
        _ => return Err(ApiError::bad("operation must be inference or job")),
    };

    let mut access = match state
        .managed_identities
        .prepare_access(&principal, &profile, &endpoint, required_permission)
        .await
    {
        Ok(access) => access,
        Err(error) => {
            tracing::warn!(
                api_token_id = %principal.id,
                endpoint_id = %endpoint.endpoint.id,
                %error,
                "managed access preflight failed"
            );
            let mut access =
                state
                    .managed_identities
                    .access_for(&principal, &profile, &endpoint.endpoint)?;
            access["availability"] = json!("blocked");
            access["availability_reason"] = json!("managed_identity_unavailable");
            access["blocked_reason"] = json!("managed_identity_unavailable");
            access
        }
    };
    // Credentials and policy take precedence. When those pass, reuse the
    // admission checks so a client can tell an offline or paused machine and
    // an occupied slot apart from a credential problem before it starts work.
    // Only a real block changes `availability`: contract-3 clients stop on any
    // value outside their ready set. Conditions the relay handles by itself
    // (waking a node, an occupied slot that answers 409 and is retried) are
    // reported additively so existing clients keep working. Durable jobs
    // queue, so slot occupancy is not reported for them.
    if access["availability"] == "available" && operation == "inference" {
        match super::runtime::runtime_availability(&state, &profile)? {
            Some(("blocked", reason)) => {
                access["availability"] = json!("blocked");
                access["availability_reason"] = json!(reason);
                access["blocked_reason"] = json!(reason);
            }
            Some((runtime_state, reason)) => {
                access["runtime_state"] = json!(runtime_state);
                access["runtime_reason"] = json!(reason);
            }
            None => {}
        }
        if access["availability"] == "available"
            && super::relay::client_occupancy(&profile, &state.occupancy).0
        {
            access["capacity"] = json!("in_use");
            access["capacity_reason"] = json!("capacity_in_use");
        }
    }
    if operation == "job" {
        let runner = state.runners.status();
        access["runner_status"] = json!(runner);
        if runner != "connected" && access["warning_reason"].is_null() {
            access["warning_reason"] = json!("runner_not_connected");
        }
    }
    tracing::info!(
        api_token_id = %principal.id,
        profile_id = %profile.id,
        operation,
        availability = %access["availability"],
        reason = %access["availability_reason"],
        "access preflight evaluated"
    );
    access["profile_id"] = json!(profile.id);
    access["profile_name"] = json!(profile.name);
    access["data_destination"] = json!(profile.data_destination);
    access["endpoint_kind"] = json!(endpoint.endpoint.kind);
    access["endpoint_id"] = json!(endpoint.endpoint.id);
    access["operation"] = json!(operation);
    access["schema_version"] = json!(1);

    let usage = state.managed_identities.usage_for(&principal).await?;
    access["usage"] = usage["managed_identities"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["endpoint_id"] == endpoint.endpoint.id)
        })
        .cloned()
        .unwrap_or(Value::Null);
    Ok(Json(access))
}
