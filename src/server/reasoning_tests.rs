use super::*;
use crate::domain::ResourceCheck;
use crate::server::relay::{apply_reasoning_settings, constrain_interactive_request};
use std::collections::BTreeSet;

fn profile(runtime: Option<&str>, thinking: &str) -> ModelProfile {
    ModelProfile {
        id: "profile".into(),
        name: "orion-27b".into(),
        aliases: Vec::new(),
        endpoint_id: "endpoint".into(),
        model_id: "qwen3.8-27b-mtp".into(),
        model_label: None,
        context_tokens: 131072,
        max_output_tokens: 8192,
        uses: BTreeSet::from(["interactive".to_string()]),
        workload_class: "interactive-coding".into(),
        wake_policy: "never".into(),
        node_id: runtime.map(|_| "node".into()),
        runtime_profile: runtime.map(str::to_owned),
        idle_seconds: None,
        data_destination: "operator_network".into(),

        data_destination_source: "profile".into(),
        thinking: thinking.into(),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
        catalog: Default::default(),
        retirement: Default::default(),
        created_at: "now".into(),
        last_check: ResourceCheck::default(),
    }
}

fn llamacpp(thinking: &str) -> ModelProfile {
    profile(Some("llamacpp"), thinking)
}

fn shaped(mut body: Value, profile: &ModelProfile) -> Value {
    apply_reasoning_settings(&mut body, profile);
    body
}

#[test]
fn a_level_becomes_the_template_kwargs_llamacpp_honours() {
    for level in ["minimal", "low", "medium", "high", "xhigh"] {
        // The profile default does not matter once the client names a level.
        for thinking in ["off", "model_default"] {
            let body = shaped(
                json!({"model": "x", "reasoning_effort": level}),
                &llamacpp(thinking),
            );
            assert_eq!(
                body["chat_template_kwargs"],
                json!({"enable_thinking": true, "reasoning_effort": level}),
                "{level} on a {thinking} profile"
            );
        }
    }
    // The other spelling of the same choice.
    let body = shaped(
        json!({"model": "x", "reasoning": {"effort": "low"}}),
        &llamacpp("off"),
    );
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": true, "reasoning_effort": "low"})
    );
}

#[test]
fn none_and_an_off_profile_force_thinking_off() {
    for thinking in ["off", "model_default"] {
        let body = shaped(
            json!({"model": "x", "reasoning_effort": "none"}),
            &llamacpp(thinking),
        );
        assert_eq!(
            body["chat_template_kwargs"],
            json!({"enable_thinking": false})
        );
    }
    let body = shaped(json!({"model": "x"}), &llamacpp("off"));
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    // No second carrier of the same default: the kwargs are the one source.
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn a_model_default_profile_with_no_level_sends_nothing() {
    let original = json!({"model": "x", "messages": []});
    assert_eq!(
        shaped(original.clone(), &llamacpp("model_default")),
        original
    );
}

#[test]
fn a_client_enable_thinking_is_respected_untouched() {
    for switch in [true, false] {
        let original = json!({
            "model": "x",
            "reasoning_effort": "low",
            "chat_template_kwargs": {"enable_thinking": switch, "preserve_thinking": true},
        });
        assert_eq!(shaped(original.clone(), &llamacpp("off")), original);
    }
}

#[test]
fn kwargs_the_client_sent_are_never_replaced() {
    let body = shaped(
        json!({"model": "x", "reasoning_effort": "high",
               "chat_template_kwargs": {"reasoning_effort": "low", "custom": 1}}),
        &llamacpp("off"),
    );
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"reasoning_effort": "low", "custom": 1, "enable_thinking": true})
    );
    // Something that is not an object is left for the server to refuse.
    let odd = json!({"model": "x", "reasoning_effort": "low", "chat_template_kwargs": "x"});
    assert_eq!(shaped(odd.clone(), &llamacpp("off")), odd);
}

#[test]
fn other_engines_keep_the_behaviour_they_had() {
    for runtime in [None, Some("ollama"), Some("coding"), Some("host-manager")] {
        let off = profile(runtime, "off");
        assert_eq!(
            shaped(json!({"model": "x"}), &off),
            json!({"model": "x", "reasoning_effort": "none"})
        );
        let explicit = json!({"model": "x", "reasoning_effort": "low"});
        assert_eq!(shaped(explicit.clone(), &off), explicit);
        let default = profile(runtime, "model_default");
        assert_eq!(
            shaped(json!({"model": "x"}), &default),
            json!({"model": "x"})
        );
    }
}

#[test]
fn the_profile_budget_applies_only_when_this_request_thinks() {
    let mut profile = llamacpp("off");
    profile.thinking_budget_tokens = Some(4096);
    let body = shaped(json!({"model": "x", "reasoning_effort": "low"}), &profile);
    assert_eq!(body["thinking_budget_tokens"], 4096);
    // A client that turns thinking on itself gets it too.
    let body = shaped(
        json!({"model": "x", "chat_template_kwargs": {"enable_thinking": true}}),
        &profile,
    );
    assert_eq!(body["thinking_budget_tokens"], 4096);
    // The client's own budget wins.
    let body = shaped(
        json!({"model": "x", "reasoning_effort": "low", "thinking_budget_tokens": 512}),
        &profile,
    );
    assert_eq!(body["thinking_budget_tokens"], 512);
    // Nothing to bound when thinking is off.
    for body in [
        json!({"model": "x"}),
        json!({"model": "x", "reasoning_effort": "none"}),
        json!({"model": "x", "chat_template_kwargs": {"enable_thinking": false}}),
    ] {
        assert!(
            shaped(body, &profile)
                .get("thinking_budget_tokens")
                .is_none()
        );
    }
    // A model-default profile leaves the server's own default alone.
    let mut default = llamacpp("model_default");
    default.thinking_budget_tokens = Some(4096);
    assert!(
        shaped(json!({"model": "x"}), &default)
            .get("thinking_budget_tokens")
            .is_none()
    );
}

fn constrained(body: Value, profile: &ModelProfile) -> Result<Value, String> {
    let mut body = body;
    constrain_interactive_request(&mut body, profile)
        .map(|()| body)
        .map_err(|error| error.to_string())
}

#[test]
fn a_budget_is_accepted_for_llamacpp_only_and_must_fit_the_output_limit() {
    let profile = llamacpp("off");
    for accepted in [1, 4096, 8192] {
        let body = constrained(
            json!({"model": "x", "thinking_budget_tokens": accepted}),
            &profile,
        )
        .unwrap();
        assert_eq!(body["thinking_budget_tokens"], accepted);
    }
    for refused in [
        json!(0),
        json!(8193),
        json!(-1),
        json!(1.5),
        json!(4096.0),
        json!("4096"),
        json!(true),
        json!(null),
        json!([4096]),
    ] {
        let message = constrained(
            json!({"model": "x", "thinking_budget_tokens": refused}),
            &profile,
        )
        .unwrap_err();
        assert!(
            message.contains("thinking_budget_tokens"),
            "{refused}: {message}"
        );
    }
    // Every other engine still refuses the field by name, as before.
    for runtime in [None, Some("ollama"), Some("coding")] {
        let message = constrained(
            json!({"model": "x", "thinking_budget_tokens": 1024}),
            &profile_for(runtime),
        )
        .unwrap_err();
        assert_eq!(
            message,
            "unsupported inference option: thinking_budget_tokens"
        );
    }
}

fn profile_for(runtime: Option<&str>) -> ModelProfile {
    profile(runtime, "model_default")
}

#[test]
fn a_bad_budget_is_a_coded_refusal() {
    let mut body = json!({"model": "x", "thinking_budget_tokens": 0});
    let error = constrain_interactive_request(&mut body, &llamacpp("off")).unwrap_err();
    assert_eq!(error.code(), Some("invalid_thinking_budget"));
}
