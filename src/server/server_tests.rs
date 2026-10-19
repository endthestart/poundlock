use super::*;
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn state() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("poundlock.sqlite3"), secrets.clone()).unwrap();
    let config = AppConfig {
        allow_job_network: false,
        wake_socket: None,
        git_hosts: vec!["github.com".into()],
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: directory.path().join("data"),
        runtime_dir: directory.path().join("runtime"),
        projects_dir: directory.path().join("projects"),
        work_dir: directory.path().join("work"),
        admin_token: "admin-secret".into(),
        runner_token: "runner-secret".into(),
        secrets,
        external_timeout_seconds: 30,
        job_inference_timeout_seconds: 30,
        node_offline_seconds: 15,
        client_artifacts_dir: None,
        max_active_jobs_per_token: 10,
        max_requests_in_flight: 512,
        capacity_default_wait_seconds: 0,
        openrouter_catalog: crate::config::CatalogMode::OnRequest,
        backup: None,
    };
    let state = AppState {
        config: Arc::new(config),
        managed_identities: crate::litellm::ManagedIdentityManager::new(store.clone()).unwrap(),
        store,
        inference: InferenceClient::new(30).unwrap(),
        occupancy: capacity::Occupancy::new(),
        runners: Default::default(),
    };
    (directory, state)
}

#[test]
fn static_bearer_scopes_require_the_exact_token() {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, "Bearer client-secret".parse().unwrap());
    assert!(authorize(&headers, "client-secret", "client").is_ok());
    assert!(authorize(&headers, "admin-secret", "admin").is_err());
    assert!(authorize(&HeaderMap::new(), "", "admin").is_err());
    assert!(authorize(&HeaderMap::new(), "admin-secret", "admin").is_err());
}

#[tokio::test]
async fn readiness_checks_the_store() {
    let (_directory, state) = state();
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
    assert_eq!(body["status"], "ready");
    // Readiness is deliberately scoped to the controller and its storage.
    assert_eq!(body["scope"], "controller_storage");
    assert_eq!(body["checks"]["storage"], "ok");
}

#[tokio::test]
async fn application_openapi_document_is_served_without_credentials() {
    let (_directory, state) = state();
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/openapi.yaml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/yaml; charset=utf-8"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.starts_with("openapi: 3.1.0"));
    assert!(text.contains("/api/v1/access/preflight:"));
    assert!(!text.contains("/api/v1/endpoints:"));
}

#[tokio::test]
async fn internal_errors_do_not_expose_details() {
    let response = ApiError::from(anyhow::anyhow!("private database path")).into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("internal server error"));
    assert!(!text.contains("private database path"));
}

#[tokio::test]
async fn structured_api_errors_include_only_public_reason_codes() {
    let response = ApiError::forbidden("external_permission_required", "external access denied")
        .into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["code"], "external_permission_required");
    assert_eq!(value["error"]["message"], "external access denied");
}

#[tokio::test]
async fn duplicate_names_are_conflicts_without_sql_text() {
    let (_directory, state) = state();
    let app = router(state);
    let body = serde_json::json!({
        "name": "local",
        "kind": "openai",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key": "k"
    });
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/endpoints")
                .header(AUTHORIZATION, "Bearer admin-secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/endpoints")
                .header(AUTHORIZATION, "Bearer admin-secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let text = String::from_utf8(
        second
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("already exists"));
    assert!(!text.contains("UNIQUE"));
    assert!(!text.contains("endpoints.name"));
}

/// Every operator-only route with a JSON body must refuse the wrong bearer
/// token, or none, before axum even tries to deserialize what was sent —
/// otherwise an application token, or an anonymous caller, gets a 422
/// deserialization error instead of a 401 by posting `{}`.
#[tokio::test]
async fn admin_authentication_precedes_json_body_parsing() {
    let (_directory, state) = state();
    let app_token = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "app".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap()
        .token;
    let app = router(state);
    let routes: &[(&str, &str)] = &[
        ("POST", "/api/v1/endpoints"),
        ("PATCH", "/api/v1/endpoints/missing"),
        ("POST", "/api/v1/model-profiles"),
        ("PATCH", "/api/v1/model-profiles/missing"),
        ("POST", "/api/v1/model-profiles/missing/test"),
        ("POST", "/api/v1/model-profiles/missing/verify-reasoning"),
        ("POST", "/api/v1/projects"),
        ("PATCH", "/api/v1/projects/missing"),
        ("POST", "/api/v1/discovery/test"),
        ("POST", "/api/v1/discovery/publish"),
        ("POST", "/api/v1/api-tokens"),
    ];
    for &(method, uri) in routes {
        let (status, body) =
            ops_tests::send(app.clone(), method, uri, "", serde_json::json!({})).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} with no token: {body}"
        );
        let (status, body) =
            ops_tests::send(app.clone(), method, uri, &app_token, serde_json::json!({})).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} with an application token: {body}"
        );
        let (status, body) = ops_tests::send(
            app.clone(),
            method,
            uri,
            "admin-secret",
            serde_json::json!({}),
        )
        .await;
        assert!(
            status.is_client_error() && status != StatusCode::UNAUTHORIZED,
            "{method} {uri} with the admin token should be a validation error, not {status}: {body}"
        );
    }
}

#[tokio::test]
async fn runner_authentication_precedes_any_request_body_poll() {
    let (_directory, state) = state();
    let app = router(state);
    for route in ["complete", "fail", "events"] {
        let body = Body::from_stream(futures_util::stream::poll_fn(|_| {
            panic!("unauthorized runner body was polled");
            #[allow(unreachable_code)]
            std::task::Poll::<Option<Result<bytes::Bytes, std::io::Error>>>::Pending
        }));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runner/v1/jobs/unknown/{route}"))
                    .header("content-type", "application/json")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn every_response_carries_browser_security_headers() {
    let (_directory, state) = state();
    let app = router(state);
    let get = |uri: &'static str| {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
    };
    for uri in ["/", "/app.js", "/api/v1/nodes", "/healthz"] {
        let response = get(uri).await.unwrap();
        let headers = response.headers();
        assert_eq!(
            headers["content-security-policy"],
            headers::CONTENT_SECURITY_POLICY,
            "{uri}"
        );
        assert!(
            headers::CONTENT_SECURITY_POLICY.contains("frame-ancestors 'none'"),
            "{uri}"
        );
        assert_eq!(headers["x-frame-options"], "DENY", "{uri}");
        assert_eq!(headers["x-content-type-options"], "nosniff", "{uri}");
        assert_eq!(headers["referrer-policy"], "no-referrer", "{uri}");
    }
    // API answers, including refusals, are never cached; static assets keep
    // their own caching.
    let refused = get("/api/v1/nodes").await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(refused.headers()["cache-control"], "no-store");
    let font = get("/fonts/inter.woff2").await.unwrap();
    assert_eq!(
        font.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
}

/// The policy forbids inline script and style, so the UI must not use them.
#[test]
fn the_ui_has_no_inline_script_style_or_handlers() {
    let html = include_str!("../../web/index.html");
    for tag in html.split("<script").skip(1) {
        let open = &tag[..tag.find('>').unwrap()];
        assert!(open.contains("src="), "inline <script{open}>");
    }
    assert!(!html.contains("<style"));
    let sources = [
        html,
        include_str!("../../web/app.js"),
        include_str!("../../web/jobs.js"),
        include_str!("../../web/connections.js"),
        include_str!("../../web/models.js"),
        include_str!("../../web/gateway-handoff.js"),
        include_str!("../../web/machines.js"),
        include_str!("../../web/theme-boot.js"),
    ];
    for source in sources {
        assert!(!source.contains(" style="), "inline style attribute");
        assert!(!source.contains("javascript:"), "javascript: URL");
        for handler in [
            " onclick=",
            " onload=",
            " onerror=",
            " onsubmit=",
            " onchange=",
        ] {
            assert!(!source.contains(handler), "inline {handler} handler");
        }
    }
}

#[tokio::test]
async fn moving_an_endpoint_to_another_origin_requires_its_key_again() {
    use super::ops_tests::send;
    use serde_json::json;
    let (_directory, state) = state();
    let app = router(state.clone());
    let create = |key: &str| {
        send(
            app.clone(),
            "POST",
            "/api/v1/endpoints",
            "admin-secret",
            json!({"name":format!("up-{key}"),"kind":"openai",
                "base_url":"https://models.example.test/v1","api_key":key}),
        )
    };
    let (status, keyed) = create("provider-key").await;
    assert_eq!(status, StatusCode::OK, "{keyed}");
    let path = format!("/api/v1/endpoints/{}", keyed["id"].as_str().unwrap());
    let patch = |body: serde_json::Value| send(app.clone(), "PATCH", &path, "admin-secret", body);

    for elsewhere in [
        "https://attacker.example.test/v1",
        "http://models.example.test/v1",
        "https://models.example.test:8443/v1",
    ] {
        let (status, body) = patch(json!({"base_url":elsewhere})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{elsewhere}: {body}");
        assert_eq!(body["error"]["code"], "api_key_required");
    }
    let saved = state
        .store
        .endpoint_secret(keyed["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(saved.endpoint.base_url, "https://models.example.test/v1");

    // Same origin, another path: the key stays with its host.
    let (status, body) = patch(json!({"base_url":"https://models.example.test/api/v1"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Re-entering the key moves it deliberately.
    let (status, body) = patch(json!({
        "base_url":"https://other.example.test/v1","api_key":"provider-key"
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A keyless endpoint has nothing to leak.
    let (_, keyless) = create("").await;
    let (status, body) = send(
        app.clone(),
        "PATCH",
        &format!("/api/v1/endpoints/{}", keyless["id"].as_str().unwrap()),
        "admin-secret",
        json!({"base_url":"http://10.20.0.9:8080/v1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
