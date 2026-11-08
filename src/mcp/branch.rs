//! A job's patch as a new branch: one commit on top of the folder's `HEAD`,
//! made with Git's plumbing in a scratch index, so the working tree, the real
//! index and the checked-out branch never change. The commit's author is the
//! job, so its code is told apart from the person's in history.

use anyhow::{Context, Result, bail, ensure};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Who a job's commit is by.
pub const JOB_AUTHOR_NAME: &str = "Poundlock job";
pub const JOB_AUTHOR_EMAIL: &str = "job@poundlock.invalid";

#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    pub branch: String,
    pub commit: String,
    pub base: String,
}

fn git(directory: &Path, index: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        // No hooks, no signing prompt in a server with no terminal.
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .stdin(Stdio::null());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

fn run(mut command: Command, input: Option<&str>) -> Result<String> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run git")?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("open git input")?
            .write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Checks that `branch` can be created in `directory`'s repository.
fn check_branch(directory: &Path, branch: &str) -> Result<()> {
    let mut format = git(directory, None);
    format.args(["check-ref-format", "--branch", branch]);
    run(format, None).map_err(|_| anyhow::anyhow!("{branch} is not a valid branch name"))?;
    let mut exists = git(directory, None);
    exists.args([
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ]);
    // Captured: this server's standard output is its MCP channel.
    if exists.output()?.status.success() {
        bail!("a branch named {branch} already exists; choose another name");
    }
    Ok(())
}

/// Commits `patch`, whose paths are relative to `directory`, as a new branch
/// on top of `HEAD`. With `check_only`, says whether it would apply and
/// changes nothing.
pub fn commit_patch(
    directory: &Path,
    patch: &str,
    branch: &str,
    message: &str,
    check_only: bool,
) -> Result<Option<Committed>> {
    let mut prefix = git(directory, None);
    prefix.args(["rev-parse", "--show-prefix"]);
    let prefix = run(prefix, None).context("the folder is not in a Git repository")?;
    let mut head = git(directory, None);
    head.args(["rev-parse", "--verify", "HEAD^{commit}"]);
    let base = run(head, None).context("the repository has no commit to build a branch on")?;
    check_branch(directory, branch)?;
    let scratch = tempfile::tempdir()?;
    let index = scratch.path().join("index");
    let mut read = git(directory, Some(&index));
    read.args(["read-tree", &base]);
    run(read, None)?;
    let apply = |check: bool| -> Result<()> {
        let mut command = git(directory, Some(&index));
        command.args(["apply", "--cached", "--whitespace=nowarn"]);
        if !prefix.is_empty() {
            command.arg(format!("--directory={}", prefix.trim_end_matches('/')));
        }
        if check {
            command.arg("--check");
        }
        command.arg("-");
        run(command, Some(patch)).map(|_| ())
    };
    apply(true).map_err(|error| {
        anyhow::anyhow!(
            "The patch does not apply to {branch}'s base {}: {error}",
            &base[..12]
        )
    })?;
    if check_only {
        return Ok(None);
    }
    apply(false)?;
    let mut write = git(directory, Some(&index));
    write.arg("write-tree");
    let tree = run(write, None)?;
    let mut commit = git(directory, Some(&index));
    commit
        .args(["commit-tree", &tree, "-p", &base, "-F", "-"])
        .env("GIT_AUTHOR_NAME", JOB_AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", JOB_AUTHOR_EMAIL);
    // The person who applied it commits it; without an identity, the job does.
    let mut email = git(directory, None);
    email.args(["config", "user.email"]);
    if run(email, None).is_err() {
        commit
            .env("GIT_COMMITTER_NAME", JOB_AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", JOB_AUTHOR_EMAIL);
    }
    let commit = run(commit, Some(message))?;
    let mut update = git(directory, None);
    // The empty old value makes this fail if the branch appeared meanwhile.
    update.args([
        "update-ref",
        "-m",
        "poundlock apply_patch",
        &format!("refs/heads/{branch}"),
        &commit,
        "",
    ]);
    run(update, None)?;
    Ok(Some(Committed {
        branch: branch.to_owned(),
        commit,
        base,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    const PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(root.join("app/src/lib.rs"), "old\n").unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        sh(
            &root,
            &[
                "-c",
                "user.name=Owner",
                "-c",
                "user.email=owner@example.net",
                "add",
                ".",
            ],
        );
        sh(
            &root,
            &[
                "-c",
                "user.name=Owner",
                "-c",
                "user.email=owner@example.net",
                "commit",
                "-q",
                "-m",
                "start",
            ],
        );
        dir
    }

    #[test]
    fn commits_a_branch_without_touching_the_working_tree() {
        let dir = repository();
        let root = dir.path().join("repo");
        let app = root.join("app");
        // An edit in progress stays exactly as it was.
        std::fs::write(app.join("src/lib.rs"), "being edited\n").unwrap();
        assert!(
            commit_patch(&app, PATCH, "job/one", "m", true)
                .unwrap()
                .is_none()
        );
        let done = commit_patch(
            &app,
            PATCH,
            "job/one",
            "Make it new\n\nPoundlock-Job: 1\n",
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(done.base, sh(&root, &["rev-parse", "HEAD"]));
        assert_eq!(sh(&root, &["show", "job/one:app/src/lib.rs"]), "new");
        assert_eq!(
            sh(
                &root,
                &[
                    "log",
                    "-1",
                    "--format=%an <%ae>|%s|%(trailers:key=Poundlock-Job,valueonly)",
                    "job/one"
                ]
            ),
            "Poundlock job <job@poundlock.invalid>|Make it new|1"
        );
        assert_eq!(
            std::fs::read_to_string(app.join("src/lib.rs")).unwrap(),
            "being edited\n"
        );
        assert_eq!(sh(&root, &["branch", "--show-current"]), "main");
        assert_eq!(sh(&root, &["status", "--porcelain"]), "M app/src/lib.rs");
        // Never over an existing branch, nor with a name Git refuses.
        assert!(
            commit_patch(&app, PATCH, "job/one", "m", false)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert!(
            commit_patch(&app, PATCH, "bad..name", "m", false)
                .unwrap_err()
                .to_string()
                .contains("not a valid branch name")
        );
    }

    #[test]
    fn says_when_the_patch_does_not_fit_the_base() {
        let dir = repository();
        let app = dir.path().join("repo/app");
        let stale = PATCH.replace("-old", "-other");
        let error = commit_patch(&app, &stale, "job/two", "m", false).unwrap_err();
        assert!(error.to_string().contains("does not apply"), "{error}");
        assert!(sh(&dir.path().join("repo"), &["branch", "--list", "job/two"]).is_empty());
    }
}
