use crate::acp_local::{self, AcpApi, AcpSession};
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ConnectionTo, Stdio};
use anyhow::{Result, bail};
use futures_util::future::{AbortHandle, Abortable};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
pub struct AcpOptions {
    pub controller: String,
    pub token: String,
    pub execution: String,
    pub project_id: Option<String>,
    pub model_profile_id: String,
    pub harness_profile_id: String,
    pub workspace_root: Option<PathBuf>,
    pub instruction_files: Vec<String>,
}

pub async fn run(options: AcpOptions) -> Result<()> {
    let execution = options.execution.trim();
    if execution != "attached" && execution != "delegated" {
        bail!("--execution must be attached or delegated");
    }
    if execution == "delegated" && options.project_id.as_deref().unwrap_or_default().is_empty() {
        bail!("delegated ACP requires --project-id");
    }
    let api = Arc::new(AcpApi::new(options)?);
    let sessions = Arc::new(Mutex::new(HashMap::<String, AcpSession>::new()));
    let children = Arc::new(Mutex::new(HashMap::<String, AbortHandle>::new()));
    let new_sessions = Arc::clone(&sessions);
    let prompt_sessions = Arc::clone(&sessions);
    let cancel_sessions = Arc::clone(&sessions);
    let prompt_children = Arc::clone(&children);
    let cancel_children = Arc::clone(&children);
    let prompt_api = Arc::clone(&api);
    let cancel_api = Arc::clone(&api);
    let new_root = api.options.workspace_root.clone();
    let connected = Agent
        .builder()
        .name("smart-local-agent")
        .on_receive_request(
            async |request: acp::InitializeRequest, responder, _connection| {
                responder.respond(
                    acp::InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(acp::AgentCapabilities::default())
                        .agent_info(acp::Implementation::new(
                            "Smart Local AI",
                            env!("CARGO_PKG_VERSION"),
                        )),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::NewSessionRequest, responder, _connection| {
                let id = Uuid::new_v4().to_string();
                let cwd = if request.cwd.as_os_str().is_empty() {
                    new_root.clone().unwrap_or(request.cwd)
                } else {
                    request.cwd
                };
                new_sessions
                    .lock()
                    .await
                    .insert(id.clone(), AcpSession { cwd, job_id: None });
                responder.respond(acp::NewSessionResponse::new(acp::SessionId::new(id)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::PromptRequest,
                        responder,
                        connection: ConnectionTo<agent_client_protocol::Client>| {
                let session = request.session_id.0.to_string();
                let prompt = request
                    .prompt
                    .into_iter()
                    .filter_map(|block| match block {
                        acp::ContentBlock::Text(text) => Some(text.text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let (cancel, registration) = AbortHandle::new_pair();
                {
                    let mut active = prompt_children.lock().await;
                    if active.contains_key(&session) {
                        return responder
                            .respond(acp::PromptResponse::new(acp::StopReason::Refusal));
                    }
                    active.insert(session.clone(), cancel);
                }
                let turn = acp_local::drive_prompt(
                    &prompt_api,
                    &connection,
                    request.session_id.clone(),
                    &prompt,
                    &prompt_sessions,
                    &session,
                );
                let result = Abortable::new(turn, registration).await;
                prompt_children.lock().await.remove(&session);
                let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("Turn cancelled")));
                let reason = if result.is_ok() {
                    acp::StopReason::EndTurn
                } else {
                    acp::StopReason::Refusal
                };
                if let Err(error) = result {
                    let _ = acp_local::notify(
                        &connection,
                        request.session_id,
                        &format!("Smart Local AI could not complete this turn: {error}"),
                    );
                }
                responder.respond(acp::PromptResponse::new(reason))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: acp::CancelNotification, _connection| {
                let session = notification.session_id.0.to_string();
                if let Some(cancel) = cancel_children.lock().await.get(&session) {
                    cancel.abort();
                }
                if let Some(job) = cancel_sessions
                    .lock()
                    .await
                    .get(&session)
                    .and_then(|state| state.job_id.clone())
                {
                    let _ = cancel_api.cancel(&job).await;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await;
    for cancel in children.lock().await.values() {
        cancel.abort();
    }
    connected?;
    Ok(())
}
