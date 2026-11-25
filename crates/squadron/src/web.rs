use askama::Template;
use axum::body::Body;
use axum::extract::{ConnectInfo, Json, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html as HtmlResponse, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use store::{Lane, Run, Store};
use tokio_stream::wrappers::ReceiverStream;

use crate::csrf;
use crate::ratelimit::Rejection;
use crate::state::AppState;

/// Wave C: the dialog owns the lane count — one worker per parallel agent,
/// hard cap shared with the decomposition phase (MAX_DECOMPOSE_CASES = 8).
pub const MAX_WORKERS: usize = orchestrator::MAX_DECOMPOSE_CASES;
/// Case prompts are DATA: a decomposed impl_prompt is the brief plus the
/// case's angle (the boilerplate constraints ride along at execution time, see
/// `prompts::impl_prompt_with_constraints`), so a multiple of the brief cap
/// bounds the payload without rejecting realistic decompositions.
pub const MAX_CASE_PROMPT_CHARS: usize = crate::api::MAX_BRIEF_CHARS * 4;
const MAX_LABEL_CHARS: usize = 200;

pub type SharedState = Arc<AppState>;

/// R2 finding 6: a run cannot change once it reaches one of these statuses, so
/// SSE streams stop there instead of polling the database forever. `partial`
/// (F27: some lanes validated, some failed) is terminal too — the winner among
/// the surviving lanes is already chosen.
fn is_terminal_status(status: &str) -> bool {
    matches!(status, "judged" | "partial" | "failed" | "promoted")
}

/// F28: the samples demo the decomposition features — explicit counts and
/// "or"-alternatives become one parallel worker per item.
const SAMPLE_BRIEFS: [&str; 3] = [
    "Build a Netscape-era 1996 homepage in 5 different color schemes.",
    "Marketing site for a fictional VM product in Svelte or React.",
    "Make a playable Pong, Tetris, or Space Invaders arcade game.",
];

/// Review F15 #3: complete short chip labels (never truncation ellipses).
const SAMPLE_SHORTS: [&str; 3] = [
    "Netscape homepage ×5 colors",
    "VM marketing site (Svelte or React)",
    "Pong or Tetris or Space Invaders",
];

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/runs", get(runs_page))
        .route("/runs/table", get(runs_table))
        // Wave C: the dialog flow replaces the direct form submit —
        // preview is stateless, /runs (confirm) is the only persistence.
        .route("/api/preview", post(preview))
        .route("/runs", post(confirm))
        .route("/{run_id}", get(gallery))
        .route("/{run_id}/events", get(events))
        .route("/{run_id}/lane/{idx}", get(lane_detail))
        .route("/{run_id}/thumb/{idx}", get(thumb))
        .route("/{run_id}/source/{idx}", get(source))
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
pub struct PreviewBody {
    pub brief: Option<String>,
    /// Review F3 #3: double-submit token (must match the squadron_csrf cookie).
    pub csrf_token: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct ConfirmBody {
    /// Display brief for the runs table; when absent the first case prompt
    /// stands in (the dialog usually sends the typed brief).
    pub brief: Option<String>,
    /// The edited cases from the prompt-editing dialog — one worker per
    /// parallel agent. This is the ONLY persistence input: no decomposition
    /// happens at confirm time.
    #[serde(default)]
    pub cases: Vec<CaseInput>,
    /// Review F3 #3: double-submit token (must match the squadron_csrf cookie).
    pub csrf_token: Option<String>,
}

#[derive(Deserialize, Clone, Serialize)]
pub struct CaseInput {
    pub label: Option<String>,
    pub impl_prompt: String,
}

/// Wave C #1: stateless preview. Runs the SAME decomposition call the
/// pipeline uses (`orchestrator::decompose_brief`: same retry-on-empty and
/// guaranteed single-raw-brief fallback), gated + injection-scanned like all
/// agent output. Nothing is persisted — no run row, no lanes; the cases go
/// back to the browser for editing in the dialog.
async fn preview(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<PreviewBody>,
) -> Response {
    // CSRF first (fail closed) — the dialog fetch echoes the page token.
    if !csrf::check(&headers, body.csrf_token.as_deref()) {
        return (StatusCode::FORBIDDEN, "invalid or missing CSRF token").into_response();
    }
    let Some(brief) = body
        .brief
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, "brief must not be empty").into_response();
    };
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
    // The session id only feeds the gateway's x-opencode-session header
    // (F12): a fresh id per preview keeps user sessions from bleeding into
    // each other; nothing with this id is persisted.
    let preview_id = format!("preview-{}", uuid::Uuid::new_v4().simple());
    let cases = match &state.machine_llm {
        Some(llm) => {
            orchestrator::decompose_brief(llm, &preview_id, &brief)
                .await
                .cases
        }
        None => template_cases(&brief, state.config.n_lanes),
    };
    Json(json!({ "cases": cases })).into_response()
}

/// No machine LLM (dev/test): mirror the pipeline's deterministic fallback —
/// template variants of the default lane count, reshaped into the dialog's
/// {label, impl_prompt} cases. F25: the impl_prompt carries only the brief +
/// the variant's angle/stack/tone — NO boilerplate constraints (those are
/// appended at execution time, never shown to the user). F32: the shared
/// `template_case_prompt` states the prompt exactly once, then its angle.
fn template_cases(brief: &str, n: usize) -> Vec<Value> {
    prompts::template_variants(n)
        .into_iter()
        .map(|v| {
            json!({
                "label": v.angle,
                "impl_prompt": prompts::template_case_prompt(brief, &v)
            })
        })
        .collect()
}

/// Wave C #3: the confirm button. The ONLY persistence point of the dialog
/// flow: the run row and its lanes are created from the SUBMITTED (edited)
/// cases — the lane count comes from `cases.len()`, never from a decomposition
/// at confirm time. F24: the submitted cases also DRIVE the pipeline — the
/// handler enters the same execution machinery as a decomposed run
/// (`start_run_cases`): rows are created and the lanes start immediately, or
/// the run is queued for FIFO promotion when the slots don't fit. A rows-only
/// persist here is the bug this fixes: nothing ever picked the run up, so it
/// sat `queued` forever.
async fn confirm(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ConnectInfo(info): ConnectInfo<SocketAddr>,
    Json(body): Json<ConfirmBody>,
) -> Response {
    // Review F3 #3: CSRF first (fail closed), then validation, then limits.
    if !csrf::check(&headers, body.csrf_token.as_deref()) {
        return (StatusCode::FORBIDDEN, "invalid or missing CSRF token").into_response();
    }
    if body.cases.is_empty() {
        return (StatusCode::BAD_REQUEST, "at least one worker is required").into_response();
    }
    if body.cases.len() > MAX_WORKERS {
        return (
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_WORKERS} workers per run"),
        )
            .into_response();
    }
    let mut cases: Vec<Value> = Vec::with_capacity(body.cases.len());
    for (i, case) in body.cases.iter().enumerate() {
        let prompt = case.impl_prompt.trim();
        if prompt.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                format!("worker {} prompt must not be empty", i + 1),
            )
                .into_response();
        }
        if prompt.chars().count() > MAX_CASE_PROMPT_CHARS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "worker {} prompt must be at most {} characters",
                    i + 1,
                    MAX_CASE_PROMPT_CHARS
                ),
            )
                .into_response();
        }
        let label = case
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("worker")
            .to_owned();
        if label.chars().count() > MAX_LABEL_CHARS {
            return (
                StatusCode::BAD_REQUEST,
                format!("worker {} label is too long", i + 1),
            )
                .into_response();
        }
        cases.push(json!({"label": label, "impl_prompt": prompt}));
    }
    // Escaping/injection scan on each submitted case: the dialog text is
    // untrusted DATA exactly like every agent output.
    let markers = prompts::injection_markers(&json!({ "cases": cases }));
    if !markers.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            format!("injection markers detected at {}", markers.join(",")),
        )
            .into_response();
    }
    let brief = body
        .brief
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| {
            cases[0]
                .get("impl_prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        });
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
    // Review F3 #1/#2: per-IP and process-wide run-creation limits apply to
    // the confirm just like they did to the old form submit.
    let ip = client_ip(&headers, ConnectInfo(info));
    if let Err(rejection) = state.limiter.check(&ip, &state.store).await {
        return too_many_requests_json(&rejection);
    }
    // F24: the submitted cases DRIVE the pipeline — one execution path with
    // decomposed runs (run row + lanes + slot accounting + start/enqueue).
    match state.start_pipeline_cases(brief, cases).await {
        Ok(run) => {
            state.limiter.record(&ip, &run.id);
            (
                StatusCode::CREATED,
                Json(json!({
                    "run_id": run.id,
                    "n_lanes": run.n_lanes,
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Review F3 #2: 429 + Retry-After as JSON (the dialog fetch reads the error
/// field and shows it in the dialog).
fn too_many_requests_json(rejection: &Rejection) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        Json(json!({ "error": rejection.message() })),
    )
        .into_response()
}

/// Review F3 #1: best-effort client IP for the per-IP run-creation limit.
/// ONLY `Fly-Client-IP` is trusted: Fly's edge proxy sets it from the real
/// connection, so clients cannot spoof it. Raw `X-Forwarded-For` is never
/// trusted — any client can forge it and rotate the per-IP limit. Without
/// the header (local dev) the socket address is used when the service was
/// made with connect info, else a shared "unknown" bucket.
pub(crate) fn client_ip(headers: &HeaderMap, info: ConnectInfo<SocketAddr>) -> String {
    if let Some(value) = headers.get("fly-client-ip").and_then(|v| v.to_str().ok()) {
        if let Some(first) = value.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() {
                return ip.to_owned();
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
    /// F25: lane count column (run.n_lanes).
    pub n_lanes: i64,
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
                    // F25: the id column is gone; the brief column got the
                    // freed space and clamps to two lines in the template.
                    brief: r.brief,
                    status: r.status,
                    n_lanes: r.n_lanes,
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
    pub val_features: Vec<String>,
    pub val_bugs: Vec<String>,
    pub val_security: Vec<String>,
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
            val_features: Vec::new(),
            val_bugs: Vec::new(),
            val_security: Vec::new(),
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

/// Hydrate the persisted validator report (features/bugs/security) for the
/// lane detail view; falls back to empty lists when absent. Legacy
/// string-shaped entries are repaired; placeholder text ("see array") is
/// dropped (F18).
fn validator_report_view(lane: &store::Lane) -> (Vec<String>, Vec<String>, Vec<String>) {
    lane.val_report_json
        .as_deref()
        .and_then(prompts::parse_validator_report)
        .map(|out| (out.features, out.bugs, out.security))
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "gallery.html")]
pub struct GalleryTemplate {
    pub run_id: String,
    /// F25: the FULL user prompt, shown prominently at the top of the page.
    pub brief: String,
    pub events_url: String,
    pub snapshot_url: String,
    pub status: String,
    /// Review F3 #3: CSRF token (also set as a cookie on this GET) for the
    /// island's launch dialog.
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
    // cards fill in without visiting each lane page first — and lazily
    // revives lanes whose public URL died with the sprite's sleep (F30).
    if let Ok(lanes) = state.store.list_lanes(&run.id).await {
        for lane in &lanes {
            enqueue_pending_render(&state.screenshots, &run.id, lane);
            spawn_lane_revive(&state, &run.id, lane);
        }
    }
    let token = csrf::new_token();
    let tpl = GalleryTemplate {
        run_id: run.id.clone(),
        brief: run.brief.clone(),
        events_url: format!("/{}/events", run.id),
        snapshot_url: format!("/api/runs/{}", run.id),
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
    // #3: viewing a lane also triggers the lazy screenshot render — and the
    // lazy lane revival (F30) when its public URL went dead.
    enqueue_pending_render(&state.screenshots, &run_id, &lane);
    spawn_lane_revive(&state, &run_id, &lane);
    HtmlResponse(lane_tpl.render().unwrap_or_default()).into_response()
}

/// F30: lazy lane revival. Spawned in the background so the page render is
/// never blocked: the revive module probes the lane's public URL, and when a
/// previously-good URL is dead it execs the lane's stored run_command
/// detached nohup-style on the lane sprite, waits, and re-verifies the 200
/// (logging `lane_revived` / `lane_revive_failed` lane events). Attempts are
/// capped at one per lane per hour inside the revive module. Skipped for
/// test/fixture lanes and for lanes whose implementer phase never passed the
/// 200-gate (only previously-good URLs are revived).
fn spawn_lane_revive(state: &AppState, run_id: &str, lane: &Lane) {
    if state.config.test_mode || state.fixtures {
        return;
    }
    let (Some(sprite_name), Some(url)) = (&lane.sprite_name, &lane.impl_url) else {
        return;
    };
    if !matches!(lane.state.as_str(), "done" | "validated" | "judged") {
        return;
    }
    let store = state.store.clone();
    let sprite = state.sprite.clone();
    let allow_loopback = state.config.probe_allow_loopback;
    let rid = run_id.to_owned();
    let idx = lane.idx;
    let sprite_name = sprite_name.clone();
    let url = url.clone();
    tokio::spawn(async move {
        let workdir = orchestrator::pipeline::sprite_workdir(&rid, idx);
        let _ = orchestrator::revive::revive_lane(
            &store,
            &sprite,
            &rid,
            idx,
            &sprite_name,
            &url,
            &workdir,
            allow_loopback,
        )
        .await;
    });
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

#[cfg(test)]
mod client_ip_tests {
    #![allow(clippy::unwrap_used)] // test code: panics on failure are the assertion mechanism

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn socket() -> ConnectInfo<SocketAddr> {
        ConnectInfo("10.1.2.3:4321".parse().unwrap())
    }

    /// Security review F36: a spoofed `X-Forwarded-For` must be ignored —
    /// trusting it let a client rotate the per-IP run limit for free.
    #[test]
    fn spoofed_x_forwarded_for_is_ignored() {
        let h = headers(&[("x-forwarded-for", "6.6.6.6, 7.7.7.7")]);
        assert_eq!(client_ip(&h, socket()), "10.1.2.3");
    }

    /// `fly-client-ip` is edge-set (unspoofable) and wins over anything else.
    #[test]
    fn fly_client_ip_wins() {
        let h = headers(&[("x-forwarded-for", "6.6.6.6"), ("fly-client-ip", "5.5.5.5")]);
        assert_eq!(client_ip(&h, socket()), "5.5.5.5");
    }

    /// Local dev has no proxy headers: the socket address is the identity.
    #[test]
    fn socket_fallback_without_headers() {
        assert_eq!(client_ip(&headers(&[]), socket()), "10.1.2.3");
    }
}
