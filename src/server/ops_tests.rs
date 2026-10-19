use super::*;
use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub(super) fn state_with_artifacts(
    artifacts: Option<std::path::PathBuf>,
) -> (tempfile::TempDir, AppState) {
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
        client_artifacts_dir: artifacts,
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

pub(super) async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json =
        serde_json::from_slice(&bytes).unwrap_or(json!({"raw": String::from_utf8_lossy(&bytes)}));
    (status, json)
}

fn endpoint_body() -> Value {
    json!({
        "name": "local",
        "kind": "openai",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key": ""
    })
}

#[tokio::test]
async fn wake_adapter_can_differ_from_the_node_connection() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state.store.enroll_node(&crate::domain::EnrollNode {
        token: state.store.create_enrollment_token(15, None).unwrap(),
        name: "wired-node".into(), platform: "linux".into(),
        capabilities: json!({"network":{"interface":"eth0","status":"connected","mac":"02:00:00:00:00:01"}}),
    }).unwrap();
    let app = router(state.clone());
    let uri = format!("/api/v1/nodes/{}/wol", node.node_id);
    let (status, _) = send(
        app.clone(),
        "PUT",
        &uri,
        "admin-secret",
        json!({"mac":"02:00:00:00:00:02","broadcast":"192.168.1.255:9"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state
            .store
            .node(&node.node_id, 15)
            .unwrap()
            .unwrap()
            .wol_mac
            .as_deref(),
        Some("02:00:00:00:00:02")
    );
    let (status, _) = send(
        app,
        "PUT",
        &uri,
        "admin-secret",
        json!({"mac":"02:00:00:00:00:01","broadcast":"192.168.1.255:9"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state
            .store
            .node(&node.node_id, 15)
            .unwrap()
            .unwrap()
            .wol_mac
            .as_deref(),
        Some("02:00:00:00:00:01")
    );
}

#[tokio::test]
async fn a_profile_cannot_name_a_model_its_runtime_cannot_serve() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "serving-node".into(),
            platform: "macos".into(),
            capabilities: json!({"inventory":{"runtimes":{"local-general":{
                "servable":["vega-active"]
            }}}}),
        })
        .unwrap();
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"direct","kind":"openai","base_url":"http://127.0.0.1:8080/v1","api_key":""}),
    )
    .await;
    let profile = |model_id: &str| {
        json!({"name":format!("profile-{model_id}"),"endpoint_id":endpoint["id"],"model_id":model_id,
            "context_tokens":8192,"max_output_tokens":1024,"uses":["interactive"],
            "wake_policy":"on-demand","data_destination":"operator_network",
            "node_id":node.node_id,"runtime_profile":"local-general"})
    };
    let (status, body) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        profile("vega-quality"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("vega-active")
    );
    let (status, _) = send(
        app,
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        profile("vega-active"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn wake_mac_must_belong_to_an_interface_the_machine_reports() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "two-nic-node".into(),
            platform: "linux".into(),
            capabilities: json!({"network":{"mode":"system","interfaces":[
                {"name":"data","mac":"02:00:00:00:00:01"},
                {"name":"board","mac":"02:00:00:00:00:02"}
            ]}}),
        })
        .unwrap();
    let app = router(state.clone());
    let uri = format!("/api/v1/nodes/{}/wol", node.node_id);
    let (status, body) = send(
        app.clone(),
        "PUT",
        &uri,
        "admin-secret",
        json!({"mac":"02:00:00:00:00:09","broadcast":"10.20.0.255:9"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("02:00:00:00:00:01")
    );
    // Waking a different interface than the one carrying inference is normal.
    let (status, _) = send(
        app,
        "PUT",
        &uri,
        "admin-secret",
        json!({"mac":"02:00:00:00:00:02","broadcast":"10.20.0.255:9"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn runner_reports_cross_default_json_limit_and_replay_only_identical_evidence() {
    use crate::domain::*;
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(&serde_json::from_value(endpoint_body()).unwrap())
        .unwrap();
    let model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"test", "endpoint_id":endpoint.id, "model_id":"test",
                "context_tokens":8192, "max_output_tokens":1024,
                "workload_class":"durable-coding-agent", "wake_policy":"never"
            }))
            .unwrap(),
        )
        .unwrap();
    let project = state
        .store
        .create_project(&CreateProject {
            name: "test".into(),
            roots: vec!["repo".into()],
            instruction_files: vec![],
        })
        .unwrap();
    let job = state
        .store
        .submit_job(&SubmitJob {
            source: None,
            idempotency_key: "test".into(),
            policy: Default::default(),
            project_id: project.id,
            model_profile_id: model.id,
            harness_profile_id: "pi".into(),
            prompt: "test".into(),
        })
        .unwrap();
    state
        .store
        .set_job_status(&job.id, "ready", "ready", "ready")
        .unwrap();
    state.store.claim_runner_job(None).unwrap();
    let app = router(state.clone());
    let report = json!({"patches":[{"root":"repo","base_revision":"base","patch":"diff"}],
        "output":"x".repeat(3 * 1024 * 1024),"provenance":{},"cleanup":{}});
    let uri = format!("/runner/v1/jobs/{}/complete", job.id);
    for _ in 0..2 {
        let (status, response) =
            send(app.clone(), "POST", &uri, "runner-secret", report.clone()).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["accepted"], true);
    }
    let patch_uri = format!("/api/v1/jobs/{}/patches/0", job.id);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&patch_uri)
                .header(AUTHORIZATION, "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "diff"
    );
    let submit_only = state
        .store
        .create_api_token(&CreateApiToken {
            name: "submit only".into(),
            permissions: vec!["jobs:submit".into()],
        })
        .unwrap();
    assert_eq!(
        send(
            app.clone(),
            "GET",
            &patch_uri,
            &submit_only.token,
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for suffix in ["?attempt=0", "?attempt=2"] {
        assert_eq!(
            send(
                app.clone(),
                "GET",
                &(patch_uri.clone() + suffix),
                "admin-secret",
                json!({})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    let mut changed = report;
    changed["output"] = json!("different evidence");
    let (status, _) = send(app, "POST", &uri, "runner-secret", changed).await;
    assert!(!status.is_success());
    assert_eq!(
        state.store.job(&job.id).unwrap().unwrap().result.unwrap()["output"]
            .as_str()
            .unwrap()
            .len(),
        3 * 1024 * 1024
    );
}

#[tokio::test]
async fn failed_endpoint_check_is_persisted() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (status, created) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let (status, _) = send(
        app.clone(),
        "POST",
        &format!("/api/v1/endpoints/{id}/test"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let listed = state.store.list_endpoints().unwrap();
    assert_eq!(listed[0].last_check.last_check_ok, Some(false));
    assert!(listed[0].last_check.last_checked_at.is_some());
    assert_eq!(
        listed[0].last_check.last_check_message.as_deref(),
        Some("Check failed")
    );
}

#[tokio::test]
async fn failed_generate_check_returns_the_operator_message() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let (_, model) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({
            "name":"generate-check",
            "endpoint_id":endpoint["id"],
            "model_id":"test",
            "context_tokens":8192,
            "max_output_tokens":16,
            "workload_class":"interactive-coding",
            "wake_policy":"never"
        }),
    )
    .await;
    let id = model["id"].as_str().unwrap();
    let (status, body) = send(
        app,
        "POST",
        &format!("/api/v1/model-profiles/{id}/test"),
        "admin-secret",
        json!({"generate":true}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["message"], "Generate check failed");
    assert_ne!(body["error"]["message"], "model compatibility check failed");
    let listed = state.store.list_model_profiles().unwrap();
    let profile = listed.iter().find(|item| item.id == id).unwrap();
    assert_eq!(profile.last_check.last_check_ok, Some(false));
    assert_eq!(
        profile.last_check.last_check_message.as_deref(),
        Some("Generate check failed")
    );
}

#[tokio::test]
async fn successful_endpoint_check_is_persisted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|| async { axum::Json(json!({"data":[{"id":"shown"}]})) }),
        );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let mut body = endpoint_body();
    body["base_url"] = json!(format!("http://{addr}/v1"));
    body["name"] = json!("reachable");
    let (status, created) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let (status, _) = send(
        app,
        "POST",
        &format!("/api/v1/endpoints/{id}/test"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = state.store.list_endpoints().unwrap();
    let endpoint = listed.iter().find(|item| item.id == id).unwrap();
    assert_eq!(endpoint.last_check.last_check_ok, Some(true));
    assert_eq!(
        endpoint.last_check.last_check_message.as_deref(),
        Some("Listed models")
    );
    assert!(
        state.store.list_model_profiles().unwrap().is_empty(),
        "testing an endpoint must not invent published model profiles"
    );
}

#[tokio::test]
async fn managed_litellm_setup_validates_key_management_separately() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async { axum::Json(json!({"data":[]})) }),
            )
            .route(
                "/key/list",
                axum::routing::get(|| async { axum::Json(json!({"keys":[]})) }),
            );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let body = json!({
        "name":"managed-gateway",
        "kind":"litellm",
        "base_url":format!("http://{addr}/v1"),
        "api_key":"sk-admin",
        "managed_identity":{
            "enabled":true,
            "max_budget":5.0,
            "budget_duration":"30d"
        }
    });
    let (status, created) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap();
    let (status, checked) = send(
        app.clone(),
        "POST",
        &format!("/api/v1/endpoints/{id}/test"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{checked}");
    assert_eq!(checked["poundlock"]["management"], "available");
    let endpoint = state
        .store
        .list_endpoints()
        .unwrap()
        .into_iter()
        .find(|item| item.id == id)
        .unwrap();
    assert_eq!(
        endpoint.last_check.last_check_message.as_deref(),
        Some("Listed models and verified application identity management")
    );
    let (_, profile) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({
            "name":"managed-external",
            "endpoint_id":id,
            "model_id":"provider/model",
            "context_tokens":8192,
            "max_output_tokens":1024,
            "workload_class":"interactive-coding",
            "wake_policy":"never",
            "data_destination":"external"
        }),
    )
    .await;
    let (status, error) = send(
        app.clone(),
        "POST",
        &format!(
            "/api/v1/model-profiles/{}/test",
            profile["id"].as_str().unwrap()
        ),
        "admin-secret",
        json!({"generate":true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        error["error"]["message"],
        "Generate through an application token after access preflight; Poundlock does not use the LiteLLM administrative key for inference"
    );

    let missing = json!({
        "name":"missing-management",
        "kind":"litellm",
        "base_url":"http://127.0.0.1:9/v1",
        "api_key":"sk-admin",
        "managed_identity":{"enabled":true,"max_budget":5.0,"budget_duration":"30d"}
    });
    let (status, error) = send(app, "POST", "/api/v1/endpoints", "admin-secret", missing).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        error["error"]["message"],
        "LiteLLM application identity management is unavailable; verify the administrative key and database-backed proxy configuration"
    );
}

#[tokio::test]
async fn config_example_returns_always_ready_toml() {
    let (_directory, state) = state_with_artifacts(None);
    let (status, body) = send(
        router(state),
        "GET",
        "/api/v1/nodes/config-example",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let toml = body["toml"].as_str().unwrap();
    assert!(toml.contains("always-ready"));
    assert!(toml.contains("model-manager") || toml.contains("# [runtimes.coding]"));
    assert_eq!(body["name"], "node.toml");
}

#[tokio::test]
async fn wake_test_times_out_without_a_new_heartbeat() {
    let (_directory, state) = state_with_artifacts(None);
    let token = state.store.create_enrollment_token(15, None).unwrap();
    let enrolled = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token,
            name: "sleeper".into(),
            platform: "linux".into(),
            capabilities: json!({"runtimes":["coding"]}),
        })
        .unwrap();
    state
        .store
        .configure_wol(
            &enrolled.node_id,
            Some("02:00:00:00:00:01"),
            Some("127.0.0.1:9"),
        )
        .unwrap();
    let (status, body) = send(
        router(state),
        "POST",
        &format!("/api/v1/nodes/{}/wake-test", enrolled.node_id),
        "admin-secret",
        json!({"timeout_seconds":1}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("heartbeat"), "{message}");
}

#[tokio::test]
async fn update_manifest_and_binary_use_client_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let artifact_dir = directory.path().join("macos-aarch64");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let payload = b"poundlock-node-artifact";
    std::fs::write(artifact_dir.join("poundlock"), payload).unwrap();
    let expected = crate::crypto::sha256_hex(payload);
    let (_keep, state) = state_with_artifacts(Some(directory.path().to_path_buf()));
    let token = state.store.create_enrollment_token(15, None).unwrap();
    let enrolled = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token,
            name: "mac-node".into(),
            platform: "macos".into(),
            capabilities: json!({"runtimes":["coding"],"os":"macos","arch":"aarch64"}),
        })
        .unwrap();
    let app = router(state);
    let (status, body) = send(
        app.clone(),
        "GET",
        &format!("/node/v1/nodes/{}/update", enrolled.node_id),
        &enrolled.credential,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], true);
    assert_eq!(body["sha256"], expected);
    assert_eq!(body["os"], "macos");
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/node/v1/nodes/{}/update/binary", enrolled.node_id))
                .header(AUTHORIZATION, format!("Bearer {}", enrolled.credential))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], payload);
}

#[tokio::test]
async fn a_signed_node_manifest_is_offered_only_for_the_binary_it_names() {
    let directory = tempfile::tempdir().unwrap();
    let artifact_dir = directory.path().join("macos-aarch64");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let payload = b"poundlock-node-artifact";
    std::fs::write(artifact_dir.join("poundlock"), payload).unwrap();
    let sha256 = crate::crypto::sha256_hex(payload);
    let (_keep, state) = state_with_artifacts(Some(directory.path().to_path_buf()));
    let enrolled = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "mac-node".into(),
            platform: "macos".into(),
            capabilities: json!({"runtimes":["coding"],"os":"macos","arch":"aarch64"}),
        })
        .unwrap();
    let app = router(state);
    let path = format!("/node/v1/nodes/{}/update", enrolled.node_id);
    let offer = || send(app.clone(), "GET", &path, &enrolled.credential, json!({}));
    let (_, body) = offer().await;
    assert!(body.get("signed_manifest").is_none(), "{body}");

    let manifest =
        json!({"os":"macos","arch":"aarch64","version":"9.0.0","sha256":sha256}).to_string();
    std::fs::write(artifact_dir.join("poundlock.manifest.json"), &manifest).unwrap();
    std::fs::write(
        artifact_dir.join("poundlock.manifest.sig"),
        "c2lnbmF0dXJl\n",
    )
    .unwrap();
    let (status, body) = offer().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["signed_manifest"], manifest);
    assert_eq!(body["signature"], "c2lnbmF0dXJl");

    // A manifest for another build is not relayed as if it named this one.
    let stale = json!({"os":"macos","arch":"aarch64","version":"9.0.0","sha256":"0".repeat(64)});
    std::fs::write(
        artifact_dir.join("poundlock.manifest.json"),
        stale.to_string(),
    )
    .unwrap();
    let (_, body) = offer().await;
    assert!(body.get("signed_manifest").is_none(), "{body}");
    assert!(body.get("signature").is_none(), "{body}");
}

#[tokio::test]
async fn an_uploaded_signature_is_kept_only_for_a_served_binary_of_this_version() {
    let directory = tempfile::tempdir().unwrap();
    let artifact_dir = directory.path().join("macos-aarch64");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let payload = b"poundlock-node-artifact";
    std::fs::write(artifact_dir.join("poundlock"), payload).unwrap();
    let sha256 = crate::crypto::sha256_hex(payload);
    let (_keep, state) = state_with_artifacts(Some(directory.path().to_path_buf()));
    let enrolled = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "mac-node".into(),
            platform: "macos".into(),
            capabilities: json!({"os":"macos","arch":"aarch64"}),
        })
        .unwrap();
    let app = router(state);
    let upload = |manifest: String, token: &'static str| {
        send(
            app.clone(),
            "PUT",
            "/api/v1/nodes/update-signatures",
            token,
            json!([{"manifest": manifest, "signature": "c2lnbmF0dXJl"}]),
        )
    };
    let version = env!("CARGO_PKG_VERSION");
    let manifest =
        json!({"os":"macos","arch":"aarch64","version":version,"sha256":sha256}).to_string();
    assert_eq!(
        upload(manifest.clone(), "wrong").await.0,
        StatusCode::UNAUTHORIZED
    );
    let other_build =
        json!({"os":"macos","arch":"aarch64","version":version,"sha256":"0".repeat(64)})
            .to_string();
    assert_eq!(
        upload(other_build, "admin-secret").await.0,
        StatusCode::BAD_REQUEST
    );
    let other_version =
        json!({"os":"macos","arch":"aarch64","version":"9.9.9","sha256":sha256}).to_string();
    assert_eq!(
        upload(other_version, "admin-secret").await.0,
        StatusCode::BAD_REQUEST
    );

    let (status, body) = upload(manifest.clone(), "admin-secret").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = format!("/node/v1/nodes/{}/update", enrolled.node_id);
    let (_, offer) = send(app.clone(), "GET", &path, &enrolled.credential, json!({})).await;
    assert_eq!(offer["signed_manifest"], manifest);
    assert_eq!(offer["signature"], "c2lnbmF0dXJl");
}

#[tokio::test]
async fn one_shot_secrets_are_not_returned_on_list() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state);
    let (status, issued) = send(
        app.clone(),
        "POST",
        "/api/v1/api-tokens",
        "admin-secret",
        json!({"name":"editor","permissions":["models:read"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let secret = issued["token"].as_str().unwrap().to_owned();
    assert!(!secret.is_empty());
    let (status, capabilities) = send(
        app.clone(),
        "GET",
        "/api/v1/capabilities",
        &secret,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["api_major"], 1);
    assert_eq!(
        capabilities["contract_revision"],
        crate::contract::CONTRACT_REVISION
    );
    assert!(
        capabilities["features"]
            .as_array()
            .unwrap()
            .contains(&json!(crate::contract::FEATURE_VERIFIER_TOOLCHAIN))
    );
    assert_eq!(capabilities["verifier"]["python"], "3.14");
    assert_eq!(capabilities["verifier"]["uv"], "0.12.24");
    assert!(
        capabilities["features"]
            .as_array()
            .unwrap()
            .contains(&json!("external_invoke_permission"))
    );
    let (status, listed) = send(
        app.clone(),
        "GET",
        "/api/v1/api-tokens",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed_text = listed.to_string();
    assert!(!listed_text.contains(&secret));
    let (status, enrollment) = send(
        app.clone(),
        "POST",
        "/api/v1/nodes/enrollment-tokens",
        "admin-secret",
        json!({"valid_minutes":15}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let enroll_secret = enrollment["token"].as_str().unwrap().to_owned();
    let (status, overview) = send(app, "GET", "/api/v1/overview", "admin-secret", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let overview_text = overview.to_string();
    assert!(!overview_text.contains(&enroll_secret));
    assert!(!overview_text.contains(&secret));
}

#[tokio::test]
async fn jobs_tokens_submit_files_but_cannot_manage_or_read_runner_source() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let (_, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret", json!({"name":"coding","endpoint_id":endpoint["id"],"model_id":"test","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never","data_destination":"operator_network"})).await;
    let issued = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "jobs only".into(),
            permissions: vec!["jobs:read".into(), "jobs:submit".into()],
        })
        .unwrap();
    let body = json!({"idempotency_key":"source-api", "source":{"kind":"snapshot","files":[{"path":"hello.txt","content_base64":"aGVsbG8="}]},"model_profile_id":model["id"],"harness_profile_id":"pi","prompt":"change hello"});
    let (status, job) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert!(job["project_id"].is_null());
    let (status, replay) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(job["id"], replay["id"]);
    // A model server declares one destination for all its models, so the
    // external model runs on a server of its own.
    let (_, external_server) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"external","kind":"openai","base_url":"http://127.0.0.1:9/v1",
            "api_key":"","data_destination":"external"}),
    )
    .await;
    let (_, external_model) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({
            "name":"external-coding",
            "endpoint_id":external_server["id"],
            "model_id":"external-test",
            "context_tokens":8192,
            "max_output_tokens":1024,
            "workload_class":"durable-coding-agent",
            "wake_policy":"never",
            "data_destination":"external"
        }),
    )
    .await;
    let mut external_body = body.clone();
    external_body["idempotency_key"] = json!("external-denied");
    external_body["model_profile_id"] = external_model["id"].clone();
    let (status, denied) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        external_body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["error"]["code"], "external_permission_required");
    let id = job["id"].as_str().unwrap();
    for (method, uri, payload) in [
        ("POST", format!("/api/v1/jobs/{id}/cancel"), json!({})),
        ("POST", "/api/v1/endpoints".into(), endpoint_body()),
        ("GET", "/api/v1/endpoints".into(), json!({})),
        ("GET", "/api/v1/model-profiles".into(), json!({})),
        ("GET", format!("/runner/v1/jobs/{id}/source"), json!({})),
    ] {
        assert_eq!(
            send(app.clone(), method, &uri, &issued.token, payload)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let mut traversal = body.clone();
    traversal["idempotency_key"] = json!("traversal");
    traversal["source"]["files"][0]["path"] = json!("../outside");
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/jobs",
            &issued.token,
            traversal
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    state
        .store
        .set_job_status(id, "ready", "ready", "Ready")
        .unwrap();
    state.store.claim_runner_job(None).unwrap();
    assert_eq!(
        send(
            app.clone(),
            "GET",
            &format!("/runner/v1/jobs/{id}/source"),
            "runner-secret",
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    state.store.request_cancel(id).unwrap();
    assert_eq!(
        send(
            app,
            "GET",
            &format!("/runner/v1/jobs/{id}/source"),
            "runner-secret",
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn developer_git_submission_requires_approved_host_and_keeps_credentials_private() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let (_, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret", json!({"name":"git-coding","endpoint_id":endpoint["id"],"model_id":"test","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never","data_destination":"operator_network"})).await;
    let issued = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "git-jobs".into(),
            permissions: vec!["jobs:read".into(), "jobs:submit".into()],
        })
        .unwrap();
    let mut input = json!({"idempotency_key":"git-api", "source":{"kind":"git","repository":"https://unapproved.invalid/repo.git","revision":"a".repeat(40),"credentials":{"username":"reader","token":"private-git-secret"}},"model_profile_id":model["id"],"harness_profile_id":"pi","prompt":"change it"});
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/jobs",
            &issued.token,
            input.clone()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    input["source"]["repository"] = json!("https://github.com/owner/repo.git");
    let (status, job) = send(app.clone(), "POST", "/api/v1/jobs", &issued.token, input).await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let id = job["id"].as_str().unwrap();
    let (_, events) = send(
        app,
        "GET",
        &format!("/api/v1/jobs/{id}/events"),
        &issued.token,
        json!({}),
    )
    .await;
    assert!(!events.to_string().contains("private-git-secret"));
    assert!(
        !serde_json::to_string(&state.store.runner_package(id).unwrap())
            .unwrap()
            .contains("private-git-secret")
    );
}

#[tokio::test]
async fn a_token_past_its_unfinished_job_limit_is_refused_with_a_stable_code() {
    let (_directory, mut state) = state_with_artifacts(None);
    state.config = Arc::new(AppConfig {
        max_active_jobs_per_token: 1,
        ..(*state.config).clone()
    });
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let (_, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret", json!({"name":"capped","endpoint_id":endpoint["id"],"model_id":"test","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never","data_destination":"operator_network"})).await;
    let issued = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "capped-jobs".into(),
            permissions: vec!["jobs:read".into(), "jobs:submit".into()],
        })
        .unwrap();
    let job = |key: &str| json!({"idempotency_key":key,"source":{"kind":"empty"},"model_profile_id":model["id"],"harness_profile_id":"pi","prompt":"change it"});

    let (_, options) = send(
        app.clone(),
        "GET",
        "/api/v1/job-options",
        &issued.token,
        json!({}),
    )
    .await;
    assert_eq!(options["active_jobs_per_token"], 1);
    let (status, first) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        job("first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, refused) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        job("second"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{refused}");
    assert_eq!(refused["error"]["code"], "too_many_active_jobs");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("limit of unfinished jobs (1)"),
        "{refused}"
    );

    let (status, replayed) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        &issued.token,
        job("first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["id"], first["id"]);
    // The operator is not capped.
    let (status, _) = send(app, "POST", "/api/v1/jobs", "admin-secret", job("operator")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn node_maintenance_and_removal_require_that_nodes_credential() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "installer".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let app = router(state.clone());
    for (method, path, body) in [
        ("GET", "status", json!({})),
        ("POST", "maintenance", json!({"paused":true})),
        ("POST", "revoke", json!({})),
    ] {
        let uri = format!("/node/v1/nodes/{}/{path}", node.node_id);
        for wrong in ["admin-secret", "runner-secret", "other-node"] {
            let (status, _) = send(app.clone(), method, &uri, wrong, body.clone()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
    }
    let uri = format!("/node/v1/nodes/{}/status", node.node_id);
    let seen = state
        .store
        .node(&node.node_id, 15)
        .unwrap()
        .unwrap()
        .last_seen_at;
    let (status, body) = send(app.clone(), "GET", &uri, &node.credential, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(
        state
            .store
            .node(&node.node_id, 15)
            .unwrap()
            .unwrap()
            .last_seen_at,
        seen
    );
    let revoke = format!("/node/v1/nodes/{}/revoke", node.node_id);
    assert_eq!(
        send(app.clone(), "POST", &revoke, &node.credential, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(app.clone(), "GET", &uri, &node.credential, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            app,
            "GET",
            "/api/v1/nodes/linux-installer",
            "runner-secret",
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn job_policy_rejects_unapproved_network_and_invalid_paths() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state);
    let base = json!({"idempotency_key":"policy","source":{"kind":"empty"},"model_profile_id":"fixture","harness_profile_id":"pi","prompt":"Task"});
    for policy in [
        json!({"network":"outbound"}),
        json!({"verification":{"protected_paths":["../tests"]}}),
        json!({"max_repairs":4}),
    ] {
        let mut body = base.clone();
        body["policy"] = policy;
        let (status, _) = send(app.clone(), "POST", "/api/v1/jobs", "admin-secret", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, choices) =
        send(app, "GET", "/api/v1/job-options", "admin-secret", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(choices["outbound_network_allowed"], false);
    assert_eq!(
        choices["contract_revision"],
        crate::contract::CONTRACT_REVISION
    );
    assert_eq!(choices["verifier"]["python"], "3.14");
    assert_eq!(
        choices["verifier"]["writable_exec_paths"],
        json!(["/tmp", "/work", "/home/worker"])
    );
    assert_eq!(choices["effective_budgets"][0]["max_repairs"], 0);
    // Wall time cannot be included in the generic job-options table: it is
    // only knowable once a specific job has been claimed by a runner (see
    // the submit-response and job-resource assertions below).
    assert!(choices.get("wall_seconds").is_none());
}

/// B1: the job/submit responses report `wall_seconds` and `deadline_at`
/// next to `effective_budgets`, honestly reflecting that both are unknown
/// until a runner actually claims the job and reports its configured
/// budget — never guessed from policy or defaults.
#[tokio::test]
async fn submit_and_job_responses_report_the_wall_budget_once_a_runner_claims_it() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let (_, model) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({"name":"coding","endpoint_id":endpoint["id"],"model_id":"test",
        "context_tokens":8192,"max_output_tokens":1024,
        "workload_class":"durable-coding-agent","wake_policy":"never"}),
    )
    .await;
    let body = json!({"idempotency_key":"wall-budget","source":{"kind":"empty"},
        "model_profile_id":model["id"],"harness_profile_id":"pi","prompt":"Task"});
    let (status, submitted) = send(app.clone(), "POST", "/api/v1/jobs", "admin-secret", body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(submitted.get("effective_budgets").is_some());
    assert_eq!(submitted["wall_seconds"], Value::Null);
    assert_eq!(submitted["deadline_at"], Value::Null);
    let id = submitted["id"].as_str().unwrap();
    let (status, fetched) = send(
        app.clone(),
        "GET",
        &format!("/api/v1/jobs/{id}"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["wall_seconds"], Value::Null);
    assert_eq!(fetched["deadline_at"], Value::Null);
    state
        .store
        .set_job_status(id, "ready", "ready", "ready")
        .unwrap();
    assert_eq!(
        state.store.claim_runner_job(Some(900)).unwrap().as_deref(),
        Some(id)
    );
    let (status, claimed) = send(
        app,
        "GET",
        &format!("/api/v1/jobs/{id}"),
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claimed["wall_seconds"], 900);
    assert!(claimed["deadline_at"].as_str().is_some());
}

#[tokio::test]
async fn operator_removal_preserves_identity_and_blocks_active_work() {
    let (_directory, state) = state_with_artifacts(None);
    let enrolled = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "reinstalled-linux".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let id = &enrolled.node_id;
    let app = router(state.clone());
    let uri = format!("/api/v1/nodes/{id}");
    let scoped = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "reader".into(),
            permissions: vec!["jobs:read".into()],
        })
        .unwrap();
    for token in ["", "runner-secret", &enrolled.credential, &scoped.token] {
        assert_eq!(
            send(app.clone(), "DELETE", &uri, token, json!({})).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let slot = capacity::SlotKey {
        node_id: id.clone(),
        runtime_profile: "ready".into(),
    };
    let held = state
        .occupancy
        .try_claim_interactive(slot.clone(), "before-ensure")
        .unwrap();
    assert_eq!(
        send(app.clone(), "DELETE", &uri, "admin-secret", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(
            app.clone(),
            "POST",
            &format!("/node/v1/nodes/{id}/revoke"),
            &enrolled.credential,
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            app.clone(),
            "POST",
            &format!("/node/v1/nodes/{id}/maintenance"),
            &enrolled.credential,
            json!({"paused":true})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    drop(held);
    state
        .occupancy
        .try_claim_job(&slot, "reserved-job")
        .unwrap();
    assert_eq!(
        send(app.clone(), "DELETE", &uri, "admin-secret", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    state.occupancy.release_job("reserved-job");
    let payload = json!({"lease_id":"occupied", "runtime_profile":"ready"});
    let ensure = state
        .store
        .enqueue_action(id, None, "ensure", payload.clone())
        .unwrap();
    assert_eq!(
        send(app.clone(), "DELETE", &uri, "admin-secret", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(
        state
            .store
            .authenticate_node(id, &enrolled.credential)
            .unwrap()
    );
    state.store.poll_node_action(id).unwrap();
    state
        .store
        .complete_node_action(id, &ensure.id, true, json!({}))
        .unwrap();
    let release = state
        .store
        .enqueue_action(id, None, "release", payload)
        .unwrap();
    assert_eq!(
        send(app.clone(), "DELETE", &uri, "admin-secret", json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    state.store.poll_node_action(id).unwrap();
    state
        .store
        .complete_node_action(id, &release.id, true, json!({}))
        .unwrap();
    for _ in 0..2 {
        let (status, result) = send(app.clone(), "DELETE", &uri, "admin-secret", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["removed"], true);
    }
    assert!(
        !state
            .store
            .authenticate_node(id, &enrolled.credential)
            .unwrap()
    );
    assert_eq!(state.store.node(id, 15).unwrap().unwrap().status, "revoked");
    assert!(state.store.action(&ensure.id).unwrap().is_some());
    assert_eq!(
        send(
            app,
            "DELETE",
            "/api/v1/nodes/absent",
            "admin-secret",
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let renewed = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state
                .store
                .create_enrollment_token(15, Some("reinstalled-linux"))
                .unwrap(),
            name: "reinstalled-linux".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    assert_eq!(renewed.node_id, *id);
    assert!(
        state
            .store
            .authenticate_node(id, &renewed.credential)
            .unwrap()
    );
    assert!(
        !state
            .store
            .authenticate_node(id, &enrolled.credential)
            .unwrap()
    );
}

#[tokio::test]
async fn operator_pause_blocks_work_and_requires_admin() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "desktop".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let app = router(state.clone());
    let path = format!("/api/v1/nodes/{}/maintenance", node.node_id);
    assert_eq!(
        send(app.clone(), "POST", &path, "wrong", json!({"paused":true}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let guard = state
        .occupancy
        .try_claim_interactive(
            capacity::SlotKey {
                node_id: node.node_id.clone(),
                runtime_profile: "ollama".into(),
            },
            "request",
        )
        .unwrap();
    assert_eq!(
        send(
            app.clone(),
            "POST",
            &path,
            "admin-secret",
            json!({"paused":true})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    drop(guard);
    assert_eq!(
        send(
            app.clone(),
            "POST",
            &path,
            "admin-secret",
            json!({"paused":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        state.store.node(&node.node_id, 15).unwrap().unwrap().status,
        "maintenance"
    );
    assert_eq!(
        send(app, "POST", &path, "admin-secret", json!({"paused":false}))
            .await
            .0,
        StatusCode::OK
    );
}

#[test]
fn managed_job_is_fenced_after_node_restart_and_policy_is_frozen() {
    let (_directory, state) = state_with_artifacts(None);
    let node = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "desktop".into(),
            platform: "linux".into(),
            capabilities: json!({"model_memory":{"boot_id":"first"}}),
        })
        .unwrap();
    let endpoint = state
        .store
        .create_endpoint(&serde_json::from_value(endpoint_body()).unwrap())
        .unwrap();
    let model=state.store.create_model_profile(&serde_json::from_value(json!({"name":"managed","endpoint_id":endpoint.id,"model_id":"coding:4b","context_tokens":4096,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"manual","node_id":node.node_id,"runtime_profile":"ollama","idle_seconds":0})).unwrap()).unwrap();
    let job=state.store.submit_job(&serde_json::from_value(json!({"source":{"kind":"empty"},"idempotency_key":"memory","model_profile_id":model.id,"harness_profile_id":"pi","prompt":"work"})).unwrap()).unwrap();
    state
        .store
        .patch_model_profile(
            &model.id,
            &serde_json::from_value(json!({"idle_seconds":300})).unwrap(),
        )
        .unwrap();
    let package = state.store.runner_package(&job.id).unwrap().unwrap();
    assert_eq!(package.model.idle_seconds, Some(0));
    let action = state
        .store
        .ensure_action(
            &node.node_id,
            &job.id,
            "ensure",
            json!({"lease_id":job.attempt_id,"runtime_profile":"ollama"}),
        )
        .unwrap();
    state.store.poll_node_action(&node.node_id).unwrap();
    state
        .store
        .complete_node_action(
            &node.node_id,
            &action.id,
            true,
            json!({
                "boot_id":"first",
                "resolved_model_id":"poundlock-context-fixture:latest"
            }),
        )
        .unwrap();
    assert!(runtime::managed_job_runtime_valid(&state, &package).unwrap());
    assert_eq!(
        runtime::managed_job_model_id(&state, &package).unwrap(),
        "poundlock-context-fixture:latest"
    );
    state
        .store
        .connection()
        .unwrap()
        .execute(
            "UPDATE nodes SET capabilities_json=?1 WHERE id=?2",
            rusqlite::params![
                json!({"model_memory":{"boot_id":"second"}}).to_string(),
                node.node_id
            ],
        )
        .unwrap();
    assert!(!runtime::managed_job_runtime_valid(&state, &package).unwrap());
    assert!(
        state
            .store
            .patch_model_profile(
                &model.id,
                &serde_json::from_value(json!({"idle_seconds":3601})).unwrap()
            )
            .is_err()
    );
}

#[tokio::test]
async fn setup_edits_preserve_keys_clear_idle_and_explain_invalid_values() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (status, endpoint) = send(app.clone(), "POST", "/api/v1/endpoints", "admin-secret",
        json!({"name":"edit","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":"private-fixture-key"})).await;
    assert_eq!(status, StatusCode::OK);
    let id = endpoint["id"].as_str().unwrap();
    let path = format!("/api/v1/endpoints/{id}");
    let (status, value) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"name":"renamed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["name"], "renamed");
    assert!(!value.to_string().contains("private-fixture-key"));
    assert_eq!(
        state.store.endpoint_secret(id).unwrap().unwrap().api_key,
        "private-fixture-key"
    );
    let (_, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret",
        json!({"name":"coding-edit","endpoint_id":id,"model_id":"fixture","context_tokens":8192,"max_output_tokens":4096,"workload_class":"durable-coding-agent","wake_policy":"never","idle_seconds":60})).await;
    let model_path = format!("/api/v1/model-profiles/{}", model["id"].as_str().unwrap());
    let (status, _) = send(
        app.clone(),
        "PATCH",
        &model_path,
        "admin-secret",
        json!({"context_tokens":1024}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, value) = send(
        app.clone(),
        "PATCH",
        &model_path,
        "admin-secret",
        json!({"name":"renamed-model","idle_seconds":null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["name"], "renamed-model");
    assert!(value["idle_seconds"].is_null());
    assert_eq!(value["context_tokens"], 8192);
    let (status, _) = send(app.clone(), "DELETE", &model_path, "invalid", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(app.clone(), "DELETE", &path, "admin-secret", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        app.clone(),
        "DELETE",
        &model_path,
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, value) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"api_key":""}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["api_key_set"], false);
    let (status, _) = send(app, "DELETE", &path, "admin-secret", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn enrollment_refuses_a_live_or_paused_machine_name() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let first = state
        .store
        .enroll_node(&crate::domain::EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "workstation".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let live_token = state
        .store
        .create_enrollment_token(15, Some("workstation"))
        .unwrap();
    let (status, body) = send(
        app.clone(),
        "POST",
        "/node/v1/enroll",
        "",
        json!({"token":live_token,"name":"workstation","platform":"linux","capabilities":{}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        state
            .store
            .authenticate_node(&first.node_id, &first.credential)
            .unwrap()
    );

    state
        .store
        .set_node_maintenance(&first.node_id, None, true)
        .unwrap();
    let paused_token = state
        .store
        .create_enrollment_token(15, Some("workstation"))
        .unwrap();
    let (status, body) = send(
        app.clone(),
        "POST",
        "/node/v1/enroll",
        "",
        json!({"token":paused_token,"name":"workstation","platform":"linux","capabilities":{}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    state
        .store
        .set_node_maintenance(&first.node_id, None, false)
        .unwrap();
    state
        .store
        .connection()
        .unwrap()
        .execute(
            "UPDATE nodes SET last_seen_at='2000-01-01T00:00:00Z' WHERE id=?1",
            [&first.node_id],
        )
        .unwrap();
    let (status, enrolled) = send(
        app,
        "POST",
        "/node/v1/enroll",
        "",
        json!({"token":live_token,"name":"workstation","platform":"linux","capabilities":{}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    assert_eq!(enrolled["node_id"], first.node_id);
    assert!(
        !state
            .store
            .authenticate_node(&first.node_id, &first.credential)
            .unwrap()
    );
}

#[tokio::test]
async fn model_profile_uses_accepts_legacy_input_and_rejects_conflicts() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let endpoint_id = endpoint["id"].as_str().unwrap();
    let base = json!({
        "endpoint_id": endpoint_id, "model_id": "test",
        "context_tokens": 8192, "max_output_tokens": 1024, "wake_policy": "never"
    });

    // Legacy workload_class alone still derives `uses`.
    let mut legacy = base.clone();
    legacy["name"] = json!("legacy");
    legacy["workload_class"] = json!("durable-coding-agent");
    let (status, legacy) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        legacy,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{legacy}");
    assert_eq!(legacy["uses"], json!(["durable"]));
    assert_eq!(legacy["workload_class"], "durable-coding-agent");

    // `uses` alone, with several uses, derives a primary workload_class.
    let mut multi = base.clone();
    multi["name"] = json!("multi");
    multi["uses"] = json!(["durable", "interactive"]);
    let (status, multi) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        multi,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{multi}");
    assert_eq!(multi["uses"], json!(["durable", "interactive"]));
    assert_eq!(multi["workload_class"], "interactive-coding");

    // Agreeing `uses` and `workload_class` are accepted.
    let mut agree = base.clone();
    agree["name"] = json!("agree");
    agree["uses"] = json!(["durable", "interactive"]);
    agree["workload_class"] = json!("interactive-coding");
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/model-profiles",
            "admin-secret",
            agree
        )
        .await
        .0,
        StatusCode::OK
    );

    // Disagreeing `uses` and `workload_class` are rejected.
    let mut conflict = base.clone();
    conflict["name"] = json!("conflict");
    conflict["uses"] = json!(["durable"]);
    conflict["workload_class"] = json!("interactive-coding");
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/model-profiles",
            "admin-secret",
            conflict
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // `completion` cannot combine with a coding use.
    let mut incompatible = base.clone();
    incompatible["name"] = json!("incompatible");
    incompatible["uses"] = json!(["completion", "durable"]);
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/model-profiles",
            "admin-secret",
            incompatible
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // Neither `uses` nor `workload_class` is rejected.
    let mut missing = base.clone();
    missing["name"] = json!("missing");
    assert_eq!(
        send(
            app.clone(),
            "POST",
            "/api/v1/model-profiles",
            "admin-secret",
            missing
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    // PATCH can widen `uses`, deriving a new primary workload_class.
    let path = format!("/api/v1/model-profiles/{}", legacy["id"].as_str().unwrap());
    let (status, patched) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"uses": ["durable", "interactive"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["uses"], json!(["durable", "interactive"]));
    assert_eq!(patched["workload_class"], "interactive-coding");

    // A conflicting PATCH is rejected too.
    assert_eq!(
        send(
            app.clone(),
            "PATCH",
            &path,
            "admin-secret",
            json!({"uses": ["durable"], "workload_class": "interactive-coding"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

/// A `poundlock_toolkit`-sourced capability block, as pasted from
/// `scripts/toolkit-capabilities.py`.
fn capability_block(name: &str, context_tokens: u64, reasoning: &str) -> Value {
    json!({
        "name": name,
        "status": "qualified",
        "passed": 3,
        "attempted": 3,
        "source": "poundlock_toolkit",
        "toolkit_version": "0.4.0",
        "run_ids": ["run-1", "run-2", "run-3"],
        "identity_sha256": "a".repeat(64),
        "result_sha256": "b".repeat(64),
        "measured_at": "2026-08-01T00:00:00Z",
        "measured_settings": {
            "context_tokens": context_tokens,
            "max_output_tokens": 1024,
            "reasoning": reasoning,
            "harness": "pi"
        },
        "note": "measured on orion"
    })
}

#[tokio::test]
async fn model_profile_capabilities_accept_a_valid_block_and_reject_unknown_names_and_missing_provenance()
 {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        endpoint_body(),
    )
    .await;
    let endpoint_id = endpoint["id"].as_str().unwrap();
    let (status, model) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({
            "name": "coding",
            "endpoint_id": endpoint_id,
            "model_id": "test",
            "context_tokens": 8192,
            "max_output_tokens": 1024,
            "workload_class": "durable-coding-agent",
            "wake_policy": "never"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{model}");
    let id = model["id"].as_str().unwrap();
    let path = format!("/api/v1/model-profiles/{id}");

    // A valid pasted block is accepted on PATCH.
    let (status, patched) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"capabilities": [capability_block("code_repair_verified", 8192, "model_default")]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["capabilities"].as_array().unwrap().len(), 1);
    assert_eq!(patched["capabilities"][0]["name"], "code_repair_verified");

    // An unknown capability name is rejected.
    let (status, rejected) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"capabilities": [capability_block("not_a_real_capability", 8192, "model_default")]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");

    // A poundlock_toolkit entry with no run ids is rejected.
    let mut missing_run_ids = capability_block("plan_tool_schema", 8192, "model_default");
    missing_run_ids["run_ids"] = json!([]);
    let (status, rejected) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"capabilities": [missing_run_ids]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");

    // Create also accepts a valid block.
    let (status, created) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles",
        "admin-secret",
        json!({
            "name": "coding-with-evidence",
            "endpoint_id": endpoint_id,
            "model_id": "test",
            "context_tokens": 8192,
            "max_output_tokens": 1024,
            "workload_class": "durable-coding-agent",
            "wake_policy": "never",
            "capabilities": [capability_block("sealed_investigation", 8192, "model_default")]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["capabilities"].as_array().unwrap().len(), 1);
}

/// A job belongs to the token that submitted it.
///
/// `api_token_id` was recorded at submission and never read back, so job
/// reads, patch downloads and cancels were gated on holding the permission
/// rather than on being the submitter: any developer token could read another
/// token's patches — that project's source — and cancel its work. Per-token
/// managed identities and per-token budgets already treat a token as a
/// tenant; the job routes now agree.
#[tokio::test]
async fn one_token_cannot_read_or_cancel_another_tokens_job() {
    use crate::domain::*;
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(&serde_json::from_value(endpoint_body()).unwrap())
        .unwrap();
    let model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"test", "endpoint_id":endpoint.id, "model_id":"test",
                "context_tokens":8192, "max_output_tokens":1024,
                "workload_class":"durable-coding-agent", "wake_policy":"never"
            }))
            .unwrap(),
        )
        .unwrap();
    let project = state
        .store
        .create_project(&CreateProject {
            name: "test".into(),
            roots: vec!["repo".into()],
            instruction_files: vec![],
        })
        .unwrap();
    let token = |name: &str| {
        state
            .store
            .create_api_token(&CreateApiToken {
                name: name.into(),
                permissions: vec![
                    "jobs:read".into(),
                    "jobs:submit".into(),
                    "jobs:cancel".into(),
                ],
            })
            .unwrap()
    };
    let mine = token("mine");
    let theirs = token("theirs");
    let job = state
        .store
        .submit_job_for(
            &SubmitJob {
                source: None,
                idempotency_key: "tenant".into(),
                policy: Default::default(),
                project_id: project.id,
                model_profile_id: model.id,
                harness_profile_id: "pi".into(),
                prompt: "test".into(),
            },
            Some(&mine.details.id),
            10,
        )
        .unwrap();

    let app = router(state.clone());
    let job_uri = format!("/api/v1/jobs/{}", job.id);
    let patch_uri = format!("/api/v1/jobs/{}/patches/0", job.id);
    let cancel_uri = format!("/api/v1/jobs/{}/cancel", job.id);

    // The submitter reads its own job, and the operator reads everything.
    for reader in [mine.token.as_str(), "admin-secret"] {
        assert_eq!(
            send(app.clone(), "GET", &job_uri, reader, json!({}))
                .await
                .0,
            StatusCode::OK
        );
    }

    // Another token holding every job permission sees nothing. Reported as
    // missing rather than forbidden: whether a job id exists is not its
    // business either.
    for (method, uri) in [
        ("GET", &job_uri),
        ("GET", &patch_uri),
        ("POST", &cancel_uri),
        ("GET", &format!("/api/v1/jobs/{}/events", job.id)),
    ] {
        assert_eq!(
            send(app.clone(), method, uri, &theirs.token, json!({}))
                .await
                .0,
            StatusCode::NOT_FOUND,
            "{method} {uri} must not cross the tenant boundary"
        );
    }
    assert!(!state.store.job(&job.id).unwrap().unwrap().cancel_requested);

    // Listing is scoped the same way.
    let (status, listed) = send(app.clone(), "GET", "/api/v1/jobs", &theirs.token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 0);
    let (_, ours) = send(app.clone(), "GET", "/api/v1/jobs", &mine.token, json!({})).await;
    assert_eq!(ours.as_array().unwrap().len(), 1);
    let (_, all) = send(app, "GET", "/api/v1/jobs", "admin-secret", json!({})).await;
    assert_eq!(all.as_array().unwrap().len(), 1);
}

/// Every bound a submitted policy must satisfy is published, and the
/// published number is the one that enforces.
///
/// A consumer discovered the verification-timeout range by submitting a job
/// and reading the 400. That is a fine way to learn a limit once and a poor
/// way to build against one.
#[tokio::test]
async fn job_options_publishes_the_policy_bounds_it_enforces() {
    use crate::job_policy as policy;
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (status, body) = send(
        app.clone(),
        "GET",
        "/api/v1/job-options",
        "admin-secret",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let limits = &body["policy_limits"];

    assert_eq!(limits["max_repairs"], policy::MAX_REPAIRS);
    assert_eq!(
        limits["verification_commands"],
        policy::MAX_VERIFICATION_COMMANDS
    );
    assert_eq!(limits["protected_paths"], policy::MAX_PROTECTED_PATHS);
    assert_eq!(limits["policy_bytes"], policy::MAX_POLICY_BYTES);
    let timeout = &limits["verification_timeout_seconds"];
    assert_eq!(timeout["min"], policy::MIN_VERIFICATION_TIMEOUT_SECONDS);
    assert_eq!(timeout["max"], policy::MAX_VERIFICATION_TIMEOUT_SECONDS);
    assert_eq!(
        timeout["default"],
        policy::DEFAULT_VERIFICATION_TIMEOUT_SECONDS
    );

    // Non-vacuous: a policy one second past the published maximum is refused,
    // and refused by the number that was published.
    let over = serde_json::from_value::<policy::JobPolicy>(json!({
        "verification": {"commands": [{"command": "true",
            "timeout_seconds": policy::MAX_VERIFICATION_TIMEOUT_SECONDS + 1}]}
    }))
    .unwrap();
    let refused = over.validate().unwrap_err().to_string();
    assert!(
        refused.contains(&policy::MAX_VERIFICATION_TIMEOUT_SECONDS.to_string()),
        "{refused}"
    );
    let at_limit = serde_json::from_value::<policy::JobPolicy>(json!({
        "verification": {"commands": [{"command": "true",
            "timeout_seconds": policy::MAX_VERIFICATION_TIMEOUT_SECONDS}]}
    }))
    .unwrap();
    assert!(at_limit.validate().is_ok());
}

#[tokio::test]
async fn a_disabled_profile_takes_no_new_durable_jobs() {
    let (_directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(
                json!({"name":"up","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""}),
            )
            .unwrap(),
        )
        .unwrap();
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"jobs","endpoint_id":endpoint.id,"model_id":"m","context_tokens":65536,
                "max_output_tokens":4096,"uses":["durable"],"wake_policy":"never",
                "data_destination":"operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    let gone = crate::store::retirement::RouteOutcome::Gone {
        status: 404,
        detail: "HTTP 404: upstream reported \"model not found\"".into(),
    };
    for at in [
        "2026-09-23T10:00:00Z",
        "2026-09-23T10:20:00Z",
        "2026-09-23T10:40:00Z",
    ] {
        state
            .store
            .record_route_outcome_at(&profile.id, "relay", &gone, at.into())
            .unwrap();
    }
    let app = router(state);
    let (status, body) = send(
        app.clone(),
        "POST",
        "/api/v1/jobs",
        "admin-secret",
        json!({"idempotency_key":"retired","source":{"kind":"empty"},"model_profile_id":profile.id,
               "harness_profile_id":"pi","prompt":"Task"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "model_disabled");
    let (_, choices) = send(app, "GET", "/api/v1/job-options", "admin-secret", json!({})).await;
    assert_eq!(choices["models"][0]["status"], "disabled");
}

#[tokio::test]
async fn a_model_server_declares_its_destination_once_for_its_models() {
    let (_directory, state) = state_with_artifacts(None);
    let app = router(state.clone());
    let (status, server) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"server","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":"",
            "data_destination":"operator_network"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{server}");
    assert_eq!(server["data_destination"], "operator_network");
    let (status, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret",
        json!({"name":"coding","endpoint_id":server["id"],"model_id":"fixture","context_tokens":8192,
            "max_output_tokens":1024,"uses":["durable"],"wake_policy":"never"})).await;
    assert_eq!(status, StatusCode::OK, "{model}");
    assert_eq!(model["data_destination"], "operator_network");
    assert_eq!(model["data_destination_source"], "endpoint");
    let server_path = format!("/api/v1/endpoints/{}", server["id"].as_str().unwrap());
    // Only the declaration: the server is not re-saved, its models follow.
    let (status, changed) = send(
        app.clone(),
        "PATCH",
        &server_path,
        "admin-secret",
        json!({"data_destination":"external"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["data_destination"], "external");
    let token = state
        .store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap()
        .token;
    let (_, listed) = send(app.clone(), "GET", "/v1/models", &token, json!({})).await;
    assert_eq!(
        listed["data"][0]["poundlock"]["data_destination"],
        "external"
    );
    let (status, _) = send(
        app.clone(),
        "PATCH",
        &server_path,
        "admin-secret",
        json!({"data_destination":"guessed"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A gateway's routes declare their own.
    let (status, refused) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"gateway","kind":"litellm","base_url":"http://127.0.0.1:9/v1","api_key":"",
            "data_destination":"external"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("each route")
    );
    let (status, gateway) = send(
        app,
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"gateway","kind":"litellm","base_url":"http://127.0.0.1:9/v1","api_key":""}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(gateway.get("data_destination").is_none(), "{gateway}");
}
