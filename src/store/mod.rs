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
        store.bind_secrets(path)?;
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

    fn migrate(&self) -> Result<()> {
        let connection = self.connection()?;
        for (table, column, decl) in [
            ("endpoints", "removed_at", "TEXT"),
            ("model_profiles", "removed_at", "TEXT"),
            ("projects", "removed_at", "TEXT"),
            ("endpoints", "last_checked_at", "TEXT"),
            ("endpoints", "last_check_ok", "INTEGER"),
            ("endpoints", "last_check_message", "TEXT"),
            (
                "endpoints",
                "managed_identity_json",
                "TEXT NOT NULL DEFAULT '{}'",
            ),
            ("endpoints", "revision_nonce", "TEXT NOT NULL DEFAULT ''"),
            ("model_profiles", "last_checked_at", "TEXT"),
            ("model_profiles", "last_check_ok", "INTEGER"),
            ("model_profiles", "last_check_message", "TEXT"),
            ("model_profiles", "aliases_json", "TEXT"),
            ("model_profiles", "idle_seconds", "INTEGER"),
            ("model_profiles", "model_label", "TEXT"),
            (
                "model_profiles",
                "revision_nonce",
                "TEXT NOT NULL DEFAULT ''",
            ),
            (
                "model_profiles",
                "data_destination",
                "TEXT NOT NULL DEFAULT 'unknown'",
            ),
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
            ("projects", "instruction_files_json", "TEXT"),
            ("endpoints", "public_url", "TEXT"),
            ("jobs", "contract_ciphertext", "TEXT"),
            ("jobs", "source_ciphertext", "TEXT"),
            ("nodes", "credential_issued_at", "TEXT"),
        ] {
            add_column_if_missing(&connection, table, column, decl)?;
        }
        if !column_exists(&connection, "endpoints", "data_destination")? {
            // One transaction: the declaration moves to the server and its
            // models start inheriting it together, or not at all.
            let transaction = connection.unchecked_transaction()?;
            transaction
                .execute_batch(include_str!("../../migrations/endpoint-destination.sql"))
                .context("apply endpoint destination migration")?;
            transaction.commit()?;
        }
        if !column_exists(&connection, "model_profiles", "uses_json")? {
            connection
                .execute_batch(include_str!("../../migrations/model-profile-uses.sql"))
                .context("apply model profile uses migration")?;
        }
        let project_required: bool = connection.query_row(
            "SELECT \"notnull\" FROM pragma_table_info('jobs') WHERE name='project_id'",
            [],
            |row| row.get(0),
        )?;
        if project_required {
            connection.pragma_update(None, "foreign_keys", false)?;
            let migrated = (|| -> Result<()> {
                let transaction = connection.unchecked_transaction()?;
                transaction.execute_batch(include_str!("../../migrations/job-source.sql"))?;
                let violations: i64 = transaction.query_row(
                    "SELECT count(*) FROM pragma_foreign_key_check",
                    [],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(
                    violations == 0,
                    "job source migration violates foreign keys"
                );
                transaction.commit()?;
                Ok(())
            })();
            connection.pragma_update(None, "foreign_keys", true)?;
            migrated?;
        }
        add_column_if_missing(&connection, "jobs", "inference_token_hash", "TEXT")?;
        add_column_if_missing(
            &connection,
            "jobs",
            "policy_json",
            "TEXT NOT NULL DEFAULT '{}'",
        )?;
        // Add columns introduced after the historical jobs-table rebuild only
        // once that rebuild is complete, or the copied schema will drop them.
        add_column_if_missing(&connection, "jobs", "api_token_id", "TEXT")?;
        // Counts a token's unfinished jobs at submission without reading rows.
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS jobs_token_status ON jobs(api_token_id, status)",
        )?;
        // A job's source, including any Git read token, is needed only until
        // the job ends, and every ending now removes it. This removes it from
        // jobs that ended before that; afterwards it changes nothing.
        connection.execute(
            concat!(
                "UPDATE jobs SET source_ciphertext=NULL
             WHERE status IN ",
                crate::terminal_job_statuses!(),
                " AND source_ciphertext IS NOT NULL"
            ),
            [],
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
             );
             CREATE TABLE IF NOT EXISTS managed_litellm_keys (
               api_token_id TEXT NOT NULL REFERENCES api_tokens(id),
               endpoint_id TEXT NOT NULL REFERENCES endpoints(id),
               virtual_key_ciphertext TEXT NOT NULL,
               virtual_key_hash TEXT NOT NULL,
               remote_key_id TEXT,
               policy_hash TEXT NOT NULL,
               status TEXT NOT NULL,
               last_error TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               PRIMARY KEY (api_token_id, endpoint_id)
             );
             CREATE INDEX IF NOT EXISTS managed_litellm_keys_status
             ON managed_litellm_keys(status, updated_at);",
        )?;
        Ok(())
    }

    /// Re-seals every value written before secrets were bound to their rows
    /// (L7), in one transaction. Afterwards an unbound value never opens. An
    /// empty value is a cleared secret, such as a removed endpoint's key.
    fn bind_secrets(&self, path: &Path) -> Result<()> {
        let mut connection = self.connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )?;
        let bound_before: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM store_meta WHERE key='secrets_bound')",
            [],
            |row| row.get(0),
        )?;
        let unbound: i64 = SEALED_COLUMNS
            .iter()
            .map(|(table, column, ..)| {
                connection.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table}
                         WHERE {column} IS NOT NULL AND {column}!='' AND {column} NOT LIKE 'v2.%'"
                    ),
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .sum::<rusqlite::Result<i64>>()?;
        // Re-sealing is a one-time migration. Afterwards an unbound value can only
        // have been written in by hand, and re-sealing it would bind it to
        // whichever row it was pasted into (review of L7).
        if bound_before {
            anyhow::ensure!(
                unbound == 0,
                "the database holds {unbound} secret(s) sealed without their row, which only an edit of the database can put there; restore a backup or remove them"
            );
            return Ok(());
        }
        if unbound == 0 {
            connection.execute(
                "INSERT OR IGNORE INTO store_meta (key,value) VALUES ('secrets_bound',?1)",
                [now()],
            )?;
            return Ok(());
        }
        // A 0.1.x controller cannot read re-sealed secrets, so keep a copy of
        // the database as it was, for a rollback. The first copy is kept.
        let snapshot = path.with_extension("sqlite3.pre-secret-binding");
        if !snapshot.exists() {
            connection
                .execute("VACUUM INTO ?1", [snapshot.to_string_lossy()])
                .context("snapshot the database before re-sealing secrets")?;
            // It holds the same secrets as the database, cleared ones included.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o600))?;
            }
            tracing::info!(snapshot = %snapshot.display(), "saved the database before re-sealing secrets");
        }
        let transaction = connection.transaction()?;
        let mut resealed = 0;
        for (table, column, key, bind) in SEALED_COLUMNS {
            let rows: Vec<(String, String)> = transaction
                .prepare(&format!(
                    "SELECT {key},{column} FROM {table}
                     WHERE {column} IS NOT NULL AND {column}!='' AND {column} NOT LIKE 'v2.%'"
                ))?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (row, value) in rows {
                let plaintext = self
                    .secrets
                    .open_unbound(&value)
                    .with_context(|| format!("re-seal {table}.{column}"))?;
                let sealed = self.secrets.seal(&bind(&row), &plaintext)?;
                transaction.execute(
                    &format!("UPDATE {table} SET {column}=?1 WHERE {key}=?2"),
                    rusqlite::params![sealed, row],
                )?;
                resealed += 1;
            }
        }
        transaction.execute(
            "INSERT OR IGNORE INTO store_meta (key,value) VALUES ('secrets_bound',?1)",
            [now()],
        )?;
        transaction.commit()?;
        if resealed > 0 {
            tracing::info!(resealed, "bound stored secrets to their rows");
        }
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

type Bind = fn(&str) -> Binding<'_>;

/// Every sealed column: its table, column, the SQL expression naming its row,
/// and the binding its values are sealed with.
const SEALED_COLUMNS: [(&str, &str, &str, Bind); 4] = [
    ("endpoints", "api_key_ciphertext", "id", endpoint_key),
    ("jobs", "contract_ciphertext", "id", job_contract),
    ("jobs", "source_ciphertext", "id", job_source),
    (
        "managed_litellm_keys",
        "virtual_key_ciphertext",
        "api_token_id || '/' || endpoint_id",
        managed_key,
    ),
];

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
