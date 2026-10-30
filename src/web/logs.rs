//! The Logs tab: list the printer host's log files, tail one, follow it, download it.
//!
//! Opening the tab makes no printer call; the list loads when its box loads, and a log is
//! only read when the user picks it. Follow mode is an explicit per-view toggle (not the
//! remembered auto-refresh pref) and lives in the browser.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::CurrentUser;
use crate::error::AppError;
use crate::moonraker::files::{Tail, clean_path};
use crate::moonraker::{MoonrakerClient, MoonrakerError};
use crate::state::AppState;
use crate::web::files::attachment;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::widgets;

pub const LINE_CHOICES: [usize; 3] = [200, 500, 2000];
const DEFAULT_LINES: usize = 500;
const MIN_WINDOW: u64 = 64 << 10;
const MAX_WINDOW: u64 = 8 << 20;

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Template)]
#[template(path = "tabs/logs.html")]
struct LogsTab {
    select: Option<String>,
    list_url: String,
}

struct LogFile {
    name: String,
    size: String,
    modified: String,
}

#[derive(Template)]
#[template(path = "partials/log_list.html")]
struct LogList {
    id: i64,
    files: Vec<LogFile>,
    error: Option<String>,
    viewer: Option<String>,
}

impl LogList {
    fn view_url(&self, name: &str) -> String {
        format!("/p/{}/logs/view?name={}", self.id, urlencode(name))
    }

    fn push_url(&self, name: &str) -> String {
        format!("/ui/logs?printer={}&name={}", self.id, urlencode(name))
    }
}

#[derive(Template)]
#[template(path = "partials/log_viewer.html")]
struct LogViewer {
    name: String,
    tail_url: String,
    download_url: String,
    choices: [usize; 3],
    default_lines: usize,
}

struct Line {
    text: String,
    class: &'static str,
}

#[derive(Template)]
#[template(path = "partials/log_tail.html")]
struct LogTail {
    lines: Vec<Line>,
    shown: usize,
    size: String,
    error: Option<String>,
}

/// Colour hints: errors and tracebacks red, warnings yellow, Klipper's frequent `Stats` dim.
fn classify(line: &str) -> &'static str {
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("stats ") {
        "dim"
    } else if lower.contains("traceback")
        || lower.contains("error")
        || lower.contains("exception")
        || lower.starts_with("!!")
        || lower.contains("shutdown")
    {
        "err"
    } else if lower.contains("warn") {
        "warn"
    } else {
        ""
    }
}

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
    name: Option<String>,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    let selection = select::select(&state, user.id, q.printer, "/ui/logs").await?;
    let html = match selection {
        None => LogsTab {
            select: None,
            list_url: String::new(),
        },
        Some(sel) => {
            let mut list_url = format!("/p/{}/logs", sel.printer.id);
            if let Some(name) = q.name.as_deref().and_then(clean_path).filter(|n| !n.is_empty()) {
                list_url = format!("{list_url}?name={}", urlencode(&name));
            }
            LogsTab {
                select: Some(sel.select.render()?),
                list_url,
            }
        }
    }
    .render()?;
    page(hx, &user, Tab::Logs, html)
}

fn viewer(id: i64, name: &str) -> LogViewer {
    LogViewer {
        name: name.to_string(),
        tail_url: format!("/p/{id}/logs/tail?name={}", urlencode(name)),
        download_url: format!("/p/{id}/logs/download?name={}", urlencode(name)),
        choices: LINE_CHOICES,
        default_lines: DEFAULT_LINES,
    }
}

#[derive(Deserialize, Default)]
pub struct NameQuery {
    #[serde(default)]
    name: String,
}

/// Log names are root-relative; anything absolute-looking is rejected outright.
fn clean_name(name: &str) -> Result<String, AppError> {
    Some(name)
        .filter(|n| !n.starts_with('/'))
        .and_then(clean_path)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AppError::BadRequest("That log name isn't valid.".into()))
}

/// The log file list (one directory call), plus the viewer shell if a log was deep-linked.
pub async fn list(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<NameQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let listing = match state.printer_client(&p) {
        Ok(client) => client.directory("logs", "").await,
        Err(e) => Err(e),
    };
    let (files, error) = match listing {
        Ok(mut l) => {
            l.files.sort_by(|a, b| b.modified.total_cmp(&a.modified));
            let files = l
                .files
                .into_iter()
                .filter(|f| !f.filename.starts_with('.'))
                .map(|f| LogFile {
                    size: widgets::human_size(f.size),
                    modified: widgets::age(f.modified),
                    name: f.filename,
                })
                .collect();
            (files, None)
        }
        Err(e) => (Vec::new(), Some(format!("{}: {}", e.label(), e.detail()))),
    };
    let viewer = match clean_path(&q.name).filter(|n| !n.is_empty()) {
        Some(name) => Some(viewer(id, &name).render()?),
        None => None,
    };
    let html = LogList {
        id,
        files,
        error,
        viewer,
    }
    .render()?;
    Ok(Html(html).into_response())
}

/// The viewer shell (controls); its body loads the tail separately. No printer call here.
pub async fn view(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<NameQuery>,
) -> Result<Response, AppError> {
    enabled_printer(&state, id).await?;
    let name = clean_name(&q.name)?;
    Ok(Html(viewer(id, &name).render()?).into_response())
}

#[derive(Deserialize, Default)]
pub struct TailQuery {
    #[serde(default)]
    name: String,
    lines: Option<usize>,
}

/// Fetches enough of the file's end for `lines` lines: a window sized for ~256-byte lines,
/// widened once if that came up short.
async fn read_tail(client: &MoonrakerClient, name: &str, lines: usize) -> Result<Tail, MoonrakerError> {
    let window = (lines as u64 * 256).clamp(MIN_WINDOW, MAX_WINDOW);
    let tail = client.tail("logs", name, window).await?;
    if tail.truncated && tail.last_lines(lines).len() < lines && window < MAX_WINDOW {
        return client.tail("logs", name, (window * 4).min(MAX_WINDOW)).await;
    }
    Ok(tail)
}

pub async fn tail(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<TailQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let name = clean_name(&q.name)?;
    let lines = q
        .lines
        .filter(|n| LINE_CHOICES.contains(n))
        .unwrap_or(DEFAULT_LINES);
    let result = match state.printer_client(&p) {
        Ok(client) => read_tail(&client, &name, lines).await,
        Err(e) => Err(e),
    };
    let html = match result {
        Ok(t) => {
            let lines: Vec<Line> = t
                .last_lines(lines)
                .into_iter()
                .map(|text| Line {
                    class: classify(&text),
                    text,
                })
                .collect();
            LogTail {
                shown: lines.len(),
                lines,
                size: widgets::human_size(t.total),
                error: None,
            }
        }
        Err(e) => LogTail {
            lines: Vec::new(),
            shown: 0,
            size: String::new(),
            error: Some(format!("{}: {}", e.label(), e.detail())),
        },
    }
    .render()?;
    Ok(Html(html).into_response())
}

pub async fn download(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<NameQuery>,
) -> Response {
    let Ok(name) = clean_name(&q.name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let p = match enabled_printer(&state, id).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let client = match state.printer_client(&p) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.detail()).into_response(),
    };
    match client.download("logs", &name).await {
        Ok(res) => attachment(res, &name),
        Err(MoonrakerError::Http { status: 404, .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.detail()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_classes() {
        assert_eq!(classify("Stats 1234.5: gcodein=0 mcu: mcu_awake=0.002"), "dim");
        assert_eq!(classify("Traceback (most recent call last):"), "err");
        assert_eq!(classify("MCU 'mcu' shutdown: Timer too close"), "err");
        assert_eq!(classify("!! Must home axis first"), "err");
        assert_eq!(classify("[WARNING] something odd"), "warn");
        assert_eq!(classify("Starting Klippy..."), "");
    }
}
