//! The node action poll: held open when the node asks, immediate when it does
//! not. Time is paused, so a 20 second hold costs the test nothing and the
//! elapsed times below are exact.
use super::ops_tests::{send, state_with_artifacts};
use super::*;
use crate::domain::{EnrollNode, NodeAction};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
use tower::ServiceExt;

#[derive(Clone)]
struct Node {
    id: String,
    credential: String,
}

fn enroll_named(state: &AppState, name: &str) -> Node {
    let enrolled = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: name.into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    Node {
        id: enrolled.node_id,
        credential: enrolled.credential,
    }
}

fn enroll(state: &AppState) -> Node {
    enroll_named(state, "gpu")
}

async fn poll(app: axum::Router, node: &Node, body: Value) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        &format!("/node/v1/nodes/{}/actions/poll", node.id),
        &node.credential,
        body,
    )
    .await
}

fn ensure(state: &AppState, node: &Node, lease: &str) -> NodeAction {
    state
        .store
        .enqueue_action(
            &node.id,
            None,
            "ensure",
            json!({"runtime_profile":"coding","lease_id":lease}),
        )
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn held_poll_returns_as_soon_as_an_action_is_queued() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let app = router(state.clone());
    let started = Instant::now();
    let held = tokio::spawn({
        let node = node.clone();
        async move { poll(app, &node, json!({"wait_seconds":20})).await }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!held.is_finished(), "nothing queued yet, so the poll waits");
    let queued = ensure(&state, &node, "lease-1");
    let (status, body) = held.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], queued.id.as_str());
    assert_eq!(body["status"], "running");
    assert_eq!(started.elapsed(), Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn held_poll_returns_null_when_the_wait_ends() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let started = Instant::now();
    let (status, body) = poll(router(state), &node, json!({"wait_seconds":7})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);
    assert_eq!(started.elapsed(), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn the_hold_is_capped_by_the_controller() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let started = Instant::now();
    let (status, body) = poll(router(state), &node, json!({"wait_seconds":86_400})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Value::Null);
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(crate::node::MAX_POLL_WAIT_SECONDS)
    );
}

#[tokio::test(start_paused = true)]
async fn held_poll_serves_queued_actions_in_order_without_waiting() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let first = ensure(&state, &node, "lease-1");
    let second = ensure(&state, &node, "lease-2");
    let app = router(state);
    let started = Instant::now();
    for expected in [first.id, second.id] {
        let (_, body) = poll(app.clone(), &node, json!({"wait_seconds":20})).await;
        assert_eq!(body["id"], expected.as_str());
    }
    assert_eq!(started.elapsed(), Duration::ZERO);
}

/// An agent that predates `wait_seconds` sends `{}`, or in principle nothing
/// useful at all, and must be answered at once.
#[tokio::test(start_paused = true)]
async fn a_poll_without_wait_seconds_answers_at_once() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let app = router(state.clone());
    let started = Instant::now();
    let uri = format!("/node/v1/nodes/{}/actions/poll", node.id);
    let (status, body) = poll(app.clone(), &node, json!({})).await;
    assert_eq!((status, body), (StatusCode::OK, Value::Null));
    for raw in ["", "not json", r#"{"wait_seconds":"soon"}"#, "[1,2]"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header("authorization", format!("Bearer {}", node.credential))
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "body {raw:?}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"null", "body {raw:?}");
    }
    let queued = ensure(&state, &node, "lease-1");
    let (_, body) = poll(app, &node, json!({})).await;
    assert_eq!(body["id"], queued.id.as_str());
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn held_poll_ends_when_the_node_is_revoked() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let started = Instant::now();
    let held = tokio::spawn({
        let (app, node) = (router(state.clone()), node.clone());
        async move { poll(app, &node, json!({"wait_seconds":20})).await }
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(state.store.remove_node(&node.id).unwrap());
    let (status, _) = held.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(started.elapsed(), Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn one_nodes_actions_do_not_wake_another_nodes_poll() {
    let (_directory, state) = state_with_artifacts(None);
    let node = enroll(&state);
    let other = enroll_named(&state, "other");
    let started = Instant::now();
    let held = tokio::spawn({
        let (app, node) = (router(state.clone()), node.clone());
        async move { poll(app, &node, json!({"wait_seconds":10})).await }
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    ensure(&state, &other, "elsewhere");
    let (_, body) = held.await.unwrap();
    assert_eq!(body, Value::Null);
    assert_eq!(started.elapsed(), Duration::from_secs(10));
}
