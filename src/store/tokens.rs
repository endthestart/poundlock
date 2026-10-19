use super::jobs::insert_event;
use super::{Store, now};
use crate::crypto::{random_token, token_hash};
use crate::domain::{ApiToken, ApiTokenPrincipal, CreateApiToken, IssuedApiToken};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// How stale an API token's `last_used_at` may get before a request refreshes it.
const LAST_USED_REFRESH_SECONDS: i64 = 60;

/// Name of the controller-owned identity that runs operator checks.
pub const OPERATOR_CHECK_TOKEN: &str = "Poundlock operator checks";

impl Store {
    pub fn create_api_token(&self, input: &CreateApiToken) -> Result<IssuedApiToken> {
        let token = random_token();
        let details = ApiToken {
            id: Uuid::new_v4().to_string(),
            name: input.name.clone(),
            permissions: input.permissions.clone(),
            created_at: now(),
            last_used_at: None,
            revoked_at: None,
        };
        self.connection()?.execute(
            "INSERT INTO api_tokens
             (id,name,secret_hash,permissions_json,created_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                details.id,
                details.name,
                token_hash(&token),
                serde_json::to_string(&details.permissions)?,
                details.created_at
            ],
        )?;
        Ok(IssuedApiToken { details, token })
    }

    /// The identity Poundlock itself uses to run an operator's checks —
    /// verifying which reasoning levels a route honours — through the same
    /// scoped path an application uses. It exists because the gateway's
    /// administrative key is never used for inference (ADR 0001).
    ///
    /// Created on first use. Its secret is discarded at creation, so nobody,
    /// including the operator, can authenticate as it: it is a principal for
    /// Poundlock's own requests, not a token anyone holds. Revoking it in
    /// Access stops the checks; the next check creates a fresh one.
    pub fn operator_check_principal(&self) -> Result<ApiTokenPrincipal> {
        let existing = self
            .list_api_tokens()?
            .into_iter()
            .find(|token| token.name == OPERATOR_CHECK_TOKEN && token.revoked_at.is_none());
        let id = match existing {
            Some(token) => token.id,
            None => {
                self.create_api_token(&CreateApiToken {
                    name: OPERATOR_CHECK_TOKEN.into(),
                    permissions: vec![
                        "models:read".into(),
                        "inference:invoke".into(),
                        "external:invoke".into(),
                    ],
                })?
                .details
                .id
            }
        };
        self.api_token_principal(&id)?
            .ok_or_else(|| anyhow::anyhow!("operator check identity is unavailable"))
    }

    pub fn list_api_tokens(&self) -> Result<Vec<ApiToken>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,name,permissions_json,created_at,last_used_at,revoked_at
             FROM api_tokens ORDER BY created_at DESC",
        )?;
        Ok(statement
            .query_map([], token_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn api_token_principal(&self, id: &str) -> Result<Option<ApiTokenPrincipal>> {
        let row = self
            .connection()?
            .query_row(
                "SELECT id,name,permissions_json FROM api_tokens
                 WHERE id=?1 AND revoked_at IS NULL",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, name, permissions_json)| {
            Ok(ApiTokenPrincipal {
                id,
                name,
                permissions: serde_json::from_str(&permissions_json)?,
            })
        })
        .transpose()
    }

    pub fn api_token_has_permission(&self, token: &str, permission: &str) -> Result<bool> {
        Ok(self.authenticate_api_token(token, permission)?.is_some())
    }

    pub fn authenticate_api_token(
        &self,
        token: &str,
        permission: &str,
    ) -> Result<Option<ApiTokenPrincipal>> {
        if token.is_empty() {
            return Ok(None);
        }
        let supplied_hash = token_hash(token);
        let mut authenticated = None;
        let mut last_used = None;
        {
            let reader = self.reader()?;
            let mut statement = reader.prepare(
                "SELECT id,name,secret_hash,permissions_json,last_used_at FROM api_tokens
                 WHERE revoked_at IS NULL ORDER BY id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;
            for row in rows {
                let (id, name, expected_hash, permissions_json, used) = row?;
                let matches = bool::from(expected_hash.as_bytes().ct_eq(supplied_hash.as_bytes()));
                if matches {
                    let permissions: Vec<String> = serde_json::from_str(&permissions_json)?;
                    authenticated = Some(ApiTokenPrincipal {
                        id,
                        name,
                        permissions,
                    });
                    last_used = used;
                }
            }
        }
        let Some(principal) = authenticated else {
            return Ok(None);
        };
        if !principal
            .permissions
            .iter()
            .any(|candidate| candidate == permission)
        {
            return Ok(None);
        }
        // Shown to operators, never enforced. A write commits to disk, so it is
        // refreshed at most once per window, and only then is the writer used.
        let stale_before = (chrono::Utc::now()
            - chrono::Duration::seconds(LAST_USED_REFRESH_SECONDS))
        .to_rfc3339();
        if last_used.is_some_and(|used| used >= stale_before) {
            return Ok(Some(principal));
        }
        self.connection()?.execute(
            "UPDATE api_tokens SET last_used_at=?1 WHERE id=?2 AND revoked_at IS NULL
             AND (last_used_at IS NULL OR last_used_at<?3)",
            params![now(), principal.id, stale_before],
        )?;
        Ok(Some(principal))
    }

    pub fn revoke_api_token(&self, id: &str) -> Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let revoked = transaction.execute(
            "UPDATE api_tokens SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL",
            params![now(), id],
        )? == 1;
        if revoked {
            transaction.execute(
                "UPDATE managed_litellm_keys
                 SET status='pending_revoke',last_error=NULL,updated_at=?1
                 WHERE api_token_id=?2 AND status NOT IN ('revoked','deleted')",
                params![now(), id],
            )?;
            // Revocation is the incident response for a leaked token, so the
            // work it queued stops with it rather than running on the shared key.
            let unfinished = {
                let mut statement = transaction.prepare(concat!(
                    "SELECT id FROM jobs WHERE api_token_id=?1 AND cancel_requested=0
                     AND status NOT IN ",
                    crate::terminal_job_statuses!()
                ))?;
                statement
                    .query_map([id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for job in &unfinished {
                transaction.execute(
                    "UPDATE jobs SET cancel_requested=1,updated_at=?1 WHERE id=?2",
                    params![now(), job],
                )?;
                insert_event(
                    &transaction,
                    job,
                    "cancel_requested",
                    "Cancellation requested: the application token that submitted this job was revoked.",
                    &json!({"reason":"api_token_revoked"}),
                )?;
            }
        }
        transaction.commit()?;
        Ok(revoked)
    }
}

fn token_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiToken> {
    let permissions: String = row.get(2)?;
    let permissions = serde_json::from_str(&permissions).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, error.into())
    })?;
    Ok(ApiToken {
        id: row.get(0)?,
        name: row.get(1)?,
        permissions,
        created_at: row.get(3)?,
        last_used_at: row.get(4)?,
        revoked_at: row.get(5)?,
    })
}

#[cfg(test)]
mod reader_tests {
    use crate::crypto::SecretBox;
    use crate::domain::CreateApiToken;
    use crate::store::Store;
    use std::time::{Duration, Instant};

    #[test]
    fn a_token_check_does_not_wait_behind_a_write() {
        let directory = tempfile::tempdir().unwrap();
        let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
        let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
        let issued = store
            .create_api_token(&CreateApiToken {
                name: "client".into(),
                permissions: vec!["inference:invoke".into()],
            })
            .unwrap();
        // The first check records last use; the next ones only read.
        assert!(
            store
                .authenticate_api_token(&issued.token, "inference:invoke")
                .unwrap()
                .is_some()
        );
        let held = store.connection().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let checker = store.clone();
        let token = issued.token.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let found = checker
                .authenticate_api_token(&token, "inference:invoke")
                .unwrap();
            sender.send((found.is_some(), started.elapsed())).unwrap();
        });
        let (found, elapsed) = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("the token check waited for the held writer");
        drop(held);
        assert!(found, "the token was not recognised");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }
}
