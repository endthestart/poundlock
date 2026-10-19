mod args;

use anyhow::{Result, bail};
use args::{Args, Command, Remote};
use clap::Parser;
use poundlock::domain::{CreateEndpoint, CreateModelProfile, CreateProject, SubmitJob};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    poundlock::config::reject_legacy_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("poundlock=info".parse()?),
        )
        .init();
    match Args::parse().command {
        Command::InspectCandidate => print(poundlock::runner::inspect_candidate().await?),
        Command::FetchSource {
            repository,
            revision,
            into,
            copy_to,
        } => {
            let credentials = match (
                std::env::var("POUNDLOCK_GIT_USERNAME"),
                std::env::var("POUNDLOCK_GIT_TOKEN"),
            ) {
                (Ok(username), Ok(token)) => {
                    Some(poundlock::git_source::GitCredentials { username, token })
                }
                _ => None,
            };
            // One line on stderr: the supervisor reports its last line.
            match poundlock::git_source::fetch_source(
                &into,
                &copy_to,
                &repository,
                &revision,
                credentials.as_ref(),
            )
            .await
            {
                Ok(base) => print(serde_json::json!({ "base_revision": base })),
                Err(error) => {
                    eprintln!("{error:#}");
                    std::process::exit(1);
                }
            }
        }
        Command::RunCheck { cwd, command } => {
            poundlock::runner::verification_command(&cwd, &command)?
        }
        Command::WakeRelay { socket } => poundlock::wake::run_relay(&socket).await?,
        Command::NodeInterfaces => print(serde_json::to_value(
            poundlock::node::network::interfaces()?,
        )?),
        Command::Backup { to, data_dir } => {
            print(serde_json::to_value(poundlock::backup::back_up(
                &data_dir.join("poundlock.sqlite3"),
                &to,
                chrono::Utc::now(),
            )?)?)
        }
        Command::Controller => {
            poundlock::server::run(poundlock::config::AppConfig::load_controller()?).await?
        }
        Command::Runner {
            controller,
            token_file,
            projects_dir,
            work_dir,
            result_dir,
            poll_seconds,
            wall_seconds,
            capacity,
            docker_socket,
            worker_runtime,
        } => {
            poundlock::runner::run(poundlock::runner::RunnerOptions {
                controller,
                token_file,
                projects_dir,
                work_dir,
                result_dir,
                poll_seconds,
                wall_seconds,
                capacity,
                docker_socket,
                worker_runtime: (!worker_runtime.is_empty()).then_some(worker_runtime),
            })
            .await?
        }
        Command::Node {
            controller,
            name,
            enrollment_token,
            credential_file,
            config,
            data_dir,
        } => {
            poundlock::node::run(poundlock::node::NodeOptions {
                controller,
                name,
                enrollment_token,
                credential_file,
                config_file: config,
                data_dir: data_dir.unwrap_or_else(poundlock::node::default_data_dir),
            })
            .await?
        }
        Command::Acp {
            controller,
            token,
            token_file,
            execution,
            project_id,
            model_profile_id,
            harness_profile_id,
            workspace_root,
            instruction_file,
        } => {
            poundlock::acp::run(poundlock::acp::AcpOptions {
                controller,
                token: resolve_token(token, token_file.as_deref())?,
                execution,
                project_id,
                model_profile_id,
                harness_profile_id,
                workspace_root,
                instruction_files: instruction_file,
            })
            .await?
        }
        Command::Status(remote) => print(api_get(&remote, "/api/v1/overview").await?),
        Command::EndpointAdd {
            remote,
            name,
            kind,
            base_url,
            api_key,
        } => print(
            api_post(
                &remote,
                "/api/v1/endpoints",
                &CreateEndpoint {
                    name,
                    kind,
                    base_url,
                    api_key,
                    managed_identity: Default::default(),
                },
            )
            .await?,
        ),
        Command::ModelAdd {
            remote,
            name,
            endpoint_id,
            model_id,
            context_tokens,
            max_output_tokens,
            uses,
            wake_policy,
            node_id,
            runtime_profile,
            alias,
        } => print(
            api_post(
                &remote,
                "/api/v1/model-profiles",
                &CreateModelProfile {
                    name,
                    aliases: alias,
                    endpoint_id,
                    model_id,
                    model_label: None,
                    context_tokens,
                    max_output_tokens,
                    uses: Some(uses),
                    workload_class: None,
                    wake_policy,
                    node_id,
                    runtime_profile,
                    idle_seconds: None,
                    data_destination: "unknown".into(),
                    thinking: "model_default".into(),
                    thinking_budget_tokens: None,
                    capabilities: Vec::new(),
                },
            )
            .await?,
        ),
        Command::ProjectAdd {
            remote,
            name,
            root,
            instruction_file,
        } => print(
            api_post(
                &remote,
                "/api/v1/projects",
                &CreateProject {
                    name,
                    roots: root,
                    instruction_files: instruction_file,
                },
            )
            .await?,
        ),
        Command::JobSubmit {
            remote,
            project_id,
            model_profile_id,
            harness_profile_id,
            prompt,
            idempotency_key,
            watch,
        } => {
            let idempotency_key = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
            let job: Value = api_post(
                &remote,
                "/api/v1/jobs",
                &SubmitJob {
                    source: None,
                    idempotency_key: idempotency_key.clone(),
                    policy: Default::default(),
                    project_id,
                    model_profile_id,
                    harness_profile_id,
                    prompt,
                },
            )
            .await?;
            print(job.clone());
            if watch {
                watch_job(&remote, submitted_job_id(&job, &idempotency_key)?).await?;
            }
        }
        Command::JobWatch { remote, id } => watch_job(&remote, &id).await?,
        Command::JobCancel { remote, id } => print(
            api_post(
                &remote,
                &format!("/api/v1/jobs/{id}/cancel"),
                &serde_json::json!({}),
            )
            .await?,
        ),
        Command::NodeToken {
            remote,
            valid_minutes,
        } => print(
            api_post(
                &remote,
                "/api/v1/nodes/enrollment-tokens",
                &serde_json::json!({"valid_minutes":valid_minutes}),
            )
            .await?,
        ),
        Command::NodeWol {
            remote,
            id,
            mac,
            broadcast,
        } => print(
            api_put(
                &remote,
                &format!("/api/v1/nodes/{id}/wol"),
                &serde_json::json!({"mac":mac,"broadcast":broadcast}),
            )
            .await?,
        ),
        Command::NodeCheck {
            config,
            require_identity,
        } => print(poundlock::node::check_config_identity(
            &config,
            require_identity,
        )?),
        Command::NodeEnroll(files) => print(poundlock::node::enroll_once(files.options()).await?),
        Command::NodeDoctor(files) => print(poundlock::node::doctor(files.options()).await?),
        Command::NodePause(files) => {
            print(poundlock::node::maintenance(files.options(), true).await?)
        }
        Command::NodeResume(files) => {
            print(poundlock::node::maintenance(files.options(), false).await?)
        }
        Command::NodeRevoke(files) => print(poundlock::node::revoke(files.options()).await?),
        Command::NodeConfigExample => print!("{}", poundlock::node::example_config()),
    }
    Ok(())
}

/// Validate the submission response before watching it. An incompatible or
/// partial controller response must produce an actionable error, not a panic.
fn submitted_job_id<'a>(job: &'a Value, idempotency_key: &str) -> Result<&'a str> {
    let recovery = format!(
        "the job may already exist; rerun the same submission with \
         --idempotency-key {idempotency_key} to recover it without creating a duplicate"
    );
    let Some(fields) = job.as_object() else {
        bail!("controller returned a non-object job submission response; {recovery}");
    };
    match fields.get("id").and_then(Value::as_str) {
        Some(id)
            if !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) =>
        {
            Ok(id)
        }
        Some(_) => bail!("controller returned an invalid job id; {recovery}"),
        None => bail!("controller job submission response has no string id; {recovery}"),
    }
}

async fn watch_job(remote: &Remote, id: &str) -> Result<()> {
    let mut after = 0_i64;
    loop {
        let events: Vec<Value> =
            api_get(remote, &format!("/api/v1/jobs/{id}/events?after={after}")).await?;
        for event in events {
            after = event["sequence"].as_i64().unwrap_or(after);
            println!(
                "{}  {}",
                event["kind"].as_str().unwrap_or("event"),
                event["message"].as_str().unwrap_or("")
            );
        }
        let job: Value = api_get(remote, &format!("/api/v1/jobs/{id}")).await?;
        if poundlock::domain::is_terminal_job_status(job["status"].as_str().unwrap_or("")) {
            print(job);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn api_get<T: serde::de::DeserializeOwned>(remote: &Remote, path: &str) -> Result<T> {
    api(remote, reqwest::Method::GET, path, None::<&Value>).await
}
async fn api_post<T: serde::de::DeserializeOwned>(
    remote: &Remote,
    path: &str,
    body: &impl serde::Serialize,
) -> Result<T> {
    api(remote, reqwest::Method::POST, path, Some(body)).await
}
async fn api_put<T: serde::de::DeserializeOwned>(
    remote: &Remote,
    path: &str,
    body: &impl serde::Serialize,
) -> Result<T> {
    api(remote, reqwest::Method::PUT, path, Some(body)).await
}
async fn api<T: serde::de::DeserializeOwned>(
    remote: &Remote,
    method: reqwest::Method,
    path: &str,
    body: Option<&impl serde::Serialize>,
) -> Result<T> {
    let mut request = reqwest::Client::new()
        .request(
            method,
            format!("{}{}", remote.controller.trim_end_matches('/'), path),
        )
        .bearer_auth(&remote.token);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        bail!(
            "controller returned {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn print(value: Value) {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

fn resolve_token(token: Option<String>, token_file: Option<&std::path::Path>) -> Result<String> {
    match (token, token_file) {
        (Some(token), _) => Ok(token),
        (None, Some(path)) => poundlock::config::load_token(path),
        (None, None) => bail!("--token or --token-file is required"),
    }
}

#[cfg(test)]
mod tests {
    use super::submitted_job_id;
    use serde_json::json;

    #[test]
    fn watch_accepts_a_submitted_job_id() {
        let job = json!({"id":"0b9f5c1e-7d2a-4c1f-9d8e-1a2b3c4d5e6f","status":"queued"});
        assert_eq!(
            submitted_job_id(&job, "key-1").unwrap(),
            "0b9f5c1e-7d2a-4c1f-9d8e-1a2b3c4d5e6f"
        );
    }

    #[test]
    fn watch_rejects_responses_without_a_usable_job_id() {
        for (response, expected) in [
            (json!({"status":"queued"}), "no string id"),
            (json!({"id":42}), "no string id"),
            (json!({"id":null}), "no string id"),
            (json!({"id":""}), "invalid job id"),
            (json!({"id":"../admin"}), "invalid job id"),
            (json!(["queued"]), "non-object job submission response"),
            (json!("ok"), "non-object job submission response"),
        ] {
            let error = submitted_job_id(&response, "key-1")
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{response}: {error}");
            assert!(
                error.contains("--idempotency-key key-1"),
                "error must say how to recover: {error}"
            );
        }
    }
}
