use super::*;
use crate::server::{
    ops_tests::{send, state_with_artifacts},
    router,
};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn scoped_inference_keeps_upstream_private_and_fences_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(bearer(&headers), Some("upstream-secret"));
                assert_eq!(body["model"], "accepted-model");
                assert!(body["max_tokens"].as_u64().unwrap() <= 1024);
                if body["stream"] == true {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>("data: starting\n\n");
                            std::future::pending::<()>().await;
                        }))
                        .unwrap()
                } else {
                    Response::new(Body::from(
                        json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]})
                            .to_string(),
                    ))
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state.store.create_endpoint(&serde_json::from_value(json!({"name":"private","kind":"openai","base_url":format!("http://{address}/v1"),"api_key":"upstream-secret"})).unwrap()).unwrap();
    let model = state.store.create_model_profile(&serde_json::from_value(json!({"name":"accepted","endpoint_id":endpoint.id,"model_id":"accepted-model","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never"})).unwrap()).unwrap();
    let job = state.store.submit_job(&serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":"scoped","model_profile_id":model.id,"harness_profile_id":"pi","prompt":"test"})).unwrap()).unwrap();
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "Ready")
        .unwrap();
    let app = router(state.clone());
    let (status, claim) = send(
        app.clone(),
        "POST",
        "/runner/v1/claim",
        "runner-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!claim.to_string().contains("upstream-secret"));
    assert!(!claim.to_string().contains(&address.to_string()));
    let token = claim["endpoint"]["api_key"].as_str().unwrap();
    let path = claim["endpoint"]["base_url"].as_str().unwrap();
    let uri = format!("{path}/chat/completions");
    for uri in [
        "/api/v1/overview".into(),
        "/runner/v1/claim".into(),
        format!("/runner/v1/jobs/{}/source", job.id),
        "/job/v1/other/models".into(),
    ] {
        let method = if uri.ends_with("claim") {
            "POST"
        } else {
            "GET"
        };
        assert_eq!(
            send(app.clone(), method, &uri, token, json!({})).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let body = json!({"model":"accepted-model","messages":[{"role":"user","content":"hello"}]});
    for credential in ["runner-secret", "admin-secret", "invalid"] {
        assert_eq!(
            send(app.clone(), "POST", &uri, credential, body.clone())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // A worker on a thinking-off profile sends reasoning_effort; it must pass.
    let mut thinking_off = body.clone();
    thinking_off["reasoning_effort"] = json!("none");
    assert_eq!(
        send(app.clone(), "POST", &uri, token, thinking_off).await.0,
        StatusCode::OK
    );
    for (key, value) in [
        ("model", json!("other")),
        ("api_base", json!("http://elsewhere")),
        ("n", json!(2)),
        ("max_tokens", json!(1025)),
        ("reasoning_effort", json!("unbounded")),
    ] {
        let mut invalid = body.clone();
        invalid[key] = value;
        assert_eq!(
            send(app.clone(), "POST", &uri, token, invalid).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // Later operator edits do not redirect an accepted attempt or change its key.
    state
        .store
        .connection()
        .unwrap()
        .execute(
            "UPDATE endpoints SET base_url='http://127.0.0.1:9/v1' WHERE id=?1",
            [&endpoint.id],
        )
        .unwrap();
    let (status, response) = send(app.clone(), "POST", &uri, token, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["choices"][0]["message"]["content"], "ok");
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(streamed.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        send(app.clone(), "POST", &uri, token, body.clone()).await.0,
        StatusCode::CONFLICT
    );
    state.store.request_cancel(&job.id).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), response.into_body().collect())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        send(app.clone(), "POST", &uri, token, body).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert!(state.occupancy.try_job_request(&job.id).is_some());
    upstream.abort();
}

#[tokio::test]
async fn token_expiry_rotation_and_terminal_status_survive_reopen() {
    let (directory, state) = state_with_artifacts(None);
    let endpoint = state.store.create_endpoint(&serde_json::from_value(json!({"name":"test","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""})).unwrap()).unwrap();
    let model = state.store.create_model_profile(&serde_json::from_value(json!({"name":"test","endpoint_id":endpoint.id,"model_id":"test","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never"})).unwrap()).unwrap();
    let job = state.store.submit_job(&serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":"expiry","model_profile_id":model.id,"harness_profile_id":"pi","prompt":"test"})).unwrap()).unwrap();
    assert!(state.store.issue_job_inference_token(&job.id).is_err());
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "ready")
        .unwrap();
    state.store.claim_runner_job(None).unwrap();
    let first = state.store.issue_job_inference_token(&job.id).unwrap();
    let token = state.store.issue_job_inference_token(&job.id).unwrap();
    assert!(
        !state
            .store
            .job_inference_authorized(&job.id, &first)
            .unwrap()
    );
    let reopened = crate::store::Store::open(
        &directory.path().join("poundlock.sqlite3"),
        state.config.secrets.clone(),
    )
    .unwrap();
    assert!(reopened.job_inference_authorized(&job.id, &token).unwrap());
    reopened
        .connection()
        .unwrap()
        .execute(
            "UPDATE jobs SET runner_lease_expires_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&job.id],
        )
        .unwrap();
    assert!(!reopened.job_inference_authorized(&job.id, &token).unwrap());
    assert!(reopened.issue_job_inference_token(&job.id).is_err());
    reopened.fail_expired_runners().unwrap();
    assert!(!reopened.job_inference_authorized(&job.id, &token).unwrap());
}

/// A job's model request that never answers must not hold the job until its
/// wall budget runs out, spending the full hour on one stuck request and
/// returning an empty patch. Both silences — before the
/// response starts, and between chunks after it has — now end in an error the
/// harness can see, well inside that budget.
#[tokio::test]
async fn a_model_that_stops_answering_ends_the_request_not_the_job() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|Json(body): Json<Value>| async move {
                if body["user"] == "silent" {
                    // Never sends a status line or headers.
                    std::future::pending::<()>().await;
                }
                // Starts streaming, then goes quiet.
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(async_stream::stream! {
                        yield Ok::<_, std::io::Error>("data: starting\n\n");
                        std::future::pending::<()>().await;
                    }))
                    .unwrap()
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let (_directory, state) = state_with_artifacts(None);
    let mut config = (*state.config).clone();
    config.job_inference_timeout_seconds = 1;
    let state = AppState {
        config: std::sync::Arc::new(config),
        ..state
    };
    let endpoint = state.store.create_endpoint(&serde_json::from_value(json!({"name":"stalls","kind":"openai","base_url":format!("http://{address}/v1"),"api_key":"k"})).unwrap()).unwrap();
    let model = state.store.create_model_profile(&serde_json::from_value(json!({"name":"stalls","endpoint_id":endpoint.id,"model_id":"stalls-model","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never"})).unwrap()).unwrap();
    let job = state.store.submit_job(&serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":"stalls","model_profile_id":model.id,"harness_profile_id":"pi","prompt":"test"})).unwrap()).unwrap();
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "Ready")
        .unwrap();
    let app = router(state.clone());
    let (_, claim) = send(
        app.clone(),
        "POST",
        "/runner/v1/claim",
        "runner-secret",
        json!({}),
    )
    .await;
    let token = claim["endpoint"]["api_key"].as_str().unwrap().to_owned();
    let uri = format!(
        "{}/chat/completions",
        claim["endpoint"]["base_url"].as_str().unwrap()
    );
    let body = json!({"model":"stalls-model","messages":[{"role":"user","content":"hello"}]});

    let mut silent = body.clone();
    silent["user"] = json!("silent");
    let (status, error) = tokio::time::timeout(
        Duration::from_secs(10),
        send(app.clone(), "POST", &uri, &token, silent),
    )
    .await
    .expect("a silent model must not hold the request open");
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(error["error"]["code"], "upstream_timeout");
    // The request slot is released, so the job's next request can go out.
    assert!(state.occupancy.try_job_request(&job.id).is_some());

    let mut stalled = body.clone();
    stalled["stream"] = json!(true);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(stalled.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let collected = tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
        .await
        .expect("a stream that goes quiet must end");
    assert!(
        collected.is_err(),
        "a stalled stream ends in an error, not a clean finish"
    );
    upstream.abort();
}

#[tokio::test]
async fn durable_jobs_on_llamacpp_get_the_same_reasoning_rules_as_the_relay() {
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = seen.clone();
    tokio::spawn(async move {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(body);
                    Json(json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}))
                }
            }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({"name":"private","kind":"openai",
                "base_url":format!("http://{address}/v1"),"api_key":""}))
            .unwrap(),
        )
        .unwrap();
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"durable-llama","endpoint_id":endpoint.id,"model_id":"qwen",
                "context_tokens":8192,"max_output_tokens":1024,
                "workload_class":"durable-coding-agent","wake_policy":"never",
                "data_destination":"operator_network","thinking":"off",
                "thinking_budget_tokens":256,
                "node_id":node.node_id,"runtime_profile":"llamacpp"
            }))
            .unwrap(),
        )
        .unwrap();
    let job = state
        .store
        .submit_job(
            &serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":"llama",
                "model_profile_id":model.id,"harness_profile_id":"pi","prompt":"test"}))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "Ready")
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
    let token = claim["endpoint"]["api_key"].as_str().unwrap();
    let uri = format!(
        "{}/chat/completions",
        claim["endpoint"]["base_url"].as_str().unwrap()
    );
    let body = |extra: Value| {
        let mut body = json!({"model":"qwen","messages":[{"role":"user","content":"hi"}]});
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };
    // Pi sends no reasoning field, and the profile says off.
    assert_eq!(
        send(app.clone(), "POST", &uri, token, body(json!({})))
            .await
            .0,
        StatusCode::OK
    );
    // A client that turns thinking on gets the profile's budget.
    let on = body(json!({"chat_template_kwargs":{"enable_thinking":true}}));
    assert_eq!(
        send(app.clone(), "POST", &uri, token, on).await.0,
        StatusCode::OK
    );
    let own = body(json!({"reasoning_effort":"low","thinking_budget_tokens":100}));
    assert_eq!(
        send(app.clone(), "POST", &uri, token, own).await.0,
        StatusCode::OK
    );
    for bad in [json!(0), json!(1025), json!("100")] {
        let refused = body(json!({"thinking_budget_tokens": bad}));
        let (status, response) = send(app.clone(), "POST", &uri, token, refused).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(
            response["error"]["code"], "invalid_thinking_budget",
            "{bad}"
        );
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "refusals never reach the model");
    assert_eq!(
        seen[0]["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert!(seen[0].get("reasoning_effort").is_none());
    assert!(seen[0].get("thinking_budget_tokens").is_none());
    assert_eq!(seen[1]["thinking_budget_tokens"], 256);
    assert_eq!(
        seen[2]["chat_template_kwargs"],
        json!({"enable_thinking": true, "reasoning_effort": "low"})
    );
    assert_eq!(seen[2]["thinking_budget_tokens"], 100);
}
