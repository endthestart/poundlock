//! Each subcommand end to end: argv through clap, the shared controller client,
//! a real controller router on a loopback port, and what the command prints.

use super::*;
use crate::server::{AppState, ops_tests::test_state, router};
use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: RemoteCommand,
}

struct Controller {
    _directory: tempfile::TempDir,
    state: AppState,
    url: String,
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    url
}

async fn controller() -> Controller {
    let (directory, state) = test_state(|_| {});
    let url = serve(router(state.clone())).await;
    Controller {
        _directory: directory,
        state,
        url,
    }
}

/// `poundlock <args> --controller <controller> --token <token>`, and what it
/// printed.
async fn poundlock(args: &[&str], controller: &str, token: &str) -> Result<String> {
    let argv = ["poundlock"]
        .into_iter()
        .chain(args.iter().copied())
        .chain(["--controller", controller, "--token", token]);
    let cli = Cli::try_parse_from(argv)?;
    let mut out = Vec::new();
    run(cli.command, &mut out).await?;
    Ok(String::from_utf8(out)?)
}

fn json(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|error| panic!("{error}: {output}"))
}

impl Controller {
    async fn admin(&self, args: &[&str]) -> Result<String> {
        poundlock(args, &self.url, "admin-secret").await
    }

    fn endpoint(&self) -> String {
        self.state
            .store
            .create_endpoint(
                &serde_json::from_value(json!({
                    "name": "workstation", "kind": "openai",
                    "base_url": "http://127.0.0.1:9/v1", "api_key": ""
                }))
                .unwrap(),
            )
            .unwrap()
            .id
    }

    /// An endpoint, a durable model and a project: what a job needs.
    fn job_fixture(&self) -> (String, String) {
        let endpoint = self.endpoint();
        let model = self
            .state
            .store
            .create_model_profile(
                &serde_json::from_value(json!({
                    "name": "agent", "endpoint_id": endpoint, "model_id": "local",
                    "context_tokens": 32768, "max_output_tokens": 4096,
                    "uses": ["durable"], "wake_policy": "never"
                }))
                .unwrap(),
            )
            .unwrap()
            .id;
        let project = self
            .state
            .store
            .create_project(&CreateProject {
                name: "web".into(),
                roots: vec!["web".into()],
                instruction_files: Vec::new(),
            })
            .unwrap()
            .id;
        (model, project)
    }

    async fn submit(&self, key: &str) -> Value {
        let (model, project) = self.job_fixture();
        json(
            &self
                .admin(&[
                    "job-submit",
                    "--project-id",
                    &project,
                    "--model-profile-id",
                    &model,
                    "--prompt",
                    "Fix the failing test",
                    "--idempotency-key",
                    key,
                ])
                .await
                .unwrap(),
        )
    }
}

#[tokio::test]
async fn status_prints_the_overview_and_names_a_refused_token() {
    let controller = controller().await;
    let overview = json(&controller.admin(&["status"]).await.unwrap());
    assert_eq!(overview["version"], crate::VERSION);
    assert!(overview["endpoints"].is_array());

    // --token-file, as in the controller container, wins over --token.
    let token_file = controller._directory.path().join("admin-token");
    std::fs::write(&token_file, "admin-secret\n").unwrap();
    let path = token_file.to_str().unwrap();
    let overview = json(
        &poundlock(&["status", "--token-file", path], &controller.url, "wrong")
            .await
            .unwrap(),
    );
    assert_eq!(overview["version"], crate::VERSION);

    let error = poundlock(&["status"], &controller.url, "wrong")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the controller refused the token: valid admin bearer token required")
            && error.contains("--token-file"),
        "{error}"
    );
}

#[tokio::test]
async fn an_unreachable_controller_is_named_with_what_to_check() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = poundlock(&["status"], &url, "admin-secret")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with(&format!("cannot reach the controller at {url}: "))
            && error.contains("POUNDLOCK_CONTROLLER_URL"),
        "{error}"
    );

    let error = poundlock(&["status"], "controller:7331", "admin-secret")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("--controller must be"), "{error}");
}

#[tokio::test]
async fn refusals_from_an_older_or_broken_controller_read_plainly() {
    use axum::http::StatusCode;
    use axum::routing::get;
    let url = serve(
        axum::Router::new()
            .route(
                "/api/v1/nodes/config-example",
                get(|| async { (StatusCode::BAD_GATEWAY, "upstream proxy error") }),
            )
            .route("/api/v1/overview", get(|| async { "<html>sign in</html>" })),
    )
    .await;
    let error = poundlock(&["node-config-example"], &url, "admin-secret")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "GET /api/v1/nodes/config-example failed on the controller: upstream proxy error (HTTP 502)"
    );
    let error = poundlock(&["status"], &url, "admin-secret")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with(
            "the controller answered GET /api/v1/overview with a body that is not the expected JSON"
        ),
        "{error}"
    );
    let error = poundlock(&["job-watch", "missing"], &url, "admin-secret")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with(&format!(
            "the controller at {url} has no GET /api/v1/jobs/missing; it may run an older Poundlock"
        )),
        "{error}"
    );
}

#[tokio::test]
async fn endpoint_add_creates_the_endpoint_and_reports_validation() {
    let controller = controller().await;
    let endpoint = json(
        &controller
            .admin(&[
                "endpoint-add",
                "--name",
                "workstation",
                "--base-url",
                "http://127.0.0.1:9/v1/",
                "--data-destination",
                "operator_network",
                "--public-url",
                "https://workstation.example",
            ])
            .await
            .unwrap(),
    );
    assert_eq!(endpoint["name"], "workstation");
    assert_eq!(endpoint["kind"], "openai");
    assert_eq!(endpoint["base_url"], "http://127.0.0.1:9/v1");
    assert_eq!(endpoint["data_destination"], "operator_network");
    assert_eq!(endpoint["public_url"], "https://workstation.example");

    let error = controller
        .admin(&[
            "endpoint-add",
            "--name",
            "other",
            "--kind",
            "vllm",
            "--base-url",
            "http://127.0.0.1:9/v1",
        ])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "POST /api/v1/endpoints was refused: kind must be openai or litellm (HTTP 400)"
    );
}

#[tokio::test]
async fn model_add_creates_the_profile_and_reports_validation() {
    let controller = controller().await;
    let endpoint = controller.endpoint();
    let profile = json(
        &controller
            .admin(&[
                "model-add",
                "--name",
                "workstation-coder",
                "--endpoint-id",
                &endpoint,
                "--model-id",
                "coder",
                "--context-tokens",
                "32768",
                "--max-output-tokens",
                "4096",
            ])
            .await
            .unwrap(),
    );
    assert_eq!(profile["name"], "workstation-coder");
    assert_eq!(profile["endpoint_id"], endpoint);
    assert_eq!(profile["context_tokens"], 32768);

    let error = controller
        .admin(&[
            "model-add",
            "--name",
            "nowhere",
            "--endpoint-id",
            "missing",
            "--model-id",
            "coder",
            "--context-tokens",
            "32768",
            "--max-output-tokens",
            "4096",
        ])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("endpoint_id does not exist (HTTP 400)"),
        "{error}"
    );
}

/// The console's Add model shows the draft check's values; model-add given the
/// same answers and nothing else must save exactly those.
#[tokio::test]
async fn model_add_takes_the_defaults_the_console_shows() {
    let controller = controller().await;
    let store = &controller.state.store;
    let endpoint = controller.endpoint();
    store
        .set_endpoint_destination(&endpoint, "operator_network")
        .unwrap();
    let capabilities = json!({
        "runtimes": ["llamacpp"],
        "inventory": {"runtimes": {"llamacpp": {
            "servable": ["qwen3.8-27b-mtp"],
            "models": [{"id": "qwen3.8-27b-mtp", "label": "Qwen3.8-27B IQ3_S",
                "context_tokens": 131072}]
        }}}
    });
    let node = store
        .enroll_node(&crate::domain::EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: capabilities.clone(),
        })
        .unwrap()
        .node_id;
    store.heartbeat_node(&node, "linux", &capabilities).unwrap();
    store
        .configure_wol(&node, Some("02:00:00:00:00:01"), Some("10.20.0.255:9"))
        .unwrap();
    let console = ControllerClient::new(&controller.url, "admin-secret").unwrap();
    let mut saved = Vec::new();
    for (answers, flags) in [
        (
            json!({"node_id": node, "runtime_profile": "llamacpp", "model_id": "qwen3.8-27b-mtp"}),
            vec![
                "--node-id",
                &node,
                "--runtime-profile",
                "llamacpp",
                "--model-id",
                "qwen3.8-27b-mtp",
            ],
        ),
        (
            json!({"model_id": "coder", "context_tokens": 32768}),
            vec!["--model-id", "coder", "--context-tokens", "32768"],
        ),
    ] {
        let mut answers = answers;
        answers["endpoint_id"] = json!(endpoint);
        let draft: Value = console
            .post("/api/v1/model-profiles/draft", &answers)
            .await
            .unwrap();
        let mut argv = vec!["model-add", "--endpoint-id", &endpoint];
        argv.extend(flags);
        let profile = json(&controller.admin(&argv).await.unwrap());
        for key in [
            "name",
            "context_tokens",
            "max_output_tokens",
            "uses",
            "wake_policy",
            "thinking",
            "model_label",
        ] {
            assert_eq!(profile[key], draft[key]["value"], "{key}: {profile}");
        }
        saved.push(profile);
    }
    // The machine's answers, not a fixed table.
    let machine = &saved[0];
    assert_eq!(machine["name"], "orion-qwen3.8-27b-mtp");
    assert_eq!(machine["context_tokens"], 131072);
    assert_eq!(machine["max_output_tokens"], 32768);
    assert_eq!(machine["wake_policy"], "on-demand");
    assert_eq!(machine["thinking"], "off");
    assert_eq!(machine["model_label"], "Qwen3.8-27B IQ3_S");
    let server = &saved[1];
    assert_eq!(server["name"], "workstation-coder");
    assert_eq!(server["max_output_tokens"], 8192);
    assert_eq!(server["uses"], json!(["durable", "interactive"]));
    assert_eq!(server["wake_policy"], "never");
    assert_eq!(server["thinking"], "model_default");

    let error = controller
        .admin(&[
            "model-add",
            "--endpoint-id",
            &endpoint,
            "--model-id",
            "unlisted",
        ])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("context_tokens is required: nothing reported this model's context"),
        "{error}"
    );
}

#[tokio::test]
async fn project_add_registers_roots_and_reports_validation() {
    let controller = controller().await;
    let project = json(
        &controller
            .admin(&[
                "project-add",
                "--name",
                "web",
                "--root",
                "web",
                "--root",
                "shared",
                "--instruction-file",
                "AGENTS.md",
            ])
            .await
            .unwrap(),
    );
    assert_eq!(project["name"], "web");
    assert_eq!(project["roots"], json!(["web", "shared"]));

    let error = controller
        .admin(&["project-add", "--name", "escape", "--root", "../etc"])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Repository paths must be relative and cannot contain '..' (HTTP 400)"),
        "{error}"
    );
}

#[tokio::test]
async fn node_token_takes_the_controller_default_and_reports_validation() {
    let controller = controller().await;
    let issued = json(
        &controller
            .admin(&["node-token", "--node-name", "orion"])
            .await
            .unwrap(),
    );
    assert_eq!(issued["valid_minutes"], 15, "the controller's default");
    assert_eq!(issued["node_name"], "orion");
    assert!(!issued["token"].as_str().unwrap().is_empty());
    let issued = json(
        &controller
            .admin(&["node-token", "--valid-minutes", "60"])
            .await
            .unwrap(),
    );
    assert_eq!(issued["valid_minutes"], 60);

    let long_name = "n".repeat(81);
    let error = controller
        .admin(&["node-token", "--node-name", &long_name])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("node name must be 1 to 80 characters (HTTP 400)"),
        "{error}"
    );
}

#[tokio::test]
async fn node_wol_configures_a_machine_and_reports_an_unknown_one() {
    let controller = controller().await;
    let store = &controller.state.store;
    let node = store
        .enroll_node(&crate::domain::EnrollNode {
            token: store.create_enrollment_token(15, None).unwrap(),
            name: "orion".into(),
            platform: "linux".into(),
            capabilities: json!({}),
        })
        .unwrap()
        .node_id;
    let configured = json(
        &controller
            .admin(&["node-wol", &node, "--mac", "02:00:00:00:00:01"])
            .await
            .unwrap(),
    );
    assert_eq!(configured, json!({"configured": true}));
    let saved = store.node(&node, 15).unwrap().unwrap();
    assert_eq!(saved.wol_mac.as_deref(), Some("02:00:00:00:00:01"));
    assert_eq!(saved.wol_broadcast.as_deref(), Some("255.255.255.255:9"));

    let unknown = Uuid::new_v4().to_string();
    let error = controller
        .admin(&["node-wol", &unknown, "--mac", "02:00:00:00:00:01"])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        format!("PUT /api/v1/nodes/{unknown}/wol was refused: node not found (HTTP 404)")
    );
    let error = controller
        .admin(&[
            "node-wol",
            "../enrollment-tokens",
            "--mac",
            "02:00:00:00:00:01",
        ])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("is not a machine id"), "{error}");
}

#[tokio::test]
async fn node_config_example_prints_the_controllers_toml() {
    let controller = controller().await;
    assert_eq!(
        controller.admin(&["node-config-example"]).await.unwrap(),
        crate::node::example_config()
    );
    let error = poundlock(&["node-config-example"], &controller.url, "wrong")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("refused the token"), "{error}");
}

#[tokio::test]
async fn job_submit_queues_a_job_and_reports_validation() {
    let controller = controller().await;
    let job = controller.submit("submit-1").await;
    assert_eq!(job["idempotency_key"], "submit-1");
    assert_eq!(job["prompt"], "Fix the failing test");
    assert!(!crate::domain::is_terminal_job_status(
        job["status"].as_str().unwrap()
    ));

    let project = job["project_id"].as_str().unwrap();
    let error = controller
        .admin(&[
            "job-submit",
            "--project-id",
            project,
            "--model-profile-id",
            "missing",
            "--prompt",
            "Fix it",
        ])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "POST /api/v1/jobs was refused: model_profile_id does not exist (HTTP 400)"
    );
}

#[tokio::test]
async fn job_submit_watch_recovers_a_submission_by_its_key_and_follows_it() {
    let controller = controller().await;
    let job = controller.submit("watch-1").await;
    let id = job["id"].as_str().unwrap();
    controller
        .state
        .store
        .fail_job(id, json!({"message": "stopped"}), true)
        .unwrap();
    // The same submission and key return the same job, now finished.
    let model = job["model_profile_id"].as_str().unwrap();
    let project = job["project_id"].as_str().unwrap();
    let output = controller
        .admin(&[
            "job-submit",
            "--project-id",
            project,
            "--model-profile-id",
            model,
            "--prompt",
            "Fix the failing test",
            "--idempotency-key",
            "watch-1",
            "--watch",
        ])
        .await
        .unwrap();
    assert!(output.contains("\ncancelled  Job cancelled.\n"), "{output}");
    let last = &output[output.rfind("\n{").unwrap()..];
    assert_eq!(json(last)["status"], "cancelled");
}

#[tokio::test]
async fn job_watch_prints_events_then_the_finished_job() {
    let controller = controller().await;
    let job = controller.submit("watch-2").await;
    let id = job["id"].as_str().unwrap();
    controller
        .state
        .store
        .fail_job(id, json!({"message": "out of time"}), false)
        .unwrap();
    let output = controller.admin(&["job-watch", id]).await.unwrap();
    let (events, finished) = output.split_at(output.find('{').unwrap());
    assert!(
        events.ends_with("failed  Job failed. Check the error details.\n"),
        "{events}"
    );
    assert_eq!(json(finished)["status"], "failed");

    let unknown = Uuid::new_v4().to_string();
    let error = controller
        .admin(&["job-watch", &unknown])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        format!("GET /api/v1/jobs/{unknown} was refused: job not found (HTTP 404)")
    );
}

#[tokio::test]
async fn job_cancel_requests_cancellation_and_reports_an_unknown_job() {
    let controller = controller().await;
    let job = controller.submit("cancel-1").await;
    let id = job["id"].as_str().unwrap();
    let cancelled = json(&controller.admin(&["job-cancel", id]).await.unwrap());
    assert_eq!(cancelled, json!({"cancel_requested": true}));
    assert!(
        controller
            .state
            .store
            .job(id)
            .unwrap()
            .unwrap()
            .cancel_requested
    );

    let unknown = Uuid::new_v4().to_string();
    let error = controller
        .admin(&["job-cancel", &unknown])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        format!("POST /api/v1/jobs/{unknown}/cancel was refused: job not found (HTTP 404)")
    );
}

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
