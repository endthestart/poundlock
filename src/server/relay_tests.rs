use super::{
    COMPLETIONS_RELAY_TIMEOUT_SECONDS, REQUEST_ID_HEADER, advertised_model, advertised_models,
    constrain_interactive_request, inject_default_reasoning_off, wait_request,
};
use crate::client::InferenceClient;
use crate::config::AppConfig;
use crate::domain::{
    CapabilityMeasurement, CreateApiToken, EnrollNode, MeasuredSettings, ModelProfile,
    ResourceCheck,
};
use crate::server::capacity::{Occupancy, WaitClass};
use crate::server::ops_tests::send;
use crate::server::{AppState, router};
use crate::store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tower::ServiceExt;

fn coding_large() -> ModelProfile {
    ModelProfile {
        id: "profile".into(),
        name: "coding-large".into(),
        aliases: vec!["coding".into()],
        endpoint_id: "endpoint".into(),
        model_id: "upstream-large".into(),
        model_label: None,
        context_tokens: 131072,
        max_output_tokens: 8192,
        uses: BTreeSet::from(["interactive".to_string()]),
        workload_class: "interactive-coding".into(),
        wake_policy: "on-demand".into(),
        node_id: Some("large-node".into()),
        runtime_profile: Some("large-runtime".into()),
        idle_seconds: None,
        data_destination: "unknown".into(),

        data_destination_source: "profile".into(),
        thinking: "model_default".into(),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
        catalog: Default::default(),
        retirement: Default::default(),
        created_at: "now".into(),
        last_check: ResourceCheck::default(),
    }
}

#[test]
fn models_advertise_the_configured_profile_context() {
    let occupancy = Occupancy::new();
    let listed = advertised_model(
        &coding_large(),
        "coding-large",
        "openai",
        "revision",
        &occupancy,
    );
    assert_eq!(listed["id"], "coding-large");
    assert_eq!(listed["context_length"], 131072);
    assert_eq!(listed["context_window"], 131072);
    assert_eq!(listed["context_tokens"], 131072);
    assert_eq!(listed["meta"]["n_ctx"], 131072);
    assert_eq!(listed["owned_by"], "poundlock");
    assert_eq!(listed["poundlock"]["n_ctx"], 131072);
    assert_eq!(listed["poundlock"]["context_tokens"], 131072);
    assert_eq!(listed["max_output_tokens"], 8192);
    assert_eq!(listed["poundlock"]["canonical_name"], "coding-large");
    assert_eq!(listed["poundlock"]["endpoint_kind"], "openai");
    assert_eq!(listed["poundlock"]["data_destination"], "unknown");
    assert_eq!(listed["busy"], false);
    assert_eq!(listed["occupancy"]["kind"], Value::Null);
    assert_eq!(listed["occupancy"]["node_id"], "large-node");
    assert_eq!(listed["occupancy"]["runtime_profile"], "large-runtime");
    assert!(listed.get("base_url").is_none());
    assert_ne!(listed["context_window"], 65536);
}

fn capability_measurement(
    name: &str,
    context_tokens: u64,
    reasoning: &str,
    measured_at: &str,
) -> CapabilityMeasurement {
    CapabilityMeasurement {
        name: name.into(),
        status: "qualified".into(),
        passed: 3,
        attempted: 3,
        source: "poundlock_toolkit".into(),
        toolkit_version: "0.4.0".into(),
        run_ids: vec!["run-1".into()],
        identity_sha256: "a".repeat(64),
        result_sha256: "b".repeat(64),
        measured_at: measured_at.into(),
        measured_settings: MeasuredSettings {
            context_tokens,
            max_output_tokens: 8192,
            reasoning: reasoning.into(),
            harness: "pi".into(),
        },
        note: String::new(),
        recorded_at: measured_at.into(),
    }
}

#[test]
fn published_capabilities_report_staleness_and_settings_match() {
    let mut profile = coding_large();
    // coding_large() is context_tokens: 131072, thinking: "model_default".
    profile.capabilities = vec![
        capability_measurement(
            "code_repair_verified",
            131072,
            "model_default",
            "2020-01-01T00:00:00Z",
        ),
        capability_measurement(
            "plan_tool_schema",
            131072,
            "model_default",
            &chrono::Utc::now().to_rfc3339(),
        ),
        capability_measurement(
            "sealed_investigation",
            65536,
            "model_default",
            &chrono::Utc::now().to_rfc3339(),
        ),
    ];
    let listed = advertised_model(
        &profile,
        "coding-large",
        "openai",
        "revision",
        &Occupancy::new(),
    );
    let capabilities = listed["poundlock"]["capabilities"].as_array().unwrap();
    assert_eq!(capabilities.len(), 3);
    let by_name = |name: &str| {
        capabilities
            .iter()
            .find(|capability| capability["name"] == name)
            .unwrap()
    };
    // Measured years ago: stale, but still at this profile's live settings.
    assert_eq!(by_name("code_repair_verified")["stale"], true);
    assert_eq!(by_name("code_repair_verified")["settings_match"], "exact");
    // Measured just now: fresh, and also at this profile's live settings.
    assert_eq!(by_name("plan_tool_schema")["stale"], false);
    assert_eq!(by_name("plan_tool_schema")["settings_match"], "exact");
    // Measured at a different context window than the profile now has.
    assert_eq!(by_name("sealed_investigation")["stale"], false);
    assert_eq!(
        by_name("sealed_investigation")["settings_match"],
        "different"
    );
    // The durable job-options row publishes the same described view. A reader
    // that saw only the stored fields could not tell a stale or
    // differently-configured measurement from a current one.
    let listed =
        crate::server::jobs::durable_job_model(&profile, "openai", "revision", &Occupancy::new());
    let options = listed["capabilities"].as_array().unwrap();
    assert_eq!(options.len(), 3);
    for capability in options {
        assert!(capability["stale"].is_boolean());
        assert!(capability["settings_match"].is_string());
    }
}

#[test]
fn aliases_are_metadata_without_duplicate_catalog_rows() {
    let listed = advertised_models(coding_large(), "openai", "revision", &Occupancy::new());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "coding-large");
    assert_eq!(listed[0]["poundlock"]["aliases"], json!(["coding"]));
}

#[test]
fn gateway_profiles_do_not_infer_data_destination() {
    let listed = advertised_model(
        &coding_large(),
        "coding-large",
        "litellm",
        "revision",
        &Occupancy::new(),
    );
    assert_eq!(listed["poundlock"]["endpoint_kind"], "litellm");
    assert_eq!(listed["poundlock"]["data_destination"], "unknown");
    assert_eq!(listed["busy"], false);
}

#[test]
fn exclusive_slot_occupancy_is_advertised_without_job_ids() {
    let occupancy = Occupancy::new();
    let profile = coding_large();
    let slot = Occupancy::exclusive_slot(&profile).unwrap();
    let _guard = occupancy.try_claim_interactive(slot, "lease-1").unwrap();
    let listed = advertised_model(&profile, "coding-large", "openai", "revision", &occupancy);
    assert_eq!(listed["busy"], true);
    assert_eq!(listed["occupancy"]["kind"], "interactive");
    assert_eq!(listed["poundlock"]["busy"], true);
    assert!(listed["occupancy"].get("job_id").is_none());
    assert!(listed["poundlock"]["occupancy"].get("lease_id").is_none());
}

#[test]
fn job_occupancy_is_shared_by_aliases_but_not_other_nodes() {
    let occupancy = Occupancy::new();
    let profile = coding_large();
    occupancy
        .try_claim_job(&Occupancy::exclusive_slot(&profile).unwrap(), "private-job")
        .unwrap();
    for row in advertised_models(profile.clone(), "litellm", "revision", &occupancy) {
        assert_eq!(row["busy"], true);
        assert_eq!(row["occupancy"]["kind"], "job");
        assert!(!row.to_string().contains("private-job"));
    }
    let mut independent = profile;
    independent.node_id = Some("another-node".into());
    assert_eq!(
        advertised_model(&independent, "other", "openai", "revision", &occupancy)["busy"],
        false
    );
    occupancy.release_job("private-job");
    assert!(occupancy.snapshot().is_empty());
}

#[tokio::test]
async fn model_label_is_display_only_persists_and_clears_on_route_changes() {
    let (directory, state) = relay_state(30);
    let app = router(state.clone());
    let token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "catalog-label".into(),
            permissions: vec!["models:read".into(), "jobs:read".into()],
        })
        .unwrap()
        .token;
    let (_, endpoint) = send(
        app.clone(),
        "POST",
        "/api/v1/endpoints",
        "admin-secret",
        json!({"name":"server","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""}),
    )
    .await;
    let (_, profile) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret",
        json!({"name":"durable","endpoint_id":endpoint["id"],"model_id":"routing-alias","model_label":"Example 4B Q4","context_tokens":8192,"max_output_tokens":1024,"workload_class":"durable-coding-agent","wake_policy":"never"})).await;
    let path = format!("/api/v1/model-profiles/{}", profile["id"].as_str().unwrap());
    let (_, listed) = send(app.clone(), "GET", "/v1/models", &token, json!({})).await;
    let (_, options) = send(app.clone(), "GET", "/api/v1/job-options", &token, json!({})).await;
    assert_eq!(
        listed["data"][0]["poundlock"]["model_label"],
        "Example 4B Q4"
    );
    assert_eq!(
        listed["data"][0]["poundlock"]["upstream_model"],
        "routing-alias"
    );
    assert_eq!(options["models"][0]["model_label"], "Example 4B Q4");
    assert_eq!(options["models"][0]["model_id"], "routing-alias");
    let reopened = Store::open(
        &directory.path().join("poundlock.sqlite3"),
        state.config.secrets.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .model_profile(profile["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .model_label
            .as_deref(),
        Some("Example 4B Q4")
    );
    let (_, renamed) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"name":"renamed"}),
    )
    .await;
    assert_eq!(renamed["model_label"], "Example 4B Q4");
    assert_eq!(renamed["id"], profile["id"]);
    let (_, changed) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"model_id":"new-route"}),
    )
    .await;
    assert_eq!(changed["model_label"], Value::Null);
    for label in [json!(""), json!("line\nnext"), json!("x".repeat(257))] {
        assert_eq!(
            send(
                app.clone(),
                "PATCH",
                &path,
                "admin-secret",
                json!({"model_label":label})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"model_label":"New label"}),
    )
    .await;
    let (_, cleared) = send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"model_label":null}),
    )
    .await;
    assert_eq!(cleared["model_label"], Value::Null);
    send(
        app.clone(),
        "PATCH",
        &path,
        "admin-secret",
        json!({"model_label":"New label"}),
    )
    .await;
    let ep_path = format!("/api/v1/endpoints/{}", endpoint["id"].as_str().unwrap());
    send(
        app.clone(),
        "PATCH",
        &ep_path,
        "admin-secret",
        json!({"base_url":"http://127.0.0.1:10/v1"}),
    )
    .await;
    assert_eq!(
        state
            .store
            .model_profile(profile["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .model_label,
        None
    );
}

#[tokio::test]
async fn destination_is_declared_independently_of_endpoint_kind_and_persists() {
    let (directory, state) = relay_state(30);
    let app = router(state.clone());
    let token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "catalog".into(),
            permissions: vec!["models:read".into(), "jobs:read".into()],
        })
        .unwrap()
        .token;
    for kind in ["openai", "litellm"] {
        for destination in ["unknown", "operator_network", "external"] {
            let name = format!("{kind}-{destination}");
            // A gateway route declares its own destination; a model server
            // declares one for every model on it, here through its first.
            let (status, endpoint) = send(app.clone(), "POST", "/api/v1/endpoints", "admin-secret",
                json!({"name":name,"kind":kind,"base_url":"http://127.0.0.1:9/v1","api_key":"private-endpoint-key"})).await;
            assert_eq!(status, StatusCode::OK);
            let (status, model) = send(app.clone(), "POST", "/api/v1/model-profiles", "admin-secret",
                json!({"name":name,"aliases":[format!("{name}-alias")],"endpoint_id":endpoint["id"],
                    "model_id":"upstream","context_tokens":8192,"max_output_tokens":1024,
                    "workload_class":"durable-coding-agent","wake_policy":"never","data_destination":destination})).await;
            assert_eq!(status, StatusCode::OK);
            let path = format!("/api/v1/model-profiles/{}", model["id"].as_str().unwrap());
            assert_eq!(
                send(
                    app.clone(),
                    "PATCH",
                    &path,
                    &token,
                    json!({"data_destination":"external"})
                )
                .await
                .0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                send(
                    app.clone(),
                    "PATCH",
                    &path,
                    "admin-secret",
                    json!({"data_destination":"guessed"})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
    }
    let (_, listed) = send(app.clone(), "GET", "/v1/models", &token, json!({})).await;
    let (_, options) = send(app.clone(), "GET", "/api/v1/job-options", &token, json!({})).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 6);
    assert_eq!(options["models"].as_array().unwrap().len(), 6);
    for row in listed["data"].as_array().unwrap() {
        let meta = &row["poundlock"];
        let canonical = meta["canonical_name"].as_str().unwrap();
        let job = options["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == canonical)
            .unwrap();
        assert_eq!(meta["data_destination"], job["data_destination"]);
        assert_eq!(meta["endpoint_kind"], job["endpoint_kind"]);
        assert!(canonical.ends_with(meta["data_destination"].as_str().unwrap()));
        assert!(meta.get("leaves_this_computer").is_none());
        assert!(job.get("leaves_this_computer").is_none());
    }
    for body in [&listed, &options] {
        assert!(!body.to_string().contains("private-endpoint-key"));
        assert!(!body.to_string().contains("127.0.0.1"));
    }
    let reopened = Store::open(
        &directory.path().join("poundlock.sqlite3"),
        state.config.secrets.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened
            .model_profile_by_name("litellm-operator_network-alias")
            .unwrap()
            .unwrap()
            .data_destination,
        "operator_network"
    );
    let only_models = state
        .store
        .create_api_token(&CreateApiToken {
            name: "models only".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap();
    assert_eq!(
        send(
            app.clone(),
            "GET",
            "/api/v1/job-options",
            &only_models.token,
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let only_jobs = state
        .store
        .create_api_token(&CreateApiToken {
            name: "jobs only".into(),
            permissions: vec!["jobs:read".into()],
        })
        .unwrap();
    assert_eq!(
        send(app, "GET", "/v1/models", &only_jobs.token, json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn interactive_requests_cannot_override_destination_or_output_budget() {
    let profile = coding_large();
    let mut body = json!({
        "model": "coding",
        "messages": [{"role":"user","content":"hi"}],
        "n": 2,
        "api_base": "http://evil.example/v1",
        "max_tokens": 99999
    });
    assert!(constrain_interactive_request(&mut body, &profile).is_err());

    let mut body = json!({
        "model": "coding",
        "messages": [{"role":"user","content":"hi"}],
        "n": 1,
        "api_key": "stolen",
        "api_base": "http://evil.example/v1",
        "custom_llm_provider": "openai"
    });
    constrain_interactive_request(&mut body, &profile).unwrap();
    assert_eq!(body["n"], 1);
    assert_eq!(body["max_tokens"], 8192);
    assert!(body.get("api_key").is_none());
    assert!(body.get("api_base").is_none());
    assert!(body.get("custom_llm_provider").is_none());
}

#[test]
fn reasoning_off_injection_never_overrides_an_explicit_client_signal() {
    let mut body = json!({"model": "x", "messages": []});
    inject_default_reasoning_off(&mut body);
    assert_eq!(body["reasoning_effort"], "none");

    let mut body = json!({"model": "x", "reasoning_effort": "high"});
    inject_default_reasoning_off(&mut body);
    assert_eq!(body["reasoning_effort"], "high");

    let mut body = json!({"model": "x", "reasoning": {"effort": "high"}});
    inject_default_reasoning_off(&mut body);
    assert!(body.get("reasoning_effort").is_none());

    let mut body = json!({"model": "x", "chat_template_kwargs": {"enable_thinking": true}});
    inject_default_reasoning_off(&mut body);
    assert!(body.get("reasoning_effort").is_none());
}

fn relay_state(timeout: u64) -> (tempfile::TempDir, AppState) {
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
        external_timeout_seconds: timeout,
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
        inference: InferenceClient::new(timeout).unwrap(),
        occupancy: Occupancy::new(),
        runners: Default::default(),
    };
    (directory, state)
}

fn chat_ok() -> Value {
    json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]})
}

async fn configured_relay(
    timeout: u64,
    upstream: &str,
    exclusive: bool,
) -> (tempfile::TempDir, axum::Router, String, String) {
    let (directory, state) = relay_state(timeout);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":upstream,"api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let mut node_id = None;
    let mut runtime_profile: Option<String> = None;
    if exclusive {
        let enrolled = state
            .store
            .enroll_node(&EnrollNode {
                token: state.store.create_enrollment_token(15, None).unwrap(),
                name: "gpu".into(),
                platform: "linux".into(),
                capabilities: json!({}),
            })
            .unwrap();
        node_id = Some(enrolled.node_id);
        runtime_profile = Some("coding".into());
    }
    let model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class": if exclusive { "interactive-coding" } else { "fast-code-completion" },
                "wake_policy":"never",
                "data_destination":"operator_network",
                "node_id": node_id,
                "runtime_profile": runtime_profile
            }))
            .unwrap(),
        )
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into(), "inference:invoke".into()],
        })
        .unwrap();
    (directory, router(state), issued.token, model.name)
}

#[tokio::test]
async fn application_tokens_see_published_profiles_and_slot_busy_without_operator_routes() {
    let (_directory, state) = relay_state(30);
    let openai = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"local","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let gateway = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"gateway","kind":"litellm","base_url":"http://127.0.0.1:8/v1","api_key":"secret-key"
            }))
            .unwrap(),
        )
        .unwrap();
    let enrolled = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "gpu".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    let local = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding-local",
                "aliases":["coding"],
                "endpoint_id":openai.id,
                "model_id":"qwen-local",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"interactive-coding",
                "wake_policy":"never",
                "node_id": enrolled.node_id,
                "runtime_profile": "coding"
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding-frontier",
                "endpoint_id":gateway.id,
                "model_id":"claude-sonnet",
                "context_tokens":200000,
                "max_output_tokens":8192,
                "workload_class":"interactive-coding",
                "wake_policy":"never",
                "data_destination":"external"
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"durable-local",
                "endpoint_id":openai.id,
                "model_id":"qwen-local",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"durable-coding-agent",
                "wake_policy":"never",
                "node_id": enrolled.node_id,
                "runtime_profile": "coding"
            }))
            .unwrap(),
        )
        .unwrap();
    let slot = Occupancy::exclusive_slot(&local).unwrap();
    let _guard = state
        .occupancy
        .try_claim_interactive(slot, "lease-1")
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec![
                "models:read".into(),
                "inference:invoke".into(),
                "jobs:read".into(),
                "jobs:submit".into(),
                "jobs:cancel".into(),
            ],
        })
        .unwrap();
    let app = router(state.clone());
    let (status, body) = send(app.clone(), "GET", "/v1/models", &issued.token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let listed_text = body.to_string();
    assert!(!listed_text.contains("127.0.0.1"));
    assert!(!listed_text.contains("secret-key"));
    let listed = body["data"].as_array().unwrap();
    let local_row = listed
        .iter()
        .find(|row| row["id"] == "coding-local")
        .unwrap();
    assert_eq!(local_row["poundlock"]["endpoint_kind"], "openai");
    assert_eq!(local_row["poundlock"]["data_destination"], "unknown");
    assert_eq!(local_row["poundlock"]["availability"], "blocked");
    assert_eq!(
        local_row["poundlock"]["blocked_reason"],
        "data_destination_confirmation_required"
    );
    assert_eq!(local_row["busy"], true);
    assert_eq!(local_row["occupancy"]["kind"], "interactive");
    assert_eq!(local_row["occupancy"]["node_id"], enrolled.node_id);
    assert_eq!(local_row["occupancy"]["runtime_profile"], "coding");
    assert!(local_row["occupancy"].get("job_id").is_none());
    // Two profiles on one machine's runtime publish the same slot, which is
    // how a client knows they run one at a time; a hosted profile has none.
    let local_slot = format!("{}/coding", enrolled.node_id);
    assert_eq!(local_row["occupancy"]["slot"], local_slot.as_str());
    assert!(listed.iter().all(|row| row["id"] != "coding"));
    assert_eq!(local_row["poundlock"]["aliases"], json!(["coding"]));
    let frontier = listed
        .iter()
        .find(|row| row["id"] == "coding-frontier")
        .unwrap();
    assert_eq!(frontier["poundlock"]["endpoint_kind"], "litellm");
    assert_eq!(frontier["poundlock"]["data_destination"], "external");
    assert_eq!(frontier["poundlock"]["connection_mode"], "gateway_only");
    assert_eq!(frontier["poundlock"]["availability"], "blocked");
    assert_eq!(
        frontier["poundlock"]["blocked_reason"],
        "external_permission_required"
    );
    assert_eq!(
        frontier["poundlock"]["routing_revision"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(frontier["busy"], false);
    assert_eq!(frontier["occupancy"]["kind"], Value::Null);
    assert_eq!(frontier["occupancy"]["slot"], Value::Null);
    let durable = listed
        .iter()
        .find(|row| row["id"] == "durable-local")
        .unwrap();
    assert_eq!(durable["busy"], true);
    assert_eq!(durable["occupancy"]["kind"], "interactive");
    assert_eq!(durable["occupancy"]["slot"], local_slot.as_str());
    let (status, error) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"coding-frontier","messages":[{"role":"user","content":"plan"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "external_permission_required");
    let (status, preflight) = send(
        app.clone(),
        "POST",
        "/api/v1/access/preflight",
        &issued.token,
        json!({"model":"coding-frontier"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preflight["profile_name"], "coding-frontier");
    assert_eq!(preflight["operation"], "inference");
    assert_eq!(preflight["connection_mode"], "gateway_only");
    assert_eq!(preflight["availability"], "blocked");
    assert_eq!(preflight["blocked_reason"], "external_permission_required");
    let job_token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "job preflight".into(),
            permissions: vec![
                "models:read".into(),
                "jobs:submit".into(),
                "external:invoke".into(),
            ],
        })
        .unwrap();
    let (status, job_preflight) = send(
        app.clone(),
        "POST",
        "/api/v1/access/preflight",
        &job_token.token,
        json!({"model":"coding-frontier","operation":"job"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(job_preflight["operation"], "job");
    assert_eq!(job_preflight["availability"], "available");
    assert_eq!(job_preflight["runner_status"], "not_seen");
    assert_eq!(job_preflight["warning_reason"], "runner_not_connected");
    let (status, invalid) = send(
        app.clone(),
        "POST",
        "/api/v1/access/preflight",
        &job_token.token,
        json!({"model":"coding-frontier","operation":"deploy"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    for uri in [
        "/api/v1/endpoints",
        "/api/v1/model-profiles",
        "/api/v1/overview",
    ] {
        assert_eq!(
            send(app.clone(), "GET", uri, &issued.token, json!({}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, options) = send(app, "GET", "/api/v1/job-options", &issued.token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let models = options["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["name"], "durable-local");
    assert_eq!(models[0]["endpoint_kind"], "openai");
    assert_eq!(models[0]["data_destination"], "unknown");
    assert_eq!(models[0]["availability"], "blocked");
    assert_eq!(
        models[0]["blocked_reason"],
        "data_destination_confirmation_required"
    );
    assert_eq!(models[0]["busy"], true);
    assert_eq!(models[0]["occupancy"]["kind"], "interactive");
    let options_text = options.to_string();
    assert!(!options_text.contains("coding-frontier"));
    assert!(!options_text.contains("secret-key"));
    assert!(!options_text.contains("127.0.0.1:8"));
}

async fn relay_request(
    app: axum::Router,
    token: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Result<bytes::Bytes, String>) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let collected = response.into_body().collect().await;
    (
        status,
        collected
            .map(|body| body.to_bytes())
            .map_err(|error| error.to_string()),
    )
}

#[tokio::test]
async fn chat_and_fim_succeed_for_ordinary_client_parameters() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let seen = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post({
                    let seen = seen.clone();
                    move |axum::Json(body): axum::Json<Value>| {
                        let seen = seen.clone();
                        async move {
                            seen.fetch_add(1, Ordering::SeqCst);
                            assert!(body.get("api_base").is_none());
                            assert!(body.get("n").is_none() || body["n"] == 1);
                            assert_eq!(body["temperature"], 0.2);
                            assert!(body.get("tools").is_some());
                            axum::Json(chat_ok())
                        }
                    }
                }),
            )
            .route(
                "/v1/completions",
                axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body["model"], "upstream");
                    axum::Json(json!({"choices":[{"text":"return 1;","finish_reason":"stop"}]}))
                }),
            );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, app, token, _name) =
        configured_relay(2, &format!("http://{addr}/v1"), false).await;
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        json!({
            "model":"coding",
            "messages":[{"role":"user","content":"hi"}],
            "n":1,
            "temperature":0.2,
            "tools":[{"type":"function","function":{"name":"echo"}}],
            "api_base":"http://evil.example/v1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    let (status, body) = send(
        app,
        "POST",
        "/v1/completions",
        &token,
        json!({"model":"coding","prompt":"fn x(){","max_tokens":16}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["text"], "return 1;");
}

#[tokio::test]
async fn litellm_relay_replaces_caller_identity_with_trusted_attribution() {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post({
            let captured = captured.clone();
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let captured = captured.clone();
                async move {
                    let authorization = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    *captured.lock().unwrap() = Some(json!({
                        "authorization": authorization,
                        "body": body,
                    }));
                    axum::Json(chat_ok())
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (_directory, state) = relay_state(2);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"gateway",
                "kind":"litellm",
                "base_url":format!("http://{addr}/v1"),
                "api_key":"gateway-master"
            }))
            .unwrap(),
        )
        .unwrap();
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"frontier-plan",
                "endpoint_id":endpoint.id,
                "model_id":"provider/plan",
                "context_tokens":16384,
                "max_output_tokens":2048,
                "workload_class":"interactive-coding",
                "wake_policy":"never",
                "data_destination":"external"
            }))
            .unwrap(),
        )
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "dispatch".into(),
            permissions: vec!["inference:invoke".into(), "external:invoke".into()],
        })
        .unwrap();
    let expected_revision = state.store.routing_revision(&profile).unwrap();
    let (status, response) = send(
        router(state),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({
            "model":"frontier-plan",
            "messages":[{"role":"user","content":"plan"}],
            "user":"caller-controlled",
            "metadata":{
                "poundlock_application_token_id":"caller-controlled",
                "poundlock_request_id":"caller-controlled",
                "note":"preserved"
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");

    let captured = captured.lock().unwrap().clone().unwrap();
    assert_eq!(captured["authorization"], "Bearer gateway-master");
    assert_eq!(captured["body"]["model"], "provider/plan");
    assert_eq!(
        captured["body"]["user"],
        format!("poundlock:application-token:{}", issued.details.id)
    );
    assert_eq!(
        captured["body"]["metadata"]["poundlock_application_token_id"],
        issued.details.id
    );
    assert_eq!(
        captured["body"]["metadata"]["poundlock_profile_id"],
        profile.id
    );
    assert_eq!(
        captured["body"]["metadata"]["poundlock_routing_revision"],
        expected_revision
    );
    assert_eq!(captured["body"]["metadata"]["note"], "preserved");
    let serialized = captured.to_string();
    assert!(!serialized.contains(&issued.token));
    assert!(!serialized.contains("caller-controlled"));
}

#[tokio::test]
async fn interactive_http_rejects_multiple_completions() {
    let (_directory, app, token, _name) = configured_relay(2, "http://127.0.0.1:9/v1", false).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"n":2}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("one completion"),
        "{body}"
    );
}

#[tokio::test]
async fn stall_before_headers_returns_a_timeout_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            held.push(stream);
        }
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), false).await;
    let started = Instant::now();
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body:?}");
    assert_eq!(body["error"]["code"], "upstream_timeout");
}

#[tokio::test]
async fn stall_after_headers_fails_the_stream_and_frees_the_slot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let hits = hits.clone();
        async move {
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post({
                    let hits = hits.clone();
                    move || {
                        let hits = hits.clone();
                        async move {
                            let n = hits.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                axum::http::Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(Body::from_stream(async_stream::stream! {
                                        yield Ok::<_, std::io::Error>(
                                            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_string(),
                                        );
                                        std::future::pending::<()>().await;
                                    }))
                                    .unwrap()
                            } else {
                                axum::http::Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Body::from(chat_ok().to_string()))
                                    .unwrap()
                            }
                        }
                    }
                }),
            );
            axum::serve(listener, app).await.ok();
        }
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), true).await;
    let started = Instant::now();
    let body = json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"stream":true});
    let (status, collected) =
        relay_request(app.clone(), &token, "/v1/chat/completions", body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(collected.is_err(), "{collected:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({
            "model":"coding","messages":[{"role":"user","content":"hi"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn drip_fed_chunks_are_allowed_by_the_idle_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(async_stream::stream! {
                        for _ in 0..3 {
                            yield Ok::<_, std::io::Error>("data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n".to_string());
                            tokio::time::sleep(Duration::from_millis(400)).await;
                        }
                    }))
                    .unwrap()
            }),
        );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), false).await;
    let started = Instant::now();
    let (status, collected) = relay_request(
        app,
        &token,
        "/v1/chat/completions",
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"stream":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let collected = collected.expect("stream should complete successfully");
    assert!(collected.contains(&b'.'), "{collected:?}");
    let elapsed = started.elapsed();
    assert!(elapsed > Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[tokio::test]
async fn non_streaming_chat_stall_fails_before_a_complete_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|| async {
                axum::http::Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from_stream(async_stream::stream! {
                        std::future::pending::<()>().await;
                        yield Ok::<_, std::io::Error>(chat_ok().to_string());
                    }))
                    .unwrap()
            }),
        );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), false).await;
    let (status, collected) = relay_request(
        app,
        &token,
        "/v1/chat/completions",
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(collected.is_err(), "{collected:?}");
}

#[tokio::test]
async fn client_disconnect_releases_occupancy_within_the_bound() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let hits = hits.clone();
        async move {
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post({
                    let hits = hits.clone();
                    move || {
                        let hits = hits.clone();
                        async move {
                            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                                axum::http::Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(Body::from_stream(async_stream::stream! {
                                        yield Ok::<_, std::io::Error>("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_string());
                                        std::future::pending::<()>().await;
                                    }))
                                    .unwrap()
                            } else {
                                axum::http::Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Body::from(chat_ok().to_string()))
                                    .unwrap()
                            }
                        }
                    }
                }),
            );
            axum::serve(listener, app).await.ok();
        }
    });
    let (_directory, app, token, _name) =
        configured_relay(30, &format!("http://{addr}/v1"), true).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
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
    let _ = tokio::time::timeout(Duration::from_millis(200), response.into_body().collect()).await;
    let started = Instant::now();
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn fim_keeps_a_shorter_inference_budget_than_chat() {
    assert_eq!(COMPLETIONS_RELAY_TIMEOUT_SECONDS, 30);
}

/// Serve one scripted upstream response, then plain successes, so each test
/// can prove the exclusive slot is released after the scripted outcome.
async fn scripted_upstream(
    first: impl Fn() -> axum::http::Response<Body> + Clone + Send + Sync + 'static,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let hits = hits.clone();
            let first = first.clone();
            async move {
                if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    first()
                } else {
                    axum::http::Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(chat_ok().to_string()))
                        .unwrap()
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    format!("http://{addr}/v1")
}

async fn assert_slot_released(app: axum::Router, token: &str) {
    let started = Instant::now();
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        token,
        json!({"model":"coding","messages":[{"role":"user","content":"again"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "slot was not released: {body}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn streaming_completion_relays_every_chunk_with_a_request_id() {
    let upstream = scripted_upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                for token in ["a", "b", "c"] {
                    yield Ok::<_, std::io::Error>(format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{token}\"}}}}]}}\n\n"
                    ));
                }
                yield Ok("data: [DONE]\n\n".to_string());
            }))
            .unwrap()
    })
    .await;
    let (_directory, app, token, _name) = configured_relay(2, &upstream, true).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
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
    assert_eq!(
        response.headers()["content-type"],
        "text/event-stream",
        "streaming content type must be preserved"
    );
    let request_id = response.headers()[REQUEST_ID_HEADER]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(uuid::Uuid::parse_str(&request_id).is_ok(), "{request_id}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    for expected in ["\"a\"", "\"b\"", "\"c\"", "[DONE]"] {
        assert!(text.contains(expected), "{text}");
    }
    assert_slot_released(app, &token).await;
}

#[tokio::test]
async fn upstream_connection_failure_is_a_bad_gateway_error() {
    let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = unused.local_addr().unwrap();
    drop(unused);
    let (_directory, app, token, _name) =
        configured_relay(2, &format!("http://{addr}/v1"), true).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"coding","messages":[{"role":"user","content":"hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().contains_key(REQUEST_ID_HEADER));
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["code"], "upstream_connection_failed");
    // The exclusive slot must not stay claimed after a failed connection.
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "upstream_connection_failed");
}

#[tokio::test]
async fn upstream_error_status_is_relayed_and_frees_the_slot() {
    let upstream = scripted_upstream(|| {
        axum::http::Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"error":{"message":"model crashed","code":"upstream_boom"}}).to_string(),
            ))
            .unwrap()
    })
    .await;
    let (_directory, app, token, _name) = configured_relay(2, &upstream, true).await;
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "upstream_boom");
    assert_slot_released(app, &token).await;
}

#[tokio::test]
async fn upstream_abort_mid_stream_fails_the_client_stream_and_frees_the_slot() {
    let upstream = scripted_upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_string(),
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                yield Err(std::io::Error::other("upstream worker crashed"));
            }))
            .unwrap()
    })
    .await;
    // A long idle budget proves the failure comes from the upstream abort,
    // not from the idle timeout.
    let (_directory, app, token, _name) = configured_relay(30, &upstream, true).await;
    let started = Instant::now();
    let (status, collected) = relay_request(
        app.clone(),
        &token,
        "/v1/chat/completions",
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"stream":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(collected.is_err(), "{collected:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_slot_released(app, &token).await;
}

#[tokio::test]
async fn paused_runtime_node_reports_a_distinct_admission_error() {
    let (_directory, state) = relay_state(2);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let node_id = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "gpu".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap()
        .node_id;
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"interactive-coding",
                "wake_policy":"manual",
                "data_destination":"operator_network",
                "node_id": node_id,
                "runtime_profile": "coding"
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .set_node_maintenance(&node_id, None, true)
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into(), "inference:invoke".into()],
        })
        .unwrap();
    let (status, body) = send(
        router(state),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "runtime_node_unavailable");
}

#[tokio::test]
async fn preflight_distinguishes_capacity_runtime_and_runner_state() {
    let (_directory, state) = relay_state(2);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":"http://127.0.0.1:9/v1","api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let node_id = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "gpu".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap()
        .node_id;
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"coding",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"interactive-coding",
                "wake_policy":"manual",
                "data_destination":"operator_network",
                "node_id": node_id,
                "runtime_profile": "coding"
            }))
            .unwrap(),
        )
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec![
                "models:read".into(),
                "inference:invoke".into(),
                "jobs:submit".into(),
            ],
        })
        .unwrap();
    let app = router(state.clone());
    let preflight = |operation: &'static str| {
        let app = app.clone();
        let token = issued.token.clone();
        async move {
            let (status, body) = send(
                app,
                "POST",
                "/api/v1/access/preflight",
                &token,
                json!({"model":"coding","operation":operation}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body
        }
    };

    let ready = preflight("inference").await;
    assert_eq!(ready["availability"], "available", "{ready}");
    assert!(ready.get("runner_status").is_none());

    let slot = Occupancy::exclusive_slot(&profile).unwrap();
    let guard = state
        .occupancy
        .try_claim_interactive(slot, "other-request")
        .unwrap();
    // An occupied slot is additive: contract-3 clients must still see a ready
    // preflight because the relay answers 409 and the client retries.
    let busy = preflight("inference").await;
    assert_eq!(busy["availability"], "available", "{busy}");
    assert_eq!(busy["capacity"], "in_use");
    assert_eq!(busy["capacity_reason"], "capacity_in_use");
    assert_eq!(busy["blocked_reason"], Value::Null);

    // Durable jobs queue, so slot occupancy is not reported for them.
    state.runners.record();
    let job = preflight("job").await;
    assert_eq!(job["availability"], "available", "{job}");
    assert!(job.get("capacity").is_none(), "{job}");
    assert_eq!(job["runner_status"], "connected");
    assert_eq!(job["warning_reason"], Value::Null);
    drop(guard);

    state
        .store
        .set_node_maintenance(&node_id, None, true)
        .unwrap();
    let paused = preflight("inference").await;
    assert_eq!(paused["availability"], "blocked");
    assert_eq!(paused["blocked_reason"], "runtime_node_unavailable");
}

#[tokio::test]
async fn multi_use_profile_lists_for_durable_jobs_and_still_relays_interactively() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(
                |axum::Json(_): axum::Json<Value>| async move { axum::Json(chat_ok()) },
            ),
        );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, state) = relay_state(2);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":format!("http://{addr}/v1"),"api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"both",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "uses":["durable","interactive"],
                "wake_policy":"never",
                "data_destination":"operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(model.workload_class, "interactive-coding");
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec![
                "models:read".into(),
                "inference:invoke".into(),
                "jobs:read".into(),
                "jobs:submit".into(),
            ],
        })
        .unwrap();
    let app = router(state.clone());
    let (status, options) = send(
        app.clone(),
        "GET",
        "/api/v1/job-options",
        &issued.token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["models"].as_array().unwrap().len(), 1);
    assert_eq!(options["models"][0]["id"], model.id);
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"both","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
}

#[tokio::test]
async fn thinking_off_profiles_get_reasoning_effort_injected_only_for_absent_chat_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen_chat: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_completions: Arc<std::sync::Mutex<Vec<Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen_chat = seen_chat.clone();
        let seen_completions = seen_completions.clone();
        tokio::spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/v1/chat/completions",
                    axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                        let seen_chat = seen_chat.clone();
                        async move {
                            seen_chat.lock().unwrap().push(body);
                            axum::Json(chat_ok())
                        }
                    }),
                )
                .route(
                    "/v1/completions",
                    axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                        let seen_completions = seen_completions.clone();
                        async move {
                            seen_completions.lock().unwrap().push(body);
                            axum::Json(json!({"choices":[{"text":"ok","finish_reason":"stop"}]}))
                        }
                    }),
                );
            axum::serve(listener, app).await.ok();
        });
    }
    let (_directory, state) = relay_state(2);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":format!("http://{addr}/v1"),"api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let off_model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"thinking-off",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"durable-coding-agent",
                "wake_policy":"never",
                "data_destination":"operator_network",
                "thinking":"off"
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(off_model.thinking, "off");
    let default_model = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name":"thinking-default",
                "endpoint_id":endpoint.id,
                "model_id":"upstream",
                "context_tokens":8192,
                "max_output_tokens":1024,
                "workload_class":"durable-coding-agent",
                "wake_policy":"never",
                "data_destination":"operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    assert_eq!(default_model.thinking, "model_default");
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into(), "inference:invoke".into()],
        })
        .unwrap();
    let app = router(state.clone());

    // Absent reasoning field on an `off` profile: the relay injects "none".
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"thinking-off","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An explicit reasoning_effort from the client is never overridden.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"thinking-off","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Nor is an explicit `reasoning` object.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"thinking-off","messages":[{"role":"user","content":"hi"}],"reasoning":{"effort":"high"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A model_default profile never gets anything injected.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"thinking-default","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Nor a chat-template choice. This case was unreachable for a day: the
    // allowlist refused `chat_template_kwargs` before this relay could honour
    // it, and a unit test calling the injection helper directly hid that. So
    // it goes through the handler.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &issued.token,
        json!({"model":"thinking-off","messages":[{"role":"user","content":"hi"}],
               "chat_template_kwargs":{"enable_thinking":true}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // FIM completions are never touched, even for an `off` profile.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/completions",
        &issued.token,
        json!({"model":"thinking-off","prompt":"fn x(){"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let chats = seen_chat.lock().unwrap();
    assert_eq!(chats.len(), 5);
    assert_eq!(chats[0]["reasoning_effort"], "none");
    assert_eq!(chats[1]["reasoning_effort"], "high");
    assert!(chats[2].get("reasoning_effort").is_none());
    assert_eq!(chats[2]["reasoning"]["effort"], "high");
    assert!(chats[3].get("reasoning_effort").is_none());
    assert!(chats[4].get("reasoning_effort").is_none());
    assert_eq!(chats[4]["chat_template_kwargs"]["enable_thinking"], true);

    let completions = seen_completions.lock().unwrap();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].get("reasoning_effort").is_none());
}

/// The relay decides where a user's conversation goes, so it states what it
/// accepts rather than what it happens to know is dangerous.
///
/// It used to strip seven known override keys and pass everything else
/// through — correct only until LiteLLM adds an eighth, which it does
/// routinely. The job inference proxy on this same controller already answers
/// the question with an allowlist.
#[tokio::test]
async fn the_relay_accepts_only_options_it_understands() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                assert!(body.get("api_base").is_none());
                axum::Json(chat_ok())
            }),
        );
        axum::serve(listener, app).await.ok();
    });
    let (_directory, app, token, _name) =
        configured_relay(2, &format!("http://{addr}/v1"), false).await;
    let base = |extra: Value| {
        let mut body = json!({
            "model":"coding",
            "messages":[{"role":"user","content":"hi"}],
            "max_tokens":16
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };

    // Every key the pinned Pi SDK's OpenAI-completions adapter can send is
    // accepted. Checked against the bundle rather than guessed: the first
    // draft of this allowlist would have rejected `prompt_cache_key` and
    // 400'd the next interactive turn.
    let (status, body) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        base(json!({
            "prompt_cache_key":"abc",
            "stream_options":{"include_usage":true},
            "parallel_tool_calls":false,
            "metadata":{"caller":"pi"},
            "store":false,
            "user":"someone"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A key nobody here has heard of is refused, by name.
    let (status, error) = send(
        app.clone(),
        "POST",
        "/v1/chat/completions",
        &token,
        base(json!({"litellm_something_new": {"route": "elsewhere"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("litellm_something_new"),
        "{error}"
    );

    // The long-stripped destination overrides keep being stripped rather than
    // refused: clients set them out of habit and have always worked.
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        base(json!({"api_base":"http://evil.example/v1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Local runtimes take sampling and template controls a hosted API rejects.
/// They pass for a profile whose data stays on the operator network, on both
/// inference paths, and are refused by name everywhere else.
#[tokio::test]
async fn local_runtime_options_pass_only_for_operator_network_profiles() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/v1/chat/completions",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(body);
                        axum::Json(chat_ok())
                    }
                }),
            );
            axum::serve(listener, app).await.ok();
        });
    }
    let (_directory, app, token, _name) =
        configured_relay(2, &format!("http://{addr}/v1"), false).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model":"coding","messages":[{"role":"user","content":"hi"}],"max_tokens":16,
               "top_k":20,"min_p":0.05,"repeat_penalty":1.05,
               "chat_template_kwargs":{"enable_thinking":false}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let upstream = seen.lock().unwrap();
    assert_eq!(upstream[0]["top_k"], 20);
    assert_eq!(
        upstream[0]["chat_template_kwargs"]["enable_thinking"],
        false
    );

    // The same keys against a hosted destination are refused, and named.
    let external: ModelProfile = serde_json::from_value(json!({
        "id":"p","name":"frontier","aliases":[],"endpoint_id":"e","model_id":"m",
        "context_tokens":8192,"max_output_tokens":1024,"uses":["interactive"],
        "workload_class":"interactive-coding","wake_policy":"never",
        "data_destination":"external","created_at":"now"
    }))
    .unwrap();
    let fields = json!({"model":"m","top_k":20}).as_object().unwrap().clone();
    assert_eq!(
        super::unsupported_option(&fields, super::RELAY_ONLY_KEYS, &external),
        Some("top_k")
    );
    let fields = json!({"model":"m","temperature":0.2})
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        super::unsupported_option(&fields, super::RELAY_ONLY_KEYS, &external),
        None
    );
}

#[test]
fn explicit_reasoning_is_checked_and_never_rewritten() {
    let mut profile = coding_large();
    let request =
        |effort: Value| json!({"model": "coding", "messages": [], "reasoning_effort": effort});

    // Unverified profile: any level in the vocabulary passes as sent.
    let mut body = request(json!("high"));
    constrain_interactive_request(&mut body, &profile).unwrap();
    assert_eq!(body["reasoning_effort"], "high");
    for refused in [json!("max"), json!("turbo"), json!(3)] {
        let error =
            constrain_interactive_request(&mut request(refused.clone()), &profile).unwrap_err();
        assert_eq!(
            error.code(),
            Some("unsupported_reasoning_effort"),
            "{refused}"
        );
    }

    // Verified profile: only verified levels, and the refusal names them.
    profile.catalog.reasoning_levels = vec!["none".into(), "high".into()];
    let mut body = request(json!("none"));
    constrain_interactive_request(&mut body, &profile).unwrap();
    assert_eq!(body["reasoning_effort"], "none");
    let error = constrain_interactive_request(&mut request(json!("low")), &profile).unwrap_err();
    assert_eq!(error.code(), Some("unsupported_reasoning_effort"));
    assert!(
        error.to_string().contains("supported: none, high"),
        "{error}"
    );
    let mut nested = json!({"model": "coding", "messages": [], "reasoning": {"effort": "medium"}});
    assert!(constrain_interactive_request(&mut nested, &profile).is_err());
    // Omitted stays omitted.
    let mut body = json!({"model": "coding", "messages": []});
    constrain_interactive_request(&mut body, &profile).unwrap();
    assert!(body.get("reasoning_effort").is_none());
}

async fn retirement_relay(upstream: &str) -> (tempfile::TempDir, AppState, String, String) {
    let (directory, state) = relay_state(5);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(
                json!({"name": "up", "kind": "openai", "base_url": upstream, "api_key": ""}),
            )
            .unwrap(),
        )
        .unwrap();
    let profile = state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "coding", "endpoint_id": endpoint.id, "model_id": "upstream",
                "context_tokens": 8192, "max_output_tokens": 1024, "workload_class": "fast-code-completion",
                "wake_policy": "never", "data_destination": "operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into(), "inference:invoke".into()],
        })
        .unwrap();
    (directory, state, profile.id, issued.token)
}

fn upstream_error(
    status: StatusCode,
    message: &'static str,
) -> impl Fn() -> axum::http::Response<Body> + Clone {
    move || {
        axum::http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"error": {"message": message}}).to_string(),
            ))
            .unwrap()
    }
}

#[tokio::test]
async fn only_a_not_found_answer_counts_toward_retirement() {
    for (status, message, counted) in [
        (
            StatusCode::NOT_FOUND,
            "No endpoints found for openai/gpt-5.6-sol.",
            true,
        ),
        (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded", false),
        (StatusCode::INTERNAL_SERVER_ERROR, "model not found", false),
        (StatusCode::NOT_FOUND, "Not Found", false),
    ] {
        let upstream = scripted_upstream(upstream_error(status, message)).await;
        let (_directory, state, id, token) = retirement_relay(&upstream).await;
        let request = json!({"model": "coding", "messages": [{"role": "user", "content": "hi"}]});
        let (relayed, body) = send(
            router(state.clone()),
            "POST",
            "/v1/chat/completions",
            &token,
            request.clone(),
        )
        .await;
        assert_eq!(
            relayed, status,
            "the client sees the upstream answer unchanged: {body}"
        );
        assert_eq!(body["error"]["message"], message);
        let failures = state
            .store
            .profile_retirement(&id)
            .unwrap()
            .unwrap()
            .hard_failures;
        assert_eq!(failures.len(), usize::from(counted), "{status} {message}");
        // The next answer succeeds and clears the count.
        let (relayed, _) = send(
            router(state.clone()),
            "POST",
            "/v1/chat/completions",
            &token,
            request,
        )
        .await;
        assert_eq!(relayed, StatusCode::OK);
        assert!(
            state
                .store
                .profile_retirement(&id)
                .unwrap()
                .unwrap()
                .hard_failures
                .is_empty()
        );
    }
}

#[tokio::test]
async fn relay_error_bodies_redact_the_provider_account_id() {
    // LiteLLM routing to OpenRouter embeds OpenRouter's raw error JSON,
    // including the operator's own OpenRouter account id, as an escaped
    // string inside its own `error.message`.
    let message = "litellm.BadRequestError: OpenrouterException - {\"error\":{\"message\":\"openai/x is not a valid model ID\",\"code\":400},\"user_id\":\"user_2abc\"}. Received Model Group=x";
    let upstream = scripted_upstream(move || {
        axum::http::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"error": {"message": message, "type": "invalid_request_error", "code": "400"}})
                    .to_string(),
            ))
            .unwrap()
    })
    .await;
    let (_directory, state, id, token) = retirement_relay(&upstream).await;
    let (status, body) = send(
        router(state.clone()),
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model": "coding", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let text = body.to_string();
    assert!(!text.contains("user_2abc"), "{text}");
    assert!(text.contains("is not a valid model ID"), "{text}");
    // Classification still runs on the original, unredacted text.
    assert_eq!(
        state
            .store
            .profile_retirement(&id)
            .unwrap()
            .unwrap()
            .hard_failures
            .len(),
        1,
        "{body}"
    );
}

#[tokio::test]
async fn a_disabled_profile_is_listed_as_disabled_and_refused() {
    let upstream = scripted_upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(chat_ok().to_string()))
            .unwrap()
    })
    .await;
    let (_directory, state, id, token) = retirement_relay(&upstream).await;
    let gone = crate::store::retirement::RouteOutcome::Gone {
        status: 404,
        detail: "HTTP 404: gone".into(),
    };
    for at in [
        "2026-09-23T10:00:00Z",
        "2026-09-23T10:20:00Z",
        "2026-09-23T10:40:00Z",
    ] {
        assert_eq!(
            state
                .store
                .record_route_outcome_at(&id, "relay", &gone, at.into())
                .unwrap(),
            at.ends_with("40:00Z")
        );
    }
    let (status, body) = send(
        router(state.clone()),
        "POST",
        "/v1/chat/completions",
        &token,
        json!({"model": "coding", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "model_disabled");
    let (_, models) = send(router(state), "GET", "/v1/models", &token, Value::Null).await;
    let listed = &models["data"][0]["poundlock"];
    assert_eq!(listed["status"], "disabled");
    assert_eq!(listed["disabled"]["reason"], "HTTP 404: gone");
    assert_eq!(listed["disabled"]["source"], "relay");
}

#[tokio::test]
async fn served_model_and_gateway_cost_are_forwarded_as_headers() {
    let upstream = scripted_upstream(|| {
        axum::http::Response::builder()
            .header("content-type", "application/json")
            .header("x-litellm-response-cost", "0.000412")
            .header("x-litellm-api-base", "http://secret-host/v1")
            .body(Body::from(chat_ok().to_string()))
            .unwrap()
    })
    .await;
    let (_directory, state, id, token) = retirement_relay(&upstream).await;
    let catalog: crate::domain::PatchModelProfile =
        serde_json::from_value(json!({"catalog": {"served_model": "openrouter/openai/gpt-6-sol"}}))
            .unwrap();
    state.store.patch_model_profile(&id, &catalog).unwrap();
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "coding", "messages": [{"role": "user", "content": "hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers["x-poundlock-served-model"],
        "openrouter/openai/gpt-6-sol"
    );
    assert_eq!(headers["x-poundlock-response-cost"], "0.000412");
    assert!(headers.get("x-litellm-api-base").is_none());
}

/// The published revision-4 row: a verified, priced, retired external profile.
fn contract_revision_4_row() -> Value {
    let mut profile = coding_large();
    profile.id = "8f0c7a52-3f0e-4c47-9a57-0d5b8f4e2a10".into();
    profile.name = "frontier-gpt-sol".into();
    profile.aliases = Vec::new();
    profile.model_id = "frontier-gpt-sol".into();
    profile.node_id = None;
    profile.runtime_profile = None;
    profile.wake_policy = "never".into();
    profile.data_destination = "external".into();
    profile.thinking = "model_default".into();
    profile.catalog = serde_json::from_value(json!({
        "reasoning_levels": ["none", "low", "medium", "high", "xhigh"],
        "reasoning_verified_at": "2026-09-23T14:05:00Z",
        "served_model": "openrouter/openai/gpt-6-sol",
        "provider": "openrouter",
        "pricing": {"input_per_mtok": 2.0, "output_per_mtok": 10.0, "source": "litellm_route",
                    "refreshed_at": "2026-09-23T14:00:00Z"}
    }))
    .unwrap();
    let failure = |at: &str| {
        json!({"at": at, "status": 404, "source": "relay",
        "detail": "HTTP 404: upstream reported \"no endpoints found\""})
    };
    profile.retirement = serde_json::from_value(json!({
        "disabled": {
            "reason": "HTTP 404: upstream reported \"no endpoints found\"",
            "source": "relay",
            "since": "2026-09-23T15:00:00Z",
            "evidence": [failure("2026-09-23T14:20:00Z"), failure("2026-09-23T14:41:00Z"), failure("2026-09-23T15:00:00Z")]
        }
    }))
    .unwrap();
    advertised_model(
        &profile,
        &profile.name,
        "litellm",
        "rev-9c1d",
        &Occupancy::new(),
    )
}

#[test]
fn published_models_fixture_is_what_the_relay_emits() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../docs/reference/fixtures/models-contract-revision-5.json"
    ))
    .unwrap();
    if std::env::var_os("POUNDLOCK_WRITE_FIXTURES").is_some() {
        let document = json!({
            "object": "list",
            "poundlock": crate::contract::models_poundlock_metadata(),
            "data": [contract_revision_4_row()],
        });
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/docs/reference/fixtures/models-contract-revision-5.json"
            ),
            serde_json::to_string_pretty(&document).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    assert_eq!(
        fixture["poundlock"],
        crate::contract::models_poundlock_metadata()
    );
    assert_eq!(fixture["data"][0], contract_revision_4_row());
}

/// A llama.cpp stand-in that records every chat body it is sent.
async fn llamacpp_upstream() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(body);
                    axum::Json(chat_ok())
                }
            }),
        );
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}/v1"), seen)
}

/// Two interactive profiles on one upstream: `orion` on the `llamacpp`
/// runtime and `other` on a runtime the relay knows nothing about.
async fn llamacpp_relay(
    thinking: &str,
    budget: Option<u64>,
) -> (
    tempfile::TempDir,
    axum::Router,
    String,
    Arc<std::sync::Mutex<Vec<Value>>>,
) {
    let (upstream, seen) = llamacpp_upstream().await;
    let (directory, state) = relay_state(5);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name":"up","kind":"openai","base_url":upstream,"api_key":""
            }))
            .unwrap(),
        )
        .unwrap();
    let node = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap();
    for (name, runtime, budget) in [("orion", "llamacpp", budget), ("other", "coding", None)] {
        state
            .store
            .create_model_profile(
                &serde_json::from_value(json!({
                    "name":name,
                    "endpoint_id":endpoint.id,
                    "model_id":"upstream",
                    "context_tokens":8192,
                    "max_output_tokens":1024,
                    "workload_class":"interactive-coding",
                    "wake_policy":"never",
                    "data_destination":"operator_network",
                    "node_id":node.node_id,
                    "runtime_profile":runtime,
                    "thinking":thinking,
                    "thinking_budget_tokens":budget
                }))
                .unwrap(),
            )
            .unwrap();
    }
    let issued = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into(), "inference:invoke".into()],
        })
        .unwrap();
    (directory, router(state), issued.token, seen)
}

async fn chat_as(
    app: &axum::Router,
    token: &str,
    model: &str,
    extra: Value,
) -> (StatusCode, Value) {
    let mut body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    send(app.clone(), "POST", "/v1/chat/completions", token, body).await
}

#[tokio::test]
async fn a_llamacpp_profile_turns_levels_into_template_kwargs() {
    let (_directory, app, token, seen) = llamacpp_relay("off", None).await;
    for extra in [
        json!({}),
        json!({"reasoning_effort": "none"}),
        json!({"reasoning_effort": "low"}),
        json!({"reasoning": {"effort": "medium"}}),
        json!({"chat_template_kwargs": {"enable_thinking": true, "reasoning_effort": "low"}}),
        json!({"reasoning_effort": "high", "chat_template_kwargs": {"enable_thinking": false}}),
    ] {
        let (status, body) = chat_as(&app, &token, "orion", extra).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let seen = seen.lock().unwrap();
    let kwargs: Vec<&Value> = seen
        .iter()
        .map(|body| &body["chat_template_kwargs"])
        .collect();
    assert_eq!(kwargs[0], &json!({"enable_thinking": false}));
    assert_eq!(kwargs[1], &json!({"enable_thinking": false}));
    assert_eq!(
        kwargs[2],
        &json!({"enable_thinking": true, "reasoning_effort": "low"})
    );
    assert_eq!(
        kwargs[3],
        &json!({"enable_thinking": true, "reasoning_effort": "medium"})
    );
    // The client's own switch is left exactly as it sent it.
    assert_eq!(
        kwargs[4],
        &json!({"enable_thinking": true, "reasoning_effort": "low"})
    );
    assert_eq!(kwargs[5], &json!({"enable_thinking": false}));
    // The profile default no longer rides as a top-level field no server reads.
    assert!(seen[0].get("reasoning_effort").is_none());
    assert!(
        seen.iter()
            .all(|body| body.get("thinking_budget_tokens").is_none())
    );
}

#[tokio::test]
async fn other_runtimes_keep_the_reasoning_effort_default() {
    let (_directory, app, token, seen) = llamacpp_relay("off", None).await;
    let (status, body) = chat_as(&app, &token, "other", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = chat_as(&app, &token, "other", json!({"reasoning_effort": "low"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0]["reasoning_effort"], "none");
    assert!(seen[0].get("chat_template_kwargs").is_none());
    assert_eq!(seen[1]["reasoning_effort"], "low");
    assert!(seen[1].get("chat_template_kwargs").is_none());
}

#[tokio::test]
async fn a_thinking_budget_is_forwarded_bounded_and_llamacpp_only() {
    let (_directory, app, token, seen) = llamacpp_relay("off", Some(512)).await;
    // The client's budget wins over the profile's default.
    let (status, body) = chat_as(
        &app,
        &token,
        "orion",
        json!({"reasoning_effort": "low", "thinking_budget_tokens": 300}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The profile default fills in when the client names none.
    let (status, body) = chat_as(&app, &token, "orion", json!({"reasoning_effort": "low"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Off asks for no thinking, so there is nothing to bound.
    let (status, body) = chat_as(&app, &token, "orion", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["thinking_budget_tokens"], 300);
        assert_eq!(seen[1]["thinking_budget_tokens"], 512);
        assert!(seen[2].get("thinking_budget_tokens").is_none());
    }
    for bad in [json!(0), json!(1025), json!(-3), json!(2.5), json!("512")] {
        let (status, body) = chat_as(
            &app,
            &token,
            "orion",
            json!({"thinking_budget_tokens": bad}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "invalid_thinking_budget", "{bad}");
    }
    // A profile on any other runtime refuses the field by name, as before.
    let (status, body) = chat_as(
        &app,
        &token,
        "other",
        json!({"thinking_budget_tokens": 100}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported inference option: thinking_budget_tokens")
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "refusals never reach the model"
    );
}

#[test]
fn only_llamacpp_profiles_advertise_a_thinking_budget() {
    let mut profile = coding_large();
    let other = advertised_model(&profile, "p", "openai", "rev", &Occupancy::new());
    assert!(
        other["poundlock"]["reasoning"]
            .get("thinking_budget_tokens")
            .is_none()
    );
    profile.runtime_profile = Some("llamacpp".into());
    profile.thinking_budget_tokens = Some(4096);
    let row = advertised_model(&profile, "p", "openai", "rev", &Occupancy::new());
    assert_eq!(
        row["poundlock"]["reasoning"]["thinking_budget_tokens"],
        json!({"max": 8192, "default": 4096})
    );
    profile.thinking_budget_tokens = None;
    let row = advertised_model(&profile, "p", "openai", "rev", &Occupancy::new());
    assert_eq!(
        row["poundlock"]["reasoning"]["thinking_budget_tokens"],
        json!({"max": 8192, "default": null})
    );
}

mod stalled_reader {
    use super::super::{Release, pumped};
    use futures_util::StreamExt;
    use std::io::Error as IoError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Set when the slot it stands for is released.
    struct Held(Arc<AtomicBool>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn slot() -> (Arc<AtomicBool>, Release) {
        let released = Arc::new(AtomicBool::new(false));
        (released.clone(), Release::new(Held(released)))
    }

    fn endless() -> impl futures_util::Stream<Item = Result<axum::body::Bytes, IoError>> {
        async_stream::stream! {
            loop {
                yield Ok(axum::body::Bytes::from_static(b"chunk"));
            }
        }
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_frees_the_model_after_the_stall() {
        let (released, release) = slot();
        let mut client = std::pin::pin!(pumped(
            endless(),
            Duration::from_millis(50),
            "test".into(),
            release
        ));
        assert!(client.next().await.is_some());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !released.load(Ordering::SeqCst),
            "freed before the stall window"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            released.load(Ordering::SeqCst),
            "a stalled reader still holds the model"
        );
    }

    #[tokio::test]
    async fn a_client_that_disconnects_frees_the_model_at_once() {
        let (released, release) = slot();
        let silent = async_stream::stream! {
            yield Ok::<_, IoError>(axum::body::Bytes::from_static(b"first"));
            std::future::pending::<()>().await;
        };
        let mut client = Box::pin(pumped(
            silent,
            Duration::from_secs(60),
            "test".into(),
            release,
        ));
        assert!(client.next().await.is_some());
        drop(client);
        assert!(released.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_reading_client_gets_every_chunk_and_the_end() {
        let (released, release) = slot();
        let source = futures_util::stream::iter(
            (0..100).map(|n| Ok(axum::body::Bytes::from(n.to_string()))),
        );
        let received: Vec<_> = pumped(source, Duration::from_secs(5), "test".into(), release)
            .collect()
            .await;
        assert_eq!(received.len(), 100);
        assert_eq!(received[99].as_ref().unwrap(), "99");
        assert!(released.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn inference_checks_the_token_before_reading_the_body() {
    let (_directory, app, _token, _name) =
        configured_relay(30, "http://127.0.0.1:9/v1", true).await;
    for path in ["/v1/chat/completions", "/v1/completions"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from("{ not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // A parse error here would mean the body was read for an unknown caller.
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

async fn send_waiting(
    app: axum::Router,
    token: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(
                    json!({"model":"coding","messages":[{"role":"user","content":"hi"}]})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_busy_model_holds_the_request_in_line_and_says_where_it_stands() {
    // An upstream that never answers: each request holds the slot until the
    // relay's one-second timeout.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            held.push(stream);
        }
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), true).await;
    let first = {
        let (app, token) = (app.clone(), token.clone());
        tokio::spawn(async move { send_waiting(app, &token, &[]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Owner priority needs `inference:priority`, which this token lacks.
    let (status, headers, body) = send_waiting(
        app.clone(),
        &token,
        &[("poundlock-wait", "0"), ("poundlock-priority", "owner")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "capacity_in_use");
    assert_eq!(body["queue"]["position"], 1);
    assert_eq!(body["queue"]["holder_kind"], "interactive");
    assert!(
        body["queue"].get("ticket").is_none(),
        "a request that did not wait keeps no place"
    );
    assert_eq!(headers["retry-after"], "15");

    // Waiting a little: still busy, with a place kept and a ticket.
    let (status, headers, body) = send_waiting(
        app.clone(),
        &token,
        &[("poundlock-wait", "0"), ("poundlock-session", "s")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, capacity) = send(app.clone(), "GET", "/v1/capacity", &token, Value::Null).await;
    assert_eq!(capacity["object"], "capacity");
    assert_eq!(capacity["slots"][0]["holder_kind"], "interactive");
    assert_eq!(capacity["slots"][0]["models"], json!(["coding"]));
    drop(headers);

    // Waiting long enough: the request gets the slot when the first lets go,
    // and then times out upstream itself rather than answering 409.
    let (status, _, body) = send_waiting(
        app.clone(),
        &token,
        &[("poundlock-wait", "5"), ("poundlock-priority", "owner")],
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(first.await.unwrap().0, StatusCode::GATEWAY_TIMEOUT);
}

async fn send_stream(
    app: axum::Router,
    token: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = json!({"model":"coding","stream":true,"messages":[{"role":"user","content":"hi"}]});
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn a_chat_app_that_sends_no_queue_headers_waits_on_its_open_stream() {
    // The first answer takes 700 ms, holding the slot; the next is at once.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let hits = hits.clone();
            async move {
                let first = hits.fetch_add(1, Ordering::SeqCst) == 0;
                if first {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                axum::http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        if first { "one" } else { "two" }
                    )))
                    .unwrap()
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, upstream).await.ok() });
    let (_directory, app, token, _name) =
        configured_relay(30, &format!("http://{addr}/v1"), true).await;
    let first = {
        let (app, token) = (app.clone(), token.clone());
        tokio::spawn(async move { send_stream(app, &token, &[]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A client that speaks the queue protocol still hears where it stands.
    let (status, _, _) = send_stream(app.clone(), &token, &[("poundlock-wait", "0")]).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, headers, text) = send_stream(app.clone(), &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(headers["content-type"], "text/event-stream");
    assert!(
        text.starts_with(": waiting for coding, position 1\n\n"),
        "{text}"
    );
    assert!(text.ends_with("data: two\n\ndata: [DONE]\n\n"), "{text}");
    assert!(first.await.unwrap().2.contains("data: one"));
}

#[tokio::test]
async fn a_held_stream_reports_its_turns_error_as_an_event() {
    // An upstream that never answers: each request holds the slot until the
    // relay's one-second timeout.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            held.push(stream);
        }
    });
    let (_directory, app, token, _name) =
        configured_relay(1, &format!("http://{addr}/v1"), true).await;
    let first = {
        let (app, token) = (app.clone(), token.clone());
        tokio::spawn(async move { send_stream(app, &token, &[]).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, _, text) = send_stream(app.clone(), &token, &[]).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let event = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("an error event");
    let error: Value = serde_json::from_str(event).unwrap();
    assert_eq!(error["error"]["code"], "upstream_timeout", "{text}");
    assert_eq!(first.await.unwrap().0, StatusCode::GATEWAY_TIMEOUT);
}

#[test]
fn a_token_allowed_to_go_ahead_waits_as_the_owner_unless_it_says() {
    let (_directory, state) = relay_state(30);
    let principal = |permissions: &[&str]| crate::domain::ApiTokenPrincipal {
        id: "t".into(),
        name: "chat".into(),
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
    };
    let none = axum::http::HeaderMap::new();
    let mut worker = axum::http::HeaderMap::new();
    worker.insert("poundlock-priority", "worker".parse().unwrap());
    let priority = principal(&["inference:invoke", "inference:priority"]);
    assert_eq!(
        wait_request(&none, &priority, &state).class,
        WaitClass::Owner
    );
    assert_eq!(
        wait_request(&worker, &priority, &state).class,
        WaitClass::Worker
    );
    // A value that does not parse is ordinary work, not the owner's.
    let mut typo = axum::http::HeaderMap::new();
    typo.insert("poundlock-priority", "bacground".parse().unwrap());
    assert_eq!(
        wait_request(&typo, &priority, &state).class,
        WaitClass::Worker
    );
    let plain = principal(&["inference:invoke"]);
    assert_eq!(wait_request(&none, &plain, &state).class, WaitClass::Worker);
}
