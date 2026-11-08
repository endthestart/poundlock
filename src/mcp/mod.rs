//! `poundlock mcp`: a Model Context Protocol server that lets an outside agent,
//! such as Claude Code, use Poundlock durable jobs as its only way to write
//! code. The agent designs, researches, reviews and tests; a job writes the
//! code, inside Poundlock's isolated runner, behind checks the agent names.
//!
//! The server speaks MCP over standard input and output (newline-delimited
//! JSON-RPC 2.0) and talks to the controller with one application token. It
//! uploads only from folders the person who configured it named with
//! `--root`, never uploads credential files, and never shows the token.

pub mod branch;
pub mod brief;
pub mod controller;
pub mod gitsource;
pub mod provenance;
pub mod snapshot;
pub mod tools;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Protocol revisions this server speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest JSON-RPC line read from the client.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

const INSTRUCTIONS: &str = "Poundlock runs coding tasks as durable jobs on the operator's own models. \
Use it as your only way to write code: plan and research yourself, then delegate_task one bounded \
task at a time with the files to change and acceptance_checks that prove it works; freeze tests \
that define correct behaviour with frozen_paths. Follow the job with wait_for_job, review \
job_result and get_patch, and apply_patch only after you have read the patch. A finished job \
is not accepted work until you have reviewed it and run the project's checks yourself. Before \
calling work done, run provenance on the folder: it lists any file changed other than by a job.";

pub struct McpOptions {
    pub controller: String,
    pub token: String,
    pub roots: Vec<PathBuf>,
    /// Read access for private repositories when a task uses a Git source.
    pub git_credentials: Option<crate::git_source::GitCredentials>,
}

pub async fn run(options: McpOptions) -> Result<()> {
    let tools = Arc::new(tools::Tools {
        controller: controller::Controller::new(&options.controller, options.token)?,
        workspace: tools::Workspace::new(&options.roots)?,
        git_credentials: options.git_credentials,
        poll: Duration::from_secs(3),
    });
    eprintln!(
        "poundlock mcp: serving {} for folders {}",
        options.controller,
        tools.workspace.describe()
    );
    serve(
        tools,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

/// Reads requests until the input closes. Each tool call runs on its own
/// task, so a long wait never blocks a status check or a cancellation.
pub async fn serve<R, W>(tools: Arc<tools::Tools>, input: R, output: W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let output = Arc::new(Mutex::new(output));
    let running: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> = Default::default();
    let mut lines = input.lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("read from the MCP client")?
    {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_MESSAGE_BYTES {
            write(&output, &error(Value::Null, -32600, "message too large")).await?;
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                write(&output, &error(Value::Null, -32700, "parse error")).await?;
                continue;
            }
        };
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let id = message.get("id").cloned();
        match (method.as_str(), id) {
            ("notifications/cancelled", None) => {
                let key = message["params"]["requestId"].to_string();
                if let Some(task) = running.lock().await.remove(&key) {
                    task.abort();
                }
            }
            // Other notifications, and answers to requests this server never
            // sends, need nothing.
            (_, None) => {}
            ("tools/call", Some(id)) => {
                let tools = Arc::clone(&tools);
                let output = Arc::clone(&output);
                let key = id.to_string();
                let finished = Arc::clone(&running);
                let finished_key = key.clone();
                let params = message["params"].clone();
                let task = tokio::spawn(async move {
                    let name = params["name"].as_str().unwrap_or_default().to_owned();
                    // A client that sent a progress token hears how a long
                    // wait is going, so it need not guess whether to give up.
                    let token = params["_meta"]["progressToken"].clone();
                    let (progress, reporter) = if token.is_string() || token.is_number() {
                        let (sender, mut receiver) =
                            tokio::sync::mpsc::unbounded_channel::<String>();
                        let output = Arc::clone(&output);
                        let reporter = tokio::spawn(async move {
                            let mut count = 0u64;
                            while let Some(message) = receiver.recv().await {
                                count += 1;
                                let notification = json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {"progressToken": token, "progress": count, "message": message},
                                });
                                let _ = write(&output, &notification).await;
                            }
                        });
                        (Some(sender), Some(reporter))
                    } else {
                        (None, None)
                    };
                    let outcome = tools
                        .call_with_progress(&name, params["arguments"].clone(), progress)
                        .await;
                    // Every progress line is written before the result.
                    if let Some(reporter) = reporter {
                        let _ = reporter.await;
                    }
                    finished.lock().await.remove(&finished_key);
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": outcome.text}],
                            "structuredContent": outcome.structured,
                            "isError": outcome.is_error,
                        }
                    });
                    let _ = write(&output, &response).await;
                });
                running.lock().await.insert(key, task.abort_handle());
            }
            (method, Some(id)) => {
                write(&output, &answer(method, &message["params"], id)).await?;
            }
        }
    }
    // The client closed the connection: stop any work it was waiting for.
    for (_, task) in running.lock().await.drain() {
        task.abort();
    }
    Ok(())
}

/// The response to any request other than a tool call.
fn answer(method: &str, params: &Value, id: Value) -> Value {
    match method {
        "initialize" => {
            let requested = params["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|version| **version == requested)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "poundlock", "title": "Poundlock", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }
            })
        }
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": tools::definitions()},
        }),
        _ => error(id, -32601, &format!("method not found: {method}")),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn write<W>(output: &Mutex<W>, message: &Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    let mut output = output.lock().await;
    output.write_all(&line).await?;
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
