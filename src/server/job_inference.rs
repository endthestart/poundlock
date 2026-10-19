use super::{ApiError, ApiResult, AppState, bearer};
use crate::domain::RunnerPackage;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

#[cfg(test)]
#[path = "job_inference_tests.rs"]
mod tests;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/job/v1/{id}/models", get(models))
        .route("/job/v1/{id}/chat/completions", post(chat))
}

fn authorize(state: &AppState, id: &str, headers: &HeaderMap) -> ApiResult<String> {
    let token = bearer(headers).unwrap_or_default();
    if !state.store.job_inference_authorized(id, token)? {
        return Err(ApiError::unauthorized(
            "active job inference token required",
        ));
    }
    Ok(token.into())
}

fn package(state: &AppState, id: &str) -> ApiResult<RunnerPackage> {
    state
        .store
        .runner_package(id)?
        .ok_or_else(|| ApiError::not_found("job contract unavailable"))
}

async fn models(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorize(&state, &id, &headers)?;
    let model = package(&state, &id)?.model;
    Ok(Json(
        json!({"object":"list","data":[{"id":model.model_id,"object":"model"}]}),
    ))
}

fn validate_request(body: &mut Value, package: &RunnerPackage) -> ApiResult<()> {
    let fields = body
        .as_object_mut()
        .ok_or_else(|| ApiError::bad("expected an object"))?;
    // Do not pass gateway-specific routing, credential or destination
    // overrides: the same list the interactive relay uses, plus nothing.
    if let Some(unsupported) = super::relay::unsupported_option(fields, &[], &package.model) {
        return Err(ApiError::bad(format!(
            "unsupported job inference option: {unsupported}"
        )));
    }
    if fields.get("model").and_then(Value::as_str) != Some(&package.model.model_id) {
        return Err(ApiError::bad("model must match the accepted job"));
    }
    if fields.get("reasoning_effort").is_some_and(|value| {
        !matches!(
            value.as_str(),
            Some("none" | "minimal" | "low" | "medium" | "high")
        )
    }) {
        return Err(ApiError::bad("unsupported reasoning_effort"));
    }
    if let Some(message) = super::reasoning::budget_problem(fields, &package.model) {
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
            .filter(|n| *n > 0 && *n <= package.model.max_output_tokens)
            .ok_or_else(|| ApiError::bad("output token limit exceeds the accepted model budget"))?,
        None => package.model.max_output_tokens,
    };
    fields.insert(key.into(), json!(limit));
    Ok(())
}

async fn revoked(state: &AppState, id: &str, token: &str) {
    loop {
        if !state
            .store
            .runner_package(id)
            .ok()
            .flatten()
            .is_some_and(|p| super::runtime::managed_job_runtime_valid(state, &p).unwrap_or(false))
        {
            return;
        }
        if !state
            .store
            .job_inference_authorized(id, token)
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn chat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult<Response<Body>> {
    let token = authorize(&state, &id, &headers)?;
    let package = package(&state, &id)?;
    validate_request(&mut body, &package)?;
    if !super::runtime::managed_job_runtime_valid(&state, &package)? {
        return Err(ApiError::unavailable(
            "The managed model lease ended; submit a new job",
        ));
    }
    body["model"] = Value::String(super::runtime::managed_job_model_id(&state, &package)?);
    // One rule for every client: the profile's thinking default is applied by the
    // server on both paths, and an explicit client value always wins.
    super::relay::apply_reasoning_settings(&mut body, &package.model);
    let guard = state
        .occupancy
        .try_job_request(&id)
        .ok_or_else(|| ApiError::conflict("a request for this job is already in progress"))?;
    // Correlate job diagnostics by job, attempt and profile; never log prompts.
    let attempt_id = package.job.attempt_id.clone();
    let profile_id = package.model.id.clone();
    // The relay client has a connect timeout and nothing else, and lease
    // revocation was the only other way out of this await. So a request the
    // model never answered held the job until its wall budget expired — an
    // hour spent, an empty patch, and no error anywhere a consumer could read.
    // Bounded here, and again between chunks below, with a coded 5xx the
    // harness's own retry treats as transient.
    let silence = Duration::from_secs(state.config.job_inference_timeout_seconds.max(1));
    let upstream = tokio::select! {
        response = tokio::time::timeout(silence, state.inference.relay(&package.endpoint, "chat/completions", body)) => match response {
            Ok(response) => response.map_err(|error| {
                tracing::warn!(job_id=%id, attempt_id=%attempt_id, profile_id=%profile_id, %error, "job inference connection failed");
                ApiError::unavailable("could not connect to the configured inference endpoint")
            })?,
            Err(_) => {
                tracing::warn!(job_id=%id, attempt_id=%attempt_id, profile_id=%profile_id, timeout_seconds=silence.as_secs(), "job inference upstream sent no response in time");
                return Err(ApiError::gateway_timeout(
                    "upstream_timeout",
                    format!("the model sent no response within {} seconds", silence.as_secs()),
                ));
            }
        },
        _ = revoked(&state, &id, &token) => return Err(ApiError::unauthorized("job inference lease ended")),
    };
    if !upstream.status().is_success() {
        let status = upstream.status();
        tracing::warn!(
            job_id = %id,
            attempt_id = %attempt_id,
            profile_id = %profile_id,
            upstream_status = status.as_u16(),
            "job inference upstream returned an error status"
        );
        // The job sees the upstream's message without gateway internals (L3).
        let body = tokio::time::timeout(silence, super::redact::read_capped(upstream, 64 * 1024))
            .await
            .unwrap_or_default();
        tracing::debug!(job_id = %id, upstream_body = %String::from_utf8_lossy(&body), "upstream error body");
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(super::redact::client_error_body(
                status.as_u16(),
                &body,
            )))
            .map_err(|_| ApiError::unavailable("could not construct the job inference response"));
    }
    let mut response = Response::builder().status(upstream.status());
    if let Some(content_type) = upstream.headers().get("content-type") {
        response = response.header("content-type", content_type)
    }
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut bytes = upstream.bytes_stream();
        let revocation = revoked(&state, &id, &token);
        tokio::pin!(revocation);
        loop {
            tokio::select! {
                _ = &mut revocation => {
                    tracing::warn!(job_id=%id, attempt_id=%attempt_id, profile_id=%profile_id, "job inference stream stopped because its lease ended");
                    yield Err(std::io::Error::other("job inference lease ended"));
                    break;
                }
                chunk = tokio::time::timeout(silence, bytes.next()) => match chunk {
                    Ok(Some(Ok(chunk))) => yield Ok(chunk),
                    Ok(Some(Err(error))) => {
                        tracing::warn!(job_id=%id, attempt_id=%attempt_id, profile_id=%profile_id, %error, "job inference stream read failed");
                        yield Err(std::io::Error::other(error));
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!(job_id=%id, attempt_id=%attempt_id, profile_id=%profile_id, idle_timeout_seconds=silence.as_secs(), "job inference stream idle timeout");
                        yield Err(std::io::Error::other("the model stopped responding mid-stream"));
                        break;
                    }
                }
            }
        }
    };
    response
        .body(Body::from_stream(stream))
        .map_err(|_| ApiError::unavailable("could not construct inference response"))
}
