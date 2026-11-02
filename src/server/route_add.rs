//! Adding an OpenRouter catalog model as a LiteLLM route and publishing it, in
//! one step the operator confirms (ADR 0001, amended 27 Oct 2026). The route
//! carries the catalog's prices as its own cost per token, so LiteLLM counts
//! its spend, and uses a provider credential LiteLLM already holds by name.
//! Nothing here edits or deletes a route or writes a credential.

use super::discovery::{
    PublishRoute, check_destination, gateway, health_error, openrouter_catalog, publish,
};
use super::{AdminAuth, ApiError, ApiResult, AppState};
use crate::config::CatalogMode;
use crate::domain::{EndpointSecret, ModelProfile};
use anyhow::{Context, bail};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[cfg(test)]
#[path = "route_add_tests.rs"]
mod tests;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/discovery/credentials", get(credentials))
        .route("/api/v1/discovery/add-route", post(add_route))
}

#[derive(Deserialize)]
struct GatewayQuery {
    endpoint_id: String,
}

/// The gateway's named provider credentials: names and providers, never values.
async fn credentials(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Query(query): Query<GatewayQuery>,
) -> ApiResult<Json<Value>> {
    let secret = managed_gateway(&state, &query.endpoint_id).await?;
    let names = credential_names(&secret).await?;
    Ok(Json(json!({"credentials": names})))
}

#[derive(Deserialize)]
struct AddRoute {
    endpoint_id: String,
    /// The OpenRouter catalog id, such as `anthropic/claude-sonnet-5.5`.
    catalog_id: String,
    /// The gateway model name applications ask for.
    route: String,
    credential: String,
    context_tokens: u64,
    max_output_tokens: u64,
    data_destination: String,
}

async fn add_route(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(input): Json<AddRoute>,
) -> ApiResult<Json<ModelProfile>> {
    // Everything is checked before LiteLLM is written to.
    check_destination(&input.data_destination)?;
    let route = input.route.trim();
    if route.is_empty() || route.chars().any(char::is_whitespace) {
        return Err(ApiError::bad("name the route without spaces"));
    }
    let secret = managed_gateway(&state, &input.endpoint_id).await?;
    let model = catalog_model(&state, &input.catalog_id).await?;
    let (input_price, output_price) = match (
        model["pricing"]["input_per_mtok"].as_f64(),
        model["pricing"]["output_per_mtok"].as_f64(),
    ) {
        (Some(input), Some(output)) => (input, output),
        _ => {
            return Err(ApiError::bad(
                "OpenRouter lists no price for this model, so LiteLLM could not count its spend",
            ));
        }
    };
    check_limits(&model, input.context_tokens, input.max_output_tokens)?;
    let deployments = state
        .managed_identities
        .route_info(&secret)
        .await
        .map_err(|_| ApiError::unavailable("could not read the gateway's routes"))?;
    if deployments
        .iter()
        .any(|deployment| deployment["model_name"].as_str() == Some(route))
    {
        return Err(ApiError::conflict(format!(
            "the gateway already has a route named {route}; choose another name, or edit that route in LiteLLM"
        )));
    }
    if !credential_names(&secret)
        .await?
        .iter()
        .any(|credential| credential["name"].as_str() == Some(input.credential.as_str()))
    {
        return Err(ApiError::bad(format!(
            "the gateway has no credential named {}",
            input.credential
        )));
    }
    let body = new_route(&input, route, &model, input_price, output_price);
    litellm_post(&secret, "model/new", body)
        .await
        .map_err(|error| {
            ApiError::unavailable(format!("LiteLLM did not add the route: {error:#}"))
        })?;
    let published = publish(
        &state,
        &PublishRoute {
            endpoint_id: input.endpoint_id.clone(),
            route: route.to_owned(),
            data_destination: input.data_destination.clone(),
            context_tokens: Some(input.context_tokens),
            max_output_tokens: Some(input.max_output_tokens),
            ..PublishRoute::default()
        },
    )
    .await;
    published.map(Json).map_err(|error| ApiError {
        message: format!(
            "The route {route} was added in LiteLLM, but publishing it failed: {}. Publish it from Routes on your gateways.",
            error.message
        ),
        ..error
    })
}

/// A route may only be added where Poundlock already holds the gateway's
/// administrative key for key management (ADR 0001).
async fn managed_gateway(state: &AppState, endpoint_id: &str) -> ApiResult<EndpointSecret> {
    let secret = gateway(state, endpoint_id).await?;
    if !secret.endpoint.managed_identity.enabled {
        return Err(ApiError::bad(
            "adding a route needs the gateway's administrative key: choose Separate application identities and budgets for it on Connections",
        ));
    }
    Ok(secret)
}

async fn catalog_model(state: &AppState, catalog_id: &str) -> ApiResult<Value> {
    if state.config.openrouter_catalog == CatalogMode::Never {
        return Err(ApiError::bad(
            "the OpenRouter catalog is turned off on this controller",
        ));
    }
    let (_, models) = openrouter_catalog()
        .await
        .map_err(|_| ApiError::unavailable("The OpenRouter catalog could not be read."))?;
    models
        .into_iter()
        .find(|model| model["id"].as_str() == Some(catalog_id))
        .ok_or_else(|| ApiError::not_found("OpenRouter's catalog does not list that model"))
}

/// The confirmed limits, within what the catalog says the model takes.
fn check_limits(model: &Value, context: u64, output: u64) -> ApiResult<()> {
    let context_limit = model["context_tokens"].as_u64().unwrap_or(u64::MAX);
    if context == 0 || context > context_limit {
        return Err(ApiError::bad(format!(
            "the context must be between 1 and {context_limit} tokens"
        )));
    }
    let output_limit = model["max_output_tokens"]
        .as_u64()
        .unwrap_or(u64::MAX)
        .min(context);
    if output == 0 || output > output_limit {
        return Err(ApiError::bad(format!(
            "the maximum output must be between 1 and {output_limit} tokens"
        )));
    }
    Ok(())
}

fn new_route(
    input: &AddRoute,
    route: &str,
    model: &Value,
    input_price: f64,
    output_price: f64,
) -> Value {
    let per_token = |per_mtok: f64| per_mtok / 1_000_000.0;
    let reasoning = model["reasoning_efforts"]
        .as_array()
        .is_some_and(|efforts| !efforts.is_empty());
    json!({
        "model_name": route,
        "litellm_params": {
            "model": format!("openrouter/{}", input.catalog_id),
            "litellm_credential_name": input.credential,
            "input_cost_per_token": per_token(input_price),
            "output_cost_per_token": per_token(output_price),
        },
        "model_info": {
            "mode": "chat",
            "max_input_tokens": input.context_tokens,
            "max_output_tokens": input.max_output_tokens,
            "supports_reasoning": reasoning,
        },
    })
}

async fn credential_names(secret: &EndpointSecret) -> ApiResult<Vec<Value>> {
    let listed = litellm_get(secret, "credentials")
        .await
        .map_err(|_| ApiError::unavailable("could not read the gateway's credentials"))?;
    Ok(listed["credentials"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item["credential_name"].as_str()?;
                    Some(json!({"name": name, "provider": item["credential_info"]["custom_llm_provider"]}))
                })
                .collect()
        })
        .unwrap_or_default())
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn litellm_get(secret: &EndpointSecret, path: &str) -> anyhow::Result<Value> {
    let url = crate::litellm::management_url(&secret.endpoint.base_url, path)?;
    let response = client()?
        .get(url)
        .bearer_auth(&secret.api_key)
        .send()
        .await
        .with_context(|| format!("connect to LiteLLM {path}"))?;
    answer(response).await
}

async fn litellm_post(secret: &EndpointSecret, path: &str, body: Value) -> anyhow::Result<Value> {
    let url = crate::litellm::management_url(&secret.endpoint.base_url, path)?;
    let response = client()?
        .post(url)
        .bearer_auth(&secret.api_key)
        .header("litellm-changed-by", "poundlock")
        .json(&body)
        .send()
        .await
        .with_context(|| format!("connect to LiteLLM {path}"))?;
    answer(response).await
}

/// LiteLLM's own reason on a refusal, first line only and without the
/// provider account id an OpenRouter error carries.
async fn answer(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let text = response.text().await.context("read LiteLLM's answer")?;
    if !status.is_success() {
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let reason = body["error"]["message"]
            .as_str()
            .or_else(|| body["detail"]["error"].as_str())
            .or_else(|| body["detail"].as_str())
            .map(health_error)
            .unwrap_or_default();
        bail!(
            "HTTP {status}{}",
            if reason.is_empty() {
                String::new()
            } else {
                format!(": {reason}")
            }
        )
    }
    serde_json::from_str(&text).context("decode LiteLLM's answer")
}
