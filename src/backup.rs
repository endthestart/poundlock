//! Database backup. A consistent copy of the live SQLite
//! database is taken with `VACUUM INTO` while the controller runs, checked,
//! compressed and kept: the 14 newest copies, and the first copy of each of
//! the last six months. `last-backup.json` says how the latest run went, for
//! monitoring. The master key is never copied: the copies usually go offsite,
//! and the operator keeps the key once, somewhere safe.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use flate2::{Compression, write::GzEncoder};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Daily copies kept.
pub const DAILY_KEEP: usize = 14;
/// Months whose first copy is kept beyond the daily ones.
pub const MONTHLY_KEEP: usize = 6;
/// Tables whose row counts the report gives, so a copy that lost data shows.
const COUNTED: [&str; 4] = ["api_tokens", "model_profiles", "nodes", "jobs"];
const PREFIX: &str = "poundlock-";
const SUFFIX: &str = ".sqlite3.gz";
pub const STATUS_FILE: &str = "last-backup.json";

/// Where and when the controller backs up its database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupConfig {
    pub dir: PathBuf,
    /// Time of day, UTC.
    pub at: NaiveTime,
}

#[derive(Debug, Serialize)]
pub struct BackupReport {
    pub ok: bool,
    pub at: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub integrity: String,
    pub counts: BTreeMap<String, i64>,
    pub kept: Vec<String>,
    pub removed: Vec<String>,
}

/// Take, check, compress and keep one copy of `database` in `dir`, then
/// prune old copies and write `last-backup.json`.
pub fn back_up(database: &Path, dir: &Path, now: DateTime<Utc>) -> Result<BackupReport> {
    private_dir(dir)?;
    let date = now.date_naive();
    let name = format!("{PREFIX}{date}.sqlite3");
    // The uncompressed copy holds everything the database does: it is made in
    // the database's own folder, never in the backup folder, which is often a
    // share others can read or write.
    let copy = database
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(".backup-{date}.sqlite3.tmp"));
    let result = (|| -> Result<BackupReport> {
        // Owner-only from the start: VACUUM INTO writes into an empty file it
        // finds, keeping its mode, where it would create one readable by all.
        drop(private_file(&copy)?);
        let tables = {
            // Never create: a missing database is a failed backup, not an empty one.
            let live = Connection::open_with_flags(
                database,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("open {}", database.display()))?;
            live.busy_timeout(Duration::from_secs(60))?;
            // The controller's root filesystem is read-only: no temporary files.
            live.pragma_update(None, "temp_store", "MEMORY")?;
            // A consistent snapshot of a WAL database, taken without stopping writers.
            live.execute(
                "VACUUM INTO ?1",
                [copy.to_str().context("backup path is not UTF-8")?],
            )
            .context("VACUUM INTO")?;
            table_names(&live)?
        };
        let (integrity, counts) = check_copy(&copy, &tables)?;
        let file = format!("{name}.gz");
        let target = dir.join(&file);
        let (bytes, sha256) = compress(&copy, &target)?;
        let mut sums = private_file(&dir.join(format!("{file}.sha256")))?;
        sums.write_all(format!("{sha256}  {file}\n").as_bytes())?;
        sums.sync_all()?;
        let (kept, removed) = prune(dir, date)?;
        Ok(BackupReport {
            ok: true,
            at: now.to_rfc3339(),
            file,
            bytes,
            sha256,
            integrity,
            counts,
            kept,
            removed,
        })
    })();
    let _ = std::fs::remove_file(&copy);
    match &result {
        Ok(report) => write_status(dir, &serde_json::to_value(report)?)?,
        Err(error) => write_status(
            dir,
            &serde_json::json!({"ok": false, "at": now.to_rfc3339(), "error": format!("{error:#}")}),
        )?,
    }
    result
}

fn table_names(connection: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(names)
}

/// The copy must pass SQLite's integrity check and hold every table the live
/// database has; the counts are reported.
fn check_copy(copy: &Path, tables: &BTreeSet<String>) -> Result<(String, BTreeMap<String, i64>)> {
    let connection = Connection::open(copy).context("open the copy")?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("the copy failed its integrity check: {integrity}");
    }
    let copied = table_names(&connection)?;
    let missing: Vec<_> = tables.difference(&copied).cloned().collect();
    if !missing.is_empty() {
        bail!("the copy lacks tables: {}", missing.join(", "));
    }
    let mut counts = BTreeMap::new();
    for table in COUNTED.iter().filter(|table| copied.contains(**table)) {
        let count: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        counts.insert((*table).to_owned(), count);
    }
    Ok((integrity, counts))
}

/// Gzip `from` into `to` (owner-only), returning its size and the SHA-256 of
/// exactly the bytes written, never a re-read that a swapped file could change.
fn compress(from: &Path, to: &Path) -> Result<(u64, String)> {
    let partial = to.with_extension("gz.partial");
    let (bytes, sha256) = {
        let mut input = std::fs::File::open(from)?;
        let output = Hashed {
            file: private_file(&partial)?,
            hash: Sha256::new(),
            bytes: 0,
        };
        let mut encoder = GzEncoder::new(output, Compression::default());
        std::io::copy(&mut input, &mut encoder)?;
        let output = encoder.finish()?;
        output.file.sync_all()?;
        (output.bytes, hex(&output.hash.finalize()))
    };
    std::fs::rename(&partial, to)?;
    Ok((bytes, sha256))
}

/// A file writer that hashes and counts what it writes.
struct Hashed {
    file: std::fs::File,
    hash: Sha256,
    bytes: u64,
}

impl Write for Hashed {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buf)?;
        self.hash.update(&buf[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// A new owner-only file at `path`. Whatever was there is removed first, and
/// the open neither follows a symlink nor reuses a file someone planted, so a
/// writable backup folder cannot point a write at the master key or database.
fn private_file(path: &Path) -> Result<std::fs::File> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).with_context(|| format!("remove {}", path.display()));
        }
        _ => {}
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

/// The backup folder, created owner-only if missing. One that is a symlink,
/// belongs to someone else or that others may write is refused: its copies
/// and status could be swapped or redirected.
fn private_dir(dir: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        bail!("{} is not a folder (a symlink is refused)", dir.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            bail!(
                "{} belongs to UID {}, not this process's {me}",
                dir.display(),
                meta.uid()
            );
        }
        if meta.mode() & 0o022 != 0 {
            bail!(
                "{} may be written by others (mode {:o}); make it 0700",
                dir.display(),
                meta.mode() & 0o777
            );
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The copy's date, from a file name this module writes.
fn copy_date(name: &str) -> Option<NaiveDate> {
    let date = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

/// Which copies to keep on `today`: the newest `DAILY_KEEP`, and the earliest
/// copy of each of the last `MONTHLY_KEEP` months (this one included).
pub fn keep(dates: &[NaiveDate], today: NaiveDate) -> BTreeSet<NaiveDate> {
    let mut sorted: Vec<_> = dates
        .iter()
        .copied()
        .filter(|date| *date <= today)
        .collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut kept: BTreeSet<_> = sorted.iter().rev().take(DAILY_KEEP).copied().collect();
    let month = |date: NaiveDate| date.year() * 12 + date.month0() as i32;
    let oldest = month(today) - (MONTHLY_KEEP as i32 - 1);
    let mut firsts = BTreeMap::new();
    for date in &sorted {
        if month(*date) >= oldest {
            firsts.entry(month(*date)).or_insert(*date);
        }
    }
    kept.extend(firsts.into_values());
    kept
}

fn prune(dir: &Path, today: NaiveDate) -> Result<(Vec<String>, Vec<String>)> {
    let mut copies = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(date) = copy_date(&name) {
            copies.insert(date, name);
        }
    }
    let dates: Vec<_> = copies.keys().copied().collect();
    let kept = keep(&dates, today);
    let mut removed = Vec::new();
    for (date, name) in &copies {
        if !kept.contains(date) && *date <= today {
            std::fs::remove_file(dir.join(name))?;
            let _ = std::fs::remove_file(dir.join(format!("{name}.sha256")));
            removed.push(name.clone());
        }
    }
    let kept = copies
        .into_iter()
        .filter(|(date, _)| kept.contains(date))
        .map(|(_, name)| name)
        .collect();
    Ok((kept, removed))
}

fn write_status(dir: &Path, status: &serde_json::Value) -> Result<()> {
    let partial = dir.join(format!("{STATUS_FILE}.partial"));
    let mut file = private_file(&partial)?;
    file.write_all(serde_json::to_string_pretty(status)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&partial, dir.join(STATUS_FILE))?;
    Ok(())
}

/// The next time at or after `now` that the backup runs.
pub fn next_run(now: DateTime<Utc>, at: NaiveTime) -> DateTime<Utc> {
    let today = now.date_naive().and_time(at).and_utc();
    if today > now {
        today
    } else {
        today + chrono::Duration::days(1)
    }
}

/// The last run's status, as `last-backup.json` holds it: only a regular
/// file of at most 64 KiB is read (a planted FIFO would block the reader).
/// Blocking: call it off the async runtime.
pub fn last_status(dir: &Path) -> Option<serde_json::Value> {
    let path = dir.join(STATUS_FILE);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// A start with no good copy in the last day backs up soon, rather than at the
/// next scheduled time: a first deploy is checked at once, and a controller
/// that was down overnight does not skip a night.
pub fn missed(status: Option<&serde_json::Value>, now: DateTime<Utc>) -> bool {
    let Some(status) = status else { return true };
    let at = status["at"]
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok());
    status["ok"] != true
        || at.is_none_or(|at| now - at.with_timezone(&Utc) > chrono::Duration::hours(24))
}

/// Back up every day at `config.at` (UTC) for as long as the controller runs.
pub async fn schedule(config: BackupConfig, database: PathBuf) {
    let dir = config.dir.clone();
    let status = tokio::task::spawn_blocking(move || last_status(&dir))
        .await
        .ok()
        .flatten();
    if missed(status.as_ref(), Utc::now()) {
        tokio::time::sleep(Duration::from_secs(60)).await;
        run_once(&config, &database).await;
    }
    loop {
        let now = Utc::now();
        let wait = (next_run(now, config.at) - now)
            .to_std()
            .unwrap_or(Duration::from_secs(60));
        tokio::time::sleep(wait).await;
        run_once(&config, &database).await;
    }
}

async fn run_once(config: &BackupConfig, database: &Path) {
    let (database, dir) = (database.to_path_buf(), config.dir.clone());
    match tokio::task::spawn_blocking(move || back_up(&database, &dir, Utc::now())).await {
        Ok(Ok(report)) => tracing::info!(
            file = %report.file,
            bytes = report.bytes,
            removed = report.removed.len(),
            "database backed up"
        ),
        Ok(Err(error)) => tracing::error!(error = %format!("{error:#}"), "database backup failed"),
        Err(error) => tracing::error!(%error, "database backup task failed"),
    }
}

#[cfg(test)]
#[path = "backup_tests.rs"]
mod tests;
