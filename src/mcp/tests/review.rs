//! A finished job's review and apply, and following or cancelling a job.

use super::*;

#[tokio::test]
async fn a_finished_job_is_reviewed_then_applied() {
    let controller = controller().await;
    let project = project();
    let root = project.path().join("app");
    let tools = tools(&controller, project.path());
    let started = tools
        .call("delegate_task", delegate_args(project.path()))
        .await;
    let id = started.structured["job_id"].as_str().unwrap().to_owned();

    let pending = tools.call("job_result", json!({"job_id": id})).await;
    assert_eq!(pending.structured["finished"], false);
    let waited = tools
        .call("wait_for_job", json!({"job_id": id, "timeout_seconds": 1}))
        .await;
    assert_eq!(waited.structured["finished"], false, "{}", waited.text);

    claim(&controller, &id);
    let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-pub fn greet() -> &'static str { \"hi\" }\n+pub fn greet() -> &'static str { \"hello\" }\n";
    controller
        .state
        .store
        .complete_job(
            &id,
            &RunnerResult {
                patches: vec![RootPatch {
                    root: "workspace".into(),
                    base_revision: "base".into(),
                    patch: patch.into(),
                }],
                output: "Changed greet to say hello. cargo test passes.".into(),
                provenance: json!({
                    "attempt_count": 1,
                    "verification": {
                        "status": "passed", "reason": "", "changed_protected_paths": [],
                        "commands": [{"command": "cargo test", "cwd": "", "exit_code": 0,
                                      "duration_ms": 1200, "output": "test result: ok", "truncated": false, "error": null}]
                    }
                }),
                cleanup: json!({}),
            },
        )
        .unwrap();

    let waited = tools.call("wait_for_job", json!({"job_id": id})).await;
    assert_eq!(waited.structured["finished"], true);
    let result = tools.call("job_result", json!({"job_id": id})).await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(result.structured["verification"]["status"], "passed");
    assert_eq!(
        result.structured["verification"]["checks"][0]["passed"],
        true
    );
    assert_eq!(
        result.structured["patches"][0]["files"],
        json!(["src/lib.rs"])
    );
    assert!(result.text.contains("check `cargo test`: passed"));

    let shown = tools.call("get_patch", json!({"job_id": id})).await;
    assert_eq!(shown.text, patch);
    let saved_to = project.path().join("review.patch");
    let saved = tools
        .call(
            "get_patch",
            json!({"job_id": id, "save_to": saved_to.to_string_lossy()}),
        )
        .await;
    assert!(!saved.is_error, "{}", saved.text);
    assert_eq!(std::fs::read_to_string(&saved_to).unwrap(), patch);

    // As a branch: one commit by the job, and the folder untouched.
    let branched = tools
        .call(
            "apply_patch",
            json!({"job_id": id, "directory": root.to_string_lossy(), "branch": "job/greet"}),
        )
        .await;
    assert!(!branched.is_error, "{}", branched.text);
    assert_eq!(branched.structured["branch"], "job/greet");
    let show = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["log", "-1", "--format=%an|%B", "job/greet"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&show.stdout);
    assert!(log.starts_with("Poundlock job|"), "{log}");
    assert!(log.contains(&format!("Poundlock-Job: {id}")), "{log}");
    assert!(log.contains("Poundlock-Verification: passed"), "{log}");
    assert!(
        std::fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("\"hi\"")
    );

    let checked = tools
        .call(
            "apply_patch",
            json!({"job_id": id, "directory": root.to_string_lossy(), "check_only": true}),
        )
        .await;
    assert_eq!(checked.structured["applies"], true, "{}", checked.text);
    assert!(
        std::fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("\"hi\"")
    );
    let applied = tools
        .call(
            "apply_patch",
            json!({"job_id": id, "directory": root.to_string_lossy()}),
        )
        .await;
    assert!(!applied.is_error, "{}", applied.text);
    assert!(
        std::fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("\"hello\"")
    );
    let twice = tools
        .call(
            "apply_patch",
            json!({"job_id": id, "directory": root.to_string_lossy()}),
        )
        .await;
    assert!(twice.is_error, "an applied patch does not apply again");
    assert!(
        twice.text.contains("does not apply cleanly"),
        "{}",
        twice.text
    );

    // Provenance: the applied patch is recorded, and a change by hand shows.
    assert_eq!(applied.structured["provenance_recorded"], true);
    let directory = json!({"directory": root.to_string_lossy()});
    let clean = tools.call("provenance", directory.clone()).await;
    assert_eq!(clean.structured["clean"], true, "{}", clean.text);
    assert_eq!(clean.structured["checks_passed"], true, "{}", clean.text);
    assert_eq!(clean.structured["applied"][0]["verification"], "passed");
    assert_eq!(clean.structured["applied"][0]["job_id"], id.as_str());
    std::fs::write(root.join("src/lib.rs"), "pub fn greet() {}\n").unwrap();
    let found = tools.call("provenance", directory).await;
    assert_eq!(found.structured["clean"], false);
    assert!(
        found
            .text
            .contains(&format!("since job {id}: changed src/lib.rs")),
        "{}",
        found.text
    );
}

#[tokio::test]
async fn a_patch_that_touches_a_secret_is_refused() {
    let controller = controller().await;
    let project = project();
    let root = project.path().join("app");
    let tools = tools(&controller, project.path());
    let started = tools
        .call("delegate_task", delegate_args(project.path()))
        .await;
    let id = started.structured["job_id"].as_str().unwrap().to_owned();
    claim(&controller, &id);
    controller
        .state
        .store
        .complete_job(
            &id,
            &RunnerResult {
                patches: vec![RootPatch {
                    root: "workspace".into(),
                    base_revision: "base".into(),
                    patch: "diff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@ -1 +1 @@\n-API_KEY=do-not-upload\n+API_KEY=stolen\n".into(),
                }],
                output: String::new(),
                provenance: json!({"verification": {"status": "unverified"}}),
                cleanup: json!({}),
            },
        )
        .unwrap();
    let refused = tools
        .call(
            "apply_patch",
            json!({"job_id": id, "directory": root.to_string_lossy()}),
        )
        .await;
    assert!(refused.is_error);
    assert!(refused.text.contains("credential file"), "{}", refused.text);
    assert_eq!(
        std::fs::read_to_string(root.join(".env")).unwrap(),
        "API_KEY=do-not-upload\n"
    );
}

#[tokio::test]
async fn status_and_cancel_follow_the_job() {
    let controller = controller().await;
    let project = project();
    let tools = tools(&controller, project.path());
    let started = tools
        .call("delegate_task", delegate_args(project.path()))
        .await;
    let id = started.structured["job_id"].as_str().unwrap().to_owned();
    let status = tools.call("job_status", json!({"job_id": id})).await;
    assert!(!status.is_error, "{}", status.text);
    assert_eq!(status.structured["finished"], false);
    assert!(status.structured["next_cursor"].as_i64().unwrap() >= 1);
    let cancelled = tools.call("cancel_job", json!({"job_id": id})).await;
    assert!(!cancelled.is_error, "{}", cancelled.text);
    assert!(
        controller
            .state
            .store
            .job(&id)
            .unwrap()
            .unwrap()
            .cancel_requested
    );
    let bad = tools
        .call("job_status", json!({"job_id": "../api/v1/overview"}))
        .await;
    assert!(bad.is_error);
}

#[tokio::test]
async fn a_wrong_token_is_reported_without_showing_it() {
    let controller = controller().await;
    let project = project();
    let tools = Tools {
        controller: super::controller::Controller::new(&controller.url, "not-a-real-token".into())
            .unwrap(),
        workspace: Workspace::new(&[project.path().to_path_buf()]).unwrap(),
        git_credentials: None,
        poll: Duration::from_millis(20),
    };
    let refused = tools.call("list_models", json!({})).await;
    assert!(refused.is_error);
    assert_eq!(refused.structured["status"], 401);
    assert!(!refused.text.contains("not-a-real-token"));
    let listed = self::tools(&controller, project.path())
        .call("list_models", Value::Null)
        .await;
    assert!(!listed.is_error, "{}", listed.text);
    assert_eq!(listed.structured["models"][0]["name"], "coder");
    assert!(listed.text.contains("No job runner"));
}
