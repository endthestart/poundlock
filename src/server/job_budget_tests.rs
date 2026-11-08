//! A durable job's own budget: its external spend and its wall clock.

use crate::server::budget::tests::{priced_answer, upstream};
use crate::server::ops_tests::send;
use crate::server::router;
use axum::http::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn a_jobs_own_budget_stops_its_external_requests_and_its_wall_clock() {
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
                "uses": ["durable"], "wake_policy": "never",
                "data_destination": "external",
            }))
            .unwrap(),
        )
        .unwrap();
    let submit = |key: &str, budget: Value| {
        let job = state
            .store
            .submit_job_for(
                &serde_json::from_value(json!({
                    "source": {"kind": "empty"}, "idempotency_key": key,
                    "model_profile_id": profile.id, "harness_profile_id": "pi", "prompt": "build",
                    "policy": {"budget": budget}
                }))
                .unwrap(),
                None,
                10,
            )
            .unwrap();
        state
            .store
            .set_job_status(&job.id, "ready", "ready", "Ready")
            .unwrap();
        job.id
    };
    // Without a price there is nothing to meter the job's budget by.
    let unpriced = submit(
        "unpriced",
        json!({"frontier_cents": 150, "wall_seconds": 300}),
    );
    let app = router(state.clone());
    let claim = |app: axum::Router| async move {
        let (status, claim) =
            send(app, "POST", "/runner/v1/claim", "runner-secret", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{claim}");
        let token = claim["endpoint"]["api_key"].as_str().unwrap().to_owned();
        let uri = format!(
            "{}/chat/completions",
            claim["endpoint"]["base_url"].as_str().unwrap()
        );
        (token, uri, claim)
    };
    let body = json!({"model": "provider/model", "messages": [{"role": "user", "content": "go"}]});
    let (token, uri, _) = claim(app.clone()).await;
    let (status, refused) = send(app.clone(), "POST", &uri, &token, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "job_budget_unpriced");
    state.store.request_cancel(&unpriced).ok();

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
    let id = submit("budgeted", json!({"frontier_cents": 150}));
    // A shorter wall budget than the runner's is the one the job gets.
    let walled = submit("walled", json!({"wall_seconds": 300}));
    let (token, uri, _) = claim(app.clone()).await;
    // No install ceiling: only the job's own $1.50 applies. The first request
    // costs $2 and is admitted; the job is then over, and the next is refused.
    let (status, answer) = send(app.clone(), "POST", &uri, &token, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, refused) = send(app.clone(), "POST", &uri, &token, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "job_budget_exhausted");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("$1.50")
    );
    let (_, job) = send(
        app.clone(),
        "GET",
        &format!("/api/v1/jobs/{id}"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(job["frontier_spend_usd"], 2.0);
    assert_eq!(job["policy"]["budget"]["frontier_cents"], 150);

    state.store.request_cancel(&id).ok();
    assert_eq!(
        state.store.claim_runner_job(Some(900)).unwrap().as_deref(),
        Some(walled.as_str())
    );
    let (_, job) = send(
        app,
        "GET",
        &format!("/api/v1/jobs/{walled}"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(job["wall_seconds"], 300);
}
