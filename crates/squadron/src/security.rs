use axum::extract::Request;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

pub const CSP: &str = "default-src 'self'; frame-src 'self' https://*.sprites.app; img-src 'self' data: https://*.sprites.app; script-src 'self'; style-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'self'; object-src 'none'; frame-ancestors 'self'";

/// Review F15 #12: the service is HTTPS-only behind Fly, so tell browsers to
/// stop trying plain HTTP.
const HSTS: &str = "max-age=31536000; includeSubDomains";

pub async fn harden(req: Request, next: Next) -> Response {
    // Static assets are immutable-ish and already carry their own
    // `max-age` from the static handler; do not clobber it with no-store.
    let cache_exempt = req.uri().path().starts_with("/static/");
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        HeaderName::from_static("strict-transport-security"),
        HeaderValue::from_static(HSTS),
    );
    if !cache_exempt {
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("no-store, max-age=0"),
        );
    }
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );
    res
}
