//! Audit R3 finding 2: server-initiated fetches of agent-controlled URLs must
//! not follow unvalidated redirects or connect to non-public resolved IPs
//! (link-local metadata endpoints, RFC1918, loopback).
#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use orchestrator::net::{fetch_public_text, fetch_text, FetchPolicy};
use tokio::sync::Mutex;

/// A 302 redirect to the cloud-metadata endpoint must be rejected: even with
/// the test-only loopback allowance for the first hop, the redirect target
/// (169.254.169.254, link-local) is not public and is never followed.
#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_private_metadata_target_is_rejected() {
    let hits = Arc::new(Mutex::new(0u32));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let state = hits.clone();
    let app = Router::new().route(
        "/",
        get(move || {
            let state = state.clone();
            async move {
                *state.lock().await += 1;
                (
                    StatusCode::FOUND,
                    [("location", "http://169.254.169.254/latest/meta-data/")],
                )
                    .into_response()
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let policy = FetchPolicy {
        max_hops: 3,
        allow_loopback: true, // test servers run on loopback
    };
    let err = fetch_text(&format!("http://{addr}/"), policy)
        .await
        .unwrap_err();
    assert!(
        err.contains("non-public address"),
        "rejection must name the address check, got: {err}"
    );
    // The metadata target was never contacted: the only request hit was the
    // first hop itself, and no follow-up request was made.
    assert_eq!(*hits.lock().await, 1);
    // The strict wrapper also rejects the loopback first hop outright.
    assert!(
        fetch_public_text(&format!("http://{addr}/")).await.is_err(),
        "strict fetch must reject loopback targets"
    );
}

/// A redirect between validated public-ish (loopback-allowed) hops is followed
/// manually, bounded by the hop count.
#[tokio::test(flavor = "multi_thread")]
async fn validated_redirect_is_followed_to_the_final_body() {
    let listener_b = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr_b = listener_b.local_addr().unwrap();
    let app_b = Router::new().route("/", get(|| async { "final body" }));
    tokio::spawn(async move {
        let _ = axum::serve(listener_b, app_b).await;
    });

    let listener_a = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr_a = listener_a.local_addr().unwrap();
    let app_a = Router::new().route(
        "/",
        get(move || {
            let target = format!("http://{addr_b}/");
            async move { (StatusCode::FOUND, [("location", target)]).into_response() }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener_a, app_a).await;
    });

    let policy = FetchPolicy {
        max_hops: 3,
        allow_loopback: true,
    };
    let body = fetch_text(&format!("http://{addr_a}/"), policy)
        .await
        .unwrap();
    assert_eq!(body, "final body");
}

/// A redirect loop is cut off by the bounded hop count.
#[tokio::test(flavor = "multi_thread")]
async fn redirect_loop_is_bounded() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/",
        get(move || {
            let target = format!("http://{addr}/");
            async move { (StatusCode::FOUND, [("location", target)]).into_response() }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let policy = FetchPolicy {
        max_hops: 2,
        allow_loopback: true,
    };
    let err = fetch_text(&format!("http://{addr}/"), policy)
        .await
        .unwrap_err();
    assert!(err.contains("too many redirects"), "got: {err}");
}

/// A direct fetch of a private-range IP literal is rejected before any
/// connection is made (no server needed on the target port).
#[tokio::test]
async fn private_ip_literal_targets_are_rejected() {
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://127.0.0.1:1/",
    ] {
        let err = fetch_public_text(url).await.unwrap_err();
        assert!(
            err.contains("non-public address"),
            "{url} must be rejected by the address check, got: {err}"
        );
    }
}

/// Hostnames that resolve to non-public addresses are rejected too (localhost
/// resolves to loopback via /etc/hosts), and unresolvable hosts fail honestly.
#[tokio::test]
async fn localhost_hostname_and_unresolvable_host_are_rejected() {
    let err = fetch_public_text("http://localhost:1/").await.unwrap_err();
    assert!(err.contains("non-public address"), "got: {err}");
    assert!(
        fetch_public_text("http://nonexistent-host.invalid/")
            .await
            .is_err(),
        "unresolvable host must fail, not fall through"
    );
}
