use super::{Store, job_source, json_or_none, now};
use crate::crypto::token_hash;
use crate::domain::*;
use anyhow::{Result, bail};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

impl Store {
    pub fn submit_job(&self, input: &SubmitJob) -> Result<Job> {
        self.submit_job_for(input, None, u32::MAX)
    }

    /// `api_token_id` is the submitting application token, `None` for the
    /// operator. A token may hold at most `active_job_limit` unfinished jobs;
    /// replaying one of them with its idempotency key still returns it.
    /// Operator submissions are not capped.
    pub fn submit_job_for(
        &self,
        input: &SubmitJob,
        api_token_id: Option<&str>,
        active_job_limit: u32,
    ) -> Result<Job> {
        input.policy.validate()?;
        if input.project_id.is_empty() == input.source.is_none() {
            bail!("Supply source or a registered project, not both.");
        }
        if let Some(source) = &input.source {
            source.validate()?;
        }
        let request_hash = token_hash(&serde_json::to_string(&(input, api_token_id))?);
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = transaction.query_row(
            "SELECT id,idempotency_key,project_id,model_profile_id,harness_profile_id,prompt,
             status,attempt_id,cancel_requested,result_json,error_json,created_at,updated_at,policy_json,request_hash
             FROM jobs WHERE idempotency_key=?1", [&input.idempotency_key],
            |row| Ok((job_row(row)?, row.get::<_, String>(14)?)),
        ).optional()?;
        if let Some((existing, stored)) = existing {
            if stored != request_hash {
                bail!("idempotency key was reused for different input");
            }
            return Ok(existing);
        }
        if let Some(token) = api_token_id {
            let active: u32 = transaction.query_row(
                concat!(
                    "SELECT count(*) FROM jobs WHERE api_token_id=?1
                 AND status NOT IN ",
                    crate::terminal_job_statuses!()
                ),
                [token],
                |row| row.get(0),
            )?;
            if active >= active_job_limit {
                return Err(super::TooManyActiveJobsError(active_job_limit).into());
            }
        }
        let timestamp = now();
        let job = Job {
            id: Uuid::new_v4().to_string(),
            idempotency_key: input.idempotency_key.clone(),
            project_id: (!input.project_id.is_empty()).then(|| input.project_id.clone()),
            model_profile_id: input.model_profile_id.clone(),
            harness_profile_id: input.harness_profile_id.clone(),
            prompt: input.prompt.clone(),
            policy: input.policy.clone(),
            status: "submitted".into(),
            attempt_id: Uuid::new_v4().to_string(),
            cancel_requested: false,
            result: None,
            error: None,
            created_at: timestamp.clone(),
            updated_at: timestamp.clone(),
        };
        let summary = input.source.as_ref().map(crate::source::JobSource::summary);
        let contract = self.freeze_contract(&transaction, &job, summary.clone(), api_token_id)?;
        let source = input
            .source
            .as_ref()
            .map(|source| {
                self.secrets
                    .seal(&job_source(&job.id), &serde_json::to_string(source)?)
            })
            .transpose()?;
        transaction.execute(
            "INSERT INTO jobs (id,idempotency_key,request_hash,project_id,model_profile_id,
             harness_profile_id,prompt,status,attempt_id,created_at,updated_at,contract_ciphertext,source_ciphertext,policy_json,api_token_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,?11,?12,?13,?14)",
            params![
                job.id,
                job.idempotency_key,
                request_hash,
                job.project_id,
                job.model_profile_id,
                job.harness_profile_id,
                job.prompt,
                job.status,
                job.attempt_id,
                timestamp,
                contract, source, serde_json::to_string(&input.policy)?, api_token_id
            ],
        )?;
        insert_event(
            &transaction,
            &job.id,
            "accepted",
            "Job accepted.",
            &json!({"source":summary}),
        )?;
        transaction.commit()?;
        Ok(job)
    }

    pub fn list_jobs(&self, limit: u32) -> Result<Vec<Job>> {
        self.list_jobs_for(limit, None)
    }

    /// `api_token_id: Some(id)` lists only that token's jobs; `None` lists
    /// every job, which is the operator view.
    pub fn list_jobs_for(&self, limit: u32, api_token_id: Option<&str>) -> Result<Vec<Job>> {
        let connection = self.connection()?;
        let select =
            "SELECT id,idempotency_key,project_id,model_profile_id,harness_profile_id,prompt,
             status,attempt_id,cancel_requested,NULL,NULL,created_at,updated_at,policy_json
             FROM jobs";
        Ok(match api_token_id {
            Some(token) => connection
                .prepare(&format!(
                    "{select} WHERE api_token_id=?1 ORDER BY created_at DESC LIMIT ?2"
                ))?
                .query_map(params![token, limit], job_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => connection
                .prepare(&format!("{select} ORDER BY created_at DESC LIMIT ?1"))?
                .query_map([limit], job_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        })
    }

    pub fn job(&self, id: &str) -> Result<Option<Job>> {
        self.job_where("id", id)
    }

    /// Which API token submitted this job, `None` for one an operator
    /// submitted. Recorded at submission and, until now, never read back:
    /// job reads, patch downloads and cancels were gated on holding the
    /// permission, not on being the submitter, so any developer token could
    /// read any other token's source patches and cancel its work.
    pub fn job_api_token_id(&self, id: &str) -> Result<Option<Option<String>>> {
        Ok(self
            .connection()?
            .query_row("SELECT api_token_id FROM jobs WHERE id=?1", [id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?)
    }

    fn job_where(&self, column: &str, value: &str) -> Result<Option<Job>> {
        let sql = format!(
            "SELECT id,idempotency_key,project_id,model_profile_id,
            harness_profile_id,prompt,status,attempt_id,cancel_requested,result_json,error_json,
            created_at,updated_at,policy_json FROM jobs WHERE {column}=?1"
        );
        Ok(self
            .connection()?
            .query_row(&sql, [value], job_row)
            .optional()?)
    }

    pub fn events(&self, job_id: &str, after: i64) -> Result<Vec<Event>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT sequence,job_id,kind,message,payload_json,created_at FROM events
             WHERE job_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 1000",
        )?;
        Ok(statement
            .query_map(params![job_id, after], |row| {
                let payload: String = row.get(4)?;
                Ok(Event {
                    sequence: row.get(0)?,
                    job_id: row.get(1)?,
                    kind: row.get(2)?,
                    message: row.get(3)?,
                    payload: serde_json::from_str(&payload).unwrap_or(json!({})),
                    created_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_event(&self, job_id: &str, kind: &str, message: &str, payload: Value) -> Result<()> {
        let connection = self.connection()?;
        insert_event(&connection, job_id, kind, message, &payload)
    }

    /// Moves a job between unfinished states. A job ends only through
    /// `finish_job` or `fail_expired_runners`, which also remove its source.
    pub fn set_job_status(&self, id: &str, status: &str, kind: &str, message: &str) -> Result<()> {
        anyhow::ensure!(
            !crate::domain::is_terminal_job_status(status),
            "a job's final status is recorded with its result, not set directly"
        );
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            concat!(
                "UPDATE jobs SET status=?1,updated_at=?2 WHERE id=?3
             AND status NOT IN ",
                crate::terminal_job_statuses!(),
                " AND cancel_requested=0"
            ),
            params![status, now(), id],
        )?;
        if changed == 0 {
            return Ok(());
        }
        insert_event(
            &transaction,
            id,
            kind,
            message,
            &json!({ "status": status }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn request_cancel(&self, id: &str) -> Result<bool> {
        let changed = self.connection()?.execute(
            concat!(
                "UPDATE jobs SET cancel_requested=1,updated_at=?1
             WHERE id=?2 AND status NOT IN ",
                crate::terminal_job_statuses!()
            ),
            params![now(), id],
        )?;
        if changed > 0 {
            self.add_event(id, "cancel_requested", "Cancellation requested.", json!({}))?;
        }
        Ok(changed > 0)
    }

    pub fn schedulable_jobs(&self) -> Result<Vec<Job>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,idempotency_key,project_id,model_profile_id,harness_profile_id,prompt,
             status,attempt_id,cancel_requested,result_json,error_json,created_at,updated_at,policy_json
             FROM jobs WHERE status IN ('submitted','waiting_node','waiting_capacity','preparing')
             OR (status='ready' AND cancel_requested=1) ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], job_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn occupying_jobs(&self) -> Result<Vec<Job>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,idempotency_key,project_id,model_profile_id,harness_profile_id,prompt,
             status,attempt_id,cancel_requested,result_json,error_json,created_at,updated_at,policy_json
             FROM jobs WHERE status IN ('preparing','ready','running')
             ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'ready' THEN 1 ELSE 2 END, created_at",
        )?;
        Ok(statement
            .query_map([], job_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_jobs(&self) -> Result<Vec<Job>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,idempotency_key,project_id,model_profile_id,harness_profile_id,prompt,
             status,attempt_id,cancel_requested,result_json,error_json,created_at,updated_at,policy_json
             FROM jobs WHERE status IN ('ready','running') ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], job_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn mark_wake_sent(&self, id: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE jobs SET wake_last_at=?1 WHERE id=?2",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn wake_due(&self, id: &str, seconds: i64) -> Result<bool> {
        let value: Option<String> = self.connection()?.query_row(
            "SELECT wake_last_at FROM jobs WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        Ok(value
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(&raw).ok())
            .map(|then| {
                Utc::now()
                    .signed_duration_since(then.with_timezone(&Utc))
                    .num_seconds()
                    >= seconds
            })
            .unwrap_or(true))
    }
}

pub(super) fn job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: row.get(0)?,
        idempotency_key: row.get(1)?,
        project_id: row.get(2)?,
        model_profile_id: row.get(3)?,
        harness_profile_id: row.get(4)?,
        prompt: row.get(5)?,
        status: row.get(6)?,
        attempt_id: row.get(7)?,
        cancel_requested: row.get(8)?,
        result: json_or_none(row.get(9)?),
        error: json_or_none(row.get(10)?),
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        policy: serde_json::from_str(&row.get::<_, String>(13)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

pub(super) fn insert_event(
    connection: &rusqlite::Connection,
    job_id: &str,
    kind: &str,
    message: &str,
    payload: &Value,
) -> Result<()> {
    connection.execute(
        "INSERT INTO events (job_id,kind,message,payload_json,created_at)
        VALUES (?1,?2,?3,?4,?5)",
        params![job_id, kind, message, payload.to_string(), now()],
    )?;
    Ok(())
}
