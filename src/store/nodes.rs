use super::{Store, json_or_none, now};
use crate::crypto::{random_token, token_hash, token_matches};
use crate::domain::*;
use anyhow::{Result, bail};
use chrono::{Duration, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) const LIVE_ENROLLMENT_CONFLICT: &str = "that machine name is already enrolled and reachable; wait until it is offline or remove it first";

fn live_enrollment(status: &str, last_seen_at: Option<&str>, offline_seconds: i64) -> bool {
    if status == "maintenance" {
        return true;
    }
    if status == "revoked" {
        return false;
    }
    let threshold = Utc::now() - Duration::seconds(offline_seconds);
    last_seen_at
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|seen| seen > threshold)
}

impl Store {
    /// A single-use enrollment token. One created for a machine name enrolls
    /// only that name; without a name it can enroll a new machine but not take
    /// over an existing one (L8).
    pub fn create_enrollment_token(
        &self,
        valid_minutes: i64,
        node_name: Option<&str>,
    ) -> Result<String> {
        let token = random_token();
        self.connection()?.execute(
            "INSERT INTO enrollment_tokens (token_hash,expires_at,node_name) VALUES (?1,?2,?3)",
            params![
                token_hash(&token),
                (Utc::now() + Duration::minutes(valid_minutes)).to_rfc3339(),
                node_name
            ],
        )?;
        Ok(token)
    }

    pub fn enroll_node(&self, input: &EnrollNode) -> Result<EnrolledNode> {
        self.enroll_node_with_offline(input, 15)
    }

    pub fn enroll_node_with_offline(
        &self,
        input: &EnrollNode,
        offline_seconds: i64,
    ) -> Result<EnrolledNode> {
        let hash = token_hash(&input.token);
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        let token: Option<(String, Option<String>)> = transaction.query_row(
            "SELECT expires_at,node_name FROM enrollment_tokens WHERE token_hash=?1 AND consumed_at IS NULL",
            [&hash], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((expires_at, bound_name)) = token else {
            bail!("enrollment token is invalid, expired, or already used");
        };
        let valid = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .is_ok_and(|expiry| expiry > Utc::now());
        if !valid {
            bail!("enrollment token is invalid, expired, or already used");
        }
        if bound_name
            .as_deref()
            .is_some_and(|bound| bound != input.name)
        {
            bail!("enrollment token was created for another machine name");
        }
        let existing = transaction
            .query_row(
                "SELECT id,status,last_seen_at FROM nodes WHERE name=?1",
                [&input.name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((_, status, last_seen_at)) = &existing
            && live_enrollment(status, last_seen_at.as_deref(), offline_seconds)
        {
            bail!(LIVE_ENROLLMENT_CONFLICT);
        }
        if existing.is_some() && bound_name.is_none() {
            bail!("re-enrolling an existing machine needs a token created for its name");
        }
        let existing_id = existing.map(|(id, _, _)| id);
        let node_id = existing_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let credential = random_token();
        let seen_at = now();
        if existing_id.is_some() {
            transaction.execute(
                "UPDATE node_actions SET status='failed',result_json=?1,updated_at=?2
                 WHERE node_id=?3 AND status IN ('pending','running')",
                params![
                    json!({"error":"node_reenrolled"}).to_string(),
                    seen_at,
                    node_id
                ],
            )?;
            transaction.execute(
                "UPDATE nodes SET credential_hash=?1,platform=?2,capabilities_json=CASE
                 WHEN json_type(?3,'$.inventory') IS NULL AND json_type(capabilities_json,'$.inventory')='object'
                 THEN json_set(?3,'$.inventory',json_extract(capabilities_json,'$.inventory')) ELSE ?3 END,
                 status='online',last_seen_at=?4,credential_issued_at=?4 WHERE id=?5",
                params![
                    token_hash(&credential),
                    input.platform,
                    input.capabilities.to_string(),
                    seen_at,
                    node_id
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO nodes
                (id,name,credential_hash,platform,capabilities_json,status,last_seen_at,created_at,credential_issued_at)
                VALUES (?1,?2,?3,?4,?5,'online',?6,?6,?6)",
                params![
                    node_id,
                    input.name,
                    token_hash(&credential),
                    input.platform,
                    input.capabilities.to_string(),
                    seen_at
                ],
            )?;
        }
        transaction.execute(
            "UPDATE enrollment_tokens SET consumed_at=?1 WHERE token_hash=?2",
            params![now(), hash],
        )?;
        transaction.commit()?;
        if existing_id.is_some() {
            self.announce_node_change(&node_id);
        }
        Ok(EnrolledNode {
            node_id,
            credential,
        })
    }

    pub fn authenticate_node(&self, id: &str, credential: &str) -> Result<bool> {
        let hash: Option<String> = self
            .connection()?
            .query_row(
                "SELECT credential_hash FROM nodes WHERE id=?1 AND status!='revoked'",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(hash.is_some_and(|expected| token_matches(&expected, credential)))
    }

    pub fn node_active(&self, id: &str) -> Result<bool> {
        node_active(&*self.connection()?, id)
    }

    pub fn node_maintenance(&self, id: &str, credential: &str, paused: bool) -> Result<()> {
        self.set_node_maintenance(id, Some(credential), paused)
    }

    pub fn set_node_maintenance(
        &self,
        id: &str,
        credential: Option<&str>,
        paused: bool,
    ) -> Result<()> {
        let connection = self.connection()?;
        let tx = connection.unchecked_transaction()?;
        let expected: String = tx.query_row(
            "SELECT credential_hash FROM nodes WHERE id=?1 AND status!='revoked'",
            [id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            credential.is_none_or(|value| token_matches(&expected, value)),
            "Node credential is invalid"
        );
        if paused {
            anyhow::ensure!(
                !node_active(&tx, id)?,
                "Node has active work; finish or cancel it before maintenance"
            );
        }
        tx.execute(
            "UPDATE nodes SET status=?1 WHERE id=?2",
            params![if paused { "maintenance" } else { "offline" }, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn revoke_node(&self, id: &str, credential: &str) -> Result<()> {
        anyhow::ensure!(
            self.remove_node_inner(id, Some(credential))?,
            "Node not found"
        );
        Ok(())
    }

    pub fn remove_node(&self, id: &str) -> Result<bool> {
        self.remove_node_inner(id, None)
    }

    fn remove_node_inner(&self, id: &str, credential: Option<&str>) -> Result<bool> {
        let connection = self.connection()?;
        let tx = connection.unchecked_transaction()?;
        let expected: Option<String> = tx
            .query_row(
                "SELECT credential_hash FROM nodes WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(expected) = expected else {
            return Ok(false);
        };
        if let Some(credential) = credential {
            anyhow::ensure!(
                token_matches(&expected, credential),
                "Node credential is invalid"
            );
        }
        anyhow::ensure!(
            !node_active(&tx, id)?,
            "Machine is still reserved for work; finish or cancel its work, or wait for the last reservation to expire"
        );
        tx.execute(
            "UPDATE nodes SET credential_hash='',status='revoked',last_seen_at=NULL WHERE id=?1",
            [id],
        )?;
        tx.execute("UPDATE node_actions SET status='failed',result_json=?1,updated_at=?2 WHERE node_id=?3 AND status IN ('pending','running')",
            params![json!({"error":"node_revoked"}).to_string(),now(),id])?;
        tx.commit()?;
        self.announce_node_change(id);
        Ok(true)
    }

    pub fn heartbeat_node(&self, id: &str, platform: &str, capabilities: &Value) -> Result<()> {
        self.connection()?.execute(
            "UPDATE nodes SET status=CASE WHEN status='maintenance' THEN status ELSE 'online' END,last_seen_at=?1,
            platform=?2,capabilities_json=CASE
            WHEN json_type(?3,'$.inventory') IS NULL AND json_type(capabilities_json,'$.inventory')='object'
            THEN json_set(?3,'$.inventory',json_extract(capabilities_json,'$.inventory')) ELSE ?3 END
            WHERE id=?4 AND status!='revoked'",
            params![now(), platform, capabilities.to_string(), id],
        )?;
        Ok(())
    }

    pub fn list_nodes(&self, offline_seconds: i64) -> Result<Vec<Node>> {
        self.query_nodes("ORDER BY name", [], offline_seconds)
    }

    /// One node, without loading and parsing every other node's inventory.
    pub fn node(&self, id: &str, offline_seconds: i64) -> Result<Option<Node>> {
        Ok(self
            .query_nodes("WHERE id=?1", [id], offline_seconds)?
            .into_iter()
            .next())
    }

    fn query_nodes(
        &self,
        clause: &str,
        params: impl rusqlite::Params,
        offline_seconds: i64,
    ) -> Result<Vec<Node>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(&format!(
            "SELECT id,name,platform,capabilities_json,status,
            last_seen_at,wol_mac,wol_broadcast,created_at FROM nodes {clause}"
        ))?;
        let mut nodes = statement
            .query_map(params, node_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let threshold = Utc::now() - Duration::seconds(offline_seconds);
        for node in &mut nodes {
            if matches!(node.status.as_str(), "revoked" | "maintenance") {
                continue;
            }
            let online = node
                .last_seen_at
                .as_ref()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .is_some_and(|seen| seen > threshold);
            node.status = if online { "online" } else { "offline" }.into();
        }
        Ok(nodes)
    }

    pub fn configure_wol(
        &self,
        id: &str,
        mac: Option<&str>,
        broadcast: Option<&str>,
    ) -> Result<bool> {
        Ok(self.connection()?.execute(
            "UPDATE nodes SET wol_mac=?1,wol_broadcast=?2 WHERE id=?3",
            params![mac, broadcast, id],
        )? > 0)
    }

    pub fn ensure_action(
        &self,
        node_id: &str,
        job_id: &str,
        kind: &str,
        payload: Value,
    ) -> Result<NodeAction> {
        if let Some(action) = self.action_for_job(job_id, kind)? {
            return Ok(action);
        }
        self.enqueue_action(node_id, Some(job_id), kind, payload)
    }

    pub fn enqueue_action(
        &self,
        node_id: &str,
        job_id: Option<&str>,
        kind: &str,
        payload: Value,
    ) -> Result<NodeAction> {
        let mut action = NodeAction {
            id: Uuid::new_v4().to_string(),
            node_id: node_id.into(),
            job_id: job_id.map(str::to_owned),
            kind: kind.into(),
            payload,
            status: "pending".into(),
            result: None,
        };
        let connection = self.connection()?;
        let _relaxed = RelaxedSync::new(&connection, self.relaxed_sync_allowed);
        let transaction = connection.unchecked_transaction()?;
        let revoked: bool = transaction.query_row(
            "SELECT status='revoked' OR (status='maintenance' AND ?2 IN ('ensure','renew')) FROM nodes WHERE id=?1",
            params![node_id,kind],
            |row| row.get(0),
        )?;
        if revoked {
            action.status = "failed".into();
            action.result = Some(json!({"error":"node_unavailable"}));
        }
        if let Some(lease) = action.payload.get("lease_id").and_then(Value::as_str) {
            let ended = json!({"error":"runtime_lease_ended"});
            if matches!(kind, "ensure" | "renew") {
                let released: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM node_actions WHERE node_id=?1
                     AND json_extract(payload_json,'$.lease_id')=?2
                     AND kind IN ('release','stop'))",
                    params![node_id, lease],
                    |row| row.get(0),
                )?;
                if released {
                    action.status = "failed".into();
                    action.result = Some(ended);
                }
            }
        }
        transaction.execute(
            "INSERT INTO node_actions
            (id,node_id,job_id,kind,payload_json,status,result_json,created_at,updated_at)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
            params![
                action.id,
                action.node_id,
                action.job_id,
                action.kind,
                action.payload.to_string(),
                action.status,
                action.result.as_ref().map(Value::to_string),
                now()
            ],
        )?;
        let fenced = if matches!(kind, "release" | "stop") {
            fence_released_actions(&transaction, node_id)?
        } else {
            0
        };
        transaction.commit()?;
        let signal = self.node_signal(node_id);
        if action.status == "pending" {
            signal.queued.notify_waiters();
        }
        if action.status != "pending" || fenced > 0 {
            signal.settled.notify_waiters();
        }
        Ok(action)
    }

    pub fn action_for_job(&self, job_id: &str, kind: &str) -> Result<Option<NodeAction>> {
        Ok(self.connection()?.query_row("SELECT id,node_id,job_id,kind,payload_json,status,
            result_json FROM node_actions WHERE job_id=?1 AND kind=?2 ORDER BY created_at DESC LIMIT 1",
            params![job_id, kind], action_row).optional()?)
    }

    pub fn list_node_actions(&self, node_id: &str) -> Result<Vec<NodeAction>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,node_id,job_id,kind,payload_json,status,
             result_json FROM node_actions WHERE node_id=?1 ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([node_id], action_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn action(&self, id: &str) -> Result<Option<NodeAction>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT id,node_id,job_id,kind,payload_json,status,
            result_json FROM node_actions WHERE id=?1",
                [id],
                action_row,
            )
            .optional()?)
    }

    pub fn poll_node_action(&self, node_id: &str) -> Result<Option<NodeAction>> {
        let connection = self.connection()?;
        let _relaxed = RelaxedSync::new(&connection, self.relaxed_sync_allowed);
        let transaction = connection.unchecked_transaction()?;
        let stale_before = (Utc::now() - Duration::minutes(11)).to_rfc3339();
        transaction.execute(
            "UPDATE node_actions SET status='pending', updated_at=?1
            WHERE node_id=?2 AND status='running' AND updated_at < ?3",
            params![now(), node_id, stale_before],
        )?;
        // Also fences actions queued by older controllers and stale action retries.
        let fenced = fence_released_actions(&transaction, node_id)?;
        let action = transaction
            .query_row(
                "SELECT id,node_id,job_id,kind,payload_json,status,
            result_json FROM node_actions WHERE node_id=?1 AND status='pending'
            ORDER BY created_at LIMIT 1",
                [node_id],
                action_row,
            )
            .optional()?;
        if let Some(action) = &action {
            transaction.execute(
                "UPDATE node_actions SET status='running',
            updated_at=?1 WHERE id=?2",
                params![now(), action.id],
            )?;
        }
        transaction.commit()?;
        if fenced > 0 {
            self.node_signal(node_id).settled.notify_waiters();
        }
        Ok(action.map(|mut action| {
            action.status = "running".into();
            action
        }))
    }

    pub fn complete_node_action(
        &self,
        node_id: &str,
        action_id: &str,
        success: bool,
        result: Value,
    ) -> Result<bool> {
        let status = if success { "completed" } else { "failed" };
        let connection = self.connection()?;
        let relaxed = RelaxedSync::new(&connection, self.relaxed_sync_allowed);
        let changed = connection.execute(
            "UPDATE node_actions SET status=?1,result_json=?2,updated_at=?3
            WHERE id=?4 AND node_id=?5 AND status='running'",
            params![status, result.to_string(), now(), action_id, node_id],
        )? > 0;
        drop(relaxed);
        drop(connection);
        if changed {
            self.node_signal(node_id).settled.notify_waiters();
        }
        Ok(changed)
    }

    /// Wake everything waiting on the node after its queue or credentials
    /// changed outside the ordinary enqueue and complete paths.
    fn announce_node_change(&self, node_id: &str) {
        let signal = self.node_signal(node_id);
        signal.queued.notify_waiters();
        signal.settled.notify_waiters();
    }
}

/// Lets the node-action writes (enqueue, claim, complete) commit without
/// waiting for a disk flush, until it drops. Every other write keeps FULL.
///
/// Each model request commits six of these, and the flushes dominated its
/// latency. On power loss or an OS crash, the last few seconds of these rows
/// can be lost: any later FULL commit, such as the node's heartbeat, syncs the
/// ones before it. A lost row means an idempotent action runs again or a lease
/// is held until it expires. A process crash loses nothing.
struct RelaxedSync<'a> {
    connection: &'a rusqlite::Connection,
    active: bool,
}

impl<'a> RelaxedSync<'a> {
    fn new(connection: &'a rusqlite::Connection, allowed: bool) -> Self {
        let active = allowed
            && connection
                .pragma_update(None, "synchronous", "NORMAL")
                .is_ok();
        #[cfg(test)]
        if active {
            tests::RELAXED_WRITES.with(|count| count.set(count.get() + 1));
        }
        Self { connection, active }
    }
}

impl Drop for RelaxedSync<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.connection.pragma_update(None, "synchronous", "FULL")
        {
            // Later writes would silently stay relaxed.
            tracing::error!(%error, "could not restore full database sync");
        }
    }
}

/// Fails pending ensure and renew actions of leases that already ended, and
/// returns how many it failed.
fn fence_released_actions(connection: &rusqlite::Connection, node_id: &str) -> Result<usize> {
    Ok(connection.execute(
        "UPDATE node_actions AS pending SET status='failed',result_json=?1,updated_at=?2
         WHERE node_id=?3 AND status='pending' AND kind IN ('ensure','renew')
         AND EXISTS(SELECT 1 FROM node_actions AS terminal
           WHERE terminal.node_id=pending.node_id
           AND json_extract(terminal.payload_json,'$.lease_id')=json_extract(pending.payload_json,'$.lease_id')
           AND terminal.kind IN ('release','stop'))",
        params![json!({"error":"runtime_lease_ended"}).to_string(), now(), node_id],
    )?)
}

fn node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let capabilities: String = row.get(3)?;
    Ok(Node {
        id: row.get(0)?,
        name: row.get(1)?,
        platform: row.get(2)?,
        capabilities: serde_json::from_str(&capabilities).unwrap_or(json!({})),
        status: row.get(4)?,
        last_seen_at: row.get(5)?,
        wol_mac: row.get(6)?,
        wol_broadcast: row.get(7)?,
        created_at: row.get(8)?,
    })
}

fn action_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeAction> {
    let payload: String = row.get(4)?;
    Ok(NodeAction {
        id: row.get(0)?,
        node_id: row.get(1)?,
        job_id: row.get(2)?,
        kind: row.get(3)?,
        payload: serde_json::from_str(&payload).unwrap_or(json!({})),
        status: row.get(5)?,
        result: json_or_none(row.get(6)?),
    })
}

fn node_active(connection: &rusqlite::Connection, id: &str) -> Result<bool> {
    // A missing release must not hold an abandoned lease forever. Allow the
    // longest supported inhibitor timeout, one in-flight action and a heartbeat.
    // Live jobs and recent renewal activity still hold the registration.
    let stale_before = (Utc::now()
        - Duration::seconds(
            (crate::node::MAX_LEASE_TIMEOUT_SECONDS
                + crate::node::RUNTIME_ACTION_TIMEOUT_SECONDS
                + crate::node::MAX_HEARTBEAT_SECONDS) as i64,
        ))
    .to_rfc3339();
    Ok(connection.query_row(
        concat!("SELECT EXISTS(SELECT 1 FROM node_actions a
        WHERE a.node_id=?1 AND a.created_at >= (SELECT COALESCE(credential_issued_at,created_at) FROM nodes WHERE id=?1)
        AND a.kind='ensure' AND a.status IN ('pending','running','completed')
        AND (a.updated_at >= ?2
            OR EXISTS(SELECT 1 FROM jobs j WHERE j.status NOT IN ", crate::terminal_job_statuses!(), "
                AND (j.id=a.job_id OR j.attempt_id=json_extract(a.payload_json,'$.lease_id')))
            OR EXISTS(SELECT 1 FROM node_actions renewal WHERE renewal.node_id=a.node_id
                AND renewal.kind='renew' AND renewal.status IN ('pending','running','completed')
                AND renewal.updated_at >= ?2
                AND json_extract(renewal.payload_json,'$.lease_id')=json_extract(a.payload_json,'$.lease_id')))
        AND NOT EXISTS(SELECT 1 FROM node_actions r WHERE r.node_id=a.node_id
            AND json_extract(r.payload_json,'$.lease_id')=json_extract(a.payload_json,'$.lease_id')
            AND r.kind IN ('release','stop') AND r.status='completed'))"),
        params![id,stale_before],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
#[path = "nodes_tests.rs"]
mod tests;
