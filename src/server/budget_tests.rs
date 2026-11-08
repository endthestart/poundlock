use crate::domain::CreateApiToken;
use crate::server::ops_tests::send;
use crate::server::{AppState, router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub(crate) const ADMIN: &str = "admin-secret";

pub(crate) fn state() -> (tempfile::TempDir, AppState) {
    crate::server::ops_tests::test_state(|config| config.external_timeout_seconds = 10)
}

/// An upstream that answers every chat request with `answer` and keeps the
/// bodies it was sent.
pub(crate) async fn upstream(
    answer: impl Fn() -> axum::http::Response<Body> + Clone + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let recorded = recorded.clone();
            let answer = answer.clone();
            async move {
                recorded.lock().unwrap().push(body);
                answer()
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (format!("http://{address}/v1"), seen)
}

pub(crate) fn json_answer(body: Value) -> axum::http::Response<Body> {
    axum::http::Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(crate) fn priced_answer() -> axum::http::Response<Body> {
    json_answer(json!({
        "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 100_000}
    }))
}

/// One profile on `upstream` with the given destination and price, and an
/// application token that may use it. Returns the profile name and the token.
pub(crate) fn configure(
    state: &AppState,
    upstream: &str,
    destination: &str,
    price: Option<(f64, f64)>,
) -> (String, String) {
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "provider", "kind": "openai", "base_url": upstream, "api_key": "k"
            }))
            .unwrap(),
        )
        .unwrap();
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "frontier",
                "endpoint_id": endpoint.id,
                "model_id": "provider/model",
                "context_tokens": 200000,
                "max_output_tokens": 1000,
                "workload_class": "fast-code-completion",
                "wake_policy": "never",
                "data_destination": destination,
            }))
            .unwrap(),
        )
        .unwrap();
    if let Some((input, output)) = price {
        let patch: crate::domain::PatchModelProfile = serde_json::from_value(json!({
            "catalog": {"pricing": {
                "input_per_mtok": input, "output_per_mtok": output,
                "source": "operator", "refreshed_at": "2026-10-16T00:00:00Z"
            }}
        }))
        .unwrap();
        state
            .store
            .patch_model_profile(&profile.id, &patch)
            .unwrap();
    }
    let token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec![
                "models:read".into(),
                "inference:invoke".into(),
                "external:invoke".into(),
            ],
        })
        .unwrap();
    (profile.name, token.token)
}

pub(crate) async fn chat(
    app: axum::Router,
    token: &str,
    body: Value,
) -> (StatusCode, Value, axum::http::HeaderMap) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)}));
    (status, json, headers)
}

pub(crate) fn message(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

pub(crate) async fn spend(app: axum::Router) -> Value {
    let (status, view) = send(app, "GET", "/api/v1/frontier-budget", ADMIN, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    view
}

#[tokio::test]
async fn external_requests_are_metered_even_without_a_ceiling() {
    let (_directory, state) = state();
    let (address, _) = upstream(priced_answer).await;
    let (model, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state);
    let (status, _, headers) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("x-poundlock-frontier-budget-state").is_none());
    let view = spend(app.clone()).await;
    assert_eq!(view["status"]["configured"], false);
    assert_eq!(view["status"]["state"], "unconfigured");
    // 1M input at $1 plus 100K output at $10.
    assert_eq!(view["spend"]["total_usd"], 2.0);
    assert_eq!(view["spend"]["lines"][0]["requests"], 1);
    assert_eq!(view["spend"]["lines"][0]["profile_name"], "frontier");
    assert_eq!(view["spend"]["lines"][0]["api_token_name"], "client");
    let (status, usage) = send(app, "GET", "/api/v1/usage", &token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(usage["frontier_budget"]["configured"], false);
}

#[tokio::test]
async fn operator_network_requests_are_never_metered() {
    let (_directory, state) = state();
    let (address, _) = upstream(priced_answer).await;
    let (model, token) = configure(&state, &address, "operator_network", Some((1.0, 10.0)));
    state
        .store
        .save_frontier_budget(&crate::budget::FrontierBudget {
            weekly_usd: Some(0.01),
            ..Default::default()
        })
        .unwrap();
    let app = router(state);
    for _ in 0..2 {
        let (status, _, _) = chat(app.clone(), &token, message(&model)).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(spend(app).await["spend"]["total_usd"], 0.0);
}

#[tokio::test]
async fn a_used_up_ceiling_refuses_external_work_and_says_so_everywhere() {
    let (_directory, state) = state();
    let (address, seen) = upstream(priced_answer).await;
    let (model, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state);
    let (status, view) = send(
        app.clone(),
        "PUT",
        "/api/v1/frontier-budget",
        ADMIN,
        json!({"weekly_usd": 2.5, "monthly_usd": 100.0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["status"]["state"], "ok");

    // $2 of $2.50: allowed, and now at the 80% warning.
    let (status, _, headers) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-poundlock-frontier-budget-state"], "ok");
    let (_, models) = send(app.clone(), "GET", "/v1/models", &token, json!({})).await;
    let row = &models["data"][0];
    assert_eq!(row["availability"], "available");
    assert_eq!(row["warning_reason"], "frontier_budget_low");
    assert_eq!(row["poundlock"]["frontier_budget"]["state"], "low");
    assert_eq!(row["poundlock"]["frontier_budget"]["remaining_usd"], 0.5);

    // $4 of $2.50: a request admitted under the ceiling is allowed to finish.
    let (status, _, headers) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-poundlock-frontier-budget-state"], "low");

    let (status, refused, _) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "frontier_budget_exhausted");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("weekly frontier budget of $2.50")
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "a refused request never leaves"
    );

    let (_, models) = send(app.clone(), "GET", "/v1/models", &token, json!({})).await;
    assert_eq!(models["data"][0]["availability"], "blocked");
    assert_eq!(
        models["data"][0]["blocked_reason"],
        "frontier_budget_exhausted"
    );
    let (status, preflight) = send(
        app.clone(),
        "POST",
        "/api/v1/access/preflight",
        &token,
        json!({"model": model}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preflight["blocked_reason"], "frontier_budget_exhausted");
    assert_eq!(preflight["frontier_budget"]["state"], "exhausted");
    let (_, usage) = send(app.clone(), "GET", "/api/v1/usage", &token, json!({})).await;
    assert_eq!(usage["frontier_budget"]["state"], "exhausted");
    assert_eq!(usage["frontier_budget"]["periods"][0]["period"], "weekly");
    assert_eq!(usage["frontier_budget"]["periods"][0]["spent_usd"], 4.0);
    assert_eq!(usage["frontier_budget"]["periods"][1]["state"], "ok");

    // Raising the ceiling lets work continue.
    send(
        app.clone(),
        "PUT",
        "/api/v1/frontier-budget",
        ADMIN,
        json!({"weekly_usd": 10.0}),
    )
    .await;
    let (status, _, _) = chat(app, &token, message(&model)).await;
    assert_eq!(status, StatusCode::OK);
}
