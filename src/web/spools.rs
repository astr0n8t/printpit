//! The Spools tab: which Spoolman spool is loaded in each of a printer's tools, filament
//! tracking on/off, and the bookings printpit made from the printer's job history.
//!
//! The tab renders from the database. The spool picker loads separately and asks Spoolman for
//! its spool list. Neither contacts the printer; only `sync now` and saving the loaded spools
//! read its history.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{Admin, ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::audit;
use crate::db::spoolman::{self as store, Booking, MAX_TOOLS};
use crate::error::AppError;
use crate::spoolman::client::Spool;
use crate::spoolman::{Spoolman, SyncError, describe_part};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::widgets;

const RECENT: i64 = 30;

/// 404 for every Spoolman route when it isn't configured.
fn spoolman(state: &AppState) -> Result<Arc<Spoolman>, AppError> {
    state.spoolman.clone().ok_or(AppError::NotFound)
}

struct BookingRow {
    filename: String,
    ended: String,
    used: String,
    label: &'static str,
    class: &'static str,
    parts: Vec<String>,
    note: String,
    error: Option<String>,
}

impl BookingRow {
    fn new(b: Booking, multi: bool) -> Self {
        let (label, class) = match b.status.as_str() {
            "booked" => ("BOOKED", "badge-ok"),
            "skipped" => ("SKIPPED", "badge-dim"),
            "failed" => ("RETRYING", "badge-warn"),
            "sending" => ("SENDING", "badge-info"),
            "unknown" => ("UNKNOWN", "badge-warn"),
            _ => ("FAILED", "badge-err"),
        };
        Self {
            parts: b
                .parts
                .iter()
                .map(|p| {
                    let s = describe_part(p, multi);
                    if p.done { s } else { format!("{s} (not booked)") }
                })
                .collect(),
            ended: b.job_end.map(widgets::age).unwrap_or_else(|| "—".into()),
            used: format!("{:.2} m", b.used_mm / 1000.0),
            filename: b.filename,
            label,
            class,
            note: b.note,
            error: b.error,
        }
    }
}

#[derive(Template)]
#[template(path = "tabs/spools.html")]
struct SpoolsTab {
    select: Option<String>,
    id: i64,
    notice: Option<String>,
    error: Option<String>,
    tracking: bool,
    tools: i64,
    max_tools: i64,
    since: String,
    last_sync: Option<String>,
    sync_error: Option<String>,
    is_admin: bool,
    is_operator: bool,
    bookings: Vec<BookingRow>,
}

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    spoolman(&state)?;
    let html = render_tab(&state, &user, q.printer, None, None).await?;
    page(hx, &user, Tab::Spools, html)
}

async fn render_tab(
    state: &AppState,
    user: &CurrentUser,
    printer: Option<i64>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<String, AppError> {
    let mut t = SpoolsTab {
        select: None,
        id: 0,
        notice,
        error,
        tracking: false,
        tools: 1,
        max_tools: MAX_TOOLS,
        since: String::new(),
        last_sync: None,
        sync_error: None,
        is_admin: user.has(Role::Admin),
        is_operator: user.has(Role::Operator),
        bookings: Vec::new(),
    };
    if let Some(sel) = select::select(state, user.id, printer, "/ui/spools").await? {
        let id = sel.printer.id;
        let s = store::settings(&state.db, id).await?;
        t.select = Some(sel.select.render()?);
        t.id = id;
        t.tracking = s.tracking;
        t.tools = s.tools;
        t.since = widgets::age(s.since);
        t.last_sync = s.synced_at.map(|at| widgets::age(at as f64));
        t.sync_error = s.sync_error;
        t.bookings = store::recent(&state.db, id, RECENT)
            .await?
            .into_iter()
            .map(|b| BookingRow::new(b, s.tools > 1))
            .collect();
    }
    Ok(t.render()?)
}

struct SlotRow {
    tool: usize,
    label: String,
    selected: Option<i64>,
    /// The loaded spool's description, or why it can't be shown.
    current: String,
}

#[derive(Template)]
#[template(path = "partials/spool_slots.html")]
struct SlotsPanel {
    id: i64,
    slots: Vec<SlotRow>,
    /// `(id, "label · summary")`
    spools: Vec<(i64, String)>,
    error: Option<String>,
    can_edit: bool,
}

impl SlotsPanel {
    fn is_selected(&self, slot: &SlotRow, spool: &i64) -> bool {
        slot.selected == Some(*spool)
    }

    /// A loaded spool that isn't in Spoolman's active list (archived or deleted) still needs an
    /// option, or saving the form would unload it.
    fn missing(&self, slot: &SlotRow) -> Option<i64> {
        slot.selected
            .filter(|s| !self.spools.iter().any(|(id, _)| id == s))
    }
}

/// The loaded spools, with Spoolman's list to choose from.
pub async fn slots(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let sp = spoolman(&state)?;
    enabled_printer(&state, id).await?;
    let settings = store::settings(&state.db, id).await?;
    let loaded = store::slots(&state.db, id, settings.tools).await?;
    let (spools, error) = match sp.client.spools().await {
        Ok(list) => (list, None),
        Err(e) => (Vec::new(), Some(format!("{}: {}", e.label(), e.detail()))),
    };
    let by_id: HashMap<i64, &Spool> = spools.iter().map(|s| (s.id, s)).collect();
    let multi = loaded.len() > 1;
    let rows = loaded
        .iter()
        .enumerate()
        .map(|(tool, spool)| SlotRow {
            tool,
            label: if multi { format!("T{tool}") } else { "spool".into() },
            selected: *spool,
            current: match spool {
                None => "empty".into(),
                Some(s) => match by_id.get(s) {
                    Some(sp) => [sp.label(), sp.summary()]
                        .into_iter()
                        .filter(|x| !x.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                    None if error.is_some() => format!("#{s}"),
                    None => format!("#{s} (archived or not in Spoolman)"),
                },
            },
        })
        .collect();
    let panel = SlotsPanel {
        id,
        slots: rows,
        spools: spools
            .iter()
            .map(|s| {
                let summary = s.summary();
                let label = if summary.is_empty() {
                    s.label()
                } else {
                    format!("{} · {summary}", s.label())
                };
                (s.id, label)
            })
            .collect(),
        can_edit: user.has(Role::Operator) && error.is_none(),
        error,
    };
    Ok(Html(panel.render()?).into_response())
}

/// `tool0=12&tool1=` → the spool in each tool.
pub async fn assign(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let sp = spoolman(&state)?;
    let printer = enabled_printer(&state, id).await?;
    let settings = store::settings(&state.db, id).await?;
    let mut slots = Vec::new();
    for tool in 0..settings.tools {
        let raw = form
            .get(&format!("tool{tool}"))
            .map(|v| v.trim())
            .unwrap_or_default();
        slots.push(match raw {
            "" => None,
            v => Some(
                v.parse::<i64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| AppError::BadRequest("That isn't a spool id.".into()))?,
            ),
        });
    }
    let before = store::slots(&state.db, id, settings.tools).await?;
    let sync_error = sp.assign(id, &slots).await?;
    let describe = |slots: &[Option<i64>]| {
        slots
            .iter()
            .enumerate()
            .map(|(t, s)| format!("T{t}={}", s.map_or_else(|| "-".into(), |s| format!("#{s}"))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "spoolman_assign",
            detail: Some(format!("{} (was {})", describe(&slots), describe(&before))),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    let (notice, error) = match sync_error {
        None => (
            Some(format!("Saved the spools loaded in {}.", printer.name)),
            None,
        ),
        Some(e) => (
            Some(format!("Saved the spools loaded in {}.", printer.name)),
            Some(format!(
                "Couldn't book finished jobs to the previous spools first: {}. They'll go to the new ones.",
                e.detail()
            )),
        ),
    };
    let html = render_tab(&state, &user, Some(id), notice, error).await?;
    page(hx, &user, Tab::Spools, html)
}

pub async fn sync(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let sp = spoolman(&state)?;
    enabled_printer(&state, id).await?;
    let result = sp.sync(id).await;
    let detail = match &result {
        Ok(r) => format!("booked={} skipped={} failed={}", r.booked, r.skipped, r.failed),
        Err(e) => format!("error: {}", e.detail()),
    };
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "spoolman_sync",
            detail: Some(detail),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    let (notice, error) = match result {
        Ok(r) => (Some(r.describe()), None),
        Err(SyncError::Db(e)) => return Err(e.into()),
        Err(e) => (None, Some(e.detail())),
    };
    let html = render_tab(&state, &user, Some(id), notice, error).await?;
    page(hx, &user, Tab::Spools, html)
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    tracking: Option<String>,
    #[serde(default)]
    tools: String,
}

pub async fn save_settings(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<SettingsForm>,
) -> Result<Response, AppError> {
    let sp = spoolman(&state)?;
    let printer = enabled_printer(&state, id).await?;
    let tools = f
        .tools
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|n| (1..=MAX_TOOLS).contains(n))
        .ok_or_else(|| AppError::BadRequest(format!("Tools must be a number from 1 to {MAX_TOOLS}.")))?;
    let tracking = f.tracking.is_some();
    let turned_on = store::set_settings(&state.db, id, tracking, tools).await?;
    sp.sync_watches().await;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "spoolman_settings",
            detail: Some(format!(
                "tracking={} tools={tools}",
                if tracking { "on" } else { "off" }
            )),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    let notice = match (tracking, turned_on) {
        (true, true) => format!(
            "Tracking filament for {}. Jobs that start from now on are booked to Spoolman.",
            printer.name
        ),
        (true, false) => format!("Saved {}'s filament tracking.", printer.name),
        (false, _) => format!("Filament tracking is off for {}.", printer.name),
    };
    let html = render_tab(&state, &user, Some(id), Some(notice), None).await?;
    page(hx, &user, Tab::Spools, html)
}
