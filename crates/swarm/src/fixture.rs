use askama::Template;
use axum::body::Body;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use crate::snapshot::{LaneSnapshot, RunSnapshot, Snapshot};
use crate::web::GalleryTemplate;

const THUMB_PNG_B64: &str = "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";

pub fn fixture_snapshot() -> Snapshot {
    let thumb = format!("data:image/png;base64,{THUMB_PNG_B64}");
    Snapshot {
        run: RunSnapshot {
            id: "__fixtures".to_owned(),
            status: "judged".to_owned(),
            judge_verdict: Some(
                "Most polished arcade feel with crisp controls and authentic CRT aesthetics; the pricing-page lane reads better as marketing copy.".to_owned(),
            ),
            winner_lane: Some(1),
        },
        lanes: vec![
            LaneSnapshot {
                idx: 0,
                state: "running".to_owned(),
                model: Some("glm-5.3-flash".to_owned()),
                impl_url: Some("https://swarm-fixture-0.sprites.app".to_owned()),
                cost_usd: Some(0.42),
                build_ms: None,
                files: None,
                loc: None,
                screenshot_state: Some("pending".to_owned()),
                val_verdict: None,
                verdict_card: None,
                console_tail: Some(
                    "harness=opencode-cli model=glm-5.3-flash\n[12:00:04] sprite created, deadline 60m\n[12:00:09] diversity variant: terminal-punk visual, keyboard-first\n[12:03:11] scaffold written, 6 files\n[12:11:38] dev server up on :8080, local 200\n".to_owned(),
                ),
                cost_series: Some(vec![0.12, 0.27, 0.42]),
                thumbnail_data_url: None,
                error: None,
            },
            LaneSnapshot {
                idx: 1,
                state: "validated".to_owned(),
                model: Some("deepseek-v4.1-flash".to_owned()),
                impl_url: Some("https://swarm-fixture-1.sprites.app".to_owned()),
                cost_usd: Some(0.87),
                build_ms: Some(2_640_000),
                files: Some(14),
                loc: Some(1832),
                screenshot_state: Some("done".to_owned()),
                val_verdict: Some("pass".to_owned()),
                verdict_card: Some("Winner: crisp arcade loop, authentic CRT styling, zero dead links.".to_owned()),
                console_tail: Some(
                    "harness=opencode-cli model=deepseek-v4.1-flash\n[12:00:05] sprite created, deadline 60m\n[12:02:19] variant: CRT arcade cabinet, scanlines, hiscore table\n[12:44:09] build done: 14 files, site 200\n[12:52:41] validator: pass, quality 8.5\n[12:58:02] screenshot rendered\n".to_owned(),
                ),
                cost_series: Some(vec![0.31, 0.55, 0.71, 0.87]),
                thumbnail_data_url: Some(thumb),
                error: None,
            },
            LaneSnapshot {
                idx: 2,
                state: "failed".to_owned(),
                model: Some("glm-5.3-flash".to_owned()),
                impl_url: None,
                cost_usd: Some(0.09),
                build_ms: None,
                files: None,
                loc: None,
                screenshot_state: Some("pending".to_owned()),
                val_verdict: None,
                verdict_card: None,
                console_tail: Some(
                    "harness=opencode-cli model=glm-5.3-flash\n[12:00:06] sprite created, deadline 60m\n[12:00:21] variant: dark minimal dashboard\n[12:01:02] harness install failed, retry 1/2\n[12:04:55] harness install failed again, lane hard-failed\n".to_owned(),
                ),
                cost_series: Some(vec![0.09]),
                thumbnail_data_url: None,
                error: Some("harness install failed twice; lane hard-failed".to_owned()),
            },
        ],
        ts: 1_732_020_000_000,
    }
}

pub fn fixture_json() -> String {
    serde_json::to_string(&fixture_snapshot()).unwrap_or_else(|_| "{}".to_owned())
}

pub fn fixture_router() -> Router {
    Router::new()
        .route("/__fixtures/run", get(fixture_run))
        .route("/__fixtures/events", get(fixture_events))
        .route("/__fixtures/gallery", get(fixture_gallery))
        .route("/__fixtures/source/{idx}", get(fixture_source))
}

async fn fixture_run() -> Response {
    let body = fixture_json();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn fixture_events() -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    let snap = fixture_snapshot();
    tokio::spawn(async move {
        let mut ticks: u32 = 0;
        if let Ok(json) = serde_json::to_string(&snap) {
            let frame = format!("data: {json}\n\n");
            if tx.send(Ok(frame.into())).await.is_err() {
                return;
            }
        }
        loop {
            if ticks >= 15 {
                ticks = 0;
                if tx
                    .send(Ok(axum::body::Bytes::from_static(b": hb\n\n")))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            ticks += 1;
        }
    });
    sse_response(ReceiverStream::new(rx))
}

async fn fixture_gallery() -> Response {
    let tpl = GalleryTemplate {
        run_id: "__fixtures".to_owned(),
        events_url: "/__fixtures/events".to_owned(),
        status: "judged".to_owned(),
    };
    Html(tpl.render().unwrap_or_default()).into_response()
}

async fn fixture_source() -> Response {
    let body = serde_json::to_vec(&json!({"fixture": true, "note": "dev-only fake tarball"}))
        .unwrap_or_default();
    let mut res = Response::new(Body::from(body));
    *res.headers_mut() = {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/gzip"),
        );
        h.insert(
            header::CONTENT_DISPOSITION,
            axum::http::HeaderValue::from_static("attachment; filename=\"lane-fixture.tar.gz\""),
        );
        h
    };
    res
}

pub fn sse_response(rx: ReceiverStream<Result<axum::body::Bytes, std::io::Error>>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(rx))
        .unwrap()
}

use axum::routing::get;
use axum::Router;
