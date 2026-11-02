//! Editing an application token keeps its secret and changes what it may do.
use crate::server::ops_tests::{send, state_with_artifacts};
use crate::server::router;
use crate::store::OPERATOR_CHECK_TOKEN;
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn an_edited_token_keeps_its_secret_and_loses_a_removed_permission() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state);
    let (_, issued) = send(
        app.clone(),
        "POST",
        "/api/v1/api-tokens",
        "admin-secret",
        json!({"name":"editor","permissions":["models:read","jobs:read"]}),
    )
    .await;
    let secret = issued["token"].as_str().unwrap();
    let path = format!("/api/v1/api-tokens/{}", issued["id"].as_str().unwrap());
    let edit = |token: &'static str, body| send(app.clone(), "PATCH", &path, token, body);

    let (status, edited) = edit(
        "admin-secret",
        json!({"name":" laptop ","permissions":["jobs:read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["name"], "laptop");
    assert_eq!(edited["permissions"], json!(["jobs:read"]));
    let (status, _) = send(app.clone(), "GET", "/v1/models", secret, json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A token without models:read still learns what its permissions are called.
    let (status, capabilities) = send(
        app.clone(),
        "GET",
        "/api/v1/capabilities",
        secret,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["token_permissions"][0]["code"], "models:read");
    assert_eq!(capabilities["caller_permissions"], json!(["jobs:read"]));
    let (status, _) = send(app.clone(), "GET", "/api/v1/jobs", secret, json!({})).await;
    assert_eq!(status, StatusCode::OK);

    let (status, refused) = edit("admin-secret", json!({"name":"laptop","permissions":[]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        refused["error"]["message"],
        "choose at least one permission"
    );
    let (status, refused) = edit(
        "admin-secret",
        json!({"name":OPERATOR_CHECK_TOKEN,"permissions":["jobs:read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");

    send(
        app.clone(),
        "POST",
        &format!("{path}/revoke"),
        "admin-secret",
        json!({}),
    )
    .await;
    let (status, _) = edit(
        "admin-secret",
        json!({"name":"laptop","permissions":["jobs:read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
