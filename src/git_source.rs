use crate::git::{
    Workspace, Worktree, git_command, git_output, git_text, git_text_records_limited,
};
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCredentials {
    pub username: String,
    pub token: String,
}
impl std::fmt::Debug for GitCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitCredentials([redacted])")
    }
}

pub fn validate(
    repository: &str,
    revision: &str,
    credentials: Option<&GitCredentials>,
) -> Result<()> {
    let url = reqwest::Url::parse(repository)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() == "/"
        || repository.len() > 2048
        || repository.chars().any(char::is_control)
        // The allowlist reads the URL as a browser would, git as curl would.
        // They disagree about a backslash in the authority
        // (`https://github.com\@elsewhere/`), so it is refused outright.
        || repository.contains('\\')
        || repository.chars().any(char::is_whitespace)
    {
        bail!("Use an HTTPS repository URL without embedded credentials, query or custom port.");
    }
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Git revision must be a full 40-character commit ID, not a branch or tag.");
    }
    if let Some(credentials) = credentials
        && (credentials.username.is_empty()
            || credentials.username.contains([':', '\r', '\n'])
            || credentials.token.is_empty()
            || credentials.token.len() > 8192
            || credentials.token.contains(['\r', '\n']))
    {
        bail!("Git credentials require a username and a token without newlines.");
    }
    Ok(())
}

pub fn host_allowed(repository: &str, hosts: &[String]) -> bool {
    reqwest::Url::parse(repository)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            hosts
                .iter()
                .any(|allowed| host.eq_ignore_ascii_case(allowed))
        })
}

pub async fn checkout(
    attempt: &Path,
    repository: &str,
    revision: &str,
    credentials: Option<&GitCredentials>,
) -> Result<Workspace> {
    validate(repository, revision, credentials)?;
    // Git receives the same parsed URL whose host the allowlist approved.
    let repository = reqwest::Url::parse(repository)?;
    let repository = repository.as_str();
    let target = attempt.join("workspace");
    tokio::fs::create_dir(&target).await?;
    let result = async {
        git_text(&target, &["init", "--quiet"]).await?;
        let mut fetch = git_command(&target);
        // git_command clears the environment; in the fetch container the way out
        // is the supervisor's egress proxy, so name it explicitly (L13).
        if let Some(proxy) = fetch_proxy() {
            fetch.arg("-c").arg(format!("http.proxy={proxy}"));
        }
        fetch.args(["-c", "protocol.allow=never", "-c", "protocol.https.allow=always",
            "-c", "http.followRedirects=false", "-c", "credential.helper=",
            // Malformed objects are refused as they arrive, before checkout (L13).
            "-c", "transfer.fsckObjects=true",
            "fetch", "--quiet", "--no-tags", "--depth=1", "--no-recurse-submodules", "--", repository, revision]);
        if let Some(credentials) = credentials {
            // Command-scoped Git configuration: never stored in argv, .git/config or the workspace.
            fetch.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {}", STANDARD.encode(format!("{}:{}", credentials.username, credentials.token))));
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(300), git_output(fetch)).await??;
        if !output.status.success() {
            // Remote stderr is untrusted and could echo credentials supplied by a server.
            bail!("Read-only Git fetch failed. Check the approved host, commit ID and repository-read credential.");
        }
        finish_checkout(&target, revision).await
    }.await;
    if result.is_err() {
        tokio::fs::remove_dir_all(&target).await?;
    }
    result
}

/// The proxy the environment names for HTTPS, if any: the fetch container's
/// egress proxy, which reaches only the repository's host.
fn fetch_proxy() -> Option<String> {
    ["HTTPS_PROXY", "https_proxy"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| value.starts_with("http://") && !value.contains(char::is_whitespace))
}

pub async fn finish_checkout(target: &Path, revision: &str) -> Result<Workspace> {
    let actual = git_text(target, &["rev-parse", "FETCH_HEAD^{commit}"]).await?;
    if !actual.trim().eq_ignore_ascii_case(revision) {
        bail!("Fetched Git commit does not match the accepted revision.");
    }
    // Reused tree objects can expand a small fetch into a huge recursive listing.
    // Bound that expansion before retaining output or materializing any files.
    let tree = git_text_records_limited(
        target,
        &["ls-tree", "-r", "-l", "-z", actual.trim()],
        16 * 1024 * 1024,
        10_000,
    )
    .await?;
    let mut paths = std::collections::BTreeSet::new();
    let mut total = 0usize;
    let mut count = 0usize;
    for entry in tree.split('\0').filter(|entry| !entry.is_empty()) {
        let (metadata, path) = entry
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("Invalid Git tree"))?;
        let fields: Vec<_> = metadata.split_whitespace().collect();
        if fields.len() != 4 || !matches!(fields[0], "100644" | "100755") {
            bail!(
                "Git intake supports regular files only; supply a resolved snapshot for symlinks or submodules."
            );
        }
        crate::source::validate_path(path)?;
        if !paths.insert(path.to_lowercase()) {
            bail!("Git checkout contains case-aliased paths; use an unambiguous snapshot.");
        }
        total = total
            .checked_add(fields[3].parse::<usize>()?)
            .ok_or_else(|| anyhow::anyhow!("Git checkout size exceeds the source limit"))?;
        count += 1;
        if total > crate::source::MAX_SOURCE_BYTES || count > 10_000 {
            bail!("Git checkout exceeds the 50 MiB or 10,000-file limit.");
        }
    }
    for path in &paths {
        for (index, _) in path.match_indices('/') {
            if paths.contains(&path[..index]) {
                bail!("Git checkout contains a file/directory path alias.");
            }
        }
    }
    git_text(target, &["checkout", "--quiet", "--detach", actual.trim()]).await?;
    // Missing LFS content must never silently become an apparently complete workspace.
    let files = git_text(target, &["ls-files", "-z"]).await?;
    for path in files.split('\0').filter(|path| !path.is_empty()) {
        use tokio::io::AsyncReadExt;
        let mut prefix = [0u8; 128];
        let mut file = tokio::fs::File::open(target.join(path)).await?;
        let length = file.read(&mut prefix).await?;
        if prefix[..length].starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
            bail!(
                "Git LFS objects require a resolved folder snapshot; pointer-only checkout rejected."
            );
        }
    }
    git_text(target, &["config", "user.name", "poundlock agent"]).await?;
    git_text(target, &["config", "user.email", "agent@poundlock.invalid"]).await?;
    Ok(Workspace {
        cwd: target.to_owned(),
        worktrees: vec![Worktree {
            root_name: "workspace".into(),
            path: target.to_owned(),
            base_revision: actual.trim().into(),
        }],
        fetch: None,
    })
}

/// `poundlock fetch-source`, run inside the job's fetch container: check out
/// the commit into `into/workspace`, then copy it to `copy_to/workspace`. Only
/// regular files and directories are copied.
pub async fn fetch_source(
    into: &Path,
    copy_to: &Path,
    repository: &str,
    revision: &str,
    credentials: Option<&GitCredentials>,
) -> Result<String> {
    let workspace = checkout(into, repository, revision, credentials).await?;
    let base = workspace.worktrees[0].base_revision.clone();
    copy_tree(&into.join("workspace"), &copy_to.join("workspace"))?;
    Ok(base)
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
            let mode = entry.metadata()?.permissions().mode() & 0o755;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        } else {
            bail!("Git checkout produced a special file");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fetch_takes_the_environments_https_proxy() {
        // SAFETY: this test alone sets these variables in this process.
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("https_proxy");
        }
        assert_eq!(fetch_proxy(), None);
        unsafe { std::env::set_var("HTTPS_PROXY", "http://poundlock-supervisor:7333") };
        assert_eq!(
            fetch_proxy().as_deref(),
            Some("http://poundlock-supervisor:7333")
        );
        unsafe { std::env::set_var("HTTPS_PROXY", "http://evil host") };
        assert_eq!(fetch_proxy(), None);
        unsafe { std::env::remove_var("HTTPS_PROXY") };
    }

    #[test]
    fn the_copy_keeps_files_and_modes_and_refuses_links() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let from = directory.path().join("from");
        std::fs::create_dir_all(from.join(".git/objects")).unwrap();
        std::fs::write(from.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(from.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(from.join(".git/objects/pack"), "pack").unwrap();
        copy_tree(&from, &directory.path().join("to")).unwrap();
        let copied = directory.path().join("to");
        assert_eq!(
            std::fs::read(copied.join(".git/objects/pack")).unwrap(),
            b"pack"
        );
        let mode = std::fs::metadata(copied.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);

        std::os::unix::fs::symlink("/etc/passwd", from.join("link")).unwrap();
        assert!(copy_tree(&from, &directory.path().join("again")).is_err());
    }
    #[test]
    fn remote_policy_rejects_mutable_refs_credentials_and_unapproved_hosts() {
        let commit = "a".repeat(40);
        for url in [
            "http://github.com/a/b",
            "https://token@github.com/a/b",
            "https://github.com:8443/a/b",
            "file:///etc/passwd",
            "https://github.com/a/b?token=x",
        ] {
            assert!(validate(url, &commit, None).is_err());
        }
        // A backslash reads as a path to the allowlist but as userinfo to curl.
        let smuggled = "https://github.com\\@evil.invalid/a/b";
        assert!(host_allowed(smuggled, &["github.com".into()]));
        assert!(validate(smuggled, &commit, None).is_err());
        assert!(validate("https://github.com/a/b\\c", &commit, None).is_err());
        assert!(validate("https://github.com/a /b", &commit, None).is_err());
        assert!(validate("https://github.com/a/b", "main", None).is_err());
        assert!(validate("https://github.com/a/b", &commit, None).is_ok());
        assert!(!host_allowed(
            "https://github.com.evil.invalid/a/b",
            &["github.com".into()]
        ));
        assert!(host_allowed(
            "https://gitea.example.com/owner/repo.git",
            &["gitea.example.com".into()]
        ));
        assert!(!host_allowed(
            "https://gitea.example.com/owner/repo.git",
            &["github.com".into(), "gitlab.com".into()]
        ));
        let secret = GitCredentials {
            username: "user".into(),
            token: "private-token".into(),
        };
        assert!(!format!("{secret:?}").contains("private-token"));
    }
}
