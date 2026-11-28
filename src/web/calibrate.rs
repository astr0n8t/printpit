//! The calibration walkthrough (`/ui/slice/calibrate`, under the Slice tab; PLAN §15.13).
//!
//! For one printer and filament profile, the page steps through Orca's calibration prints in
//! order. Each step slices printpit's own test piece with the printer's machine profile, the
//! chosen process and the filament (as an ordinary slice job), and then asks what the print
//! showed. A result becomes changes to the filament profile: an admin saves them to the library
//! (straight away, or later); until then they're carried into the next tests, so each one builds
//! on the last.
//!
//! Like the Slice tab, the page renders from the database only: no printer and no worker calls.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::auth::{ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::calibration::{self as cal_db, Step};
use crate::db::slicer::{self as store, NewJob, StoredProfile};
use crate::db::{audit, printers};
use crate::error::AppError;
use crate::slicer::calibrate::{self, Calibration, Change, Test};
use crate::slicer::options::{self, SliceOptions};
use crate::slicer::queue::{self, MAX_ACTIVE_PER_USER};
use crate::slicer::{Kind, Profile};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::slice::{badge, fits, panel, system_name};
use crate::web::slicer::slicing;

const PATH: &str = "/ui/slice/calibrate";

/// The page's query and every form's fields (each handler reads what it needs).
#[derive(Debug, Default, Deserialize)]
pub struct CalForm {
    printer: Option<i64>,
    process: Option<i64>,
    filament: Option<i64>,
    test: Option<String>,
    #[serde(default)]
    start: String,
    #[serde(default)]
    end: String,
    #[serde(default)]
    step: String,
    start_print: Option<String>,
    #[serde(default)]
    value: String,
    save: Option<String>,
    undo: Option<String>,
}

struct Choice {
    id: i64,
    name: String,
    fits: bool,
}

struct Row {
    slug: &'static str,
    title: &'static str,
    /// `(text, css class)`.
    status: (String, &'static str),
    current: bool,
}

struct Current {
    test: Test,
    number: usize,
    title: &'static str,
    purpose: &'static str,
    reading: &'static str,
    setting: &'static str,
    has_range: bool,
    unit: &'static str,
    start: String,
    end: String,
    step: String,
    /// A print exists, so a result can be picked from these `(value, label)`.
    choices: Vec<(String, String)>,
    picked: Option<String>,
    /// A sliced job's G-code isn't there yet (still queued or slicing).
    job_pending: bool,
    skipped: bool,
    printed: bool,
    /// The print's job: `(id, badge label, badge class)`. The job panel's refresh updates the
    /// badge (`job-badge-{id}`) out of band.
    job: Option<(i64, &'static str, &'static str)>,
}

#[derive(Template)]
#[template(path = "tabs/calibrate.html")]
struct PageTemplate {
    select: Option<String>,
    printer_id: i64,
    printer_name: String,
    machine: String,
    no_form: Option<String>,
    processes: Vec<Choice>,
    filaments: Vec<Choice>,
    process_id: i64,
    filament_id: i64,
    filament_name: String,
    rows: Vec<Row>,
    done: usize,
    total: usize,
    current: Option<Current>,
    /// Results not in the filament profile yet: `key = value` lines.
    unsaved: Vec<String>,
    operator: bool,
    admin: bool,
    job: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

/// What the page is about, resolved from the request and the database.
struct Ctx {
    printer: printers::Printer,
    select: String,
    machine: StoredProfile,
    process: StoredProfile,
    filament: StoredProfile,
    processes: Vec<Choice>,
    filaments: Vec<Choice>,
    steps: Vec<Step>,
}

enum Resolved {
    Ready(Box<Ctx>),
    /// Nothing to calibrate: why, and the printer selector if there's a printer.
    Missing(Option<String>, Option<(i64, String)>, String),
}

async fn resolve(state: &AppState, user: &CurrentUser, f: &CalForm) -> Result<Resolved, AppError> {
    let Some(sel) = select::select(state, user.id, f.printer, PATH).await? else {
        return Ok(Resolved::Missing(
            None,
            None,
            "No printers are set up yet.".into(),
        ));
    };
    let p = sel.printer;
    let select = sel.select.render()?;
    let missing = |why: String| {
        Ok(Resolved::Missing(
            Some(select.clone()),
            Some((p.id, p.name.clone())),
            why,
        ))
    };
    let Some(setup) = store::printer_slicer(&state.db, p.id).await? else {
        return missing(format!(
            "{} has no slicer profile yet. An admin sets it on the printer's page (Printers → edit → slicing).",
            p.name
        ));
    };
    let Some(machine) = store::get_profile(&state.db, setup.machine_profile_id).await? else {
        return missing("This printer's machine profile is missing.".into());
    };
    let system = system_name(&machine.settings);
    let list = async |kind| -> Result<Vec<StoredProfile>, AppError> {
        let mut v = store::profiles_with_settings(&state.db, kind).await?;
        v.sort_by_key(|p| !fits(p, &system));
        Ok(v)
    };
    let all_processes = list(Kind::Process).await?;
    let all_filaments = list(Kind::Filament).await?;
    let pick = |all: &[StoredProfile], asked: Option<i64>, default: Option<i64>| {
        let find = |id: Option<i64>| id.and_then(|id| all.iter().find(|p| p.info.id == id));
        find(asked).or_else(|| find(default)).or(all.first()).cloned()
    };
    let (Some(process), Some(filament)) = (
        pick(&all_processes, f.process, setup.default_process_id),
        pick(&all_filaments, f.filament, setup.default_filament_id),
    ) else {
        return missing("The profile library needs a process and a filament (the Slicer tab).".into());
    };
    let choices = |all: &[StoredProfile]| {
        all.iter()
            .map(|p| Choice {
                id: p.info.id,
                name: p.info.name.clone(),
                fits: fits(p, &system),
            })
            .collect()
    };
    let steps = cal_db::steps(&state.db, p.id, filament.info.id).await?;
    Ok(Resolved::Ready(Box::new(Ctx {
        processes: choices(&all_processes),
        filaments: choices(&all_filaments),
        printer: p,
        select,
        machine,
        process,
        filament,
        steps,
    })))
}

impl Ctx {
    fn step(&self, t: Test) -> Option<&Step> {
        self.steps.iter().find(|s| s.test == t)
    }

    /// Unsaved changes from the tests before `t` (all of them for `None`), in order.
    fn carry(&self, before: Option<Test>) -> Vec<Change> {
        self.steps
            .iter()
            .take_while(|s| Some(s.test) != before)
            .filter(|s| s.unsaved())
            .flat_map(|s| s.changes.clone())
            .collect()
    }

    /// The filament as the test `t` sees it: the library's, plus earlier unsaved results.
    fn filament_for(&self, t: Option<Test>) -> Profile {
        let mut f = self.filament.settings.clone();
        calibrate::apply_changes(&mut f, &self.carry(t));
        f
    }
}

/// The calibration of a step's job, if it still exists and is one.
async fn step_job(
    state: &AppState,
    step: Option<&Step>,
) -> Result<Option<(store::Job, Calibration)>, AppError> {
    let Some(id) = step.and_then(|s| s.job_id) else {
        return Ok(None);
    };
    Ok(store::get_job(&state.db, id).await?.and_then(|j| {
        let c = SliceOptions::from_json(&j.options_json).calibration?;
        Some((j, c))
    }))
}

pub async fn show(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<CalForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    render(&state, hx, &user, &q, None, None, None).await
}

async fn render(
    state: &AppState,
    hx: HxRequest,
    user: &CurrentUser,
    f: &CalForm,
    focus: Option<Test>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let ctx = match resolve(state, user, f).await? {
        Resolved::Ready(c) => c,
        Resolved::Missing(select, printer, why) => {
            let (printer_id, printer_name) = printer.unwrap_or_default();
            let html = PageTemplate {
                select,
                printer_id,
                printer_name,
                machine: String::new(),
                no_form: Some(why),
                processes: Vec::new(),
                filaments: Vec::new(),
                process_id: 0,
                filament_id: 0,
                filament_name: String::new(),
                rows: Vec::new(),
                done: 0,
                total: Test::ALL.len(),
                current: None,
                unsaved: Vec::new(),
                operator: user.has(Role::Operator),
                admin: user.has(Role::Admin),
                job: None,
                notice,
                error,
            }
            .render()?;
            return page(hx, user, Tab::Slice, html);
        }
    };
    let asked = focus.or_else(|| f.test.as_deref().and_then(Test::parse));
    let first_open = Test::ALL
        .into_iter()
        .find(|t| !ctx.step(*t).is_some_and(Step::is_done));
    let current_test = asked.or(first_open).unwrap_or(Test::ALL[Test::ALL.len() - 1]);
    let mut rows = Vec::new();
    for t in Test::ALL {
        let step = ctx.step(t);
        let status = match step {
            Some(s) if s.skipped => ("skipped".to_string(), "dim"),
            Some(Step {
                result: Some(r),
                saved_at,
                ..
            }) => (
                format!(
                    "✓ {r}{}{}",
                    if t.unit() == "%" { " %" } else { "" },
                    if saved_at.is_some() {
                        " · saved"
                    } else {
                        " · not saved yet"
                    }
                ),
                if saved_at.is_some() { "ok" } else { "warn" },
            ),
            Some(Step { job_id: Some(j), .. }) => {
                (format!("printed (job #{j}), waiting for a result"), "info")
            }
            _ => ("—".to_string(), "dim"),
        };
        rows.push(Row {
            slug: t.slug(),
            title: t.title(),
            status,
            current: t == current_test,
        });
    }
    let done = ctx.steps.iter().filter(|s| s.is_done()).count();
    let step = ctx.step(current_test);
    let job = step_job(state, step).await?;
    let filament_now = ctx.filament_for(Some(current_test));
    let (start, end, st) = match &job {
        Some((_, c)) if c.test.has_range() => (c.start, c.end, c.step),
        _ => calibrate::defaults(current_test, &ctx.machine.settings, &filament_now),
    };
    let places = current_test.places();
    let job_pending = job
        .as_ref()
        .is_some_and(|(j, _)| matches!(j.status(), store::JobStatus::Queued | store::JobStatus::Slicing));
    let current = Current {
        test: current_test,
        number: Test::ALL.iter().position(|t| *t == current_test).unwrap_or(0) + 1,
        title: current_test.title(),
        purpose: current_test.purpose(),
        reading: current_test.reading(),
        setting: current_test.setting(),
        has_range: current_test.has_range(),
        unit: current_test.unit(),
        start: calibrate::gcode::fmt(start, places),
        end: calibrate::gcode::fmt(end, places),
        step: calibrate::gcode::fmt(st, places),
        choices: job
            .as_ref()
            .map(|(_, c)| c.choices(&filament_now))
            .unwrap_or_default(),
        picked: step.and_then(|s| s.result.clone()),
        job_pending,
        skipped: step.is_some_and(|s| s.skipped),
        printed: job.is_some(),
        job: job.as_ref().map(|(j, _)| {
            let (label, class) = badge(j.status());
            (j.id, label, class)
        }),
    };
    let job_html = match &job {
        Some((j, _)) => Some(panel(state, user, j, None, None, false).await?),
        None => None,
    };
    let html = PageTemplate {
        select: Some(ctx.select.clone()),
        printer_id: ctx.printer.id,
        printer_name: ctx.printer.name.clone(),
        machine: ctx.machine.info.name.clone(),
        no_form: None,
        process_id: ctx.process.info.id,
        filament_id: ctx.filament.info.id,
        filament_name: ctx.filament.info.name.clone(),
        rows,
        done,
        total: Test::ALL.len(),
        current: Some(current),
        unsaved: ctx.carry(None).iter().map(|c| c.to_string()).collect(),
        operator: user.has(Role::Operator),
        admin: user.has(Role::Admin),
        job: job_html,
        notice,
        error,
        processes: ctx.processes,
        filaments: ctx.filaments,
    }
    .render()?;
    page(hx, user, Tab::Slice, html)
}

fn test_of(f: &CalForm) -> Result<Test, AppError> {
    f.test
        .as_deref()
        .and_then(Test::parse)
        .ok_or_else(|| AppError::BadRequest("Unknown calibration test.".into()))
}

async fn ready(state: &AppState, user: &CurrentUser, f: &CalForm) -> Result<Box<Ctx>, AppError> {
    match resolve(state, user, f).await? {
        Resolved::Ready(c) => Ok(c),
        Resolved::Missing(_, _, why) => Err(AppError::BadRequest(why)),
    }
}

fn number(raw: &str, what: &str) -> Result<f64, String> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("{what} must be a number."))
}

/// Slices a test's piece for the printer (and starts it, if asked and the printer is idle).
pub async fn slice(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Form(f): Form<CalForm>,
) -> Result<Response, AppError> {
    let s = slicing(&state)?.clone();
    let test = test_of(&f)?;
    let ctx = ready(&state, &user, &f).await?;
    let fail = async |e: String| render(&state, hx, &user, &f, Some(test), None, Some(e)).await;
    let printer = enabled_printer(&state, ctx.printer.id).await?;
    if store::active_jobs_for_user(&state.db, user.id).await? >= MAX_ACTIVE_PER_USER {
        return fail(format!(
            "You already have {MAX_ACTIVE_PER_USER} jobs waiting or running. Let one finish or cancel it."
        ))
        .await;
    }
    let range = if test.has_range() {
        match (
            number(&f.start, "Start"),
            number(&f.end, "End"),
            number(&f.step, "Step"),
        ) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return fail(e).await,
        }
    } else {
        (0.0, 0.0, 0.0)
    };
    let mut c = match Calibration::new(test, range.0, range.1, range.2, &ctx.machine.settings) {
        Ok(c) => c,
        Err(e) => return fail(e).await,
    };
    c.carry = ctx.carry(Some(test));
    let stem = c.file_stem();
    let output = options::output_name(&format!("{stem}_{}", ctx.filament.info.name), "", "", "")
        .ok_or_else(|| AppError::internal("no file name for a calibration"))?;
    let model_name = format!("{stem}.stl");
    let model = c.model(&ctx.machine.settings, &ctx.process.settings);

    let mut notes = Vec::new();
    let mut start_print = false;
    if f.start_print.is_some() {
        match state.printer_client(&printer) {
            Ok(client) => match queue::printer_state(&client).await {
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
            Err(_) => notes.push("The printer can't be reached, so it won't start printing.".into()),
        }
    }
    let opts = SliceOptions {
        output: Some(output.clone()),
        calibration: Some(c.clone()),
        ..SliceOptions::default()
    };
    let incoming = s.storage.incoming_path();
    let write = async {
        let mut out = s.storage.create(&incoming).await?;
        out.write_all(&model).await?;
        out.flush().await
    };
    if let Err(e) = write.await {
        let _ = tokio::fs::remove_file(&incoming).await;
        return Err(AppError::internal(format!(
            "can't store a calibration model: {e}"
        )));
    }
    // The job and its model appear together, as for an upload (`web::slice`).
    let mut tx = state.db.begin().await?;
    let id = store::create_job(
        &mut *tx,
        &NewJob {
            user_id: user.id,
            printer_id: printer.id,
            model_name: &model_name,
            model_size: model.len() as i64,
            machine_profile_id: Some(ctx.machine.info.id),
            process_id: Some(ctx.process.info.id),
            filament_id: Some(ctx.filament.info.id),
            options_json: &opts.to_json(),
            start_print,
        },
    )
    .await?;
    if let Err(e) = s.storage.adopt_model(&incoming, id, "stl").await {
        let _ = tokio::fs::remove_file(&incoming).await;
        let _ = s.storage.remove_job(id).await;
        return Err(AppError::internal(format!("can't store the model: {e}")));
    }
    tx.commit().await?;
    cal_db::set_job(&state.db, printer.id, ctx.filament.info.id, test, id).await?;
    s.queue.wake();
    record(
        &state,
        ip,
        &user,
        printer.id,
        "slice_job_create",
        format!(
            "job={id} calibration={} range={}..{} step={} process={} filament={} output={}/{output} start_print={start_print}",
            test.slug(),
            c.start,
            c.end,
            c.step,
            ctx.process.info.name,
            ctx.filament.info.name,
            queue::OUTPUT_DIR
        ),
    )
    .await;
    let mut notice = format!("Queued the {} for {}.", test.title(), printer.name);
    for n in notes {
        notice.push(' ');
        notice.push_str(&n);
    }
    render(&state, hx, &user, &f, Some(test), Some(notice), None).await
}

/// Records what a test's print showed; an admin can save it to the filament profile at once.
pub async fn result(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Form(f): Form<CalForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let test = test_of(&f)?;
    let ctx = ready(&state, &user, &f).await?;
    let Some((_, c)) = step_job(&state, ctx.step(test)).await? else {
        return render(
            &state,
            hx,
            &user,
            &f,
            Some(test),
            None,
            Some("Print this test first.".into()),
        )
        .await;
    };
    let changes = match c.changes(&f.value, &ctx.filament_for(Some(test))) {
        Ok(ch) => ch,
        Err(e) => return render(&state, hx, &user, &f, Some(test), None, Some(e)).await,
    };
    let value = f.value.trim().to_string();
    let (pid, fid) = (ctx.printer.id, ctx.filament.info.id);
    cal_db::set_result(&state.db, pid, fid, test, &value, &changes, false).await?;
    let list: Vec<String> = changes.iter().map(|c| c.to_string()).collect();
    record(
        &state,
        ip,
        &user,
        pid,
        "calibration_result",
        format!(
            "test={} value={value} filament={} {}",
            test.slug(),
            ctx.filament.info.name,
            list.join(", ")
        ),
    )
    .await;
    let mut notice = format!("Recorded {value} for the {}.", test.title());
    if f.save.is_some() && user.has(Role::Admin) {
        let ctx = ready(&state, &user, &f).await?;
        notice.push(' ');
        notice.push_str(&save_all(&state, ip, &user, &ctx).await?);
    }
    let next = Test::ALL
        .into_iter()
        .skip_while(|t| *t != test)
        .nth(1)
        .unwrap_or(test);
    render(&state, hx, &user, &f, Some(next), Some(notice), None).await
}

/// Writes every unsaved result to the filament profile, in walkthrough order.
async fn save_all(state: &AppState, ip: ClientIp, user: &CurrentUser, ctx: &Ctx) -> Result<String, AppError> {
    let changes = ctx.carry(None);
    if changes.is_empty() {
        return Ok("Nothing to save.".into());
    }
    let mut p = store::get_profile(&state.db, ctx.filament.info.id)
        .await?
        .ok_or(AppError::NotFound)?;
    calibrate::apply_changes(&mut p.settings, &changes);
    store::update_settings(&state.db, p.info.id, &p.settings).await?;
    let tests: Vec<Test> = ctx.steps.iter().filter(|s| s.unsaved()).map(|s| s.test).collect();
    cal_db::mark_saved(&state.db, ctx.printer.id, p.info.id, &tests).await?;
    // The last value of each key is what the profile has now.
    let mut last: Vec<&Change> = Vec::new();
    for c in &changes {
        last.retain(|l| l.key != c.key);
        last.push(c);
    }
    let list = last.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ");
    record(
        state,
        ip,
        user,
        ctx.printer.id,
        "slicer_profile_edit",
        format!(
            "profile={} kind=filament name={} calibration {list}",
            p.info.id, p.info.name
        ),
    )
    .await;
    Ok(format!("Saved to {}: {list}.", p.info.name))
}

/// Saves the walkthrough's unsaved results to the filament profile (admin).
pub async fn save(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<crate::auth::Admin>,
    Form(f): Form<CalForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let ctx = ready(&state, &user, &f).await?;
    let notice = save_all(&state, ip, &user, &ctx).await?;
    render(&state, hx, &user, &f, None, Some(notice), None).await
}

/// Skips a test (or, with `undo`, takes that back).
pub async fn skip(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Operator>,
    Form(f): Form<CalForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let test = test_of(&f)?;
    let ctx = ready(&state, &user, &f).await?;
    let skipped = f.undo.is_none();
    cal_db::set_skipped(&state.db, ctx.printer.id, ctx.filament.info.id, test, skipped).await?;
    let (focus, notice) = if skipped {
        (None, format!("Skipped the {}.", test.title()))
    } else {
        (
            Some(test),
            format!("The {} is back in the walkthrough.", test.title()),
        )
    };
    render(&state, hx, &user, &f, focus, Some(notice), None).await
}

/// Starts over for this printer and filament (results already saved stay in the profile).
pub async fn reset(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Form(f): Form<CalForm>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let ctx = ready(&state, &user, &f).await?;
    cal_db::reset(&state.db, ctx.printer.id, ctx.filament.info.id).await?;
    record(
        &state,
        ip,
        &user,
        ctx.printer.id,
        "calibration_reset",
        format!("filament={}", ctx.filament.info.name),
    )
    .await;
    let f = CalForm { test: None, ..f };
    render(
        &state,
        hx,
        &user,
        &f,
        Some(Test::ALL[0]),
        Some("Started over.".into()),
        None,
    )
    .await
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    printer_id: i64,
    action: &'static str,
    detail: String,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(printer_id),
            action,
            detail: Some(detail),
            ip: ip.to_string_opt(),
        },
    )
    .await;
}
