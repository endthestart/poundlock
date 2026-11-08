//! The controller's application API, as one application token sees it. The
//! token is sent only to the controller and never appears in a result.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Method, Url};
use serde_json::Value;
use std::time::Duration;

/// The most of one response read into memory: a job carries up to 10 MiB of
/// patches plus its output.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Controller {
    base: Url,
    token: String,
    client: Client,
}

impl Controller {
    pub fn new(base: &str, token: String) -> Result<Self> {
        let mut base = Url::parse(base.trim()).context("the controller URL is not a URL")?;
        if !matches!(base.scheme(), "http" | "https") {
            bail!("the controller URL must be http or https");
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        if token.trim().is_empty() {
            bail!("an application token is required (POUNDLOCK_TOKEN or --token-file)");
        }
        Ok(Self {
            base,
            token: token.trim().to_owned(),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                // Uploads can be 72 MiB of JSON; everything else is small.
                .timeout(Duration::from_secs(300))
                .build()?,
        })
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.json(Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.json(Method::POST, path, Some(body)).await
    }

    async fn json(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let bytes = self.send(method, path, body).await?;
        serde_json::from_slice(&bytes).context("the controller sent a response that is not JSON")
    }

    /// A text body, such as a patch.
    pub async fn get_text(&self, path: &str) -> Result<String> {
        let bytes = self.send(Method::GET, path, None).await?;
        String::from_utf8(bytes).context("the controller sent text that is not UTF-8")
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Vec<u8>> {
        let url = self
            .base
            .join(path.trim_start_matches('/'))
            .context("build controller URL")?;
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            anyhow::anyhow!(
                "could not reach the Poundlock controller at {}: {}",
                self.base,
                error.without_url()
            )
        })?;
        let status = response.status();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read the controller's response")?;
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                bail!("the controller's response is larger than 16 MiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let detail: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let message = detail["error"]["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).into_owned()
                });
            return Err(ControllerError {
                status: status.as_u16(),
                code: detail["error"]["code"].as_str().map(str::to_owned),
                message,
            }
            .into());
        }
        Ok(bytes)
    }
}

/// An answer the controller gave with an error status.
#[derive(Debug, thiserror::Error)]
#[error("Poundlock answered {status}{}: {message}", code.as_deref().map(|code| format!(" ({code})")).unwrap_or_default())]
pub struct ControllerError {
    pub status: u16,
    pub code: Option<String>,
    pub message: String,
}
