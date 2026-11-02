use super::*;
use crate::domain::CreateApiToken;
use crate::server::discovery::{CATALOG_SEED, catalog_models, seed_catalog};
use crate::server::ops_tests::{send, state_with_artifacts};
use crate::server::router;
use axum::http::StatusCode;
use std::sync::{Arc, Mutex};

/// A LiteLLM stand-in that keeps its routes in memory: one existing route, an
/// `openrouter` credential, and `/model/new` adding what it is sent.
async fn litellm() -> (String, Arc<Mutex<Vec<Value>>>) {
    let routes = Arc::new(Mutex::new(vec![json!({
        "model_name": "frontier-sol",
        "litellm_params": {"model": "openrouter/openai/gpt-6-sol"},
        "model_info": {"db_model": true}
    })]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let listed = routes.clone();
    let added = routes.clone();
    let app = axum::Router::new()
        .route(
            "/model/info",
            get(move || {
                let routes = listed.clone();
                async move { Json(json!({"data": routes.lock().unwrap().clone()})) }
            }),
        )
        .route(
            "/credentials",
            get(|| async {
                Json(json!({"credentials": [{
                    "credential_name": "openrouter",
                    "credential_info": {"custom_llm_provider": "openrouter"},
                    "credential_values": {"api_key": "sk-or-****"}
                }]}))
            }),
        )
        .route(
            "/model/new",
            post(move |Json(body): Json<Value>| {
                let routes = added.clone();
                async move {
                    let mut info = body["model_info"].clone();
                    info["db_model"] = json!(true);
                    routes.lock().unwrap().push(json!({
                        "model_name": body["model_name"],
                        "litellm_params": body["litellm_params"],
                        "model_info": info
                    }));
                    Json(json!({"model_id": "new"}))
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (format!("http://{addr}/v1"), routes)
}

async fn configured(
    managed: bool,
) -> (tempfile::TempDir, AppState, String, Arc<Mutex<Vec<Value>>>) {
    let (base_url, routes) = litellm().await;
    let (directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "gateway", "kind": "litellm", "base_url": base_url, "api_key": "gateway-master",
                "managed_identity": {"enabled": managed, "max_budget": 20.0, "budget_duration": "30d"}
            }))
            .unwrap(),
        )
        .unwrap();
    (directory, state, endpoint.id, routes)
}

fn seed() {
    seed_catalog(catalog_models(&json!({"data": [
        {"id": "anthropic/claude-sonnet-5.5", "name": "Claude Sonnet 5.5", "context_length": 1000000,
         "pricing": {"prompt": "0.000003", "completion": "0.000015"},
         "top_provider": {"max_completion_tokens": 64000},
         "reasoning": {"supported_efforts": ["low", "high"]}},
        {"id": "vendor/unpriced", "name": "Unpriced", "context_length": 32000, "pricing": {}}
    ]})));
}

fn request(endpoint_id: &str) -> Value {
    json!({
        "endpoint_id": endpoint_id, "catalog_id": "anthropic/claude-sonnet-5.5",
        "route": "claude-sonnet-5.5", "credential": "openrouter",
        "context_tokens": 1000000, "max_output_tokens": 64000, "data_destination": "external"
    })
}

async fn add(state: &AppState, body: Value) -> (StatusCode, Value) {
    send(
        router(state.clone()),
        "POST",
        "/api/v1/discovery/add-route",
        "admin-secret",
        body,
    )
    .await
}

#[tokio::test]
async fn adds_a_catalog_model_with_its_prices_and_publishes_it() {
    let _seeding = CATALOG_SEED.lock().await;
    seed();
    let (_directory, state, endpoint_id, routes) = configured(true).await;
    let (status, profile) = add(&state, request(&endpoint_id)).await;
    assert_eq!(status, StatusCode::OK, "{profile}");
    let added = routes.lock().unwrap()[1].clone();
    assert_eq!(added["model_name"], "claude-sonnet-5.5");
    assert_eq!(
        added["litellm_params"]["model"],
        "openrouter/anthropic/claude-sonnet-5.5"
    );
    assert_eq!(
        added["litellm_params"]["litellm_credential_name"],
        "openrouter"
    );
    let per_mtok =
        |value: &Value| (value.as_f64().unwrap() * 1_000_000.0 * 1000.0).round() / 1000.0;
    assert_eq!(
        per_mtok(&added["litellm_params"]["input_cost_per_token"]),
        3.0
    );
    assert_eq!(
        per_mtok(&added["litellm_params"]["output_cost_per_token"]),
        15.0
    );
    assert_eq!(added["model_info"]["max_output_tokens"], 64000);
    assert_eq!(added["model_info"]["supports_reasoning"], true);
    assert_eq!(profile["model_id"], "claude-sonnet-5.5");
    assert_eq!(profile["data_destination"], "external");
    assert_eq!(profile["context_tokens"], 1000000);
    assert_eq!(profile["max_output_tokens"], 64000);
    assert_eq!(profile["catalog"]["pricing"]["source"], "litellm_route");
    assert_eq!(
        profile["catalog"]["served_model"],
        "openrouter/anthropic/claude-sonnet-5.5"
    );
}

#[tokio::test]
async fn refuses_before_writing_anything_to_litellm() {
    let _seeding = CATALOG_SEED.lock().await;
    seed();
    let (_directory, state, endpoint_id, routes) = configured(true).await;
    let with = |field: &str, value: Value| {
        let mut body = request(&endpoint_id);
        body[field] = value;
        body
    };
    let refused = [
        (
            with("credential", json!("anthropic")),
            StatusCode::BAD_REQUEST,
        ),
        (with("route", json!("frontier-sol")), StatusCode::CONFLICT),
        (with("route", json!("two words")), StatusCode::BAD_REQUEST),
        (
            with("context_tokens", json!(2000000)),
            StatusCode::BAD_REQUEST,
        ),
        (
            with("max_output_tokens", json!(100000)),
            StatusCode::BAD_REQUEST,
        ),
        (with("data_destination", json!("")), StatusCode::BAD_REQUEST),
        (
            with("catalog_id", json!("vendor/missing")),
            StatusCode::NOT_FOUND,
        ),
        (
            with("catalog_id", json!("vendor/unpriced")),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (body, expected) in refused {
        let (status, answer) = add(&state, body.clone()).await;
        assert_eq!(status, expected, "{body}: {answer}");
    }
    assert_eq!(routes.lock().unwrap().len(), 1, "nothing may reach LiteLLM");
    assert!(state.store.list_model_profiles().unwrap().is_empty());

    let (_directory, unmanaged, unmanaged_id, unmanaged_routes) = configured(false).await;
    let (status, answer) = add(&unmanaged, request(&unmanaged_id)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(unmanaged_routes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn only_the_operator_may_list_credentials_or_add_routes() {
    let (_directory, state, endpoint_id, routes) = configured(true).await;
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "dispatch".into(),
            permissions: vec![
                "models:read".into(),
                "inference:invoke".into(),
                "external:invoke".into(),
            ],
        })
        .unwrap();
    let credentials = format!("/api/v1/discovery/credentials?endpoint_id={endpoint_id}");
    for token in [issued.token.as_str(), "", "runner-secret"] {
        for (method, uri) in [
            ("GET", credentials.as_str()),
            ("POST", "/api/v1/discovery/add-route"),
        ] {
            let (status, body) = send(
                router(state.clone()),
                method,
                uri,
                token,
                request(&endpoint_id),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {body}");
        }
    }
    let (status, body) = send(
        router(state),
        "GET",
        &credentials,
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["credentials"],
        json!([{"name": "openrouter", "provider": "openrouter"}])
    );
    assert!(!body.to_string().contains("sk-or"));
    assert_eq!(routes.lock().unwrap().len(), 1);
}
