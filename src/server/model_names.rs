//! Model names: the string a client sends, as opposed to the server model ID
//! a machine or gateway uses for the same weights.
//!
//! A request names a profile by its UUID, its name or an explicit alias, and
//! nothing else. The server model ID is never accepted in its place, because
//! which profile it meant would depend on the set of other profiles. What this
//! module adds is saying so helpfully when a client sends the wrong string.

use super::AppState;
use crate::domain::{ApiTokenPrincipal, ModelProfile};
use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use serde_json::json;

/// At most this many names are offered, so a vague request cannot list the
/// catalogue through the error.
const MAX_SUGGESTIONS: usize = 5;

/// A name the client may have meant, and why it was offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Suggestion {
    pub name: String,
    pub profile_id: String,
    /// `server_model_id`: the client sent this profile's server model ID.
    /// `similar_name`: the request matches a name, alias or server model ID
    /// ignoring case, or as its suffix.
    pub reason: &'static str,
}

/// Profiles a client may have meant by `requested`: exact server model IDs
/// first, then loose matches, never more than `MAX_SUGGESTIONS`.
pub(super) fn did_you_mean(requested: &str, profiles: &[ModelProfile]) -> Vec<Suggestion> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<Suggestion> = profiles
        .iter()
        .filter(|profile| profile.model_id == requested)
        .map(|profile| Suggestion {
            name: profile.name.clone(),
            profile_id: profile.id.clone(),
            reason: "server_model_id",
        })
        .collect();
    let wanted = requested.to_lowercase();
    for profile in profiles {
        if found.len() >= MAX_SUGGESTIONS {
            break;
        }
        if found.iter().any(|item| item.profile_id == profile.id) {
            continue;
        }
        let similar = std::iter::once(&profile.name)
            .chain(profile.aliases.iter())
            .chain(std::iter::once(&profile.model_id))
            .any(|candidate| loosely_matches(&candidate.to_lowercase(), &wanted));
        if similar {
            found.push(Suggestion {
                name: profile.name.clone(),
                profile_id: profile.id.clone(),
                reason: "similar_name",
            });
        }
    }
    found.truncate(MAX_SUGGESTIONS);
    found
}

/// Equal, or one ends with the other at a word boundary: `qwen3.8-27b-mtp`
/// matches `orion-qwen3.8-27b-mtp` and `openrouter/qwen3.8-27b-mtp`.
fn loosely_matches(candidate: &str, wanted: &str) -> bool {
    let suffix = |long: &str, short: &str| {
        short.len() >= 3
            && long.len() > short.len()
            && long.ends_with(short)
            && long[..long.len() - short.len()].ends_with(['-', '_', '.', '/', ':'])
    };
    candidate == wanted || suffix(candidate, wanted) || suffix(wanted, candidate)
}

/// The 404 for a request whose `model` names no profile. Status and code stay
/// what Dispatch's permission probe expects; the message says what to send
/// instead, and `poundlock.did_you_mean` lists it. Names appear only for a
/// token that may list models anyway, so this reveals nothing `/v1/models`
/// would not.
pub(super) fn unknown_model(
    state: &AppState,
    principal: &ApiTokenPrincipal,
    requested: &str,
) -> axum::response::Response {
    let may_list = principal.has_permission("models:read");
    let suggestions = if may_list {
        match state.store.list_model_profiles() {
            Ok(profiles) => Some(did_you_mean(requested, &profiles)),
            Err(error) => {
                tracing::warn!(%error, "could not list model names for an unknown-model reply");
                None
            }
        }
    } else {
        None
    };
    unknown_model_response(requested, suggestions.as_deref(), may_list)
}

pub(super) fn unknown_model_response(
    requested: &str,
    suggestions: Option<&[Suggestion]>,
    may_list: bool,
) -> axum::response::Response {
    let message = unknown_model_message(requested, suggestions, may_list);
    let mut error = json!({
        "type": "poundlock_error",
        "code": "unknown_model_profile",
        "param": "model",
        "message": message,
    });
    if let Some(suggestions) = suggestions {
        error["poundlock"] = json!({ "did_you_mean": suggestions });
    }
    (StatusCode::NOT_FOUND, Json(json!({ "error": error }))).into_response()
}

fn unknown_model_message(
    requested: &str,
    suggestions: Option<&[Suggestion]>,
    may_list: bool,
) -> String {
    let requested = shown(requested);
    let listing = if may_list {
        "GET /v1/models lists the names available to this token."
    } else {
        "Ask the operator for the model name, or use a token with models:read to list them."
    };
    if requested.is_empty() {
        return format!("The request does not name a model. {listing}");
    }
    let suggestions = suggestions.unwrap_or_default();
    let quoted = |items: &[&Suggestion]| {
        items
            .iter()
            .map(|item| format!("'{}'", item.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let exact: Vec<&Suggestion> = suggestions
        .iter()
        .filter(|item| item.reason == "server_model_id")
        .collect();
    let similar: Vec<&Suggestion> = suggestions
        .iter()
        .filter(|item| item.reason != "server_model_id")
        .collect();
    match (exact.as_slice(), similar.as_slice()) {
        ([], []) => format!("No model is named '{requested}'. {listing}"),
        ([only], _) => format!(
            "No model is named '{requested}'. That is the server's own model ID for '{}'. Send that name instead. {listing}",
            only.name
        ),
        ([_, _, ..], _) => format!(
            "No model is named '{requested}'. That is the server's own model ID for {}. Send one of those names instead. {listing}",
            quoted(&exact)
        ),
        ([], [only]) => format!(
            "No model is named '{requested}'. Did you mean '{}'? {listing}",
            only.name
        ),
        ([], _) => format!(
            "No model is named '{requested}'. Did you mean one of {}? {listing}",
            quoted(&similar)
        ),
    }
}

/// The client's own string, shortened and without control characters, so the
/// message stays one readable line.
fn shown(requested: &str) -> String {
    requested
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}

#[cfg(test)]
#[path = "model_names_tests.rs"]
mod tests;
