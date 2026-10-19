//! Browser security headers for every controller response.
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// The UI loads only its own scripts, styles and fonts and calls only its own
/// API, with no inline script or style, so everything else is refused. It may
/// not be framed, which keeps "Pause inference" and friends out of reach of
/// clickjacking.
pub(super) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
    style-src 'self'; font-src 'self'; img-src 'self'; connect-src 'self'; \
    form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

pub(super) async fn security_headers(request: Request, next: Next) -> Response {
    // Responses under these paths carry tokens, job data and model output.
    let private = ["/api/", "/v1/"]
        .iter()
        .any(|prefix| request.uri().path().starts_with(prefix));
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if private && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
