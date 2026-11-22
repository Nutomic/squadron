//! Review F3 #3: double-submit CSRF tokens for the HTML form endpoints
//! (POST /runs and POST /{run_id}/promote). The service is public-by-design
//! (no login), so a stateless double-submit cookie is the fit:
//!
//! - every GET that renders a form issues a fresh unguessable token and sets
//!   it both in a cookie (`swarm_csrf`) and in the page (hidden input /
//!   `data-csrf-token` for the island's promote form);
//! - the POST is accepted only when the cookie and the form token are both
//!   present, non-empty and equal;
//! - the JSON API is unaffected (it does not rely on ambient cookie auth).

use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use uuid::Uuid;

pub const CSRF_COOKIE: &str = "swarm_csrf";

/// A fresh double-submit token: uuid v4 (122 random bits).
pub fn new_token() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Cookie value for the token (HttpOnly + Secure: the page reads the token
/// from its own data attribute, never from document.cookie).
pub fn cookie_value(token: &str) -> String {
    format!("{CSRF_COOKIE}={token}; Path=/; Max-Age=7200; SameSite=Lax; Secure; HttpOnly")
}

pub fn set_cookie_header(token: &str) -> (HeaderName, HeaderValue) {
    (
        SET_COOKIE,
        HeaderValue::from_str(&cookie_value(token)).expect("cookie value is ASCII"),
    )
}

/// Read the double-submit token out of the request's Cookie header.
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&format!("{CSRF_COOKIE}=")) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Constant-time-ish equality for token comparison.
fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The POST is valid only when cookie token and form token are present,
/// non-empty and equal.
pub fn check(headers: &HeaderMap, form_token: Option<&str>) -> bool {
    let Some(cookie) = token_from_headers(headers) else {
        return false;
    };
    let Some(form) = form_token.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    tokens_equal(&cookie, form)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn headers_with_cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(value).expect("cookie header"));
        headers
    }

    #[test]
    fn round_trip_and_rejections() {
        let token = new_token();
        assert_eq!(token.len(), 32);
        assert_ne!(new_token(), token, "tokens are unguessable and fresh");

        let headers = headers_with_cookie(&cookie_value(&token));
        assert!(check(&headers, Some(&token)));
        // missing / empty / mismatching form token
        assert!(!check(&headers, None));
        assert!(!check(&headers, Some("")));
        assert!(!check(&headers, Some("   ")));
        assert!(!check(&headers, Some(&new_token())));
        // missing cookie
        let empty = HeaderMap::new();
        assert!(!check(&empty, Some(&token)));
        // unrelated cookies do not satisfy the check
        let other = headers_with_cookie("session=abc");
        assert!(!check(&other, Some(&token)));
    }
}
