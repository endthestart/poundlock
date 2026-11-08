//! A job's patch for review, the files it names, and the record of applied patches.

use super::*;

impl Tools {
    pub(super) async fn download_patch(
        &self,
        id: &str,
        index: usize,
        attempt: Option<usize>,
    ) -> Result<String> {
        let mut path = format!("{}/patches/{index}", job_path(id)?);
        if let Some(attempt) = attempt {
            path.push_str(&format!("?attempt={attempt}"));
        }
        self.controller.get_text(&path).await
    }

    pub(super) async fn patch(&self, args: PatchArgs) -> Result<Outcome> {
        let patch = self
            .download_patch(&args.job_id, args.index, args.attempt)
            .await?;
        let files = patch_paths(&patch).unwrap_or_default();
        if let Some(save_to) = args.save_to {
            let target = self.workspace.resolve_new_file(&save_to)?;
            tokio::fs::write(&target, &patch)
                .await
                .with_context(|| format!("write {}", target.display()))?;
            return Ok(Outcome::ok(
                format!(
                    "Saved the patch ({} bytes, {} file(s)) to {}.",
                    patch.len(),
                    files.len(),
                    target.display()
                ),
                json!({"job_id": args.job_id, "index": args.index, "saved_to": target.display().to_string(), "bytes": patch.len(), "files": files}),
            ));
        }
        if patch.len() > MAX_INLINE_PATCH_BYTES {
            bail!(
                "The patch is {} bytes, over the 200 KiB shown inline; pass save_to to write it to a file.",
                patch.len()
            );
        }
        Ok(Outcome::ok(
            if patch.is_empty() {
                "The patch is empty.".to_owned()
            } else {
                patch.clone()
            },
            json!({"job_id": args.job_id, "index": args.index, "bytes": patch.len(), "files": files}),
        ))
    }

    pub(super) fn provenance(&self, directory: &str) -> Result<Outcome> {
        let directory = self.workspace.resolve(directory)?;
        let report = provenance::report(&directory)?;
        Ok(Outcome::ok(
            report.text(),
            json!({
                "directory": report.directory,
                "clean": report.clean(),
                "checks_passed": report.without_evidence().is_empty(),
                "applied": report.applied.iter().map(|entry| json!({
                    "job_id": entry.job_id, "at": entry.at, "files": entry.files,
                    "verification": entry.verification,
                })).collect::<Vec<_>>(),
                "outside": report.outside,
                "unreadable": report.unreadable,
            }),
        ))
    }
}

/// The paths a Git patch touches, in order, each once: every name on a
/// `diff --git`, `---`, `+++`, rename or copy line. A header this cannot read,
/// such as a quoted path, is an error, so no path escapes the checks applied
/// to them.
pub fn patch_paths(patch: &str) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    let mut add = |path: &str| -> Result<()> {
        ensure!(
            !path.starts_with('"'),
            "the patch names a quoted path ({path}); review and apply it yourself"
        );
        if !path.is_empty() && path != "/dev/null" && !paths.iter().any(|known| known == path) {
            paths.push(path.to_owned());
        }
        Ok(())
    };
    // Only a file's header names paths; a hunk line such as "--- note" is
    // content that happens to start like one.
    let mut in_header = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            in_header = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_header = true;
            ensure!(
                !rest.starts_with('"'),
                "the patch names a quoted path; review and apply it yourself"
            );
            // `a/<path> b/<path>`; a path with spaces is split at " b/".
            let (old, new) = rest
                .strip_prefix("a/")
                .and_then(|rest| rest.split_once(" b/"))
                .with_context(|| format!("the patch has a header this cannot read: {line}"))?;
            add(old)?;
            add(new)?;
        } else if !in_header {
            continue;
        } else if let Some(path) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            let path = path.split('\t').next().unwrap_or(path);
            let path = path
                .strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(path);
            add(path)?;
        } else if let Some(path) = ["rename from ", "rename to ", "copy from ", "copy to "]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
        {
            add(path)?;
        }
    }
    Ok(paths)
}
