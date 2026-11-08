//! The install's frontier budget and the spend recorded against it. Rows hold
//! amounts, token counts and identifiers only, never request content.

use super::Store;
use crate::budget::{Charge, FrontierBudget};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

/// Spend older than this is never needed for a weekly or monthly period.
const SPEND_RETENTION_DAYS: i64 = 400;

const BUDGET_KEY: &str = "frontier_budget";

/// One metered request, as recorded.
#[derive(Debug, Clone)]
pub struct SpendRecord<'a> {
    pub request_id: &'a str,
    pub api_token_id: &'a str,
    pub profile_id: &'a str,
    /// `relay` for interactive inference, `job` for a durable job's.
    pub origin: &'static str,
    pub job_id: Option<&'a str>,
    pub charge: &'a Charge,
    pub at: DateTime<Utc>,
}

/// Spend in a period, grouped by application and profile.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SpendLine {
    pub api_token_id: String,
    pub api_token_name: Option<String>,
    pub profile_id: String,
    pub profile_name: Option<String>,
    pub requests: u64,
    pub cost_usd: f64,
    /// Requests charged their admission estimate because they reported nothing.
    pub estimated_requests: u64,
}

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS controller_settings (
       key TEXT PRIMARY KEY,
       value_json TEXT NOT NULL,
       updated_at TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS frontier_spend (
       request_id TEXT PRIMARY KEY,
       at_ms INTEGER NOT NULL,
       api_token_id TEXT NOT NULL,
       profile_id TEXT NOT NULL,
       origin TEXT NOT NULL,
       job_id TEXT,
       prompt_tokens INTEGER,
       completion_tokens INTEGER,
       cost_usd REAL NOT NULL,
       cost_source TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS frontier_spend_at ON frontier_spend(at_ms);
     CREATE INDEX IF NOT EXISTS frontier_spend_job ON frontier_spend(job_id);";

impl Store {
    pub fn frontier_budget(&self) -> Result<FrontierBudget> {
        let raw: Option<String> = self
            .reader()?
            .query_row(
                "SELECT value_json FROM controller_settings WHERE key=?1",
                [BUDGET_KEY],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            Some(raw) => serde_json::from_str(&raw).context("read the frontier budget"),
            None => Ok(FrontierBudget::default()),
        }
    }

    pub fn save_frontier_budget(&self, budget: &FrontierBudget) -> Result<()> {
        budget.validate()?;
        self.connection()?.execute(
            "INSERT INTO controller_settings (key,value_json,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json, updated_at=excluded.updated_at",
            params![BUDGET_KEY, serde_json::to_string(budget)?, super::now()],
        )?;
        Ok(())
    }

    /// Records one metered request. Recording the same request twice keeps the
    /// first row.
    pub fn record_frontier_spend(&self, record: &SpendRecord<'_>) -> Result<()> {
        let at_ms = record.at.timestamp_millis();
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR IGNORE INTO frontier_spend
             (request_id,at_ms,api_token_id,profile_id,origin,job_id,prompt_tokens,
              completion_tokens,cost_usd,cost_source)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                record.request_id,
                at_ms,
                record.api_token_id,
                record.profile_id,
                record.origin,
                record.job_id,
                record.charge.prompt_tokens.map(|value| value as i64),
                record.charge.completion_tokens.map(|value| value as i64),
                record.charge.cost_usd,
                record.charge.source,
            ],
        )?;
        connection.execute(
            "DELETE FROM frontier_spend WHERE at_ms < ?1",
            [at_ms - SPEND_RETENTION_DAYS * 24 * 60 * 60 * 1000],
        )?;
        Ok(())
    }

    /// The total recorded at or after `since`.
    pub fn frontier_spend_since(&self, since: DateTime<Utc>) -> Result<f64> {
        Ok(self.reader()?.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM frontier_spend WHERE at_ms >= ?1",
            [since.timestamp_millis()],
            |row| row.get(0),
        )?)
    }

    /// What one durable job's external requests have cost, all attempts.
    pub fn frontier_spend_for_job(&self, job_id: &str) -> Result<f64> {
        Ok(self.reader()?.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM frontier_spend WHERE job_id = ?1",
            [job_id],
            |row| row.get(0),
        )?)
    }

    /// Spend at or after `since`, by application and profile, largest first.
    pub fn frontier_spend_lines(&self, since: DateTime<Utc>) -> Result<Vec<SpendLine>> {
        let connection = self.reader()?;
        let mut statement = connection.prepare(
            "SELECT spend.api_token_id, tokens.name, spend.profile_id, profiles.name,
                    COUNT(*), SUM(spend.cost_usd),
                    SUM(CASE WHEN spend.cost_source='estimate' THEN 1 ELSE 0 END)
             FROM frontier_spend spend
             LEFT JOIN api_tokens tokens ON tokens.id = spend.api_token_id
             LEFT JOIN model_profiles profiles ON profiles.id = spend.profile_id
             WHERE spend.at_ms >= ?1
             GROUP BY spend.api_token_id, spend.profile_id
             ORDER BY SUM(spend.cost_usd) DESC",
        )?;
        let lines = statement
            .query_map([since.timestamp_millis()], |row| {
                Ok(SpendLine {
                    api_token_id: row.get(0)?,
                    api_token_name: row.get(1)?,
                    profile_id: row.get(2)?,
                    profile_name: row.get(3)?,
                    requests: row.get::<_, i64>(4)? as u64,
                    cost_usd: row.get(5)?,
                    estimated_requests: row.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(lines)
    }
}

#[cfg(test)]
#[path = "spend_tests.rs"]
mod tests;
