use poundlock::git::{Workspace, patch_collection_config};
use std::path::Path;
use std::process::Command;

/// Git's repository-location variables (`git rev-parse --local-env-vars`). Run
/// from a hook or `git rebase --exec`, a test inherits them, and its `git init`
/// and `git config` in a temp folder then write to the enclosing repository.
const GIT_LOCATION_VARIABLES: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// `git` that finds only the repository the test made.
fn git_command() -> Command {
    let mut command = Command::new("git");
    for name in GIT_LOCATION_VARIABLES {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| {
            name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_")
        }) {
            command.env_remove(name);
        }
    }
    command
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = git_command()
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The patch the collector would produce for this workspace.
///
/// Runs the sequence and configuration `runner_sandbox::collect_patches` uses
/// — stage against the recorded base, then diff the index — rather than a
/// second implementation that happens to look similar. The collector does
/// this inside a container with the pristine `.git` mounted separately; here
/// the workspace's own repository stands in for it.
fn collected_patch(cwd: &Path, base: &str) -> String {
    let excludes = cwd.join("poundlock-exclude-test");
    std::fs::write(&excludes, "").unwrap();
    let config = patch_collection_config(&excludes);
    let config: Vec<&str> = config.iter().map(String::as_str).collect();
    let run = |extra: &[&str]| {
        let mut args = config.clone();
        args.extend_from_slice(extra);
        git(cwd, &args)
    };
    run(&["read-tree", base]);
    run(&["add", "--all"]);
    let patch = run(&[
        "diff",
        "--cached",
        "--binary",
        "--no-ext-diff",
        "--no-textconv",
        base,
        "--",
    ]);
    std::fs::remove_file(&excludes).unwrap();
    patch
}

#[tokio::test]
async fn workspaces_include_new_files_and_remove_git_metadata() {
    let projects = tempfile::tempdir().unwrap();
    let repo = projects.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "poundlock Test"]);
    git(&repo, &["config", "user.email", "test@invalid"]);
    std::fs::write(repo.join("existing.txt"), "before\n").unwrap();
    git(&repo, &["add", "existing.txt"]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);

    let attempt = tempfile::tempdir().unwrap();
    let workspace = Workspace::create(projects.path(), attempt.path(), &["repo".into()])
        .await
        .unwrap();
    assert!(workspace.cwd.join(".git").is_dir());
    assert!(git(&workspace.cwd, &["remote"]).trim().is_empty());
    let common = git(&workspace.cwd, &["rev-parse", "--git-common-dir"]);
    assert_eq!(common.trim(), ".git");
    std::fs::write(workspace.cwd.join("existing.txt"), "after\n").unwrap();
    git(&workspace.cwd, &["add", "existing.txt"]);
    git(
        &workspace.cwd,
        &["commit", "-q", "-m", "agent committed change"],
    );
    std::fs::write(workspace.cwd.join("new.txt"), "created\n").unwrap();
    let collected = collected_patch(&workspace.cwd, &workspace.worktrees[0].base_revision);
    assert!(collected.contains("existing.txt"));
    assert!(collected.contains("new.txt"));
    let patch = attempt.path().join("result.patch");
    std::fs::write(&patch, &collected).unwrap();
    git(&repo, &["apply", "--check", patch.to_str().unwrap()]);
    assert!(workspace.cleanup().await.is_empty());

    let listed = git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains(attempt.path().to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(repo.join("existing.txt")).unwrap(),
        "before\n"
    );
    assert!(!repo.join("new.txt").exists());
}

#[tokio::test]
async fn snapshot_patch_applies_to_supplied_baseline_and_preserves_binary_and_mode() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use poundlock::source::{JobSource, SourceFile};
    let source = JobSource::Snapshot {
        files: vec![
            SourceFile {
                path: "script.sh".into(),
                content_base64: STANDARD.encode("#!/bin/sh\necho original\n"),
                executable: true,
            },
            SourceFile {
                path: "binary.dat".into(),
                content_base64: STANDARD.encode([0, 1, 2, 3]),
                executable: false,
            },
        ],
    };
    let attempt = tempfile::tempdir().unwrap();
    let baseline = tempfile::tempdir().unwrap();
    let workspace = Workspace::from_source(attempt.path(), &source)
        .await
        .unwrap();
    let reference = Workspace::from_source(baseline.path(), &source)
        .await
        .unwrap();
    assert!(git(&workspace.cwd, &["ls-files", "--stage", "script.sh"]).starts_with("100755"));
    std::fs::write(workspace.cwd.join("binary.dat"), [0, 1, 4, 5]).unwrap();
    std::fs::write(workspace.cwd.join("new.txt"), "new\n").unwrap();
    let collected = collected_patch(&workspace.cwd, &workspace.worktrees[0].base_revision);
    assert!(collected.contains("GIT binary patch"));
    let patch = attempt.path().join("result.patch");
    std::fs::write(&patch, &collected).unwrap();
    git(&reference.cwd, &["apply", patch.to_str().unwrap()]);
    assert_eq!(
        std::fs::read(reference.cwd.join("binary.dat")).unwrap(),
        [0, 1, 4, 5]
    );
    assert_eq!(
        std::fs::read_to_string(reference.cwd.join("new.txt")).unwrap(),
        "new\n"
    );
    assert!(workspace.cleanup().await.is_empty());
    assert!(reference.cleanup().await.is_empty());
}

#[tokio::test]
async fn pinned_git_checkout_rejects_wrong_revision_and_incomplete_lfs_content() {
    let temporary = tempfile::tempdir().unwrap();
    let source = poundlock::source::JobSource::Empty;
    let workspace = Workspace::from_source(temporary.path(), &source)
        .await
        .unwrap();
    let revision = git(&workspace.cwd, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    std::fs::write(
        workspace.cwd.join(".git/FETCH_HEAD"),
        format!("{revision}\n"),
    )
    .unwrap();
    assert!(
        poundlock::git_source::finish_checkout(&workspace.cwd, &"a".repeat(40))
            .await
            .is_err()
    );
    let checkout = poundlock::git_source::finish_checkout(&workspace.cwd, &revision)
        .await
        .unwrap();
    assert_eq!(checkout.worktrees[0].base_revision, revision);
    std::fs::write(
        workspace.cwd.join("large.bin"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:example\nsize 123\n",
    )
    .unwrap();
    git(&workspace.cwd, &["add", "large.bin"]);
    git(&workspace.cwd, &["commit", "-q", "-m", "lfs pointer"]);
    let revision = git(&workspace.cwd, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    std::fs::write(
        workspace.cwd.join(".git/FETCH_HEAD"),
        format!("{revision}\n"),
    )
    .unwrap();
    assert!(
        poundlock::git_source::finish_checkout(&workspace.cwd, &revision)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("LFS")
    );
}

#[tokio::test]
#[ignore = "explicit authenticated HTTPS source acceptance; requires POUNDLOCK_TEST_GIT_URL and POUNDLOCK_TEST_GIT_REVISION"]
async fn live_pinned_https_git_checkout_has_no_remote_or_stored_credential() {
    let repository = std::env::var("POUNDLOCK_TEST_GIT_URL").unwrap();
    let revision = std::env::var("POUNDLOCK_TEST_GIT_REVISION").unwrap();
    let credentials = std::env::var("POUNDLOCK_TEST_GIT_TOKEN").ok().map(|token| {
        poundlock::git_source::GitCredentials {
            username: std::env::var("POUNDLOCK_TEST_GIT_USERNAME").unwrap(),
            token,
        }
    });
    let temporary = tempfile::tempdir().unwrap();
    let workspace = poundlock::git_source::checkout(
        temporary.path(),
        &repository,
        &revision,
        credentials.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(workspace.worktrees[0].base_revision, revision);
    assert!(git(&workspace.cwd, &["remote"]).trim().is_empty());
    let config = std::fs::read_to_string(workspace.cwd.join(".git/config")).unwrap();
    assert!(!config.contains("extraHeader"));
    if let Some(credentials) = credentials {
        assert!(!config.contains(&credentials.token));
    }
    assert!(workspace.cleanup().await.is_empty());
}

#[tokio::test]
async fn repeated_git_subtrees_are_rejected_before_checkout() {
    use std::io::Write;
    use std::process::Stdio;
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    git(cwd, &["init", "-q"]);
    git(cwd, &["config", "user.name", "Test"]);
    git(cwd, &["config", "user.email", "test@invalid"]);
    fn input(cwd: &Path, args: &[&str], value: &str) -> String {
        let mut child = git_command()
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    let blob = input(cwd, &["hash-object", "-w", "--stdin"], "small\n");
    let mut tree = input(cwd, &["mktree"], &format!("100644 blob {blob}\tfile\n"));
    // Six tiny tree objects represent over a million paths, without creating
    // those files or large unique Git objects on the test machine.
    for _ in 0..5 {
        let entries = (0..16)
            .map(|i| format!("040000 tree {tree}\td{i:02}\n"))
            .collect::<String>();
        tree = input(cwd, &["mktree"], &entries);
    }
    let revision = input(cwd, &["commit-tree", &tree], "repeated subtrees\n");
    std::fs::write(cwd.join(".git/FETCH_HEAD"), format!("{revision}\n")).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        poundlock::git_source::finish_checkout(cwd, &revision),
    )
    .await
    .expect("recursive inventory failed to terminate");
    let error = result.err().expect("amplified source was accepted");
    // The record limit, not the byte limit or the test deadline, must stop
    // recursive expansion of reused tree objects.
    assert!(error.to_string().contains("record output limit"), "{error}");
    assert!(
        !cwd.join("d00").exists(),
        "checkout started before inventory validation"
    );
}

#[tokio::test]
async fn a_git_source_leaves_nothing_on_the_supervisor_to_clean_up() {
    // The fetch container writes into the job's volumes (L13); the supervisor's
    // attempt folder never holds the workspace, and cleanup must accept that.
    let attempt = tempfile::tempdir().unwrap();
    let source = poundlock::source::JobSource::Git {
        repository: "https://github.com/octocat/Hello-World".into(),
        revision: "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d".into(),
        credentials: None,
    };
    let workspace = poundlock::git::Workspace::from_source(attempt.path(), &source)
        .await
        .unwrap();
    assert!(workspace.fetch.is_some());
    assert!(!workspace.cwd.exists());
    assert!(workspace.cleanup().await.is_empty());
}
