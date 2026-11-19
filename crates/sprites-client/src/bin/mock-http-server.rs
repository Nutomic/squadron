#[tokio::main]
async fn main() {
    let port = std::env::var("MOCK_HTTP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock http server");
    axum::serve(listener, sprites_client::mock_http::router())
        .await
        .expect("serve mock http server");
}
