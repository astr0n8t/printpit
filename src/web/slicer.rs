//! `[s]Slicer` (admin): the profile library and the slicer worker (PLAN §15.3).
//!
//! Only when slicing is configured; otherwise every route here is a 404 and the tab is hidden.
//! The tab itself renders from the database: the worker is contacted only by what an admin
//! clicks (`test`, browsing and importing system profiles, resolving uploaded presets).

use std::sync::Arc;

use askama::Template;
use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{Admin, ClientIp, RequireRole};
use crate::db::slicer::{self as store, DeleteProfile, NewProfile, PrinterSlicer, ProfileInfo, Source};
use crate::db::{audit, printers};
use crate::error::AppError;
use crate::slicer::client::{CompatEntry, VendorIndex};
use crate::slicer::import::{self, Fit, Parsed};
use crate::slicer::{Kind, Profile, SlicerError, Slicing};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::widgets;

/// Orca's shared filament library (the worker's vendor name for it).
const SHARED_VENDOR: &str = "OrcaFilamentLibrary";
/// Most profiles one system import may bring in.
const MAX_SYSTEM_IMPORT: usize = 200;

/// The slicing setup, or a 404 when slicing is off.
pub(crate) fn slicing(state: &AppState) -> Result<&Arc<Slicing>, AppError> {
    state.slicing.as_ref().ok_or(AppError::NotFound)
}

// ── the tab ──────────────────────────────────────────────────────────────────────────────

struct Group {
    title: &'static str,
    rows: Vec<Row>,
}

struct Row {
    id: i64,
    name: String,
    source: String,
    updated: String,
}

/// What an import did, one line per uploaded preset or chosen profile.
#[derive(Default)]
pub struct Report {
    lines: Vec<ReportLine>,
    skipped: Vec<String>,
}

struct ReportLine {
    source: String,
    outcome: &'static str,
    class: &'static str,
    detail: String,
}

impl Report {
    fn push(&mut self, source: impl Into<String>, result: Result<Stored, String>, kind: Option<Kind>) {
        let (outcome, class, detail) = match result {
            Ok(Stored::Created(name)) => ("ADDED", "badge-ok", format!("{} {name}", kind_name(kind))),
            Ok(Stored::Updated(name)) => ("UPDATED", "badge-info", format!("{} {name}", kind_name(kind))),
            Err(e) => ("SKIPPED", "badge-err", e),
        };
        self.lines.push(ReportLine {
            source: source.into(),
            outcome,
            class,
            detail,
        });
    }

    fn counts(&self) -> String {
        let n = |o: &str| self.lines.iter().filter(|l| l.outcome == o).count();
        format!(
            "{} added, {} updated, {} skipped",
            n("ADDED"),
            n("UPDATED"),
            n("SKIPPED")
        )
    }
}

fn kind_name(kind: Option<Kind>) -> &'static str {
    kind.map_or("profile", Kind::as_str)
}

#[derive(Template)]
#[template(path = "tabs/slicer.html")]
struct TabTemplate {
    groups: Vec<Group>,
    notice: Option<String>,
    error: Option<String>,
    report: Option<Report>,
    max_files: usize,
}

async fn tab_page(
    state: &AppState,
    hx: HxRequest,
    user: &RequireRole<Admin>,
    notice: Option<String>,
    error: Option<String>,
    report: Option<Report>,
) -> Result<Response, AppError> {
    let profiles = store::list_profiles(&state.db, None).await?;
    let mut groups: Vec<Group> = [
        (Kind::Machine, "machines"),
        (Kind::Process, "processes"),
        (Kind::Filament, "filaments"),
    ]
    .into_iter()
    .map(|(kind, title)| Group {
        title,
        rows: profiles
            .iter()
            .filter(|p| p.kind() == Some(kind))
            .map(|p| Row {
                id: p.id,
                name: p.name.clone(),
                source: source_label(p),
                updated: widgets::age(p.updated_at as f64),
            })
            .collect(),
    })
    .collect();
    groups.retain(|g| !g.rows.is_empty() || g.title == "machines");
    let html = TabTemplate {
        groups,
        notice,
        error,
        report,
        max_files: import::MAX_FILES,
    }
    .render()?;
    page(hx, user, Tab::Slicer, html)
}

fn source_label(p: &ProfileInfo) -> String {
    match (p.source.as_str(), &p.vendor) {
        ("system", Some(v)) => format!("orca: {v}"),
        ("system", None) => "orca".into(),
        _ => "imported".into(),
    }
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Admin>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    tab_page(&state, hx, &user, None, None, None).await
}

// ── test ─────────────────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/printer_test.html")]
struct TestResult {
    label: String,
    class: &'static str,
    detail: String,
}

impl From<SlicerError> for TestResult {
    fn from(e: SlicerError) -> Self {
        Self {
            label: e.label().into(),
            class: e.class(),
            detail: e.detail(),
        }
    }
}

pub async fn test(State(state): State<AppState>, _user: RequireRole<Admin>) -> Result<Response, AppError> {
    let s = slicing(&state)?;
    let result = match probe(s).await {
        Ok(r) => r,
        Err(e) => e.into(),
    };
    Ok(Html(result.render()?).into_response())
}

async fn probe(s: &Slicing) -> Result<TestResult, SlicerError> {
    let health = s.client.health().await?;
    let info = s.client.info().await?;
    let busy = if info.busy { ", busy slicing" } else { "" };
    Ok(TestResult {
        label: "OK".into(),
        class: "badge-ok",
        detail: format!(
            "OrcaSlicer {}, worker {}{busy}; {} vendors, models up to {} MB, {} s per slice",
            health.orca_version,
            info.worker_version,
            info.vendors.len(),
            info.max_model_mb,
            info.timeout_secs
        ),
    })
}

// ── system profiles ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SystemQuery {
    #[serde(default)]
    vendor: String,
    #[serde(default)]
    machine: String,
    /// Show every filament in Orca's shared library, not just the generic ones.
    #[serde(default)]
    all: Option<String>,
}

struct Choice {
    name: String,
    note: String,
    /// `?`: compatibility depends on a condition we don't evaluate.
    maybe: bool,
    have: bool,
}

#[derive(Template)]
#[template(path = "partials/slicer_system.html")]
struct SystemTemplate {
    vendors: Vec<String>,
    vendor: String,
    /// (value, label) pairs.
    machines: Vec<(String, String)>,
    machine: String,
    processes: Vec<Choice>,
    filaments: Vec<Choice>,
    shared_hidden: usize,
    all: bool,
    error: Option<TestResult>,
}

impl SystemTemplate {
    fn new() -> Self {
        Self {
            vendors: Vec::new(),
            vendor: String::new(),
            machines: Vec::new(),
            machine: String::new(),
            processes: Vec::new(),
            filaments: Vec::new(),
            shared_hidden: 0,
            all: false,
            error: None,
        }
    }

    fn enc(&self, s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }
}

/// The step-by-step picker: vendor → machine → processes and filaments. Each step is one
/// click, and only then is the worker asked.
pub async fn system(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Admin>,
    Query(q): Query<SystemQuery>,
) -> Result<Response, AppError> {
    let s = slicing(&state)?;
    let mut t = SystemTemplate::new();
    if let Err(e) = fill_system(&state, s, &q, &mut t).await {
        t.error = Some(e.into());
    }
    let html = t.render()?;
    if hx.0 {
        Ok(Html(html).into_response())
    } else {
        page(hx, &user, Tab::Slicer, html)
    }
}

async fn fill_system(
    state: &AppState,
    s: &Slicing,
    q: &SystemQuery,
    t: &mut SystemTemplate,
) -> Result<(), SlicerError> {
    let vendor = q.vendor.trim();
    if vendor.is_empty() {
        let info = s.client.info().await?;
        t.vendors = info
            .vendors
            .into_iter()
            .map(|v| v.name)
            .filter(|n| n != SHARED_VENDOR)
            .collect();
        return Ok(());
    }
    let index = s.client.vendor(vendor).await?;
    t.vendor = index.name.clone();
    let mut machines: Vec<(String, String)> = index
        .machines
        .iter()
        .map(|m| {
            let label = match (m.printer_model.is_empty(), m.nozzle.is_empty()) {
                (false, false) => format!("{} · {} mm nozzle", m.printer_model, m.nozzle),
                _ => m.name.clone(),
            };
            (m.name.clone(), label)
        })
        .collect();
    machines.sort_by(|a, b| a.1.cmp(&b.1));
    t.machines = machines;
    let machine = q.machine.trim();
    if machine.is_empty() {
        return Ok(());
    }
    if !index.machines.iter().any(|m| m.name == machine) {
        return Err(SlicerError::NotFound(format!(
            "{vendor} has no printer named {machine}"
        )));
    }
    t.machine = machine.to_string();
    t.all = q.all.is_some();
    let have = |kind: Kind, name: &str| {
        let db = state.db.clone();
        let name = name.to_string();
        async move {
            store::find_profile(&db, kind, &name)
                .await
                .ok()
                .flatten()
                .is_some()
        }
    };
    for p in &index.processes {
        if let Some(c) = choice(p, machine, &layer_note(p)) {
            t.processes.push(Choice {
                have: have(Kind::Process, &c.name).await,
                ..c
            });
        }
    }
    let mut filaments = fitting(&index, machine);
    if vendor != SHARED_VENDOR {
        let shared = s.client.vendor(SHARED_VENDOR).await?;
        for c in fitting(&shared, machine) {
            if t.all || c.name.starts_with("Generic ") {
                filaments.push(c);
            } else {
                t.shared_hidden += 1;
            }
        }
    }
    for c in filaments {
        t.filaments.push(Choice {
            have: have(Kind::Filament, &c.name).await,
            ..c
        });
    }
    Ok(())
}

fn fitting(index: &VendorIndex, machine: &str) -> Vec<Choice> {
    index
        .filaments
        .iter()
        .filter_map(|f| choice(f, machine, &f.filament_type))
        .collect()
}

fn choice(e: &CompatEntry, machine: &str, note: &str) -> Option<Choice> {
    let fit = import::fit(&e.compatible_printers, &e.compatible_printers_condition, machine);
    (fit != Fit::No).then(|| Choice {
        name: e.name.clone(),
        note: note.to_string(),
        maybe: fit == Fit::Maybe,
        have: false,
    })
}

fn layer_note(e: &CompatEntry) -> String {
    if e.layer_height.is_empty() {
        String::new()
    } else {
        format!("{} mm layers", e.layer_height)
    }
}

/// Imports the chosen machine and the ticked processes and filaments, each flattened by the
/// worker. A profile that's already in the library is updated in place.
pub async fn import_system(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    body: Bytes,
) -> Result<Response, AppError> {
    let s = slicing(&state)?;
    let mut vendor = String::new();
    let mut wanted: Vec<(Kind, String)> = Vec::new();
    for (k, v) in url::form_urlencoded::parse(&body) {
        let v = v.trim().to_string();
        match k.as_ref() {
            "vendor" => vendor = v,
            "machine" if !v.is_empty() => wanted.insert(0, (Kind::Machine, v)),
            "process" if !v.is_empty() => wanted.push((Kind::Process, v)),
            "filament" if !v.is_empty() => wanted.push((Kind::Filament, v)),
            _ => {}
        }
    }
    wanted.dedup();
    if vendor.is_empty() || wanted.is_empty() {
        return Err(AppError::BadRequest("Choose a vendor and a printer.".into()));
    }
    if wanted.len() > MAX_SYSTEM_IMPORT {
        return Err(AppError::BadRequest(format!(
            "Import at most {MAX_SYSTEM_IMPORT} profiles at a time."
        )));
    }
    let mut report = Report::default();
    for (kind, name) in wanted {
        let result = match s.client.profile(kind, &vendor, &name).await {
            Ok(settings) => {
                store_profile(
                    &state,
                    ip,
                    &user,
                    kind,
                    &name,
                    Source::System,
                    Some(&vendor),
                    settings,
                )
                .await
            }
            // A worker that's down or refusing us won't do better for the next one.
            Err(e @ (SlicerError::Offline(_) | SlicerError::Unauthorized | SlicerError::Timeout)) => {
                report.push(format!("{} {name}", kind.as_str()), Err(e.detail()), Some(kind));
                break;
            }
            Err(e) => Err(e.detail()),
        };
        report.push(format!("orca: {vendor}"), result, Some(kind));
    }
    let notice = format!("Import from {vendor}: {}.", report.counts());
    tab_page(&state, hx, &user, Some(notice), None, Some(report)).await
}

// ── user presets and bundles ─────────────────────────────────────────────────────────────

/// Imports uploaded preset JSON and Orca bundles. The worker resolves their `inherits`
/// against its system library and each other, so what's stored is self-contained.
pub async fn import_user(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    let s = slicing(&state)?;
    let mut parsed = Parsed::default();
    let mut files = 0usize;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => {
                return Err(AppError::BadRequest(
                    "The upload was cut short or too large.".into(),
                ));
            }
        };
        if field.name() != Some("files") {
            continue;
        }
        let name = field.file_name().unwrap_or_default().to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|_| AppError::BadRequest("The upload was cut short or too large.".into()))?;
        if name.is_empty() && bytes.is_empty() {
            continue; // the browser's empty file input
        }
        files += 1;
        if files > import::MAX_FILES {
            return Err(AppError::BadRequest(format!(
                "Upload at most {} files at a time.",
                import::MAX_FILES
            )));
        }
        parsed.add_file(&name, &bytes);
    }
    if files == 0 {
        return tab_page(
            &state,
            hx,
            &user,
            None,
            Some("Choose a file to import.".into()),
            None,
        )
        .await;
    }

    let mut report = Report {
        skipped: std::mem::take(&mut parsed.skipped),
        ..Report::default()
    };
    let (good, bad): (Vec<_>, Vec<_>) = parsed.candidates.into_iter().partition(|c| c.preset.is_ok());
    for c in bad {
        report.push(c.source, Err(c.preset.unwrap_err()), None);
    }
    if good.len() > import::MAX_PRESETS {
        return Err(AppError::BadRequest(format!(
            "Import at most {} presets at a time.",
            import::MAX_PRESETS
        )));
    }
    if !good.is_empty() {
        let (sources, presets): (Vec<String>, Vec<Profile>) = good
            .into_iter()
            .map(|c| (c.source, c.preset.unwrap_or_default()))
            .unzip();
        match s.client.resolve(&presets).await {
            Ok(results) => {
                for (source, r) in sources.into_iter().zip(results) {
                    match r {
                        Ok(p) => {
                            let stored = store_profile(
                                &state,
                                ip,
                                &user,
                                p.kind,
                                &p.name,
                                Source::User,
                                None,
                                p.profile,
                            )
                            .await;
                            report.push(source, stored, Some(p.kind));
                        }
                        Err(why) => report.push(source, Err(why), None),
                    }
                }
            }
            Err(e) => {
                let error = format!(
                    "The slicer couldn't resolve the presets: {} ({})",
                    e.detail(),
                    e.label()
                );
                return tab_page(&state, hx, &user, None, Some(error), Some(report)).await;
            }
        }
    }
    let notice = format!("Import: {}.", report.counts());
    tab_page(&state, hx, &user, Some(notice), None, Some(report)).await
}

enum Stored {
    Created(String),
    Updated(String),
}

/// Adds a profile to the library, or replaces the one of that kind and name.
#[allow(clippy::too_many_arguments)]
async fn store_profile(
    state: &AppState,
    ip: ClientIp,
    user: &RequireRole<Admin>,
    kind: Kind,
    name: &str,
    source: Source,
    vendor: Option<&str>,
    mut settings: Profile,
) -> Result<Stored, String> {
    let name = import::valid_name(name)?;
    // What Orca keys the profile by must match what we file it under.
    settings.insert("name".into(), name.clone().into());
    settings.insert("type".into(), kind.as_str().into());
    let internal = |e: sqlx::Error| {
        tracing::error!(error = %e, "storing a slicer profile failed");
        "couldn't be saved (see the server log)".to_string()
    };
    let existing = store::find_profile(&state.db, kind, &name)
        .await
        .map_err(internal)?;
    let (id, action, stored) = match existing {
        Some(p) => {
            store::update_profile(&state.db, p.id, source, vendor, &settings)
                .await
                .map_err(internal)?;
            (p.id, "slicer_profile_update", Stored::Updated(name.clone()))
        }
        None => {
            let id = store::create_profile(
                &state.db,
                &NewProfile {
                    kind,
                    name: &name,
                    source,
                    vendor,
                    settings: &settings,
                },
            )
            .await
            .map_err(internal)?;
            (id, "slicer_profile_create", Stored::Created(name.clone()))
        }
    };
    let detail = format!(
        "profile={id} kind={} name={name} source={}{}",
        kind.as_str(),
        source.as_str(),
        vendor.map(|v| format!(" vendor={v}")).unwrap_or_default()
    );
    record(state, ip, user, action, None, detail).await;
    Ok(stored)
}

pub async fn delete_profile(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let p = store::get_profile(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?
        .info;
    match store::delete_profile(&state.db, id).await? {
        DeleteProfile::Deleted => {
            let detail = format!("profile={id} kind={} name={}", p.kind, p.name);
            record(&state, ip, &user, "slicer_profile_delete", None, detail).await;
            tab_page(
                &state,
                hx,
                &user,
                Some(format!("Deleted {} {}.", p.kind, p.name)),
                None,
                None,
            )
            .await
        }
        DeleteProfile::NotFound => Err(AppError::NotFound),
        DeleteProfile::InUse { printers, jobs } => {
            let mut uses = Vec::new();
            if !printers.is_empty() {
                uses.push(format!(
                    "printer{} {}",
                    plural(printers.len()),
                    printers.join(", ")
                ));
            }
            if jobs > 0 {
                uses.push(format!("{jobs} unfinished slice job{}", plural(jobs as usize)));
            }
            let error = format!("{} {} is still used by {}.", p.kind, p.name, uses.join(" and "));
            tab_page(&state, hx, &user, None, Some(error), None).await
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ── per-printer settings (shown on the printer's edit page) ─────────────────────────────

#[derive(Template)]
#[template(path = "partials/printer_slicer.html")]
pub struct PrinterPanel {
    id: i64,
    machines: Vec<(i64, String)>,
    processes: Vec<(i64, String)>,
    filaments: Vec<(i64, String)>,
    current: Option<PrinterSlicer>,
    notice: Option<String>,
    error: Option<String>,
}

impl PrinterPanel {
    fn is(&self, which: &str, id: &i64) -> bool {
        let Some(c) = &self.current else { return false };
        match which {
            "machine" => c.machine_profile_id == *id,
            "process" => c.default_process_id == Some(*id),
            _ => c.default_filament_id == Some(*id),
        }
    }
}

/// The printer page's slicing panel, or `None` when slicing is off.
pub(crate) async fn printer_panel(
    state: &AppState,
    printer_id: i64,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Option<String>, AppError> {
    if state.slicing.is_none() {
        return Ok(None);
    }
    let all = store::list_profiles(&state.db, None).await?;
    let of = |k: Kind| {
        all.iter()
            .filter(|p| p.kind() == Some(k))
            .map(|p| (p.id, p.name.clone()))
            .collect::<Vec<_>>()
    };
    let panel = PrinterPanel {
        id: printer_id,
        machines: of(Kind::Machine),
        processes: of(Kind::Process),
        filaments: of(Kind::Filament),
        current: store::printer_slicer(&state.db, printer_id).await?,
        notice,
        error,
    };
    Ok(Some(panel.render()?))
}

#[derive(Deserialize)]
pub struct PrinterSlicerForm {
    #[serde(default)]
    machine: String,
    #[serde(default)]
    process: String,
    #[serde(default)]
    filament: String,
}

fn opt_id(s: &str) -> Result<Option<i64>, AppError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse()
        .map(Some)
        .map_err(|_| AppError::BadRequest("Malformed form.".into()))
}

pub async fn save_printer(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
    axum::Form(f): axum::Form<PrinterSlicerForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let printer = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let name_of = async |pid: Option<i64>| -> Result<String, AppError> {
        Ok(match pid {
            Some(pid) => store::get_profile(&state.db, pid)
                .await?
                .map_or_else(|| "?".into(), |p| p.info.name),
            None => "-".into(),
        })
    };
    let (notice, error) = match opt_id(&f.machine)? {
        None => {
            store::clear_printer_slicer(&state.db, id).await?;
            record(
                &state,
                ip,
                &user,
                "printer_slicer_update",
                Some(id),
                "machine=-".into(),
            )
            .await;
            (
                Some(format!("{} no longer has a slicer profile.", printer.name)),
                None,
            )
        }
        Some(machine) => {
            let s = PrinterSlicer {
                printer_id: id,
                machine_profile_id: machine,
                default_process_id: opt_id(&f.process)?,
                default_filament_id: opt_id(&f.filament)?,
            };
            if store::set_printer_slicer(&state.db, &s).await? {
                let detail = format!(
                    "machine={} process={} filament={}",
                    name_of(Some(machine)).await?,
                    name_of(s.default_process_id).await?,
                    name_of(s.default_filament_id).await?
                );
                record(&state, ip, &user, "printer_slicer_update", Some(id), detail).await;
                (
                    Some(format!("Saved slicing settings for {}.", printer.name)),
                    None,
                )
            } else {
                (
                    None,
                    Some("Choose profiles of the right kind from the library.".into()),
                )
            }
        }
    };
    crate::web::printers::edit_page(&state, hx, &user, id, notice, error).await
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &RequireRole<Admin>,
    action: &'static str,
    printer_id: Option<i64>,
    detail: String,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id,
            action,
            detail: Some(detail),
            ip: ip.to_string_opt(),
        },
    )
    .await;
}
