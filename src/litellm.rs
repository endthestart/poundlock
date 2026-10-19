use crate::crypto::sha256_hex;
use crate::domain::{
    ApiTokenPrincipal, Endpoint, EndpointSecret, ManagedIdentityPolicy, ManagedLiteLlmKey,
    ModelProfile,
};
use crate::store::{Store, now};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const MANAGEMENT_TIMEOUT_SECONDS: u64 = 15;
const MAX_MANAGEMENT_JSON_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct ManagedIdentityManager {
    store: Store,
    client: Client,
    mutation_lock: Arc<Mutex<()>>,
}

impl ManagedIdentityManager {
    pub fn new(store: Store) -> Result<Self> {
        Ok(Self {
            store,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(MANAGEMENT_TIMEOUT_SECONDS))
                .build()?,
            mutation_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn access_for(
        &self,
        principal: &ApiTokenPrincipal,
        profile: &ModelProfile,
        endpoint: &Endpoint,
    ) -> Result<Value> {
        self.access_for_permission(principal, profile, endpoint, "inference:invoke")
    }

    pub fn access_for_job(
        &self,
        principal: &ApiTokenPrincipal,
        profile: &ModelProfile,
        endpoint: &Endpoint,
    ) -> Result<Value> {
        self.access_for_permission(principal, profile, endpoint, "jobs:submit")
    }

    fn access_for_permission(
        &self,
        principal: &ApiTokenPrincipal,
        profile: &ModelProfile,
        endpoint: &Endpoint,
        required_permission: &str,
    ) -> Result<Value> {
        let connection_mode = connection_mode(endpoint);
        let warning_reason =
            (profile.data_destination == "unknown").then_some("data_destination_unknown");
        let mut status = "available";
        let mut reason = None;
        let mut managed_status = None::<String>;

        if profile.retirement.disabled.is_some() {
            // Taken out of service on authoritative evidence that its upstream
            // model is gone. Still listed, so a client can say why rather than
            // find the name missing.
            status = "blocked";
            reason = Some("model_disabled");
        } else if !principal.has_permission(required_permission) {
            status = "blocked";
            reason = Some(if required_permission == "jobs:submit" {
                "job_submission_permission_required"
            } else {
                "inference_permission_required"
            });
        } else if let Some(destination_reason) = destination_block_reason(principal, profile) {
            status = "blocked";
            reason = Some(destination_reason);
        } else if endpoint.kind == "litellm" && endpoint.managed_identity.enabled {
            if profile.data_destination == "external"
                && (endpoint.managed_identity.max_budget.is_none()
                    || endpoint.managed_identity.budget_duration.is_none())
            {
                status = "blocked";
                reason = Some("managed_budget_required");
            } else {
                let mapping = self
                    .store
                    .managed_litellm_key(&principal.id, &endpoint.id)?;
                managed_status = mapping.as_ref().map(|item| item.status.clone());
                match mapping.as_ref().map(|item| item.status.as_str()) {
                    Some("active") => {}
                    None => {
                        status = "preparation_required";
                        reason = Some("managed_identity_not_provisioned");
                    }
                    Some("provision_failed") => {
                        status = "blocked";
                        reason = Some("managed_identity_provision_failed");
                    }
                    Some(_) => {
                        status = "blocked";
                        reason = Some("managed_identity_reconciliation_required");
                    }
                }
            }
        }

        Ok(json!({
            "availability": status,
            "availability_reason": reason,
            "blocked_reason": if status == "blocked" { reason } else { None },
            "warning_reason": warning_reason,
            "connection_mode": connection_mode,
            "managed_identity_status": managed_status,
        }))
    }

    pub async fn prepare_access(
        &self,
        principal: &ApiTokenPrincipal,
        profile: &ModelProfile,
        endpoint: &EndpointSecret,
        required_permission: &str,
    ) -> Result<Value> {
        let before = self.access_for_permission(
            principal,
            profile,
            &endpoint.endpoint,
            required_permission,
        )?;
        if before["availability"] == "blocked"
            && before["blocked_reason"] != "managed_identity_provision_failed"
        {
            return Ok(before);
        }
        if endpoint.endpoint.kind == "litellm" && endpoint.endpoint.managed_identity.enabled {
            self.credential_for(principal, endpoint, profile, required_permission)
                .await?;
        }
        self.access_for_permission(principal, profile, &endpoint.endpoint, required_permission)
    }

    pub async fn validate_management_connection(
        &self,
        base_url: &str,
        api_key: &str,
    ) -> Result<()> {
        let response = self
            .client
            .get(management_url(base_url, "key/list")?)
            .bearer_auth(api_key)
            .query(&[("page", "1"), ("size", "1")])
            .send()
            .await
            .context("connect to LiteLLM key management")?;
        checked_json(response).await?;
        Ok(())
    }

    pub async fn credential_for(
        &self,
        principal: &ApiTokenPrincipal,
        endpoint: &EndpointSecret,
        profile: &ModelProfile,
        required_permission: &str,
    ) -> Result<Option<String>> {
        if endpoint.endpoint.kind != "litellm" || !endpoint.endpoint.managed_identity.enabled {
            return Ok(None);
        }
        ensure!(
            profile.endpoint_id == endpoint.endpoint.id,
            "model profile does not belong to this LiteLLM endpoint"
        );
        let access = self.access_for_permission(
            principal,
            profile,
            &endpoint.endpoint,
            required_permission,
        )?;
        ensure!(
            access["availability"] != "blocked"
                || access["blocked_reason"] == "managed_identity_provision_failed",
            "{}",
            access["blocked_reason"]
                .as_str()
                .unwrap_or("managed_identity_unavailable")
        );
        validate_policy(&endpoint.endpoint.managed_identity)?;
        let _guard = self.mutation_lock.lock().await;
        let endpoint = self
            .store
            .endpoint_secret(&endpoint.endpoint.id)?
            .context("managed LiteLLM endpoint is unavailable")?;
        ensure!(
            endpoint.endpoint.kind == "litellm" && endpoint.endpoint.managed_identity.enabled,
            "managed LiteLLM identity is no longer enabled"
        );
        let models = self.allowed_model_ids(principal, &endpoint.endpoint)?;
        ensure!(
            !models.is_empty(),
            "managed LiteLLM endpoint has no published profiles"
        );
        let policy_hash = policy_hash(&endpoint.endpoint.managed_identity, &models)?;
        if let Some(mut existing) = self
            .store
            .managed_litellm_key(&principal.id, &endpoint.endpoint.id)?
        {
            if existing.status == "active" && existing.policy_hash != policy_hash {
                if let Err(error) = self
                    .update_remote_key(&endpoint, &existing.virtual_key, &models)
                    .await
                {
                    self.store.record_managed_litellm_key_error(
                        &principal.id,
                        &endpoint.endpoint.id,
                        "active",
                        "LiteLLM key policy update failed; retry is required",
                    )?;
                    return Err(error);
                }
                existing.policy_hash = policy_hash.clone();
                existing.updated_at = now();
                existing.last_error = None;
                self.store.save_managed_litellm_key(&existing)?;
            }
            if existing.status == "active" {
                return Ok(Some(existing.virtual_key));
            }
            ensure!(
                existing.status == "provision_failed",
                "managed LiteLLM identity is pending operator reconciliation"
            );
        }

        let value = match self
            .post(
                &endpoint,
                "key/generate",
                generate_body(principal, &endpoint, &models),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                self.record_provision_failure(principal, &endpoint, &policy_hash)?;
                return Err(error);
            }
        };
        let Some(virtual_key) = value
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
        else {
            self.record_provision_failure(principal, &endpoint, &policy_hash)?;
            bail!("LiteLLM key management did not return a virtual key")
        };
        let remote_key_id = value
            .get("key_name")
            .or_else(|| value.get("token_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let timestamp = now();
        let mapping = ManagedLiteLlmKey {
            api_token_id: principal.id.clone(),
            endpoint_id: endpoint.endpoint.id.clone(),
            virtual_key_hash: sha256_hex(virtual_key.as_bytes()),
            virtual_key,
            remote_key_id,
            policy_hash,
            status: "active".into(),
            last_error: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        if let Err(error) = self.store.save_managed_litellm_key(&mapping) {
            let _ = self.block_remote_key(&endpoint, &mapping.virtual_key).await;
            return Err(error).context("store managed LiteLLM identity");
        }
        Ok(Some(mapping.virtual_key))
    }

    fn allowed_model_ids(
        &self,
        principal: &ApiTokenPrincipal,
        endpoint: &Endpoint,
    ) -> Result<Vec<String>> {
        let allow_external = principal.has_permission("external:invoke")
            && endpoint.managed_identity.max_budget.is_some()
            && endpoint.managed_identity.budget_duration.is_some();
        let mut models = self
            .store
            .list_model_profiles()?
            .into_iter()
            .filter(|profile| profile.endpoint_id == endpoint.id)
            .filter(|profile| destination_block_reason(principal, profile).is_none())
            .filter(|profile| profile.data_destination != "external" || allow_external)
            .map(|profile| profile.model_id)
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        Ok(models)
    }

    fn record_provision_failure(
        &self,
        principal: &ApiTokenPrincipal,
        endpoint: &EndpointSecret,
        policy_hash: &str,
    ) -> Result<()> {
        let timestamp = now();
        self.store.save_managed_litellm_key(&ManagedLiteLlmKey {
            api_token_id: principal.id.clone(),
            endpoint_id: endpoint.endpoint.id.clone(),
            virtual_key: String::new(),
            virtual_key_hash: sha256_hex(b""),
            remote_key_id: None,
            policy_hash: policy_hash.into(),
            status: "provision_failed".into(),
            last_error: Some("LiteLLM key provisioning failed; the next request may retry".into()),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
    }

    pub async fn reconcile_pending(&self) -> Result<usize> {
        let _guard = self.mutation_lock.lock().await;
        let mappings = self.store.pending_managed_litellm_revocations()?;
        let mut completed = 0;
        for mapping in mappings {
            if mapping.virtual_key.is_empty() {
                self.store.record_managed_litellm_key_error(
                    &mapping.api_token_id,
                    &mapping.endpoint_id,
                    "revoked",
                    "",
                )?;
                completed += 1;
                continue;
            }
            let Some(endpoint) = self.store.endpoint_secret(&mapping.endpoint_id)? else {
                self.store.record_managed_litellm_key_error(
                    &mapping.api_token_id,
                    &mapping.endpoint_id,
                    "pending_revoke",
                    "endpoint is unavailable for LiteLLM key revocation",
                )?;
                continue;
            };
            match self.block_remote_key(&endpoint, &mapping.virtual_key).await {
                Ok(()) => {
                    self.store.record_managed_litellm_key_error(
                        &mapping.api_token_id,
                        &mapping.endpoint_id,
                        "revoked",
                        "",
                    )?;
                    completed += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        endpoint_id = %mapping.endpoint_id,
                        api_token_id = %mapping.api_token_id,
                        %error,
                        "managed LiteLLM revocation requires reconciliation"
                    );
                    self.store.record_managed_litellm_key_error(
                        &mapping.api_token_id,
                        &mapping.endpoint_id,
                        "pending_revoke",
                        "LiteLLM key revocation is pending",
                    )?;
                }
            }
        }
        Ok(completed)
    }

    pub async fn rotate_endpoint(&self, endpoint_id: &str) -> Result<usize> {
        let _guard = self.mutation_lock.lock().await;
        let endpoint = self
            .store
            .endpoint_secret(endpoint_id)?
            .context("LiteLLM endpoint was not found")?;
        ensure!(
            endpoint.endpoint.kind == "litellm" && endpoint.endpoint.managed_identity.enabled,
            "managed LiteLLM identity is not enabled"
        );
        let mappings = self.store.managed_litellm_keys_for_endpoint(endpoint_id)?;
        let mut rotated = 0;
        for mut mapping in mappings.into_iter().filter(|item| item.status == "active") {
            let Some(principal) = self.store.api_token_principal(&mapping.api_token_id)? else {
                continue;
            };
            let models = self.allowed_model_ids(&principal, &endpoint.endpoint)?;
            ensure!(
                !models.is_empty(),
                "managed LiteLLM application has no authorized published profiles"
            );
            let policy_hash = policy_hash(&endpoint.endpoint.managed_identity, &models)?;
            let mut body = generate_body(&principal, &endpoint, &models);
            body["key_alias"] = json!(format!(
                "poundlock-{}-{}-rotation-{}",
                short_id(&principal.id),
                short_id(&endpoint.endpoint.id),
                short_id(&uuid::Uuid::new_v4().to_string())
            ));
            let value = self.post(&endpoint, "key/generate", body).await?;
            let new_key = value
                .get("key")
                .and_then(Value::as_str)
                .context("LiteLLM key rotation did not return a virtual key")?
                .to_owned();
            self.block_remote_key(&endpoint, &mapping.virtual_key)
                .await?;
            mapping.virtual_key = new_key;
            mapping.virtual_key_hash = sha256_hex(mapping.virtual_key.as_bytes());
            mapping.remote_key_id = value
                .get("key_name")
                .or_else(|| value.get("token_id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            mapping.policy_hash = policy_hash.clone();
            mapping.updated_at = now();
            mapping.last_error = None;
            if let Err(error) = self.store.save_managed_litellm_key(&mapping) {
                let _ = self.block_remote_key(&endpoint, &mapping.virtual_key).await;
                return Err(error).context("store rotated LiteLLM identity");
            }
            rotated += 1;
        }
        Ok(rotated)
    }

    pub async fn revoke_endpoint_keys(&self, endpoint: &EndpointSecret) -> Result<usize> {
        let _guard = self.mutation_lock.lock().await;
        let mappings = self
            .store
            .managed_litellm_keys_for_endpoint(&endpoint.endpoint.id)?;
        let mut revoked = 0;
        for mapping in mappings.into_iter().filter(|item| item.status == "active") {
            self.block_remote_key(endpoint, &mapping.virtual_key)
                .await?;
            self.store.record_managed_litellm_key_error(
                &mapping.api_token_id,
                &mapping.endpoint_id,
                "revoked",
                "",
            )?;
            revoked += 1;
        }
        Ok(revoked)
    }

    pub async fn usage_for(&self, principal: &ApiTokenPrincipal) -> Result<Value> {
        let mut rows = Vec::new();
        for endpoint in self.store.list_endpoints()? {
            if endpoint.kind != "litellm" || !endpoint.managed_identity.enabled {
                continue;
            }
            let Some(mapping) = self
                .store
                .managed_litellm_key(&principal.id, &endpoint.id)?
            else {
                rows.push(json!({
                    "endpoint_id": endpoint.id,
                    "status": "not_provisioned",
                    "connection_mode": connection_mode(&endpoint),
                    "external_authorized": principal.has_permission("external:invoke"),
                    "usage_status": "not_provisioned",
                    "budget": configured_budget(&endpoint.managed_identity),
                }));
                continue;
            };
            let mut row = json!({
                "endpoint_id": endpoint.id,
                "status": mapping.status,
                "connection_mode": connection_mode(&endpoint),
                "external_authorized": principal.has_permission("external:invoke"),
                "budget": configured_budget(&endpoint.managed_identity),
            });
            if mapping.status == "active"
                && let Some(secret) = self.store.endpoint_secret(&endpoint.id)?
            {
                match self.key_info(&secret, &mapping.virtual_key_hash).await {
                    Ok(info) => merge_safe_usage(&mut row, &info),
                    Err(error) => {
                        tracing::warn!(endpoint_id=%endpoint.id, api_token_id=%principal.id, %error, "LiteLLM usage lookup failed");
                        row["usage_status"] = json!("unavailable");
                    }
                }
            }
            rows.push(row);
        }
        Ok(json!({"managed_identities": rows}))
    }

    async fn update_remote_key(
        &self,
        endpoint: &EndpointSecret,
        virtual_key: &str,
        models: &[String],
    ) -> Result<()> {
        let mut body = policy_fields(&endpoint.endpoint.managed_identity);
        body.insert("key".into(), json!(virtual_key));
        body.insert("models".into(), json!(models));
        self.post(endpoint, "key/update", Value::Object(body))
            .await?;
        Ok(())
    }

    async fn block_remote_key(&self, endpoint: &EndpointSecret, virtual_key: &str) -> Result<()> {
        self.post(endpoint, "key/block", json!({"key": virtual_key}))
            .await?;
        Ok(())
    }

    async fn key_info(&self, endpoint: &EndpointSecret, virtual_key_hash: &str) -> Result<Value> {
        let url = management_url(&endpoint.endpoint.base_url, "key/info")?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&endpoint.api_key)
            .query(&[("key", virtual_key_hash)])
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("connect to LiteLLM key management"))?;
        checked_json(response).await
    }

    /// LiteLLM's own description of its routes: model group name, upstream
    /// model, prices and model info. Read-only, with the endpoint's stored
    /// management credential; keys come back masked and are never forwarded.
    pub async fn route_info(&self, endpoint: &EndpointSecret) -> Result<Vec<Value>> {
        let response = self
            .client
            .get(management_url(&endpoint.endpoint.base_url, "model/info")?)
            .bearer_auth(&endpoint.api_key)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .context("connect to LiteLLM model info")?;
        let body = checked_json(response).await?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Ask LiteLLM to health-check one of its own routes. LiteLLM makes the
    /// probe request with its own provider credential, so Poundlock never
    /// sends inference with the administrative key.
    pub async fn route_health(&self, endpoint: &EndpointSecret, route: &str) -> Result<Value> {
        let response = self
            .client
            .get(management_url(&endpoint.endpoint.base_url, "health")?)
            .query(&[("model", route)])
            .bearer_auth(&endpoint.api_key)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .context("connect to LiteLLM health check")?;
        // An unhealthy route is answered with 503 and the same report, so the
        // status alone does not mean the check failed to run.
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            let body: Value = response
                .json()
                .await
                .context("decode LiteLLM health report")?;
            ensure!(
                body.get("unhealthy_endpoints").is_some(),
                "LiteLLM health check returned HTTP 503"
            );
            return Ok(body);
        }
        checked_json(response).await
    }

    async fn post(&self, endpoint: &EndpointSecret, path: &str, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(management_url(&endpoint.endpoint.base_url, path)?)
            .bearer_auth(&endpoint.api_key)
            .header("litellm-changed-by", "poundlock")
            .json(&body)
            .send()
            .await
            .context("connect to LiteLLM key management")?;
        checked_json(response).await
    }
}

pub(crate) fn destination_block_reason(
    principal: &ApiTokenPrincipal,
    profile: &ModelProfile,
) -> Option<&'static str> {
    match profile.data_destination.as_str() {
        "operator_network" => None,
        "external" if principal.has_permission("external:invoke") => None,
        "external" => Some("external_permission_required"),
        _ => Some("data_destination_confirmation_required"),
    }
}

fn generate_body(
    principal: &ApiTokenPrincipal,
    endpoint: &EndpointSecret,
    models: &[String],
) -> Value {
    let mut body = policy_fields(&endpoint.endpoint.managed_identity);
    body.insert("models".into(), json!(models));
    // LiteLLM's LLM API key type prevents a generated key from reaching its
    // management routes even if gateway ownership or defaults later change.
    body.insert("key_type".into(), json!("llm_api"));
    body.insert(
        "key_alias".into(),
        json!(format!(
            "poundlock-{}-{}",
            short_id(&principal.id),
            short_id(&endpoint.endpoint.id)
        )),
    );
    body.insert(
        "user_id".into(),
        json!(format!("poundlock:application-token:{}", principal.id)),
    );
    body.insert(
        "metadata".into(),
        json!({
            "managed_by": "poundlock",
            "poundlock_application_token_id": principal.id,
            "poundlock_endpoint_id": endpoint.endpoint.id,
        }),
    );
    Value::Object(body)
}

fn policy_fields(policy: &ManagedIdentityPolicy) -> Map<String, Value> {
    let mut body = Map::new();
    if let Some(value) = policy.max_budget {
        body.insert("max_budget".into(), json!(value));
    }
    if let Some(value) = &policy.budget_duration {
        body.insert("budget_duration".into(), json!(value));
    }
    if let Some(value) = &policy.duration {
        body.insert("duration".into(), json!(value));
    }
    if let Some(value) = policy.rpm_limit {
        body.insert("rpm_limit".into(), json!(value));
    }
    if let Some(value) = policy.tpm_limit {
        body.insert("tpm_limit".into(), json!(value));
    }
    body
}

fn configured_budget(policy: &ManagedIdentityPolicy) -> Value {
    json!({
        "max_budget": policy.max_budget,
        "budget_duration": policy.budget_duration,
        "rpm_limit": policy.rpm_limit,
        "tpm_limit": policy.tpm_limit,
        "duration": policy.duration,
    })
}

fn merge_safe_usage(row: &mut Value, source: &Value) {
    let info = source.get("info").unwrap_or(source);
    let mut usage = Map::new();
    for key in [
        "spend",
        "max_budget",
        "budget_duration",
        "budget_reset_at",
        "expires",
        "rpm_limit",
        "tpm_limit",
        "blocked",
    ] {
        if let Some(value) = info.get(key) {
            usage.insert(key.into(), value.clone());
        }
    }
    row["usage_status"] = json!("available");
    if let (Some(spend), Some(maximum)) = (
        usage.get("spend").and_then(Value::as_f64),
        usage.get("max_budget").and_then(Value::as_f64),
    ) {
        usage.insert("remaining_budget".into(), json!((maximum - spend).max(0.0)));
    }
    usage.insert("source".into(), json!("litellm"));
    usage.insert("observed_at".into(), json!(now()));
    row["usage"] = Value::Object(usage);
}

fn policy_hash(policy: &ManagedIdentityPolicy, models: &[String]) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(&json!({
        "policy": policy,
        "models": models,
    }))?))
}

pub(crate) fn validate_policy(policy: &ManagedIdentityPolicy) -> Result<()> {
    if let Some(value) = policy.max_budget {
        ensure!(
            value.is_finite() && value > 0.0,
            "max_budget must be positive"
        );
    }
    ensure!(
        policy.max_budget.is_some() == policy.budget_duration.is_some(),
        "maximum spend and budget period must be configured together"
    );
    if let Some(value) = policy.budget_duration.as_deref() {
        ensure!(
            ["1d", "7d", "30d"].contains(&value),
            "budget period must be daily, weekly or monthly"
        );
    }
    for (name, value) in [("duration", policy.duration.as_deref())] {
        if let Some(value) = value {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= 32
                    && !value.chars().any(char::is_control),
                "{name} must contain 1 to 32 characters without control characters"
            );
        }
    }
    ensure!(policy.rpm_limit != Some(0), "rpm_limit must be positive");
    ensure!(policy.tpm_limit != Some(0), "tpm_limit must be positive");
    Ok(())
}

pub(crate) fn connection_mode(endpoint: &Endpoint) -> &'static str {
    if endpoint.kind != "litellm" {
        "direct"
    } else if endpoint.managed_identity.enabled {
        "managed_applications"
    } else {
        "gateway_only"
    }
}

fn short_id(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn management_url(base_url: &str, path: &str) -> Result<Url> {
    let mut url = Url::parse(base_url).context("parse LiteLLM base URL")?;
    let mut segments = url
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.last() == Some(&"v1") {
        segments.pop();
    }
    segments.push(path);
    url.set_path(&segments.join("/"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

async fn checked_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        if status == StatusCode::NOT_FOUND {
            bail!("LiteLLM key management is unavailable; verify database-backed proxy mode")
        }
        bail!("LiteLLM key management returned HTTP {status}")
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read LiteLLM key-management response")?;
        if body.len().saturating_add(chunk.len()) > MAX_MANAGEMENT_JSON_BYTES {
            bail!("LiteLLM key-management response exceeded {MAX_MANAGEMENT_JSON_BYTES} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("decode LiteLLM key-management response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretBox;
    use crate::domain::{
        CreateApiToken, CreateEndpoint, CreateModelProfile, CreateProject, SubmitJob,
    };
    use axum::extract::Query;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct FakeGateway {
        generated: Arc<Mutex<Vec<Value>>>,
        updated: Arc<Mutex<Vec<Value>>>,
        blocked: Arc<Mutex<Vec<Value>>>,
        info_keys: Arc<Mutex<Vec<String>>>,
        failures_remaining: Arc<AtomicUsize>,
    }

    async fn generated(
        State(state): State<FakeGateway>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        if state
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"sensitive upstream detail"})),
            )
                .into_response();
        }
        let mut generated = state.generated.lock().await;
        generated.push(body.clone());
        Json(json!({
            "key": format!("virtual-key-{}", generated.len()),
            "key_name": format!("hash-{}", generated.len()),
        }))
        .into_response()
    }

    async fn updated(State(state): State<FakeGateway>, Json(body): Json<Value>) -> Json<Value> {
        state.updated.lock().await.push(body);
        Json(json!({"updated": true}))
    }

    async fn blocked(State(state): State<FakeGateway>, Json(body): Json<Value>) -> Json<Value> {
        state.blocked.lock().await.push(body);
        Json(json!({"blocked": true}))
    }

    async fn key_info(
        State(state): State<FakeGateway>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .info_keys
            .lock()
            .await
            .push(query.get("key").cloned().unwrap_or_default());
        Json(json!({"info":{
            "spend": 1.5,
            "max_budget": 5.0,
            "budget_duration": "30d",
            "key": "must-not-leak",
            "token": "must-not-leak",
        }}))
    }

    async fn key_list() -> Json<Value> {
        Json(json!({"keys": []}))
    }

    async fn fake_gateway() -> (FakeGateway, String) {
        let state = FakeGateway::default();
        let app = Router::new()
            .route("/key/generate", post(generated))
            .route("/key/update", post(updated))
            .route("/key/block", post(blocked))
            .route("/key/info", get(key_info))
            .route("/key/list", get(key_list))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (state, format!("http://{address}/v1"))
    }

    fn configured_store(
        base_url: &str,
        enabled: bool,
    ) -> (tempfile::TempDir, Store, EndpointSecret) {
        let directory = tempfile::tempdir().unwrap();
        let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
        let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
        let endpoint = store
            .create_endpoint(&CreateEndpoint {
                name: "gateway".into(),
                kind: "litellm".into(),
                base_url: base_url.into(),
                api_key: "master-key".into(),
                managed_identity: ManagedIdentityPolicy {
                    enabled,
                    max_budget: Some(5.0),
                    budget_duration: Some("30d".into()),
                    duration: Some("90d".into()),
                    rpm_limit: Some(10),
                    tpm_limit: Some(20_000),
                },
            })
            .unwrap();
        for (name, model_id) in [
            ("frontier-review", "provider/review"),
            ("frontier-plan", "provider/plan"),
        ] {
            store
                .create_model_profile(&CreateModelProfile {
                    name: name.into(),
                    aliases: Vec::new(),
                    endpoint_id: endpoint.id.clone(),
                    model_id: model_id.into(),
                    model_label: None,
                    context_tokens: 16_384,
                    max_output_tokens: 2_048,
                    uses: Some(vec!["interactive".into()]),
                    workload_class: None,
                    wake_policy: "never".into(),
                    node_id: None,
                    runtime_profile: None,
                    idle_seconds: None,
                    data_destination: "external".into(),
                    thinking: "model_default".into(),
                    thinking_budget_tokens: None,
                    capabilities: Vec::new(),
                })
                .unwrap();
        }
        let secret = store.endpoint_secret(&endpoint.id).unwrap().unwrap();
        (directory, store, secret)
    }

    fn principal(store: &Store, name: &str) -> (String, ApiTokenPrincipal) {
        let issued = store
            .create_api_token(&CreateApiToken {
                name: name.into(),
                permissions: vec![
                    "models:read".into(),
                    "inference:invoke".into(),
                    "external:invoke".into(),
                ],
            })
            .unwrap();
        let principal = store
            .authenticate_api_token(&issued.token, "inference:invoke")
            .unwrap()
            .unwrap();
        (issued.token, principal)
    }

    fn external_profile(store: &Store) -> ModelProfile {
        store
            .model_profile_by_name("frontier-plan")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn management_url_removes_only_the_openai_v1_suffix() {
        assert_eq!(
            management_url("http://gateway:4000/v1", "key/generate")
                .unwrap()
                .as_str(),
            "http://gateway:4000/key/generate"
        );
        assert_eq!(
            management_url("https://host/proxy/v1", "key/info")
                .unwrap()
                .as_str(),
            "https://host/proxy/key/info"
        );
    }

    #[test]
    fn usage_filter_never_copies_key_material() {
        let mut row = json!({});
        merge_safe_usage(
            &mut row,
            &json!({"info":{"spend":1.25,"key":"secret","api_key":"secret","max_budget":5}}),
        );
        assert_eq!(row["usage"]["spend"], 1.25);
        assert!(row["usage"].get("key").is_none());
        assert!(row["usage"].get("api_key").is_none());
        assert_eq!(row["usage"]["remaining_budget"], 3.75);
        assert_eq!(row["usage"]["source"], "litellm");
        assert!(row["usage"]["observed_at"].is_string());
    }

    #[test]
    fn external_access_requires_permission_and_managed_budget() {
        let (_directory, store, mut endpoint) = configured_store("http://127.0.0.1:1/v1", true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let profile = external_profile(&store);
        let (_, mut principal) = principal(&store, "client");

        principal
            .permissions
            .retain(|permission| permission != "external:invoke");
        let denied = manager
            .access_for(&principal, &profile, &endpoint.endpoint)
            .unwrap();
        assert_eq!(denied["availability"], "blocked");
        assert_eq!(denied["blocked_reason"], "external_permission_required");

        principal.permissions.push("external:invoke".into());
        let mut unknown = profile.clone();
        unknown.data_destination = "unknown".into();
        let unconfirmed = manager
            .access_for(&principal, &unknown, &endpoint.endpoint)
            .unwrap();
        assert_eq!(unconfirmed["availability"], "blocked");
        assert_eq!(
            unconfirmed["blocked_reason"],
            "data_destination_confirmation_required"
        );

        endpoint.endpoint.managed_identity.max_budget = None;
        endpoint.endpoint.managed_identity.budget_duration = None;
        let unbudgeted = manager
            .access_for(&principal, &profile, &endpoint.endpoint)
            .unwrap();
        assert_eq!(unbudgeted["availability"], "blocked");
        assert_eq!(unbudgeted["blocked_reason"], "managed_budget_required");
    }

    #[tokio::test]
    async fn management_connection_requires_the_key_management_api() {
        let (_gateway, base_url) = fake_gateway().await;
        let directory = tempfile::tempdir().unwrap();
        let secrets = SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
        let store = Store::open(&directory.path().join("state.sqlite3"), secrets).unwrap();
        let manager = ManagedIdentityManager::new(store).unwrap();
        manager
            .validate_management_connection(&base_url, "master-key")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn identities_are_distinct_reused_scoped_and_concurrently_idempotent() {
        let (gateway, base_url) = fake_gateway().await;
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, first) = principal(&store, "first");
        let (_, second) = principal(&store, "second");
        let profile = external_profile(&store);

        let (one, duplicate) = tokio::join!(
            manager.credential_for(&first, &endpoint, &profile, "inference:invoke"),
            manager.credential_for(&first, &endpoint, &profile, "inference:invoke"),
        );
        assert_eq!(one.unwrap(), duplicate.unwrap());
        assert_eq!(gateway.generated.lock().await.len(), 1);
        assert_eq!(
            manager
                .credential_for(&first, &endpoint, &profile, "inference:invoke")
                .await
                .unwrap(),
            Some("virtual-key-1".into())
        );
        assert_eq!(gateway.generated.lock().await.len(), 1);
        assert_eq!(
            manager
                .credential_for(&second, &endpoint, &profile, "inference:invoke")
                .await
                .unwrap(),
            Some("virtual-key-2".into())
        );

        let generated = gateway.generated.lock().await;
        assert_eq!(generated.len(), 2);
        assert_eq!(
            generated[0]["models"],
            json!(["provider/plan", "provider/review"])
        );
        assert_eq!(generated[0]["max_budget"], 5.0);
        assert_eq!(generated[0]["budget_duration"], "30d");
        assert_eq!(generated[0]["duration"], "90d");
        assert_eq!(generated[0]["rpm_limit"], 10);
        assert_eq!(generated[0]["tpm_limit"], 20_000);
        assert_eq!(generated[0]["key_type"], "llm_api");
        assert_ne!(generated[0]["user_id"], generated[1]["user_id"]);
        assert!(!generated[0].to_string().contains("master-key"));
    }

    #[tokio::test]
    async fn revocation_blocks_poundlock_and_reconciles_the_gateway_key() {
        let (gateway, base_url) = fake_gateway().await;
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (token, principal) = principal(&store, "client");
        let profile = external_profile(&store);
        manager
            .credential_for(&principal, &endpoint, &profile, "inference:invoke")
            .await
            .unwrap();

        assert!(store.revoke_api_token(&principal.id).unwrap());
        assert!(
            store
                .authenticate_api_token(&token, "inference:invoke")
                .unwrap()
                .is_none()
        );
        assert_eq!(manager.reconcile_pending().await.unwrap(), 1);
        assert_eq!(gateway.blocked.lock().await.len(), 1);
        let mapping = store
            .managed_litellm_key(&principal.id, &endpoint.endpoint.id)
            .unwrap()
            .unwrap();
        assert_eq!(mapping.status, "revoked");
    }

    #[tokio::test]
    async fn managed_mode_fails_closed_while_shared_mode_remains_compatible() {
        let (_directory, store, managed) = configured_store("http://127.0.0.1:1/v1", true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, managed_principal) = principal(&store, "managed");
        let managed_profile = external_profile(&store);
        assert!(
            manager
                .credential_for(
                    &managed_principal,
                    &managed,
                    &managed_profile,
                    "inference:invoke",
                )
                .await
                .is_err()
        );

        let (_directory, store, shared) = configured_store("http://127.0.0.1:1/v1", false);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, shared_principal) = principal(&store, "shared");
        let shared_profile = external_profile(&store);
        assert_eq!(
            manager
                .credential_for(
                    &shared_principal,
                    &shared,
                    &shared_profile,
                    "inference:invoke",
                )
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn failed_provisioning_is_visible_and_retries_without_leaking_upstream_details() {
        let (gateway, base_url) = fake_gateway().await;
        gateway.failures_remaining.store(1, Ordering::SeqCst);
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, principal) = principal(&store, "retry");
        let profile = external_profile(&store);

        let error = manager
            .credential_for(&principal, &endpoint, &profile, "inference:invoke")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 503"));
        assert!(!error.contains("sensitive upstream detail"));
        let status = store.managed_litellm_key_statuses().unwrap();
        assert_eq!(status[0]["status"], "provision_failed");
        assert_eq!(
            status[0]["last_error"],
            "LiteLLM key provisioning failed; the next request may retry"
        );

        assert_eq!(
            manager
                .credential_for(&principal, &endpoint, &profile, "inference:invoke")
                .await
                .unwrap(),
            Some("virtual-key-1".into())
        );
        assert_eq!(gateway.generated.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn rotation_replaces_and_blocks_existing_keys_without_client_disclosure() {
        let (gateway, base_url) = fake_gateway().await;
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, principal) = principal(&store, "rotation");
        let profile = external_profile(&store);
        manager
            .credential_for(&principal, &endpoint, &profile, "inference:invoke")
            .await
            .unwrap();

        assert_eq!(
            manager
                .rotate_endpoint(&endpoint.endpoint.id)
                .await
                .unwrap(),
            1
        );
        assert_eq!(gateway.blocked.lock().await.len(), 1);
        let mapping = store
            .managed_litellm_key(&principal.id, &endpoint.endpoint.id)
            .unwrap()
            .unwrap();
        assert_eq!(mapping.virtual_key, "virtual-key-2");
        assert_eq!(mapping.status, "active");
        let status = serde_json::to_string(&store.managed_litellm_key_statuses().unwrap()).unwrap();
        assert!(!status.contains("virtual-key"));
    }

    #[tokio::test]
    async fn usage_reports_only_safe_budget_fields() {
        let (gateway, base_url) = fake_gateway().await;
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, principal) = principal(&store, "usage");
        let profile = external_profile(&store);
        manager
            .credential_for(&principal, &endpoint, &profile, "inference:invoke")
            .await
            .unwrap();
        let usage = manager.usage_for(&principal).await.unwrap();
        assert_eq!(usage["managed_identities"][0]["usage"]["spend"], 1.5);
        let info_keys = gateway.info_keys.lock().await;
        assert_eq!(info_keys.len(), 1);
        assert_eq!(info_keys[0].len(), 64);
        assert!(
            info_keys[0]
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
        assert!(!info_keys[0].contains("virtual-key"));
        let text = usage.to_string();
        assert!(!text.contains("must-not-leak"));
        assert!(!text.contains("virtual-key"));
        assert!(!text.contains("master-key"));
    }

    #[tokio::test]
    async fn a_managed_endpoint_serves_a_token_identity_and_refuses_the_admin_key() {
        let (_gateway, base_url) = fake_gateway().await;
        let (_directory, store, endpoint) = configured_store(&base_url, true);
        let manager = ManagedIdentityManager::new(store.clone()).unwrap();
        let (_, principal) = principal(&store, "dispatch");
        let model = store
            .model_profile_by_name("frontier-plan")
            .unwrap()
            .unwrap();
        manager
            .credential_for(&principal, &endpoint, &model, "inference:invoke")
            .await
            .unwrap();
        let project = store
            .create_project(&CreateProject {
                name: "managed durable".into(),
                roots: vec!["repo".into()],
                instruction_files: Vec::new(),
            })
            .unwrap();
        let input = |idempotency_key: &str| SubmitJob {
            source: None,
            idempotency_key: idempotency_key.into(),
            policy: Default::default(),
            project_id: project.id.clone(),
            model_profile_id: model.id.clone(),
            harness_profile_id: "pi".into(),
            prompt: "implement the requested change".into(),
        };

        let client_job = store
            .submit_job_for(&input("managed-client"), Some(&principal.id), 10)
            .unwrap();
        assert_eq!(
            store
                .runner_package(&client_job.id)
                .unwrap()
                .unwrap()
                .endpoint
                .api_key,
            "virtual-key-1"
        );

        // An operator-submitted job has no token identity and so no virtual
        // key. It used to fall back to the endpoint's own credential — the
        // LiteLLM administrative key — and hand it to a runner to spend.
        // `admin.rs` refuses to use that key for inference in so many words;
        // freezing it into a job contract is the same act, so it is refused
        // here too, and the job never reaches a runner holding it.
        let refused = store.submit_job(&input("operator-shared")).unwrap_err();
        assert!(
            refused.to_string().contains("administrative key"),
            "{refused}"
        );
    }
}
