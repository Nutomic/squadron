use askama::Template;
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html as HtmlResponse, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{TimeZone, Utc};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use store::{Lane, Run, Store};
use tokio_stream::wrappers::ReceiverStream;

use crate::csrf;
use crate::ratelimit::Rejection;
use crate::state::AppState;

pub type SharedState = Arc<AppState>;

/// R2 finding 6: a run cannot change once it reaches one of these statuses, so
/// SSE streams stop there instead of polling the database forever.
fn is_terminal_status(status: &str) -> bool {
    matches!(status, "judged" | "failed" | "promoted")
}

const SAMPLE_BRIEFS: [&str; 3] = [
    "A 1996-style Netscape-era homepage for a fictional browser launch.",
    "Marketing site for a VM product pitched at AI agents, not people.",
    "Build the most fun 90s arcade clone you can in one hour.",
];

/// Review F15 #3: complete short chip labels (never truncation ellipses).
const SAMPLE_SHORTS: [&str; 3] = [
    "1996 Netscape homepage",
    "VM product marketing site",
    "90s arcade clone",
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
        .route("/robots.txt", get(robots_txt))
        .route("/static/{*path}", get(serve_static))
        .with_state(state)
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub briefs: Vec<Brief>,
    /// Review F3 #3: double-submit CSRF token, also set as a cookie.
    pub csrf_token: String,
}

pub struct Brief {
    pub text: String,
    pub short: String,
}

async fn index() -> Response {
    let token = csrf::new_token();
    let tpl = IndexTemplate {
        briefs: SAMPLE_BRIEFS
            .iter()
            .zip(SAMPLE_SHORTS)
            .map(|(b, short)| Brief {
                text: b.to_string(),
                short: short.to_string(),
            })
            .collect(),
        csrf_token: token.clone(),
    };
    let mut res = HtmlResponse(tpl.render().unwrap_or_default()).into_response();
    let (name, value) = csrf::set_cookie_header(&token);
    res.headers_mut().insert(name, value);
    res
}

#[derive(Deserialize)]
pub struct SubmitForm {
    pub brief: Option<String>,
    pub n: Option<i64>,
    /// Review F3 #3: double-submit token (must match the swarm_csrf cookie).
    pub csrf_token: Option<String>,
}

async fn submit(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ConnectInfo(info): ConnectInfo<SocketAddr>,
    Form(form): Form<SubmitForm>,
) -> Response {
    // Review F3 #3: CSRF first (fail closed), then the rate limits.
    if !csrf::check(&headers, form.csrf_token.as_deref()) {
        return (StatusCode::FORBIDDEN, "invalid or missing CSRF token").into_response();
    }
    let Some(brief) = form
        .brief
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, "brief must not be empty").into_response();
    };
    // R2-3#5: the form brief is capped like the API (brief is DATA).
    if !valid_brief_len(&brief) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "brief must be at most {} characters",
                crate::api::MAX_BRIEF_CHARS
            ),
        )
            .into_response();
    }
    let n = form.n.unwrap_or(state.config.n_lanes as i64).clamp(3, 10) as usize;
    // Review F3 #1/#2: per-IP and process-wide run-creation limits; the form
    // gets a graceful (HTML) retry message instead of a JSON error.
    let ip = client_ip(&headers, ConnectInfo(info));
    if let Err(rejection) = state.limiter.check(&ip, &state.store).await {
        return too_many_requests_html(&rejection);
    }
    match state.start_pipeline(brief, n, state.real_harness()).await {
        Ok(run) => {
            state.limiter.record(&ip, &run.id);
            Redirect::to(&format!("/{}", run.id)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Review F3 #2: 429 + Retry-After with a human-facing HTML page for the form
/// path (the JSON API wraps the same reason in a JSON error).
fn too_many_requests_html(rejection: &Rejection) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        HtmlResponse(format!(
            "<!DOCTYPE html><html><body><h1>429 — too many runs right now</h1>\
             <p>{}.</p><p><a href=\"/\">Back to the form</a></p></body></html>",
            html_escape(&rejection.message())
        )),
    )
        .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Review F3 #1: best-effort client IP for the per-IP run-creation limit.
/// Reverse-proxy headers first (Fly sets `Fly-Client-IP`), then the socket
/// address when the service was made with connect info, else a shared
/// "unknown" bucket.
pub(crate) fn client_ip(headers: &HeaderMap, info: ConnectInfo<SocketAddr>) -> String {
    for name in ["x-forwarded-for", "fly-client-ip"] {
        if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if let Some(first) = value.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_owned();
                }
            }
        }
    }
    info.0.ip().to_string()
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
    /// Full-precision timestamp kept for the hover title only.
    pub created_full: String,
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
        Ok(mut runs) => {
            // The subtitle promises "newest first" — enforce it here.
            runs.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            let now_ms = Utc::now().timestamp_millis();
            runs.into_iter()
                .map(|r| RunRow {
                    id: r.id,
                    brief: truncate(&r.brief, 120),
                    status: r.status,
                    created: fmt_relative(r.created_at, now_ms),
                    created_full: fmt_utc(r.created_at),
                })
                .collect()
        }
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
    pub val_features: String,
    pub val_bugs: String,
    pub val_security: String,
    pub tokei_json: String,
    pub console_tail: String,
    pub error: Option<String>,
    pub screenshot_state: String,
}

/// #32: the lane fields shared by real and fixture lane rendering, so both
/// paths map them identically. Val-report fields are filled per source.
#[derive(Clone)]
pub struct LaneCommon {
    pub idx: i64,
    pub state: String,
    pub model: String,
    pub impl_url: Option<String>,
    pub build: String,
    pub cost: String,
    pub val_verdict: Option<String>,
    pub tokei_json: String,
    pub console_tail: String,
    pub error: Option<String>,
    pub screenshot_state: String,
}

impl LaneCommon {
    pub fn from_store(lane: &Lane) -> Self {
        Self {
            idx: lane.idx,
            state: lane.state.clone(),
            model: lane.impl_model.clone().unwrap_or_default(),
            impl_url: lane.impl_url.clone(),
            build: build_time(lane),
            cost: fmt_cost(lane.cost_usd),
            val_verdict: lane.val_verdict.clone(),
            tokei_json: lane.tokei_json.clone().unwrap_or_default(),
            console_tail: lane.console_tail.clone().unwrap_or_default(),
            error: lane.error.clone(),
            screenshot_state: lane.screenshot_state.clone().unwrap_or_default(),
        }
    }

    pub fn from_view(view: &crate::snapshot::LaneSnapshot) -> Self {
        Self {
            idx: view.idx,
            state: view.state.clone(),
            model: view.model.clone().unwrap_or_default(),
            impl_url: view.impl_url.clone(),
            build: view
                .build_ms
                .map(fmt_build_ms)
                .unwrap_or_else(|| "-".to_owned()),
            cost: fmt_cost(view.cost_usd),
            val_verdict: view.val_verdict.clone(),
            tokei_json: String::new(),
            console_tail: view.console_tail.clone().unwrap_or_default(),
            error: None,
            screenshot_state: view.screenshot_state.clone().unwrap_or_default(),
        }
    }
}

impl LaneTemplate {
    pub fn from_common(run_id: &str, c: &LaneCommon) -> Self {
        Self {
            run_id: run_id.to_owned(),
            idx: c.idx,
            state: c.state.clone(),
            model: c.model.clone(),
            impl_url: c.impl_url.clone(),
            build: c.build.clone(),
            cost: c.cost.clone(),
            val_verdict: c.val_verdict.clone(),
            val_quality: None,
            val_summary: String::new(),
            val_features: String::new(),
            val_bugs: String::new(),
            val_security: String::new(),
            tokei_json: c.tokei_json.clone(),
            console_tail: c.console_tail.clone(),
            error: c.error.clone(),
            screenshot_state: c.screenshot_state.clone(),
        }
    }
}

fn fmt_build_ms(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins >= 60 {
        format!("{}h{}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

fn text_or_list_to_string(v: &prompts::TextOrList) -> String {
    match v {
        prompts::TextOrList::Text(s) => s.clone(),
        prompts::TextOrList::List(items) => items.join("; "),
    }
}

/// Hydrate the persisted validator report (features/bugs/security) for the
/// lane detail view; falls back to empty strings when absent.
fn validator_report_view(lane: &store::Lane) -> (String, String, String) {
    lane.val_report_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<prompts::ValidatorOutput>(json).ok())
        .map(|out| {
            (
                text_or_list_to_string(&out.features),
                text_or_list_to_string(&out.bugs),
                text_or_list_to_string(&out.security),
            )
        })
        .unwrap_or_default()
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
    /// Review F3 #3: CSRF token for the island's promote form (also set as a
    /// cookie on this GET).
    pub csrf_token: String,
}

/// Review F15 #13: minimal robots policy; the static file wins when present,
/// otherwise the built-in default keeps crawlers fully allowed.
async fn robots_txt(State(state): State<SharedState>) -> Response {
    const DEFAULT: &str = "User-agent: *\nAllow: /\n";
    let body = tokio::fs::read_to_string(state.static_dir.join("robots.txt"))
        .await
        .unwrap_or_else(|_| DEFAULT.to_owned());
    (
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::CACHE_CONTROL, "max-age=300"),
        ],
        body,
    )
        .into_response()
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
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
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
    // R2-3#3: gallery now gates the run id like every other {run_id} route.
    if !valid_id(&run_id) {
        return not_found();
    }
    let Some(run) = get_run(&state, &run_id).await else {
        return not_found();
    };
    // R2 finding 2: viewing the gallery enqueues pending renders so gallery
    // cards fill in without visiting each lane page first.
    if let Ok(lanes) = state.store.list_lanes(&run.id).await {
        for lane in &lanes {
            enqueue_pending_render(&state.screenshots, &run.id, lane);
        }
    }
    let token = csrf::new_token();
    let tpl = GalleryTemplate {
        run_id: run.id.clone(),
        events_url: format!("/{}/events", run.id),
        status: run.status.clone(),
        csrf_token: token.clone(),
    };
    let mut res = HtmlResponse(tpl.render().unwrap_or_default()).into_response();
    let (name, value) = csrf::set_cookie_header(&token);
    res.headers_mut().insert(name, value);
    res
}

async fn events(State(state): State<SharedState>, Path(run_id): Path<String>) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    let store = state.store.clone();
    let screenshots = state.screenshots.clone();
    let first = match crate::snapshot::build_snapshot(&store, &run_id).await {
        Some(s) => s,
        None => return not_found(),
    };
    let mut hub_rx = state.get_hub(&run_id).await.map(|h| h.subscribe());
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let mut last = String::new();
        let mut last_hb = std::time::Instant::now();
        // R2 finding 6: hub payloads are the wake signal (event-driven, no
        // per-second DB polling); the wire format stays the full UI snapshot,
        // rebuilt once per wake. Without a hub, fall back to the single
        // initial DB snapshot and only re-check on the slow heartbeat.
        if send_snapshot(&tx, &mut last, &first).await {
            return;
        }
        // A run that is already terminal at connect time gets exactly one
        // snapshot and the stream ends.
        if is_terminal_status(&first.run.status) {
            screenshots.evict_terminal(&run_id).await;
            return;
        }
        loop {
            let mut hub_closed = false;
            let mut hub_terminal = false;
            let mut hub_payload: Option<crate::snapshot::Snapshot> = None;
            match hub_rx.as_mut() {
                Some(hub_rx) => {
                    tokio::select! {
                        msg = hub_rx.recv() => match msg {
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                hub_closed = true;
                            }
                            Err(_) => {}
                            Ok(payload) => {
                                hub_terminal = is_terminal_status(&payload.run.status);
                                hub_payload = Some(payload);
                            }
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {}
                    }
                }
                None => tokio::time::sleep(std::time::Duration::from_secs(15)).await,
            }
            if hub_closed {
                hub_rx = None;
            }
            // R2-4 unification: the hub payload is the live source; the DB
            // rebuild below is only the late-subscriber fallback.
            let snap = match hub_payload {
                Some(p) => p,
                None => match crate::snapshot::build_snapshot(&store, &run_id).await {
                    Some(s) => s,
                    None => break,
                },
            };
            if send_snapshot(&tx, &mut last, &snap).await {
                break;
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
            // Terminal (from the hub payload or the fresh snapshot): send the
            // final state, evict cached screenshot entries for the run, stop.
            if hub_terminal || is_terminal_status(&snap.run.status) {
                screenshots.evict_terminal(&run_id).await;
                break;
            }
        }
    });
    crate::fixture::sse_response(ReceiverStream::new(rx))
}

/// Send one snapshot frame; true when the receiver is gone.
async fn send_snapshot(
    tx: &tokio::sync::mpsc::Sender<Result<axum::body::Bytes, std::io::Error>>,
    last: &mut String,
    snap: &crate::snapshot::Snapshot,
) -> bool {
    match serde_json::to_string(snap) {
        Ok(json) if json != *last => {
            *last = json.clone();
            let frame = format!("data: {json}\n\n");
            tx.send(Ok(frame.into())).await.is_err()
        }
        _ => false,
    }
}

/// R2 finding 2: enqueue a lazy screenshot render for a lane whose render has
/// not started (or queued) yet, without blocking the response.
fn enqueue_pending_render(queue: &orchestrator::ScreenshotQueue, run_id: &str, lane: &Lane) {
    let Some(url) = lane.impl_url.clone() else {
        return;
    };
    let current = lane.screenshot_state.clone().unwrap_or_default();
    if current.is_empty() || current == "pending" || current == "queued" {
        let queue = queue.clone();
        let rid = run_id.to_owned();
        let idx = lane.idx;
        tokio::spawn(async move {
            queue.request(&rid, idx, &url).await;
        });
    }
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
    let common = LaneCommon::from_store(&lane);
    let mut lane_tpl = LaneTemplate::from_common(run.id.as_str(), &common);
    let (val_features, val_bugs, val_security) = validator_report_view(&lane);
    lane_tpl.val_quality = lane.val_quality.map(|q| format!("{q:.2}"));
    lane_tpl.val_summary = lane.val_summary.clone().unwrap_or_default();
    lane_tpl.val_features = val_features;
    lane_tpl.val_bugs = val_bugs;
    lane_tpl.val_security = val_security;
    // #3: viewing a lane also triggers the lazy screenshot render.
    enqueue_pending_render(&state.screenshots, &run_id, &lane);
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
    // #3: first view triggers the lazy render for lanes that have not been
    // rendered yet (pending or failed-before-render).
    enqueue_pending_render(&state.screenshots, &run_id, &lane);
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

/// Review F3 #3: the promote POST carries the double-submit token too (the
/// island's promote form includes it; the token is also rendered as
/// data-csrf-token on the island root).
#[derive(Deserialize)]
pub struct PromoteForm {
    pub csrf_token: Option<String>,
}

async fn promote(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Form(form): Form<PromoteForm>,
) -> Response {
    if !valid_id(&run_id) {
        return not_found();
    }
    if !csrf::check(&headers, form.csrf_token.as_deref()) {
        return (StatusCode::FORBIDDEN, "invalid or missing CSRF token").into_response();
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

/// R2-3#5: single helper every `{run_id}` extractor routes through (web + API).
pub(crate) fn valid_id(id: &str) -> bool {
    // #31: canonical ID validation shared with the store layer; also used by
    // the API routes so every {run_id} extractor goes through one helper.
    store::is_valid_run_id(id)
}

/// R2-3#5: shared brief cap for the web form and the JSON API.
pub(crate) fn valid_brief_len(brief: &str) -> bool {
    brief.chars().count() <= crate::api::MAX_BRIEF_CHARS
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max).collect();
    // Prefer a word boundary so sample chips and briefs never cut mid-word.
    if let Some(space) = cut.rfind(char::is_whitespace) {
        if space >= max / 2 {
            let trimmed: String = cut.chars().take(space).collect();
            return format!("{trimmed}...");
        }
    }
    format!("{cut}...")
}

/// Relative age for the runs table ("2h ago"); falls back to a short date
/// (no milliseconds) once the run is older than a week.
fn fmt_relative(ms: i64, now_ms: i64) -> String {
    let diff = (now_ms - ms).max(0);
    let mins = diff / 60_000;
    if mins < 1 {
        return "just now".to_owned();
    }
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    match Utc.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%b %d %H:%M").to_string(),
        None => "-".to_owned(),
    }
}

fn build_time(lane: &Lane) -> String {
    match (lane.impl_started_at, lane.val_started_at) {
        (Some(start), Some(end)) if end >= start => fmt_build_ms(end - start),
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
    match Utc.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S%.3fZ").to_string(),
        None => "-".to_owned(),
    }
}

fn not_found() -> Response {
    let tpl = NotFoundTemplate;
    let html = tpl.render().unwrap_or_default();
    (StatusCode::NOT_FOUND, HtmlResponse(html)).into_response()
}

#[derive(Template)]
#[template(path = "404.html")]
struct NotFoundTemplate;
