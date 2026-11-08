//! The files of one approved folder, ready to upload as a job's source.
//!
//! A Git folder contributes what Git would: tracked files plus untracked files
//! its ignore rules allow. Any other folder is walked, skipping dependency and
//! build directories. Either way, credential files never leave the machine,
//! symbolic links are never followed, and the controller's own limits (10,000
//! files, 50 MiB) are checked before anything is sent.
//!
//! The credential list matches Poundlock Dispatch's `SECRET_PATHS`, so the two
//! clients upload the same thing from the same folder.

mod secrets;
pub use secrets::is_secret;
#[cfg(test)]
mod tests;

use crate::source::{MAX_SOURCE_BYTES, SourceFile};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The most files a job's source may hold.
pub const MAX_SOURCE_FILES: usize = 10_000;

/// Dependency, build and cache directories, skipped when a folder has no Git
/// ignore rules to say so.
const WALK_SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "target",
    ".build",
    ".dispatch",
    ".venv",
    "venv",
    "__pycache__",
];

const WALK_SKIPPED_FILES: &[&str] = &[".DS_Store"];

/// A folder's files, and what was left out and why.
#[derive(Debug)]
pub struct Snapshot {
    pub files: Vec<SourceFile>,
    pub bytes: u64,
    /// SHA-256 over every path, mode and content, in path order.
    pub digest: String,
    /// Credential files left out, by path. Never their contents.
    pub secrets: Vec<String>,
    /// Paths left out for another reason: a link, a special file, a name the
    /// controller cannot accept.
    pub skipped: Vec<(String, &'static str)>,
    /// Whether the folder's own Git rules chose the files.
    pub from_git: bool,
}

/// Collects `root`, which must be a directory the caller has already checked
/// it may upload.
pub fn collect(root: &Path) -> Result<Snapshot> {
    let listed = match git_listing(root)? {
        Some(paths) => (paths, true),
        None => (walk(root)?, false),
    };
    let (mut paths, from_git) = listed;
    paths.sort();
    paths.dedup();
    let mut snapshot = Snapshot {
        files: Vec::new(),
        bytes: 0,
        digest: String::new(),
        secrets: Vec::new(),
        skipped: Vec::new(),
        from_git,
    };
    let mut digest = Sha256::new();
    for path in paths {
        if is_secret(&path) {
            snapshot.secrets.push(path);
            continue;
        }
        if crate::source::validate_path(&path).is_err() {
            snapshot.skipped.push((path, "unsupported_path"));
            continue;
        }
        let full = root.join(&path);
        // `symlink_metadata`, so a link is seen as a link and never followed.
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            // A tracked file deleted in the working tree is simply absent.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        if metadata.file_type().is_symlink() {
            snapshot.skipped.push((path, "symbolic_link"));
            continue;
        }
        if !metadata.is_file() {
            snapshot.skipped.push((path, "not_a_regular_file"));
            continue;
        }
        if snapshot.files.len() == MAX_SOURCE_FILES {
            bail!(
                "This folder has more than {MAX_SOURCE_FILES} files to upload; choose a smaller folder."
            );
        }
        let content = std::fs::read(&full).with_context(|| format!("read {path}"))?;
        snapshot.bytes += content.len() as u64;
        if snapshot.bytes > MAX_SOURCE_BYTES as u64 {
            bail!(
                "This folder has more than 50 MiB to upload; choose a smaller folder or add ignore rules."
            );
        }
        let executable = executable(&metadata);
        digest.update(path.as_bytes());
        digest.update([0, u8::from(executable)]);
        digest.update((content.len() as u64).to_be_bytes());
        digest.update(&content);
        snapshot.files.push(SourceFile {
            path,
            content_base64: STANDARD.encode(&content),
            executable,
        });
    }
    if snapshot.files.is_empty() {
        bail!("There are no files to upload in this folder.");
    }
    snapshot.digest = crate::crypto::to_hex(&digest.finalize());
    Ok(snapshot)
}

/// The paths `collect` considers, sorted: Git's choice in a Git folder, a
/// walk anywhere else.
pub fn listing(root: &Path) -> Result<Vec<String>> {
    let mut paths = match git_listing(root)? {
        Some(paths) => paths,
        None => walk(root)?,
    };
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(unix)]
pub(super) fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(super) fn executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// What Git would carry from `root`: tracked files and untracked files its
/// ignore rules allow, relative to `root`. `None` outside a Git work tree.
fn git_listing(root: &Path) -> Result<Option<Vec<String>>> {
    let inside = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    match inside {
        Ok(output) if output.status.success() && output.stdout.starts_with(b"true") => {}
        _ => return Ok(None),
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        // Plain paths, whatever the user's quoting setting.
        .args(["-c", "core.quotePath=false"])
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .context("list the folder's files with Git")?;
    if !output.status.success() {
        bail!(
            "Git could not list this folder's files: {}",
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(512)])
        );
    }
    Ok(Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
    ))
}

fn walk(root: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(relative) = pending.pop() {
        let directory = if relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&relative)
        };
        let entries = std::fs::read_dir(&directory)
            .with_context(|| format!("read folder {}", directory.display()))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !WALK_SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                    pending.push(path);
                }
            } else if !WALK_SKIPPED_FILES.contains(&name.as_str()) {
                found.push(path);
            }
            if found.len() > MAX_SOURCE_FILES * 2 {
                bail!(
                    "This folder has far more than {MAX_SOURCE_FILES} files; choose a smaller folder."
                );
            }
        }
    }
    Ok(found)
}
