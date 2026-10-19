//! Revoking an application token reaches requests already in flight.
use super::ops_tests::state_with_artifacts;
use super::router;
use crate::domain::CreateApiToken;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// An upstream that keeps streaming for far longer than the test waits.
async fn endless_upstream() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            axum::http::Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(async_stream::stream! {
                    for _ in 0..300 {
                        yield Ok::<_, std::io::Error>(
                            "data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n".to_string(),
                        );
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }))
                .unwrap()
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn revoking_a_token_stops_its_inference_stream() {
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":endless_upstream().await,"api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"fast-code-completion",
                "wake_policy":"never",
                "data_destination":"operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["inference:invoke".into()],
        })
        .unwrap();
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {}", issued.token))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"stream":true})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    assert!(
        body.frame().await.unwrap().is_ok(),
        "the stream never started"
    );

    let revoked_at = Instant::now();
    assert!(state.store.revoke_api_token(&issued.details.id).unwrap());
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match body.frame().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(error.to_string()),
                None => return Ok(()),
            }
        }
    })
    .await
    .expect("the stream outlived its token's revocation");
    assert!(ended.is_err(), "a revoked stream must end as a failure");
    assert!(revoked_at.elapsed() < Duration::from_secs(5));
}
