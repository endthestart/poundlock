//! Delegating a task: what is uploaded, what is refused, and how big a folder may be.

use super::*;

#[tokio::test]
async fn a_delegated_task_uploads_the_folder_and_enforces_its_checks() {
    let controller = controller().await;
    let project = project();
    let tools = tools(&controller, project.path());

    // A call without its required arguments is an error result, not a panic.
    assert!(tools.call("delegate_task", Value::Null).await.is_error);

    let mut args = delegate_args(project.path());
    args["dry_run"] = json!(true);
    let dry = tools.call("delegate_task", args).await;
    assert!(!dry.is_error, "{}", dry.text);
    assert_eq!(
        dry.structured["paths"],
        json!(["src/lib.rs", "tests/greet.rs"])
    );
    assert_eq!(
        dry.structured["upload"]["credential_files_left_out"],
        json!([".env"])
    );
    assert!(
        controller.state.store.list_jobs(10).unwrap().is_empty(),
        "a dry run sends nothing"
    );

    let mut budgeted = delegate_args(project.path());
    budgeted["budget_minutes"] = json!(30);
    budgeted["budget_usd"] = json!(1.25);
    let started = tools.call("delegate_task", budgeted.clone()).await;
    assert!(!started.is_error, "{}", started.text);
    let id = started.structured["job_id"].as_str().unwrap().to_owned();
    let job = controller.state.store.job(&id).unwrap().unwrap();
    assert_eq!(job.policy.budget.wall_seconds, Some(1800));
    assert_eq!(job.policy.budget.frontier_cents, Some(125));
    assert!(job.prompt.starts_with("Make greet() say hello."));
    assert!(job.prompt.contains("- `cargo test`"));
    assert_eq!(job.policy.verification.commands[0].command, "cargo test");
    assert_eq!(
        job.policy.verification.protected_paths[0].path(),
        "tests/greet.rs"
    );
    assert_eq!(job.policy.max_repairs, 1);
    // What a runner would receive, read as one claims the job.
    let uploaded = claimed_source(&controller, &id);
    assert!(uploaded.contains("src/lib.rs"));
    assert!(
        !uploaded.contains(".env"),
        "the secret never leaves the machine"
    );

    // The same call again is the same job.
    let again = tools.call("delegate_task", budgeted).await;
    assert_eq!(again.structured["job_id"], json!(id));
    assert_eq!(controller.state.store.list_jobs(10).unwrap().len(), 1);
}

#[tokio::test]
async fn delegation_stays_inside_the_approved_folders() {
    let controller = controller().await;
    let project = project();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("notes.txt"), "private").unwrap();
    let tools = tools(&controller, &project.path().join("app"));
    let mut args = delegate_args(project.path());
    args["directory"] = json!(elsewhere.path().to_string_lossy());
    let refused = tools.call("delegate_task", args).await;
    assert!(refused.is_error);
    assert!(
        refused.text.contains("outside the folders"),
        "{}",
        refused.text
    );

    let mut unknown_model = delegate_args(project.path());
    unknown_model["model"] = json!("frontier");
    let refused = tools.call("delegate_task", unknown_model).await;
    assert!(refused.is_error);
    assert!(
        refused.text.contains("choose one of: coder"),
        "{}",
        refused.text
    );

    let mut stray = delegate_args(project.path());
    stray["unexpected"] = json!(true);
    assert!(tools.call("delegate_task", stray).await.is_error);
    assert!(controller.state.store.list_jobs(10).unwrap().is_empty());
}

#[tokio::test]
async fn a_folder_of_several_megabytes_uploads() {
    let controller = controller().await;
    let project = project();
    let root = project.path().join("app");
    for index in 0..3 {
        let content: String = (0..1_300_000)
            .map(|n| char::from(b'a' + ((n * 7 + index) % 26) as u8))
            .collect();
        std::fs::write(root.join(format!("src/data{index}.txt")), content).unwrap();
    }
    let tools = tools(&controller, project.path());
    let started = tools
        .call("delegate_task", delegate_args(project.path()))
        .await;
    assert!(!started.is_error, "{}", started.text);
    assert!(started.structured["upload"]["bytes"].as_u64().unwrap() > 3_900_000);
}
