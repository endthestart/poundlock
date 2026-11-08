//! The folders an agent may upload from and apply patches to.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

/// The folders an agent may upload from and apply patches to, as the person
/// who configured the server chose them.
#[derive(Debug, Clone)]
pub struct Workspace {
    roots: Vec<PathBuf>,
}

impl Workspace {
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        ensure!(
            !roots.is_empty(),
            "name at least one folder the agent may use with --root"
        );
        let roots = roots
            .iter()
            .map(|root| {
                std::fs::canonicalize(root)
                    .with_context(|| format!("--root {} is not a folder", root.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { roots })
    }

    /// `directory`, resolved through any links, if it is an approved root or
    /// inside one.
    pub fn resolve(&self, directory: &str) -> Result<PathBuf> {
        let requested = Path::new(directory);
        ensure!(
            requested.is_absolute(),
            "directory must be an absolute path"
        );
        let resolved = std::fs::canonicalize(requested)
            .with_context(|| format!("{directory} does not exist"))?;
        ensure!(resolved.is_dir(), "{directory} is not a folder");
        ensure!(
            self.roots.iter().any(|root| resolved.starts_with(root)),
            "{directory} is outside the folders this Poundlock server may use: {}",
            self.describe()
        );
        Ok(resolved)
    }

    /// A file path inside an approved root, which may not exist yet.
    pub(super) fn resolve_new_file(&self, path: &str) -> Result<PathBuf> {
        let requested = Path::new(path);
        ensure!(requested.is_absolute(), "save_to must be an absolute path");
        let parent = requested
            .parent()
            .context("save_to must name a file inside a folder")?;
        let name = requested.file_name().context("save_to must name a file")?;
        let folder = self.resolve(&parent.to_string_lossy())?;
        let target = folder.join(name);
        ensure!(
            !target
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink()),
            "save_to must not be a symbolic link"
        );
        Ok(target)
    }

    pub fn describe(&self) -> String {
        self.roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
