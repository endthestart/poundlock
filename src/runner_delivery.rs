use super::RunnerClient;
use crate::domain::RunnerResult;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub(super) struct Report {
    pub job_id: String,
    pub result: Option<RunnerResult>,
    pub error: Option<Value>,
    pub cancelled: bool,
}

impl Report {
    // Call only after owned workers and working files have been removed. Keep
    // retained patches and the failed/cancelled outcome; cleanup is not success.
    pub(super) fn confirm_recovered_cleanup(&mut self, error: Value, restarted: bool) {
        if !self.error.as_ref().is_some_and(|value| {
            matches!(
                value["code"].as_str(),
                Some("cleanup_interrupted" | "cleanup_failed")
            )
        }) {
            return;
        }
        if let Some(result) = self.result.as_mut() {
            if result.provenance["verification"]["status"] == "pending" {
                result.provenance["verification"]["status"] = serde_json::json!("unverified");
                result.provenance["verification"]["reason"] =
                    serde_json::json!("Verification was interrupted before completion.");
            }
            result.cleanup = serde_json::json!({"containers_removed":true,"worktrees_removed":true,"attempt_directory_removed":true,"recovered_after_restart":restarted});
        }
        self.error = Some(error);
    }

    pub fn save(&self, work_dir: &Path) -> Result<PathBuf> {
        let directory = work_dir.join("outbox");
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let target = directory.join(format!("{}.json", self.job_id));
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        serde_json::to_writer(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(&target).context("persist runner report")?;
        #[cfg(unix)]
        std::fs::File::open(&directory)?.sync_all()?;
        Ok(target)
    }
}

#[derive(Serialize, Deserialize)]
struct ActiveAttempt {
    job_id: String,
    attempt_id: String,
}

pub(super) fn begin(result_dir: &Path, job: &crate::domain::Job) -> Result<()> {
    let directory = result_dir.join("active");
    std::fs::create_dir_all(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    serde_json::to_writer(
        &mut file,
        &ActiveAttempt {
            job_id: job.id.clone(),
            attempt_id: job.attempt_id.clone(),
        },
    )?;
    file.as_file().sync_all()?;
    file.persist_noclobber(directory.join(format!("{}.json", job.attempt_id)))?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

pub(super) fn ended(result_dir: &Path, attempt: &str) -> Result<()> {
    std::fs::remove_file(result_dir.join("active").join(format!("{attempt}.json")))?;
    #[cfg(unix)]
    std::fs::File::open(result_dir.join("active"))?.sync_all()?;
    Ok(())
}

// Called only after the daemon confirmed removal of every owned worker. The
// record lives with the supervisor outbox, outside every worker mount.
pub(super) fn recover(result_dir: &Path, work_dir: &Path) -> Result<()> {
    let directory = result_dir.join("active");
    if !directory.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let active: ActiveAttempt = serde_json::from_reader(std::fs::File::open(&path)?)?;
        uuid::Uuid::parse_str(&active.job_id)?;
        uuid::Uuid::parse_str(&active.attempt_id)?;
        let attempt = work_dir.join(&active.attempt_id);
        if attempt.exists() {
            std::fs::remove_dir_all(attempt)?;
        }
        let retained = result_dir
            .join("outbox")
            .join(format!("{}.json", active.job_id));
        let mut report = if retained.exists() {
            serde_json::from_reader::<_, Report>(std::fs::File::open(retained)?)?
        } else {
            Report {
                job_id: active.job_id,
                result: None,
                cancelled: false,
                error: Some(serde_json::json!({
                    "layer":"runner","code":"runner_interrupted","retry_safe":false,
                    "message":"The supervisor stopped during this attempt. Its workers have been removed; inspect retained evidence before retrying."
                })),
            }
        };
        report.confirm_recovered_cleanup(
            serde_json::json!({"layer":"runner","code":"runner_interrupted","retry_safe":false,
                "message":"The supervisor stopped before completing this attempt. Its workers and working files have been removed."}),
            true,
        );
        report.save(result_dir)?;
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// A report is removed only after an authenticated, successful acknowledgement.
// A terminal conflict retains evidence for operator recovery; it never reruns
// the harness or overwrites the controller's terminal decision.
pub(super) async fn deliver_pending(
    client: &RunnerClient,
    work_dir: &Path,
    active: &std::collections::HashSet<String>,
) -> Result<()> {
    let directory = work_dir.join("outbox");
    if !directory.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&directory)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let report: Report = serde_json::from_reader(std::fs::File::open(&path)?)?;
        if active.contains(&report.job_id) {
            continue;
        }
        let delivered = if let Some(error) = &report.error {
            client
                .fail_with_result(
                    &report.job_id,
                    error,
                    report.cancelled,
                    report.result.as_ref(),
                )
                .await
        } else if let Some(result) = &report.result {
            client.complete(&report.job_id, result).await
        } else {
            anyhow::bail!("runner report has neither result nor error")
        };
        match delivered {
            Ok(()) => std::fs::remove_file(&path)?,
            // Already decided upstream: drop the report rather than retrying
            // it on every poll for the life of this runner.
            Err(error) if error.downcast_ref::<super::AlreadySettled>().is_some() => {
                tracing::info!(job_id=%report.job_id, %error,
                    "controller already recorded this job; discarding the retained report");
                std::fs::remove_file(&path)?;
            }
            Err(error) => tracing::warn!(job_id=%report.job_id, %error,
                "report retained in runner outbox; delivery will retry"),
        }
    }
    Ok(())
}
