use crate::config::load_token;
use crate::domain::{RunnerPackage, RunnerResult};
use crate::git::Workspace;
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "runner_docker.rs"]
mod docker;
#[path = "runner_egress.rs"]
mod egress;
#[path = "runner_proxy.rs"]
mod proxy;
#[path = "runner_sandbox.rs"]
mod sandbox;
#[path = "runner_verify.rs"]
mod verify;
pub use verify::{inspect_candidate, verification_command};

#[path = "runner_delivery.rs"]
mod delivery;

/// The controller already recorded this job's outcome. Not a delivery
/// failure: there is nothing left to deliver.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct AlreadySettled(pub String);
use delivery::Report;

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;

#[derive(Clone)]
pub struct RunnerOptions {
    pub controller: String,
    pub token_file: PathBuf,
    pub projects_dir: PathBuf,
    pub work_dir: PathBuf,
    pub result_dir: PathBuf,
    pub poll_seconds: u64,
    pub wall_seconds: u64,
    pub capacity: u16,
    pub docker_socket: PathBuf,
    /// A Docker runtime for job containers, such as `runsc` (gVisor).
    pub worker_runtime: Option<String>,
}

/// The memory the runner plans for: `POUNDLOCK_RUNNER_MEMORY_BYTES` when set,
/// otherwise MemTotal from /proc/meminfo, the host's memory even in a container.
fn host_memory_bytes() -> Option<u64> {
    if let Ok(value) = std::env::var("POUNDLOCK_RUNNER_MEMORY_BYTES")
        && let Ok(bytes) = value.trim().parse()
    {
        return Some(bytes);
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

pub async fn run(options: RunnerOptions) -> Result<()> {
    anyhow::ensure!(
        (1..=16).contains(&options.capacity),
        "runner capacity must be between 1 and 16"
    );
    tokio::fs::create_dir_all(&options.work_dir).await?;
    tokio::fs::create_dir_all(&options.result_dir).await?;
    let identity = options.result_dir.join("supervisor-id");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(options.result_dir.join("supervisor.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        anyhow::ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another supervisor owns this result directory"
        );
    }
    if !identity.exists() {
        crate::crypto::write_private(&identity, &uuid::Uuid::new_v4().to_string())?;
    }
    let owner = uuid::Uuid::parse_str(std::fs::read_to_string(identity)?.trim())?.to_string();
    let docker = docker::Docker::connect(
        &options.docker_socket,
        &std::env::var("HOSTNAME").unwrap_or_default(),
        owner,
        &tokio::fs::canonicalize(&options.result_dir).await?,
        options.worker_runtime.clone(),
    )
    .await?;
    docker.cleanup(None).await?;
    delivery::recover(&options.result_dir, &options.work_dir)?;
    let proxy = proxy::start(&options.controller).await?;
    let egress = egress::start(docker.egress.clone()).await?;
    let token = wait_for_token(&options.token_file).await?;
    let client = RunnerClient::new(options.controller.clone(), token)?;
    let capacity =
        crate::verifier_toolchain::capacity_for_memory(options.capacity, host_memory_bytes());
    if capacity < options.capacity {
        tracing::warn!(
            requested = options.capacity,
            capacity,
            "runner capacity lowered to what fits in half of this host's memory"
        );
    }
    tracing::info!(controller=%options.controller, capacity, "runner connected");
    let mut tasks = tokio::task::JoinSet::new();
    let mut active = HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(options.poll_seconds.max(1)));
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! { result=tokio::signal::ctrl_c()=>result?, _=term.recv()=>{} }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        Ok::<_, anyhow::Error>(())
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! { _=tick.tick()=>{}, result=&mut shutdown=>{ result?; break; } }
        while let Some(result) = tasks.try_join_next() {
            active.remove(&result.context("worker task stopped unexpectedly")??);
        }
        anyhow::ensure!(!proxy.is_finished(), "worker inference listener stopped");
        anyhow::ensure!(!egress.is_finished(), "job egress proxy stopped");
        // Reports written before cleanup must not be delivered while the worker
        // is still running. Completed jobs can be delivered while others continue.
        delivery::deliver_pending(&client, &options.result_dir, &active).await?;
        if active.len() >= usize::from(capacity) {
            continue;
        }
        match client.claim(options.wall_seconds).await {
            Ok(Some(package)) => {
                let id = package.job.id.clone();
                delivery::begin(&options.result_dir, &package.job)?;
                active.insert(id.clone());
                let client = client.clone();
                let options = options.clone();
                let docker = docker.clone();
                tasks.spawn(async move {
                    let attempt = package.job.attempt_id.clone();
                    if let Err(error) = execute(&client, &options, &docker, package).await {
                        tracing::error!(job_id=%id, %error, "attempt failed");
                        // A failed cleanup stops this supervisor before it can
                        // acknowledge completion or claim any replacement work.
                        docker.cleanup(Some(&attempt)).await?;
                        let directory = options.work_dir.join(&attempt);
                        if directory.exists() {
                            tokio::fs::remove_dir_all(directory).await?;
                        }
                        let payload = json!({"layer":"runner","code":"attempt_failed",
                        "message":error.to_string(),"retry_safe":false});
                        let retained = options.result_dir.join("outbox").join(format!("{id}.json"));
                        let mut report = if retained.exists() {
                            serde_json::from_reader::<_, Report>(std::fs::File::open(retained)?)?
                        } else {
                            Report {
                                job_id: id.clone(),
                                result: None,
                                error: Some(payload.clone()),
                                cancelled: false,
                            }
                        };
                        report.confirm_recovered_cleanup(payload, false);
                        report.save(&options.result_dir)?;
                    }
                    delivery::ended(&options.result_dir, &attempt)?;
                    Ok::<_, anyhow::Error>(id)
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "runner poll failed");
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    docker.cleanup(None).await?;
    delivery::recover(&options.result_dir, &options.work_dir)?;
    delivery::deliver_pending(&client, &options.result_dir, &HashSet::new()).await?;
    proxy.abort();
    egress.abort();
    Ok(())
}

async fn execute(
    client: &RunnerClient,
    options: &RunnerOptions,
    docker: &docker::Docker,
    mut package: RunnerPackage,
) -> Result<()> {
    if package.endpoint.endpoint.base_url.starts_with("/job/v1/") {
        package.endpoint.endpoint.base_url = format!(
            "{}{}",
            options.controller.trim_end_matches('/'),
            package.endpoint.endpoint.base_url
        );
    }
    let id = package.job.id.clone();
    let attempt_dir = options.work_dir.join(&package.job.attempt_id);
    // A repeated claim must never erase evidence from an earlier execution.
    tokio::fs::create_dir(&attempt_dir).await?;
    client
        .event(
            &id,
            "workspace_preparing",
            "Preparing the working files.",
            json!({}),
        )
        .await?;
    let expires = tokio::time::Instant::now() + Duration::from_secs(options.wall_seconds);
    let workspace = {
        let preparation = async {
            let workspace = match &package.source {
                Some(summary) => {
                    let source = client.source(&id).await?;
                    source.validate()?;
                    if source.summary() != *summary {
                        bail!("Source does not match the accepted job contract");
                    }
                    Workspace::from_source(&attempt_dir, &source).await?
                }
                None => {
                    Workspace::create(&options.projects_dir, &attempt_dir, &package.project.roots)
                        .await?
                }
            };
            Ok::<_, anyhow::Error>(workspace)
        };
        tokio::pin!(preparation);
        let mut keepalive = tokio::time::interval(Duration::from_secs(10));
        let deadline = tokio::time::sleep_until(expires);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => bail!("Workspace preparation exceeded the job wall budget"),
                workspace = &mut preparation => break workspace?,
                _ = keepalive.tick() => {
                    if tokio::time::timeout_at(expires, client.heartbeat(&id)).await?? { bail!("Workspace preparation cancelled or lease lost"); }
                }
            }
        }
    };
    let outcome = async {
        client
            .event(
                &id,
                "harness_starting",
                "Starting Pi.",
                json!({"harness":package.harness,"model":package.model.name}),
            )
            .await?;
        sandbox::execute(
            docker,
            client,
            &package,
            &attempt_dir,
            &workspace,
            expires,
            &options.result_dir,
        )
        .await
    }
    .await;
    let mut error = None;
    let mut cancelled = false;
    let mut verification =
        crate::job_policy::VerificationReport::not_run("Execution did not finish.");
    let mut history = Vec::new();
    let mut consumed = Value::Null;
    let (output, patches) = match outcome {
        Ok(outcome) => {
            verification = outcome.verification;
            history = outcome.history;
            // What the attempt actually spent, read before `bounded_output`
            // takes the buffers. A consumer can see *that* a job died on the
            // output budget but never how close a successful one came, so
            // there is no way to size a task against the limit that binds.
            consumed = json!({
                "stdout": outcome.agent.stdout.len(),
                "stderr": outcome.agent.stderr.len(),
                "limit": crate::job_policy::effective_output_bytes_per_stream(
                    package.job.policy.max_repairs,
                ),
                "stdout_truncated": outcome.agent.stdout_truncated,
                "stderr_truncated": outcome.agent.stderr_truncated,
                "omitted_stream_events": outcome.agent.omitted_stream_events,
                "compacted_events": outcome.agent.compacted_events,
            });
            let output = crate::harness::bounded_output(
                outcome.agent.stdout,
                outcome.agent.stderr,
                4 * 1024 * 1024,
            );
            // A truncated capture never stopped the agent process (see
            // `Docker::exec`), so its exit code and any terminal message it
            // emitted are both real. The code still distinguishes a cutoff
            // that landed after the agent's turn settled from one that landed
            // mid-turn — but both now carry verification evidence, so the
            // code describes how the run ended rather than whether the patch
            // was checked.
            // Only the agent's own stdout: a stderr overflow does not cut the
            // transcript, and calling it an output-limit failure attributed a
            // noisy dependency's warnings to the agent's work.
            let code = if outcome.agent.stdout_truncated {
                Some(if crate::harness::agent_turn_settled(&output) {
                    "output_limit_after_turn"
                } else {
                    "output_limit_mid_turn"
                })
            } else if outcome.agent.code != 0 {
                Some("harness_failed")
            } else {
                None
            };
            // Say which cutoff the checks ran after, so a reader can never
            // take `passed` for "the agent finished the task".
            if let Some(code) = code.filter(|_| verification.status != "unverified") {
                verification.checked_after = Some(code.to_owned());
            }
            if let Some(failure) = outcome.failure {
                cancelled = failure
                    .downcast_ref::<Stopped>()
                    .is_some_and(|v| v.cancelled);
                if failure
                    .downcast_ref::<Stopped>()
                    .is_some_and(|v| v.code == "wall_budget_exhausted")
                {
                    // The agent's own turn may have already settled before the
                    // wall clock ran out (see `runner_sandbox::execute`, which
                    // ran verification in that case). Distinguish that from a
                    // mid-turn cutoff the same way the output-budget arm does.
                    let (code, message) =
                        wall_budget_outcome(crate::harness::agent_turn_settled(&output));
                    error = Some(
                        json!({"layer":"runner","code":code,"message":message,"retry_safe":false}),
                    );
                } else {
                    let code = failure.downcast_ref::<Stopped>().map_or_else(
                        || {
                            verification
                                .failure_code
                                .as_deref()
                                .unwrap_or("harness_failed")
                        },
                        |v| v.code,
                    );
                    error = Some(
                        json!({"layer":"runner","code":code,"message":failure.to_string(),"retry_safe":false}),
                    );
                }
            } else if let Some(code) = code {
                let message = if code == "output_limit_after_turn" {
                    "The agent's final turn finished before its output budget was exhausted; review the retained patch and verification evidence."
                } else {
                    "Attempt stopped; inspect retained output."
                };
                error = Some(
                    json!({"layer":"runner","code":code,"message":message,"retry_safe":false}),
                );
            }
            if error.is_none() {
                error = crate::harness::pi_failure(&output);
            }
            (output, outcome.patches)
        }
        Err(failure) => {
            cancelled = failure
                .downcast_ref::<Stopped>()
                .is_some_and(|v| v.cancelled);
            if failure
                .downcast_ref::<Stopped>()
                .is_some_and(|v| v.code == "wall_budget_exhausted")
            {
                // No agent turn ran at all (the deadline hit during workspace
                // preparation), so there is trivially no settled evidence.
                let (code, message) = wall_budget_outcome(false);
                error = Some(
                    json!({"layer":"runner","code":code,"message":message,"retry_safe":false}),
                );
            } else {
                let code = failure
                    .downcast_ref::<Stopped>()
                    .map_or("harness_failed", |v| v.code);
                error = Some(json!({"layer":"runner","code":code,
                    "message":failure.to_string(),"retry_safe":false}));
            }
            (String::new(), Ok(Vec::new()))
        }
    };
    let patches = match patches {
        Ok(patches) => patches,
        Err(failure) => {
            if let Some(error) = error.as_mut() {
                error["patch_collection_error"] = json!(failure.to_string());
            } else {
                error = Some(
                    json!({"layer":"workspace","code":"patch_failed","message":failure.to_string()}),
                );
            }
            Vec::new()
        }
    };
    let patch_bytes = patches.iter().map(|patch| patch.patch.len()).sum::<usize>();
    if patch_bytes == 0
        && error.is_none()
        && verification.failure_code.is_none()
        && package
            .source
            .as_ref()
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str)
            != Some("empty")
    {
        error = Some(json!({"layer":"verification","code":"no_changes",
            "message":"Harness exited without producing a code change."}));
    }
    if error.is_none()
        && let Some(code) = &verification.failure_code
    {
        let message = if package.job.policy.max_repairs > 0
            && history.len() == usize::from(package.job.policy.max_repairs)
            && matches!(
                code.as_str(),
                "verification_failed" | "protected_paths_changed"
            ) {
            format!(
                "Verification still failed after {} repair attempt(s). Review the check output and retained patches.",
                history.len()
            )
        } else {
            verification.reason.clone()
        };
        error = Some(
            json!({"layer":"verification","code":code,"message":message,"retry_safe":false,"repairs_used":history.len(),"repair_limit":package.job.policy.max_repairs}),
        );
    }
    let result = RunnerResult {
        patches,
        output,
        provenance: provenance(&package, docker, &verification, &history, consumed),
        cleanup: json!({"worktrees_removed":false,"attempt_directory_removed":false}),
    };
    // Persist output and patches before deleting any workspace, even on failure.
    let mut report = Report {
        job_id: id,
        result: Some(result),
        cancelled,
        error: Some(json!({"layer":"cleanup","code":"cleanup_interrupted",
            "message":"Runner stopped before confirming workspace cleanup."})),
    };
    report.save(&options.result_dir)?;
    docker.cleanup(Some(&package.job.attempt_id)).await?;
    let cleanup_failures = workspace.cleanup().await;
    let removed = if cleanup_failures.is_empty() {
        tokio::fs::remove_dir_all(&attempt_dir).await.is_ok()
    } else {
        false
    };
    report.result.as_mut().unwrap().cleanup = json!({
        "containers_removed":true,"worktrees_removed":cleanup_failures.is_empty(), "attempt_directory_removed":removed,
        "failures":cleanup_failures});
    if !removed {
        // Keep the attempt's own failure beside the cleanup one; it is usually the cause.
        let mut cleanup = json!({"layer":"cleanup","code":"cleanup_failed",
            "message":"Workspace removal failed; inspect cleanup evidence."});
        if let Some(original) = error.take() {
            cleanup["attempt_error"] = original;
        }
        error = Some(cleanup);
    }
    report.error = error;
    report.save(&options.result_dir)?;
    anyhow::ensure!(
        removed,
        "Workspace removal failed; retry cleanup before accepting more work"
    );
    Ok(())
}

fn provenance(
    package: &RunnerPackage,
    docker: &docker::Docker,
    verification: &crate::job_policy::VerificationReport,
    history: &[Value],
    output_bytes: Value,
) -> Value {
    json!({"model_profile_id":package.model.id,"model":package.model.model_id,
        "serving_identity":package.serving_identity,
        "endpoint":package.endpoint.endpoint.name,"endpoint_kind":package.endpoint.endpoint.kind,
        "harness":package.harness.name,"harness_version":package.harness.version,
        "attempt_id":package.job.attempt_id,"project_roots":package.project.roots,
        "source":package.source,
        "execution":{"isolation":"docker","image":docker.image,"network":package.job.policy.network,
            "output_bytes":output_bytes},
        "toolchain":crate::verifier_toolchain::provenance_toolchain(
            verification.observed_python.as_deref(),
            verification.observed_uv.as_deref(),
        ),
        "policy":package.job.policy,"verification":verification,"previous_attempts":history,"attempt_count":history.len()+1})
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct Stopped {
    code: &'static str,
    message: &'static str,
    cancelled: bool,
}

/// Maps a wall-deadline cutoff to a code and message that distinguish a
/// cutoff landing after the agent's own turn had already settled (the
/// retained patch reflects finished work, and `runner_sandbox::execute`
/// already ran verification against it) from one that cut the agent off
/// mid-turn (no such evidence exists). Mirrors
/// `output_limit_after_turn`/`output_limit_mid_turn`.
fn wall_budget_outcome(turn_settled: bool) -> (&'static str, &'static str) {
    if turn_settled {
        (
            "wall_budget_after_turn",
            "The job's wall time budget was exhausted after the agent's final turn finished; review the retained patch and verification evidence.",
        )
    } else {
        (
            "wall_budget_mid_turn",
            "The job's wall time budget was exhausted while the agent was still working; inspect retained output.",
        )
    }
}

/// How often the supervisor renews a claim while work is running.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How long the controller may stay unreachable before a running attempt
/// gives up. Shorter than the 120 s lease (`store/runner.rs`), so this runner
/// stops before the controller can hand the job to another one.
const LEASE_GRACE: Duration = Duration::from_secs(90);

async fn monitor<T>(
    client: &RunnerClient,
    job: &str,
    expires: tokio::time::Instant,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let lease = async {
        // The lease itself lasts 120 s, so a single failed heartbeat says
        // nothing about whether this job is still ours — and killing the
        // attempt on the first one meant a ten-second controller restart
        // ended every job in flight. Keep trying until we are close enough to
        // the lease expiry that the controller is about to reclaim the job
        // anyway; stopping first is what keeps two runners off one job.
        let mut unreachable_for = Duration::ZERO;
        loop {
            match client.heartbeat(job).await {
                Ok(false) => unreachable_for = Duration::ZERO,
                Ok(true) => {
                    return Stopped {
                        code: "cancelled_or_lease_lost",
                        message: "The job was cancelled or its lease ended.",
                        cancelled: true,
                    };
                }
                Err(_) => {
                    unreachable_for += HEARTBEAT_INTERVAL;
                    if unreachable_for >= LEASE_GRACE {
                        return Stopped {
                            code: "controller_unreachable",
                            message: "The controller could not confirm the job lease.",
                            cancelled: false,
                        };
                    }
                }
            }
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        }
    };
    tokio::select! {
        result = work => result,
        stopped = lease => Err(stopped.into()),
        _ = tokio::time::sleep_until(expires) => Err(Stopped { code: "wall_budget_exhausted", message: "The job exhausted its wall time budget.", cancelled: false }.into()),
    }
}

async fn wait_for_token(path: &Path) -> Result<String> {
    for _ in 0..120 {
        if path.exists() {
            return load_token(path);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("runner token did not appear at {}", path.display())
}

#[derive(Clone)]
struct RunnerClient {
    base: String,
    token: String,
    client: Client,
}

impl RunnerClient {
    fn new(base: String, token: String) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            token,
            client: Client::builder().timeout(Duration::from_secs(30)).build()?,
        })
    }
    async fn claim(&self, wall_seconds: u64) -> Result<Option<RunnerPackage>> {
        self.post_json("/runner/v1/claim", &json!({"wall_seconds": wall_seconds}))
            .await
    }
    async fn source(&self, id: &str) -> Result<crate::source::JobSource> {
        Ok(self
            .client
            .get(format!("{}/runner/v1/jobs/{id}/source", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    async fn heartbeat(&self, id: &str) -> Result<bool> {
        let value: Value = self
            .post_json(&format!("/runner/v1/jobs/{id}/heartbeat"), &json!({}))
            .await?;
        value
            .get("cancel_requested")
            .and_then(Value::as_bool)
            .context("controller heartbeat omitted its lease decision")
    }
    async fn event(&self, id: &str, kind: &str, message: &str, payload: Value) -> Result<()> {
        let _: Value = self
            .post_json(
                &format!("/runner/v1/jobs/{id}/events"),
                &json!({"kind":kind,"message":message,"payload":payload}),
            )
            .await?;
        Ok(())
    }
    async fn complete(&self, id: &str, result: &RunnerResult) -> Result<()> {
        let acknowledgement: Value = self
            .post_json(&format!("/runner/v1/jobs/{id}/complete"), result)
            .await?;
        if acknowledgement.get("accepted") != Some(&json!(true)) {
            bail!("controller did not acknowledge result acceptance");
        }
        Ok(())
    }
    async fn fail_with_result(
        &self,
        id: &str,
        error: &Value,
        cancelled: bool,
        result: Option<&RunnerResult>,
    ) -> Result<()> {
        let acknowledgement: Value = self
            .post_json(
                &format!("/runner/v1/jobs/{id}/fail"),
                &json!({"error":error,"cancelled":cancelled,"result":result}),
            )
            .await?;
        if acknowledgement.get("accepted") != Some(&json!(true)) {
            bail!("controller did not acknowledge failure acceptance");
        }
        Ok(())
    }
    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        let response = self
            .client
            .post(format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if status == reqwest::StatusCode::CONFLICT {
            // The controller has already recorded an outcome for this job.
            // Retrying cannot change that, so the report is delivered as far
            // as this runner is concerned and leaves the outbox.
            return Err(AlreadySettled(String::from_utf8_lossy(&bytes).into_owned()).into());
        }
        if !status.is_success() {
            bail!(
                "controller returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}
