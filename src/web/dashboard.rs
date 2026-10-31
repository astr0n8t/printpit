//! The dashboard: printer cards, live updates over SSE, manual refresh, core print controls and
//! the camera proxy.
//!
//! Lazy-loading contract: rendering the dashboard page itself never contacts a printer. Live
//! data flows only while a browser holds `/ui/dashboard/stream` open (and only if the user's
//! auto-refresh pref is on); otherwise only explicit `/p/{id}/status` requests touch Moonraker.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::Form;
use axum::body::Body;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use futures_util::StreamExt;
use futures_util::stream::select_all;
use serde::Deserialize;
use tokio_stream::wrappers::WatchStream;

use crate::auth::session::still_valid;
use crate::auth::{ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::printers::{self, Printer};
use crate::db::{audit, prefs};
use crate::error::AppError;
use crate::moonraker::hub::poll_once;
use crate::moonraker::status::{Link, Snapshot, current_file};
use crate::moonraker::{MoonrakerClient, MoonrakerError};
use crate::state::AppState;
use crate::web::card;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::enabled_printer;
use crate::web::widgets;

/// How often an open stream re-checks that its session is still valid.
const SESSION_RECHECK: Duration = Duration::from_secs(60);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_MAX_BYTES: usize = 8 << 20;
const PICKER_FILES: usize = 25;

#[derive(Template)]
#[template(path = "tabs/dashboard.html")]
struct DashboardTemplate {
    printers: Vec<(i64, String)>,
    auto_refresh: bool,
    is_admin: bool,
}

#[derive(Template)]
#[template(path = "partials/printer_test.html")]
pub(crate) struct Flash {
    label: String,
    class: &'static str,
    detail: String,
}

impl Flash {
    pub(crate) fn ok(detail: impl Into<String>) -> Self {
        Self {
            label: "OK".into(),
            class: "badge-ok",
            detail: detail.into(),
        }
    }

    pub(crate) fn error(e: &MoonrakerError) -> Self {
        Self {
            label: e.label().into(),
            class: e.class(),
            detail: e.detail(),
        }
    }
}

struct PickerFile {
    path: String,
    size: String,
    age: String,
}

#[derive(Template)]
#[template(path = "partials/start_picker.html")]
struct StartPicker {
    id: i64,
    name: String,
    files: Vec<PickerFile>,
    error: Option<String>,
}

async fn dashboard_page(state: &AppState, hx: HxRequest, user: &CurrentUser) -> Result<Response, AppError> {
    let printers = printers::list(&state.db)
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| (p.id, p.name))
        .collect();
    let html = DashboardTemplate {
        printers,
        auto_refresh: prefs::get(&state.db, user.id).await?.auto_refresh,
        is_admin: user.has(Role::Admin),
    }
    .render()?;
    page(hx, user, Tab::Dashboard, html)
}

pub async fn show(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
) -> Result<Response, AppError> {
    dashboard_page(&state, hx, &user).await
}

#[derive(Deserialize)]
pub struct PrefsForm {
    auto_refresh: Option<String>,
}

/// Saves the auto-refresh checkbox and re-renders the dashboard, which starts or stops the
/// live stream accordingly.
pub async fn set_prefs(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    form: Result<Form<PrefsForm>, FormRejection>,
) -> Result<Response, AppError> {
    let on = form.ok().and_then(|Form(f)| f.auto_refresh).is_some();
    prefs::set_auto_refresh(&state.db, user.id, on).await?;
    if hx.0 {
        dashboard_page(&state, hx, &user).await
    } else {
        Ok(Redirect::to("/ui/dashboard").into_response())
    }
}

enum Tick {
    Update(i64, String, std::sync::Arc<Snapshot>),
    Recheck,
    End,
}

/// One multiplexed SSE stream for all cards (browsers allow only ~6 connections per host).
/// Each event's data is `<target element id>\n<html>`.
pub async fn stream(State(state): State<AppState>, user: CurrentUser) -> Result<Response, AppError> {
    if !prefs::get(&state.db, user.id).await?.auto_refresh {
        // 204 tells EventSource to stop for good.
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let can_control = user.has(Role::Operator);
    let enabled: Vec<Printer> = printers::list(&state.db)
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .collect();
    let mut updates = select_all(enabled.into_iter().map(|p| {
        let sub = state.hub.subscribe(p.id);
        let rx = sub.rx.clone();
        // The subscription lives as long as this per-printer stream; dropping the response
        // (browser gone) drops it and releases the viewer.
        WatchStream::new(rx).map(move |snap| {
            let _keep = &sub;
            (p.id, p.name.clone(), snap)
        })
    }));

    let events = async_stream::stream! {
        let mut last_controls: HashMap<i64, String> = HashMap::new();
        let mut recheck = tokio::time::interval(SESSION_RECHECK);
        recheck.tick().await;
        loop {
            let tick = tokio::select! {
                item = updates.next() => match item {
                    Some((id, name, snap)) => Tick::Update(id, name, snap),
                    None => Tick::End,
                },
                _ = recheck.tick() => Tick::Recheck,
            };
            match tick {
                Tick::End => break,
                Tick::Recheck => {
                    if !still_valid(&state, &user.session_hash).await {
                        break;
                    }
                }
                Tick::Update(id, name, snap) => {
                    let card = card::build(id, &name, &snap, can_control, false);
                    if let Ok(html) = card.live.render() {
                        yield Ok::<_, Infallible>(Event::default().data(format!("live-{id}\n{html}")));
                    }
                    if let Ok(html) = card.controls.render()
                        && last_controls.get(&id) != Some(&html)
                    {
                        yield Ok(Event::default().data(format!("ctl-{id}\n{html}")));
                        last_controls.insert(id, html);
                    }
                }
            }
        }
    };
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}

/// Manual refresh: one HTTP snapshot, rendered as the live part plus the controls (out of band).
pub async fn status(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let (link, status, estimated_time) = match state.printer_client(&p) {
        Ok(client) => match poll_once(&client).await {
            Ok((link, status)) => {
                let estimate = slicer_estimate(&state, id, &client, &status).await;
                (link, status, estimate)
            }
            Err(e) => (Link::Down(e), Default::default(), None),
        },
        Err(e) => (Link::Down(e), Default::default(), None),
    };
    let snap = Snapshot {
        link,
        status,
        history: Default::default(),
        estimated_time,
    };
    let card = card::build(id, &p.name, &snap, user.has(Role::Operator), true);
    let html = format!(
        "{}<div id=\"ctl-{id}\" class=\"controls\" hx-swap-oob=\"true\">{}</div>",
        card.live.render()?,
        card.controls.render()?
    );
    Ok(Html(html).into_response())
}

/// The slicer estimate for the loaded file, from the shared cache or one metadata call.
async fn slicer_estimate(
    state: &AppState,
    id: i64,
    client: &MoonrakerClient,
    status: &serde_json::Map<String, serde_json::Value>,
) -> Option<f64> {
    let file = current_file(status)?;
    if let Some(estimate) = state.slicer.get(id, file) {
        return estimate;
    }
    match client.metadata(file).await {
        Ok(m) => {
            state.slicer.insert(id, file, m.estimated_time);
            m.estimated_time
        }
        Err(e) => {
            tracing::debug!(printer_id = id, error = %e, "no slicer metadata");
            None
        }
    }
}

/// The start-print picker: most recently modified G-code files, rendered into the modal.
pub async fn start_picker(
    State(state): State<AppState>,
    _user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let listing = match state.printer_client(&p) {
        Ok(client) => client.gcode_files().await,
        Err(e) => Err(e),
    };
    let (files, error) = match listing {
        Ok(mut files) => {
            files.sort_by(|a, b| b.modified.total_cmp(&a.modified));
            let files = files
                .into_iter()
                .take(PICKER_FILES)
                .map(|f| PickerFile {
                    size: widgets::human_size(f.size),
                    age: widgets::age(f.modified),
                    path: f.path,
                })
                .collect();
            (files, None)
        }
        Err(e) => (Vec::new(), Some(format!("{}: {}", e.label(), e.detail()))),
    };
    let html = StartPicker {
        id,
        name: p.name,
        files,
        error,
    }
    .render()?;
    Ok(Html(html).into_response())
}

#[derive(Deserialize, Default)]
pub struct ActionForm {
    #[serde(default)]
    filename: String,
}

fn valid_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && !name.starts_with('/')
        && !name.split('/').any(|seg| seg == ".." || seg.is_empty())
        && !name.chars().any(char::is_control)
}

/// A printer action: the Moonraker endpoint, its audit record and the success message.
struct Action<'a> {
    path: &'static str,
    query: Vec<(&'static str, &'a str)>,
    audit: &'static str,
    detail: Option<String>,
    done: &'static str,
}

async fn run_action(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    p: &Printer,
    action: Action<'_>,
) -> Result<Response, AppError> {
    let result = match state.printer_client(p) {
        Ok(client) => client.post_action(action.path, &action.query).await,
        Err(e) => Err(e),
    };
    let flash = match &result {
        Ok(()) => {
            audit::record(
                &state.db,
                audit::Entry {
                    user_id: Some(user.id),
                    printer_id: Some(p.id),
                    action: action.audit,
                    detail: action.detail,
                    ip: ip.to_string_opt(),
                },
            )
            .await;
            tracing::info!(user = %user.username, printer = %p.name, action = action.audit, "printer action");
            Flash::ok(action.done)
        }
        Err(e) => Flash::error(e),
    };
    let mut res = Html(flash.render()?).into_response();
    if result.is_ok() {
        res.headers_mut()
            .insert("hx-trigger", HeaderValue::from_static("printpit:close-modal"));
    }
    Ok(res)
}

pub async fn print_action(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path((id, action)): Path<(i64, String)>,
    form: Result<Form<ActionForm>, FormRejection>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let form = form.map(|Form(f)| f).unwrap_or_default();
    let (path, audit, done) = match action.as_str() {
        "pause" => ("/printer/print/pause", "print_pause", "Pause sent."),
        "resume" => ("/printer/print/resume", "print_resume", "Resume sent."),
        "cancel" => ("/printer/print/cancel", "print_cancel", "Cancel sent."),
        "start" => ("/printer/print/start", "print_start", "Print started."),
        _ => return Err(AppError::NotFound),
    };
    let (query, detail) = if action == "start" {
        if !valid_filename(&form.filename) {
            return Err(AppError::BadRequest("Pick a G-code file to print.".into()));
        }
        (
            vec![("filename", form.filename.as_str())],
            Some(format!("file={}", form.filename)),
        )
    } else {
        (Vec::new(), None)
    };
    let action = Action {
        path,
        query,
        audit,
        detail,
        done,
    };
    run_action(&state, ip, &user, &p, action).await
}

pub async fn estop(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let action = Action {
        path: "/printer/emergency_stop",
        query: Vec::new(),
        audit: "emergency_stop",
        detail: None,
        done: "Emergency stop sent. Klipper needs a FIRMWARE_RESTART.",
    };
    run_action(&state, ip, &user, &p, action).await
}

async fn camera_client(state: &AppState, id: i64) -> Result<(MoonrakerClient, Printer), Box<Response>> {
    let p = match enabled_printer(state, id).await {
        Ok(p) => p,
        Err(e) => return Err(Box::new(e.into_response())),
    };
    match state.printer_client(&p) {
        Ok(c) => Ok((c, p)),
        Err(_) => Err(Box::new(StatusCode::BAD_GATEWAY.into_response())),
    }
}

fn camera_error(e: &MoonrakerError) -> Response {
    tracing::debug!(error = %e, "camera request failed");
    StatusCode::BAD_GATEWAY.into_response()
}

/// Proxies one JPEG snapshot. 404 when the printer has no camera.
pub async fn camera_snapshot(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
) -> Response {
    let (client, p) = match camera_client(&state, id).await {
        Ok(v) => v,
        Err(res) => return *res,
    };
    let cam = match state.webcams.get(id, &client, p.webcam_override.as_deref()).await {
        Ok(cam) => cam,
        Err(e) => return camera_error(&e),
    };
    let Some(url) = cam.and_then(|c| c.snapshot) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let res = match client.fetch(url, Some(SNAPSHOT_TIMEOUT)).await {
        Ok(res) => res,
        Err(e) => return camera_error(&e),
    };
    let content_type = res.headers().get(header::CONTENT_TYPE).cloned();
    if !content_type
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("image/"))
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if res
        .content_length()
        .is_some_and(|len| len as usize > SNAPSHOT_MAX_BYTES)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut body = Vec::new();
    let mut chunks = res.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let Ok(chunk) = chunk else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        if body.len() + chunk.len() > SNAPSHOT_MAX_BYTES {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        body.extend_from_slice(&chunk);
    }
    let mut out = Response::new(Body::from(body));
    if let Some(ct) = content_type {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

/// Proxies the MJPEG stream for as long as the browser keeps it open. When the client goes
/// away, axum drops the body, which drops the upstream response and closes that connection.
pub async fn camera_stream(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
) -> Response {
    let (client, p) = match camera_client(&state, id).await {
        Ok(v) => v,
        Err(res) => return *res,
    };
    let cam = match state.webcams.get(id, &client, p.webcam_override.as_deref()).await {
        Ok(cam) => cam,
        Err(e) => return camera_error(&e),
    };
    let Some(url) = cam.and_then(|c| c.stream) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let res = match client.fetch(url, None).await {
        Ok(res) => res,
        Err(e) => return camera_error(&e),
    };
    let Some(content_type) = res
        .headers()
        .get(header::CONTENT_TYPE)
        .filter(|v| v.to_str().is_ok_and(|ct| ct.starts_with("multipart/")))
        .cloned()
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let mut out = Response::new(Body::from_stream(res.bytes_stream()));
    out.headers_mut().insert(header::CONTENT_TYPE, content_type);
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames() {
        assert!(valid_filename("benchy.gcode"));
        assert!(valid_filename("parts/clip v2.gcode"));
        assert!(!valid_filename(""));
        assert!(!valid_filename("/etc/passwd"));
        assert!(!valid_filename("../x.gcode"));
        assert!(!valid_filename("a//b.gcode"));
        assert!(!valid_filename("a\nb"));
    }
}
