//! Delegating from a pushed Git commit.

use super::*;

/// The project, with an HTTPS remote whose branch already holds HEAD, as if
/// it had been pushed.
fn pushed(project: &tempfile::TempDir) -> std::path::PathBuf {
    let root = project.path().join("app");
    git(
        &root,
        &["remote", "add", "origin", "git@github.com:acme/app.git"],
    );
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    std::fs::remove_file(root.join(".env")).unwrap();
    root
}

#[tokio::test]
async fn a_git_source_sends_the_pushed_commit_with_credentials_kept_back() {
    let controller = controller().await;
    let project = project();
    let root = pushed(&project);
    let mut tools = tools(&controller, project.path());
    tools.git_credentials = Some(crate::git_source::GitCredentials {
        username: "x-access-token".into(),
        token: "gh-read-token".into(),
    });
    let mut args = delegate_args(project.path());
    args["source"] = json!("git");
    args["dry_run"] = json!(true);
    let dry = tools.call("delegate_task", args.clone()).await;
    assert!(!dry.is_error, "{}", dry.text);
    assert_eq!(
        dry.structured["git"]["repository"],
        "https://github.com/acme/app.git"
    );
    assert_eq!(dry.structured["git"]["credentials"], true);
    args["dry_run"] = json!(false);
    let started = tools.call("delegate_task", args).await;
    assert!(!started.is_error, "{}", started.text);
    assert!(!started.text.contains("gh-read-token"));
    assert!(!started.structured.to_string().contains("gh-read-token"));
    let id = started.structured["job_id"].as_str().unwrap().to_owned();
    claim(&controller, &id);
    match controller.state.store.runner_source(&id).unwrap().unwrap() {
        crate::source::JobSource::Git {
            repository,
            revision,
            credentials,
        } => {
            assert_eq!(repository, "https://github.com/acme/app.git");
            assert_eq!(revision.len(), 40);
            assert_eq!(credentials.unwrap().token, "gh-read-token");
        }
        other => panic!("expected a Git source, got {other:?}"),
    }
    let _ = root;
}

#[tokio::test]
async fn a_git_source_refuses_what_the_runner_could_not_fetch_exactly() {
    let controller = controller().await;
    let project = project();
    let tools = tools(&controller, project.path());
    let mut args = delegate_args(project.path());
    args["source"] = json!("git");
    // Uncommitted .env and no remote.
    let refused = tools.call("delegate_task", args.clone()).await;
    assert!(refused.is_error);
    assert!(
        refused.text.contains("not committed and pushed"),
        "{}",
        refused.text
    );
    let root = pushed(&project);
    // A commit that is not on any remote branch.
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn greet() -> &'static str { \"hey\" }\n",
    )
    .unwrap();
    git(&root, &["commit", "-qam", "local only"]);
    let refused = tools.call("delegate_task", args.clone()).await;
    assert!(refused.is_error);
    assert!(refused.text.contains("push it first"), "{}", refused.text);
    git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    // A host the controller does not approve.
    git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://gitlab.example.net/acme/app.git",
        ],
    );
    let refused = tools.call("delegate_task", args.clone()).await;
    assert!(refused.is_error);
    assert!(
        refused
            .text
            .contains("not a repository host this controller approves"),
        "{}",
        refused.text
    );
    // A subfolder is not the repository.
    git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/app.git",
        ],
    );
    args["directory"] = json!(root.join("src").to_string_lossy());
    let refused = tools.call("delegate_task", args).await;
    assert!(refused.is_error);
    assert!(
        refused.text.contains("use its top folder"),
        "{}",
        refused.text
    );
    assert!(controller.state.store.list_jobs(10).unwrap().is_empty());
}
