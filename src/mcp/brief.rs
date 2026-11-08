//! A task an outside manager hands to a durable job: what to do, where, and how
//! the result will be judged. The brief becomes the job's prompt and its
//! verification policy, so the checks the manager names are run by Poundlock's
//! runner, not merely asked for in prose.

use crate::job_policy::{
    JobNetwork, JobPolicy, ProtectedPath, ProtectionMode, VerificationCommand, VerificationPolicy,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// The longest task text accepted, in bytes.
const MAX_TASK_BYTES: usize = 64 * 1024;
/// The longest context text accepted, in bytes.
const MAX_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskBrief {
    /// What to build or change, in the manager's words.
    pub task: String,
    /// Files the manager expects the task to create or change.
    #[serde(default)]
    pub files: Vec<String>,
    /// What the worker should know: decisions, constraints, code to reuse.
    #[serde(default)]
    pub context: Option<String>,
    /// Commands that must succeed in the workspace for the result to count,
    /// such as `cargo test` or `npm test`. Run by the runner after the agent.
    #[serde(default)]
    pub acceptance_checks: Vec<String>,
    /// Seconds each acceptance check may take.
    #[serde(default)]
    pub check_timeout_seconds: Option<u64>,
    /// Paths the job must not change at all, such as tests that define the
    /// expected behaviour.
    #[serde(default)]
    pub frozen_paths: Vec<String>,
    /// Paths whose existing files must not change, though new files may be
    /// added beneath them.
    #[serde(default)]
    pub protect_existing: Vec<String>,
    /// Repair rounds when a check fails, 0 to 3. Defaults to 1 when there is
    /// anything to check, else 0.
    #[serde(default)]
    pub max_repairs: Option<u8>,
    /// `inference_only` (default) or `outbound`, for a task that must download
    /// dependencies.
    #[serde(default)]
    pub network: Option<String>,
    /// Ask the agent to resolve package versions from the registry.
    #[serde(default)]
    pub dependency_freshness: bool,
    /// Minutes the whole job may take, repairs included.
    #[serde(default)]
    pub budget_minutes: Option<u64>,
    /// US dollars the job may spend on an external model.
    #[serde(default)]
    pub budget_usd: Option<f64>,
}

impl TaskBrief {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.task.trim().is_empty(), "task must describe the work");
        ensure!(
            self.task.len() <= MAX_TASK_BYTES,
            "task must be at most 64 KiB"
        );
        ensure!(
            self.context.as_deref().unwrap_or_default().len() <= MAX_CONTEXT_BYTES,
            "context must be at most 64 KiB"
        );
        for path in self
            .files
            .iter()
            .chain(&self.frozen_paths)
            .chain(&self.protect_existing)
        {
            crate::source::validate_path(path.trim_end_matches('/'))
                .map_err(|_| anyhow::anyhow!("{path} must be a relative path inside the folder"))?;
        }
        if let Some(usd) = self.budget_usd {
            ensure!(
                usd.is_finite() && usd >= 0.01,
                "budget_usd must be at least 0.01"
            );
        }
        if let Some(network) = &self.network {
            ensure!(
                matches!(network.as_str(), "inference_only" | "outbound"),
                "network must be inference_only or outbound"
            );
        }
        self.policy().validate()
    }

    /// The job's verification policy: the brief's checks and protections, run
    /// and enforced by the runner.
    pub fn policy(&self) -> JobPolicy {
        let commands = self
            .acceptance_checks
            .iter()
            .filter(|command| !command.trim().is_empty())
            .map(|command| VerificationCommand {
                command: command.clone(),
                cwd: String::new(),
                timeout_seconds: self
                    .check_timeout_seconds
                    .unwrap_or(crate::job_policy::DEFAULT_VERIFICATION_TIMEOUT_SECONDS),
            })
            .collect::<Vec<_>>();
        let protected_paths = self
            .frozen_paths
            .iter()
            .map(|path| ProtectedPath::Bare(path.trim_end_matches('/').to_owned()))
            .chain(
                self.protect_existing
                    .iter()
                    .map(|path| ProtectedPath::Entry {
                        path: path.trim_end_matches('/').to_owned(),
                        mode: ProtectionMode::ExistingOnly,
                    }),
            )
            .collect::<Vec<_>>();
        let verification = VerificationPolicy {
            commands,
            protected_paths,
        };
        let checked = verification.configured();
        JobPolicy {
            verification,
            network: match self.network.as_deref() {
                Some("outbound") => JobNetwork::Outbound,
                _ => JobNetwork::InferenceOnly,
            },
            max_repairs: self.max_repairs.unwrap_or(u8::from(checked)),
            dependency_freshness: self.dependency_freshness,
            budget: crate::job_budget::JobBudget {
                wall_seconds: self
                    .budget_minutes
                    .map(|minutes| minutes.saturating_mul(60)),
                frontier_cents: self.budget_usd.map(|usd| (usd * 100.0).round() as u64),
            },
        }
    }

    /// The job's prompt. The checks and protections are restated so the agent
    /// aims at them; the runner enforces them either way.
    pub fn prompt(&self) -> String {
        let mut prompt = self.task.trim().to_owned();
        if !self.files.is_empty() {
            prompt.push_str("\n\nFiles you are expected to create or change:\n");
            for file in &self.files {
                prompt.push_str(&format!("- {file}\n"));
            }
            prompt.push_str(
                "If the task needs other files, change them too and say why in your final message.",
            );
        }
        if let Some(context) = self
            .context
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            prompt.push_str("\n\nContext from the manager who planned this task:\n");
            prompt.push_str(context);
        }
        if !self.acceptance_checks.is_empty() {
            prompt.push_str(
                "\n\nThe result is accepted only if these commands succeed in this workspace afterwards:\n",
            );
            for check in &self.acceptance_checks {
                prompt.push_str(&format!("- `{check}`\n"));
            }
            prompt.push_str(
                "Run them yourself before you finish. Never weaken, skip or delete a test to make them pass.",
            );
        }
        if !self.frozen_paths.is_empty() || !self.protect_existing.is_empty() {
            prompt.push_str("\n\nDo not change these paths; they define what correct means:\n");
            for path in self.frozen_paths.iter().chain(&self.protect_existing) {
                prompt.push_str(&format!("- {path}\n"));
            }
            prompt.pop();
        }
        prompt.push_str(
            "\n\nWork only inside this workspace. When you finish, end with a short summary: what you changed, how you checked it, and anything you could not do.",
        );
        prompt
    }
}

#[cfg(test)]
#[path = "brief_tests.rs"]
mod tests;
