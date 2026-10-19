use super::capacity::{
    InteractiveGuard, MAX_WAIT, Occupancy, Occupant, QueuePlace, SlotKey, WaitClass, WaitError,
    WaitRequest,
};
use super::runtime::{RuntimeLease, prepare_profile};
use super::{ApiError, ApiResult, AppState, api_token_auth};
use crate::domain::ModelProfile;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::io::Error as IoError;
use std::time::Duration;

pub fn routes(state: AppState) -> Router<AppState> {
    // Inference checks the token before its body is read or parsed (L4).
    let inference = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/completions", post(completions))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            authorize_invoke,
        ));
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/capacity", get(capacity))
        .merge(inference)
}

async fn authorize_invoke(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match api_token_auth(&state, request.headers(), "inference:invoke") {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

/// Each exclusive model slot that is held or has a line: who holds it, and who
/// waits, by class and for how long. No prompts, tokens or sessions.
async fn capacity(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    api_token_auth(&state, &headers, "models:read")?;
    let profiles = state.store.list_model_profiles()?;
    let slots: Vec<Value> = state
        .occupancy
        .queue_snapshot()
        .into_iter()
        .map(|view| {
            let names: Vec<&str> = profiles
                .iter()
                .filter(|profile| {
                    profile.node_id.as_deref() == Some(view.node_id.as_str())
                        && profile.runtime_profile.as_deref() == Some(view.runtime_profile.as_str())
                })
                .map(|profile| profile.name.as_str())
                .collect();
            json!({
                "slot": format!("{}/{}", view.node_id, view.runtime_profile),
                "models": names,
                "holder_kind": view.holder_kind,
                "waiting": view.waiting,
            })
        })
        .collect();
    Ok(Json(json!({
        "object": "capacity",
        "max_wait_seconds": MAX_WAIT.as_secs(),
        "default_wait_seconds": state.config.capacity_default_wait_seconds,
        "held_stream_seconds": HELD_STREAM_WAIT.as_secs(),
        "slots": slots,
    })))
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let principal = api_token_auth(&state, &headers, "models:read")?;
    let kinds = state.store.endpoint_kinds()?;
    let occupancy = &state.occupancy;
    let mut data = Vec::new();
    for profile in state.store.list_model_profiles()? {
        let kind = kinds
            .get(&profile.endpoint_id)
            .map(String::as_str)
            .unwrap_or("openai");
        let revision = state.store.routing_revision(&profile)?;
        let endpoint = state
            .store
            .endpoint_secret(&profile.endpoint_id)?
            .ok_or_else(|| ApiError::unavailable("configured endpoint is unavailable"))?;
        let mut access =
            state
                .managed_identities
                .access_for(&principal, &profile, &endpoint.endpoint)?;
        let (busy, _) = client_occupancy(&profile, occupancy);
        if busy && access["availability"] == "available" {
            access["availability"] = json!("busy");
            access["availability_reason"] = json!("capacity_in_use");
        }
        let mut rows = advertised_models(profile, kind, &revision, occupancy);
        for row in &mut rows {
            apply_access_metadata(row, &access);
        }
        data.extend(rows);
    }
    Ok(Json(json!({
        "object":"list",
        "poundlock":crate::contract::models_poundlock_metadata(),
        "data":data
    })))
}

pub(super) fn apply_access_metadata(model: &mut Value, access: &Value) {
    for field in [
        "availability",
        "availability_reason",
        "blocked_reason",
        "warning_reason",
        "connection_mode",
        "managed_identity_status",
    ] {
        model[field] = access.get(field).cloned().unwrap_or(Value::Null);
        model["poundlock"][field] = access.get(field).cloned().unwrap_or(Value::Null);
    }
}

pub(super) fn advertised_models(
    profile: ModelProfile,
    endpoint_kind: &str,
    routing_revision: &str,
    occupancy: &Occupancy,
) -> Vec<Value> {
    vec![advertised_model(
        &profile,
        &profile.name,
        endpoint_kind,
        routing_revision,
        occupancy,
    )]
}

pub(super) fn advertised_model(
    profile: &ModelProfile,
    id: &str,
    endpoint_kind: &str,
    routing_revision: &str,
    occupancy: &Occupancy,
) -> Value {
    let (busy, occupancy_json) = client_occupancy(profile, occupancy);
    json!({
        "id": id,
        "object": "model",
        "owned_by": "poundlock",
        "context_length": profile.context_tokens,
        "context_window": profile.context_tokens,
        "context_tokens": profile.context_tokens,
        "max_output_tokens": profile.max_output_tokens,
        "busy": busy,
        "occupancy": occupancy_json.clone(),
        "meta": {"n_ctx": profile.context_tokens},
        "poundlock": {
            // Every row is a profile: `id` is the profile's own name, and the
            // weights answering it are `upstream_model`. Saying so lets a
            // client pin the stable name deliberately and still record which
            // model replied, instead of inferring it by comparing the two.
            "kind": "profile",
            "profile_id": profile.id,
            "canonical_name": profile.name,
            "aliases": profile.aliases,
            "upstream_model": profile.model_id,
            "model_label": profile.model_label,
            "endpoint_kind": endpoint_kind,
            "routing_revision": routing_revision,
            "data_destination": profile.data_destination,
            "thinking": profile.thinking,
            "uses": profile.uses,
            "workload_class": profile.workload_class,
            "wake_policy": profile.wake_policy,
            "context_tokens": profile.context_tokens,
            "max_output_tokens": profile.max_output_tokens,
            "n_ctx": profile.context_tokens,
            // Measured evidence, augmented at read time with age_days, stale
            // and settings_match. Empty means unknown, never capable.
            "capabilities": crate::domain::describe_capabilities(profile),
            // Explicit values a client may send, each shown by a live request
            // to take effect. Empty: offer only the model's own default, which
            // means sending nothing. `default` is this profile's own setting
            // when the client sends nothing (`off` injects "none").
            "reasoning": reasoning_metadata(profile),
            // What answers behind the stable name, and an estimated price with
            // its source. Neither is used for routing.
            "served_model": profile.catalog.served_model,
            "provider": profile.catalog.provider,
            "pricing": profile.catalog.pricing,
            "status": if profile.retirement.disabled.is_some() { "disabled" } else { "enabled" },
            "disabled": profile.retirement.disabled.as_ref().map(|state| json!({
                "reason": state.reason,
                "source": state.source,
                "since": state.since,
                "last_check": state.evidence.last().map(|failure| failure.at.clone()),
            })),
            "busy": busy,
            "occupancy": occupancy_json
        }
    })
}

/// `poundlock.reasoning` for one profile. A llama.cpp profile also says it
/// takes `thinking_budget_tokens`, up to what bound, and what a request that
/// turns thinking on gets when it names none; other profiles omit the key.
fn reasoning_metadata(profile: &ModelProfile) -> Value {
    let mut reasoning = json!({
        "supported": profile.catalog.reasoning_levels,
        "default": profile.thinking,
        "verified_at": profile.catalog.reasoning_verified_at,
    });
    if crate::domain::serves_llamacpp(profile) {
        reasoning["thinking_budget_tokens"] = json!({
            "max": profile.max_output_tokens,
            "default": profile.thinking_budget_tokens,
        });
    }
    reasoning
}

pub(super) fn client_occupancy(profile: &ModelProfile, occupancy: &Occupancy) -> (bool, Value) {
    let slot = Occupancy::exclusive_slot(profile);
    let kind =
        slot.as_ref()
            .and_then(|slot| occupancy.occupant(slot))
            .map(|occupant| match occupant {
                Occupant::Interactive { .. } => "interactive",
                Occupant::Job { .. } => "job",
            });
    (
        kind.is_some(),
        json!({
            "kind": kind,
            // The exclusive slot this profile competes for, or null when it has
            // none. `node_id` and `runtime_profile` are filled in either way,
            // so a client grouping by them could not tell enforced contention
            // from two profiles that merely share a machine. Profiles with the
            // same `slot` run one at a time, and switching between them loads
            // a different model.
            "slot": slot.as_ref().map(|slot| format!("{}/{}", slot.node_id, slot.runtime_profile)),
            "node_id": slot.as_ref().map(|slot| slot.node_id.as_str())
                .or(profile.node_id.as_deref()),
            "runtime_profile": slot.as_ref().map(|slot| slot.runtime_profile.as_str())
                .or(profile.runtime_profile.as_deref())
        }),
    )
}

async fn chat(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<crate::domain::ApiTokenPrincipal>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let wait = wait_request(&headers, &principal, &state);
    let held = held_open(&headers, &body);
    relay(state, principal, "chat/completions", body, wait, held).await
}

async fn completions(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<crate::domain::ApiTokenPrincipal>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let wait = wait_request(&headers, &principal, &state);
    let held = held_open(&headers, &body);
    relay(state, principal, "completions", body, wait, held).await
}

/// The permission that lets a token ask for `owner` or `planner` priority.
pub(super) const PRIORITY_PERMISSION: &str = "inference:priority";

/// How long a streamed request from a client that sends no queue headers waits
/// in line on its open stream before it is told the model stayed busy.
pub(super) const HELD_STREAM_WAIT: Duration = Duration::from_secs(600);
/// How often a stream held in line sends an SSE comment, well inside the hub
/// proxy's idle timeout.
const HELD_KEEPALIVE: Duration = Duration::from_secs(15);

/// A streamed request from a client that does not speak the queue protocol
/// (no `Poundlock-Wait`, no ticket), such as a chat app: it waits on its open
/// stream rather than hearing "busy" (Open WebUI, for one,
/// cannot send a ticket, so its chats just failed).
fn held_open(headers: &HeaderMap, body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool) == Some(true)
        && !headers.contains_key("poundlock-wait")
        && !headers.contains_key("poundlock-queue-ticket")
}

/// How a request asks to wait when its model is busy: `Poundlock-Wait`
/// (seconds, at most `MAX_WAIT`), `Poundlock-Priority`, `Poundlock-Session`
/// and `Poundlock-Queue-Ticket`. Without them it waits the operator's
/// default, as worker-class work, or as the owner's for a token the operator
/// let go ahead: such a token is an application a person waits on.
fn wait_request(
    headers: &axum::http::HeaderMap,
    principal: &crate::domain::ApiTokenPrincipal,
    state: &AppState,
) -> WaitRequest {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    // Owner by default only when the header is absent: a value that does not
    // parse (a typo) is ordinary work, never the highest class.
    let asked = match text("poundlock-priority") {
        None if principal.has_permission(PRIORITY_PERMISSION) => WaitClass::Owner,
        None => WaitClass::Worker,
        Some(value) => WaitClass::parse(value).unwrap_or(WaitClass::Worker),
    };
    // Going ahead of other work is the operator's grant, not the client's say.
    let class = if asked < WaitClass::Worker && !principal.has_permission(PRIORITY_PERMISSION) {
        WaitClass::Worker
    } else {
        asked
    };
    let wait = text("poundlock-wait")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(
            state.config.capacity_default_wait_seconds,
        ))
        .min(MAX_WAIT);
    WaitRequest {
        class,
        session: text("poundlock-session").map(|value| value.chars().take(128).collect()),
        token_id: principal.id.clone(),
        ticket: text("poundlock-queue-ticket").map(|value| value.chars().take(64).collect()),
        wait,
    }
}

pub(super) const REQUEST_ID_HEADER: &str = "x-poundlock-request-id";

async fn relay(
    state: AppState,
    principal: crate::domain::ApiTokenPrincipal,
    path: &'static str,
    body: Value,
    wait: WaitRequest,
    held: bool,
) -> axum::response::Response {
    // One identifier correlates the client response, controller diagnostics
    // and gateway attribution. Prompts and credentials are never logged.
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut response = relay_inner(state, principal, path, body, &request_id, wait, held).await;
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

async fn relay_inner(
    state: AppState,
    principal: crate::domain::ApiTokenPrincipal,
    path: &'static str,
    mut body: Value,
    request_id: &str,
    wait: WaitRequest,
    held: bool,
) -> axum::response::Response {
    let alias = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let profile = match state.store.model_profile(alias).and_then(|found| {
        if found.is_some() {
            Ok(found)
        } else {
            state.store.model_profile_by_name(alias)
        }
    }) {
        Ok(Some(profile)) => profile,
        Ok(None) => return super::model_names::unknown_model(&state, &principal, alias),
        Err(error) => return ApiError::from(error).into_response(),
    };
    if let Some(disabled) = &profile.retirement.disabled {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "model_disabled",
            &format!(
                "this model was taken out of service: {}. An operator can retest or re-enable it",
                disabled.reason
            ),
        );
    }
    if let Some(code) = crate::litellm::destination_block_reason(&principal, &profile) {
        let message = if code == "data_destination_confirmation_required" {
            "the operator must confirm where this profile processes data before it can be used"
        } else {
            "this application token is not authorized to send data to external models"
        };
        return openai_error(StatusCode::FORBIDDEN, code, message);
    }
    if let Err(error) = constrain_interactive_request(&mut body, &profile) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            error.code().unwrap_or("invalid_request"),
            &error.to_string(),
        );
    }
    let lease_id = uuid::Uuid::new_v4().to_string();
    let occupancy_guard = match Occupancy::exclusive_slot(&profile) {
        // A held stream looks once, then waits in line on its open stream.
        Some(slot) => match state
            .occupancy
            .claim_interactive(
                slot.clone(),
                lease_id.clone(),
                &if held {
                    WaitRequest {
                        wait: Duration::ZERO,
                        ..wait.clone()
                    }
                } else {
                    wait.clone()
                },
            )
            .await
        {
            Ok(guard) => Some(guard),
            Err(WaitError::Busy(place)) if held => {
                tracing::info!(
                    request_id = %request_id,
                    profile_id = %profile.id,
                    profile = %profile.name,
                    position = place.position,
                    "exclusive coding runtime is in use; holding the stream in line"
                );
                return held_in_line(HeldRequest {
                    state,
                    principal,
                    path,
                    body,
                    request_id: request_id.to_owned(),
                    wait,
                    profile,
                    slot,
                    lease_id,
                    position: place.position,
                });
            }
            Err(WaitError::Busy(place)) => {
                tracing::info!(
                    request_id = %request_id,
                    profile_id = %profile.id,
                    profile = %profile.name,
                    position = place.position,
                    waited_seconds = wait.wait.as_secs(),
                    "exclusive coding runtime is still in use"
                );
                return capacity_busy(place);
            }
            Err(WaitError::Full { per_token }) => {
                return openai_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "capacity_queue_full",
                    if per_token {
                        "This application already has the most requests waiting for this model."
                    } else {
                        "Too many requests are waiting for this model."
                    },
                );
            }
        },
        None => None,
    };
    relay_claimed(
        state,
        principal,
        path,
        body,
        request_id,
        profile,
        occupancy_guard,
        lease_id,
    )
    .await
}

/// A request waiting in line on a stream it already has open.
struct HeldRequest {
    state: AppState,
    principal: crate::domain::ApiTokenPrincipal,
    path: &'static str,
    body: Value,
    request_id: String,
    wait: WaitRequest,
    profile: ModelProfile,
    slot: SlotKey,
    lease_id: String,
    position: usize,
}

/// Answer a held request at once with an event stream that waits its turn: an
/// SSE comment every `HELD_KEEPALIVE` (clients ignore comments) keeps the
/// connection open for up to `HELD_STREAM_WAIT`. Its turn relays as usual. The
/// status is already sent, so an error answer becomes one `data:` event with
/// the usual `{"error":…}` body. A client that leaves leaves the line.
fn held_in_line(held: HeldRequest) -> axum::response::Response {
    let HeldRequest {
        state,
        principal,
        path,
        body,
        request_id,
        wait,
        profile,
        slot,
        lease_id,
        mut position,
    } = held;
    let stream = async_stream::stream! {
        let deadline = tokio::time::Instant::now() + HELD_STREAM_WAIT;
        let mut ticket: Option<String> = None;
        let guard = loop {
            yield Ok::<_, IoError>(axum::body::Bytes::from(format!(
                ": waiting for {}, position {position}\n\n",
                profile.name
            )));
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                yield Ok(error_event(
                    "capacity_in_use",
                    "This model stayed busy for 10 minutes. Try again later.",
                ));
                return;
            }
            let attempt = WaitRequest {
                ticket: ticket.clone(),
                wait: HELD_KEEPALIVE.min(left),
                ..wait.clone()
            };
            match state
                .occupancy
                .claim_interactive(slot.clone(), lease_id.clone(), &attempt)
                .await
            {
                Ok(guard) => break guard,
                Err(WaitError::Busy(place)) => {
                    position = place.position;
                    ticket = place.ticket;
                }
                Err(WaitError::Full { .. }) => {
                    yield Ok(error_event(
                        "capacity_queue_full",
                        "Too many requests are waiting for this model.",
                    ));
                    return;
                }
            }
        };
        // A cold model can take minutes to load: keep the connection alive until
        // the relay answers, or a proxy's idle timeout drops the chat.
        let relayed = relay_claimed(
            state,
            principal,
            path,
            body,
            &request_id,
            profile,
            Some(guard),
            lease_id,
        );
        tokio::pin!(relayed);
        let response = loop {
            let ready = tokio::select! {
                response = &mut relayed => Some(response),
                _ = tokio::time::sleep(HELD_KEEPALIVE) => None,
            };
            match ready {
                Some(response) => break response,
                None => yield Ok(axum::body::Bytes::from_static(b": preparing the model\n\n")),
            }
        };
        if response.status() == StatusCode::OK {
            let mut chunks = response.into_body().into_data_stream();
            while let Some(chunk) = chunks.next().await {
                yield chunk.map_err(IoError::other);
            }
        } else {
            let error = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap_or_default();
            let error = String::from_utf8_lossy(&error).replace(['\r', '\n'], " ");
            yield Ok(axum::body::Bytes::from(format!("data: {error}\n\n")));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_response_failed",
                "could not construct relay response",
            )
        })
}

/// One SSE `data:` event carrying an OpenAI-style error.
fn error_event(code: &str, message: &str) -> axum::body::Bytes {
    let error = json!({"error":{"message":message,"type":"poundlock_error","code":code}});
    axum::body::Bytes::from(format!("data: {error}\n\n"))
}

/// The rest of a relay once the request holds its slot (or needs none).
#[allow(clippy::too_many_arguments)]
async fn relay_claimed(
    state: AppState,
    principal: crate::domain::ApiTokenPrincipal,
    path: &'static str,
    mut body: Value,
    request_id: &str,
    profile: ModelProfile,
    occupancy_guard: Option<InteractiveGuard>,
    lease_id: String,
) -> axum::response::Response {
    let mut relay_lease = RuntimeLease::new(state.clone(), profile.clone(), lease_id.clone());
    let runtime_model_id = match prepare_profile(&state, &mut relay_lease).await {
        Ok(model_id) => model_id,
        Err(error) => {
            // Reuse the preflight runtime classification so clients can tell a
            // paused or offline machine from credential or capacity problems.
            let reason = super::runtime::runtime_availability(&state, &profile)
                .ok()
                .flatten()
                .map(|(_, reason)| reason);
            tracing::warn!(
                request_id = %request_id,
                profile_id = %profile.id,
                profile = %profile.name,
                reason = reason.unwrap_or("infrastructure_unavailable"),
                %error,
                "could not prepare inference runtime"
            );
            // A node that answered and failed its ensure is retry-safe, which
            // "infrastructure unavailable" did not say; durable jobs already
            // report the same condition as `ensure_failed`.
            let (code, message) = match (
                reason,
                error.downcast_ref::<super::runtime::EnsureFailed>(),
            ) {
                (None, Some(_)) => (
                    "ensure_failed",
                    "the machine could not prepare this model; a retry starts a fresh preparation",
                ),
                _ => runtime_unavailable_error(reason),
            };
            return openai_error(StatusCode::SERVICE_UNAVAILABLE, code, message);
        }
    };
    let mut endpoint = match state.store.endpoint_secret(&profile.endpoint_id) {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "endpoint_missing",
                "configured endpoint is unavailable",
            );
        }
        Err(error) => {
            tracing::error!(
                request_id = %request_id,
                profile_id = %profile.id,
                %error,
                "could not load inference endpoint"
            );
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "endpoint_missing",
                "configured endpoint is unavailable",
            );
        }
    };
    if path == "chat/completions" {
        apply_reasoning_settings(&mut body, &profile);
    }
    let routing_revision = match state.store.routing_revision(&profile) {
        Ok(revision) => revision,
        Err(error) => {
            tracing::error!(
                request_id = %request_id,
                profile_id = %profile.id,
                %error,
                "could not compute routing revision"
            );
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "routing_policy_unavailable",
                "the configured routing policy is unavailable",
            );
        }
    };
    if endpoint.endpoint.kind == "litellm" {
        let access =
            match state
                .managed_identities
                .access_for(&principal, &profile, &endpoint.endpoint)
            {
                Ok(access) => access,
                Err(error) => return ApiError::from(error).into_response(),
            };
        if access["availability"] == "blocked"
            && access["blocked_reason"] != "managed_identity_provision_failed"
        {
            let code = access["blocked_reason"]
                .as_str()
                .unwrap_or("managed_identity_unavailable");
            let message = match code {
                "managed_budget_required" => {
                    "external access is disabled until the operator configures a budget and reset period"
                }
                "managed_identity_reconciliation_required" => {
                    "the managed gateway identity requires operator reconciliation"
                }
                _ => "the managed gateway identity is unavailable",
            };
            return openai_error(StatusCode::SERVICE_UNAVAILABLE, code, message);
        }
        inject_trusted_attribution(
            &mut body,
            &principal.id,
            request_id,
            &profile.id,
            &routing_revision,
        );
        match state
            .managed_identities
            .credential_for(&principal, &endpoint, &profile, "inference:invoke")
            .await
        {
            Ok(Some(virtual_key)) => endpoint.api_key = virtual_key,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    request_id = %request_id,
                    api_token_id = %principal.id,
                    profile_id = %profile.id,
                    endpoint_id = %endpoint.endpoint.id,
                    %error,
                    "managed LiteLLM identity is unavailable"
                );
                return openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "managed_identity_unavailable",
                    "the managed gateway identity is unavailable; operator reconciliation is required",
                );
            }
        }
    }
    tracing::info!(
        api_token_id = %principal.id,
        request_id = %request_id,
        profile_id = %profile.id,
        routing_revision = %routing_revision,
        "relaying application inference request"
    );
    body["model"] = Value::String(runtime_model_id);
    // Wake/prepare uses its own external timeout. Inference time starts here.
    let budget = inference_timeout(path, state.config.external_timeout_seconds);
    let deadline = tokio::time::Instant::now() + budget;
    let upstream =
        match tokio::time::timeout_at(deadline, state.inference.relay(&endpoint, path, body)).await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(
                    request_id = %request_id,
                    profile_id = %profile.id,
                    profile = %profile.name,
                    routing_revision = %routing_revision,
                    %error,
                    "upstream inference connection failed"
                );
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_connection_failed",
                    super::runtime::unreachable_server_message(&state, &profile),
                );
            }
            Err(_) => {
                tracing::warn!(
                    request_id = %request_id,
                    profile_id = %profile.id,
                    profile = %profile.name,
                    routing_revision = %routing_revision,
                    header_timeout_seconds = budget.as_secs(),
                    "upstream inference did not send headers in time"
                );
                return openai_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "the model did not respond in time",
                );
            }
        };
    let status = upstream.status();
    if !status.is_success() {
        tracing::warn!(
            request_id = %request_id,
            profile_id = %profile.id,
            profile = %profile.name,
            routing_revision = %routing_revision,
            upstream_status = status.as_u16(),
            "upstream inference returned an error status"
        );
        // An error answer is small and never streamed, so read it whole: it is
        // the only place the upstream says a model no longer exists, and that
        // is the evidence retirement counts.
        let body = tokio::time::timeout(budget, super::redact::read_capped(upstream, 64 * 1024))
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        if let Some(detail) = crate::store::retirement::model_gone(status.as_u16(), &text) {
            let outcome = crate::store::retirement::RouteOutcome::Gone {
                status: status.as_u16(),
                detail,
            };
            match state
                .store
                .record_route_outcome(&profile.id, "relay", &outcome)
            {
                Ok(true) => {
                    tracing::warn!(profile = %profile.name, "profile disabled: upstream reports its model gone")
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(profile = %profile.name, %error, "could not record route outcome")
                }
            }
        }
        // Classification above sees the upstream text unchanged; the client
        // never does. It gets the upstream's message without gateway internals
        // or the operator's OpenRouter account id (L3).
        tracing::debug!(request_id = %request_id, upstream_body = %text, "upstream error body");
        let body = super::redact::client_error_body(status.as_u16(), &body);
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| {
                openai_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "relay_response_failed",
                    "could not construct relay response",
                )
            });
    }
    if let Err(error) = state.store.record_route_outcome(
        &profile.id,
        "relay",
        &crate::store::retirement::RouteOutcome::Served,
    ) {
        tracing::warn!(profile = %profile.name, %error, "could not record route outcome");
    }
    let content_type = upstream.headers().get("content-type").cloned();
    // The gateway's own estimate for this call, when it gives one. Only a
    // plain decimal is forwarded, so no other upstream header can ride along.
    let response_cost = upstream
        .headers()
        .get("x-litellm-response-cost")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            value.len() <= 32
                && value
                    .parse::<f64>()
                    .is_ok_and(|cost| cost.is_finite() && cost >= 0.0)
        })
        .map(str::to_owned);
    let served_model = profile.catalog.served_model.clone();
    let stream_request_id = request_id.to_owned();
    let stream_profile_id = profile.id.clone();
    let stream_profile_name = profile.name.clone();
    let stream_routing_revision = routing_revision.clone();
    let stream_state = state.clone();
    let stream_token_id = principal.id.clone();
    let pump_request_id = request_id.to_owned();
    // The slot and lease outlive this function in a handle both ends of the
    // pump can release: the client side on disconnect, the task on a stall.
    let release = Release::new((relay_lease, occupancy_guard));
    let stream_release = release.clone();
    let stream = async_stream::stream! {
        let mut bytes = upstream.bytes_stream();
        tracing::info!(
            request_id = %stream_request_id,
            profile_id = %stream_profile_id,
            profile = %stream_profile_name,
            routing_revision = %stream_routing_revision,
            "upstream inference stream started"
        );
        let revocation = token_revoked(&stream_state, &stream_token_id);
        tokio::pin!(revocation);
        loop {
            let next = tokio::select! {
                _ = &mut revocation => {
                    tracing::warn!(
                        request_id = %stream_request_id,
                        profile_id = %stream_profile_id,
                        api_token_id = %stream_token_id,
                        "inference stream stopped because its application token was revoked"
                    );
                    yield Err(IoError::other("the application token was revoked"));
                    break;
                }
                next = tokio::time::timeout(budget, bytes.next()) => next,
            };
            match next {
                Ok(Some(Ok(chunk))) => yield Ok(chunk),
                Ok(Some(Err(error))) => {
                    tracing::warn!(
                        request_id = %stream_request_id,
                        profile_id = %stream_profile_id,
                        profile = %stream_profile_name,
                        routing_revision = %stream_routing_revision,
                        error = %error,
                        "upstream inference stream read failed"
                    );
                    yield Err(IoError::other(error));
                    break;
                }
                Ok(None) => {
                    tracing::info!(
                        request_id = %stream_request_id,
                        profile_id = %stream_profile_id,
                        profile = %stream_profile_name,
                        routing_revision = %stream_routing_revision,
                        "upstream inference stream completed"
                    );
                    break;
                }
                Err(_) => {
                    tracing::warn!(
                        request_id = %stream_request_id,
                        profile_id = %stream_profile_id,
                        profile = %stream_profile_name,
                        routing_revision = %stream_routing_revision,
                        idle_timeout_seconds = budget.as_secs(),
                        "upstream inference stream idle timeout"
                    );
                    yield Err(IoError::new(
                        std::io::ErrorKind::TimedOut,
                        "inference response exceeded the idle timeout",
                    ));
                    break;
                }
            }
        }
        stream_release.now();
    };
    let stream = pumped(stream, CLIENT_STALL, pump_request_id, release);
    let mut response = Response::builder().status(status);
    if let Some(content_type) = content_type {
        response = response.header("content-type", content_type);
    }
    // Headers, not body fields, so a streamed answer is untouched. The body's
    // `model` is the gateway's route name; this is what answered behind it.
    if let Some(served) =
        served_model.filter(|value| axum::http::HeaderValue::from_str(value).is_ok())
    {
        response = response.header("x-poundlock-served-model", served);
    }
    if let Some(cost) = response_cost {
        response = response.header("x-poundlock-response-cost", cost);
    }
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "relay_response_failed",
                "could not construct relay response",
            )
        })
}

/// How often a relay stream re-reads its application token.
const TOKEN_RECHECK: Duration = Duration::from_secs(2);

/// Resolves once `token_id` is revoked, or can no longer be read, so a stream
/// already in flight does not outlive the incident response.
async fn token_revoked(state: &AppState, token_id: &str) {
    loop {
        tokio::time::sleep(TOKEN_RECHECK).await;
        if !matches!(state.store.api_token_principal(token_id), Ok(Some(_))) {
            return;
        }
    }
}

fn runtime_unavailable_error(reason: Option<&str>) -> (&'static str, &'static str) {
    match reason {
        Some("runtime_node_missing") => (
            "runtime_node_missing",
            "the machine configured for this model no longer exists; the operator must update the profile",
        ),
        Some("runtime_node_unavailable") => (
            "runtime_node_unavailable",
            "the machine for this model is paused for maintenance or has been removed",
        ),
        Some("runtime_node_network_unavailable") => (
            "runtime_node_network_unavailable",
            "the machine for this model reports that its selected network interface is unavailable",
        ),
        Some("runtime_node_offline") => (
            "runtime_node_offline",
            "the machine for this model is asleep or offline, and this model is set not to wake it",
        ),
        Some("runtime_wake_unavailable") => (
            "runtime_wake_unavailable",
            "the machine for this model is asleep or offline, and it has no Wake-on-LAN settings to wake it",
        ),
        Some("runtime_wake_required") => (
            "runtime_wake_required",
            "the machine for this model did not become ready before the preparation deadline",
        ),
        _ => (
            "infrastructure_unavailable",
            "the configured inference runtime is unavailable",
        ),
    }
}

/// The model stayed busy for the whole wait: where the request stands, and the
/// ticket that keeps its place when it is sent again.
fn capacity_busy(place: QueuePlace) -> axum::response::Response {
    // A request holding a place should come back at once; one that did not
    // wait has no place to lose.
    let retry_after = if place.ticket.is_some() { "1" } else { "15" };
    let mut response = (
        StatusCode::CONFLICT,
        Json(json!({
            "error": {
                "message": "This model is busy. Send the request again with its queue ticket to keep its place.",
                "type": "poundlock_error",
                "code": "capacity_in_use",
            },
            "queue": place,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static(retry_after),
    );
    response
}

fn openai_error(status: StatusCode, code: &str, message: &str) -> axum::response::Response {
    let mut response = (
        status,
        Json(json!({"error":{"message":message,"type":"poundlock_error","code":code}})),
    )
        .into_response();
    if code == "capacity_in_use" {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("15"),
        );
    }
    response
}

pub(super) const COMPLETIONS_RELAY_TIMEOUT_SECONDS: u64 = 30;

fn inference_timeout(path: &str, external_timeout_seconds: u64) -> Duration {
    Duration::from_secs(if path == "completions" {
        COMPLETIONS_RELAY_TIMEOUT_SECONDS
    } else {
        external_timeout_seconds.max(1)
    })
}

/// Gateway routing and credential overrides, dropped rather than refused.
///
/// Clients set some of these harmlessly out of habit, and they have always
/// been stripped here, so refusing them now would break a working session for
/// no gain. Anything *not* on this list and not on `RELAY_ALLOWED_KEYS` is a
/// key this controller does not understand, and is refused.
const DESTINATION_OVERRIDE_KEYS: &[&str] = &[
    "api_key",
    "api_base",
    "base_url",
    "extra_headers",
    "extra_body",
    "target",
    "custom_llm_provider",
];

/// Options both inference paths accept: this interactive relay and the job
/// proxy (`job_inference.rs`), which uses this list as its own. One list, so
/// the two cannot drift — they had, when each kept a copy.
///
/// The destination check above was a seven-key denylist on its own.
/// A denylist is the wrong shape for a boundary that decides where a user's
/// conversation goes: it is correct only for as long as nobody upstream adds
/// an eighth key, and LiteLLM adds options routinely.
pub(super) const SHARED_INFERENCE_KEYS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "seed",
    "tools",
    "reasoning_effort",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "logprobs",
    "top_logprobs",
    "n",
    "user",
    "store",
];

/// Sampling and template controls a local runtime (llama.cpp, Ollama, vLLM)
/// understands and a hosted API generally rejects, accepted only when the
/// profile's data stays on the operator network.
///
/// They were refused everywhere, which also made the relay's own
/// `chat_template_kwargs.enable_thinking` honouring (see
/// `inject_default_reasoning_off`) unreachable: validation runs first, so any
/// request carrying it was a 400 before that check could see it. None of these
/// can route a request or carry a credential.
pub(super) const LOCAL_RUNTIME_KEYS: &[&str] =
    &["chat_template_kwargs", "top_k", "min_p", "repeat_penalty"];

/// What only the interactive relay additionally accepts.
const RELAY_ONLY_KEYS: &[&str] = &[
    // Both handled explicitly elsewhere in this file: `reasoning` is the
    // other thinking control this relay recognises, and caller `metadata` is
    // merged with Poundlock's own attribution keys.
    "reasoning",
    "metadata",
    // Sent by the Pi SDK's OpenAI-completions adapter (verified against the
    // pinned 1.1.0 bundle). Gateway cache hints, not routing or credentials.
    "prompt_cache_key",
    "prompt_cache_options",
    "prompt_cache_retention",
    "previous_response_id",
    "safety_identifier",
    "service_tier",
    "prediction",
    "modalities",
    "verbosity",
    "logit_bias",
    // Completion requests carry a prompt and a suffix.
    "prompt",
    "suffix",
    "echo",
    "best_of",
];

/// The first key in `fields` that this path does not accept, if any.
pub(super) fn unsupported_option<'a>(
    fields: &'a serde_json::Map<String, Value>,
    path_keys: &[&str],
    profile: &ModelProfile,
) -> Option<&'a str> {
    let local = profile.data_destination == "operator_network";
    fields.keys().map(String::as_str).find(|key| {
        !SHARED_INFERENCE_KEYS.contains(key)
            && !path_keys.contains(key)
            && !(local && LOCAL_RUNTIME_KEYS.contains(key))
            && !super::reasoning::is_llamacpp_key(key, profile)
    })
}

pub(super) fn constrain_interactive_request(
    body: &mut Value,
    profile: &ModelProfile,
) -> ApiResult<()> {
    let fields = body
        .as_object_mut()
        .ok_or_else(|| ApiError::bad("expected an object"))?;
    for key in DESTINATION_OVERRIDE_KEYS {
        fields.remove(*key);
    }
    if let Some(unsupported) = unsupported_option(fields, RELAY_ONLY_KEYS, profile) {
        return Err(ApiError::bad(format!(
            "unsupported inference option: {unsupported}"
        )));
    }
    if let Some(message) = unsupported_reasoning(fields, profile) {
        return Err(ApiError::bad_code("unsupported_reasoning_effort", message));
    }
    if let Some(message) = super::reasoning::budget_problem(fields, profile) {
        return Err(ApiError::bad_code("invalid_thinking_budget", message));
    }
    if fields.get("n").is_some_and(|n| n.as_u64() != Some(1)) {
        return Err(ApiError::bad("one completion per request is supported"));
    }
    if fields.contains_key("max_tokens") && fields.contains_key("max_completion_tokens") {
        return Err(ApiError::bad("provide only one output token limit"));
    }
    let key = if fields.contains_key("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    let limit = match fields.get(key) {
        Some(value) => value
            .as_u64()
            .filter(|n| *n > 0 && *n <= profile.max_output_tokens)
            .ok_or_else(|| {
                ApiError::bad("output token limit exceeds the advertised model budget")
            })?,
        None => profile.max_output_tokens,
    };
    fields.insert(key.into(), json!(limit));
    Ok(())
}

/// Why an explicit reasoning choice cannot be honoured as sent, if it cannot.
///
/// A value is refused rather than passed on when it is outside the vocabulary
/// this relay forwards unchanged (`REASONING_LEVELS` — `max` is not in it,
/// because LiteLLM's OpenRouter adapter rewrites it to `xhigh`), or when the
/// profile has verified levels and this is not one of them. Nothing is ever
/// rewritten: a choice either reaches the model as the client made it, or the
/// client is told which choices would.
///
/// A profile with no verified levels passes a valid explicit value through,
/// as before levels were recorded, so existing clients that already send one
/// keep working. Clients learn which levels are safe to offer from
/// `poundlock.reasoning.supported`.
pub(super) fn unsupported_reasoning(
    fields: &serde_json::Map<String, Value>,
    profile: &ModelProfile,
) -> Option<String> {
    let chosen = fields.get("reasoning_effort").or_else(|| {
        fields
            .get("reasoning")
            .and_then(|value| value.get("effort"))
    })?;
    let Some(level) = chosen.as_str() else {
        return Some("reasoning_effort must be a string".into());
    };
    let verified = &profile.catalog.reasoning_levels;
    if !crate::domain::REASONING_LEVELS.contains(&level) {
        return Some(format!(
            "reasoning_effort \"{level}\" is not supported{}",
            supported_hint(verified)
        ));
    }
    if !verified.is_empty() && !verified.iter().any(|value| value == level) {
        return Some(format!(
            "reasoning_effort \"{level}\" is not verified for {}{}",
            profile.name,
            supported_hint(verified)
        ));
    }
    None
}

fn supported_hint(verified: &[String]) -> String {
    if verified.is_empty() {
        format!(
            "; use one of {}",
            crate::domain::REASONING_LEVELS.join(", ")
        )
    } else {
        format!("; supported: {}", verified.join(", "))
    }
}

/// Put a chat request's reasoning settings in the form its upstream honours.
///
/// A llama.cpp profile gets the template kwargs that switch thinking (see
/// `reasoning`); every other profile whose `thinking` default is `off` gets the
/// `reasoning_effort: "none"` default, exactly as before. Shared by the
/// interactive relay, the job proxy and the reasoning probe, so all three send
/// the same request for the same choice. It edits the body in place and costs
/// no round trip.
pub(super) fn apply_reasoning_settings(body: &mut Value, profile: &ModelProfile) {
    if crate::domain::serves_llamacpp(profile) {
        super::reasoning::apply_llamacpp(body, profile);
    } else if profile.thinking == "off" {
        inject_default_reasoning_off(body);
    }
}

/// For a profile whose `thinking` default is `off`, ask the upstream model to
/// skip reasoning unless the client already expressed a preference. Never
/// overrides an explicit `reasoning_effort`, `reasoning` or
/// `chat_template_kwargs.enable_thinking` sent by the client.
pub(super) fn inject_default_reasoning_off(body: &mut Value) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let explicit_choice = fields.contains_key("reasoning_effort")
        || fields.contains_key("reasoning")
        || fields
            .get("chat_template_kwargs")
            .and_then(Value::as_object)
            .is_some_and(|kwargs| kwargs.contains_key("enable_thinking"));
    if !explicit_choice {
        fields.insert("reasoning_effort".into(), json!("none"));
    }
}

fn inject_trusted_attribution(
    body: &mut Value,
    api_token_id: &str,
    request_id: &str,
    profile_id: &str,
    routing_revision: &str,
) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    fields.insert(
        "user".into(),
        json!(format!("poundlock:application-token:{api_token_id}")),
    );
    if !fields.get("metadata").is_some_and(Value::is_object) {
        fields.insert("metadata".into(), json!({}));
    }
    let metadata = fields
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .expect("metadata was initialized as an object");
    metadata.retain(|key, _| !key.starts_with("poundlock_"));
    metadata.insert("poundlock_application_token_id".into(), json!(api_token_id));
    metadata.insert("poundlock_request_id".into(), json!(request_id));
    metadata.insert("poundlock_profile_id".into(), json!(profile_id));
    metadata.insert("poundlock_routing_revision".into(), json!(routing_revision));
}

/// How long a client may leave a streamed answer unread before the relay gives
/// up on it and frees the model.
const CLIENT_STALL: Duration = Duration::from_secs(120);

/// Feed a client from a task that owns `source`, and with it the slot and lease
/// guards the stream holds. A client that disconnects, or leaves a chunk unread
/// for `stall`, ends the task and drops them at once (L5); without it they
/// lasted until the connection finally closed.
fn pumped<S>(
    source: S,
    stall: Duration,
    request_id: String,
    release: Release,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, IoError>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, IoError>> + Send + 'static,
{
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let task_release = release.clone();
    tokio::spawn(async move {
        let mut source = std::pin::pin!(source);
        loop {
            // A client that leaves while the upstream is silent must free the
            // model now, not at the next chunk.
            let item = tokio::select! {
                _ = sender.closed() => break,
                item = source.next() => item,
            };
            let Some(item) = item else { break };
            let last = item.is_err();
            match tokio::time::timeout(stall, sender.send(item)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::warn!(
                        request_id = %request_id,
                        stall_seconds = stall.as_secs(),
                        "client stopped reading the inference stream; ending it and freeing the model"
                    );
                    break;
                }
            }
            if last {
                break;
            }
        }
        task_release.now();
    });
    async_stream::stream! {
        let _released = ReleasedOnDrop(release);
        while let Some(item) = receiver.recv().await {
            yield item;
        }
    }
}

/// What a streamed answer holds (its slot and lease), released by whichever
/// end of the pump finishes first, and only once.
#[derive(Clone)]
struct Release(std::sync::Arc<std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>>>);

impl Release {
    fn new(held: impl std::any::Any + Send) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(Box::new(
            held,
        )))))
    }

    fn now(&self) {
        let held = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(held);
    }
}

/// Releases the moment the client's side of the stream is dropped.
struct ReleasedOnDrop(Release);

impl Drop for ReleasedOnDrop {
    fn drop(&mut self) {
        self.0.now();
    }
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
