use askama::Template;
use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html as HtmlResponse, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use std::sync::Arc;
use store::{Lane, Run, Store};
use tokio_stream::wrappers::ReceiverStream;

use crate::state::AppState;

pub type SharedState = Arc<AppState>;

const SAMPLE_BRIEFS: [&str; 3] = [
    "A 1996-style Netscape-era homepage for a fictional browser launch.",
    "Marketing site for a VM product pitched at AI agents, not people.",
    "Build the most fun 90s arcade clone you can in one hour.",
];

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/runs", get(runs_page))
        .route("/runs/table", get(runs_table))
        .route("/runs", post(submit))
        .route("/{run_id}", get(gallery))
        .route("/{run_id}/events", get(events))
        .route("/{run_id}/lane/{idx}", get(lane_detail))
        .route("/{run_id}/lane/{idx}/tokei", get(lane_tokei))
        .route("/{run_id}/thumb/{idx}", get(thumb))
        .route("/{run_id}/source/{idx}", get(source))
        .route("/{run_id}/promote", post(promote))
        .route("/static/{*path}", get(serve_static))
        .with_state(state)
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub briefs: Vec<Brief>,
}

pub struct Brief {
    pub text: String,
    pub short: String,
}

async fn index() -> HtmlResponse<String> {
    let tpl = IndexTemplate {
        briefs: SAMPLE_BRIEFS
            .iter()
            .map(|b| Brief {
                text: b.to_string(),
                short: truncate(b, 34),
            })
            .collect(),
    };
    HtmlResponse(tpl.render().unwrap_or_default())
}

#[derive(Deserialize)]
pub struct SubmitForm {
    pub brief: Option<String>,
    pub n: Option<i64>,
}

async fn submit(State(state): State<SharedState>, Form(form): Form<SubmitForm>) -> Response {
    let Some(brief) = form
        .brief
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, "brief must not be empty").into_response();
    };
    let n = form.n.unwrap_or(state.config.n_lanes as i64).clamp(3, 10) as usize;
    match state.start_pipeline(brief, n, state.real_harness()).await {
        Ok(run) => Redirect::to(&format!("/{}", run.id)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Template)]
#[template(path = "runs.html")]
pub struct RunsTemplate {
    pub rows: Vec<RunRow>,
}

#[derive(Template)]
#[template(path = "runs_table.html")]
pub struct RunsTableTemplate {
    pub rows: Vec<RunRow>,
}

pub struct RunRow {
    pub id: String,
    pub brief: String,
    pub status: String,
    pub created: String,
}

async fn runs_page(State(state): State<SharedState>) -> HtmlResponse<String> {
    let rows = load_rows(&state.store).await;
    let tpl = RunsTemplate { rows };
    HtmlResponse(tpl.render().unwrap_or_default())
}

async fn runs_table(State(state): State<SharedState>) -> HtmlResponse<String> {
    let rows = load_rows(&state.store).await;
    let tpl = RunsTableTemplate { rows };
    HtmlResponse(tpl.render().unwrap_or_default())
}

async fn load_rows(store: &Store) -> Vec<RunRow> {
    match store.list_runs().await {
        Ok(runs) => runs
            .into_iter()
            .map(|r| RunRow {
                id: r.id,
                brief: truncate(&r.brief, 120),
                status: r.status,
                created: fmt_utc(r.created_at),
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Template)]
#[template(path = "lane.html")]
pub struct LaneTemplate {
    pub run_id: String,
    pub idx: i64,
    pub state: String,
    pub model: String,
    pub impl_url: Option<String>,
    pub build: String,
    pub cost: String,
    pub val_verdict: Option<String>,
    pub val_quality: Option<String>,
    pub val_summary: String,
    pub tokei_json: String,
    pub console_tail: String,
    pub error: Option<String>,
    pub screenshot_state: String,
}

#[derive(Template)]
#[template(path = "tokei.html")]
pub struct TokeiTemplate {
    pub idx: i64,
    pub tokei_json: String,
}

#[derive(Template)]
#[template(path = "gallery.html")]
pub struct GalleryTemplate {
    pub run_id: String,
    pub events_url: String,
    pub status: String,
}

async fn serve_static(State(state): State<SharedState>, Path(path): Path<String>) -> Response {
    if path.is_empty()
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == ".." || seg == ".")
    {
        return not_found();
    }
    let full = state.static_dir.join(&path);
    let mime = match full.extension().and_then(|e| e.to_str()) {
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("html") => "text/html",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(&full).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "max-age=300"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => not_found(),
    }
}

async fn gallery(State(state): State<SharedState>, Path(run_id): Path<String>) -> Response {
    let Some(run) = get_run(&state, &run_id).await else {
        return not_found();
    };
    let tpl = GalleryTemplate {
        run_id: run.id.clone(),
        events_url: format!("/{}/events", run.id),
        status: run.status.clone(),
    };
    HtmlResponse(tpl.render().unwrap_or_default()).into_response()
}

async fn events(State(state): State<SharedState>, Path(run_id): Path<String>) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let store = state.store.clone();
    let first = match crate::snapshot::build_snapshot(&store, &run_id).await {
        Some(s) => s,
        None => return not_found(),
    };
    let mut hub_rx = state.get_hub(&run_id).await.map(|h| h.subscribe());
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let mut last = String::new();
        let mut snapshot = Some(first);
        let mut last_hb = std::time::Instant::now();
        loop {
            let mut hub_closed = false;
            match hub_rx.as_mut() {
                Some(hub_rx) => {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        msg = hub_rx.recv() => {
                            hub_closed = matches!(
                                msg,
                                Err(tokio::sync::broadcast::error::RecvError::Closed)
                            );
                        }
                    }
                }
                None => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
            if hub_closed {
                hub_rx = None;
            }
            if let Some(snap) = snapshot.take() {
                if let Ok(json) = serde_json::to_string(&snap) {
                    if json != last {
                        last = json.clone();
                        let frame = format!("data: {json}\n\n");
                        if tx.send(Ok(frame.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            if last_hb.elapsed() >= std::time::Duration::from_secs(15) {
                last_hb = std::time::Instant::now();
                if tx
                    .send(Ok(axum::body::Bytes::from_static(b": hb\n\n")))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            snapshot = crate::snapshot::build_snapshot(&store, &run_id).await;
        }
    });
    crate::fixture::sse_response(ReceiverStream::new(rx))
}

async fn lane_detail(
    State(state): State<SharedState>,
    Path((run_id, idx)): Path<(String, i64)>,
) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let Some(run) = get_run(&state, &run_id).await else {
        return not_found();
    };
    let Some(lane) = state.store.get_lane(&run_id, idx).await.ok().flatten() else {
        return not_found();
    };
    let lane_tpl = LaneTemplate {
        run_id: run.id.clone(),
        idx: lane.idx,
        state: lane.state.clone(),
        model: lane.impl_model.clone().unwrap_or_default(),
        impl_url: lane.impl_url.clone(),
        build: build_time(&lane),
        cost: fmt_cost(lane.cost_usd),
        val_verdict: lane.val_verdict.clone(),
        val_quality: lane.val_quality.map(|q| format!("{q:.2}")),
        val_summary: lane.val_summary.clone().unwrap_or_default(),
        tokei_json: lane.tokei_json.clone().unwrap_or_default(),
        console_tail: lane.console_tail.clone().unwrap_or_default(),
        error: lane.error.clone(),
        screenshot_state: lane.screenshot_state.clone().unwrap_or_default(),
    };
    HtmlResponse(lane_tpl.render().unwrap_or_default()).into_response()
}

async fn lane_tokei(
    State(state): State<SharedState>,
    Path((run_id, idx)): Path<(String, i64)>,
) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let Some(lane) = state.store.get_lane(&run_id, idx).await.ok().flatten() else {
        return not_found();
    };
    let tpl = TokeiTemplate {
        idx: lane.idx,
        tokei_json: lane.tokei_json.clone().unwrap_or_default(),
    };
    HtmlResponse(tpl.render().unwrap_or_default()).into_response()
}

async fn thumb(
    State(state): State<SharedState>,
    Path((run_id, idx)): Path<(String, i64)>,
) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let Some(lane) = state.store.get_lane(&run_id, idx).await.ok().flatten() else {
        return not_found();
    };
    let Some(state_str) = lane.screenshot_state else {
        return not_found();
    };
    if state_str != "done" {
        return not_found();
    }
    let Some(path) = lane.screenshot_path else {
        return not_found();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut res = Response::new(Body::from(bytes));
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("image/png"),
            );
            headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("max-age=60"),
            );
            res
        }
        Err(_) => not_found(),
    }
}

async fn source(
    State(state): State<SharedState>,
    Path((run_id, idx)): Path<(String, i64)>,
) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let Some(lane) = state.store.get_lane(&run_id, idx).await.ok().flatten() else {
        return not_found();
    };
    let Some(path) = lane.tarball_path else {
        return not_found();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut res = Response::new(Body::from(bytes));
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/gzip"),
            );
            headers.insert(
                header::CONTENT_DISPOSITION,
                header::HeaderValue::from_str(&format!(
                    "attachment; filename=\"lane-{idx}.tar.gz\""
                ))
                .unwrap_or_else(|_| header::HeaderValue::from_static("attachment")),
            );
            res
        }
        Err(_) => not_found(),
    }
}

async fn promote(State(state): State<SharedState>, Path(run_id): Path<String>) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    match get_run(&state, &run_id).await {
        Some(run) if run.status == "judged" => {
            let _ = state.store.update_run_status(&run_id, "promoted").await;
            Redirect::to(&format!("/{run_id}")).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "run is not judged").into_response(),
    }
}

async fn get_run(state: &AppState, run_id: &str) -> Option<Run> {
    state.store.get_run(run_id).await.ok()?
}

fn valid_id(id: &str) -> bool {
    // canonical validator: exactly 22 chars of URL-safe base64, same as the
    // store generates for run ids
    store::is_valid_run_id(id)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}...")
    }
}

fn build_time(lane: &Lane) -> String {
    match (lane.impl_started_at, lane.val_started_at) {
        (Some(start), Some(end)) if end >= start => {
            let mins = (end - start) / 60_000;
            if mins >= 60 {
                format!("{}h{}m", mins / 60, mins % 60)
            } else {
                format!("{mins}m")
            }
        }
        _ => "-".to_owned(),
    }
}

fn fmt_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.2}"),
        None => "-".to_owned(),
    }
}

pub fn fmt_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        millis
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn not_found() -> Response {
    let tpl = NotFoundTemplate;
    let html = tpl.render().unwrap_or_default();
    (StatusCode::NOT_FOUND, HtmlResponse(html)).into_response()
}

#[derive(Template)]
#[template(path = "404.html")]
struct NotFoundTemplate;
