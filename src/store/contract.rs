use super::configuration::{MODEL_COLUMNS, managed_identity_row, model_row, project_row};
use super::{Store, endpoint_key, job_contract, job_source};
use crate::domain::*;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

impl Store {
    // Called inside the submission transaction: every resolved value belongs
    // to the same database snapshot as the accepted job.
    pub(super) fn freeze_contract(
        &self,
        db: &Connection,
        job: &Job,
        source: Option<serde_json::Value>,
        api_token_id: Option<&str>,
    ) -> Result<String> {
        let project = if let Some(project_id) = &job.project_id {
            db.query_row(
                "SELECT id,name,roots_json,created_at,instruction_files_json
             FROM projects WHERE id=?1 AND removed_at IS NULL",
                [project_id],
                project_row,
            )?
        } else {
            Project {
                id: String::new(),
                name: "Supplied workspace".into(),
                roots: vec![],
                instruction_files: vec![],
                created_at: job.created_at.clone(),
            }
        };
        let model = db.query_row(
            &format!(
                "SELECT {MODEL_COLUMNS} FROM model_profiles WHERE id=?1 AND removed_at IS NULL"
            ),
            [&job.model_profile_id],
            model_row,
        )?;
        let (endpoint, ciphertext) = db.query_row(
            "SELECT id,name,kind,base_url,api_key_ciphertext,created_at,managed_identity_json
             FROM endpoints WHERE id=?1 AND removed_at IS NULL",
            [&model.endpoint_id],
            |row| {
                Ok((
                    Endpoint {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        kind: row.get(2)?,
                        base_url: row.get(3)?,
                        api_key_set: true,
                        created_at: row.get(5)?,
                        managed_identity: managed_identity_row(row, 6)?,
                        public_url: None,
                        data_destination: None,
                        last_check: ResourceCheck::default(),
                    },
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        let mut api_key = self
            .secrets
            .open(&endpoint_key(&endpoint.id), &ciphertext)?;
        if endpoint.kind == "litellm" && endpoint.managed_identity.enabled {
            // Without a token identity there is no virtual key to substitute,
            // and the fallback was the endpoint's own credential — the
            // LiteLLM administrative key — handed to a runner to spend.
            // `admin.rs` already refuses to use that key for inference in so
            // many words; an operator-submitted job is the same act.
            let token_id = api_token_id.ok_or_else(|| {
                anyhow::anyhow!(
                    "This endpoint issues a managed identity per application token, so a job must be submitted with one. Poundlock does not use the LiteLLM administrative key for inference."
                )
            })?;
            let mapping = self
                .managed_litellm_key_in(db, token_id, &endpoint.id)?
                .filter(|mapping| mapping.status == "active")
                .ok_or_else(|| {
                    anyhow::anyhow!("managed LiteLLM identity is unavailable for this job")
                })?;
            api_key = mapping.virtual_key;
        }
        let harness = db.query_row(
            "SELECT id,name,kind,version,created_at FROM harness_profiles WHERE id=?1",
            [&job.harness_profile_id],
            |row| {
                Ok(HarnessProfile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    version: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )?;
        self.secrets.seal(
            &job_contract(&job.id),
            &serde_json::to_string(&RunnerPackage {
                job: job.clone(),
                project,
                source,
                model,
                endpoint: EndpointSecret { endpoint, api_key },
                harness,
                // Resolved by the controller at claim time, not here: the node
                // assigned to this model may report a different inventory by
                // the time a queued job actually runs.
                serving_identity: None,
            })?,
        )
    }

    pub fn runner_source(&self, id: &str) -> Result<Option<crate::source::JobSource>> {
        let ciphertext: Option<String> = self
            .connection()?
            .query_row(
                "SELECT source_ciphertext FROM jobs WHERE id=?1 AND status='running'
             AND cancel_requested=0 AND runner_lease_expires_at > ?2",
                rusqlite::params![id, super::now()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        ciphertext
            .map(|value| {
                Ok(serde_json::from_str(
                    &self.secrets.open(&job_source(id), &value)?,
                )?)
            })
            .transpose()
    }

    pub fn runner_package(&self, id: &str) -> Result<Option<RunnerPackage>> {
        let Some(job) = self.job(id)? else {
            return Ok(None);
        };
        let ciphertext: Option<String> = self
            .connection()?
            .query_row(
                "SELECT contract_ciphertext FROM jobs WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(ciphertext) = ciphertext else {
            return Ok(None);
        };
        let mut package: RunnerPackage =
            serde_json::from_str(&self.secrets.open(&job_contract(id), &ciphertext)?)?;
        package.job = job;
        Ok(Some(package))
    }
}
