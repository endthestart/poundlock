//! The tools an outside agent uses to have Poundlock write code for it: choose
//! a model, delegate a task over a folder it may upload, follow the job, read
//! the result and its checks, and apply the patch after reviewing it.

mod apply;
mod delegate;
mod jobs;
mod models;
mod patch;
mod schema;
#[cfg(test)]
mod tests;
mod workspace;

pub use patch::patch_paths;
pub use schema::definitions;
pub use workspace::Workspace;

use super::brief::TaskBrief;
use super::controller::Controller;
use super::provenance;
use super::snapshot;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// A patch longer than this is saved to a file rather than returned inline.
const MAX_INLINE_PATCH_BYTES: usize = 200 * 1024;
/// How much of an agent's output and a check's output a result shows.
const OUTPUT_TAIL_CHARS: usize = 4_000;
const CHECK_TAIL_CHARS: usize = 2_000;
/// The longest a single wait may last; an agent waits again if it needs to.
const MAX_WAIT_SECONDS: u64 = 600;
const DEFAULT_WAIT_SECONDS: u64 = 240;

pub struct Tools {
    pub controller: Controller,
    pub workspace: Workspace,
    /// Read access for Git sources on private repositories, given to the
    /// controller with a job and never shown to the agent.
    pub git_credentials: Option<crate::git_source::GitCredentials>,
    /// How often a wait asks the controller; short in tests.
    pub poll: Duration,
}

/// What a tool call returns: text for the model, the same facts as data, and
/// whether the call failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub structured: Value,
    pub is_error: bool,
}

impl Outcome {
    fn ok(text: impl Into<String>, structured: Value) -> Self {
        Self {
            text: text.into(),
            structured,
            is_error: false,
        }
    }

    fn error(error: &anyhow::Error) -> Self {
        let mut structured = json!({"error": format!("{error:#}")});
        if let Some(controller) = error.downcast_ref::<super::controller::ControllerError>() {
            structured["status"] = json!(controller.status);
            structured["code"] = json!(controller.code);
        }
        Self {
            text: format!("{error:#}"),
            structured,
            is_error: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryArgs {
    directory: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    directory: String,
    model: String,
    #[serde(flatten)]
    brief: TaskBrief,
    /// `folder` (default) uploads the folder; `git` sends its pushed commit.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    job_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    job_id: String,
    #[serde(default)]
    events_after: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    job_id: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    job_id: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    attempt: Option<usize>,
    #[serde(default)]
    save_to: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyArgs {
    job_id: String,
    directory: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    attempt: Option<usize>,
    #[serde(default)]
    check_only: bool,
    #[serde(default)]
    branch: Option<String>,
}

fn arguments<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    let value = if value.is_null() { json!({}) } else { value };
    serde_json::from_value(value).map_err(|error| anyhow::anyhow!("invalid arguments: {error}"))
}

/// A job id goes into a URL path; refuse anything that could change the path.
fn job_path(id: &str) -> Result<String> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "job_id is not a Poundlock job id"
    );
    Ok(format!("api/v1/jobs/{id}"))
}

/// Where a long call reports how it is going, when the client asked for
/// progress. Each message is one short line.
pub type Progress = tokio::sync::mpsc::UnboundedSender<String>;

impl Tools {
    pub async fn call(&self, name: &str, args: Value) -> Outcome {
        self.call_with_progress(name, args, None).await
    }

    pub async fn call_with_progress(
        &self,
        name: &str,
        args: Value,
        progress: Option<Progress>,
    ) -> Outcome {
        let result = match name {
            "list_models" => self.list_models().await,
            "delegate_task" => match arguments(args) {
                Ok(args) => self.delegate(args).await,
                Err(error) => Err(error),
            },
            "job_status" => match arguments(args) {
                Ok(args) => self.status(args).await,
                Err(error) => Err(error),
            },
            "wait_for_job" => match arguments(args) {
                Ok(args) => self.wait(args, progress.as_ref()).await,
                Err(error) => Err(error),
            },
            "job_result" => match arguments::<JobArgs>(args) {
                Ok(args) => self.result(&args.job_id).await,
                Err(error) => Err(error),
            },
            "get_patch" => match arguments(args) {
                Ok(args) => self.patch(args).await,
                Err(error) => Err(error),
            },
            "apply_patch" => match arguments(args) {
                Ok(args) => self.apply(args).await,
                Err(error) => Err(error),
            },
            "cancel_job" => match arguments::<JobArgs>(args) {
                Ok(args) => self.cancel(&args.job_id).await,
                Err(error) => Err(error),
            },
            "provenance" => match arguments::<DirectoryArgs>(args) {
                Ok(args) => self.provenance(&args.directory),
                Err(error) => Err(error),
            },
            other => Err(anyhow::anyhow!("unknown tool: {other}")),
        };
        result.unwrap_or_else(|error| Outcome::error(&error))
    }

    async fn options(&self) -> Result<Value> {
        self.controller.get("api/v1/job-options").await
    }
}

fn finished(job: &Value) -> bool {
    crate::domain::is_terminal_job_status(job["status"].as_str().unwrap_or(""))
}

/// The last `limit` characters of `text`, marked when cut.
fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - limit).collect();
    format!("[…{} earlier characters omitted]\n{kept}", count - limit)
}
