use super::*;
use crate::domain::{Endpoint, EndpointSecret, ResourceCheck};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

fn test_endpoint(base_url: &str) -> EndpointSecret {
    EndpointSecret {
        endpoint: Endpoint {
            id: "e".into(),
            name: "test".into(),
            kind: "openai".into(),
            base_url: base_url.into(),
            api_key_set: false,
            created_at: "now".into(),
            managed_identity: Default::default(),
            public_url: None,
            data_destination: None,
            last_check: ResourceCheck::default(),
        },
        api_key: String::new(),
    }
}

#[test]
fn generate_requires_answer_text() {
    for body in [
        json!({}),
        json!({"choices": []}),
        json!({"choices":[{"message":{"content":"","reasoning":"Thinking"},"finish_reason":"length"}]}),
        json!({"choices":[{"message":{"content":"  "}}]}),
    ] {
        assert!(validate_chat_completion(&body).is_err());
    }
    assert!(
        validate_chat_completion(&json!({"choices":[{"message":{"content":"OK","reasoning":"Thought"},"finish_reason":"stop"}]})).is_ok()
    );
}

#[test]
fn check_failure_does_not_echo_backend_details() {
    let client = InferenceClient::with_timeouts(10, 90).unwrap();
    let error = anyhow::anyhow!("connect to http://secret.internal:8080/v1 failed");
    let message = client.check_failure_message(&error, true);
    assert_eq!(message, "Generate check failed");
    assert!(!message.contains("secret"));
    assert_eq!(client.check_failure_message(&error, false), "Check failed");
}

#[test]
fn production_deadlines_keep_identity_short() {
    assert_eq!(IDENTITY_PROBE_TIMEOUT_SECONDS, 10);
    assert_eq!(GENERATE_CHECK_TIMEOUT_SECONDS, 90);
}

#[tokio::test]
async fn generate_check_times_out_without_retry_and_drops_the_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move {
        let client = InferenceClient::with_timeouts(1, 1).unwrap();
        let endpoint = test_endpoint(&format!("http://{addr}/v1"));
        let started = Instant::now();
        let result = client.completion_probe(&endpoint, "m", 16).await;
        (client, started.elapsed(), result)
    });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("generate check connected")
        .unwrap();
    let (client, elapsed, result) = client_task.await.unwrap();
    let error = result.expect_err("hanging backend must time out");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    let message = client.check_failure_message(&error, true);
    assert!(message.contains("did not finish in 1 second"), "{message}");
    assert!(message.contains("Retry Generate"), "{message}");
    let second = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(second.is_err(), "generate check must not retry");
    let mut buf = [0u8; 1024];
    let mut received = 0usize;
    loop {
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("timed-out generate check must not leave the connection hanging");
        match read {
            Ok(0) => break,
            Ok(n) => received += n,
            Err(error) => panic!("timed-out generate check must drop the connection: {error}"),
        }
    }
    assert!(
        received > 0,
        "generate check should send one request before timing out"
    );
}

#[tokio::test]
async fn identity_can_succeed_while_generate_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async { axum::Json(json!({"data":[{"id":"m"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                axum::routing::post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    axum::Json(json!({"choices":[{"message":{"content":"OK"}}]}))
                }),
            );
        axum::serve(listener, app).await.ok();
    });
    let client = InferenceClient::with_timeouts(1, 1).unwrap();
    let endpoint = test_endpoint(&format!("http://{addr}/v1"));
    let listed = client.probe(&endpoint, "m").await.unwrap();
    assert_eq!(
        listed["model_ids"],
        json!(["m"]),
        "the operator's model picker reads this"
    );
    let error = client
        .completion_probe(&endpoint, "m", 16)
        .await
        .expect_err("slow generation must not inherit a short identity success");
    let message = client.check_failure_message(&error, true);
    assert!(message.contains("did not finish"), "{message}");
}

#[tokio::test]
async fn probe_rejects_oversized_model_listings() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async {
                axum::Json(json!({"data":[{"id":"x".repeat(MAX_INFERENCE_JSON_BYTES)}]}))
            }),
        );
        axum::serve(listener, app).await.ok();
    });
    let client = InferenceClient::with_timeouts(5, 5).unwrap();
    let endpoint = test_endpoint(&format!("http://{addr}/v1"));
    let error = client
        .probe(&endpoint, "")
        .await
        .expect_err("oversize listing");
    assert!(error.to_string().contains("exceeded"), "{error:#}");
}
