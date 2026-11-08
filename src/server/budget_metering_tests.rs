use crate::domain::CreateApiToken;
use crate::server::budget::tests::{
    ADMIN, chat, configure, json_answer, message, priced_answer, spend, state, upstream,
};
use crate::server::ops_tests::send;
use crate::server::router;
use axum::body::Body;
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn an_unpriced_external_profile_cannot_run_under_a_ceiling() {
    let (_directory, state) = state();
    let (address, seen) = upstream(priced_answer).await;
    let (model, token) = configure(&state, &address, "external", None);
    let app = router(state);
    // Without a ceiling it runs, and is recorded as unpriced.
    let (status, _, _) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::OK);
    send(
        app.clone(),
        "PUT",
        "/api/v1/frontier-budget",
        ADMIN,
        json!({"monthly_usd": 100.0}),
    )
    .await;
    let (status, refused, _) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "frontier_budget_unpriced");
    assert_eq!(seen.lock().unwrap().len(), 1);
    let (_, models) = send(app, "GET", "/v1/models", &token, json!({})).await;
    assert_eq!(
        models["data"][0]["blocked_reason"],
        "frontier_budget_unpriced"
    );
}

#[tokio::test]
async fn a_stream_asks_for_its_usage_and_is_charged_the_providers_cost() {
    let (_directory, state) = state();
    let (address, seen) = upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"cost\":0.25}}\n\n",
                "data: [DONE]\n\n"
            )))
            .unwrap()
    })
    .await;
    let (model, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state);
    let mut body = message(&model);
    body["stream"] = json!(true);
    let (status, answer, _) = chat(app.clone(), &token, body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(answer["raw"].as_str().unwrap().contains("[DONE]"));
    assert_eq!(
        seen.lock().unwrap()[0]["stream_options"]["include_usage"],
        true
    );
    let view = spend(app).await;
    assert_eq!(view["spend"]["total_usd"], 0.25);
    assert_eq!(view["spend"]["lines"][0]["estimated_requests"], 0);
}

#[tokio::test]
async fn the_gateways_cost_wins_and_a_silent_answer_is_charged_its_estimate() {
    let (_directory, state) = state();
    let (address, _) = upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "application/json")
            .header("x-litellm-response-cost", "0.125")
            .body(Body::from(
                json!({"choices": [{"message": {"content": "ok"}}]}).to_string(),
            ))
            .unwrap()
    })
    .await;
    let (model, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state.clone());
    chat(app.clone(), &token, message(&model)).await;
    assert_eq!(spend(app.clone()).await["spend"]["total_usd"], 0.125);

    let (silent, _) =
        upstream(|| json_answer(json!({"choices": [{"message": {"content": "ok"}}]}))).await;
    let (other, other_token) = {
        let endpoint = state
            .store
            .create_endpoint(
                &serde_json::from_value(json!({
                    "name": "silent", "kind": "openai", "base_url": silent, "api_key": "k"
                }))
                .unwrap(),
            )
            .unwrap();
        let profile = state
            .store
            .create_model_profile(
                &serde_json::from_value(json!({
                    "name": "silent", "endpoint_id": endpoint.id, "model_id": "m",
                    "context_tokens": 8192, "max_output_tokens": 1000,
                    "workload_class": "fast-code-completion", "wake_policy": "never",
                    "data_destination": "external",
                }))
                .unwrap(),
            )
            .unwrap();
        let patch: crate::domain::PatchModelProfile = serde_json::from_value(json!({
            "catalog": {"pricing": {
                "input_per_mtok": 0.0, "output_per_mtok": 100.0,
                "source": "operator", "refreshed_at": "2026-10-16T00:00:00Z"
            }}
        }))
        .unwrap();
        state
            .store
            .patch_model_profile(&profile.id, &patch)
            .unwrap();
        (profile.name, token.clone())
    };
    let (status, _, _) = chat(app.clone(), &other_token, message(&other)).await;
    assert_eq!(status, StatusCode::OK);
    let view = spend(app).await;
    // The whole 1000-token output budget at $100 per million.
    assert_eq!(view["spend"]["total_usd"], 0.225);
    let silent_line = view["spend"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["profile_name"] == "silent")
        .unwrap()
        .clone();
    assert_eq!(silent_line["estimated_requests"], 1);
}

#[tokio::test]
async fn an_upstream_refusal_costs_nothing() {
    let (_directory, state) = state();
    let (address, _) = upstream(|| {
        axum::http::Response::builder()
            .status(429)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"error": {"message": "slow down"}}).to_string(),
            ))
            .unwrap()
    })
    .await;
    let (model, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state);
    let (status, _, _) = chat(app.clone(), &token, message(&model)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(spend(app).await["spend"]["total_usd"], 0.0);
}

#[tokio::test]
async fn the_budget_is_the_operators_to_set() {
    let (_directory, state) = state();
    let (address, _) = upstream(priced_answer).await;
    let (_, token) = configure(&state, &address, "external", Some((1.0, 10.0)));
    let app = router(state);
    let (status, _) = send(
        app.clone(),
        "GET",
        "/api/v1/frontier-budget",
        &token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        app.clone(),
        "PUT",
        "/api/v1/frontier-budget",
        &token,
        json!({"weekly_usd": 1.0}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for invalid in [
        json!({"weekly_usd": -5}),
        json!({"weekly_usd": 5, "warn_at": 1.5}),
        json!({"weekly": 5}),
    ] {
        let (status, _) = send(
            app.clone(),
            "PUT",
            "/api/v1/frontier-budget",
            ADMIN,
            invalid.clone(),
        )
        .await;
        assert!(status.is_client_error(), "{invalid} was accepted");
    }
    let view = spend(app).await;
    assert_eq!(
        view["settings"],
        json!({"warn_at": 0.8, "utc_offset_minutes": 0})
    );
}

#[tokio::test]
async fn a_durable_jobs_external_requests_count_against_the_ceiling() {
    let (_directory, state) = crate::server::ops_tests::state_with_artifacts(None);
    let (address, _) = upstream(priced_answer).await;
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "provider", "kind": "openai", "base_url": address, "api_key": "k"
            }))
            .unwrap(),
        )
        .unwrap();
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "frontier-jobs", "endpoint_id": endpoint.id, "model_id": "provider/model",
                "context_tokens": 200000, "max_output_tokens": 1000,
                "workload_class": "durable-coding-agent", "wake_policy": "never",
                "data_destination": "external",
            }))
            .unwrap(),
        )
        .unwrap();
    let patch: crate::domain::PatchModelProfile = serde_json::from_value(json!({
        "catalog": {"pricing": {
            "input_per_mtok": 1.0, "output_per_mtok": 10.0,
            "source": "operator", "refreshed_at": "2026-10-16T00:00:00Z"
        }}
    }))
    .unwrap();
    state
        .store
        .patch_model_profile(&profile.id, &patch)
        .unwrap();
    let submitter = state
        .store
        .create_api_token(&CreateApiToken {
            name: "outside-manager".into(),
            permissions: vec!["jobs:submit".into(), "external:invoke".into()],
        })
        .unwrap();
    let job = state
        .store
        .submit_job_for(
            &serde_json::from_value(json!({
                "source": {"kind": "empty"}, "idempotency_key": "metered",
                "model_profile_id": profile.id, "harness_profile_id": "pi", "prompt": "build"
            }))
            .unwrap(),
            Some(&submitter.details.id),
            10,
        )
        .unwrap();
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "Ready")
        .unwrap();
    state
        .store
        .save_frontier_budget(&crate::budget::FrontierBudget {
            weekly_usd: Some(1.0),
            ..Default::default()
        })
        .unwrap();
    let app = router(state);
    let (status, claim) = send(
        app.clone(),
        "POST",
        "/runner/v1/claim",
        "runner-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claim}");
    let token = claim["endpoint"]["api_key"].as_str().unwrap().to_owned();
    let uri = format!(
        "{}/chat/completions",
        claim["endpoint"]["base_url"].as_str().unwrap()
    );
    let body = json!({"model": "provider/model", "messages": [{"role": "user", "content": "go"}]});
    let (status, answer) = send(app.clone(), "POST", &uri, &token, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, refused) = send(app.clone(), "POST", &uri, &token, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "frontier_budget_exhausted");
    let view = spend(app).await;
    assert_eq!(view["spend"]["total_usd"], 2.0);
    assert_eq!(
        view["spend"]["lines"][0]["api_token_name"],
        "outside-manager"
    );
}
