use crate::domain::EndpointSecret;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use std::time::Duration;

pub const MAX_INFERENCE_JSON_BYTES: usize = 1024 * 1024;

pub const IDENTITY_PROBE_TIMEOUT_SECONDS: u64 = 10;
pub const GENERATE_CHECK_TIMEOUT_SECONDS: u64 = 90;

#[derive(Clone)]
pub struct InferenceClient {
    probe_client: Client,
    generate_client: Client,
    relay_client: Client,
    identity_timeout_seconds: u64,
    generate_timeout_seconds: u64,
}

impl InferenceClient {
    pub fn new(_timeout_seconds: u64) -> Result<Self> {
        // Outer wake/prepare may last minutes. Identity stays short so
        // wait_for_model can retry; Generate is a single bounded check.
        Self::with_timeouts(
            IDENTITY_PROBE_TIMEOUT_SECONDS,
            GENERATE_CHECK_TIMEOUT_SECONDS,
        )
    }

    fn with_timeouts(identity_seconds: u64, generate_seconds: u64) -> Result<Self> {
        let identity_seconds = identity_seconds.max(1);
        let generate_seconds = generate_seconds.max(1);
        Ok(Self {
            probe_client: Client::builder()
                .connect_timeout(Duration::from_secs(identity_seconds.min(10)))
                .timeout(Duration::from_secs(identity_seconds))
                .build()?,
            generate_client: Client::builder()
                .connect_timeout(Duration::from_secs(identity_seconds.min(10)))
                .timeout(Duration::from_secs(generate_seconds))
                .build()?,
            relay_client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            identity_timeout_seconds: identity_seconds,
            generate_timeout_seconds: generate_seconds,
        })
    }

    pub fn check_failure_message(&self, error: &anyhow::Error, generate: bool) -> String {
        if error_is_timeout(error) {
            if generate {
                return format!(
                    "Generation did not finish in {}. Retry Generate.",
                    seconds_label(self.generate_timeout_seconds)
                );
            }
            return format!(
                "The model listing did not finish in {}.",
                seconds_label(self.identity_timeout_seconds)
            );
        }
        if generate {
            "Generate check failed".into()
        } else {
            "Check failed".into()
        }
    }

    pub async fn probe(&self, endpoint: &EndpointSecret, model: &str) -> Result<Value> {
        let response = self
            .probe_client
            .get(join_url(&endpoint.endpoint.base_url, "models"))
            .headers(endpoint_headers(endpoint)?)
            .send()
            .await
            .context("connect to inference endpoint")?;
        let status = response.status();
        let body = read_json_capped(response).await?;
        if !status.is_success() {
            bail!("models request returned {status}: {}", bounded_json(&body));
        }
        let models = body.get("data").and_then(Value::as_array);
        let found = model.is_empty()
            || models.is_some_and(|models| {
                models
                    .iter()
                    .any(|item| item.get("id").and_then(Value::as_str) == Some(model))
            });
        if models.is_none() {
            bail!("models response did not contain a data array");
        }
        if !model.is_empty() && !found {
            bail!("configured model '{model}' was not returned by /models");
        }
        // Ids only, bounded: enough for an operator to pick one, and nothing
        // else from the server's answer is repeated.
        let ids: Vec<&str> = models
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("id").and_then(Value::as_str))
            .filter(|id| id.len() <= 256)
            .take(500)
            .collect();
        Ok(json!({ "reachable": true, "model_visible": found, "model_ids": ids }))
    }

    pub async fn completion_probe(
        &self,
        endpoint: &EndpointSecret,
        model: &str,
        max_output_tokens: u64,
    ) -> Result<Value> {
        let response = self
            .generate_client
            .post(join_url(&endpoint.endpoint.base_url, "chat/completions"))
            .headers(endpoint_headers(endpoint)?)
            .json(
                &json!({"model": model, "messages": [{"role":"user","content":"Reply OK."}],
                "max_tokens": max_output_tokens.min(512), "stream": false, "temperature": 0}),
            )
            .send()
            .await
            .context("send compatibility probe")?;
        let status = response.status();
        let body = read_json_capped(response).await?;
        if !status.is_success() {
            bail!(
                "completion probe returned {status}: {}",
                bounded_json(&body)
            );
        }
        validate_chat_completion(&body)?;
        Ok(json!({"reachable": true, "completion": true}))
    }

    pub async fn fim_completion_probe(
        &self,
        endpoint: &EndpointSecret,
        model: &str,
    ) -> Result<Value> {
        let response = self
            .generate_client
            .post(join_url(&endpoint.endpoint.base_url, "completions"))
            .headers(endpoint_headers(endpoint)?)
            .json(&json!({
                "model": model,
                "prompt": "<|fim_prefix|>fn answer() -> u32 {\n    <|fim_suffix|>\n}\n<|fim_middle|>",
                "max_tokens": 16,
                "stream": false,
                "temperature": 0,
                "stop": ["<|endoftext|>", "<|fim_pad|>", "<|fim_prefix|>",
                    "<|fim_middle|>", "<|fim_suffix|>"]
            }))
            .send()
            .await
            .context("send FIM compatibility probe")?;
        let status = response.status();
        let body = read_json_capped(response).await?;
        if !status.is_success() {
            bail!("FIM probe returned {status}: {}", bounded_json(&body));
        }
        let generated = body
            .pointer("/choices/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty());
        if !generated {
            bail!("FIM probe returned no completion text");
        }
        Ok(json!({"reachable": true, "fim_completion": true}))
    }

    pub async fn relay(
        &self,
        endpoint: &EndpointSecret,
        path: &str,
        body: Value,
    ) -> Result<Response> {
        Ok(self
            .relay_client
            .post(join_url(&endpoint.endpoint.base_url, path))
            .headers(endpoint_headers(endpoint)?)
            .json(&body)
            .send()
            .await?)
    }
}

async fn read_json_capped(response: reqwest::Response) -> Result<Value> {
    let mut data = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read inference response")?;
        if data.len().saturating_add(chunk.len()) > MAX_INFERENCE_JSON_BYTES {
            bail!("inference response exceeded {MAX_INFERENCE_JSON_BYTES} bytes");
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).context("decode inference JSON")
}

fn error_is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
    })
}

fn seconds_label(seconds: u64) -> String {
    if seconds == 1 {
        "1 second".into()
    } else {
        format!("{seconds} seconds")
    }
}

fn endpoint_headers(endpoint: &EndpointSecret) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    if !endpoint.api_key.is_empty() {
        let mut value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", endpoint.api_key))
                .map_err(|_| anyhow::anyhow!("inference credential is not a valid HTTP header"))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    Ok(headers)
}

pub fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn bounded_json(value: &Value) -> String {
    let value = value.to_string();
    if value.len() > 1024 {
        let mut end = 1024;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
    } else {
        value
    }
}

fn validate_chat_completion(body: &Value) -> Result<()> {
    let answer = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str);
    if !answer.is_some_and(|text| !text.trim().is_empty()) {
        bail!("Generate returned no answer text; reasoning alone is not a completed answer");
    }
    Ok(())
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
