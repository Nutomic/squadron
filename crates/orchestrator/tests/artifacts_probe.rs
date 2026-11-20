use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use orchestrator::artifacts;

/// #10: the strict 200-check accepts a lane URL that answers 200.
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
    assert!(artifacts::check_http_200(&format!("http://{addr}/")).await);
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
    assert!(!artifacts::check_http_200(&format!("http://{addr}/missing")).await);
    assert!(!artifacts::check_http_200(&format!("http://{addr}/broken")).await);
}

/// #10: connection failures are rejections, not panics.
#[tokio::test(flavor = "multi_thread")]
async fn check_http_200_rejects_connection_failures() {
    // Port 1 on loopback: nothing listens there in the test environment.
    assert!(!artifacts::check_http_200("http://127.0.0.1:1/").await);
}
