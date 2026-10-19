use super::NodeCredential;
use crate::domain::{EnrollNode, EnrolledNode, NodeAction};
use anyhow::{Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Clone)]
pub(super) struct NodeClient {
    base: String,
    client: Client,
    download: Client,
}

impl NodeClient {
    pub(super) fn new(base: String, network: &super::network::NetworkPolicy) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            client: network
                .client(
                    Client::builder()
                        .timeout(Duration::from_secs(30))
                        .redirect(reqwest::redirect::Policy::none()),
                )?
                .build()?,
            download: network
                .client(
                    Client::builder()
                        .timeout(Duration::from_secs(120))
                        .redirect(reqwest::redirect::Policy::none()),
                )?
                .build()?,
        })
    }

    pub(super) fn base(&self) -> &str {
        &self.base
    }

    pub(super) async fn enroll(&self, input: EnrollNode) -> Result<EnrolledNode> {
        self.unauthenticated("/node/v1/enroll", &input).await
    }

    pub(super) async fn heartbeat(
        &self,
        credential: &NodeCredential,
        capabilities: &Value,
    ) -> Result<()> {
        let _: Value = self
            .authenticated(
                credential,
                "heartbeat",
                &json!({
                    "platform": std::env::consts::OS,
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "agent_version": env!("CARGO_PKG_VERSION"),
                    "capabilities": capabilities
                }),
            )
            .await?;
        Ok(())
    }

    /// Asks for the next action, and for an empty poll to be held open for up
    /// to `wait` (whole seconds, capped). A controller that predates the option
    /// ignores it and answers at once; the caller tells the two apart by how
    /// long the reply took.
    pub(super) async fn poll(
        &self,
        credential: &NodeCredential,
        wait: Duration,
    ) -> Result<Option<NodeAction>> {
        let wait_seconds = wait.as_secs().min(super::MAX_POLL_WAIT_SECONDS);
        self.authenticated(
            credential,
            "actions/poll",
            &json!({"wait_seconds": wait_seconds}),
        )
        .await
    }

    pub(super) async fn complete(
        &self,
        credential: &NodeCredential,
        id: &str,
        success: bool,
        result: Value,
    ) -> Result<()> {
        let _: Value = self
            .authenticated(
                credential,
                &format!("actions/{id}/complete"),
                &json!({"success":success,"result":result}),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn status(&self, credential: &NodeCredential) -> Result<Value> {
        self.get_authenticated(credential, "status").await
    }
    pub(super) async fn maintenance(
        &self,
        credential: &NodeCredential,
        paused: bool,
    ) -> Result<Value> {
        self.authenticated(credential, "maintenance", &json!({"paused":paused}))
            .await
    }
    pub(super) async fn revoke(&self, credential: &NodeCredential) -> Result<Value> {
        self.authenticated(credential, "revoke", &json!({})).await
    }

    pub(super) async fn update_info(&self, credential: &NodeCredential) -> Result<Value> {
        self.get_authenticated(credential, "update").await
    }

    pub(super) async fn download_update(
        &self,
        credential: &NodeCredential,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<()> {
        let url = format!(
            "{}/node/v1/nodes/{}/update/binary",
            self.base, credential.node_id
        );
        let response = self
            .download
            .get(url)
            .bearer_auth(&credential.credential)
            .send()
            .await?;
        let status = response.status();
        if status.is_redirection() {
            bail!("refusing to follow a redirect for the node binary");
        }
        if !status.is_success() {
            bail!("controller returned {status} for the node binary");
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            bail!("update is larger than the size cap");
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut received = 0_u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            received += chunk.len() as u64;
            if received > max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                bail!("update exceeded the size cap");
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }

    async fn get_authenticated<T: serde::de::DeserializeOwned>(
        &self,
        credential: &NodeCredential,
        path: &str,
    ) -> Result<T> {
        let url = format!(
            "{}/node/v1/nodes/{}/{}",
            self.base, credential.node_id, path
        );
        let response = self
            .client
            .get(url)
            .bearer_auth(&credential.credential)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            bail!(
                "controller returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn authenticated<T: serde::de::DeserializeOwned>(
        &self,
        credential: &NodeCredential,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        self.request(
            format!(
                "{}/node/v1/nodes/{}/{}",
                self.base, credential.node_id, path
            ),
            Some(&credential.credential),
            body,
        )
        .await
    }

    async fn unauthenticated<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        self.request(format!("{}{}", self.base, path), None, body)
            .await
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        token: Option<&str>,
        body: &impl Serialize,
    ) -> Result<T> {
        let mut request = self.client.post(url).json(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            bail!(
                "controller returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}
