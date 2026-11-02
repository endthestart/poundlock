mod checks;
pub(crate) mod configuration;
mod contract;
mod jobs;
mod managed_keys;
mod node_signatures;
mod nodes;
pub mod retirement;
mod runner;
mod signals;
mod tokens;

pub(crate) use nodes::LIVE_ENROLLMENT_CONFLICT;
pub(crate) use signals::NodeSignal;
pub(crate) use tokens::OPERATOR_CHECK_TOKEN;

use crate::crypto::{Binding, SecretBox};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    /// A read-only connection for the per-request token check, so it never
    /// waits behind a large write on `connection` (L4). WAL lets it read while
    /// a write is in progress; without WAL it is absent.
    reader: Option<Arc<Mutex<Connection>>>,
    pub(crate) secrets: SecretBox,
    signals: Arc<signals::NodeSignals>,
    /// The database is in WAL mode, the only mode in which `synchronous=NORMAL`
    /// cannot corrupt it. Node-action writes relax their flush only when true.
    relaxed_sync_allowed: bool,
}

impl Store {
    pub fn open(path: &Path, secrets: SecretBox) -> Result<Self> {
        let connection = Connection::open(path).context("open SQLite database")?;
        // Cleared values, such as a finished job's sealed source and Git token,
        // are overwritten rather than left in free pages (review of M2).
        connection
            .pragma_update(None, "secure_delete", true)
            .context("enable secure delete")?;
        connection
            .execute_batch(include_str!("../../migrations/schema.sql"))
            .context("apply database schema")?;
        // schema.sql asks for WAL, but a filesystem may refuse it.
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .context("read journal mode")?;
        let reader = if journal_mode.eq_ignore_ascii_case("wal") {
            Some(Arc::new(Mutex::new(
                Connection::open_with_flags(
                    path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .context("open SQLite reader")?,
            )))
        } else {
            None
        };
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            reader,
            secrets,
            signals: Default::default(),
            relaxed_sync_allowed: journal_mode.eq_ignore_ascii_case("wal"),
        };
        store.migrate()?;
        store.seed_defaults()?;
        Ok(store)
    }

    pub(crate) fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock is poisoned"))
    }

    /// The read-only connection, or the main one where there is none.
    pub(crate) fn reader(&self) -> Result<MutexGuard<'_, Connection>> {
        match &self.reader {
            Some(reader) => reader
                .lock()
                .map_err(|_| anyhow::anyhow!("database reader lock is poisoned")),
            None => self.connection(),
        }
    }

    /// Wakeups for a node's action queue. Changes announce themselves here
    /// after they commit.
    pub(crate) fn node_signal(&self, node_id: &str) -> Arc<NodeSignal> {
        self.signals.node(node_id)
    }

    pub fn check_ready(&self) -> Result<()> {
        self.connection()?
            .query_row("SELECT 1", [], |_| Ok(()))
            .context("check SQLite readiness")
    }

    /// Adds the columns and tables `schema.sql` does not create. A new database
    /// gets them here, and every database a 0.2.x controller opened already has
    /// them. The upgrades from 0.1.x schemas are gone: start 0.2.x on such a
    /// database first.
    fn migrate(&self) -> Result<()> {
        let connection = self.connection()?;
        for (table, column, decl) in [
            ("endpoints", "removed_at", "TEXT"),
            ("model_profiles", "removed_at", "TEXT"),
            ("projects", "removed_at", "TEXT"),
            (
                "model_profiles",
                "thinking",
                "TEXT NOT NULL DEFAULT 'model_default'",
            ),
            (
                "model_profiles",
                "capabilities_json",
                "TEXT NOT NULL DEFAULT '[]'",
            ),
            (
                "model_profiles",
                "catalog_json",
                "TEXT NOT NULL DEFAULT '{}'",
            ),
            (
                "model_profiles",
                "retirement_json",
                "TEXT NOT NULL DEFAULT '{}'",
            ),
            ("model_profiles", "thinking_budget_tokens", "INTEGER"),
            ("endpoints", "public_url", "TEXT"),
            ("jobs", "contract_ciphertext", "TEXT"),
            ("jobs", "source_ciphertext", "TEXT"),
            ("nodes", "credential_issued_at", "TEXT"),
        ] {
            add_column_if_missing(&connection, table, column, decl)?;
        }
        add_column_if_missing(&connection, "jobs", "inference_token_hash", "TEXT")?;
        add_column_if_missing(
            &connection,
            "jobs",
            "policy_json",
            "TEXT NOT NULL DEFAULT '{}'",
        )?;
        // Counts a token's unfinished jobs at submission without reading rows.
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS jobs_token_status ON jobs(api_token_id, status)",
        )?;
        // Job lists sort by created_at: without these, every list read every row's
        // result to sort it, about 2 s on the Hub (30 Oct).
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS jobs_created ON jobs(created_at);
             CREATE INDEX IF NOT EXISTS jobs_token_created ON jobs(api_token_id, created_at);",
        )?;
        // The wall budget a runner reported when it claimed the job, and the
        // deadline derived from that claim.
        add_column_if_missing(&connection, "jobs", "runner_wall_seconds", "INTEGER")?;
        add_column_if_missing(&connection, "jobs", "wall_deadline_at", "TEXT")?;
        add_column_if_missing(&connection, "enrollment_tokens", "node_name", "TEXT")?;
        connection.execute(
            "UPDATE harness_profiles SET version = ?1 WHERE id = 'pi' AND version != ?1",
            [crate::verifier_toolchain::PI_VERSION],
        )?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS node_update_signatures (
               os TEXT NOT NULL,
               arch TEXT NOT NULL,
               sha256 TEXT NOT NULL,
               manifest TEXT NOT NULL,
               signature TEXT NOT NULL,
               uploaded_at TEXT NOT NULL,
               PRIMARY KEY (os, arch, sha256)
             );",
        )?;
        Ok(())
    }

    fn seed_defaults(&self) -> Result<()> {
        let now = now();
        self.connection()?.execute(
            "INSERT OR IGNORE INTO harness_profiles
             (id, name, kind, version, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            ("pi", "Pi", "pi", crate::verifier_toolchain::PI_VERSION, now),
        )?;
        Ok(())
    }
}

pub(crate) fn endpoint_key(id: &str) -> Binding<'_> {
    Binding::new("endpoints", "api_key_ciphertext", id)
}

pub(crate) fn job_contract(id: &str) -> Binding<'_> {
    Binding::new("jobs", "contract_ciphertext", id)
}

pub(crate) fn job_source(id: &str) -> Binding<'_> {
    Binding::new("jobs", "source_ciphertext", id)
}

/// `row` is `{api_token_id}/{endpoint_id}`.
pub(crate) fn managed_key(row: &str) -> Binding<'_> {
    Binding::new("managed_litellm_keys", "virtual_key_ciphertext", row)
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn json_or_none(value: Option<String>) -> Option<serde_json::Value> {
    value.and_then(|raw| serde_json::from_str(&raw).ok())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column))
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<()> {
    if !column_exists(connection, table, column)? {
        connection.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigurationError(pub &'static str);

/// A job whose outcome is already recorded, reported to whoever is trying to
/// record a different one.
///
/// This used to surface as a 500. The runner's outbox cannot tell "the
/// controller is broken, keep the report and retry" from "this was already
/// decided, stop" from a 500, so a settled job meant an outbox entry retried
/// forever, logging an internal error on every poll. A 409 says which it is.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct JobAlreadyEndedError(pub String);

/// An application token already has as many unfinished jobs as it may hold.
/// Each one keeps its sealed source until it ends, so the cap bounds what one
/// token can store. Reported as a 429 with a stable code.
#[derive(Debug, thiserror::Error)]
#[error(
    "This application token has reached its limit of unfinished jobs ({0}). Wait for one to finish or cancel one, then submit again."
)]
pub struct TooManyActiveJobsError(pub u32);
