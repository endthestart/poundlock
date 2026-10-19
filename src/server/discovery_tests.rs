use super::*;
use crate::domain::CreateApiToken;
use crate::server::ops_tests::{send, state_with_artifacts};
use crate::server::router;
use crate::store::retirement::RouteOutcome;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::sync::Arc;

/// The provider catalog cache is process-wide; tests that seed it take turns.
static CATALOG_SEED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A LiteLLM stand-in: two routes, a health check, and a chat endpoint whose
/// reasoning answer depends on the requested effort — `none` reasons not at
/// all, `low` and `high` do, `medium` is refused and `xhigh` is silently
/// dropped (reasons nothing), as LiteLLM does with `drop_params`.
async fn gateway() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/model/info",
            axum::routing::get(|| async {
                axum::Json(json!({"data": [
                    {
                        "model_name": "frontier-sol",
                        "litellm_params": {
                            "model": "openrouter/openai/gpt-6-sol",
                            "input_cost_per_token": 0.000002,
                            "output_cost_per_token": 0.00001,
                            "api_key": "sk-or-****"
                        },
                        "model_info": {"db_model": true, "max_input_tokens": 400000, "supports_reasoning": true}
                    },
                    {
                        "model_name": "local-qwen",
                        "litellm_params": {"model": "openai/qwen", "api_base": "http://vega:8080/v1"},
                        "model_info": {"db_model": false}
                    }
                ]}))
            }),
        )
        .route(
            "/health",
            axum::routing::get(
                |axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    if query.get("model").map(String::as_str) == Some("frontier-sol") {
                        axum::Json(json!({"healthy_count": 1, "healthy_endpoints": [{"model": "x"}], "unhealthy_endpoints": []}))
                            .into_response()
                    } else {
                        // LiteLLM answers an unhealthy route with 503 and its report.
                        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"healthy_endpoints": [],
                            "unhealthy_endpoints": [{"error": "Connection error.\nstack trace: Traceback ..."}]})))
                            .into_response()
                    }
                },
            ),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post({
                let requests = requests.clone();
                move |axum::Json(body): axum::Json<Value>| {
                    let requests = requests.clone();
                    async move {
                        requests.lock().unwrap().push(body.clone());
                        let tokens = match body["reasoning_effort"].as_str() {
                            Some("low") | Some("high") => 50,
                            Some("medium") => {
                                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": {"message": "no"}})));
                            }
                            _ => 0,
                        };
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "choices": [{"message": {"content": "5 cents"}, "finish_reason": "stop"}],
                                "usage": {"completion_tokens": 60, "completion_tokens_details": {"reasoning_tokens": tokens}}
                            })),
                        )
                    }
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (format!("http://{addr}/v1"), requests)
}

async fn configured() -> (
    tempfile::TempDir,
    AppState,
    String,
    Arc<std::sync::Mutex<Vec<Value>>>,
) {
    let (base_url, requests) = gateway().await;
    let (directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "gateway", "kind": "litellm", "base_url": base_url, "api_key": "gateway-master"
            }))
            .unwrap(),
        )
        .unwrap();
    (directory, state, endpoint.id, requests)
}

async fn publish(state: &AppState, endpoint_id: &str) -> Value {
    let (status, body) = send(
        router(state.clone()),
        "POST",
        "/api/v1/discovery/publish",
        "admin-secret",
        json!({"endpoint_id": endpoint_id, "route": "frontier-sol", "data_destination": "external"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn discovery_routes_refuse_application_tokens_and_anonymous_callers() {
    let (_directory, state, endpoint_id, _) = configured().await;
    let profile = publish(&state, &endpoint_id).await;
    let id = profile["id"].as_str().unwrap();
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
    let routes = [
        ("GET", "/api/v1/discovery".to_owned()),
        ("POST", "/api/v1/discovery/test".to_owned()),
        ("POST", "/api/v1/discovery/publish".to_owned()),
        ("POST", format!("/api/v1/model-profiles/{id}/sync-route")),
        (
            "POST",
            format!("/api/v1/model-profiles/{id}/verify-reasoning"),
        ),
        ("POST", format!("/api/v1/model-profiles/{id}/enable")),
    ];
    for (method, uri) in routes {
        for token in [issued.token.as_str(), "", "runner-secret"] {
            let (status, body) = send(
                router(state.clone()),
                method,
                &uri,
                token,
                json!({"endpoint_id": endpoint_id, "route": "frontier-sol", "data_destination": "external"}),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {body}");
            assert!(!body.to_string().contains("gateway-master"));
        }
    }
}

#[tokio::test]
async fn discovery_keeps_listings_routes_and_published_profiles_apart() {
    let (_directory, state, endpoint_id, _) = configured().await;
    let _seeding = CATALOG_SEED.lock().await;
    seed_catalog(catalog_models(&json!({"data": [
        {"id": "openai/gpt-6-sol", "name": "GPT-6 Sol", "context_length": 400000,
         "pricing": {"prompt": "0.000002", "completion": "0.00001"},
         "reasoning": {"supported_efforts": ["none", "low", "high", "max"], "default_effort": "medium"}},
        {"id": "vendor/new-model", "name": "New", "pricing": {"prompt": "0", "completion": "0"}}
    ]})));
    publish(&state, &endpoint_id).await;
    let (status, body) = send(
        router(state),
        "GET",
        "/api/v1/discovery?catalog=openrouter",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["catalog"]["requested"], true);
    let routes = body["gateways"][0]["routes"].as_array().unwrap();
    let sol = routes
        .iter()
        .find(|row| row["name"] == "frontier-sol")
        .unwrap();
    assert_eq!(sol["state"], "published");
    assert_eq!(sol["profile"]["status"], "enabled");
    assert_eq!(sol["stored_in"], "database");
    let qwen = routes
        .iter()
        .find(|row| row["name"] == "local-qwen")
        .unwrap();
    assert_eq!(qwen["state"], "configured_route");
    assert_eq!(qwen["provider"], "openai-compatible");
    assert_eq!(qwen["pricing"], Value::Null);
    let models = body["catalog"]["models"].as_array().unwrap();
    let listed = |id: &str| models.iter().find(|model| model["id"] == id).unwrap()["state"].clone();
    assert_eq!(listed("openai/gpt-6-sol"), "configured_route");
    assert_eq!(listed("vendor/new-model"), "catalog_listing");
    assert!(
        body["catalog"]["note"]
            .as_str()
            .unwrap()
            .contains("not proof")
    );
    assert!(
        !body.to_string().contains("sk-or"),
        "no key material, masked or not"
    );
}

#[tokio::test]
async fn publishing_requires_a_declared_destination_and_records_route_facts() {
    let (_directory, state, endpoint_id, _) = configured().await;
    let app = router(state.clone());
    for destination in [json!(null), json!("unknown"), json!("anywhere")] {
        let (status, _) = send(
            app.clone(),
            "POST",
            "/api/v1/discovery/publish",
            "admin-secret",
            json!({"endpoint_id": endpoint_id, "route": "frontier-sol", "data_destination": destination}),
        )
        .await;
        assert!(status.is_client_error(), "{destination}");
    }
    let (status, _) = send(
        app,
        "POST",
        "/api/v1/discovery/publish",
        "admin-secret",
        json!({"endpoint_id": endpoint_id, "route": "absent", "data_destination": "external"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let profile = publish(&state, &endpoint_id).await;
    assert_eq!(profile["model_id"], "frontier-sol");
    assert_eq!(profile["context_tokens"], 131072);
    // The same defaults as adding any other model: a quarter of the context
    // at most 32,768, for agent jobs and interactive requests.
    assert_eq!(profile["max_output_tokens"], 32768);
    assert_eq!(profile["uses"], json!(["durable", "interactive"]));
    assert_eq!(
        profile["catalog"]["served_model"],
        "openrouter/openai/gpt-6-sol"
    );
    assert_eq!(profile["catalog"]["provider"], "openrouter");
    assert_eq!(profile["catalog"]["pricing"]["source"], "litellm_route");
    assert!(
        (profile["catalog"]["pricing"]["input_per_mtok"]
            .as_f64()
            .unwrap()
            - 2.0)
            .abs()
            < 1e-9
    );
    assert!(
        (profile["catalog"]["pricing"]["output_per_mtok"]
            .as_f64()
            .unwrap()
            - 10.0)
            .abs()
            < 1e-9
    );
    assert_eq!(profile["catalog"]["reasoning_levels"], json!([]));
}

#[tokio::test]
async fn route_test_uses_the_gateway_health_check_not_inference() {
    let (_directory, state, endpoint_id, requests) = configured().await;
    let app = router(state);
    let (status, body) = send(
        app.clone(),
        "POST",
        "/api/v1/discovery/test",
        "admin-secret",
        json!({"endpoint_id": endpoint_id, "route": "frontier-sol"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["healthy"], true);
    let (_, body) = send(
        app,
        "POST",
        "/api/v1/discovery/test",
        "admin-secret",
        json!({"endpoint_id": endpoint_id, "route": "local-qwen"}),
    )
    .await;
    assert_eq!(body["healthy"], false);
    assert_eq!(body["error"], "Connection error.");
    assert!(
        requests.lock().unwrap().is_empty(),
        "the admin key never carries inference"
    );
}

#[tokio::test]
async fn reasoning_verification_keeps_only_levels_with_a_visible_effect() {
    let (_directory, state, endpoint_id, requests) = configured().await;
    let profile = publish(&state, &endpoint_id).await;
    let id = profile["id"].as_str().unwrap();
    let (status, body) = send(
        router(state.clone()),
        "POST",
        &format!("/api/v1/model-profiles/{id}/verify-reasoning"),
        "admin-secret",
        json!({"levels": ["high", "none", "low", "medium", "xhigh"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // medium was refused, xhigh reasoned nothing (dropped on the way).
    assert_eq!(body["verified"], json!(["none", "low", "high"]));
    assert_eq!(requests.lock().unwrap().len(), 5);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["model"] == "frontier-sol")
    );

    let stored = state.store.model_profile(id).unwrap().unwrap();
    assert_eq!(stored.catalog.reasoning_levels, ["none", "low", "high"]);
    assert!(stored.catalog.reasoning_verified_at.is_some());

    // Syncing the same route keeps the evidence; it was taken on this model.
    let (status, synced) = send(
        router(state.clone()),
        "POST",
        &format!("/api/v1/model-profiles/{id}/sync-route"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{synced}");
    assert_eq!(
        synced["catalog"]["reasoning_levels"],
        json!(["none", "low", "high"])
    );

    let (status, _) = send(
        router(state),
        "POST",
        &format!("/api/v1/model-profiles/{id}/verify-reasoning"),
        "admin-secret",
        json!({"levels": ["max"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "max is never offered");
}

/// A llama-server stand-in on b11192's terms: no reasoning-token count in
/// usage, reasoning text in `message.reasoning_content`, and thinking switched
/// only by `chat_template_kwargs` (a top-level `reasoning_effort` is ignored).
/// With `honours_kwargs` off it never thinks, like a server that drops the field.
async fn llamacpp_server(honours_kwargs: bool) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post({
            let requests = requests.clone();
            move |axum::Json(body): axum::Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.lock().unwrap().push(body.clone());
                    let thinks = honours_kwargs
                        && body["chat_template_kwargs"]["enable_thinking"] == json!(true);
                    let mut message = json!({"role": "assistant", "content": "5 cents"});
                    if thinks {
                        message["reasoning_content"] =
                            json!("Let the ball cost x, so the bat costs x + 1.00");
                    }
                    axum::Json(json!({
                        "choices": [{"message": message, "finish_reason": "stop"}],
                        "usage": {"completion_tokens": 60}
                    }))
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (format!("http://{addr}/v1"), requests)
}

async fn verify_on_runtime(
    runtime: Option<&str>,
    honours_kwargs: bool,
    levels: Value,
) -> (Value, Vec<Value>) {
    let (base_url, requests) = llamacpp_server(honours_kwargs).await;
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "llama", "kind": "openai", "base_url": base_url, "api_key": ""
            }))
            .unwrap(),
        )
        .unwrap();
    let node = runtime.map(|_| {
        state
            .store
            .enroll_node(&crate::domain::EnrollNode {
                token: state.store.create_enrollment_token(15, None).unwrap(),
                name: "orion".into(),
                platform: "linux".into(),
                capabilities: json!({}),
            })
            .unwrap()
            .node_id
    });
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "orion", "endpoint_id": endpoint.id, "model_id": "qwen",
                "context_tokens": 8192, "max_output_tokens": 4096,
                "workload_class": "interactive-coding", "wake_policy": "never",
                "data_destination": "operator_network", "thinking": "off",
                "node_id": node, "runtime_profile": runtime
            }))
            .unwrap(),
        )
        .unwrap();
    let (status, body) = send(
        router(state),
        "POST",
        &format!("/api/v1/model-profiles/{}/verify-reasoning", profile.id),
        "admin-secret",
        json!({"levels": levels}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = requests.lock().unwrap().clone();
    (body, seen)
}

#[tokio::test]
async fn llamacpp_levels_are_verified_from_the_reasoning_text() {
    let (body, seen) =
        verify_on_runtime(Some("llamacpp"), true, json!(["none", "low", "medium"])).await;
    // Off on the profile, yet the probe for `low` turned thinking on: it went
    // out as clients' requests do, through the template kwargs.
    assert_eq!(body["verified"], json!(["none", "low", "medium"]), "{body}");
    assert_eq!(
        seen[0]["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert_eq!(
        seen[1]["chat_template_kwargs"],
        json!({"enable_thinking": true, "reasoning_effort": "low"})
    );
    let results = body["results"].as_array().unwrap();
    assert_eq!(results[0]["reasoning_chars"], 0);
    assert!(results[1]["reasoning_chars"].as_u64().unwrap() > 0);
    // There is still no token count to report: the characters are labelled as such.
    assert_eq!(results[1]["reasoning_tokens"], Value::Null);
}

#[tokio::test]
async fn a_llamacpp_level_the_server_drops_is_not_verified() {
    let (body, _) = verify_on_runtime(Some("llamacpp"), false, json!(["none", "low"])).await;
    assert_eq!(body["verified"], json!(["none"]), "{body}");
    assert_eq!(body["results"][1]["verified"], false);
}

#[tokio::test]
async fn reasoning_text_alone_verifies_nothing_on_other_runtimes() {
    // The same answers from a profile that is not on the llamacpp runtime:
    // no token count means the effect cannot be seen, as it always was.
    let (body, seen) = verify_on_runtime(None, true, json!(["none", "low"])).await;
    assert_eq!(body["verified"], json!([]), "{body}");
    assert_eq!(body["results"][0]["reasoning_chars"], Value::Null);
    assert_eq!(seen[1]["reasoning_effort"], "low");
    assert!(seen[1].get("chat_template_kwargs").is_none());
}

#[test]
fn reasoning_text_counts_only_in_a_readable_chat_answer() {
    let with = json!({"choices": [{"message": {"content": "a", "reasoning_content": "héllo"}}]});
    assert_eq!(reasoning_text_chars(&with), Some(5));
    let without = json!({"choices": [{"message": {"content": "a"}}]});
    assert_eq!(reasoning_text_chars(&without), Some(0));
    let null = json!({"choices": [{"message": {"content": "a", "reasoning_content": null}}]});
    assert_eq!(reasoning_text_chars(&null), Some(0));
    // An answer that is not a chat completion says nothing about reasoning.
    assert_eq!(reasoning_text_chars(&json!({"error": "x"})), None);
    assert_eq!(reasoning_text_chars(&Value::Null), None);
}

#[tokio::test]
async fn only_an_operator_puts_a_disabled_profile_back_in_service() {
    let (_directory, state, endpoint_id, _) = configured().await;
    let profile = publish(&state, &endpoint_id).await;
    let id = profile["id"].as_str().unwrap();
    let gone = RouteOutcome::Gone {
        status: 404,
        detail: "HTTP 404: upstream reported \"no endpoints found\"".into(),
    };
    for at in [
        "2026-09-23T10:00:00Z",
        "2026-09-23T10:15:00Z",
        "2026-09-23T10:31:00Z",
    ] {
        state
            .store
            .record_route_outcome_at(id, "relay", &gone, at.into())
            .unwrap();
    }
    assert!(
        state
            .store
            .model_profile(id)
            .unwrap()
            .unwrap()
            .retirement
            .disabled
            .is_some()
    );
    // A later success does not re-enable it.
    state
        .store
        .record_route_outcome(id, "relay", &RouteOutcome::Served)
        .unwrap();
    assert!(
        state
            .store
            .model_profile(id)
            .unwrap()
            .unwrap()
            .retirement
            .disabled
            .is_some()
    );

    let (status, _) = send(
        router(state.clone()),
        "POST",
        &format!("/api/v1/model-profiles/{id}/verify-reasoning"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = send(
        router(state),
        "POST",
        &format!("/api/v1/model-profiles/{id}/enable"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["retirement"]["disabled"], Value::Null);
    assert_eq!(body["retirement"]["hard_failures"], Value::Null);
}

#[test]
fn a_level_is_verified_only_by_its_visible_effect() {
    assert!(level_verified("none", 200, Some(0)).0);
    assert!(!level_verified("none", 200, Some(12)).0);
    assert!(level_verified("high", 200, Some(40)).0);
    assert!(
        !level_verified("high", 200, Some(0)).0,
        "may have been dropped"
    );
    assert!(
        !level_verified("high", 200, None).0,
        "effect not observable"
    );
    assert!(!level_verified("low", 400, Some(40)).0);
}

#[test]
fn catalog_drops_max_and_reports_prices_per_million_tokens() {
    let models = catalog_models(&json!({"data": [
        {"id": "openai/gpt-6-sol", "pricing": {"prompt": "0.000002", "completion": "0.00001"},
         "reasoning": {"supported_efforts": ["none", "xhigh", "max"]}},
        {"name": "no id"}
    ]}));
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["reasoning_efforts"], json!(["none", "xhigh"]));
    assert!((models[0]["pricing"]["input_per_mtok"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(models[0]["pricing"]["source"], "openrouter_catalog");
}

#[test]
fn a_route_price_wins_over_the_bundled_cost_map() {
    let map = json!({"input_cost_per_token": 0.000001, "output_cost_per_token": 0.000002});
    assert_eq!(
        route_pricing(&json!({}), &map)["source"],
        "litellm_cost_map"
    );
    let route = json!({"input_cost_per_token": 0.000003, "output_cost_per_token": 0.000004});
    assert_eq!(route_pricing(&route, &map)["source"], "litellm_route");
    assert_eq!(route_pricing(&json!({}), &json!({})), Value::Null);
    // The bundled map says 0 for a model it does not know.
    let unknown = json!({"input_cost_per_token": 0.0, "output_cost_per_token": 0.0});
    assert_eq!(route_pricing(&json!({}), &unknown), Value::Null);
    assert_eq!(
        route_pricing(&unknown, &json!({}))["source"],
        "litellm_route"
    );
}

#[test]
fn verified_levels_do_not_survive_a_change_of_served_model() {
    let previous = RouteCatalog {
        reasoning_levels: vec!["none".into(), "high".into()],
        reasoning_verified_at: Some("2026-09-23T10:00:00Z".into()),
        served_model: Some("openrouter/openai/gpt-5.6-sol".into()),
        ..Default::default()
    };
    let same = catalog_from_route(
        &json!({"upstream_model": "openrouter/openai/gpt-5.6-sol"}),
        &previous,
    );
    assert_eq!(same.reasoning_levels, ["none", "high"]);
    let moved = catalog_from_route(
        &json!({"upstream_model": "openrouter/openai/gpt-6-sol"}),
        &previous,
    );
    assert!(moved.reasoning_levels.is_empty());
    assert!(moved.reasoning_verified_at.is_none());
}

#[tokio::test]
async fn discovery_flags_stale_model_without_mutating_until_explicit_refresh() {
    let (_directory, state, endpoint_id, requests) = configured().await;
    let _seeding = CATALOG_SEED.lock().await;
    seed_catalog(Vec::new());
    let published = publish(&state, &endpoint_id).await;
    let id = published["id"].as_str().unwrap();
    let previous = state
        .store
        .patch_model_profile(
            id,
            &serde_json::from_value(json!({
                "catalog": {"served_model": "openrouter/openai/gpt-5.6-sol",
                    "reasoning_levels": ["none", "high"],
                    "reasoning_verified_at": "2026-09-23T10:00:00Z"},
                "model_label": "Owner's custom label"
            }))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
    let revision = state.store.routing_revision(&previous).unwrap();
    let (status, found) = send(
        router(state.clone()),
        "GET",
        "/api/v1/discovery",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let route = found["gateways"][0]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "frontier-sol")
        .unwrap();
    assert_eq!(route["state"], "published");
    assert_eq!(route["profile"]["model_changed"], true);
    assert_eq!(
        route["profile"]["served_model"],
        "openrouter/openai/gpt-5.6-sol"
    );
    let unchanged = state.store.model_profile(id).unwrap().unwrap();
    assert_eq!(unchanged.catalog.reasoning_levels, ["none", "high"]);
    assert_eq!(state.store.routing_revision(&unchanged).unwrap(), revision);
    let (status, synced) = send(
        router(state.clone()),
        "POST",
        &format!("/api/v1/model-profiles/{id}/sync-route"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{synced}");
    let current = state.store.model_profile(id).unwrap().unwrap();
    assert_eq!(
        current.catalog.served_model.as_deref(),
        Some("openrouter/openai/gpt-6-sol")
    );
    assert!(current.catalog.reasoning_levels.is_empty());
    assert!(current.catalog.reasoning_verified_at.is_none());
    assert_ne!(state.store.routing_revision(&current).unwrap(), revision);
    assert_eq!(current.model_label, previous.model_label);
    assert_eq!(current.name, previous.name);
    assert_eq!(current.context_tokens, previous.context_tokens);
    assert_eq!(current.max_output_tokens, previous.max_output_tokens);
    assert_eq!(current.data_destination, previous.data_destination);
    assert!(
        requests.lock().unwrap().is_empty(),
        "refresh never spends or infers"
    );
}

#[test]
fn a_health_error_keeps_the_reason_only() {
    let raw = "litellm.BadRequestError: OpenrouterException - {\"error\":{\"message\":\"x is not a valid model ID\",\"code\":400},\"user_id\":\"user_2abc\"}. Received Model Group=x\nstack trace: Traceback (most recent call last):";
    let clean = health_error(raw);
    assert!(clean.contains("is not a valid model ID"), "{clean}");
    assert!(
        !clean.contains("user_2abc") && !clean.contains("Traceback"),
        "{clean}"
    );
    assert!(
        clean.contains("\"user_id\":\"<redacted>\"}. Received Model Group=x"),
        "{clean}"
    );
}

#[tokio::test]
async fn a_public_address_is_a_link_not_a_route() {
    let (_directory, state, endpoint_id, _) = configured().await;
    let profile = publish(&state, &endpoint_id).await;
    let stored = state
        .store
        .model_profile(profile["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let before = state.store.routing_revision(&stored).unwrap();
    let app = router(state.clone());
    let (status, body) = send(
        app.clone(),
        "PATCH",
        &format!("/api/v1/endpoints/{endpoint_id}"),
        "admin-secret",
        json!({"public_url": "https://litellm.example.net/"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["public_url"], "https://litellm.example.net");
    assert_eq!(state.store.routing_revision(&stored).unwrap(), before);
    for bad in [
        "ftp://x",
        "https://user:pw@x",
        "https://x/?q=1",
        "not a url",
    ] {
        let (status, _) = send(
            app.clone(),
            "PATCH",
            &format!("/api/v1/endpoints/{endpoint_id}"),
            "admin-secret",
            json!({"public_url": bad}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (_, body) = send(
        app,
        "PATCH",
        &format!("/api/v1/endpoints/{endpoint_id}"),
        "admin-secret",
        json!({"public_url": ""}),
    )
    .await;
    assert!(body.get("public_url").is_none(), "blank clears it: {body}");
}

fn with_catalog_mode(state: AppState, mode: CatalogMode) -> AppState {
    let mut config = (*state.config).clone();
    config.openrouter_catalog = mode;
    AppState {
        config: Arc::new(config),
        ..state
    }
}

/// The catalog is a request to openrouter.ai. A seeded cache stands in for it:
/// any listed model in the answer means the catalog was read.
#[tokio::test]
async fn discovery_reads_the_external_catalog_only_when_asked() {
    let (_directory, state, _endpoint_id, _) = configured().await;
    let _seeding = CATALOG_SEED.lock().await;
    seed_catalog(catalog_models(
        &json!({"data": [{"id": "vendor/listed", "name": "Listed"}]}),
    ));
    for (mode, uri, requested) in [
        (CatalogMode::OnRequest, "/api/v1/discovery", false),
        (
            CatalogMode::OnRequest,
            "/api/v1/discovery?catalog=openrouter",
            true,
        ),
        (
            CatalogMode::Never,
            "/api/v1/discovery?catalog=openrouter",
            false,
        ),
        (CatalogMode::Always, "/api/v1/discovery", true),
    ] {
        let state = with_catalog_mode(state.clone(), mode);
        let (status, body) = send(router(state), "GET", uri, "admin-secret", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["catalog"]["mode"], mode.as_str());
        assert_eq!(body["catalog"]["requested"], requested, "{mode:?} {uri}");
        let listed = body["catalog"]["models"].as_array().unwrap().len();
        assert_eq!(listed, usize::from(requested), "{mode:?} {uri}");
        assert!(
            body["gateways"].is_array(),
            "gateway routes are local and always listed"
        );
    }
    let (status, _) = send(
        router(state),
        "GET",
        "/api/v1/discovery?catalog=elsewhere",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
