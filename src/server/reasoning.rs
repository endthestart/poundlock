//! Reasoning levels and thinking budgets for llama.cpp profiles.
//!
//! llama-server starts with its own thinking default (here
//! `--chat-template-kwargs {"enable_thinking":false}`), and that default beats
//! a top-level `reasoning_effort`: the OpenAI-style level is accepted and does
//! nothing. Only the request's `chat_template_kwargs` switches thinking, so for
//! a profile on the `llamacpp` runtime the relay carries the same choice there.
//! It translates the choice and never changes it: the level the client named is
//! the level the template receives.
//!
//! | level the client named | profile thinking | kwargs added |
//! | --- | --- | --- |
//! | none | any | `enable_thinking: false` |
//! | minimal, low, medium, high, xhigh | any | `enable_thinking: true`, `reasoning_effort: <level>` |
//! | nothing | off | `enable_thinking: false` |
//! | nothing | model_default | nothing: the server's default applies |
//!
//! Keys the client already put in `chat_template_kwargs` are never replaced.
//! A client that sets `enable_thinking` has made the choice, and the request is
//! left as sent. Every other engine keeps the behaviour it had.

use crate::domain::{ModelProfile, serves_llamacpp};
use serde_json::{Map, Value, json};

/// Request fields only a llama.cpp profile accepts.
pub(super) const LLAMACPP_KEYS: &[&str] = &["thinking_budget_tokens"];

/// Whether `key` is a llama.cpp-only field this profile may carry.
pub(super) fn is_llamacpp_key(key: &str, profile: &ModelProfile) -> bool {
    LLAMACPP_KEYS.contains(&key) && serves_llamacpp(profile)
}

/// Why a `thinking_budget_tokens` the client sent cannot be used, if it cannot.
///
/// It must be a whole number from one up to the profile's output limit: a
/// budget above the limit would bound nothing, and a fraction or a string is
/// not a count. The field is forwarded as sent, never clamped.
pub(super) fn budget_problem(
    fields: &Map<String, Value>,
    profile: &ModelProfile,
) -> Option<String> {
    let value = fields.get("thinking_budget_tokens")?;
    match value.as_u64() {
        Some(tokens) if (1..=profile.max_output_tokens).contains(&tokens) => None,
        _ => Some(format!(
            "thinking_budget_tokens must be a whole number from 1 to {}, this model's output limit",
            profile.max_output_tokens
        )),
    }
}

/// Carry the request's reasoning level, or the profile's default, to a
/// llama.cpp server as the template kwargs it honours, and apply the profile's
/// default thinking budget when this request turns thinking on.
pub(super) fn apply_llamacpp(body: &mut Value, profile: &ModelProfile) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    let client_switch = match fields.get("chat_template_kwargs") {
        None => None,
        Some(Value::Object(kwargs)) => kwargs
            .get("enable_thinking")
            .map(|switch| switch.as_bool() == Some(true)),
        // Not an object: leave it for the server to refuse.
        Some(_) => return,
    };
    let thinking = if let Some(on) = client_switch {
        on
    } else {
        let level = requested_level(fields)
            .map(str::to_owned)
            .or_else(|| (profile.thinking == "off").then(|| "none".to_owned()));
        let Some(level) = level else {
            return;
        };
        let on = level != "none";
        let kwargs = fields
            .entry("chat_template_kwargs")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("chat_template_kwargs is an object here");
        kwargs.insert("enable_thinking".into(), json!(on));
        if on {
            kwargs
                .entry("reasoning_effort")
                .or_insert_with(|| json!(level));
        }
        on
    };
    if thinking
        && !fields.contains_key("thinking_budget_tokens")
        && let Some(budget) = profile.thinking_budget_tokens
    {
        fields.insert("thinking_budget_tokens".into(), json!(budget));
    }
}

/// The level a client asked for, in either OpenAI-compatible spelling.
fn requested_level(fields: &Map<String, Value>) -> Option<&str> {
    fields
        .get("reasoning_effort")
        .or_else(|| {
            fields
                .get("reasoning")
                .and_then(|value| value.get("effort"))
        })
        .and_then(Value::as_str)
}

#[cfg(test)]
#[path = "reasoning_tests.rs"]
mod tests;
