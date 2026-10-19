mod admin;
pub(crate) mod capacity;
mod discovery;
mod headers;
mod job_inference;
mod jobs;
pub(crate) mod limit;
mod model_draft;
mod model_names;
mod nodes;
mod reasoning;
mod redact;
mod relay;
mod runner_api;
pub(crate) mod runtime;
mod tokens;
mod update;

use crate::client::InferenceClient;
use crate::config::AppConfig;
use crate::litellm::ManagedIdentityManager;
use crate::scheduler::Scheduler;
use crate::store::Store;
use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use std::sync::Arc;
use thiserror::Error;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub store: Store,
    pub inference: InferenceClient,
    pub occupancy: capacity::Occupancy,
    pub runners: runner_api::RunnerPresence,
    pub managed_identities: ManagedIdentityManager,
}

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let store = Store::open(&config.database_path(), config.secrets.clone())?;
    let inference = InferenceClient::new(config.external_timeout_seconds)?;
    let occupancy = capacity::Occupancy::new();
    occupancy.recover(&store)?;
    let managed_identities = ManagedIdentityManager::new(store.clone())?;
    let state = AppState {
        config: Arc::new(config),
        store,
        inference,
        occupancy,
        runners: Default::default(),
        managed_identities,
    };
    let reconciliation = state.managed_identities.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if let Err(error) = reconciliation.reconcile_pending().await {
                tracing::warn!(%error, "managed LiteLLM reconciliation failed");
            }
        }
    });
    Scheduler::spawn(state.clone());
    if let Some(backup) = state.config.backup.clone() {
        tracing::info!(dir = %backup.dir.display(), at = %backup.at, "nightly database backup on (UTC)");
        tokio::spawn(crate::backup::schedule(
            backup,
            state.config.database_path(),
        ));
    }
    let bind = state.config.bind;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "controller listening");
    axum::serve(crate::tcp::nodelay(listener), router(state)).await?;
    Ok(())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route(
            "/jobs.js",
            get(|| async { js(include_str!("../../web/jobs.js")) }),
        )
        .route("/connections.js", get(connections_js))
        .route("/models.js", get(models_js))
        .route(
            "/gateway-handoff.js",
            get(|| async { js(include_str!("../../web/gateway-handoff.js")) }),
        )
        .route("/machines.js", get(machines_js))
        .route(
            "/docs/linux-node",
            get(|| async { axum::response::Redirect::temporary("/#linux-guide") }),
        )
        .route("/app.css", get(app_css))
        .route("/theme.css", get(theme_css))
        .route("/theme-boot.js", get(theme_boot_js))
        .route("/fonts/inter.woff2", get(font_inter))
        .route("/fonts/jetbrains-mono.woff2", get(font_jetbrains_mono))
        .route("/api/openapi.yaml", get(openapi))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .merge(admin::routes())
        .merge(discovery::routes())
        .merge(model_draft::routes())
        .merge(jobs::routes(state.clone()))
        .merge(nodes::routes())
        .merge(runner_api::routes(state.clone()))
        .merge(job_inference::routes())
        .merge(tokens::routes())
        .merge(relay::routes(state.clone()))
        .merge(update::routes())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        // A request body must arrive within two minutes, so slow uploads cannot
        // hold the in-flight permits below for ever (review of L4).
        .layer(tower_http::timeout::RequestBodyTimeoutLayer::new(
            std::time::Duration::from_secs(120),
        ))
        .layer(axum::middleware::from_fn_with_state(
            limit::InFlight::new(state.config.max_requests_in_flight as usize),
            limit::limit,
        ))
        .layer(axum::middleware::from_fn(headers::security_headers))
        .layer(CompressionLayer::new())
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../../web/index.html"))
}
async fn app_js() -> ([(&'static str, &'static str); 1], &'static str) {
    js(include_str!("../../web/app.js"))
}
async fn connections_js() -> ([(&'static str, &'static str); 1], &'static str) {
    js(include_str!("../../web/connections.js"))
}
async fn models_js() -> ([(&'static str, &'static str); 1], &'static str) {
    js(include_str!("../../web/models.js"))
}
async fn machines_js() -> ([(&'static str, &'static str); 1], &'static str) {
    js(include_str!("../../web/machines.js"))
}
fn js(body: &'static str) -> ([(&'static str, &'static str); 1], &'static str) {
    ([("content-type", "text/javascript; charset=utf-8")], body)
}
async fn app_css() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../../web/app.css"),
    )
}
async fn theme_css() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../../web/theme.css"),
    )
}
async fn theme_boot_js() -> ([(&'static str, &'static str); 1], &'static str) {
    js(include_str!("../../web/theme-boot.js"))
}
async fn font_inter() -> ([(&'static str, &'static str); 2], &'static [u8]) {
    font(include_bytes!("../../web/fonts/inter.woff2"))
}
async fn font_jetbrains_mono() -> ([(&'static str, &'static str); 2], &'static [u8]) {
    font(include_bytes!("../../web/fonts/jetbrains-mono.woff2"))
}
fn font(body: &'static [u8]) -> ([(&'static str, &'static str); 2], &'static [u8]) {
    (
        [
            ("content-type", "font/woff2"),
            ("cache-control", "public, max-age=31536000, immutable"),
        ],
        body,
    )
}
async fn openapi() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "application/yaml; charset=utf-8")],
        include_str!("../../docs/reference/openapi.yaml"),
    )
}
async fn health() -> impl IntoResponse {
    axum::Json(json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}))
}
/// Controller/storage readiness only: the process is serving and its SQLite
/// store answers. It deliberately does not gate on runners, nodes, model
/// endpoints or credentials, which change independently of the controller and
/// are reported per profile by access preflight and job options.
async fn ready(State(state): State<AppState>) -> ApiResult<axum::Json<serde_json::Value>> {
    state.store.check_ready().map_err(|error| {
        tracing::error!(%error, "controller storage readiness check failed");
        ApiError::unavailable("controller storage is unavailable")
    })?;
    Ok(axum::Json(json!({
        "status":"ready",
        "scope":"controller_storage",
        "checks":{"storage":"ok"},
        "version":env!("CARGO_PKG_VERSION")
    })))
}

pub fn admin_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    authorize(headers, &state.config.admin_token, "admin")
}

/// Operator-only handlers with a JSON body take this ahead of their
/// `Json<...>` extractor, so a missing or wrong bearer token is refused
/// before axum spends any effort deserializing what the caller sent —
/// otherwise an application token, or no token at all, could reach a 422
/// deserialization error on a route it was never authorized to call.
pub struct AdminAuth;

impl<S> FromRequestParts<S> for AdminAuth
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        admin_auth(&AppState::from_ref(state), &parts.headers)?;
        Ok(Self)
    }
}

pub fn api_token_auth(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
) -> ApiResult<crate::domain::ApiTokenPrincipal> {
    let token = bearer(headers).unwrap_or_default();
    if let Some(principal) = state.store.authenticate_api_token(token, permission)? {
        Ok(principal)
    } else {
        Err(ApiError::unauthorized(format!(
            "valid API token with {permission} permission required"
        )))
    }
}

pub fn runner_auth(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    authorize(headers, &state.config.runner_token, "runner")
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn authorize(headers: &HeaderMap, expected: &str, kind: &str) -> ApiResult<()> {
    use subtle::ConstantTimeEq;
    let supplied = bearer(headers).unwrap_or_default();
    if expected.is_empty()
        || supplied.is_empty()
        || !bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
    {
        return Err(ApiError::unauthorized(format!(
            "valid {kind} bearer token required"
        )));
    }
    Ok(())
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ApiError {
    status: StatusCode,
    message: String,
    code: Option<&'static str>,
}

impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            code: None,
        }
    }
    /// The machine-readable code, when the error has one.
    pub fn code(&self) -> Option<&'static str> {
        self.code
    }
    /// A 400 with a machine-readable code, for a refusal a client can act on.
    pub fn bad_code(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            code: Some(code),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            code: None,
        }
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
            code: None,
        }
    }
    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
            code: Some(code),
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
            code: None,
        }
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
            code: None,
        }
    }
    /// A 503 with a machine-readable code: refused now, for a reason an
    /// operator can clear.
    pub fn unavailable_code(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
            code: Some(code),
        }
    }
    /// An upstream that stopped answering. A 5xx, so a harness with its own
    /// retry policy treats it as transient rather than as a bad request.
    pub fn gateway_timeout(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            message: message.into(),
            code: Some(code),
        }
    }
}

fn unique_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(_, Some(message))
            if message.contains("UNIQUE constraint failed")
    )
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        // A job that already ended is a conflict, not a fault. The runner
        // needs to tell that from "the controller is broken", or its outbox
        // retries a settled report forever.
        if let Some(ended) = error.downcast_ref::<crate::store::JobAlreadyEndedError>() {
            return Self {
                status: StatusCode::CONFLICT,
                message: ended.0.clone(),
                code: Some("job_already_ended"),
            };
        }
        if let Some(limit) = error.downcast_ref::<crate::store::TooManyActiveJobsError>() {
            return Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: limit.to_string(),
                code: Some("too_many_active_jobs"),
            };
        }
        if let Some(error) = error.downcast_ref::<crate::store::ConfigurationError>() {
            return if error.0 == "a resource with that name already exists" {
                Self::conflict(error.0)
            } else {
                Self::bad(error.0)
            };
        }
        if error.chain().any(|cause| {
            cause
                .downcast_ref::<rusqlite::Error>()
                .is_some_and(unique_constraint)
        }) {
            return Self::conflict("a resource with that name already exists");
        }
        tracing::error!(%error, "internal request error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".into(),
            code: None,
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        if unique_constraint(&error) {
            return Self::conflict("a resource with that name already exists");
        }
        tracing::error!(%error, "database request error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".into(),
            code: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({"message":self.message,"status":self.status.as_u16()});
        if let Some(code) = self.code {
            error["code"] = json!(code);
        }
        (self.status, axum::Json(json!({"error":error}))).into_response()
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ops_tests.rs"]
mod ops_tests;

#[cfg(test)]
#[path = "warm_path_tests.rs"]
mod warm_path_tests;

#[cfg(test)]
#[path = "node_poll_tests.rs"]
mod node_poll_tests;

#[cfg(test)]
#[path = "revocation_tests.rs"]
mod revocation_tests;
