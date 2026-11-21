#![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::get;
use axum::Router;
use orchestrator::artifacts;

/// #10: the strict 200-check accepts a lane URL that answers 200. The loopback
/// escape is the test-only variant of the F2 SSRF-safe oracle (production
/// resolves/pins public addresses only).
#[tokio::test(flavor = "multi_thread")]
async fn check_http_200_accepts_a_200_response() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    assert!(
        artifacts::check_http_200_policy(&format!("http://{addr}/"), true).await,
        "direct 200 passes"
    );
}

/// #10: any non-200 answer (404, 500) fails the check — a lane is not done
/// until its URL really serves content.
#[tokio::test(flavor = "multi_thread")]
async fn check_http_200_rejects_non_200_responses() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/missing",
            get(|| async { (StatusCode::NOT_FOUND, "gone") }),
        )
        .route(
            "/broken",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    assert!(!artifacts::check_http_200_policy(&format!("http://{addr}/missing"), true).await);
    assert!(!artifacts::check_http_200_policy(&format!("http://{addr}/broken"), true).await);
}

/// #10: connection failures are rejections, not panics.
#[tokio::test(flavor = "multi_thread")]
async fn check_http_200_rejects_connection_failures() {
    // Port 1 on loopback: nothing listens there in the test environment.
    assert!(!artifacts::check_http_200_policy("http://127.0.0.1:1/", true).await);
}

/// Review F2: the strict oracle (production path) refuses a target that
/// resolves to a non-public address — no connection is even attempted.
#[tokio::test]
async fn check_http_200_rejects_non_public_targets() {
    // Literal private/loopback IPs are rejected by the shared is_public_ip
    // policy before any connection is made.
    for host in [
        "10.0.0.1:1",
        "172.16.0.5:1",
        "192.168.1.1:1",
        "169.254.169.254:1",
    ] {
        assert!(
            !artifacts::check_http_200(&format!("http://{host}/")).await,
            "private target {host} must not count as a passing 200"
        );
    }
}

/// Review F2: a redirect to a private IP is refused — the redirect hop is
/// resolved and validated against the same is_public_ip policy (here under
/// the loopback escape for the initial hop only; the private redirect target
/// is still rejected).
#[tokio::test(flavor = "multi_thread")]
async fn check_http_200_refuses_redirect_to_private_ip() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/",
        get(|| async { Redirect::temporary("http://10.0.0.1:1/metadata") }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    assert!(
        !artifacts::check_http_200_policy(&format!("http://{addr}/"), true).await,
        "302 to a private IP must count as failure"
    );
    assert!(
        !artifacts::check_http_200(&format!("http://{addr}/")).await,
        "the same URL also fails the strict production oracle"
    );
}
