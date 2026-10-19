//! Model discovery for operators: what the gateway already routes, what a
//! provider lists that nothing routes yet, and what Poundlock publishes.
//!
//! Three states, never blurred (ADR 0003):
//! - **catalog listing** — a provider's public catalog lists the model. That
//!   proves it exists and what it advertises, not that this account may use it
//!   or that a request will succeed.
//! - **configured route** — LiteLLM routes it with a credential. Poundlock
//!   reads this; it never writes LiteLLM routes or credentials (ADR 0001).
//! - **published profile** — a Poundlock profile offers it to applications.
//!
//! Every handler here is operator-only.

use super::{AdminAuth, ApiError, ApiResult, AppState, admin_auth};
use crate::config::CatalogMode;
use crate::domain::{
    CreateModelProfile, EndpointSecret, ModelProfile, PatchModelProfile, Pricing, REASONING_LEVELS,
    RouteCatalog,
};
use crate::store::now;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;

const OPENROUTER_CATALOG: &str = "https://openrouter.ai/api/v1/models";
const CATALOG_TTL: Duration = Duration::from_secs(3600);

/// A prompt that needs a little thought. A trivial one ("reply ok") uses no
/// reasoning at any level, so it cannot tell a level that took effect from one
/// that was dropped on the way (measured 2026-09-23 on GPT-6 Sol).
const REASONING_PROBE: &str = "A bat and a ball cost $1.10 in total. The bat costs $1.00 more than the \
ball. Then: a train leaves at 3:47pm and travels 214 km at 83 km/h. Give the ball price in cents \
and the arrival time, nothing else.";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/discovery", get(discover))
        .route("/api/v1/discovery/test", post(test_route))
        .route("/api/v1/discovery/publish", post(publish_route))
        .route("/api/v1/model-profiles/{id}/sync-route", post(sync_route))
        .route(
            "/api/v1/model-profiles/{id}/verify-reasoning",
            post(verify_reasoning),
        )
        .route("/api/v1/model-profiles/{id}/enable", post(enable_profile))
}

// ---------------------------------------------------------------- listing

#[derive(Deserialize)]
struct DiscoverQuery {
    /// `openrouter` asks for the provider catalog on this request.
    catalog: Option<String>,
}

async fn discover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DiscoverQuery>,
) -> ApiResult<Json<Value>> {
    admin_auth(&state, &headers)?;
    let asked = match query.catalog.as_deref() {
        None => false,
        Some("openrouter") => true,
        Some(_) => return Err(ApiError::bad("catalog must be openrouter")),
    };
    let mode = state.config.openrouter_catalog;
    let read_catalog = match mode {
        CatalogMode::Never => false,
        CatalogMode::OnRequest => asked,
        CatalogMode::Always => true,
    };
    let profiles = state.store.list_model_profiles()?;
    let mut gateways = Vec::new();
    let mut routed_upstreams = Vec::new();
    for endpoint in state
        .store
        .list_endpoints()?
        .into_iter()
        .filter(|e| e.kind == "litellm")
    {
        let secret = state
            .store
            .endpoint_secret(&endpoint.id)?
            .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
        match state.managed_identities.route_info(&secret).await {
            Ok(deployments) => {
                let routes: Vec<Value> = deployments
                    .iter()
                    .map(|deployment| {
                        let row = route_row(deployment);
                        if let Some(upstream) = row["upstream_model"].as_str() {
                            routed_upstreams.push(upstream.to_owned());
                        }
                        let published = profiles.iter().find(|profile| {
                            profile.endpoint_id == endpoint.id
                                && Some(profile.model_id.as_str()) == row["name"].as_str()
                        });
                        let mut row = row;
                        row["state"] = json!(if published.is_some() { "published" } else { "configured_route" });
                        row["profile"] = published.map_or(Value::Null, |profile| {
                            json!({"id": profile.id, "name": profile.name,
                                "served_model": profile.catalog.served_model,
                                "model_changed": profile.catalog.served_model.as_deref() != row["upstream_model"].as_str(),
                                "status":
                                if profile.retirement.disabled.is_some() { "disabled" } else { "enabled" }})
                        });
                        row
                    })
                    .collect();
                gateways.push(json!({"endpoint_id": endpoint.id, "endpoint_name": endpoint.name, "routes": routes}));
            }
            Err(error) => {
                tracing::warn!(endpoint = %endpoint.name, %error, "could not read gateway routes");
                gateways.push(json!({
                    "endpoint_id": endpoint.id,
                    "endpoint_name": endpoint.name,
                    "routes": [],
                    "error": "Could not read this gateway's routes. Check its administrative key and that it is reachable."
                }));
            }
        }
    }
    let catalog = if !read_catalog {
        // Nothing leaves the controller for a catalog nobody asked for.
        json!({"provider": "openrouter", "mode": mode.as_str(), "requested": false, "models": []})
    } else {
        match openrouter_catalog().await {
            Ok((fetched_at, models)) => json!({
                "provider": "openrouter",
                "mode": mode.as_str(),
                "requested": true,
                "fetched_at": fetched_at,
                "note": "Listed by OpenRouter. A listing is not proof this account may use the model, nor that a request will succeed.",
                "models": models
                    .into_iter()
                    .map(|mut model| {
                        let routed = model["id"].as_str().is_some_and(|id| {
                            routed_upstreams.iter().any(|upstream| upstream == &format!("openrouter/{id}"))
                        });
                        model["state"] = json!(if routed { "configured_route" } else { "catalog_listing" });
                        model
                    })
                    .collect::<Vec<_>>(),
            }),
            Err(error) => {
                tracing::warn!(%error, "could not read the OpenRouter catalog");
                json!({"provider": "openrouter", "mode": mode.as_str(), "requested": true, "models": [],
                "error": "The OpenRouter catalog could not be read."})
            }
        }
    };
    Ok(Json(json!({"gateways": gateways, "catalog": catalog})))
}

/// One LiteLLM deployment, as an operator needs to see it. Keys are masked by
/// LiteLLM and are not copied here at all.
pub(crate) fn route_row(deployment: &Value) -> Value {
    let params = &deployment["litellm_params"];
    let info = &deployment["model_info"];
    let upstream = params["model"].as_str().unwrap_or_default();
    json!({
        "name": deployment["model_name"],
        "upstream_model": upstream,
        "provider": provider_of(upstream, params.get("api_base").is_some_and(|base| !base.is_null())),
        "context_tokens": info["max_input_tokens"],
        "supports_reasoning": info["supports_reasoning"],
        "stored_in": if info["db_model"].as_bool() == Some(true) { "database" } else { "config_file" },
        "pricing": route_pricing(params, info),
    })
}

fn provider_of(upstream: &str, has_own_base: bool) -> &str {
    match upstream.split('/').next().unwrap_or_default() {
        "openai" if has_own_base => "openai-compatible",
        "" => "unknown",
        provider => provider,
    }
}

/// A route's price and where it comes from: the route's own declaration wins
/// over LiteLLM's bundled cost map, and neither is a bill.
pub(crate) fn route_pricing(params: &Value, info: &Value) -> Value {
    let per_mtok = |value: &Value| value.as_f64().map(|per_token| per_token * 1_000_000.0);
    let (source, input, output) = match (
        per_mtok(&params["input_cost_per_token"]),
        per_mtok(&params["output_cost_per_token"]),
    ) {
        (Some(input), Some(output)) => ("litellm_route", input, output),
        _ => match (
            per_mtok(&info["input_cost_per_token"]),
            per_mtok(&info["output_cost_per_token"]),
        ) {
            // The bundled map answers 0 for a model it does not know; that
            // is not a price.
            (Some(input), Some(output)) if input > 0.0 || output > 0.0 => {
                ("litellm_cost_map", input, output)
            }
            _ => return Value::Null,
        },
    };
    json!({"input_per_mtok": input, "output_per_mtok": output, "source": source})
}

// ------------------------------------------------------ provider catalog

type CatalogCache = Option<(Instant, String, Vec<Value>)>;
static OPENROUTER_CACHE: Mutex<CatalogCache> = Mutex::new(None);

/// OpenRouter's public model list, trimmed to what the operator compares,
/// cached for an hour. No credential is sent: this is the public catalog.
async fn openrouter_catalog() -> anyhow::Result<(String, Vec<Value>)> {
    if let Some((at, fetched_at, models)) = OPENROUTER_CACHE.lock().expect("catalog cache").as_ref()
        && at.elapsed() < CATALOG_TTL
    {
        return Ok((fetched_at.clone(), models.clone()));
    }
    let body: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?
        .get(OPENROUTER_CATALOG)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let models = catalog_models(&body);
    let fetched_at = now();
    *OPENROUTER_CACHE.lock().expect("catalog cache") =
        Some((Instant::now(), fetched_at.clone(), models.clone()));
    Ok((fetched_at, models))
}

#[cfg(test)]
pub(crate) fn seed_catalog(models: Vec<Value>) {
    *OPENROUTER_CACHE.lock().expect("catalog cache") = Some((Instant::now(), now(), models));
}

pub(crate) fn catalog_models(body: &Value) -> Vec<Value> {
    let per_mtok = |value: &Value| {
        value
            .as_str()
            .and_then(|text| text.parse::<f64>().ok())
            .map(|per_token| per_token * 1_000_000.0)
    };
    body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter(|model| model["id"].is_string())
                .map(|model| {
                    // `max` is dropped: through LiteLLM it arrives as `xhigh`.
                    let efforts: Vec<&str> = model["reasoning"]["supported_efforts"]
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .filter(|effort| REASONING_LEVELS.contains(effort))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "id": model["id"],
                        "name": model["name"],
                        "created": model["created"],
                        "context_tokens": model["context_length"],
                        "pricing": {
                            "input_per_mtok": per_mtok(&model["pricing"]["prompt"]),
                            "output_per_mtok": per_mtok(&model["pricing"]["completion"]),
                            "source": "openrouter_catalog",
                        },
                        "reasoning_efforts": efforts,
                        "default_effort": model["reasoning"]["default_effort"],
                        "expires": model["expiration_date"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// LiteLLM's reason, without the stack trace it appends or the provider
/// account id an OpenRouter error carries.
pub(crate) fn health_error(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    super::redact::redact_user_id(first)
        .chars()
        .take(300)
        .collect()
}

// ------------------------------------------------ test and publish a route

#[derive(Deserialize)]
struct RouteRef {
    endpoint_id: String,
    route: String,
}

async fn gateway(state: &AppState, endpoint_id: &str) -> ApiResult<EndpointSecret> {
    let secret = state
        .store
        .endpoint_secret(endpoint_id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    if secret.endpoint.kind != "litellm" {
        return Err(ApiError::bad("discovery reads LiteLLM gateways only"));
    }
    Ok(secret)
}

async fn find_route(state: &AppState, secret: &EndpointSecret, route: &str) -> ApiResult<Value> {
    let deployments = state
        .managed_identities
        .route_info(secret)
        .await
        .map_err(|_| ApiError::unavailable("could not read the gateway's routes"))?;
    deployments
        .into_iter()
        .find(|deployment| deployment["model_name"].as_str() == Some(route))
        .ok_or_else(|| ApiError::not_found("the gateway has no route by that name"))
}

async fn test_route(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(input): Json<RouteRef>,
) -> ApiResult<Json<Value>> {
    let secret = gateway(&state, &input.endpoint_id).await?;
    find_route(&state, &secret, &input.route).await?;
    let health = state
        .managed_identities
        .route_health(&secret, &input.route)
        .await
        .map_err(|_| ApiError::unavailable("the gateway did not complete its health check"))?;
    let healthy = health["healthy_count"].as_u64().unwrap_or(0) > 0
        || health["healthy_endpoints"]
            .as_array()
            .is_some_and(|items| !items.is_empty());
    // Only a yes/no and LiteLLM's own error text: the health payload can echo
    // deployment parameters, and nothing of that is repeated here.
    let error = health["unhealthy_endpoints"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["error"].as_str())
        .map(health_error);
    Ok(Json(
        json!({"route": input.route, "healthy": healthy, "error": error, "checked_at": now()}),
    ))
}

#[derive(Deserialize)]
struct PublishRoute {
    endpoint_id: String,
    route: String,
    #[serde(default)]
    name: Option<String>,
    data_destination: String,
    #[serde(default)]
    context_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    uses: Option<Vec<String>>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    model_label: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
}

async fn publish_route(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(input): Json<PublishRoute>,
) -> ApiResult<Json<ModelProfile>> {
    // Publishing is where an operator says where the data goes; it is never
    // inferred from the provider name.
    if !["operator_network", "external"].contains(&input.data_destination.as_str()) {
        return Err(ApiError::bad(
            "choose where this model processes data: operator_network or external",
        ));
    }
    let secret = gateway(&state, &input.endpoint_id).await?;
    let deployment = find_route(&state, &secret, &input.route).await?;
    let row = route_row(&deployment);
    let context_tokens = input.context_tokens.unwrap_or_else(|| {
        row["context_tokens"]
            .as_u64()
            .unwrap_or(131_072)
            .min(131_072)
    });
    let max_output_tokens = input
        .max_output_tokens
        .unwrap_or_else(|| crate::domain::default_max_output_tokens(context_tokens))
        .min(context_tokens);
    let profile = state.store.create_model_profile(&CreateModelProfile {
        name: input.name.clone().unwrap_or_else(|| input.route.clone()),
        aliases: input.aliases.clone(),
        endpoint_id: input.endpoint_id.clone(),
        model_id: input.route.clone(),
        model_label: input.model_label.clone(),
        context_tokens,
        max_output_tokens,
        uses: Some(input.uses.clone().unwrap_or_else(|| {
            crate::domain::DEFAULT_MODEL_USES
                .map(str::to_owned)
                .to_vec()
        })),
        workload_class: None,
        wake_policy: "never".into(),
        node_id: None,
        runtime_profile: None,
        idle_seconds: None,
        data_destination: input.data_destination.clone(),
        thinking: input
            .thinking
            .clone()
            .unwrap_or_else(|| "model_default".into()),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
    })?;
    let catalog = catalog_from_route(&row, &RouteCatalog::default());
    Ok(Json(set_catalog(&state, &profile.id, catalog)?))
}

/// The published catalog for a route, keeping verified reasoning levels only
/// while the model behind the route is the one they were verified on.
pub(crate) fn catalog_from_route(row: &Value, previous: &RouteCatalog) -> RouteCatalog {
    let served_model = row["upstream_model"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let same_model = served_model == previous.served_model;
    RouteCatalog {
        reasoning_levels: if same_model {
            previous.reasoning_levels.clone()
        } else {
            Vec::new()
        },
        reasoning_verified_at: if same_model {
            previous.reasoning_verified_at.clone()
        } else {
            None
        },
        provider: row["provider"].as_str().map(str::to_owned),
        pricing: row["pricing"].as_object().map(|pricing| Pricing {
            input_per_mtok: pricing["input_per_mtok"].as_f64().unwrap_or_default(),
            output_per_mtok: pricing["output_per_mtok"].as_f64().unwrap_or_default(),
            source: pricing["source"].as_str().unwrap_or("litellm_route").into(),
            refreshed_at: now(),
        }),
        served_model,
    }
}

fn set_catalog(state: &AppState, id: &str, catalog: RouteCatalog) -> ApiResult<ModelProfile> {
    let patch: PatchModelProfile = serde_json::from_value(json!({"catalog": catalog}))
        .map_err(|_| ApiError::bad("invalid catalog"))?;
    state
        .store
        .patch_model_profile(id, &patch)?
        .ok_or_else(|| ApiError::not_found("model profile not found"))
}

/// Refresh a published profile's served model and price from its gateway.
async fn sync_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelProfile>> {
    admin_auth(&state, &headers)?;
    let profile = state
        .store
        .model_profile(&id)?
        .ok_or_else(|| ApiError::not_found("model profile not found"))?;
    let secret = gateway(&state, &profile.endpoint_id).await?;
    let deployment = find_route(&state, &secret, &profile.model_id).await?;
    let row = route_row(&deployment);
    if row["upstream_model"]
        .as_str()
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(ApiError::unavailable(
            "the gateway did not identify the served model; nothing was refreshed",
        ));
    }
    let catalog = catalog_from_route(&row, &profile.catalog);
    Ok(Json(set_catalog(&state, &id, catalog)?))
}

// -------------------------------------------------- reasoning verification

#[derive(Deserialize, Default)]
struct VerifyReasoning {
    #[serde(default)]
    levels: Option<Vec<String>>,
}

/// Whether one probe answer shows a level taking effect. `none` must produce
/// no reasoning; any other level must produce some. A level whose upstream
/// does not report reasoning tokens cannot be told from a silently dropped
/// one, so it is not verified.
pub(crate) fn level_verified(
    level: &str,
    status: u16,
    reasoning_tokens: Option<u64>,
) -> (bool, &'static str) {
    if status != 200 {
        return (false, "refused by the upstream");
    }
    match (level, reasoning_tokens) {
        (_, None) => (
            false,
            "the upstream did not report reasoning tokens, so the effect cannot be seen",
        ),
        ("none", Some(0)) => (true, "no reasoning, as asked"),
        ("none", Some(_)) => (false, "reasoned anyway, so none is not honoured"),
        (_, Some(0)) => (
            false,
            "no reasoning at this level, so it may have been dropped on the way",
        ),
        (_, Some(_)) => (true, "reasoned at this level"),
    }
}

/// Characters of reasoning text in a llama.cpp chat answer, which carries it in
/// `message.reasoning_content` and leaves it out when the model did not think.
/// `None` when the body is not a chat answer, so an unreadable reply is never
/// mistaken for "no reasoning".
pub(crate) fn reasoning_text_chars(body: &Value) -> Option<u64> {
    let message = body["choices"][0]["message"].as_object()?;
    Some(
        message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .map_or(0, |text| text.chars().count() as u64),
    )
}

async fn verify_reasoning(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminAuth,
    body: Option<Json<VerifyReasoning>>,
) -> ApiResult<Json<Value>> {
    let input = body.map(|Json(input)| input).unwrap_or_default();
    let profile = state
        .store
        .model_profile(&id)?
        .ok_or_else(|| ApiError::not_found("model profile not found"))?;
    if profile.retirement.disabled.is_some() {
        return Err(ApiError::conflict(
            "re-enable this profile before verifying it",
        ));
    }
    let candidates: Vec<String> = input.levels.unwrap_or_else(|| {
        ["none", "low", "medium", "high", "xhigh"]
            .map(String::from)
            .to_vec()
    });
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|level| !REASONING_LEVELS.contains(&level.as_str()))
    {
        return Err(ApiError::bad(
            "levels must be some of none, minimal, low, medium, high, xhigh",
        ));
    }
    let mut endpoint = state
        .store
        .endpoint_secret(&profile.endpoint_id)?
        .ok_or_else(|| ApiError::not_found("endpoint not found"))?;
    // Through the same scoped path an application uses — never the gateway's
    // administrative key (ADR 0001).
    let principal = state.store.operator_check_principal()?;
    if let Some(key) = state
        .managed_identities
        .credential_for(&principal, &endpoint, &profile, "inference:invoke")
        .await
        .map_err(|_| ApiError::unavailable("the operator check identity could not be prepared"))?
    {
        endpoint.api_key = key;
    }
    let lease_id = uuid::Uuid::new_v4().to_string();
    let _slot = match super::capacity::Occupancy::exclusive_slot(&profile) {
        Some(slot) => Some(
            state
                .occupancy
                .try_claim_interactive(slot, lease_id.clone())
                .map_err(|_| {
                    ApiError::conflict("This model is busy. Try again when it is free.")
                })?,
        ),
        None => None,
    };
    let mut lease = super::runtime::RuntimeLease::new(state.clone(), profile.clone(), lease_id);
    let model_id = super::runtime::prepare_profile(&state, &mut lease)
        .await
        .map_err(|_| ApiError::unavailable("the configured inference runtime is unavailable"))?;
    let mut results = Vec::new();
    let mut verified = Vec::new();
    let llamacpp = crate::domain::serves_llamacpp(&profile);
    for level in &candidates {
        let mut request = json!({
            "model": model_id,
            "messages": [{"role": "user", "content": REASONING_PROBE}],
            "max_tokens": profile.max_output_tokens.min(1500),
            "reasoning_effort": level,
        });
        // The probe is shaped as the relay shapes a client's request, so it
        // shows whether the level works for clients, not whether the upstream
        // reads a field the relay would have moved.
        super::relay::apply_reasoning_settings(&mut request, &profile);
        let (status, reasoning_tokens, reasoning_chars) = match tokio::time::timeout(
            Duration::from_secs(state.config.external_timeout_seconds.max(30)),
            state
                .inference
                .relay(&endpoint, "chat/completions", request),
        )
        .await
        {
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let usage = &body["usage"];
                let tokens = usage["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .or_else(|| usage["reasoning_tokens"].as_u64());
                (
                    status,
                    tokens,
                    llamacpp.then(|| reasoning_text_chars(&body)).flatten(),
                )
            }
            _ => (0, None, None),
        };
        // What the upstream counts wins; llama.cpp builds that report nothing
        // are read from the reasoning text they return instead.
        let seen = reasoning_tokens.or(reasoning_chars);
        let (ok, note) = level_verified(level, status, seen);
        if ok {
            verified.push(level.clone());
        }
        results.push(json!({
            "level": level,
            "status": status,
            "reasoning_tokens": reasoning_tokens,
            // Characters of returned reasoning text, not tokens: a count from
            // the text when the upstream reports none. Only llama.cpp profiles.
            "reasoning_chars": reasoning_chars,
            "verified": ok,
            "note": note,
        }));
    }
    // Keep the vocabulary order, whatever order the levels were probed in.
    verified.sort_by_key(|level| REASONING_LEVELS.iter().position(|known| known == level));
    let mut catalog = profile.catalog.clone();
    catalog.reasoning_levels = verified.clone();
    catalog.reasoning_verified_at = (!verified.is_empty()).then(now);
    set_catalog(&state, &id, catalog)?;
    Ok(Json(
        json!({"profile": profile.name, "verified": verified, "results": results}),
    ))
}

async fn enable_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<ModelProfile>> {
    admin_auth(&state, &headers)?;
    if !state.store.enable_profile(&id)? {
        return Err(ApiError::not_found("model profile not found"));
    }
    Ok(Json(state.store.model_profile(&id)?.ok_or_else(|| {
        ApiError::not_found("model profile not found")
    })?))
}
