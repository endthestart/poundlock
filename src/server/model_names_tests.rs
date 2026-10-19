use super::*;
use crate::domain::CreateApiToken;
use crate::server::ops_tests::{send, state_with_artifacts};
use crate::server::router;
use serde_json::Value;

fn profile(id: &str, name: &str, model_id: &str, aliases: &[&str]) -> ModelProfile {
    serde_json::from_value(json!({
        "id": id, "name": name, "aliases": aliases, "endpoint_id": "endpoint",
        "model_id": model_id, "context_tokens": 8192, "max_output_tokens": 2048,
        "uses": ["interactive"], "workload_class": "interactive-coding",
        "wake_policy": "never", "node_id": null, "runtime_profile": null,
        "created_at": "now"
    }))
    .unwrap()
}

fn names(suggestions: &[Suggestion]) -> Vec<(&str, &str)> {
    suggestions
        .iter()
        .map(|item| (item.name.as_str(), item.reason))
        .collect()
}

#[test]
fn a_server_model_id_suggests_every_name_for_it_first() {
    let profiles = [
        profile("a", "orion-qwen3.8-27b-mtp", "qwen3.8-27b-mtp", &[]),
        profile("b", "vega-qwen3.8-27b-mtp", "qwen3.8-27b-mtp", &[]),
        profile("c", "unrelated", "other", &[]),
    ];
    assert_eq!(
        names(&did_you_mean("qwen3.8-27b-mtp", &profiles)),
        [
            ("orion-qwen3.8-27b-mtp", "server_model_id"),
            ("vega-qwen3.8-27b-mtp", "server_model_id"),
        ]
    );
}

#[test]
fn loose_matches_ignore_case_and_accept_a_suffix_at_a_word_boundary() {
    let profiles = [
        profile("a", "orion-qwen3.8-27b-mtp", "qwen3.8-27b-mtp", &[]),
        profile(
            "b",
            "Frontier",
            "openrouter/vendor/frontier",
            &["legacy-name"],
        ),
        profile("c", "unmatched", "other", &[]),
    ];
    assert_eq!(
        names(&did_you_mean("QWEN3.8-27B-MTP", &profiles)),
        [("orion-qwen3.8-27b-mtp", "similar_name")]
    );
    assert_eq!(
        names(&did_you_mean("frontier", &profiles)),
        [("Frontier", "similar_name")]
    );
    assert_eq!(
        names(&did_you_mean("LEGACY-NAME", &profiles)),
        [("Frontier", "similar_name")]
    );
    // A suffix counts only at a word boundary: "7b-mtp" is not a word of
    // "27b-mtp".
    assert!(did_you_mean("7b-mtp", &profiles).is_empty());
    assert!(did_you_mean("nothing-like-it", &profiles).is_empty());
}

#[test]
fn suggestions_stop_at_five() {
    let profiles: Vec<_> = (0..8)
        .map(|index| {
            profile(
                &index.to_string(),
                &format!("host{index}-model"),
                "model",
                &[],
            )
        })
        .collect();
    assert_eq!(did_you_mean("model", &profiles).len(), 5);
}

#[test]
fn the_message_names_the_right_string_to_send() {
    let exact = [Suggestion {
        name: "orion-qwen3.8-27b-mtp".into(),
        profile_id: "a".into(),
        reason: "server_model_id",
    }];
    assert_eq!(
        unknown_model_message("qwen3.8-27b-mtp", Some(&exact), true),
        "No model is named 'qwen3.8-27b-mtp'. That is the server's own model ID for \
         'orion-qwen3.8-27b-mtp'. Send that name instead. GET /v1/models lists the \
         names available to this token."
    );
    assert_eq!(
        unknown_model_message("gone", Some(&[]), true),
        "No model is named 'gone'. GET /v1/models lists the names available to this token."
    );
    assert!(unknown_model_message("", None, true).starts_with("The request does not name"));
    let hidden = unknown_model_message("qwen3.8-27b-mtp", None, false);
    assert!(hidden.contains("models:read"), "{hidden}");
    assert!(!hidden.contains("orion"), "{hidden}");
    let long = "x\n".repeat(400);
    assert!(unknown_model_message(&long, None, true).len() < 300);
}

async fn relay_with_names(permissions: &[&str]) -> (tempfile::TempDir, axum::Router, String) {
    let (directory, state) = state_with_artifacts(None);
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "server", "kind": "openai", "base_url": "http://127.0.0.1:9/v1", "api_key": ""
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "orion-qwen3.8-27b-mtp", "endpoint_id": endpoint.id,
                "model_id": "qwen3.8-27b-mtp", "context_tokens": 8192,
                "max_output_tokens": 2048, "uses": ["interactive"], "wake_policy": "never",
                "data_destination": "operator_network"
            }))
            .unwrap(),
        )
        .unwrap();
    let token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "client".into(),
            permissions: permissions.iter().map(|item| item.to_string()).collect(),
        })
        .unwrap()
        .token;
    (directory, router(state), token)
}

fn chat(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn a_server_model_id_gets_the_same_404_with_the_name_to_send() {
    let (_directory, app, token) = relay_with_names(&["models:read", "inference:invoke"]).await;
    for path in ["/v1/chat/completions", "/v1/completions"] {
        let (status, body) = send(app.clone(), "POST", path, &token, chat("qwen3.8-27b-mtp")).await;
        // Dispatch's permission probe depends on exactly this status and code.
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "unknown_model_profile");
        assert_eq!(body["error"]["type"], "poundlock_error");
        assert_eq!(body["error"]["param"], "model");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("'orion-qwen3.8-27b-mtp'"), "{message}");
        let suggested = &body["error"]["poundlock"]["did_you_mean"];
        assert_eq!(suggested[0]["name"], "orion-qwen3.8-27b-mtp");
        assert_eq!(suggested[0]["reason"], "server_model_id");
        assert!(suggested[0]["profile_id"].as_str().is_some());
    }
    // The name itself still works: the server model ID is never routed.
    let (status, body) = send(app, "POST", "/v1/chat/completions", &token, chat("gone")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["poundlock"]["did_you_mean"], json!([]));
}

#[tokio::test]
async fn a_token_that_cannot_list_models_learns_no_names() {
    let (_directory, app, token) = relay_with_names(&["inference:invoke"]).await;
    let (status, body) = send(
        app,
        "POST",
        "/v1/chat/completions",
        &token,
        chat("qwen3.8-27b-mtp"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_model_profile");
    assert!(body["error"].get("poundlock").is_none(), "{body}");
    assert!(!body.to_string().contains("orion"), "{body}");
}

#[tokio::test]
async fn the_published_fixture_is_what_the_relay_sends() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../docs/reference/fixtures/unknown-model-profile-error.json"
    ))
    .unwrap();
    let suggestion = Suggestion {
        name: "orion-qwen3.8-27b-mtp".into(),
        profile_id: fixture["error"]["poundlock"]["did_you_mean"][0]["profile_id"]
            .as_str()
            .unwrap()
            .into(),
        reason: "server_model_id",
    };
    let response = unknown_model_response("qwen3.8-27b-mtp", Some(&[suggestion]), true);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    use http_body_util::BodyExt;
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), fixture);
}
