//! The draft check behind "Add a model": everything Poundlock would fill in
//! for a model on a given server, and why, before anything is saved.
//!
//! The rules live here once, so the console, scripts and Gateway publish
//! derive the same name, limits and defaults. It stores nothing and sends no
//! inference request. Destination is reported as declared, never inferred.

use super::{AdminAuth, ApiError, ApiResult, AppState};
use crate::domain::{
    AGENT_MIN_CONTEXT_TOKENS, DEFAULT_MODEL_USES, ModelProfile, Node, default_max_output_tokens,
};
use axum::extract::State;
use axum::{Json, Router, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/model-profiles/draft", post(draft))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DraftInput {
    pub endpoint_id: String,
    pub model_id: String,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub runtime_profile: Option<String>,
    #[serde(default)]
    pub uses: Option<Vec<String>>,
    /// A name the operator typed; checked, never changed.
    #[serde(default)]
    pub name: Option<String>,
    /// A context the operator typed, or a gateway route reported.
    #[serde(default)]
    pub context_tokens: Option<u64>,
}

async fn draft(
    State(state): State<AppState>,
    _admin: AdminAuth,
    Json(input): Json<DraftInput>,
) -> ApiResult<Json<Value>> {
    Ok(Json(model_draft(&state, &input)?))
}

pub(super) fn model_draft(state: &AppState, input: &DraftInput) -> ApiResult<Value> {
    let model_id = input.model_id.trim();
    if model_id.is_empty() {
        return Err(ApiError::bad("model_id is required"));
    }
    let endpoint = state
        .store
        .endpoint_secret(&input.endpoint_id)?
        .ok_or_else(|| ApiError::bad("endpoint_id does not exist"))?
        .endpoint;
    let node_id = input.node_id.as_deref().filter(|id| !id.is_empty());
    let runtime = input.runtime_profile.as_deref().filter(|id| !id.is_empty());
    if node_id.is_some() != runtime.is_some() {
        return Err(ApiError::bad(
            "node_id and runtime_profile must be configured together",
        ));
    }
    let node = match node_id {
        Some(id) => state
            .store
            .node(id, state.config.node_offline_seconds)?
            .filter(|node| node.status != "revoked"),
        None => None,
    };
    let profiles = state.store.list_model_profiles()?;
    let taken = |candidate: &str| {
        profiles.iter().any(|profile| {
            profile.name == candidate || profile.aliases.iter().any(|alias| alias == candidate)
        })
    };
    let mut problems = Vec::new();
    let mut problem = |code: &str, message: String| {
        problems.push(json!({"code": code, "message": message}));
    };

    let gateway = endpoint.kind == "litellm";
    let (name, name_source) = match input.name.as_deref().map(str::trim) {
        Some(given) if !given.is_empty() => {
            if taken(given) {
                problem(
                    "name_taken",
                    format!("Another model already uses the name '{given}'. Choose another."),
                );
            }
            (given.to_owned(), "given")
        }
        _ => {
            let (base, source) = if gateway {
                (model_id.to_owned(), "route")
            } else if let Some(node) = &node {
                (
                    format!("{}-{}", slug(&node.name), slug(model_id)),
                    "machine_and_model",
                )
            } else {
                (
                    format!("{}-{}", slug(&endpoint.name), slug(model_id)),
                    "server_and_model",
                )
            };
            (free_name(&base, runtime, &taken), source)
        }
    };

    let identity = node
        .as_ref()
        .zip(runtime)
        .and_then(|(node, runtime)| runtime_identity(node, runtime));
    let reported = identity
        .and_then(|identity| identity["models"].as_array())
        .and_then(|models| models.iter().find(|model| model["id"] == model_id));
    if let (Some(node), Some(runtime)) = (&node, runtime) {
        if node.status == "maintenance" {
            problem(
                "machine_paused",
                format!(
                    "{} is paused for maintenance. Resume it on Machines before testing.",
                    node.name
                ),
            );
        }
        let runtimes = node.capabilities["runtimes"].as_array();
        if !runtimes.is_some_and(|items| items.iter().any(|item| item == runtime)) {
            problem(
                "runtime_not_reported",
                format!("{} does not report a runtime named {runtime}.", node.name),
            );
        }
        let servable: Vec<&str> = identity
            .and_then(|identity| identity["servable"].as_array())
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !servable.is_empty() && !servable.contains(&model_id) {
            problem(
                "not_servable",
                format!(
                    "{runtime} on {} serves {}, not {model_id}.",
                    node.name,
                    servable.join(", ")
                ),
            );
        }
    } else if let Some(id) = node_id {
        problem(
            "machine_missing",
            format!("No enrolled machine has the id {id}."),
        );
    }

    let (context, context_source) = match (input.context_tokens, reported) {
        (Some(given), _) => (Some(given), "given"),
        (None, Some(model)) => (model["context_tokens"].as_u64(), "runtime"),
        _ => (None, "unknown"),
    };
    let context_source = if context.is_none() {
        "unknown"
    } else {
        context_source
    };
    let uses: Vec<String> = input
        .uses
        .clone()
        .filter(|uses| !uses.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_USES.map(str::to_owned).to_vec());
    match context {
        None if !gateway => problem(
            "context_unknown",
            "Enter the context from the server's settings; nothing reported it.".into(),
        ),
        Some(context)
            if uses.iter().any(|item| item == "durable") && context < AGENT_MIN_CONTEXT_TOKENS =>
        {
            problem(
                "context_below_agent_minimum",
                format!(
                    "This server's context is {context}, and agent jobs need {AGENT_MIN_CONTEXT_TOKENS}. Choose Interactive requests, or raise it on the server."
                ),
            )
        }
        _ => {}
    }

    // Where prompts go is declared, never derived: a model server's
    // declaration is inherited, and a gateway route states its own.
    let destination = match endpoint.data_destination.as_deref() {
        None => json!({"value": null, "source": "per_route"}),
        Some("unknown") => {
            problem(
                "destination_undeclared",
                format!(
                    "Declare where {} sends prompts. Every model on it inherits it.",
                    endpoint.name
                ),
            );
            json!({"value": null, "source": "undeclared"})
        }
        Some(declared) => json!({"value": declared, "source": "endpoint"}),
    };
    let (wake_policy, wake_source) = match &node {
        None => ("never", "no_machine"),
        Some(node) if node.wol_mac.is_some() && node.wol_broadcast.is_some() => {
            ("on-demand", "wake_on_lan")
        }
        Some(_) => ("manual", "no_wake_on_lan"),
    };
    // The same test the relay uses to carry thinking to llama-server as
    // template kwargs (`serves_llamacpp`), so Off here is Off there.
    let llama_cpp = runtime == Some(crate::domain::LLAMACPP_RUNTIME);
    let (thinking, thinking_source) = if node.is_some() && llama_cpp {
        ("off", "llama_cpp_runtime")
    } else {
        ("model_default", "default")
    };
    let alias_owner = profiles
        .iter()
        .find(|profile| {
            profile.name == model_id || profile.aliases.iter().any(|alias| alias == model_id)
        })
        .map(|profile| profile.name.clone());
    let existing: Vec<Value> = profiles
        .iter()
        .filter(|profile| profile.endpoint_id == endpoint.id && profile.model_id == model_id)
        .map(|profile: &ModelProfile| json!({"id": profile.id, "name": profile.name}))
        .collect();

    Ok(json!({
        "server_model_id": model_id,
        "name": {"value": name, "source": name_source},
        "model_label": {
            "value": reported.and_then(|model| model["label"].as_str()),
            "source": if reported.is_some_and(|model| model["label"].is_string()) { "runtime" } else { "unknown" },
        },
        "context_tokens": {"value": context, "source": context_source},
        "max_output_tokens": {
            "value": context.map(default_max_output_tokens),
            "source": if context.is_some() { "quarter_of_context" } else { "unknown" },
        },
        "uses": {"value": uses, "source": if input.uses.as_ref().is_some_and(|uses| !uses.is_empty()) { "given" } else { "default" }},
        "wake_policy": {"value": wake_policy, "source": wake_source},
        "data_destination": destination,
        "thinking": {"value": thinking, "source": thinking_source},
        "server_model_id_alias": {
            "available": alias_owner.is_none() && model_id != name,
            "used_by": alias_owner,
        },
        "existing": existing,
        "problems": problems,
    }))
}

fn runtime_identity<'a>(node: &'a Node, runtime: &str) -> Option<&'a Value> {
    node.capabilities["inventory"]["runtimes"]
        .get(runtime)
        .filter(|identity| identity.is_object())
}

/// Lowercase `[a-z0-9._-]`, anything else a single hyphen.
pub(super) fn slug(value: &str) -> String {
    let mut out = String::new();
    for c in value.trim().chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out
        .trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(120)
        .collect();
    if out.is_empty() { "model".into() } else { out }
}

/// `base` if it is free, else `base-<runtime>`, else `base-2`, `base-3`, …
/// A deliberate variant such as `-64k` is the operator's to type.
pub(super) fn free_name(base: &str, runtime: Option<&str>, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    if let Some(runtime) = runtime.map(slug) {
        let candidate = format!("{base}-{runtime}");
        if !base.ends_with(&format!("-{runtime}")) && !taken(&candidate) {
            return candidate;
        }
    }
    (2..)
        .map(|number| format!("{base}-{number}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range always finds a free name")
}

#[cfg(test)]
#[path = "model_draft_tests.rs"]
mod tests;
