//! `[6]Slice`: upload a model, pick a process and a filament, and printpit slices it for a
//! printer and puts the G-code there (PLAN §15.4–15.6).
//!
//! The tab renders from the database: opening it makes no printer and no worker calls. A
//! submit checks the printer (an existing file of the same name; whether it's idle when "start
//! printing" is ticked) because the user just asked for something. The queue
//! (`slicer::queue`) does the rest in the background. The job panel refreshes itself only while
//! the job is running and the user's auto-refresh pref is on.

use std::path::{Path as FsPath, PathBuf};

use askama::Template;
use axum::extract::{Multipart, Path, Query, State};
use axum::response::Response;
use axum::{Form, extract::rejection::FormRejection};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::auth::{ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::slicer::{self as store, Job, JobStatus, NewJob, StoredProfile};
use crate::db::{audit, prefs, printers};
use crate::error::AppError;
use crate::moonraker::files::clean_name;
use crate::slicer::import::{self, Fit};
use crate::slicer::options::{self, OptionsForm, SliceOptions};
use crate::slicer::queue::{self, MAX_ACTIVE_PER_USER, OUTPUT_DIR};
use crate::slicer::{Kind, Profile, Slicing, model, storage};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::slicer::slicing;
use crate::web::widgets;

/// While a job runs, its panel refreshes this often (auto-refresh on only).
const REFRESH_MS: u32 = 2000;
const RECENT_JOBS: i64 = 20;

// ── the tab ──────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
    job: Option<i64>,
}

struct Choice {
    id: i64,
    name: String,
    fits: bool,
}

struct FormView {
    printer_id: i64,
    machine: String,
    processes: Vec<Choice>,
    filaments: Vec<Choice>,
    default_process: Option<i64>,
    default_filament: Option<i64>,
    max_mb: u64,
}

impl FormView {
    fn is_default(&self, kind: &str, id: &i64) -> bool {
        match kind {
            "process" => self.default_process == Some(*id),
            _ => self.default_filament == Some(*id),
        }
    }
}

struct JobRow {
    id: i64,
    model: String,
    label: &'static str,
    class: &'static str,
    age: String,
}

#[derive(Template)]
#[template(path = "tabs/slice.html")]
struct TabTemplate {
    select: Option<String>,
    printer_name: String,
    /// Why there's no form (viewer, or no machine profile set), when there isn't one.
    no_form: Option<String>,
    form: Option<FormView>,
    jobs: Vec<JobRow>,
    job: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    tab_page(&state, hx, &user, q.printer, q.job, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn tab_page(
    state: &AppState,
    hx: HxRequest,
    user: &CurrentUser,
    printer: Option<i64>,
    job: Option<i64>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let s = slicing(state)?;
    let Some(sel) = select::select(state, user.id, printer, "/ui/slice").await? else {
        let html = TabTemplate {
            select: None,
            printer_name: String::new(),
            no_form: Some("No printers are set up yet.".into()),
            form: None,
            jobs: Vec::new(),
            job: None,
            notice,
            error,
        }
        .render()?;
        return page(hx, user, Tab::Slice, html);
    };
    let p = &sel.printer;
    let setup = store::printer_slicer(&state.db, p.id).await?;
    let (form, no_form) = if !user.has(Role::Operator) {
        (
            None,
            Some("Viewers can follow jobs; operators slice.".to_string()),
        )
    } else {
        match setup {
            None => (
                None,
                Some(format!(
                    "{} has no slicer profile yet. An admin sets it on the printer's page (Printers → edit → slicing).",
                    p.name
                )),
            ),
            Some(setup) => match store::get_profile(&state.db, setup.machine_profile_id).await? {
                None => (None, Some("This printer's machine profile is missing.".into())),
                Some(machine) => {
                    let system = system_name(&machine.settings);
                    let choices = async |kind| -> Result<Vec<Choice>, AppError> {
                        let mut v: Vec<Choice> = store::profiles_with_settings(&state.db, kind)
                            .await?
                            .into_iter()
                            .map(|p| Choice {
                                fits: fits(&p, &system),
                                id: p.info.id,
                                name: p.info.name,
                            })
                            .collect();
                        // Those that fit first, then the rest.
                        v.sort_by_key(|c| !c.fits);
                        Ok(v)
                    };
                    (
                        Some(FormView {
                            printer_id: p.id,
                            machine: machine.info.name.clone(),
                            processes: choices(Kind::Process).await?,
                            filaments: choices(Kind::Filament).await?,
                            default_process: setup.default_process_id,
                            default_filament: setup.default_filament_id,
                            max_mb: s.config.max_model_bytes / (1024 * 1024),
                        }),
                        None,
                    )
                }
            },
        }
    };
    let jobs = store::recent_jobs(&state.db, Some(p.id), RECENT_JOBS)
        .await?
        .into_iter()
        .map(|j| {
            let (label, class) = badge(j.status());
            JobRow {
                id: j.id,
                model: j.model_name,
                label,
                class,
                age: widgets::age(j.created_at as f64),
            }
        })
        .collect();
    let job = match job {
        Some(id) => match store::get_job(&state.db, id).await? {
            Some(j) if j.printer_id == Some(p.id) => Some(panel(state, user, &j, None, None, false).await?),
            _ => None,
        },
        None => None,
    };
    let html = TabTemplate {
        select: Some(sel.select.render()?),
        printer_name: p.name.clone(),
        no_form,
        form,
        jobs,
        job,
        notice,
        error,
    }
    .render()?;
    page(hx, user, Tab::Slice, html)
}

/// The name Orca checks `compatible_printers` against: a system machine's own, else its parent.
fn system_name(machine: &Profile) -> String {
    let text = |k: &str| {
        machine
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let inherits = text("inherits");
    if text("from").eq_ignore_ascii_case("system") || inherits.is_empty() {
        text("name")
    } else {
        inherits
    }
}

fn fits(p: &StoredProfile, machine: &str) -> bool {
    let list = |k: &str| -> Vec<String> {
        match p.settings.get(k) {
            Some(serde_json::Value::Array(a)) => {
                a.iter().filter_map(|v| v.as_str()).map(String::from).collect()
            }
            _ => Vec::new(),
        }
    };
    let cond = p
        .settings
        .get("compatible_printers_condition")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    import::fit(&list("compatible_printers"), cond, machine) != Fit::No
}

fn badge(s: JobStatus) -> (&'static str, &'static str) {
    match s {
        JobStatus::Queued => ("QUEUED", "badge-dim"),
        JobStatus::Slicing => ("SLICING", "badge-info"),
        JobStatus::Uploading => ("UPLOADING", "badge-info"),
        JobStatus::Done => ("DONE", "badge-ok"),
        JobStatus::Failed => ("FAILED", "badge-err"),
        JobStatus::Cancelled => ("CANCELLED", "badge-dim"),
    }
}

// ── submit ───────────────────────────────────────────────────────────────────────────────

/// Removes an upload that never became a job.
struct Incoming(Option<PathBuf>);

impl Drop for Incoming {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct Submitted {
    printer: Option<i64>,
    process: Option<i64>,
    filament: Option<i64>,
    output: String,
    replace: bool,
    start_print: bool,
    options: OptionsForm,
    model_name: String,
    model_size: u64,
}

/// Uploads a model and queues a job. Fields come before the file (the form puts it last); the
/// file is streamed to disk under its size limit and checked before anything is queued.
pub async fn submit(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    let s = slicing(&state)?.clone();
    let mut incoming = Incoming(None);
    let sub = match read_submit(&state, &s, &user, &mut multipart, &mut incoming).await {
        Ok(sub) => sub,
        Err(Bad(e)) => return tab_page(&state, hx, &user, None, None, None, Some(e)).await,
    };
    let printer_id = sub.printer;
    match queue_job(&state, &s, &user, ip, sub, &mut incoming).await {
        Ok((job, notice)) => {
            s.queue.wake();
            tab_page(&state, hx, &user, printer_id, Some(job), Some(notice), None).await
        }
        Err(Bad(e)) => tab_page(&state, hx, &user, printer_id, None, None, Some(e)).await,
    }
}

/// A problem to show on the form.
struct Bad(String);

impl From<AppError> for Bad {
    fn from(e: AppError) -> Self {
        match e {
            AppError::BadRequest(m) | AppError::Denied(m) => Bad(m),
            e => {
                tracing::error!(error = %e, "slice submit failed");
                Bad("Something went wrong. Details are in the server log.".into())
            }
        }
    }
}

impl From<sqlx::Error> for Bad {
    fn from(e: sqlx::Error) -> Self {
        AppError::from(e).into()
    }
}

async fn read_submit(
    state: &AppState,
    s: &Slicing,
    user: &CurrentUser,
    multipart: &mut Multipart,
    incoming: &mut Incoming,
) -> Result<Submitted, Bad> {
    let mut sub = Submitted {
        printer: None,
        process: None,
        filament: None,
        output: String::new(),
        replace: false,
        start_print: false,
        options: OptionsForm::default(),
        model_name: String::new(),
        model_size: 0,
    };
    let bad = |m: &str| Bad(m.to_string());
    let id = |v: &str| v.trim().parse::<i64>().ok();
    // A request over the route's limit is refused here, before any of it is read.
    let cut_off = || {
        Bad(format!(
            "The upload was interrupted, or the model is larger than {} MB.",
            s.config.max_model_bytes / (1024 * 1024)
        ))
    };
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return Err(cut_off()),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name != "model" {
            let value = field.text().await.map_err(|_| bad("Malformed form."))?;
            match name.as_str() {
                "printer" => sub.printer = id(&value),
                "process" => sub.process = id(&value),
                "filament" => sub.filament = id(&value),
                "output" => sub.output = value,
                "replace" => sub.replace = true,
                "start_print" => sub.start_print = true,
                other => sub.options.set(other, value),
            }
            continue;
        }
        // The model. Checks that need no upload come first.
        if store::active_jobs_for_user(&state.db, user.id).await? >= MAX_ACTIVE_PER_USER {
            return Err(Bad(format!(
                "You already have {MAX_ACTIVE_PER_USER} jobs waiting or running. Let one finish or cancel it."
            )));
        }
        let file_name = field
            .file_name()
            .and_then(clean_name)
            .filter(|n| n.chars().count() <= 200)
            .ok_or_else(|| bad("Choose a model to slice."))?;
        let ext = storage::model_extension(&file_name)
            .ok_or_else(|| bad("Upload an .stl, .3mf, .obj or .step file."))?;
        let path = s.storage.incoming_path();
        incoming.0 = Some(path.clone());
        let mut out = s
            .storage
            .create(&path)
            .await
            .map_err(|e| AppError::internal(format!("can't store an upload: {e}")))?;
        let max = s.config.max_model_bytes;
        let mut size = 0u64;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    size += chunk.len() as u64;
                    if size > max {
                        return Err(Bad(format!(
                            "The model is larger than {} MB (PRINTPIT_SLICER_MAX_MODEL_MB).",
                            max / (1024 * 1024)
                        )));
                    }
                    out.write_all(&chunk)
                        .await
                        .map_err(|e| AppError::internal(format!("can't store an upload: {e}")))?;
                }
                Ok(None) => break,
                // The route's body limit (just above ours) or a dropped connection.
                Err(_) => return Err(cut_off()),
            }
        }
        out.flush().await.map_err(AppError::internal)?;
        drop(out);
        if size == 0 {
            return Err(bad("The model is empty."));
        }
        model::check(&ext, &read_head(&path).await?, size).map_err(Bad)?;
        sub.model_name = file_name;
        sub.model_size = size;
        // Anything after the file is ignored.
        while let Ok(Some(_)) = multipart.next_field().await {}
        return Ok(sub);
    }
    Err(bad("Choose a model to slice."))
}

async fn read_head(path: &FsPath) -> Result<Vec<u8>, Bad> {
    let mut f = tokio::fs::File::open(path).await.map_err(AppError::internal)?;
    let mut head = vec![0u8; model::HEAD];
    let mut n = 0;
    while n < head.len() {
        let read = f.read(&mut head[n..]).await.map_err(AppError::internal)?;
        if read == 0 {
            break;
        }
        n += read;
    }
    head.truncate(n);
    Ok(head)
}

/// Validates the choices against the library and the printer, then creates the job.
async fn queue_job(
    state: &AppState,
    s: &Slicing,
    user: &CurrentUser,
    ip: ClientIp,
    sub: Submitted,
    incoming: &mut Incoming,
) -> Result<(i64, String), Bad> {
    let printer = enabled_printer(state, sub.printer.ok_or(Bad("Pick a printer.".into()))?).await?;
    let setup = store::printer_slicer(&state.db, printer.id)
        .await?
        .ok_or_else(|| Bad(format!("{} has no slicer profile yet.", printer.name)))?;
    let machine = store::get_profile(&state.db, setup.machine_profile_id)
        .await?
        .ok_or_else(|| Bad("This printer's machine profile is missing.".into()))?;
    let is_3mf = sub.model_name.to_ascii_lowercase().ends_with(".3mf");
    let mut opts: SliceOptions = sub.options.parse(is_3mf).map_err(Bad)?;
    let pick = async |id: Option<i64>, kind: Kind, what: &str| -> Result<StoredProfile, Bad> {
        let p = match id {
            Some(id) => store::get_profile(&state.db, id).await?,
            None => None,
        }
        .filter(|p| p.info.kind() == Some(kind));
        p.ok_or_else(|| Bad(format!("Pick a {what}.")))
    };
    let (process, filament) = if opts.project_settings {
        (None, None)
    } else {
        let process = pick(sub.process, Kind::Process, "process").await?;
        let filament = pick(sub.filament, Kind::Filament, "filament").await?;
        // The same check the queue makes, so a bad layer height is a form error now.
        opts.apply(&mut process.settings.clone(), &machine.settings)
            .map_err(Bad)?;
        (Some(process), Some(filament))
    };
    let name_of = |p: &Option<StoredProfile>| p.as_ref().map(|p| p.info.name.clone()).unwrap_or_default();
    let output = options::output_name(
        &sub.output,
        &sub.model_name,
        &name_of(&process),
        &name_of(&filament),
    )
    .ok_or_else(|| Bad("Give the G-code a name with letters or digits.".into()))?;
    opts.output = Some(output.clone());

    // Asking the printer now is part of what the user asked for.
    let mut notes = Vec::new();
    let client = state.printer_client(&printer).ok();
    if !sub.replace
        && let Some(c) = &client
        && let Ok(listing) = state.dirs.get(printer.id, c, OUTPUT_DIR).await
        && listing.files.iter().any(|f| f.filename == output)
    {
        return Err(Bad(format!(
            "{OUTPUT_DIR}/{output} is already on {}. Tick \"replace\" or give it another name.",
            printer.name
        )));
    }
    let mut start_print = false;
    if sub.start_print {
        match &client {
            Some(c) => match queue::printer_state(c).await {
                Ok(st) if queue::is_idle(&st) => start_print = true,
                Ok(st) => notes.push(format!(
                    "{} is {st}, so it won't start printing by itself.",
                    printer.name
                )),
                Err(e) => notes.push(format!(
                    "Couldn't check {} ({}), so it won't start printing.",
                    printer.name,
                    e.label()
                )),
            },
            None => notes.push("The printer can't be reached, so it won't start printing.".into()),
        }
    }

    // The job and its model appear together: the queue can't see the row until the model is
    // in place, and a failed move takes the row back with it.
    let mut tx = state.db.begin().await?;
    let id = store::create_job(
        &mut *tx,
        &NewJob {
            user_id: user.id,
            printer_id: printer.id,
            model_name: &sub.model_name,
            model_size: i64::try_from(sub.model_size).unwrap_or(i64::MAX),
            machine_profile_id: Some(machine.info.id),
            process_id: process.as_ref().map(|p| p.info.id),
            filament_id: filament.as_ref().map(|p| p.info.id),
            options_json: &opts.to_json(),
            start_print,
        },
    )
    .await?;
    let from = incoming.0.take().expect("the model was stored");
    let ext = storage::model_extension(&sub.model_name).expect("checked above");
    if let Err(e) = s.storage.adopt_model(&from, id, &ext).await {
        let _ = std::fs::remove_file(&from);
        let _ = s.storage.remove_job(id).await;
        return Err(AppError::internal(format!("can't store the model: {e}")).into());
    }
    tx.commit().await?;
    record(
        state,
        ip,
        user,
        Some(printer.id),
        "slice_job_create",
        format!(
            "job={id} model={} bytes={} process={} filament={} output={OUTPUT_DIR}/{output} start_print={start_print}",
            sub.model_name,
            sub.model_size,
            name_of(&process),
            name_of(&filament)
        ),
    )
    .await;
    let mut notice = format!("Queued {} for {}.", sub.model_name, printer.name);
    for n in notes {
        notice.push(' ');
        notice.push_str(&n);
    }
    Ok((id, notice))
}

// ── the job panel ────────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/slice_job.html")]
struct PanelTemplate {
    id: i64,
    model: String,
    label: &'static str,
    class: &'static str,
    printer: String,
    created: String,
    profiles: Vec<String>,
    settings: Vec<String>,
    stats: Vec<(&'static str, String)>,
    output: Option<String>,
    /// A failure, or a remark on a waiting or finished job.
    message: Option<String>,
    message_class: &'static str,
    log_tail: Vec<String>,
    refresh_ms: Option<u32>,
    can_cancel: bool,
    can_reslice: bool,
    can_upload: bool,
    can_print: bool,
    can_delete: bool,
    form: OptionsForm,
    is_3mf: bool,
    oob_badge: bool,
    notice: Option<String>,
    error: Option<String>,
}

impl PanelTemplate {
    fn sel(&self, field: &str, value: &str) -> bool {
        match field {
            "supports" => self.form.supports == value,
            _ => self.form.brim == value,
        }
    }
}

async fn panel(
    state: &AppState,
    user: &CurrentUser,
    job: &Job,
    notice: Option<String>,
    error: Option<String>,
    // An htmx swap of the panel alone: also update the job's badge in the recent-jobs list.
    swap: bool,
) -> Result<String, AppError> {
    let s = slicing(state)?;
    let status = job.status();
    // Only when the list shows this job; htmx logs an error for a missing OOB target.
    let oob_badge = swap
        && match job.printer_id {
            Some(pid) => store::recent_jobs(&state.db, Some(pid), RECENT_JOBS)
                .await?
                .iter()
                .any(|j| j.id == job.id),
            None => false,
        };
    let (label, class) = badge(status);
    let printer = match job.printer_id {
        Some(pid) => printers::get(&state.db, pid).await?.map(|p| p.name),
        None => None,
    }
    .unwrap_or_else(|| "a deleted printer".into());
    let opts = SliceOptions::from_json(&job.options_json);
    let name = async |id: Option<i64>| -> Result<Option<String>, AppError> {
        Ok(match id {
            Some(id) => store::get_profile(&state.db, id).await?.map(|p| p.info.name),
            None => None,
        })
    };
    let mut profiles = Vec::new();
    if let Some(m) = name(job.machine_profile_id).await? {
        profiles.push(m);
    }
    if opts.project_settings {
        profiles.push("the 3MF's own process and filament".into());
    } else {
        for id in [job.process_id, job.filament_id] {
            profiles.push(name(id).await?.unwrap_or_else(|| "(deleted profile)".into()));
        }
    }
    let mut stats = vec![("model", widgets::human_size(job.model_size.max(0) as u64))];
    if let Some(t) = job.est_seconds {
        stats.push(("print time", widgets::duration(t as f64)));
    }
    if let Some(g) = job.filament_g {
        let m = job
            .filament_mm
            .map(|mm| format!(", {:.2} m", mm / 1000.0))
            .unwrap_or_default();
        stats.push(("filament", format!("{g:.1} g{m}")));
    }
    if let Some(l) = job.layers {
        stats.push(("layers", l.to_string()));
    }
    if let Some(b) = job.gcode_size {
        stats.push(("g-code", widgets::human_size(b.max(0) as u64)));
    }
    let message_class = match status {
        JobStatus::Failed => "err",
        JobStatus::Queued | JobStatus::Done => "warn",
        _ => "dim",
    };
    let model_kept = storage::model_extension(&job.model_name)
        .and_then(|ext| s.storage.model_path(job.id, &ext))
        .is_some_and(|p| p.exists());
    let gcode_kept = s.storage.output_path(job.id).exists();
    let active = status.is_active();
    let operator = user.has(Role::Operator);
    let refresh_ms = (active && prefs::get(&state.db, user.id).await?.auto_refresh).then_some(REFRESH_MS);
    let tpl = PanelTemplate {
        id: job.id,
        model: job.model_name.clone(),
        label,
        class,
        printer,
        created: widgets::age(job.created_at as f64),
        profiles,
        settings: opts.summary(),
        stats,
        output: job.output_name.clone(),
        message: job.error.clone().filter(|m| !m.trim().is_empty()),
        message_class,
        log_tail: job
            .log_tail
            .as_deref()
            .map(|t| t.lines().map(String::from).collect())
            .unwrap_or_default(),
        refresh_ms,
        can_cancel: operator && active,
        can_reslice: operator && !active && model_kept,
        can_upload: operator && !active && gcode_kept && job.gcode_size.is_some(),
        can_print: operator && status == JobStatus::Done && job.output_name.is_some(),
        can_delete: operator && !active,
        is_3mf: job.model_name.to_ascii_lowercase().ends_with(".3mf"),
        form: opts.to_form(),
        oob_badge,
        notice,
        error,
    };
    Ok(tpl.render()?)
}

async fn job_for(state: &AppState, id: i64) -> Result<Job, AppError> {
    slicing(state)?;
    store::get_job(&state.db, id).await?.ok_or(AppError::NotFound)
}

fn html(body: String) -> Response {
    axum::response::IntoResponse::into_response(axum::response::Html(body))
}

pub async fn show(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let job = job_for(&state, id).await?;
    if hx.0 {
        Ok(html(panel(&state, &user, &job, None, None, true).await?))
    } else {
        // A deep link: the tab with this job open.
        tab_page(&state, hx, &user, job.printer_id, Some(id), None, None).await
    }
}

async fn reply(
    state: &AppState,
    user: &CurrentUser,
    id: i64,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let job = job_for(state, id).await?;
    Ok(html(panel(state, user, &job, notice, error, true).await?))
}

pub async fn cancel(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let job = job_for(&state, id).await?;
    let s = slicing(&state)?;
    if !store::cancel(&state.db, id).await? {
        return reply(
            &state,
            &user,
            id,
            None,
            Some("That job has already finished.".into()),
        )
        .await;
    }
    s.queue.cancel(id);
    record(
        &state,
        ip,
        &user,
        job.printer_id,
        "slice_job_cancel",
        format!("job={id} was={}", job.status),
    )
    .await;
    reply(&state, &user, id, Some("Cancelled.".into()), None).await
}

/// Slices the same model again, with the overrides from the panel's form.
pub async fn reslice(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    form: Result<Form<OptionsForm>, FormRejection>,
) -> Result<Response, AppError> {
    let job = job_for(&state, id).await?;
    let s = slicing(&state)?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let old = SliceOptions::from_json(&job.options_json);
    let is_3mf = job.model_name.to_ascii_lowercase().ends_with(".3mf");
    let mut opts = match f.parse(is_3mf) {
        Ok(o) => o,
        Err(e) => return reply(&state, &user, id, None, Some(e)).await,
    };
    opts.output = old.output;
    if !store::retry(&state.db, id, &opts.to_json(), false).await? {
        return reply(
            &state,
            &user,
            id,
            None,
            Some("Only a finished job can be sliced again.".into()),
        )
        .await;
    }
    s.queue.wake();
    record(
        &state,
        ip,
        &user,
        job.printer_id,
        "slice_job_create",
        format!("job={id} reslice"),
    )
    .await;
    reply(&state, &user, id, Some("Queued to slice again.".into()), None).await
}

/// Uploads a finished job's G-code to its printer again (replacing the file there).
pub async fn upload(
    State(state): State<AppState>,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    job_for(&state, id).await?;
    let s = slicing(&state)?;
    if !s.storage.output_path(id).exists() || !store::start_upload(&state.db, id).await? {
        return reply(
            &state,
            &user,
            id,
            None,
            Some("There's no G-code left to upload.".into()),
        )
        .await;
    }
    s.queue.spawn_upload(state.clone(), id);
    reply(&state, &user, id, Some("Uploading.".into()), None).await
}

/// Starts printing a finished job's file, unless the printer is busy.
pub async fn print(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let job = job_for(&state, id).await?;
    let (Some(pid), Some(file), JobStatus::Done) = (job.printer_id, job.output_name.clone(), job.status())
    else {
        return reply(
            &state,
            &user,
            id,
            None,
            Some("This job has no file on the printer.".into()),
        )
        .await;
    };
    let p = enabled_printer(&state, pid).await?;
    let client = match state.printer_client(&p) {
        Ok(c) => c,
        Err(e) => return reply(&state, &user, id, None, Some(e.detail())).await,
    };
    match queue::printer_state(&client).await {
        Ok(st) if queue::is_idle(&st) => {}
        Ok(st) => return reply(&state, &user, id, None, Some(format!("{} is {st}.", p.name))).await,
        Err(e) => {
            return reply(
                &state,
                &user,
                id,
                None,
                Some(format!("{} ({})", e.detail(), e.label())),
            )
            .await;
        }
    }
    if let Err(e) = client
        .post_action("/printer/print/start", &[("filename", file.as_str())])
        .await
    {
        return reply(
            &state,
            &user,
            id,
            None,
            Some(format!("{} ({})", e.detail(), e.label())),
        )
        .await;
    }
    record(
        &state,
        ip,
        &user,
        Some(pid),
        "slice_job_print",
        format!("job={id} file={file}"),
    )
    .await;
    reply(
        &state,
        &user,
        id,
        Some(format!("Printing {file} on {}.", p.name)),
        None,
    )
    .await
}

/// Deletes a job and its files (cancelling it first if it's still running).
pub async fn delete(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let job = job_for(&state, id).await?;
    let s = slicing(&state)?;
    if store::cancel(&state.db, id).await? {
        s.queue.cancel(id);
    }
    store::delete_job(&state.db, id).await?;
    if let Err(e) = s.storage.remove_job(id).await {
        tracing::warn!(error = %e, job = id, "couldn't remove a slice job's files");
    }
    record(
        &state,
        ip,
        &user,
        job.printer_id,
        "slice_job_delete",
        format!("job={id} model={}", job.model_name),
    )
    .await;
    let notice = format!("Deleted the job for {}.", job.model_name);
    tab_page(&state, hx, &user, job.printer_id, None, Some(notice), None).await
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    printer_id: Option<i64>,
    action: &'static str,
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
