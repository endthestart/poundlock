use super::*;
use crate::domain::EnrollNode;
use crate::server::ops_tests::{send, state_with_artifacts};
use crate::server::router;
use axum::http::StatusCode;

fn orion_capabilities() -> Value {
    json!({
        "runtimes": ["llamacpp", "ollama"],
        "inventory": {"runtimes": {"llamacpp": {
            "servable": ["qwen3.5-9b-q8_0", "qwen3.8-27b-mtp"],
            "models": [
                {"id": "qwen3.8-27b-mtp", "label": "Qwen3.8-27B IQ3_S", "context_tokens": 131072},
                {"id": "qwen3.5-9b-q8_0", "label": "Qwen3.5-9B Q8_0", "context_tokens": 65536}
            ]
        }}}
    })
}

struct Fixture {
    _directory: tempfile::TempDir,
    state: AppState,
    server: String,
    gateway: String,
    orion: String,
}

fn fixture() -> Fixture {
    let (directory, state) = state_with_artifacts(None);
    let endpoint = |name: &str, kind: &str| {
        state
            .store
            .create_endpoint(
                &serde_json::from_value(json!({
                    "name": name, "kind": kind, "base_url": "http://127.0.0.1:9/v1", "api_key": ""
                }))
                .unwrap(),
            )
            .unwrap()
            .id
    };
    let server = endpoint("Orion llama-server", "openai");
    state
        .store
        .set_endpoint_destination(&server, "operator_network")
        .unwrap();
    let gateway = endpoint("LiteLLM gateway", "litellm");
    let orion = state
        .store
        .enroll_node(&EnrollNode {
            token: state.store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: orion_capabilities(),
        })
        .unwrap()
        .node_id;
    state
        .store
        .heartbeat_node(&orion, "linux", &orion_capabilities())
        .unwrap();
    Fixture {
        _directory: directory,
        state,
        server,
        gateway,
        orion,
    }
}

impl Fixture {
    fn draft(&self, input: Value) -> Value {
        model_draft(&self.state, &serde_json::from_value(input).unwrap()).unwrap()
    }

    fn machine_draft(&self, model_id: &str) -> Value {
        self.draft(json!({
            "endpoint_id": self.server, "model_id": model_id,
            "node_id": self.orion, "runtime_profile": "llamacpp"
        }))
    }

    fn add(&self, name: &str, model_id: &str) {
        self.state
            .store
            .create_model_profile(
                &serde_json::from_value(json!({
                    "name": name, "endpoint_id": self.server, "model_id": model_id,
                    "context_tokens": 65536, "max_output_tokens": 8192, "uses": ["durable"],
                    "wake_policy": "never"
                }))
                .unwrap(),
            )
            .unwrap();
    }
}

fn codes(draft: &Value) -> Vec<&str> {
    draft["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect()
}

#[test]
fn a_machine_model_is_named_and_sized_from_what_the_machine_reports() {
    let fixture = fixture();
    let draft = fixture.machine_draft("qwen3.8-27b-mtp");
    assert_eq!(
        draft["name"],
        json!({"value": "orion-qwen3.8-27b-mtp", "source": "machine_and_model"})
    );
    assert_eq!(draft["server_model_id"], "qwen3.8-27b-mtp");
    assert_eq!(
        draft["model_label"],
        json!({"value": "Qwen3.8-27B IQ3_S", "source": "runtime"})
    );
    assert_eq!(
        draft["context_tokens"],
        json!({"value": 131072, "source": "runtime"})
    );
    // A quarter of the context, at most 32,768.
    assert_eq!(draft["max_output_tokens"]["value"], 32768);
    assert_eq!(
        fixture.machine_draft("qwen3.5-9b-q8_0")["max_output_tokens"]["value"],
        16384
    );
    assert_eq!(
        draft["uses"],
        json!({"value": ["durable", "interactive"], "source": "default"})
    );
    // No Wake-on-LAN settings yet: start the model while the machine is on.
    assert_eq!(
        draft["wake_policy"],
        json!({"value": "manual", "source": "no_wake_on_lan"})
    );
    assert_eq!(
        draft["thinking"],
        json!({"value": "off", "source": "llama_cpp_runtime"})
    );
    assert_eq!(draft["server_model_id_alias"]["available"], true);
    assert_eq!(draft["existing"], json!([]));
    assert_eq!(
        draft["data_destination"],
        json!({"value": "operator_network", "source": "endpoint"})
    );
    assert_eq!(codes(&draft), Vec::<&str>::new());

    fixture
        .state
        .store
        .configure_wol(
            &fixture.orion,
            Some("02:11:22:33:44:55"),
            Some("192.0.2.255:9"),
        )
        .unwrap();
    assert_eq!(
        fixture.machine_draft("qwen3.8-27b-mtp")["wake_policy"],
        json!({"value": "on-demand", "source": "wake_on_lan"})
    );
}

#[test]
fn a_taken_name_gets_the_runtime_then_a_number() {
    let fixture = fixture();
    fixture.add("orion-qwen3.8-27b-mtp", "qwen3.8-27b-mtp");
    let draft = fixture.machine_draft("qwen3.8-27b-mtp");
    assert_eq!(draft["name"]["value"], "orion-qwen3.8-27b-mtp-llamacpp");
    assert_eq!(draft["existing"][0]["name"], "orion-qwen3.8-27b-mtp");
    // The server model ID is still free as an alias until something takes it.
    assert_eq!(draft["server_model_id_alias"]["available"], true);
    fixture.add("orion-qwen3.8-27b-mtp-llamacpp", "other");
    assert_eq!(
        fixture.machine_draft("qwen3.8-27b-mtp")["name"]["value"],
        "orion-qwen3.8-27b-mtp-2"
    );
    fixture.add("qwen3.8-27b-mtp", "other-again");
    assert_eq!(
        fixture.machine_draft("qwen3.8-27b-mtp")["server_model_id_alias"],
        json!({"available": false, "used_by": "qwen3.8-27b-mtp"})
    );
}

#[test]
fn a_typed_name_is_checked_not_changed() {
    let fixture = fixture();
    fixture.add("taken", "qwen3.8-27b-mtp");
    let draft = fixture.draft(json!({
        "endpoint_id": fixture.server, "model_id": "qwen3.8-27b-mtp",
        "node_id": fixture.orion, "runtime_profile": "llamacpp", "name": "taken"
    }));
    assert_eq!(draft["name"], json!({"value": "taken", "source": "given"}));
    assert_eq!(codes(&draft), ["name_taken"]);
}

#[test]
fn a_server_without_a_machine_asks_for_its_context_and_never_wakes() {
    let fixture = fixture();
    let draft = fixture.draft(json!({"endpoint_id": fixture.server, "model_id": "Qwen/Qwen3 8B"}));
    assert_eq!(
        draft["name"],
        json!({"value": "orion-llama-server-qwen-qwen3-8b", "source": "server_and_model"})
    );
    assert_eq!(
        draft["wake_policy"],
        json!({"value": "never", "source": "no_machine"})
    );
    assert_eq!(draft["thinking"]["value"], "model_default");
    assert_eq!(
        draft["context_tokens"],
        json!({"value": null, "source": "unknown"})
    );
    assert_eq!(codes(&draft), ["context_unknown"]);
    let small = fixture.draft(json!({
        "endpoint_id": fixture.server, "model_id": "small", "context_tokens": 4096
    }));
    assert_eq!(small["context_tokens"]["source"], "given");
    assert_eq!(small["max_output_tokens"]["value"], 1024);
    assert_eq!(codes(&small), ["context_below_agent_minimum"]);
    let interactive = fixture.draft(json!({
        "endpoint_id": fixture.server, "model_id": "small", "context_tokens": 4096,
        "uses": ["interactive"]
    }));
    assert_eq!(codes(&interactive), Vec::<&str>::new());
}

#[test]
fn a_gateway_route_keeps_its_route_name() {
    let fixture = fixture();
    let draft =
        fixture.draft(json!({"endpoint_id": fixture.gateway, "model_id": "frontier-gpt-sol"}));
    assert_eq!(
        draft["name"],
        json!({"value": "frontier-gpt-sol", "source": "route"})
    );
    // The gateway reports a route's context at publish time, and each route
    // states its own destination then.
    assert_eq!(codes(&draft), Vec::<&str>::new());
    assert_eq!(
        draft["data_destination"],
        json!({"value": null, "source": "per_route"})
    );
    assert_eq!(draft["server_model_id_alias"]["available"], false);
}

#[test]
fn problems_name_an_unservable_model_and_an_unreported_runtime() {
    let fixture = fixture();
    let draft = fixture.machine_draft("not-in-the-manifest");
    assert_eq!(codes(&draft), ["not_servable", "context_unknown"]);
    assert!(
        draft["problems"][0]["message"]
            .as_str()
            .unwrap()
            .contains("serves qwen3.5-9b-q8_0, qwen3.8-27b-mtp")
    );
    let draft = fixture.draft(json!({
        "endpoint_id": fixture.server, "model_id": "x",
        "node_id": fixture.orion, "runtime_profile": "vllm", "context_tokens": 8192
    }));
    assert_eq!(codes(&draft), ["runtime_not_reported"]);
    assert_eq!(draft["thinking"]["value"], "model_default");
}

#[test]
fn an_undeclared_server_is_asked_not_guessed() {
    let fixture = fixture();
    let fresh = fixture
        .state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "fresh", "kind": "openai", "base_url": "http://127.0.0.1:9/v1", "api_key": ""
            }))
            .unwrap(),
        )
        .unwrap();
    let draft =
        fixture.draft(json!({"endpoint_id": fresh.id, "model_id": "m", "context_tokens": 8192}));
    assert_eq!(
        draft["data_destination"],
        json!({"value": null, "source": "undeclared"})
    );
    assert_eq!(codes(&draft), ["destination_undeclared"]);
}

#[test]
fn slugs_are_lowercase_and_plain() {
    assert_eq!(slug("Orion llama-server"), "orion-llama-server");
    assert_eq!(slug("  Lab Box / GPU #2 "), "lab-box-gpu-2");
    assert_eq!(slug("qwen3.8-27b-mtp"), "qwen3.8-27b-mtp");
    assert_eq!(slug("--"), "model");
}

#[tokio::test]
async fn the_draft_route_is_admin_only_and_stores_nothing() {
    let fixture = fixture();
    let app = router(fixture.state.clone());
    let body = json!({"endpoint_id": fixture.server, "model_id": "qwen3.8-27b-mtp",
        "node_id": fixture.orion, "runtime_profile": "llamacpp"});
    let (status, _) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles/draft",
        "not-admin",
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, draft) = send(
        app.clone(),
        "POST",
        "/api/v1/model-profiles/draft",
        "admin-secret",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["name"]["value"], "orion-qwen3.8-27b-mtp");
    assert!(
        fixture
            .state
            .store
            .list_model_profiles()
            .unwrap()
            .is_empty()
    );
    let (status, _) = send(
        app,
        "POST",
        "/api/v1/model-profiles/draft",
        "admin-secret",
        json!({"endpoint_id": "missing", "model_id": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
