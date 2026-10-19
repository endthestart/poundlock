use crate::crypto::{SecretBox, make_group_readable, random_token, write_private};
use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct AppConfig {
    pub wake_socket: Option<PathBuf>,
    pub allow_job_network: bool,
    pub git_hosts: Vec<String>,
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub projects_dir: PathBuf,
    pub work_dir: PathBuf,
    pub admin_token: String,
    pub runner_token: String,
    pub secrets: SecretBox,
    pub external_timeout_seconds: u64,
    /// Longest a durable job's model request may go without a byte from the
    /// model: before headers, and between chunks once it streams. Longer than
    /// `external_timeout_seconds` because a job may prefill its whole context
    /// on the slowest qualified local model (Qwen3.8 27B prefills at about 66
    /// tokens/second, so a full 64K context is ~1,000 s). Without it, one
    /// request that never answers is ended only by the job's wall budget.
    pub job_inference_timeout_seconds: u64,
    pub node_offline_seconds: i64,
    pub client_artifacts_dir: Option<PathBuf>,
    /// Unfinished jobs one application token may hold. Each keeps its sealed
    /// source, about 90 MiB at the largest, in the database until it ends.
    pub max_active_jobs_per_token: u32,
    /// Requests the controller serves at once, streamed responses included.
    /// Over it a request gets 503 at once instead of a queue (L4).
    pub max_requests_in_flight: u32,
    /// How long a request that does not say (no `Poundlock-Wait`) waits in
    /// line for a busy model before its 409; at most 110 s, the default. Zero
    /// keeps the old answer-at-once. A streamed one waits on its open stream.
    pub capacity_default_wait_seconds: u64,
    /// When operator discovery reads OpenRouter's public model catalog, an
    /// outbound request to openrouter.ai (`POUNDLOCK_OPENROUTER_CATALOG`).
    pub openrouter_catalog: CatalogMode,
    /// The nightly database backup: `POUNDLOCK_BACKUP_DIR` turns it on,
    /// `POUNDLOCK_BACKUP_AT` (HH:MM, UTC, default 08:30) sets its time.
    pub backup: Option<crate::backup::BackupConfig>,
}

/// `on-request` (the default) reads the catalog only when an operator asks for
/// it; `always` reads it on every discovery; `never` refuses even when asked,
/// for installs that must make no third-party request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogMode {
    Never,
    OnRequest,
    Always,
}

impl CatalogMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnRequest => "on-request",
            Self::Always => "always",
        }
    }

    fn from_env() -> Result<Self> {
        match env::var("POUNDLOCK_OPENROUTER_CATALOG").as_deref() {
            Err(env::VarError::NotPresent) | Ok("on-request") => Ok(Self::OnRequest),
            Ok("always") => Ok(Self::Always),
            Ok("never") => Ok(Self::Never),
            _ => anyhow::bail!("POUNDLOCK_OPENROUTER_CATALOG must be on-request, always or never"),
        }
    }
}

impl AppConfig {
    pub fn load_controller() -> Result<Self> {
        let data_dir = path_env("POUNDLOCK_DATA_DIR", "./data");
        let runtime_dir = path_env("POUNDLOCK_RUNTIME_DIR", "./runtime");
        fs::create_dir_all(&data_dir).context("create data directory")?;
        fs::create_dir_all(&runtime_dir).context("create runtime directory")?;
        migrate_legacy_database(&data_dir)?;
        let (admin_token, created) = load_or_create_token(&data_dir.join("admin-token"))?;
        if created {
            eprintln!(
                "First-start admin token written to {}",
                data_dir.join("admin-token").display()
            );
            eprintln!("Read that file to unlock the operator console. It is not printed here.");
        }
        let runner_token_path = runtime_dir.join("runner-token");
        let (runner_token, _) = load_or_create_token(&runner_token_path)?;
        make_group_readable(&runner_token_path)?;
        let bind = env::var("POUNDLOCK_BIND")
            .unwrap_or_else(|_| "127.0.0.1:7331".into())
            .parse()
            .context("POUNDLOCK_BIND is not a socket address")?;
        let secrets = SecretBox::load_or_create(&data_dir.join("master-key"))?;
        Ok(Self {
            wake_socket: env::var_os("POUNDLOCK_WAKE_SOCKET").map(PathBuf::from),
            allow_job_network: env::var("POUNDLOCK_ALLOW_JOB_NETWORK")
                .unwrap_or_else(|_| "false".into())
                .parse()
                .context("POUNDLOCK_ALLOW_JOB_NETWORK must be true or false")?,
            git_hosts: env::var("POUNDLOCK_GIT_HOSTS")
                .unwrap_or_else(|_| "github.com,gitlab.com".into())
                .split(',')
                .map(|host| host.trim().to_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            bind,
            data_dir,
            runtime_dir,
            projects_dir: path_env("POUNDLOCK_PROJECTS_DIR", "/projects"),
            work_dir: path_env("POUNDLOCK_WORK_DIR", "/work"),
            admin_token,
            runner_token,
            secrets,
            external_timeout_seconds: number_env("POUNDLOCK_EXTERNAL_TIMEOUT_SECONDS", 600)?,
            job_inference_timeout_seconds: number_env(
                "POUNDLOCK_JOB_INFERENCE_TIMEOUT_SECONDS",
                1200,
            )?,
            node_offline_seconds: number_env("POUNDLOCK_NODE_OFFLINE_SECONDS", 15)?,
            client_artifacts_dir: env::var_os("POUNDLOCK_CLIENT_ARTIFACTS").map(PathBuf::from),
            max_active_jobs_per_token: positive(
                "POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN",
                number_env("POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN", 10)?,
            )?,
            max_requests_in_flight: positive(
                "POUNDLOCK_MAX_REQUESTS_IN_FLIGHT",
                number_env("POUNDLOCK_MAX_REQUESTS_IN_FLIGHT", 512)?,
            )?,
            capacity_default_wait_seconds: number_env(
                "POUNDLOCK_CAPACITY_DEFAULT_WAIT_SECONDS",
                110,
            )?,
            openrouter_catalog: CatalogMode::from_env()?,
            backup: backup_env()?,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("poundlock.sqlite3")
    }
}

pub fn reject_legacy_env() -> Result<()> {
    if let Some((name, _)) =
        env::vars_os().find(|(name, _)| name.to_string_lossy().starts_with("RCHARM_"))
    {
        anyhow::bail!(
            "{} is obsolete; use the equivalent POUNDLOCK_* variable",
            name.to_string_lossy()
        );
    }
    Ok(())
}

fn migrate_legacy_database(data_dir: &Path) -> Result<()> {
    let new = data_dir.join("poundlock.sqlite3");
    let old = data_dir.join("rcharm.sqlite3");
    if new.exists() || !old.exists() {
        return Ok(());
    }
    fs::rename(&old, &new)
        .with_context(|| format!("rename {} to {}", old.display(), new.display()))?;
    for suffix in ["-wal", "-shm"] {
        let from = data_dir.join(format!("rcharm.sqlite3{suffix}"));
        if from.exists() {
            let to = data_dir.join(format!("poundlock.sqlite3{suffix}"));
            fs::rename(&from, &to)
                .with_context(|| format!("rename {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn backup_env() -> Result<Option<crate::backup::BackupConfig>> {
    let Some(dir) = std::env::var_os("POUNDLOCK_BACKUP_DIR").filter(|dir| !dir.is_empty()) else {
        return Ok(None);
    };
    let at = std::env::var("POUNDLOCK_BACKUP_AT").unwrap_or_else(|_| "08:30".into());
    let at = chrono::NaiveTime::parse_from_str(at.trim(), "%H:%M")
        .map_err(|_| anyhow::anyhow!("POUNDLOCK_BACKUP_AT must be HH:MM (UTC), not {at:?}"))?;
    Ok(Some(crate::backup::BackupConfig {
        dir: PathBuf::from(dir),
        at,
    }))
}

fn path_env(name: &str, default: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into())
}

fn number_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => parse_number(name, &value),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("read {name}: {error}")),
    }
}

fn parse_number<T>(name: &str, value: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("{name} must be a valid number: {error}"))
}

fn positive(name: &str, value: u32) -> Result<u32> {
    anyhow::ensure!(value > 0, "{name} must be at least 1");
    Ok(value)
}

pub fn load_token(path: &Path) -> Result<String> {
    let token = fs::read_to_string(path)
        .with_context(|| format!("read token {}", path.display()))?
        .trim()
        .to_owned();
    if token.is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(token)
}

fn load_or_create_token(path: &Path) -> Result<(String, bool)> {
    if path.exists() {
        return Ok((load_token(path)?, false));
    }
    let token = random_token();
    write_private(path, &token)?;
    Ok((token, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_sqlite_and_sidecars_once() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        std::fs::write(data_dir.join("rcharm.sqlite3"), b"db").unwrap();
        std::fs::write(data_dir.join("rcharm.sqlite3-wal"), b"wal").unwrap();
        std::fs::write(data_dir.join("rcharm.sqlite3-shm"), b"shm").unwrap();
        migrate_legacy_database(data_dir).unwrap();
        assert!(data_dir.join("poundlock.sqlite3").exists());
        assert!(data_dir.join("poundlock.sqlite3-wal").exists());
        assert!(data_dir.join("poundlock.sqlite3-shm").exists());
        assert!(!data_dir.join("rcharm.sqlite3").exists());
        migrate_legacy_database(data_dir).unwrap();
        assert_eq!(
            std::fs::read(data_dir.join("poundlock.sqlite3")).unwrap(),
            b"db"
        );
    }

    #[test]
    fn leaves_existing_poundlock_sqlite_alone() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path();
        std::fs::write(data_dir.join("poundlock.sqlite3"), b"new").unwrap();
        std::fs::write(data_dir.join("rcharm.sqlite3"), b"old").unwrap();
        migrate_legacy_database(data_dir).unwrap();
        assert_eq!(
            std::fs::read(data_dir.join("poundlock.sqlite3")).unwrap(),
            b"new"
        );
        assert!(data_dir.join("rcharm.sqlite3").exists());
    }

    #[test]
    fn numeric_configuration_rejects_invalid_values() {
        assert_eq!(parse_number::<u64>("POUNDLOCK_TIMEOUT", "30").unwrap(), 30);
        assert!(parse_number::<u64>("POUNDLOCK_TIMEOUT", "soon").is_err());
        assert!(positive("POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN", 0).is_err());
        assert_eq!(
            positive("POUNDLOCK_MAX_ACTIVE_JOBS_PER_TOKEN", 1).unwrap(),
            1
        );
    }

    #[test]
    fn load_token_rejects_empty_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("admin-token");
        std::fs::write(&path, "   \n").unwrap();
        assert!(load_token(&path).is_err());
    }
}
