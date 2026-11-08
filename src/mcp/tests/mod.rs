mod delegate;
mod gitsource;
mod models;
mod protocol;
mod review;

use super::tools::{Tools, Workspace};
use super::*;
use crate::domain::{CreateApiToken, RootPatch, RunnerResult};
use crate::server::{AppState, router};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A real controller on a local port, one durable model, and an application
/// token that may submit, read and cancel jobs.
struct Controller {
    _directory: tempfile::TempDir,
    state: AppState,
    url: String,
    token: String,
}

async fn controller() -> Controller {
    let (directory, state) = crate::server::ops_tests::test_state(|_| {});
    let endpoint = state
        .store
        .create_endpoint(
            &serde_json::from_value(json!({
                "name": "local", "kind": "openai", "base_url": "http://127.0.0.1:9/v1", "api_key": ""
            }))
            .unwrap(),
        )
        .unwrap();
    state
        .store
        .create_model_profile(
            &serde_json::from_value(json!({
                "name": "coder", "aliases": ["coding"], "endpoint_id": endpoint.id,
                "model_id": "qwen-coder", "context_tokens": 131072, "max_output_tokens": 8192,
                "uses": ["durable"], "wake_policy": "never", "data_destination": "operator_network",
            }))
            .unwrap(),
        )
        .unwrap();
    let token = state
        .store
        .create_api_token(&CreateApiToken {
            name: "outside manager".into(),
            permissions: vec![
                "models:read".into(),
                "jobs:read".into(),
                "jobs:submit".into(),
                "jobs:cancel".into(),
            ],
        })
        .unwrap()
        .token;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Controller {
        _directory: directory,
        state,
        url: format!("http://{address}"),
        token,
    }
}

fn tools(controller: &Controller, root: &Path) -> Tools {
    Tools {
        controller: super::controller::Controller::new(&controller.url, controller.token.clone())
            .unwrap(),
        workspace: Workspace::new(&[root.to_path_buf()]).unwrap(),
        git_credentials: None,
        poll: Duration::from_millis(20),
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A small Git project with one secret and one committed test.
fn project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("app");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .unwrap();
    std::fs::write(root.join("tests/greet.rs"), "#[test] fn greets() {}\n").unwrap();
    std::fs::write(root.join(".env"), "API_KEY=do-not-upload\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "src", "tests"]);
    git(&root, &["commit", "-q", "-m", "start"]);
    directory
}

/// Claims the job as a runner would, so it can be read or completed.
fn claim(controller: &Controller, id: &str) {
    let store = &controller.state.store;
    store.set_job_status(id, "ready", "ready", "Ready").unwrap();
    assert_eq!(store.claim_runner_job(None).unwrap().as_deref(), Some(id));
}

fn claimed_source(controller: &Controller, id: &str) -> String {
    claim(controller, id);
    serde_json::to_string(&controller.state.store.runner_source(id).unwrap().unwrap()).unwrap()
}

fn delegate_args(root: &Path) -> Value {
    json!({
        "directory": root.join("app").to_string_lossy(),
        "model": "coding",
        "task": "Make greet() say hello.",
        "files": ["src/lib.rs"],
        "acceptance_checks": ["cargo test"],
        "frozen_paths": ["tests/greet.rs"],
    })
}
