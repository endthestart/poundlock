use crate::acp::AcpOptions;
use crate::domain::{Event, Job, SubmitJob};
use crate::harness::{PiLaunch, PiMode, pi_launch};
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, bail};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use uuid::Uuid;

pub(crate) struct AcpSession {
    pub cwd: PathBuf,
    pub job_id: Option<String>,
}

pub(crate) struct AcpApi {
    base: String,
    token: String,
    pub options: AcpOptions,
    client: Client,
}

impl AcpApi {
    pub(crate) fn new(options: AcpOptions) -> Result<Self> {
        Ok(Self {
            base: options.controller.trim_end_matches('/').into(),
            token: options.token.clone(),
            options,
            client: Client::builder().timeout(Duration::from_secs(30)).build()?,
        })
    }

    pub(crate) async fn cancel(&self, id: &str) -> Result<Value> {
        self.post(&format!("/api/v1/jobs/{id}/cancel"), &serde_json::json!({}))
            .await
    }

    async fn submit(&self, prompt: &str) -> Result<Job> {
        let project_id = self
            .options
            .project_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("delegated ACP requires --project-id"))?;
        self.post(
            "/api/v1/jobs",
            &SubmitJob {
                source: None,
                idempotency_key: Uuid::new_v4().to_string(),
                policy: Default::default(),
                project_id,
                model_profile_id: self.options.model_profile_id.clone(),
                harness_profile_id: self.options.harness_profile_id.clone(),
                prompt: prompt.into(),
            },
        )
        .await
    }

    async fn job(&self, id: &str) -> Result<Job> {
        self.get(&format!("/api/v1/jobs/{id}")).await
    }

    async fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        self.get(&format!("/api/v1/jobs/{id}/events?after={after}"))
            .await
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.decode(
            self.client
                .get(format!("{}{}", self.base, path))
                .bearer_auth(&self.token)
                .send()
                .await?,
        )
        .await
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        self.decode(
            self.client
                .post(format!("{}{}", self.base, path))
                .bearer_auth(&self.token)
                .json(body)
                .send()
                .await?,
        )
        .await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
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
}

pub(crate) async fn drive_prompt(
    api: &AcpApi,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    prompt: &str,
    sessions: &Mutex<HashMap<String, AcpSession>>,
    session: &str,
) -> Result<()> {
    if api.options.execution.trim() == "delegated" {
        drive_job(api, connection, session_id, prompt, sessions, session).await
    } else {
        drive_attached(api, connection, session_id, prompt, sessions, session).await
    }
}

async fn drive_job(
    api: &AcpApi,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    prompt: &str,
    sessions: &Mutex<HashMap<String, AcpSession>>,
    session: &str,
) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("the prompt contains no text");
    }
    let job = api.submit(prompt).await?;
    if let Some(state) = sessions.lock().await.get_mut(session) {
        state.job_id = Some(job.id.clone());
    }
    notify(
        connection,
        session_id.clone(),
        &format!(
            "Accepted durable job `{}`. Files stay in the host worktree.",
            job.id
        ),
    )?;
    let mut after = 0_i64;
    loop {
        for event in api.events(&job.id, after).await? {
            after = event.sequence;
            notify(
                connection,
                session_id.clone(),
                &format!("**{}** — {}", event.kind, event.message),
            )?;
        }
        let current = api.job(&job.id).await?;
        match current.status.as_str() {
            "completed" => {
                let patches = current
                    .result
                    .unwrap_or_default()
                    .get("patches")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for patch in patches {
                    let root = patch.get("root").and_then(Value::as_str).unwrap_or("root");
                    let diff = patch.get("patch").and_then(Value::as_str).unwrap_or("");
                    notify(
                        connection,
                        session_id.clone(),
                        &format!("### Patch: {root}\n\n```diff\n{diff}\n```"),
                    )?;
                }
                return Ok(());
            }
            "failed" | "cancelled" => bail!(
                "job ended as {}: {}",
                current.status,
                current.error.unwrap_or_default()
            ),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn drive_attached(
    api: &AcpApi,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    prompt: &str,
    sessions: &Mutex<HashMap<String, AcpSession>>,
    session: &str,
) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("the prompt contains no text");
    }
    let cwd = sessions
        .lock()
        .await
        .get(session)
        .map(|state| state.cwd.clone())
        .or_else(|| api.options.workspace_root.clone())
        .ok_or_else(|| anyhow::anyhow!("ACP session has no working directory"))?;
    if !cwd.is_absolute() {
        bail!("attached ACP requires an absolute working directory from the editor");
    }
    notify(
        connection,
        session_id.clone(),
        &format!(
            "Attached local turn in `{}`. Inference uses poundlock `/v1`; this tree is not uploaded.",
            cwd.display()
        ),
    )?;
    let (model_id, context_tokens, max_output_tokens) =
        advertised_model(api, &api.options.model_profile_id).await?;
    let attempt_dir = tempfile::Builder::new()
        .prefix("poundlock-acp-")
        .tempdir()?;
    let command = pi_launch(
        &PiLaunch {
            mode: PiMode::Attached,
            prompt,
            model_id: &model_id,
            context_tokens,
            max_output_tokens,
            base_url: &format!("{}/v1", api.base),
            api_key: &api.token,
            extra_note: "",
            instruction_files: &api.options.instruction_files,
        },
        attempt_dir.path(),
        &cwd,
    )
    .await?;
    run_attached(command, |line| {
        notify(connection, session_id.clone(), &visible_pi_line(line))?;
        Ok(())
    })
    .await
}

async fn run_attached(
    mut command: tokio::process::Command,
    mut on_line: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut child = command.spawn()?;
    let group = crate::process::ProcessGroup::new(&child)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Pi stdout was not captured"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Pi stderr was not captured"))?;
    let output = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                on_line(&line)?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let errors = async {
        // Drain concurrently so a verbose tool cannot block on a full stderr pipe.
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        Ok::<_, anyhow::Error>(())
    };
    let execution = async {
        let status = child.wait().await?;
        // A tool may outlive Pi and retain its pipes. End the owned group first.
        group.terminate();
        anyhow::ensure!(status.success(), "attached Pi exited with {status}");
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::try_join!(output, errors, execution);
    group.terminate();
    let _ = child.kill().await;
    result?;
    Ok(())
}

pub(crate) fn notify(
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    message: &str,
) -> agent_client_protocol::Result<()> {
    connection.send_notification(acp::SessionNotification::new(
        session_id,
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(message),
        ))),
    ))
}

async fn advertised_model(api: &AcpApi, wanted: &str) -> Result<(String, u64, u64)> {
    let body: Value = api.get("/v1/models").await?;
    for row in body
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
        let names = [
            id,
            row.pointer("/poundlock/canonical_name")
                .and_then(Value::as_str)
                .unwrap_or(id),
            row.pointer("/poundlock/profile_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ];
        if !names.contains(&wanted) {
            continue;
        }
        let context = row
            .get("context_tokens")
            .and_then(Value::as_u64)
            .or_else(|| row.pointer("/meta/n_ctx").and_then(Value::as_u64))
            .unwrap_or(8192);
        let max_output = row
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                row.pointer("/poundlock/max_output_tokens")
                    .and_then(Value::as_u64)
            })
            .unwrap_or(context.min(8192));
        return Ok((id.to_owned(), context, max_output));
    }
    Ok((wanted.to_owned(), 8192, 4096))
}

fn visible_pi_line(line: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        for key in ["text", "delta", "content", "message"] {
            if let Some(text) = value.get(key).and_then(Value::as_str)
                && !text.is_empty()
            {
                return text.to_owned();
            }
        }
    }
    line.to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures_util::future::{AbortHandle, Abortable};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn command(script: &str, directory: &std::path::Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", script])
            .current_dir(directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        command
    }

    #[tokio::test]
    async fn cancellation_and_output_error_stop_tool_descendants() {
        for cancel in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let command = command(
                "(sleep 1; echo escaped > marker) & echo ready; wait",
                directory.path(),
            );
            let ready = Arc::new(AtomicBool::new(false));
            let observed = ready.clone();
            let (abort, registration) = AbortHandle::new_pair();
            let task = tokio::spawn(Abortable::new(
                run_attached(command, move |_| {
                    observed.store(true, Ordering::SeqCst);
                    if !cancel {
                        anyhow::bail!("editor disconnected");
                    }
                    Ok(())
                }),
                registration,
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                while !ready.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            if cancel {
                abort.abort();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
            assert!(result.is_err() || result.unwrap().is_err());
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(!directory.path().join("marker").exists());
        }
    }

    #[tokio::test]
    async fn verbose_stderr_is_drained_and_successful_exit_stops_background_tools() {
        let directory = tempfile::tempdir().unwrap();
        let command = command(
            "head -c 1048576 /dev/zero >&2; (sleep 1; echo escaped > marker) & echo done",
            directory.path(),
        );
        tokio::time::timeout(Duration::from_secs(3), run_attached(command, |_| Ok(())))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!directory.path().join("marker").exists());
    }
}
