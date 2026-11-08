//! Applying a reviewed patch to the folder, or as a new branch.

use super::*;

impl Tools {
    pub(super) async fn apply(&self, args: ApplyArgs) -> Result<Outcome> {
        let directory = self.workspace.resolve(&args.directory)?;
        let patch = self
            .download_patch(&args.job_id, args.index, args.attempt)
            .await?;
        ensure!(!patch.trim().is_empty(), "the job's patch is empty");
        let files = patch_paths(&patch)?;
        for file in &files {
            ensure!(
                crate::source::validate_path(file).is_ok(),
                "the patch touches {file}, which is not a plain path inside the folder; refusing it"
            );
            ensure!(
                !snapshot::is_secret(file),
                "the patch touches {file}, a credential file; refusing it"
            );
        }
        if let Some(branch) = args.branch.as_deref() {
            return self
                .apply_to_branch(&args, &directory, &patch, &files, branch)
                .await;
        }
        let check = git_apply(&directory, &patch, true).await?;
        if !check.0 {
            bail!(
                "The patch does not apply cleanly to {}; nothing was changed. Git said: {}",
                directory.display(),
                check.1
            );
        }
        if args.check_only {
            return Ok(Outcome::ok(
                format!(
                    "The patch applies cleanly to {} ({} file(s)). Nothing was changed.",
                    directory.display(),
                    files.len()
                ),
                json!({"job_id": args.job_id, "applied": false, "applies": true, "files": files}),
            ));
        }
        // What the folder held just before, so changes made some other way
        // can be told apart later, and whether the job's checks passed.
        // Applying never waits on this record.
        let verification = match self.controller.get(&job_path(&args.job_id)?).await {
            Ok(job) => job["result"]["provenance"]["verification"]["status"]
                .as_str()
                .map(str::to_owned),
            Err(_) => None,
        };
        let mut note = String::new();
        let pending = match provenance::begin(&directory) {
            Ok(pending) => pending,
            Err(error) => {
                note = format!(" Provenance was not recorded: {error:#}.");
                None
            }
        };
        let applied = git_apply(&directory, &patch, false).await?;
        ensure!(
            applied.0,
            "Git could not apply the patch after checking it: {}",
            applied.1
        );
        let recorded = match pending {
            Some(pending) => match provenance::finish(
                pending,
                &directory,
                provenance::JobPatch {
                    job_id: &args.job_id,
                    attempt: args.attempt,
                    index: args.index,
                    patch: &patch,
                    files: &files,
                    verification: verification.as_deref(),
                },
            ) {
                Ok(_) => true,
                Err(error) => {
                    note = format!(" Provenance was not recorded: {error:#}.");
                    false
                }
            },
            None => {
                if note.is_empty() {
                    note = " Provenance is recorded only in Git folders.".to_owned();
                }
                false
            }
        };
        Ok(Outcome::ok(
            format!(
                "Applied the patch to {}: {} file(s) changed: {}. Run your own checks before relying on it.{note}",
                directory.display(),
                files.len(),
                files.join(", ")
            ),
            json!({"job_id": args.job_id, "applied": true, "files": files, "provenance_recorded": recorded}),
        ))
    }

    /// The patch as one commit on a new branch, leaving the folder as it is.
    pub(super) async fn apply_to_branch(
        &self,
        args: &ApplyArgs,
        directory: &Path,
        patch: &str,
        files: &[String],
        branch: &str,
    ) -> Result<Outcome> {
        let job = self.controller.get(&job_path(&args.job_id)?).await?;
        let subject: String = job["prompt"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Poundlock job")
            .trim()
            .chars()
            .take(72)
            .collect();
        let verification = job["result"]["provenance"]["verification"]["status"]
            .as_str()
            .unwrap_or("unknown");
        let message = format!(
            "{subject}\n\nWritten by Poundlock job {id}; its checks: {verification}.\n\nPoundlock-Job: {id}\nPoundlock-Verification: {verification}\n",
            id = args.job_id
        );
        let (directory, patch, branch_name, check_only) = (
            directory.to_owned(),
            patch.to_owned(),
            branch.to_owned(),
            args.check_only,
        );
        let committed = tokio::task::spawn_blocking(move || {
            super::super::branch::commit_patch(
                &directory,
                &patch,
                &branch_name,
                &message,
                check_only,
            )
        })
        .await??;
        Ok(match committed {
            None => Outcome::ok(
                format!(
                    "The patch applies to the folder's current commit as branch {branch} ({} file(s)). Nothing was changed.",
                    files.len()
                ),
                json!({"job_id": args.job_id, "applied": false, "applies": true, "branch": branch, "files": files}),
            ),
            Some(done) => Outcome::ok(
                format!(
                    "Committed the patch as branch {} ({}) on top of {}: {} file(s) changed: {}. The working tree, index and current branch are unchanged. Review it with `git show {}`, then merge it or delete it.",
                    done.branch,
                    &done.commit[..12],
                    &done.base[..12],
                    files.len(),
                    files.join(", "),
                    done.branch
                ),
                json!({
                    "job_id": args.job_id, "applied": true, "branch": done.branch,
                    "commit": done.commit, "base": done.base, "files": files,
                }),
            ),
        })
    }
}

/// Runs `git apply` (or `--check`) with the patch on standard input.
async fn git_apply(directory: &Path, patch: &str, check: bool) -> Result<(bool, String)> {
    use tokio::io::AsyncWriteExt;
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        // Never run hooks or attribute filters a folder configures.
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "apply",
            "--whitespace=nowarn",
        ]);
    if check {
        command.arg("--check");
    }
    let mut child = command
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("run git apply")?;
    let mut stdin = child.stdin.take().context("open git apply input")?;
    stdin.write_all(patch.as_bytes()).await?;
    drop(stdin);
    let output = child.wait_with_output().await?;
    Ok((
        output.status.success(),
        tail(&String::from_utf8_lossy(&output.stderr), 2_000),
    ))
}
