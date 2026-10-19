//! A ceiling on requests in flight across the whole controller (L4). Over it,
//! a request is refused at once with 503 instead of queueing behind the others,
//! and a streamed response keeps its place until its body ends.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::RETRY_AFTER;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body::{Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::ApiError;

#[derive(Clone)]
pub struct InFlight(Arc<Semaphore>);

impl InFlight {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(Semaphore::new(limit)))
    }
}

pub async fn limit(State(in_flight): State<InFlight>, request: Request, next: Next) -> Response {
    // Liveness answers even when the controller is full.
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let Ok(permit) = in_flight.0.clone().try_acquire_owned() else {
        let mut response = ApiError::unavailable_code(
            "controller_busy",
            "the controller is at its limit of requests in flight; retry shortly",
        )
        .into_response();
        response
            .headers_mut()
            .insert(RETRY_AFTER, axum::http::HeaderValue::from_static("1"));
        return response;
    };
    hold(next.run(request).await, permit)
}

/// The response, keeping `permit` until its body is dropped.
pub(crate) fn hold(response: Response, permit: OwnedSemaphorePermit) -> Response {
    response.map(|inner| {
        Body::new(Held {
            inner,
            _permit: permit,
        })
    })
}

/// A response body that keeps its request's place until it is dropped. It
/// forwards the size hint, so a sized response keeps its Content-Length.
struct Held {
    inner: Body,
    _permit: OwnedSemaphorePermit,
}

impl http_body::Body for Held {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(limit_to: usize) -> (Router, InFlight) {
        let in_flight = InFlight::new(limit_to);
        let router = Router::new()
            .route("/work", get(|| async { "ok" }))
            .route("/healthz", get(|| async { "alive" }))
            .layer(axum::middleware::from_fn_with_state(
                in_flight.clone(),
                limit,
            ));
        (router, in_flight)
    }

    fn get_request(path: &str) -> Request {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn refuses_over_the_limit_and_keeps_health_answering() {
        let (router, in_flight) = app(1);
        let held = in_flight.0.clone().try_acquire_owned().unwrap();
        let refused = router.clone().oneshot(get_request("/work")).await.unwrap();
        assert_eq!(
            refused.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(refused.headers()[RETRY_AFTER], "1");
        let health = router
            .clone()
            .oneshot(get_request("/healthz"))
            .await
            .unwrap();
        assert_eq!(health.status(), axum::http::StatusCode::OK);
        drop(held);
        let served = router.oneshot(get_request("/work")).await.unwrap();
        assert_eq!(served.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn a_response_body_holds_its_place_until_dropped() {
        let (router, in_flight) = app(1);
        let response = router.clone().oneshot(get_request("/work")).await.unwrap();
        assert_eq!(in_flight.0.available_permits(), 0);
        assert_eq!(http_body::Body::size_hint(response.body()).exact(), Some(2));
        let body = response.into_body().collect().await.unwrap();
        assert_eq!(&body.to_bytes()[..], b"ok");
        assert_eq!(in_flight.0.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_never_finishes_times_out_and_frees_its_place() {
        let in_flight = InFlight::new(1);
        let router = Router::new()
            .route(
                "/work",
                axum::routing::post(|body: Bytes| async move { body.len().to_string() }),
            )
            .layer(tower_http::timeout::RequestBodyTimeoutLayer::new(
                std::time::Duration::from_secs(120),
            ))
            .layer(axum::middleware::from_fn_with_state(
                in_flight.clone(),
                limit,
            ));
        let stalled = Request::builder()
            .method("POST")
            .uri("/work")
            .body(Body::from_stream(futures_util::stream::pending::<
                Result<Bytes, std::io::Error>,
            >()))
            .unwrap();
        let response = router.clone().oneshot(stalled).await.unwrap();
        assert!(!response.status().is_success(), "{}", response.status());
        drop(response);
        assert_eq!(in_flight.0.available_permits(), 1);
    }
}
