//! The one HTTP client for `poundlock` subcommands that talk to a running
//! controller: the admin CLI, the job commands and the ACP adapter. It owns the
//! base URL, the bearer token, the timeouts, and turning a failure into a
//! message an operator can act on. It is a plain client of the HTTP API.

use crate::domain::{Event, is_terminal_job_status};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

/// A controller that does not accept the connection in this time is reported
/// unreachable.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No admin or job request waits on inference, so a minute is generous.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const JOB_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct ControllerClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    #[error("{0}")]
    Setup(String),
    #[error(
        "cannot reach the controller at {base}: {reason}. Check --controller or \
         POUNDLOCK_CONTROLLER_URL, and that the controller is running"
    )]
    Unreachable { base: String, reason: String },
    #[error("the controller at {base} did not answer {request} within {seconds} s")]
    Timeout {
        base: String,
        request: String,
        seconds: u64,
    },
    #[error(
        "the controller refused the token: {message}. Check --token, --token-file or POUNDLOCK_TOKEN"
    )]
    Unauthorized { message: String },
    #[error(
        "the controller at {base} has no {request}; it may run an older Poundlock than this \
         command ({version})",
        version = crate::VERSION
    )]
    NoRoute { base: String, request: String },
    #[error("{request} was refused: {message} (HTTP {status}{suffix})", suffix = code_suffix(.code))]
    Refused {
        request: String,
        status: u16,
        code: Option<String>,
        message: String,
    },
    #[error("{request} failed on the controller: {message} (HTTP {status}{suffix})", suffix = code_suffix(.code))]
    Failed {
        request: String,
        status: u16,
        code: Option<String>,
        message: String,
    },
    #[error("the controller answered {request} with {detail}")]
    BadResponse { request: String, detail: String },
}

fn code_suffix(code: &Option<String>) -> String {
    code.as_deref()
        .map(|code| format!(", {code}"))
        .unwrap_or_default()
}

impl ControllerError {
    /// The HTTP status the controller answered with, when it answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Unauthorized { .. } => Some(401),
            Self::NoRoute { .. } => Some(404),
            Self::Refused { status, .. } | Self::Failed { status, .. } => Some(*status),
            _ => None,
        }
    }
}

impl ControllerClient {
    /// `base` is the controller's address, such as `http://127.0.0.1:7331`.
    pub fn new(base: &str, token: &str) -> Result<Self, ControllerError> {
        let base = base.trim().trim_end_matches('/');
        let parsed = url::Url::parse(base).map_err(|_| {
            ControllerError::Setup(format!(
                "--controller must be an absolute http(s) address, not '{base}'"
            ))
        })?;
        if !["http", "https"].contains(&parsed.scheme())
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ControllerError::Setup(format!(
                "--controller must be an http(s) address without a query or fragment, not '{base}'"
            )));
        }
        let token = token.trim();
        if token.is_empty() {
            return Err(ControllerError::Setup(
                "a token is required: pass --token-file, --token or set POUNDLOCK_TOKEN".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ControllerError::Setup(format!("HTTP client: {error}")))?;
        Ok(Self {
            base: base.to_owned(),
            token: token.to_owned(),
            http,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ControllerError> {
        self.send(Method::GET, path, None::<&Value>).await
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ControllerError> {
        self.send(Method::POST, path, Some(body)).await
    }

    pub async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ControllerError> {
        self.send(Method::PUT, path, Some(body)).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T, ControllerError> {
        let request = format!("{method} {}", path.split('?').next().unwrap_or(path));
        let mut builder = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| self.transport(error, &request))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.transport(error, &request))?;
        if !status.is_success() {
            return Err(self.refusal(status, &bytes, request));
        }
        serde_json::from_slice(&bytes).map_err(|error| ControllerError::BadResponse {
            request,
            detail: format!("a body that is not the expected JSON ({error})"),
        })
    }

    fn transport(&self, error: reqwest::Error, request: &str) -> ControllerError {
        if error.is_timeout() {
            return ControllerError::Timeout {
                base: self.base.clone(),
                request: request.to_owned(),
                seconds: REQUEST_TIMEOUT.as_secs(),
            };
        }
        // The innermost cause reads best: "Connection refused (os error 61)".
        let mut cause: &dyn std::error::Error = &error;
        while let Some(source) = cause.source() {
            cause = source;
        }
        ControllerError::Unreachable {
            base: self.base.clone(),
            reason: cause.to_string(),
        }
    }

    fn refusal(&self, status: StatusCode, bytes: &[u8], request: String) -> ControllerError {
        let body: Option<Value> = serde_json::from_slice(bytes).ok();
        let error = body.as_ref().and_then(|body| body.get("error"));
        let code = error
            .and_then(|error| error["code"].as_str())
            .map(str::to_owned);
        let message = error
            .and_then(|error| error["message"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(bytes);
                let text = text.trim();
                if text.is_empty() {
                    status.canonical_reason().unwrap_or("no reason").to_owned()
                } else {
                    text.chars().take(300).collect()
                }
            });
        match status {
            StatusCode::UNAUTHORIZED => ControllerError::Unauthorized { message },
            // Axum's own 404 has an empty body; every Poundlock 404 says what
            // was not found.
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED if bytes.is_empty() => {
                ControllerError::NoRoute {
                    base: self.base.clone(),
                    request,
                }
            }
            status if status.is_server_error() => ControllerError::Failed {
                request,
                status: status.as_u16(),
                code,
                message,
            },
            status => ControllerError::Refused {
                request,
                status: status.as_u16(),
                code,
                message,
            },
        }
    }

    /// Follow a job until it ends: pass each new event to `on_event`, then
    /// return the job as the controller reports it. The job is read before its
    /// events, so the events up to its final status are all passed on.
    pub async fn follow_job(
        &self,
        id: &str,
        mut on_event: impl FnMut(&Event) -> anyhow::Result<()>,
    ) -> anyhow::Result<Value> {
        let id = resource_id("job", id)?;
        let mut after = 0_i64;
        loop {
            let job: Value = self.get(&format!("/api/v1/jobs/{id}")).await?;
            let events: Vec<Event> = self
                .get(&format!("/api/v1/jobs/{id}/events?after={after}"))
                .await?;
            for event in &events {
                after = after.max(event.sequence);
                on_event(event)?;
            }
            if is_terminal_job_status(job["status"].as_str().unwrap_or_default()) {
                return Ok(job);
            }
            tokio::time::sleep(JOB_POLL_INTERVAL).await;
        }
    }
}

/// An id that goes into a request path. Controller ids are UUIDs; anything
/// outside `[A-Za-z0-9_-]` could reach a different route.
pub fn resource_id<'a>(kind: &str, id: &'a str) -> Result<&'a str, ControllerError> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        Ok(id)
    } else {
        Err(ControllerError::Setup(format!(
            "'{id}' is not a {kind} id: use the id the controller reported"
        )))
    }
}
