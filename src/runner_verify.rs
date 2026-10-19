use crate::{
    domain::RootPatch,
    job_policy::{JobPolicy, VerificationReport},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(super) struct Root {
    pub name: String,
    pub path: PathBuf,
    pub base: String,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Manifest {
    pub roots: Vec<Root>,
    pub cwd: PathBuf,
    pub policy: JobPolicy,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Candidate {
    pub patches: Vec<RootPatch>,
    pub verification: VerificationReport,
}

pub(super) fn manifest() -> Result<Manifest> {
    Ok(serde_json::from_slice(&fs::read(
        "/trusted/poundlock-roots.json",
    )?)?)
}

/// This entry point runs only in the collector after the agent has been removed.
/// Its manifest and baseline are never mounted into the agent or verifier.
pub async fn inspect_candidate() -> Result<serde_json::Value> {
    let manifest = manifest()?;
    let super::sandbox::Collected {
        patches,
        changed_paths,
    } = super::sandbox::collect_patches().await?;
    let mut verification = VerificationReport::not_run("No verification commands were supplied.");
    verification.candidate_sha256 = Some(crate::crypto::sha256_hex(&serde_json::to_vec(&patches)?));
    verification.protected_paths = manifest.policy.verification.protected_paths.clone();
    for (root, changed) in manifest.roots.iter().zip(&changed_paths) {
        for path in host_sensitive(changed) {
            verification
                .flagged_paths
                .push(if manifest.roots.len() > 1 {
                    format!("{}: {path}", root.name)
                } else {
                    path
                });
        }
    }
    let inspection = async {
        for path in &verification.protected_paths {
            let original = contained_entry(Path::new("/trusted"), &manifest.cwd.join(path.path()))?;
            ensure!(
                fs::symlink_metadata(&original).is_ok(),
                "Protected path does not exist in the original source: {}",
                path.path()
            );
            let candidate = contained_entry(Path::new("/work"), &manifest.cwd.join(path.path()))?;
            let original_fingerprint = fingerprint(&original)?;
            let candidate_fingerprint = fingerprint(&candidate)?;
            // The same path as the patch names it, within its root.
            let aliased = manifest
                .roots
                .iter()
                .zip(&changed_paths)
                .any(|(root, changed)| {
                    manifest
                        .cwd
                        .join(path.path())
                        .strip_prefix(&root.path)
                        .is_ok_and(|guarded| {
                            let guarded = guarded
                                .components()
                                .map(|part| part.as_os_str().to_string_lossy())
                                .collect::<Vec<_>>()
                                .join("/");
                            aliases(path.mode(), &guarded, &original_fingerprint, changed)
                        })
                });
            if aliased
                || crate::job_policy::protected_path_changed(
                    path.mode(),
                    &original_fingerprint,
                    &candidate_fingerprint,
                )
            {
                verification
                    .changed_protected_paths
                    .push(path.path().to_owned());
            }
        }
        if !verification.changed_protected_paths.is_empty() {
            verification.status = "failed".into();
            verification.reason =
                "Protected files changed; verification commands were skipped.".into();
            verification.failure_code = Some("protected_paths_changed".into());
            return Ok(());
        }
        if manifest.policy.verification.commands.is_empty() {
            return Ok(());
        }
        // Rebuild the deliverable, not the agent's directory (which may contain
        // ignored dependencies or generated files absent from its patch).
        for (root, patch) in manifest.roots.iter().zip(&patches) {
            let target = Path::new("/verify").join(&root.path);
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
            copy_tree(
                &Path::new("/trusted").join(&root.path),
                &target,
                &mut (0, 0),
            )?;
            if !patch.patch.is_empty() {
                let patch_file = Path::new("/tmp/candidate.patch");
                fs::write(patch_file, &patch.patch)?;
                let mut command = crate::git::git_command(&target);
                command
                    .args(["apply", "--binary", "--whitespace=nowarn"])
                    .arg(patch_file);
                let output = crate::git::git_output(command).await?;
                ensure!(
                    output.status.success(),
                    "The collected patch could not be applied to the original source: {}",
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(2048)])
                );
            }
        }
        verification.status = "pending".into();
        verification.reason = "Candidate prepared for verification.".into();
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = inspection {
        verification.status = "failed".into();
        verification.reason = error.to_string();
        verification.failure_code = Some("verification_preparation_failed".into());
    }
    Ok(serde_json::to_value(Candidate {
        patches,
        verification,
    })?)
}

/// Folds a path the way the default macOS file system compares names: without
/// regard to case or Unicode normalization.
fn fold(path: &str) -> String {
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    nfc.normalize(&nfc.normalize(path).to_lowercase())
        .into_owned()
}

/// Whether a changed path reaches the protected path `guarded` (relative to its
/// root) under another spelling. The collector's file system tells `TESTS/x`
/// from `tests/x`; a Mac applying the patch does not, so the change lands
/// inside the protected path although its fingerprint here is untouched. In
/// `ExistingOnly` mode only a spelling of an entry the original had counts.
fn aliases(
    mode: crate::job_policy::ProtectionMode,
    guarded: &str,
    original: &BTreeMap<PathBuf, String>,
    changed: &[String],
) -> bool {
    let folded = fold(guarded);
    changed.iter().any(|path| {
        let exact = path == guarded || path.starts_with(&format!("{guarded}/"));
        let candidate = fold(path);
        let inner = if candidate == folded {
            Some(String::new())
        } else {
            candidate
                .strip_prefix(&format!("{folded}/"))
                .map(str::to_owned)
        };
        match inner {
            _ if exact => false,
            None => false,
            Some(inner) => {
                mode == crate::job_policy::ProtectionMode::Strict
                    || original
                        .keys()
                        .any(|entry| fold(&entry.to_string_lossy()) == inner)
            }
        }
    })
}

/// Changed files that alter how Git applies or checks out the patch on the
/// machine that receives it. Reported for review, not failed.
fn host_sensitive(changed: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|path| fold(path.rsplit('/').next().unwrap_or(path)) == ".gitattributes")
        .cloned()
        .collect()
}

// Parents must be real directories. The final entry may be a symlink, which is
// compared as data rather than followed. Missing candidate entries are deletions.
fn contained_entry(base: &Path, relative: &Path) -> Result<PathBuf> {
    let mut result = base.to_owned();
    let parts: Vec<_> = relative.components().collect();
    for (index, part) in parts.iter().enumerate() {
        ensure!(
            matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            ),
            "Path escapes the workspace"
        );
        result.push(part);
        if index + 1 < parts.len() {
            match fs::symlink_metadata(&result) {
                Ok(meta) => ensure!(
                    meta.is_dir(),
                    "A workspace path traverses a symlink or non-directory"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(result)
}

fn fingerprint(path: &Path) -> Result<BTreeMap<PathBuf, String>> {
    fn visit(
        path: &Path,
        relative: &Path,
        out: &mut BTreeMap<PathBuf, String>,
        budget: &mut (u64, u64),
    ) -> Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        consume(&meta, budget)?;
        let value = if meta.is_dir() {
            "directory".into()
        } else if meta.is_symlink() {
            format!("link:{:?}", fs::read_link(path)?)
        } else {
            format!(
                "file:{}:{}",
                executable(&meta),
                crate::crypto::sha256_hex_file(path)?
            )
        };
        out.insert(relative.to_owned(), value);
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_name().eq_ignore_ascii_case(".git") {
                    continue;
                }
                visit(
                    &entry.path(),
                    &relative.join(entry.file_name()),
                    out,
                    budget,
                )?;
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(path, Path::new(""), &mut result, &mut (0, 0))?;
    Ok(result)
}
fn consume(meta: &fs::Metadata, budget: &mut (u64, u64)) -> Result<()> {
    ensure!(
        meta.is_dir() || meta.is_file() || meta.is_symlink(),
        "Workspace contains a special file"
    );
    budget.0 += 1;
    budget.1 += meta.len();
    ensure!(
        budget.0 <= 50000 && budget.1 <= 256 * 1024 * 1024,
        "Verification files exceed 50,000 entries or 256 MiB"
    );
    Ok(())
}
fn executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}
fn copy_tree(source: &Path, target: &Path, budget: &mut (u64, u64)) -> Result<()> {
    let meta = fs::symlink_metadata(source)?;
    consume(&meta, budget)?;
    if meta.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if !entry.file_name().eq_ignore_ascii_case(".git") {
                copy_tree(&entry.path(), &target.join(entry.file_name()), budget)?;
            }
        }
    } else if meta.is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        #[cfg(not(unix))]
        anyhow::bail!("Verification workers require Linux");
    } else {
        fs::copy(source, target)?;
    }
    Ok(())
}

/// Replace this helper with the shell; the daemon removes its entire container
/// after the command ends. The check cannot reach the original or agent volume.
pub fn verification_command(cwd: &Path, command: &str) -> Result<()> {
    let cwd = Path::new("/work")
        .join(cwd)
        .canonicalize()
        .context("Verification directory does not exist")?;
    ensure!(
        cwd.starts_with("/work"),
        "Verification directory escapes the working files"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(std::process::Command::new("/bin/sh")
            .args(["-c", "--", command])
            .current_dir(cwd)
            .env("HOME", "/home/worker")
            .env("PATH", crate::verifier_toolchain::WORKER_PATH)
            .exec()
            .into())
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        anyhow::bail!("Verification workers require Linux")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protected_directory_detects_changes_additions_deletions_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tests");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("a"), "original").unwrap();
        let original = fingerprint(&path).unwrap();
        fs::write(path.join("a"), "changed").unwrap();
        assert_ne!(original, fingerprint(&path).unwrap());
        fs::write(path.join("a"), "original").unwrap();
        assert_eq!(original, fingerprint(&path).unwrap());
        fs::write(path.join("new"), "new").unwrap();
        assert_ne!(original, fingerprint(&path).unwrap());
        fs::remove_file(path.join("new")).unwrap();
        fs::remove_file(path.join("a")).unwrap();
        assert_ne!(original, fingerprint(&path).unwrap());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", path.join("a")).unwrap();
            assert_ne!(original, fingerprint(&path).unwrap());
            assert!(contained_entry(dir.path(), Path::new("tests/a/child")).is_err());
        }
    }

    fn changed(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn a_protected_path_spelled_another_way_counts_as_changed() {
        use crate::job_policy::ProtectionMode::{ExistingOnly, Strict};
        let original: BTreeMap<PathBuf, String> = [
            (PathBuf::new(), "directory".to_string()),
            (PathBuf::from("test_api.py"), "file".to_string()),
        ]
        .into();
        // The collector sees a new directory; a Mac sees tests/ modified.
        for alias in ["TESTS/test_api.py", "Tests/new.py", "TESTS"] {
            assert!(
                aliases(Strict, "tests", &original, &changed(&[alias])),
                "{alias}"
            );
        }
        // Precomposed and decomposed é are one name on a Mac.
        assert!(aliases(
            Strict,
            "t\u{e9}sts",
            &original,
            &changed(&["te\u{301}sts/x.py"])
        ));
        // The exact spelling is the fingerprint's job, and unrelated paths pass.
        for fine in [
            "tests/test_api.py",
            "src/tests.py",
            "testsuite/x.py",
            "docs/TESTS",
        ] {
            assert!(
                !aliases(Strict, "tests", &original, &changed(&[fine])),
                "{fine}"
            );
        }
        // Existing-only protection allows additions under any spelling, not
        // edits to what was there.
        assert!(!aliases(
            ExistingOnly,
            "tests",
            &original,
            &changed(&["TESTS/new.py"])
        ));
        assert!(aliases(
            ExistingOnly,
            "tests",
            &original,
            &changed(&["TESTS/Test_API.py"])
        ));
    }

    #[test]
    fn gitattributes_changes_are_flagged_for_review() {
        assert_eq!(
            host_sensitive(&changed(&[
                ".gitattributes",
                "src/main.rs",
                "vendor/.GitAttributes",
                "docs/gitattributes.md"
            ])),
            [".gitattributes", "vendor/.GitAttributes"]
        );
    }
}
