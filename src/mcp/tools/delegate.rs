//! Turning a task brief into a durable job, from a folder or a pushed commit.

use super::*;

impl Tools {
    pub(super) async fn delegate(&self, args: DelegateArgs) -> Result<Outcome> {
        args.brief.validate()?;
        if !args.brief.policy().budget.is_empty() {
            // An older controller would refuse the whole policy for one field.
            let capabilities = self.controller.get("api/v1/capabilities").await?;
            let supported = capabilities["features"]
                .as_array()
                .is_some_and(|features| features.iter().any(|item| item == "job_budget"));
            ensure!(
                supported,
                "this Poundlock controller does not support job budgets; leave out budget_minutes and budget_usd, or update it"
            );
        }
        let directory = self.workspace.resolve(&args.directory)?;
        let options = self.options().await?;
        let model = self.find_model(&options, args.model.trim())?;
        let model_id = model["id"]
            .as_str()
            .context("the model has no id")?
            .to_owned();
        let model_name = model["name"].as_str().unwrap_or(&model_id).to_owned();
        match args.source.as_deref() {
            None | Some("folder") => {}
            Some("git") => {
                return self
                    .delegate_git(args, &directory, &options, model_id, model_name)
                    .await;
            }
            Some(other) => bail!("source must be folder or git, not {other}"),
        }
        let snapshot = {
            let directory = directory.clone();
            tokio::task::spawn_blocking(move || snapshot::collect(&directory)).await??
        };
        let prompt = args.brief.prompt();
        let policy = args.brief.policy();
        let upload = json!({
            "directory": directory.display().to_string(),
            "files": snapshot.files.len(),
            "bytes": snapshot.bytes,
            "chosen_by": if snapshot.from_git { "git" } else { "folder_walk" },
            "credential_files_left_out": snapshot.secrets,
            "skipped": snapshot.skipped.iter().map(|(path, reason)| json!({"path": path, "reason": reason})).collect::<Vec<_>>(),
        });
        if args.dry_run {
            let listed: Vec<&str> = snapshot
                .files
                .iter()
                .take(500)
                .map(|f| f.path.as_str())
                .collect();
            return Ok(Outcome::ok(
                format!(
                    "Dry run: would upload {} files ({} bytes) from {} to run on {}. Left out {} credential file(s). Nothing was sent.",
                    snapshot.files.len(),
                    snapshot.bytes,
                    directory.display(),
                    model_name,
                    snapshot.secrets.len()
                ),
                json!({
                    "dry_run": true,
                    "upload": upload,
                    "paths": listed,
                    "paths_truncated": snapshot.files.len() > 500,
                    "model": {"id": model_id, "name": model_name},
                    "prompt": prompt,
                    "policy": policy,
                }),
            ));
        }
        let idempotency_key = match args.idempotency_key.filter(|key| !key.trim().is_empty()) {
            Some(key) => key,
            None => {
                let mut digest = Sha256::new();
                digest.update(directory.to_string_lossy().as_bytes());
                digest.update([0]);
                digest.update(model_id.as_bytes());
                digest.update([0]);
                digest.update(serde_json::to_vec(&args.brief)?);
                digest.update([0]);
                digest.update(snapshot.digest.as_bytes());
                format!("mcp-{}", &crate::crypto::to_hex(&digest.finalize())[..40])
            }
        };
        let submission = json!({
            "idempotency_key": idempotency_key,
            "model_profile_id": model_id,
            "harness_profile_id": "pi",
            "prompt": prompt,
            "source": {"kind": "snapshot", "files": snapshot.files},
            "policy": policy,
        });
        let job = self.controller.post("api/v1/jobs", &submission).await?;
        let id = job["id"].as_str().unwrap_or_default().to_owned();
        Ok(Outcome::ok(
            format!(
                "Started job {id} on {model_name} with {} files from {}. Status: {}. Follow it with wait_for_job.",
                snapshot.files.len(),
                directory.display(),
                job["status"].as_str().unwrap_or("submitted"),
            ),
            json!({
                "job_id": id,
                "status": job["status"],
                "idempotency_key": idempotency_key,
                "model": {"id": model_id, "name": model_name},
                "upload": upload,
                "checks": args.brief.acceptance_checks,
                "max_repairs": policy.max_repairs,
            }),
        ))
    }

    pub(super) async fn delegate_git(
        &self,
        args: DelegateArgs,
        directory: &Path,
        options: &Value,
        model_id: String,
        model_name: String,
    ) -> Result<Outcome> {
        let source = {
            let directory = directory.to_path_buf();
            tokio::task::spawn_blocking(move || super::super::gitsource::resolve(&directory))
                .await??
        };
        let approved: Vec<&str> = options["git_hosts"]
            .as_array()
            .map(|hosts| hosts.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        ensure!(
            approved.contains(&source.host.as_str()),
            "{} is not a repository host this controller approves ({}); upload the folder instead",
            source.host,
            approved.join(", ")
        );
        let prompt = args.brief.prompt();
        let policy = args.brief.policy();
        let described = json!({
            "repository": source.repository,
            "revision": source.revision,
            "credentials": self.git_credentials.is_some(),
        });
        if args.dry_run {
            return Ok(Outcome::ok(
                format!(
                    "Dry run: the job would fetch {} at {} to run on {}. Nothing was sent.",
                    source.repository,
                    &source.revision[..12],
                    model_name
                ),
                json!({"dry_run": true, "git": described, "model": {"id": model_id, "name": model_name}, "prompt": prompt, "policy": policy}),
            ));
        }
        let idempotency_key = match args.idempotency_key.filter(|key| !key.trim().is_empty()) {
            Some(key) => key,
            None => {
                let mut digest = Sha256::new();
                digest.update(source.repository.as_bytes());
                digest.update([0]);
                digest.update(source.revision.as_bytes());
                digest.update([0]);
                digest.update(model_id.as_bytes());
                digest.update([0]);
                digest.update(serde_json::to_vec(&args.brief)?);
                format!("mcp-{}", &crate::crypto::to_hex(&digest.finalize())[..40])
            }
        };
        let mut git =
            json!({"kind": "git", "repository": source.repository, "revision": source.revision});
        if let Some(credentials) = &self.git_credentials {
            git["credentials"] =
                json!({"username": credentials.username, "token": credentials.token});
        }
        let job = self
            .controller
            .post(
                "api/v1/jobs",
                &json!({
                    "idempotency_key": idempotency_key,
                    "model_profile_id": model_id,
                    "harness_profile_id": "pi",
                    "prompt": prompt,
                    "source": git,
                    "policy": policy,
                }),
            )
            .await?;
        let id = job["id"].as_str().unwrap_or_default().to_owned();
        Ok(Outcome::ok(
            format!(
                "Started job {id} on {model_name} from {} at {}. Status: {}. Follow it with wait_for_job; apply its patch to {}.",
                source.repository,
                &source.revision[..12],
                job["status"].as_str().unwrap_or("submitted"),
                source.top.display(),
            ),
            json!({
                "job_id": id,
                "status": job["status"],
                "idempotency_key": idempotency_key,
                "model": {"id": model_id, "name": model_name},
                "git": described,
                "checks": args.brief.acceptance_checks,
                "max_repairs": policy.max_repairs,
            }),
        ))
    }
}
