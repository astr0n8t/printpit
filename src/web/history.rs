//! The History tab: a printer's past jobs from Moonraker's `[history]` component.
//!
//! Opening the tab makes no printer call; the list (one page plus the totals) loads when its
//! box loads, and a job's details only when the user picks it.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{CurrentUser, Role};
use crate::error::AppError;
use crate::moonraker::MoonrakerError;
use crate::moonraker::types::{HistoryJob, JobTotals};
use crate::state::AppState;
use crate::web::files::{facts, thumb_url};
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::widgets;

pub const PAGE: u64 = 50;

#[derive(Template)]
#[template(path = "tabs/history.html")]
struct HistoryTab {
    select: Option<String>,
    list_url: String,
}

struct JobRow {
    uid: String,
    filename: String,
    label: String,
    class: &'static str,
    ended: String,
    duration: String,
    filament: String,
}

#[derive(Template)]
#[template(path = "partials/history_list.html")]
struct HistoryList {
    id: i64,
    jobs: Vec<JobRow>,
    totals: Option<String>,
    error: Option<String>,
    /// "51–60 of 60"
    range: String,
    prev: Option<u64>,
    next: Option<u64>,
    details: Option<String>,
}

impl HistoryList {
    fn job_url(&self, uid: &str) -> String {
        format!("/p/{}/history/job?uid={uid}", self.id)
    }

    fn push_url(&self, uid: &str) -> String {
        format!("/ui/history?printer={}&job={uid}", self.id)
    }

    fn page_url(&self, start: &u64) -> String {
        format!("/p/{}/history?start={start}", self.id)
    }
}

#[derive(Template)]
#[template(path = "partials/history_job.html")]
struct JobDetails {
    id: i64,
    filename: String,
    label: String,
    class: &'static str,
    facts: Vec<(&'static str, String)>,
    thumbnail: Option<String>,
    reprint: bool,
    missing: bool,
    error: Option<String>,
}

/// Moonraker job ids are short hex strings (`000012`); nothing else is sent upstream.
fn valid_uid(uid: &str) -> bool {
    !uid.is_empty() && uid.len() <= 32 && uid.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Badge for a job status: `klippy_shutdown` → `KLIPPY SHUTDOWN` in red, and so on.
pub fn status_badge(status: &str) -> (String, &'static str) {
    let class = match status {
        "completed" => "badge-ok",
        "in_progress" => "badge-info",
        "cancelled" => "badge-dim",
        _ => "badge-err",
    };
    let label = match status {
        "in_progress" => "PRINTING".to_string(),
        "" => "UNKNOWN".to_string(),
        s => s.replace('_', " ").to_uppercase(),
    };
    (label, class)
}

fn metres(mm: f64) -> String {
    format!("{:.1} m", mm / 1000.0)
}

fn totals_line(t: &JobTotals) -> String {
    format!(
        "{} jobs · {} printing · {} filament · longest {}",
        t.total_jobs,
        widgets::duration(t.total_print_time),
        metres(t.total_filament_used),
        widgets::duration(t.longest_print)
    )
}

/// A 404 from `/server/history/*` means the component isn't configured.
fn describe(e: &MoonrakerError) -> String {
    match e {
        MoonrakerError::Http { status: 404, .. } => {
            "Job history is off on this printer. Add a [history] section to moonraker.conf.".into()
        }
        e => format!("{}: {}", e.label(), e.detail()),
    }
}

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
    job: Option<String>,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    let html = match select::select(&state, user.id, q.printer, "/ui/history").await? {
        None => HistoryTab {
            select: None,
            list_url: String::new(),
        },
        Some(sel) => {
            let mut list_url = format!("/p/{}/history", sel.printer.id);
            if let Some(job) = q.job.filter(|j| valid_uid(j)) {
                list_url = format!("{list_url}?job={job}");
            }
            HistoryTab {
                select: Some(sel.select.render()?),
                list_url,
            }
        }
    }
    .render()?;
    page(hx, &user, Tab::History, html)
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    start: u64,
    job: Option<String>,
}

/// One page of jobs and the totals, plus a deep-linked job's details.
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let client = state.printer_client(&p);
    let (listing, totals) = match &client {
        Ok(c) => {
            let (l, t) = tokio::join!(c.history(q.start, PAGE), c.history_totals());
            (l, t.ok())
        }
        Err(e) => (Err(e.clone()), None),
    };
    let mut out = HistoryList {
        id,
        jobs: Vec::new(),
        totals: totals.as_ref().map(totals_line),
        error: None,
        range: String::new(),
        prev: None,
        next: None,
        details: None,
    };
    match listing {
        Ok(l) => {
            let shown = l.jobs.len() as u64;
            if shown > 0 {
                out.range = format!("{}–{} of {}", q.start + 1, q.start + shown, l.count);
            }
            out.prev = (q.start > 0).then(|| q.start.saturating_sub(PAGE));
            out.next = (q.start + shown < l.count).then_some(q.start + shown);
            out.jobs = l
                .jobs
                .into_iter()
                .filter(|j| valid_uid(&j.job_id))
                .map(|j| {
                    let (label, class) = status_badge(&j.status);
                    JobRow {
                        ended: j.end_time.map(widgets::age).unwrap_or_else(|| "—".into()),
                        duration: widgets::duration(j.print_duration),
                        filament: metres(j.filament_used),
                        uid: j.job_id,
                        filename: j.filename,
                        label,
                        class,
                    }
                })
                .collect();
        }
        Err(e) => out.error = Some(describe(&e)),
    }
    if out.error.is_none()
        && let (Ok(c), Some(uid)) = (&client, q.job.filter(|j| valid_uid(j)))
    {
        out.details = Some(details(id, c.history_job(&uid).await, user.has(Role::Operator)).render()?);
    }
    Ok(Html(out.render()?).into_response())
}

#[derive(Deserialize, Default)]
pub struct JobQuery {
    #[serde(default)]
    uid: String,
}

pub async fn job(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<JobQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    if !valid_uid(&q.uid) {
        return Err(AppError::BadRequest("That job id isn't valid.".into()));
    }
    let job = match state.printer_client(&p) {
        Ok(c) => c.history_job(&q.uid).await,
        Err(e) => Err(e),
    };
    Ok(Html(details(id, job, user.has(Role::Operator)).render()?).into_response())
}

fn details(id: i64, job: Result<HistoryJob, MoonrakerError>, can_print: bool) -> JobDetails {
    match job {
        Ok(j) => {
            let (label, class) = status_badge(&j.status);
            let mut f = vec![
                ("started", widgets::age(j.start_time)),
                (
                    "ended",
                    j.end_time.map(widgets::age).unwrap_or_else(|| "—".into()),
                ),
                ("printing", widgets::duration(j.print_duration)),
                ("total", widgets::duration(j.total_duration)),
                ("filament", metres(j.filament_used)),
            ];
            // The file's own size/modified describe today's file, not the job.
            f.extend(
                facts(&j.metadata)
                    .into_iter()
                    .filter(|(label, _)| !matches!(*label, "size" | "modified" | "filament")),
            );
            JobDetails {
                id,
                thumbnail: j
                    .exists
                    .then(|| thumb_url(id, &j.filename, &j.metadata))
                    .flatten(),
                reprint: can_print && j.exists,
                missing: !j.exists,
                filename: j.filename,
                label,
                class,
                facts: f,
                error: None,
            }
        }
        Err(e) => JobDetails {
            id,
            filename: String::new(),
            label: String::new(),
            class: "",
            facts: Vec::new(),
            thumbnail: None,
            reprint: false,
            missing: false,
            error: Some(match e {
                MoonrakerError::Http { status: 404, .. } => "That job isn't in the history.".into(),
                e => describe(&e),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges() {
        assert_eq!(status_badge("completed"), ("COMPLETED".into(), "badge-ok"));
        assert_eq!(
            status_badge("klippy_shutdown"),
            ("KLIPPY SHUTDOWN".into(), "badge-err")
        );
        assert_eq!(status_badge("in_progress"), ("PRINTING".into(), "badge-info"));
        assert_eq!(status_badge("cancelled").1, "badge-dim");
    }

    #[test]
    fn uids() {
        assert!(valid_uid("00001A"));
        for bad in ["", "1&limit=5", "../x", "a b", &"f".repeat(33)] {
            assert!(!valid_uid(bad), "{bad}");
        }
    }
}
