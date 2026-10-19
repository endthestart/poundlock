use super::jobs::insert_event;
use super::{Store, now};
use crate::domain::RunnerResult;
use anyhow::Result;
use chrono::{Duration, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

/// A job may use inference only while the application token that submitted it
/// is still active. Operator-submitted jobs have no token.
const SUBMITTER_ACTIVE: &str = "(jobs.api_token_id IS NULL OR EXISTS (SELECT 1 FROM api_tokens
     WHERE api_tokens.id=jobs.api_token_id AND api_tokens.revoked_at IS NULL))";

impl Store {
    pub fn issue_job_inference_token(&self, id: &str) -> Result<String> {
        let token = crate::crypto::random_token();
        let changed = self.connection()?.execute(
            &format!(
                "UPDATE jobs SET inference_token_hash=?1 WHERE id=?2 AND status='running'
                 AND cancel_requested=0 AND runner_lease_expires_at > ?3 AND {SUBMITTER_ACTIVE}"
            ),
            params![crate::crypto::token_hash(&token), id, now()],
        )?;
        anyhow::ensure!(changed == 1, "inference requires a live runner lease");
        Ok(token)
    }

    pub fn job_inference_authorized(&self, id: &str, token: &str) -> Result<bool> {
        let hash: Option<String> = self
            .connection()?
            .query_row(
                &format!(
                    "SELECT inference_token_hash FROM jobs WHERE id=?1 AND status='running'
                     AND cancel_requested=0 AND runner_lease_expires_at > ?2 AND {SUBMITTER_ACTIVE}"
                ),
                params![id, now()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(hash.is_some_and(|hash| crate::crypto::token_matches(&hash, token)))
    }

    /// `wall_seconds` is the claiming runner's own configured wall budget
    /// (`POUNDLOCK_JOB_WALL_SECONDS`), reported fresh on every claim
    /// (`POST /runner/v1/claim`). It is unknown to the controller before a
    /// runner claims — never guessed — so `None` here leaves the stored
    /// budget and deadline absent rather than inventing a value. The
    /// deadline is anchored on this claim's own timestamp (the same instant
    /// recorded by the `runner_claimed` event below), not extended by later
    /// heartbeats: it is the controller's view of when this attempt's
    /// runner-reported budget runs out, not a lease renewal.
    pub fn claim_runner_job(&self, wall_seconds: Option<u64>) -> Result<Option<String>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<String> = transaction
            .query_row(
                "SELECT id FROM jobs WHERE status='ready' AND cancel_requested=0
             ORDER BY created_at LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = &id {
            let claimed_at = Utc::now();
            let expiry = (claimed_at + Duration::seconds(120)).to_rfc3339();
            let deadline_at = wall_seconds
                .map(|seconds| (claimed_at + Duration::seconds(seconds as i64)).to_rfc3339());
            transaction.execute(
                "UPDATE jobs SET status='running',runner_lease_expires_at=?1,
                updated_at=?2,runner_wall_seconds=?3,wall_deadline_at=?4 WHERE id=?5",
                params![
                    expiry,
                    now(),
                    wall_seconds.map(|seconds| seconds as i64),
                    deadline_at,
                    id
                ],
            )?;
            insert_event(
                &transaction,
                id,
                "runner_claimed",
                "Runner assigned.",
                &json!({"wall_seconds":wall_seconds,"deadline_at":deadline_at}),
            )?;
        }
        transaction.commit()?;
        Ok(id)
    }

    /// The runner-reported wall budget and the controller's computed
    /// deadline for a job's current (or most recent) claim, for the job and
    /// submit responses. Both are `None` until a runner has claimed the job
    /// and reported its configured budget — this never guesses a value a
    /// runner has not actually reported.
    pub fn job_wall_budget(&self, id: &str) -> Result<(Option<i64>, Option<String>)> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT runner_wall_seconds,wall_deadline_at FROM jobs WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    pub fn runner_heartbeat(&self, id: &str) -> Result<bool> {
        let expiry = (Utc::now() + Duration::seconds(120)).to_rfc3339();
        let connection = self.connection()?;
        let changed = connection.execute(
            "UPDATE jobs SET runner_lease_expires_at=?1,updated_at=?2
             WHERE id=?3 AND status='running' AND cancel_requested=0
             AND runner_lease_expires_at > ?2",
            params![expiry, now(), id],
        )?;
        // Missing, cancelled, expired and terminal attempts must all stop.
        Ok(changed == 0)
    }

    pub fn complete_job(&self, id: &str, result: &RunnerResult) -> Result<()> {
        self.finish_job(
            id,
            "completed",
            Some(serde_json::to_value(result)?),
            None,
            "completed",
            "Agent finished. Results are available.",
        )
    }

    pub fn fail_job(&self, id: &str, error: Value, cancelled: bool) -> Result<()> {
        self.fail_job_with_result(id, error, cancelled, None)
    }

    pub fn fail_job_with_result(
        &self,
        id: &str,
        error: Value,
        cancelled: bool,
        result: Option<&RunnerResult>,
    ) -> Result<()> {
        let (status, kind, message) = if cancelled {
            ("cancelled", "cancelled", "Job cancelled.")
        } else {
            ("failed", "failed", "Job failed. Check the error details.")
        };
        self.finish_job(
            id,
            status,
            result.map(serde_json::to_value).transpose()?,
            Some(error),
            kind,
            message,
        )
    }

    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: Option<Value>,
        error: Option<Value>,
        kind: &str,
        message: &str,
    ) -> Result<()> {
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        let (current, previous_result, previous_error, cancelled, expiry): (
            String,
            Option<String>,
            Option<String>,
            bool,
            Option<String>,
        ) = transaction.query_row(
            "SELECT status,result_json,error_json,cancel_requested,runner_lease_expires_at
                 FROM jobs WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        // Cancellation can revoke inference and finish the harness before the
        // supervisor's next heartbeat. Resolve that race in the same transaction
        // as result acceptance, including identical outbox replays.
        let (status, error, kind, message) = if cancelled {
            (
                "cancelled",
                Some(
                    json!({"layer":"controller","code":"cancelled_or_lease_lost",
                "message":"The job was cancelled before its final result was accepted.",
                "reported_error":error}),
                ),
                "cancelled",
                "Job cancelled.",
            )
        } else {
            (status, error, kind, message)
        };
        if crate::domain::is_terminal_job_status(&current) {
            if current == status
                && super::json_or_none(previous_result) == result
                && super::json_or_none(previous_error) == error
            {
                return Ok(());
            }
            return Err(
                super::JobAlreadyEndedError(format!("job already ended as {current}")).into(),
            );
        }
        if status == "completed"
            && (current != "running"
                || cancelled
                || expiry
                    .as_deref()
                    .is_none_or(|value| value <= now().as_str()))
        {
            anyhow::bail!("completion requires a live, uncancelled runner lease");
        }
        // The source is needed only to prepare the workspace, which the
        // runner did while the job ran. Nothing reads it after the job ends;
        // keeping it kept every Git read token and snapshot forever.
        transaction.execute(
            "UPDATE jobs SET status=?1,result_json=?2,error_json=?3,
            runner_lease_expires_at=NULL,source_ciphertext=NULL,updated_at=?4 WHERE id=?5",
            params![
                status,
                result.map(|value| value.to_string()),
                error.map(|value| value.to_string()),
                now(),
                id
            ],
        )?;
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

    pub fn fail_expired_runners(&self) -> Result<Vec<String>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM jobs WHERE status='running'
                 AND runner_lease_expires_at IS NOT NULL AND runner_lease_expires_at < ?1",
            )?;
            statement
                .query_map([now()], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in &ids {
            let error = json!({"layer":"runner","code":"runner_lost",
                "message":"Runner heartbeat expired; execution may be incomplete. Inspect retained runner evidence before retrying.",
                "retry_safe":false});
            transaction.execute(
                "UPDATE jobs SET status='failed',error_json=?1,runner_lease_expires_at=NULL,
                 source_ciphertext=NULL,updated_at=?2 WHERE id=?3",
                params![error.to_string(), now(), id],
            )?;
            insert_event(&transaction, id, "failed", "Runner lease expired.", &error)?;
        }
        transaction.commit()?;
        Ok(ids)
    }
}
