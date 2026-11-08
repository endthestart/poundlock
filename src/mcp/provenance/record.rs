//! Recording a patch as it is applied: the folder's files just before and just after.

use super::*;

/// What a patch is about to change: the folder's state before it.
pub struct Pending {
    records: PathBuf,
    directory: String,
    before: String,
}

/// Records the folder before a patch. `None` outside Git, where nothing is
/// recorded.
pub fn begin(directory: &Path) -> Result<Option<Pending>> {
    let Some((records, relative)) = location(directory)? else {
        return Ok(None);
    };
    let before = store(&records, &manifest(directory)?)?;
    Ok(Some(Pending {
        records,
        directory: relative,
        before,
    }))
}

/// The job a patch came from, as `finish` records it.
pub struct JobPatch<'a> {
    pub job_id: &'a str,
    pub attempt: Option<usize>,
    pub index: usize,
    pub patch: &'a str,
    pub files: &'a [String],
    /// The job's check result: `passed`, `failed`, `unverified`.
    pub verification: Option<&'a str>,
}

/// Records the patch once it is applied.
pub fn finish(pending: Pending, directory: &Path, job: JobPatch<'_>) -> Result<Applied> {
    let after = store(&pending.records, &manifest(directory)?)?;
    let entry = Applied {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        job_id: job.job_id.to_owned(),
        attempt: job.attempt,
        index: job.index,
        directory: pending.directory,
        patch_sha256: crate::crypto::to_hex(&Sha256::digest(job.patch.as_bytes())),
        files: job.files.to_vec(),
        before: pending.before,
        after,
        verification: job.verification.map(str::to_owned),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(pending.records.join("applied.jsonl"))
        .context("open the provenance record")?;
    writeln!(file, "{}", serde_json::to_string(&entry)?)?;
    Ok(entry)
}
