use super::{Store, managed_key, now};
use crate::domain::ManagedLiteLlmKey;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

impl Store {
    pub(crate) fn managed_litellm_key(
        &self,
        api_token_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<ManagedLiteLlmKey>> {
        let connection = self.connection()?;
        self.managed_litellm_key_in(&connection, api_token_id, endpoint_id)
    }

    pub(crate) fn managed_litellm_key_in(
        &self,
        connection: &Connection,
        api_token_id: &str,
        endpoint_id: &str,
    ) -> Result<Option<ManagedLiteLlmKey>> {
        let row = connection
            .query_row(
                "SELECT api_token_id,endpoint_id,virtual_key_ciphertext,virtual_key_hash,
                 remote_key_id,policy_hash,status,last_error,created_at,updated_at
                 FROM managed_litellm_keys WHERE api_token_id=?1 AND endpoint_id=?2",
                params![api_token_id, endpoint_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?;
        row.map(|row| {
            let bound = format!("{}/{}", row.0, row.1);
            Ok(ManagedLiteLlmKey {
                api_token_id: row.0,
                endpoint_id: row.1,
                virtual_key: self.secrets.open(&managed_key(&bound), &row.2)?,
                virtual_key_hash: row.3,
                remote_key_id: row.4,
                policy_hash: row.5,
                status: row.6,
                last_error: row.7,
                created_at: row.8,
                updated_at: row.9,
            })
        })
        .transpose()
    }

    pub(crate) fn save_managed_litellm_key(&self, key: &ManagedLiteLlmKey) -> Result<()> {
        let row = format!("{}/{}", key.api_token_id, key.endpoint_id);
        let ciphertext = self.secrets.seal(&managed_key(&row), &key.virtual_key)?;
        self.connection()?.execute(
            "INSERT INTO managed_litellm_keys
             (api_token_id,endpoint_id,virtual_key_ciphertext,virtual_key_hash,remote_key_id,
              policy_hash,status,last_error,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(api_token_id,endpoint_id) DO UPDATE SET
              virtual_key_ciphertext=excluded.virtual_key_ciphertext,
              virtual_key_hash=excluded.virtual_key_hash,
              remote_key_id=excluded.remote_key_id,
              policy_hash=excluded.policy_hash,status=excluded.status,
              last_error=excluded.last_error,updated_at=excluded.updated_at",
            params![
                key.api_token_id,
                key.endpoint_id,
                ciphertext,
                key.virtual_key_hash,
                key.remote_key_id,
                key.policy_hash,
                key.status,
                key.last_error,
                key.created_at,
                key.updated_at,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn managed_litellm_keys_for_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Vec<ManagedLiteLlmKey>> {
        self.managed_litellm_keys_where("endpoint_id=?1", endpoint_id)
    }

    pub(crate) fn pending_managed_litellm_revocations(&self) -> Result<Vec<ManagedLiteLlmKey>> {
        self.managed_litellm_keys_where("status='pending_revoke'", "")
    }

    pub(crate) fn managed_litellm_key_statuses(&self) -> Result<Vec<serde_json::Value>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT api_token_id,endpoint_id,status,last_error,created_at,updated_at
             FROM managed_litellm_keys ORDER BY updated_at DESC",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "api_token_id": row.get::<_, String>(0)?,
                    "endpoint_id": row.get::<_, String>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "last_error": row.get::<_, Option<String>>(3)?,
                    "created_at": row.get::<_, String>(4)?,
                    "updated_at": row.get::<_, String>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn managed_litellm_keys_where(
        &self,
        clause: &str,
        parameter: &str,
    ) -> Result<Vec<ManagedLiteLlmKey>> {
        let connection = self.connection()?;
        let sql = format!(
            "SELECT api_token_id,endpoint_id FROM managed_litellm_keys WHERE {clause} ORDER BY updated_at"
        );
        let mut statement = connection.prepare(&sql)?;
        let pairs = if parameter.is_empty() {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            statement
                .query_map([parameter], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        drop(statement);
        drop(connection);
        pairs
            .into_iter()
            .map(|(token, endpoint)| {
                self.managed_litellm_key(&token, &endpoint)?
                    .ok_or_else(|| anyhow::anyhow!("managed key disappeared during read"))
            })
            .collect()
    }

    pub(crate) fn record_managed_litellm_key_error(
        &self,
        api_token_id: &str,
        endpoint_id: &str,
        status: &str,
        message: &str,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE managed_litellm_keys SET status=?1,
             last_error=CASE WHEN ?2='' THEN NULL ELSE ?2 END,updated_at=?3
             WHERE api_token_id=?4 AND endpoint_id=?5",
            params![status, message, now(), api_token_id, endpoint_id],
        )?;
        Ok(())
    }
}
