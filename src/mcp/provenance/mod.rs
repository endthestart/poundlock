//! Which changes in a folder came from Poundlock jobs. Each `apply_patch`
//! records the folder's files just before and just after the patch, by content
//! hash, in the folder's Git directory. Comparing one patch's "after" with the
//! next one's "before", and the last "after" with the folder now, finds every
//! file that changed some other way: by the agent's own hand, a formatter, or
//! the owner.
//!
//! This is a record for review, not a proof: an agent with a shell could edit
//! it. It never holds file contents, only paths and hashes.

mod report;
pub use report::Report;
mod record;
pub use record::{JobPatch, Pending, begin, finish};
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The most files one record describes.
const MAX_MANIFEST_FILES: usize = 50_000;

/// Path to the SHA-256 of a file's mode and content.
pub type Manifest = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Applied {
    pub at: String,
    pub job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    pub index: usize,
    /// The folder the patch went to, relative to the repository's top ("." for
    /// the top itself).
    pub directory: String,
    pub patch_sha256: String,
    pub files: Vec<String>,
    /// Digests of the folder's manifest before and after the patch.
    pub before: String,
    pub after: String,
    /// The job's own check result when its patch was applied: `passed`,
    /// `failed`, `unverified`, or absent when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// Files that changed other than by a job patch, between two points.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutsideChange {
    /// The job whose patch came before the change.
    pub after_job: String,
    /// The job whose patch came after it, or none for changes since the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_job: Option<String>,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

/// Where a folder's records live: its repository's Git directory, and the
/// folder relative to the repository's top. `None` outside Git.
pub fn location(directory: &Path) -> Result<Option<(PathBuf, String)>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["rev-parse", "--absolute-git-dir", "--show-prefix"])
        .output()
        .context("run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let git_dir = lines.next().context("git gave no Git directory")?.trim();
    let prefix = lines.next().unwrap_or("").trim().trim_end_matches('/');
    Ok(Some((
        PathBuf::from(git_dir).join("poundlock"),
        if prefix.is_empty() {
            ".".to_owned()
        } else {
            prefix.to_owned()
        },
    )))
}

/// The folder's files as the upload sees them, by content hash, without
/// credential files.
pub fn manifest(directory: &Path) -> Result<Manifest> {
    let mut manifest = Manifest::new();
    for path in super::snapshot::listing(directory)? {
        if super::snapshot::is_secret(&path) {
            continue;
        }
        let full = directory.join(&path);
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        let mut hash = Sha256::new();
        if metadata.file_type().is_symlink() {
            hash.update(b"link\0");
            hash.update(
                std::fs::read_link(&full)
                    .with_context(|| format!("read {path}"))?
                    .to_string_lossy()
                    .as_bytes(),
            );
        } else if metadata.is_file() {
            hash.update([u8::from(super::snapshot::executable(&metadata)), 0]);
            hash.update(std::fs::read(&full).with_context(|| format!("read {path}"))?);
        } else {
            continue;
        }
        ensure!(
            manifest.len() < MAX_MANIFEST_FILES,
            "the folder has more than {MAX_MANIFEST_FILES} files to record"
        );
        manifest.insert(path, crate::crypto::to_hex(&hash.finalize()));
    }
    Ok(manifest)
}

fn digest(manifest: &Manifest) -> String {
    let mut hash = Sha256::new();
    for (path, file) in manifest {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(file.as_bytes());
        hash.update([0]);
    }
    crate::crypto::to_hex(&hash.finalize())
}

/// Saves a manifest once, by its digest, and returns the digest.
fn store(records: &Path, manifest: &Manifest) -> Result<String> {
    let digest = digest(manifest);
    let directory = records.join("manifests");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let path = directory.join(format!("{digest}.json"));
    if !path.exists() {
        let temporary = directory.join(format!(".{digest}.{}", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(manifest)?)?;
        std::fs::rename(&temporary, &path)?;
    }
    Ok(digest)
}

fn load(records: &Path, digest: &str) -> Option<Manifest> {
    ensure_hex(digest).ok()?;
    let text = std::fs::read(records.join("manifests").join(format!("{digest}.json"))).ok()?;
    serde_json::from_slice(&text).ok()
}

fn ensure_hex(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "not a digest"
    );
    Ok(())
}

fn compare(
    after_job: &str,
    before_job: Option<&str>,
    from: &Manifest,
    to: &Manifest,
) -> Option<OutsideChange> {
    let mut gap = OutsideChange {
        after_job: after_job.to_owned(),
        before_job: before_job.map(str::to_owned),
        added: Vec::new(),
        changed: Vec::new(),
        removed: Vec::new(),
    };
    for (path, hash) in to {
        match from.get(path) {
            None => gap.added.push(path.clone()),
            Some(old) if old != hash => gap.changed.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in from.keys() {
        if !to.contains_key(path) {
            gap.removed.push(path.clone());
        }
    }
    (!gap.added.is_empty() || !gap.changed.is_empty() || !gap.removed.is_empty()).then_some(gap)
}

/// Every patch applied to `directory` through Poundlock, and every change
/// made to it some other way since the first.
pub fn report(directory: &Path) -> Result<Report> {
    let shown = std::fs::canonicalize(directory)
        .unwrap_or_else(|_| directory.to_path_buf())
        .display()
        .to_string();
    let Some((records, relative)) = location(directory)? else {
        return Ok(Report {
            directory: shown,
            applied: Vec::new(),
            outside: Vec::new(),
            unreadable: 0,
        });
    };
    let applied: Vec<Applied> = match std::fs::read_to_string(records.join("applied.jsonl")) {
        Ok(text) => text
            .lines()
            .filter_map(|line| serde_json::from_str::<Applied>(line).ok())
            .filter(|entry| entry.directory == relative)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).context("read the provenance record"),
    };
    let mut outside = Vec::new();
    let mut unreadable = 0;
    for pair in applied.windows(2) {
        if pair[0].after == pair[1].before {
            continue;
        }
        match (
            load(&records, &pair[0].after),
            load(&records, &pair[1].before),
        ) {
            (Some(from), Some(to)) => {
                outside.extend(compare(&pair[0].job_id, Some(&pair[1].job_id), &from, &to))
            }
            _ => unreadable += 1,
        }
    }
    if let Some(last) = applied.last() {
        let now = manifest(directory)?;
        if digest(&now) != last.after {
            match load(&records, &last.after) {
                Some(from) => outside.extend(compare(&last.job_id, None, &from, &now)),
                None => unreadable += 1,
            }
        }
    }
    Ok(Report {
        directory: shown,
        applied,
        outside,
        unreadable,
    })
}
