use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;

pub const MAX_SOURCE_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_SOURCE_JSON: usize = 72 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobSource {
    Empty,
    Git {
        repository: String,
        revision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credentials: Option<crate::git_source::GitCredentials>,
    },
    Snapshot {
        files: Vec<SourceFile>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub content_base64: String,
    #[serde(default)]
    pub executable: bool,
}

impl JobSource {
    pub fn validate(&self) -> Result<()> {
        if let Self::Git {
            repository,
            revision,
            credentials,
        } = self
        {
            return crate::git_source::validate(repository, revision, credentials.as_ref());
        }
        let Self::Snapshot { files } = self else {
            return Ok(());
        };
        if files.is_empty() || files.len() > 10_000 {
            bail!("A snapshot must contain between 1 and 10,000 files.");
        }
        let mut paths = BTreeSet::new();
        let mut total = 0usize;
        for file in files {
            validate_path(&file.path)?;
            if !paths.insert(file.path.to_lowercase()) {
                bail!("Duplicate snapshot path (ignoring case): {}", file.path);
            }
            if file.content_base64.len() > MAX_SOURCE_BYTES * 4 / 3 + 4 {
                bail!("Snapshot exceeds the 50 MiB source limit.");
            }
            total += STANDARD
                .decode(&file.content_base64)
                .map_err(|_| anyhow::anyhow!("Invalid base64 for snapshot file: {}", file.path))?
                .len();
            if total > MAX_SOURCE_BYTES {
                bail!("Snapshot exceeds the 50 MiB source limit.");
            }
        }
        for path in &paths {
            for (index, _) in path.match_indices('/') {
                if paths.contains(&path[..index]) {
                    bail!("Snapshot path is both a file and a directory: {path}");
                }
            }
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        match self {
            Self::Empty => json!({"kind":"empty"}),
            Self::Git {
                repository,
                revision,
                ..
            } => json!({"kind":"git", "repository":repository,"revision":revision.to_lowercase()}),
            Self::Snapshot { files } => {
                let mut ordered = files.iter().collect::<Vec<_>>();
                ordered.sort_by(|a, b| a.path.cmp(&b.path));
                let mut digest = Sha256::new();
                let mut bytes = 0usize;
                for file in ordered {
                    let content = STANDARD
                        .decode(&file.content_base64)
                        .expect("validated source");
                    digest.update((file.path.len() as u64).to_be_bytes());
                    digest.update(file.path.as_bytes());
                    digest.update([u8::from(file.executable)]);
                    digest.update((content.len() as u64).to_be_bytes());
                    digest.update(&content);
                    bytes += content.len();
                }
                json!({"kind":"snapshot","files":files.len(),"bytes":bytes,"sha256":format!("{:x}",digest.finalize())})
            }
        }
    }

    pub async fn materialize(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        if matches!(self, Self::Git { .. }) {
            bail!("Git sources require pinned checkout preparation");
        }
        tokio::fs::create_dir(directory).await?;
        let result: Result<()> =
            async {
                if let Self::Snapshot { files } = self {
                    for file in files {
                        let destination = directory.join(&file.path);
                        tokio::fs::create_dir_all(destination.parent().unwrap()).await?;
                        let mut output = tokio::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&destination)
                            .await?;
                        use tokio::io::AsyncWriteExt;
                        output
                            .write_all(&STANDARD.decode(&file.content_base64)?)
                            .await?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            output
                                .set_permissions(std::fs::Permissions::from_mode(
                                    if file.executable { 0o755 } else { 0o644 },
                                ))
                                .await?;
                        }
                    }
                }
                Ok(())
            }
            .await;
        if result.is_err() {
            tokio::fs::remove_dir_all(directory).await?;
        }
        result
    }
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 1024
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        || path.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
        })
    {
        bail!("Snapshot paths must be relative files without traversal or Git metadata: {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(paths: &[&str]) -> JobSource {
        JobSource::Snapshot {
            files: paths
                .iter()
                .map(|path| SourceFile {
                    path: (*path).into(),
                    content_base64: STANDARD.encode("original\n"),
                    executable: false,
                })
                .collect(),
        }
    }
    #[test]
    fn rejects_escape_metadata_aliases_and_file_directory_collisions() {
        for paths in [
            vec!["../outside"],
            vec!["/outside"],
            vec![".GIT/config"],
            vec!["a/../../b"],
            vec!["a", "a/b"],
            vec!["A", "a"],
            vec!["a\\b"],
            vec!["C:/file"],
        ] {
            assert!(snapshot(&paths).validate().is_err(), "{paths:?}");
        }
    }
    #[test]
    fn snapshot_identity_is_independent_of_file_order() {
        let a = snapshot(&["src/a", "src/b"]);
        let b = snapshot(&["src/b", "src/a"]);
        a.validate().unwrap();
        b.validate().unwrap();
        assert_eq!(a.summary(), b.summary());
    }
    #[tokio::test]
    async fn materialization_never_reuses_an_existing_directory() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(
            snapshot(&["file"])
                .materialize(temporary.path())
                .await
                .is_err()
        );
        assert!(!temporary.path().join("file").exists());
    }
}
