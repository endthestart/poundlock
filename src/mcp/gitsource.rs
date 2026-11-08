//! A job's source as a pushed Git commit instead of an upload: for a
//! repository too large to send, or when the exact commit matters. The runner
//! fetches the commit itself from an approved HTTPS host, so the folder must be
//! a repository's top folder with nothing uncommitted, and its commit must
//! already be on a remote.

use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct GitSource {
    /// The HTTPS URL the runner fetches from.
    pub repository: String,
    /// The full commit ID.
    pub revision: String,
    pub host: String,
    pub top: PathBuf,
}

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .output()
        .context("run git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The HTTPS form of a remote URL: `git@host:owner/repo.git` becomes
/// `https://host/owner/repo.git`. Anything else must already be HTTPS.
pub fn https_remote(url: &str) -> Result<String> {
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
        && !host.contains('/')
        && !path.starts_with('/')
    {
        return Ok(format!("https://{host}/{path}"));
    }
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/').context("the remote URL has no path")?;
        let host = host.split(':').next().unwrap_or(host);
        return Ok(format!("https://{host}/{path}"));
    }
    ensure!(
        url.starts_with("https://"),
        "the remote {url} is not HTTPS or GitHub-style SSH; upload the folder instead"
    );
    Ok(url.to_owned())
}

/// The pushed commit `directory` is at, refusing anything the runner could not
/// fetch exactly.
pub fn resolve(directory: &Path) -> Result<GitSource> {
    let top = PathBuf::from(git(directory, &["rev-parse", "--show-toplevel"])?);
    ensure!(
        std::fs::canonicalize(&top)? == std::fs::canonicalize(directory)?,
        "a Git source is the whole repository: use its top folder, {}",
        top.display()
    );
    let status = git(
        &top,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )?;
    if !status.is_empty() {
        let changes: Vec<&str> = status.lines().take(5).collect();
        bail!(
            "the folder has changes that are not committed and pushed ({}{}); the runner would not see them. Commit and push them, or upload the folder instead",
            changes.join(", "),
            if status.lines().count() > 5 {
                ", …"
            } else {
                ""
            }
        );
    }
    let revision = git(&top, &["rev-parse", "HEAD"])?;
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "HEAD is not a commit"
    );
    let remote = git(
        &top,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .and_then(|upstream| {
        upstream
            .split_once('/')
            .map(|(remote, _)| remote.to_owned())
    })
    .unwrap_or_else(|| "origin".into());
    let url = git(&top, &["remote", "get-url", &remote])
        .with_context(|| format!("the repository has no remote named {remote}"))?;
    let repository = https_remote(&url)?;
    let host = reqwest::Url::parse(&repository)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .context("the remote URL has no host")?;
    let pushed = git(&top, &["branch", "-r", "--contains", "HEAD"])?;
    ensure!(
        !pushed.is_empty(),
        "commit {} is not on any remote branch Git knows of; push it first (or fetch, if it was pushed elsewhere)",
        &revision[..12]
    );
    crate::git_source::validate(&repository, &revision, None)?;
    Ok(GitSource {
        repository,
        revision,
        host,
        top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_become_https() {
        assert_eq!(
            https_remote("git@github.com:acme/app.git").unwrap(),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            https_remote("ssh://git@git.example.net:2222/acme/app.git").unwrap(),
            "https://git.example.net/acme/app.git"
        );
        assert_eq!(
            https_remote("https://github.com/acme/app.git").unwrap(),
            "https://github.com/acme/app.git"
        );
        assert!(https_remote("/srv/git/app.git").is_err());
        assert!(https_remote("http://github.com/acme/app.git").is_err());
    }
}
