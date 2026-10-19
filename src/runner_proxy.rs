//! A worker can reach only the existing job-scoped inference API through this
//! listener. It never receives the supervisor's controller credential.
use anyhow::Result;
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, Response, StatusCode},
    routing::{get, post},
};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
struct Proxy {
    controller: String,
    client: reqwest::Client,
}

pub(super) async fn start(controller: &str) -> Result<tokio::task::JoinHandle<()>> {
    let state = Proxy {
        controller: controller.trim_end_matches('/').into(),
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?,
    };
    let app = Router::new()
        .route("/job/v1/{id}/models", get(models))
        .route("/job/v1/{id}/chat/completions", post(chat))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        // Workers send one request at a time. Each job may have a few in flight,
        // so one misbehaving job cannot starve the others (L15, review 9), and
        // the runner as a whole has a ceiling.
        .layer(axum::middleware::from_fn_with_state(
            PerJob::default(),
            per_job,
        ))
        .layer(axum::middleware::from_fn_with_state(
            crate::server::limit::InFlight::new(64),
            crate::server::limit::limit,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:7332").await?;
    Ok(tokio::spawn(async move {
        if let Err(error) = axum::serve(crate::tcp::nodelay(listener), app).await {
            tracing::error!(%error,"worker inference listener stopped");
        }
    }))
}

async fn models(
    State(proxy): State<Proxy>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, StatusCode> {
    forward(proxy, id, headers, Method::GET, "models", None).await
}
async fn chat(
    State(proxy): State<Proxy>,
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Result<Response<Body>, StatusCode> {
    forward(
        proxy,
        id,
        headers,
        Method::POST,
        "chat/completions",
        Some(body),
    )
    .await
}
async fn forward(
    proxy: Proxy,
    id: String,
    headers: HeaderMap,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Response<Body>, StatusCode> {
    let id = uuid::Uuid::parse_str(&id).map_err(|_| StatusCode::NOT_FOUND)?;
    let authorization = headers
        .get("authorization")
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut request = proxy
        .client
        .request(method, format!("{}/job/v1/{id}/{path}", proxy.controller))
        .header("authorization", authorization);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let upstream = request.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut response = Response::builder().status(upstream.status());
    if let Some(value) = upstream.headers().get("content-type") {
        response = response.header("content-type", value);
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

/// Requests one job's worker may have in flight at once.
const PER_JOB: usize = 8;

#[derive(Clone, Default)]
struct PerJob(
    std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>,
    >,
);

async fn per_job(
    State(jobs): State<PerJob>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    // `/job/v1/{id}/...`; anything else is refused by routing anyway.
    let id = request
        .uri()
        .path()
        .split('/')
        .nth(3)
        .unwrap_or_default()
        .to_owned();
    let semaphore = {
        let mut map = jobs.0.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 256 {
            // Forget idle jobs, so ended ones do not accumulate.
            map.retain(|_, semaphore| semaphore.available_permits() < PER_JOB);
        }
        map.entry(id)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(PER_JOB)))
            .clone()
    };
    let Ok(permit) = semaphore.try_acquire_owned() else {
        return axum::response::IntoResponse::into_response((
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "1")],
            "this job has too many model requests in flight",
        ));
    };
    crate::server::limit::hold(next.run(request).await, permit)
}

#[cfg(test)]
mod per_job_tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn one_busy_job_does_not_hold_up_another() {
        let jobs = PerJob::default();
        let busy = jobs
            .0
            .lock()
            .unwrap()
            .entry("a".into())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(PER_JOB)))
            .clone();
        let _held = busy.acquire_many_owned(PER_JOB as u32).await.unwrap();
        let app = Router::new()
            .route("/job/v1/{id}/models", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(jobs, per_job));
        let call = |id: &str| {
            axum::extract::Request::builder()
                .uri(format!("/job/v1/{id}/models"))
                .body(Body::empty())
                .unwrap()
        };
        let refused = app.clone().oneshot(call("a")).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        let served = app.oneshot(call("b")).await.unwrap();
        assert_eq!(served.status(), StatusCode::OK);
    }
}
