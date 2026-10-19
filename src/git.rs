use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use tokio::process::Command;

pub struct Worktree {
    pub root_name: String,
    pub path: PathBuf,
    pub base_revision: String,
}

pub struct Workspace {
    pub worktrees: Vec<Worktree>,
    pub cwd: PathBuf,
    /// A Git source not fetched yet: the sandbox fetches it into the job's
    /// volumes from a container without the Docker socket (L13).
    pub fetch: Option<GitFetch>,
}

pub struct GitFetch {
    pub repository: String,
    pub revision: String,
    pub credentials: Option<crate::git_source::GitCredentials>,
}

impl Workspace {
    pub async fn from_source(
        attempt_dir: &Path,
        source: &crate::source::JobSource,
    ) -> Result<Self> {
        if let crate::source::JobSource::Git {
            repository,
            revision,
            credentials,
        } = source
        {
            crate::git_source::validate(repository, revision, credentials.as_ref())?;
            let target = attempt_dir.join("workspace");
            return Ok(Self {
                cwd: target.clone(),
                worktrees: vec![Worktree {
                    root_name: "workspace".into(),
                    path: target,
                    // The fetch checks that the commit it got is this one.
                    base_revision: revision.to_ascii_lowercase(),
                }],
                fetch: Some(GitFetch {
                    repository: repository.clone(),
                    revision: revision.clone(),
                    credentials: credentials.clone(),
                }),
            });
        }
        let target = attempt_dir.join("workspace");
        source.materialize(&target).await?;
        let prepared = async {
            git_ok(&target, &["init", "--quiet"]).await?;
            git_ok(&target, &["config", "user.name", "poundlock agent"]).await?;
            git_ok(
                &target,
                &["config", "user.email", "agent@poundlock.invalid"],
            )
            .await?;
            git_ok(&target, &["add", "--force", "--all"]).await?;
            git_ok(
                &target,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "Submitted workspace",
                ],
            )
            .await?;
            git_text(&target, &["rev-parse", "HEAD"]).await
        }
        .await;
        let base_revision = match prepared {
            Ok(revision) => revision.trim().to_owned(),
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&target).await;
                return Err(error);
            }
        };
        Ok(Self {
            cwd: target.clone(),
            worktrees: vec![Worktree {
                root_name: "workspace".into(),
                path: target,
                base_revision,
            }],
            fetch: None,
        })
    }
    pub async fn create(projects_dir: &Path, attempt_dir: &Path, roots: &[String]) -> Result<Self> {
        let allowed = tokio::fs::canonicalize(projects_dir)
            .await
            .with_context(|| {
                format!(
                    "projects directory {} is unavailable",
                    projects_dir.display()
                )
            })?;
        let roots_dir = attempt_dir.join("roots");
        tokio::fs::create_dir_all(&roots_dir).await?;
        let mut worktrees = Vec::new();
        for (index, relative) in roots.iter().enumerate() {
            match create_worktree(&allowed, &roots_dir, relative, index).await {
                Ok(worktree) => worktrees.push(worktree),
                Err(error) => {
                    let partial = Self {
                        worktrees,
                        cwd: roots_dir.clone(),
                        fetch: None,
                    };
                    let cleanup = partial.cleanup().await;
                    if cleanup.is_empty() {
                        return Err(error);
                    }
                    bail!("{error}; partial cleanup failed: {}", cleanup.join("; "));
                }
            }
        }
        let cwd = if worktrees.len() == 1 {
            worktrees[0].path.clone()
        } else {
            roots_dir
        };
        Ok(Self {
            worktrees,
            cwd,
            fetch: None,
        })
    }

    pub async fn cleanup(&self) -> Vec<String> {
        let mut failures = Vec::new();
        for worktree in &self.worktrees {
            match tokio::fs::remove_dir_all(&worktree.path).await {
                Ok(()) => {}
                // A Git source is fetched into the job's volumes, never here (L13).
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => failures.push(format!("{}: {error}", worktree.root_name)),
            }
        }
        failures
    }
}

async fn create_worktree(
    allowed: &Path,
    roots_dir: &Path,
    relative: &str,
    index: usize,
) -> Result<Worktree> {
    let source = tokio::fs::canonicalize(allowed.join(relative))
        .await
        .with_context(|| format!("project root '{relative}' is unavailable to the runner"))?;
    if !source.starts_with(allowed) {
        bail!("project root '{relative}' escapes the allowed directory");
    }
    let repository_root = git_text(&source, &["rev-parse", "--show-toplevel"]).await?;
    if tokio::fs::canonicalize(repository_root.trim()).await? != source {
        bail!("project root '{relative}' must name a Git repository root");
    }
    let base_revision = git_text(&source, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_owned();
    let root_name = slug(relative, index);
    let target = roots_dir.join(&root_name);
    // Independent objects and metadata prevent an agent's Git commands from
    // changing the source repository through a shared worktree common-dir.
    let created = git_ok(
        &source,
        &[
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--no-checkout",
            "--",
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    )
    .await;
    if let Err(error) = created {
        let _ = tokio::fs::remove_dir_all(&target).await;
        return Err(error).with_context(|| format!("copy repository '{relative}'"));
    }
    if let Err(error) = async {
        git_ok(&target, &["checkout", "--detach", &base_revision]).await?;
        git_ok(&target, &["remote", "remove", "origin"]).await?;
        git_ok(&target, &["config", "user.name", "poundlock agent"]).await?;
        git_ok(
            &target,
            &["config", "user.email", "agent@poundlock.invalid"],
        )
        .await
    }
    .await
    {
        let cleanup = tokio::fs::remove_dir_all(&target).await;
        return Err(error)
            .with_context(|| format!("prepare repository '{relative}'; cleanup: {cleanup:?}"));
    }
    Ok(Worktree {
        root_name,
        path: target,
        base_revision,
    })
}

/// The configuration patch collection runs under, shared so a test exercises
/// the arguments production uses rather than a copy of them.
///
/// The agent's own `.git` is never consulted: its config, index, hooks,
/// fsmonitor and filters are all bypassed. `core.excludesFile` supplies the
/// floor that keeps generated files out of the diff — see
/// `runner_sandbox::DEFAULT_PATCH_EXCLUDES`.
pub fn patch_collection_config(excludes_file: &Path) -> Vec<String> {
    vec![
        "-c".into(),
        "core.bare=false".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.attributesFile=/dev/null".into(),
        "-c".into(),
        format!("core.excludesFile={}", excludes_file.display()),
    ]
}

pub(crate) fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
        .arg("-C")
        .arg(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    command
}

// Git may spawn transport helpers. Cancelling preparation must stop the entire
// owned group, including when the parent exits before a helper closes its pipes.
pub(crate) async fn git_output(command: Command) -> Result<Output> {
    git_output_limited(command, 128 * 1024 * 1024).await
}

pub(crate) async fn git_output_limited(command: Command, limit: usize) -> Result<Output> {
    git_output_with_limits(command, limit, None).await
}

async fn git_output_with_limits(
    mut command: Command,
    limit: usize,
    stdout_record_limit: Option<usize>,
) -> Result<Output> {
    let mut child = command.spawn()?;
    let _group = crate::process::ProcessGroup::new(&child)?;
    let stdout = child.stdout.take().context("Git stdout is not piped")?;
    let stderr = child.stderr.take().context("Git stderr is not piped")?;
    // Read both pipes concurrently. An overflow cancels the other reads and
    // drops the group guard, including transport helpers holding pipes open.
    let (status, stdout, stderr) = tokio::try_join!(
        async { Ok::<_, anyhow::Error>(child.wait().await?) },
        read_git_pipe(stdout, limit, stdout_record_limit, "stdout"),
        read_git_pipe(stderr, 64 * 1024, None, "stderr"),
    )?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_git_pipe(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
    record_limit: Option<usize>,
    label: &str,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut records = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if read > limit.saturating_sub(bytes.len()) {
            bail!("Git {label} exceeds the {limit}-byte output limit");
        }
        if let Some(record_limit) = record_limit {
            records += chunk[..read].iter().filter(|byte| **byte == 0).count();
            if records > record_limit {
                bail!("Git {label} exceeds the {record_limit}-record output limit");
            }
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(bytes)
}

pub(crate) async fn git_text(cwd: &Path, args: &[&str]) -> Result<String> {
    git_text_limited(cwd, args, 128 * 1024 * 1024).await
}

pub(crate) async fn git_text_limited(cwd: &Path, args: &[&str], limit: usize) -> Result<String> {
    git_text_with_limits(cwd, args, limit, None).await
}

pub(crate) async fn git_text_records_limited(
    cwd: &Path,
    args: &[&str],
    byte_limit: usize,
    record_limit: usize,
) -> Result<String> {
    git_text_with_limits(cwd, args, byte_limit, Some(record_limit)).await
}

async fn git_text_with_limits(
    cwd: &Path,
    args: &[&str],
    byte_limit: usize,
    record_limit: Option<usize>,
) -> Result<String> {
    let mut command = git_command(cwd);
    command.args(args);
    let output = git_output_with_limits(command, byte_limit, record_limit).await?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), bounded(&output.stderr));
    }
    String::from_utf8(output.stdout).context("git output is not UTF-8")
}

pub(crate) async fn git_ok(cwd: &Path, args: &[&str]) -> Result<()> {
    git_text(cwd, args).await.map(|_| ())
}

fn slug(relative: &str, index: usize) -> String {
    let clean = relative
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("{:02}-{}", index + 1, clean.trim_matches('-'))
}

fn bounded(bytes: &[u8]) -> String {
    let bytes = if bytes.len() > 4096 {
        &bytes[bytes.len() - 4096..]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    #[tokio::test]
    async fn output_overflow_kills_children_for_both_pipes() {
        for pipe in ["stdout", "stderr"] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("survived");
            // Exercise the capture guard without contacting a Git server.
            let mut command = Command::new("sh");
            command.args(["-c", "(sleep 1; touch \"$1\") & if [ \"$2\" = stderr ]; then exec 1>&2; fi; while :; do printf '0123456789abcdef'; done", "fixture"])
                .arg(&marker).arg(pipe)
                .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                git_output_limited(command, 1024),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(error.to_string().contains(pipe), "{error}");
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            assert!(!marker.exists(), "descendant survived output overflow");
        }
    }

    #[tokio::test]
    async fn record_and_byte_limits_apply_independently() {
        // Many tiny records trip the record limit well below the byte limit.
        let records = [0u8; 11];
        let error = read_git_pipe(&records[..], 1024, Some(10), "stdout")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("10-record"), "{error}");
        // Exactly at the record limit is accepted.
        assert_eq!(
            read_git_pipe(&records[..10], 1024, Some(10), "stdout")
                .await
                .unwrap()
                .len(),
            10
        );
        // One large record still trips the byte limit.
        let large = [b'a'; 2048];
        let error = read_git_pipe(&large[..], 1024, Some(10), "stdout")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("1024-byte"), "{error}");
        // Without a record limit, record separators are not counted.
        assert!(
            read_git_pipe(&[0u8; 100][..], 1024, None, "stdout")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cancelled_git_operation_kills_transport_descendants() {
        use std::os::unix::process::CommandExt;
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("survived");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "trap '' TERM; (trap '' TERM; sleep 1; printf survived > \"$1\") & wait",
                "fixture",
            ])
            .arg(&marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), git_output(command))
                .await
                .is_err()
        );
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !marker.exists(),
            "transport descendant survived preparation cancellation"
        );
    }
}
