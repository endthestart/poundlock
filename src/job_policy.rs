use crate::job_budget::JobBudget;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobPolicy {
    pub verification: VerificationPolicy,
    pub network: JobNetwork,
    pub max_repairs: u8,
    /// Appends `DEPENDENCY_FRESHNESS_INSTRUCTION` to the job prompt so the
    /// agent resolves package versions from the registry instead of a
    /// remembered literal. Default false keeps today's behaviour.
    pub dependency_freshness: bool,
    /// Limits the submitter set on this job, enforced by the runner and the
    /// job's model proxy. Absent means only the install's own limits apply.
    #[serde(skip_serializing_if = "JobBudget::is_empty")]
    pub budget: JobBudget,
}

/// Appended to the job prompt when `JobPolicy.dependency_freshness` is set.
/// Pi can resolve current package versions when told to; left to its own
/// judgment it defaults to remembered, often stale, literals even with
/// outbound network access (see the dispatch dogfood notes this addresses).
pub const DEPENDENCY_FRESHNESS_INSTRUCTION: &str = "\n\nWhen adding or upgrading a package dependency, resolve its current version from the package registry (for example `npm view <package> version`, `pip index versions <package>`, `cargo search <package>`) instead of a remembered version literal.";

/// Bytes retained for one output stream (stdout or stderr) of one attempt,
/// given the allowed repair count. The runner's docker exec capture applies
/// this exact value per stream (`runner_sandbox.rs::run_agent`); compute it
/// here once so callers (`job-options`, the submit response) can show the
/// real effective budget instead of a second, hand-maintained constant.
/// The bounds `JobPolicy::validate` enforces, named so `job-options` can
/// publish the same values rather than a second, hand-maintained table. A
/// consumer found the timeout range by submitting a job and reading the 400.
pub const MAX_REPAIRS: u8 = 3;
pub const MAX_VERIFICATION_COMMANDS: usize = 8;
pub const MAX_PROTECTED_PATHS: usize = 64;
pub const MAX_POLICY_BYTES: usize = 32 * 1024;
pub const MIN_VERIFICATION_TIMEOUT_SECONDS: u64 = 1;
pub const MAX_VERIFICATION_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_VERIFICATION_TIMEOUT_SECONDS: u64 = 300;

pub fn effective_output_bytes_per_stream(max_repairs: u8) -> u64 {
    2 * 1024 * 1024 / (u64::from(max_repairs) + 1)
}

/// Bytes of patch retained for one attempt, given the allowed repair count.
/// Mirrors the per-attempt check in `collect_patches` (`runner_sandbox.rs`).
pub fn effective_patch_bytes(max_repairs: u8) -> u64 {
    10 * 1024 * 1024 / (u64::from(max_repairs) + 1)
}

/// Number of agent attempts a job can use: the original attempt plus repairs.
pub fn effective_attempts(max_repairs: u8) -> u8 {
    max_repairs + 1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobNetwork {
    #[default]
    InferenceOnly,
    Outbound,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationPolicy {
    pub commands: Vec<VerificationCommand>,
    /// Paths relative to the agent's initial working directory. A directory
    /// protects every entry below it; see `ProtectionMode` for exactly what
    /// counts as a change under each entry's mode.
    pub protected_paths: Vec<ProtectedPath>,
}

/// How a protected path's contents are compared between the original source
/// and the candidate. `Strict` is Poundlock's original, still-default
/// behaviour. `ExistingOnly` was added because a directory-level protection
/// otherwise forbids *any* addition beneath it (a job was rejected for
/// creating a virtualenv inside a protected directory even though nothing
/// tracked was touched) — separating "must not be modified" from "nothing
/// may be added" without weakening protection of what was actually there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionMode {
    /// Any addition, deletion or modification beneath the path fails
    /// verification.
    #[default]
    Strict,
    /// Only entries that already existed in the original source are
    /// protected against modification or deletion. New entries created
    /// beneath the path (for example a virtualenv or build cache an agent
    /// creates for its own use) do not fail verification.
    ExistingOnly,
}

/// A protected-path entry. Deserializes from a bare string (the original
/// wire format, implying `ProtectionMode::Strict`) or from an object naming
/// an explicit `mode`, so existing callers and already-stored jobs keep
/// working unchanged — this is an additive extension of the policy shape,
/// not a breaking one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProtectedPath {
    Bare(String),
    Entry {
        path: String,
        #[serde(default)]
        mode: ProtectionMode,
    },
}

impl ProtectedPath {
    pub fn path(&self) -> &str {
        match self {
            Self::Bare(path) | Self::Entry { path, .. } => path,
        }
    }
    pub fn mode(&self) -> ProtectionMode {
        match self {
            Self::Bare(_) => ProtectionMode::Strict,
            Self::Entry { mode, .. } => *mode,
        }
    }
}

/// Whether `original` and `candidate` fingerprints of one protected path
/// differ under `mode`. In `ExistingOnly` mode only entries present in
/// `original` are compared, so an added entry with no counterpart there is
/// never flagged; a modified or deleted original entry still is.
pub fn protected_path_changed(
    mode: ProtectionMode,
    original: &BTreeMap<PathBuf, String>,
    candidate: &BTreeMap<PathBuf, String>,
) -> bool {
    match mode {
        ProtectionMode::Strict => original != candidate,
        ProtectionMode::ExistingOnly => original
            .iter()
            .any(|(path, value)| candidate.get(path) != Some(value)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommand {
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_VERIFICATION_TIMEOUT_SECONDS
}

impl JobPolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_repairs <= MAX_REPAIRS,
            "max_repairs must be between 0 and {MAX_REPAIRS}"
        );
        ensure!(
            self.verification.commands.len() <= MAX_VERIFICATION_COMMANDS,
            "at most {MAX_VERIFICATION_COMMANDS} verification commands are allowed"
        );
        ensure!(
            self.verification.protected_paths.len() <= MAX_PROTECTED_PATHS,
            "at most {MAX_PROTECTED_PATHS} protected paths are allowed"
        );
        ensure!(
            self.max_repairs == 0 || self.verification.configured(),
            "repair requires verification commands or protected paths"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_POLICY_BYTES,
            "job policy exceeds 32 KiB"
        );
        for command in &self.verification.commands {
            ensure!(
                !command.command.trim().is_empty() && !command.command.contains('\0'),
                "verification command must be nonempty text without NUL characters"
            );
            ensure!(
                (MIN_VERIFICATION_TIMEOUT_SECONDS..=MAX_VERIFICATION_TIMEOUT_SECONDS)
                    .contains(&command.timeout_seconds),
                "verification timeout must be between {MIN_VERIFICATION_TIMEOUT_SECONDS} and {MAX_VERIFICATION_TIMEOUT_SECONDS} seconds"
            );
            relative_path(&command.cwd)?;
        }
        for path in &self.verification.protected_paths {
            ensure!(!path.path().is_empty(), "protected path must not be empty");
            relative_path(path.path())?;
        }
        self.budget.validate()?;
        Ok(())
    }
}

impl VerificationPolicy {
    pub fn configured(&self) -> bool {
        !self.commands.is_empty() || !self.protected_paths.is_empty()
    }
}

pub(crate) fn relative_path(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 1024 && !value.contains(['\0', '\\']),
        "invalid workspace-relative path"
    );
    ensure!(
        Path::new(value).components().all(|part| match part {
            Component::CurDir => true,
            Component::Normal(name) => !name.eq_ignore_ascii_case(".git"),
            _ => false,
        }),
        "path must stay inside the working directory and exclude Git metadata"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCheck {
    pub command: String,
    pub cwd: String,
    pub exit_code: Option<i64>,
    pub duration_ms: u64,
    pub output: String,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub status: String,
    pub reason: String,
    pub candidate_sha256: Option<String>,
    pub protected_paths: Vec<ProtectedPath>,
    pub changed_protected_paths: Vec<String>,
    /// Changed files that alter how Git applies or checks out the patch on the
    /// receiving machine, such as `.gitattributes`. Shown for review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flagged_paths: Vec<String>,
    pub commands: Vec<CommandCheck>,
    pub failure_code: Option<String>,
    /// The cutoff these checks ran after, when they ran against a turn that
    /// was interrupted. `passed` here means the retained patch satisfies the
    /// checks — never that the agent finished the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_python: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_uv: Option<String>,
}

impl VerificationReport {
    pub fn not_run(reason: &str) -> Self {
        Self {
            status: "unverified".into(),
            reason: reason.into(),
            candidate_sha256: None,
            protected_paths: vec![],
            changed_protected_paths: vec![],
            flagged_paths: vec![],
            commands: vec![],
            failure_code: None,
            checked_after: None,
            observed_python: None,
            observed_uv: None,
        }
    }
}

#[cfg(test)]
#[path = "job_policy_tests.rs"]
mod tests;
