//! `[c]Calibrate` (PLAN §15.13): calibration runs.
//!
//! A run is started on a printer with a process, a filament and a choice of tests. Its page walks
//! through them one at a time: slice the test's piece (and print it), then, once the G-code is on
//! the printer, pick the best block, band or line from that print, and go on to the next test
//! (or record and print the next one in one go). A result becomes changes to the filament, machine
//! or process profile: an admin saves them (straight away or at the end); until then they're
//! carried into the run's later tests, so each test builds on the last.
//!
//! Like the Slice tab, the pages render from the database only: no printer and no worker calls.
//! Slicing with "print it" asks the printer whether it's idle, as the Slice form does.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::HeaderValue;
use axum::response::Response;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::auth::{Admin, ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::calibration::{self as cal_db, NewRun, Run, Step};
use crate::db::slicer::{self as store, Job, JobStatus, NewJob, StoredProfile};
use crate::db::{audit, prefs, printers};
use crate::error::AppError;
use crate::slicer::Kind;
use crate::slicer::calibrate::{self, Calibration, Change, Profiles, Test};
use crate::slicer::options::{self, SliceOptions};
use crate::slicer::queue::{self, MAX_ACTIVE_PER_USER};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select;
use crate::web::slice::{badge, fits, panel, system_name};
use crate::web::slicer::slicing;
use crate::web::widgets;

const PATH: &str = "/ui/calibrate";
/// While a test's slice runs, the run page refreshes this often (auto-refresh on only).
const REFRESH_MS: u32 = 2000;
const LISTED_RUNS: i64 = 20;

// ── the tab: start a run, and the runs so far ───────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct HomeQuery {
    printer: Option<i64>,
}

struct Choice {
    id: i64,
    name: String,
    fits: bool,
}

struct TestChoice {
    slug: &'static str,
    title: &'static str,
    checked: bool,
    /// Why this printer can't print it (the box is then disabled).
    unavailable: Option<String>,
}

struct StartForm {
    printers: Vec<(i64, String)>,
    printer_id: i64,
    machine: String,
    processes: Vec<Choice>,
    filaments: Vec<Choice>,
    process_id: Option<i64>,
    filament_id: Option<i64>,
    core: Vec<TestChoice>,
    extras: Vec<TestChoice>,
    belt: Option<String>,
}

impl StartForm {
    fn is_process(&self, id: &i64) -> bool {
        self.process_id == Some(*id)
    }

    fn is_filament(&self, id: &i64) -> bool {
        self.filament_id == Some(*id)
    }
}

struct RunRow {
    id: i64,
    printer: String,
    filament: String,
    process: String,
    progress: String,
    /// The next test, or how it ended.
    next: String,
    age: String,
}

#[derive(Template)]
#[template(path = "tabs/calibrate.html")]
struct HomeTemplate {
    start: Option<StartForm>,
    /// Why there's no start form (viewer, no printer, no slicer profile), when there isn't one.
    no_form: Option<String>,
    open: Vec<RunRow>,
    finished: Vec<RunRow>,
    notice: Option<String>,
    error: Option<String>,
}

pub async fn home(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<HomeQuery>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    home_page(&state, hx, &user, q.printer, None, None).await
}

async fn home_page(
    state: &AppState,
    hx: HxRequest,
    user: &CurrentUser,
    printer: Option<i64>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let (start, no_form) = if user.has(Role::Operator) {
        match start_form(state, user, printer).await? {
            Ok(f) => (Some(f), None),
            Err(why) => (None, Some(why)),
        }
    } else {
        (
            None,
            Some("Viewers can follow calibrations; operators run them.".to_string()),
        )
    };
    let html = HomeTemplate {
        start,
        no_form,
        open: run_rows(state, false).await?,
        finished: run_rows(state, true).await?,
        notice,
        error,
    }
    .render()?;
    page(hx, user, Tab::Calibrate, html)
}

async fn start_form(
    state: &AppState,
    user: &CurrentUser,
    printer: Option<i64>,
) -> Result<Result<StartForm, String>, AppError> {
    let Some(sel) = select::select(state, user.id, printer, PATH).await? else {
        return Ok(Err("No printers are set up yet.".into()));
    };
    let p = sel.printer;
    let Some(setup) = store::printer_slicer(&state.db, p.id).await? else {
        return Ok(Err(format!(
            "{} has no slicer profile yet. An admin sets it on the printer's page (Printers → edit → slicing).",
            p.name
        )));
    };
    let Some(machine) = store::get_profile(&state.db, setup.machine_profile_id).await? else {
        return Ok(Err("This printer's machine profile is missing.".into()));
    };
    let system = system_name(&machine.settings);
    let list = async |kind| -> Result<Vec<Choice>, AppError> {
        let mut v: Vec<Choice> = store::profiles_with_settings(&state.db, kind)
            .await?
            .into_iter()
            .map(|p| Choice {
                fits: fits(&p, &system),
                id: p.info.id,
                name: p.info.name,
            })
            .collect();
        v.sort_by_key(|c| !c.fits);
        Ok(v)
    };
    let processes = list(Kind::Process).await?;
    let filaments = list(Kind::Filament).await?;
    if processes.is_empty() || filaments.is_empty() {
        return Ok(Err(
            "The profile library needs a process and a filament (the Slicer tab).".into(),
        ));
    }
    let pick = |all: &[Choice], default: Option<i64>| {
        default
            .filter(|d| all.iter().any(|c| c.id == *d))
            .or(all.first().map(|c| c.id))
    };
    let choice = |t: Test| TestChoice {
        slug: t.slug(),
        title: t.title(),
        checked: !t.optional(),
        unavailable: calibrate::unavailable(t, &machine.settings),
    };
    Ok(Ok(StartForm {
        printers: sel.select.options.clone(),
        printer_id: p.id,
        machine: machine.info.name.clone(),
        process_id: pick(&processes, setup.default_process_id),
        filament_id: pick(&filaments, setup.default_filament_id),
        processes,
        filaments,
        core: Test::ALL
            .into_iter()
            .filter(|t| !t.optional())
            .map(choice)
            .collect(),
        extras: Test::ALL
            .into_iter()
            .filter(|t| t.optional())
            .map(choice)
            .collect(),
        belt: calibrate::belt_tilt(&machine.settings)
            .ok()
            .flatten()
            .map(|a| calibrate::gcode::fmt(a, 1)),
    }))
}

async fn run_rows(state: &AppState, finished: bool) -> Result<Vec<RunRow>, AppError> {
    let mut rows = Vec::new();
    for r in cal_db::runs(&state.db, finished, LISTED_RUNS).await? {
        let steps = cal_db::steps(&state.db, r.id).await?;
        let done = r
            .tests
            .iter()
            .filter(|t| steps.iter().any(|s| s.test == **t && s.is_done()))
            .count();
        let name = async |id: Option<i64>| -> Result<String, AppError> {
            Ok(match id {
                Some(id) => store::get_profile(&state.db, id).await?.map(|p| p.info.name),
                None => None,
            }
            .unwrap_or_else(|| "(default)".into()))
        };
        let next = if finished {
            let unsaved = steps.iter().any(Step::unsaved);
            if unsaved { "results not saved yet" } else { "done" }.to_string()
        } else {
            r.tests
                .iter()
                .find(|t| !steps.iter().any(|s| s.test == **t && s.is_done()))
                .map_or_else(String::new, |t| format!("next: {}", t.title()))
        };
        rows.push(RunRow {
            id: r.id,
            printer: printers::get(&state.db, r.printer_id)
                .await?
                .map_or_else(|| "(deleted printer)".into(), |p| p.name),
            filament: name(Some(r.filament_id)).await?,
            process: name(r.process_id).await?,
            progress: format!("{done}/{}", r.tests.len()),
            next,
            age: widgets::age(r.updated_at as f64),
        });
    }
    Ok(rows)
}

/// Starts a run. Fields: `printer`, `process`, `filament`, and one `test` per chosen test.
pub async fn start(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Form(fields): Form<Vec<(String, String)>>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let get = |k: &str| {
        fields
            .iter()
            .find(|(n, _)| n == k)
            .and_then(|(_, v)| v.trim().parse::<i64>().ok())
    };
    let printer_id = get("printer");
    let fail = async |e: &str| home_page(&state, hx, &user, printer_id, None, Some(e.to_string())).await;
    let Some(printer) = printer_id.map(|id| printers::get(&state.db, id)) else {
        return fail("Pick a printer.").await;
    };
    let Some(printer) = printer.await?.filter(|p| p.enabled) else {
        return fail("Pick an enabled printer.").await;
    };
    let Some(setup) = store::printer_slicer(&state.db, printer.id).await? else {
        return fail("That printer has no slicer profile yet.").await;
    };
    let machine = store::get_profile(&state.db, setup.machine_profile_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let profile = async |id: Option<i64>, kind| -> Result<Option<StoredProfile>, AppError> {
        Ok(match id {
            Some(id) => store::get_profile(&state.db, id).await?,
            None => None,
        }
        .filter(|p| p.info.kind() == Some(kind)))
    };
    let Some(process) = profile(get("process"), Kind::Process).await? else {
        return fail("Pick a process.").await;
    };
    let Some(filament) = profile(get("filament"), Kind::Filament).await? else {
        return fail("Pick a filament.").await;
    };
    let mut tests: Vec<Test> = Vec::new();
    for (n, v) in &fields {
        if n == "test" {
            let t = Test::parse(v.trim())
                .ok_or_else(|| AppError::BadRequest("Unknown calibration test.".into()))?;
            if let Some(why) = calibrate::unavailable(t, &machine.settings) {
                return fail(&format!("{}: {why}", t.title())).await;
            }
            tests.push(t);
        }
    }
    tests.sort_by_key(|t| Test::ALL.iter().position(|a| a == t));
    tests.dedup();
    if tests.is_empty() {
        return fail("Pick at least one test.").await;
    }
    let id = cal_db::create_run(
        &state.db,
        &NewRun {
            printer_id: printer.id,
            process_id: process.info.id,
            filament_id: filament.info.id,
            user_id: user.id,
            tests: &tests,
        },
    )
    .await?;
    let slugs: Vec<&str> = tests.iter().map(|t| t.slug()).collect();
    record(
        &state,
        ip,
        &user,
        printer.id,
        "calibration_start",
        format!(
            "run={id} process={} filament={} tests={}",
            process.info.name,
            filament.info.name,
            slugs.join(",")
        ),
    )
    .await;
    let notice = format!(
        "Started calibrating {} on {}: {} {}.",
        filament.info.name,
        printer.name,
        tests.len(),
        if tests.len() == 1 { "test" } else { "tests" }
    );
    let res = run_page(&state, hx, &user, id, None, Some(notice), None).await?;
    Ok(push_url(res, &format!("/ui/calibrate/runs/{id}")))
}

fn push_url(mut res: Response, url: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(url) {
        res.headers_mut().insert("hx-push-url", v);
    }
    res
}

// ── a run ────────────────────────────────────────────────────────────────────────────────

/// A run with everything its page and actions need.
struct Ctx {
    run: Run,
    printer: printers::Printer,
    machine: StoredProfile,
    process: StoredProfile,
    filament: StoredProfile,
    steps: Vec<Step>,
}

async fn load(state: &AppState, id: i64) -> Result<Ctx, AppError> {
    slicing(state)?;
    let run = cal_db::get_run(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let printer = printers::get(&state.db, run.printer_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let setup = store::printer_slicer(&state.db, printer.id)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("{} has no slicer profile any more.", printer.name)))?;
    let get = async |id: Option<i64>| -> Result<Option<StoredProfile>, AppError> {
        Ok(match id {
            Some(id) => store::get_profile(&state.db, id).await?,
            None => None,
        })
    };
    let machine = get(Some(setup.machine_profile_id))
        .await?
        .ok_or_else(|| AppError::BadRequest("The printer's machine profile is missing.".into()))?;
    let process = match get(run.process_id).await? {
        Some(p) => p,
        None => get(setup.default_process_id).await?.ok_or_else(|| {
            AppError::BadRequest(
                "This run's process was deleted and the printer has no default process.".into(),
            )
        })?,
    };
    let filament = get(Some(run.filament_id)).await?.ok_or(AppError::NotFound)?;
    let steps = cal_db::steps(&state.db, run.id).await?;
    Ok(Ctx {
        run,
        printer,
        machine,
        process,
        filament,
        steps,
    })
}

impl Ctx {
    fn step(&self, t: Test) -> Option<&Step> {
        self.steps.iter().find(|s| s.test == t)
    }

    fn is_done(&self, t: Test) -> bool {
        self.step(t).is_some_and(Step::is_done)
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

    /// The profiles as the test `t` sees them: the library's, plus earlier unsaved results.
    fn now(&self, t: Option<Test>) -> Profiles {
        let mut p = Profiles {
            machine: self.machine.settings.clone(),
            process: self.process.settings.clone(),
            filament: self.filament.settings.clone(),
        };
        p.apply(&self.carry(t));
        p
    }

    /// The first of the run's tests still to do: after `after` if given, else from the start.
    fn next_open(&self, after: Option<Test>) -> Option<Test> {
        let tests = &self.run.tests;
        let from = after
            .and_then(|a| tests.iter().position(|t| *t == a))
            .map_or(0, |i| i + 1);
        tests[from..]
            .iter()
            .chain(tests[..from].iter())
            .find(|t| Some(**t) != after && !self.is_done(**t))
            .copied()
    }
}

/// The calibration of a step's job, if it still exists and is one.
async fn step_job(state: &AppState, step: Option<&Step>) -> Result<Option<(Job, Calibration)>, AppError> {
    let Some(id) = step.and_then(|s| s.job_id) else {
        return Ok(None);
    };
    Ok(store::get_job(&state.db, id).await?.and_then(|j| {
        let c = SliceOptions::from_json(&j.options_json).calibration?;
        Some((j, c))
    }))
}

struct StepRow {
    slug: &'static str,
    title: &'static str,
    optional: bool,
    /// `(text, css class)`.
    status: (String, &'static str),
    current: bool,
}

/// Where the current test is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not sliced yet (or the last slice failed or was cancelled).
    Ready,
    /// Queued, slicing or uploading.
    Slicing,
    /// The G-code is on the printer: pick a result.
    Printed,
}

struct Current {
    test: Test,
    number: usize,
    title: &'static str,
    purpose: &'static str,
    reading: &'static str,
    setting: &'static str,
    kind: &'static str,
    has_range: bool,
    unit: &'static str,
    start: String,
    end: String,
    step: String,
    phase: Phase,
    /// The print's job: `(id, badge label, badge class, file on the printer)`.
    job: Option<(i64, &'static str, &'static str, Option<String>)>,
    /// Why the last slice didn't work, when it didn't.
    job_error: Option<String>,
    choices: Vec<(String, String)>,
    two_axes: bool,
    picked: Option<String>,
    picked_y: Option<String>,
    /// The result recorded so far, as shown.
    recorded: Option<String>,
    klipper: Option<String>,
    skipped: bool,
    unavailable: Option<String>,
    /// The test after this one, if any is left: `(slug, title)`.
    next: Option<(&'static str, &'static str)>,
    /// Whether the next test can be sliced straight from the result form.
    next_printable: bool,
}

#[derive(Template)]
#[template(path = "tabs/calibrate_run.html")]
struct RunTemplate {
    id: i64,
    printer_name: String,
    machine: String,
    process: String,
    filament: String,
    belt: Option<String>,
    rows: Vec<StepRow>,
    done: usize,
    total: usize,
    finished: bool,
    /// When finished: `(test, result)` per test.
    summary: Vec<(&'static str, String)>,
    current: Option<Current>,
    unsaved: Vec<String>,
    operator: bool,
    admin: bool,
    job: Option<String>,
    refresh_ms: Option<u32>,
    notice: Option<String>,
    error: Option<String>,
}

impl Phase {
    fn is(&self, name: &str) -> bool {
        matches!(
            (self, name),
            (Phase::Ready, "ready") | (Phase::Slicing, "slicing") | (Phase::Printed, "printed")
        )
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RunQuery {
    test: Option<String>,
}

pub async fn show(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<RunQuery>,
) -> Result<Response, AppError> {
    let focus = q.test.as_deref().and_then(Test::parse);
    run_page(&state, hx, &user, id, focus, None, None).await
}

async fn run_page(
    state: &AppState,
    hx: HxRequest,
    user: &CurrentUser,
    id: i64,
    focus: Option<Test>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let ctx = load(state, id).await?;
    let tests = ctx.run.tests.clone();
    let finished = tests.iter().all(|t| ctx.is_done(*t));
    // The test asked for, else the first one left; a finished run shows its summary.
    let current_test = focus
        .filter(|t| tests.contains(t))
        .or_else(|| ctx.next_open(None));
    let mut rows = Vec::new();
    for t in &tests {
        let step = ctx.step(*t);
        let status = match step {
            Some(s) if s.skipped => ("skipped".to_string(), "dim"),
            Some(Step {
                result: Some(r),
                saved_at,
                ..
            }) => (
                format!(
                    "✓ {}{}",
                    show_result(*t, r),
                    if saved_at.is_some() {
                        " · saved"
                    } else {
                        " · not saved yet"
                    }
                ),
                if saved_at.is_some() { "ok" } else { "warn" },
            ),
            Some(Step { job_id: Some(j), .. }) => {
                (format!("printed as job #{j}, waiting for a result"), "info")
            }
            _ => ("—".to_string(), "dim"),
        };
        rows.push(StepRow {
            slug: t.slug(),
            title: t.title(),
            optional: t.optional(),
            status,
            current: Some(*t) == current_test,
        });
    }
    let done = tests.iter().filter(|t| ctx.is_done(**t)).count();
    let summary = if finished {
        tests
            .iter()
            .map(|t| {
                let s = ctx.step(*t);
                let text = match s {
                    Some(Step {
                        result: Some(r),
                        changes,
                        ..
                    }) => format!(
                        "{} → {}",
                        show_result(*t, r),
                        changes
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    _ => "skipped".into(),
                };
                (t.title(), text)
            })
            .collect()
    } else {
        Vec::new()
    };

    let mut job_html = None;
    let mut refresh_ms = None;
    let current = match current_test {
        None => None,
        Some(test) => {
            let step = ctx.step(test);
            let job = step_job(state, step).await?;
            let now = ctx.now(Some(test));
            let (start, end, st) = match &job {
                Some((_, c)) if c.test.has_range() => (c.start, c.end, c.step),
                _ => calibrate::defaults(test, &now.machine, &now.filament),
            };
            let phase = match job.as_ref().map(|(j, _)| j.status()) {
                Some(s) if s.is_active() => Phase::Slicing,
                Some(JobStatus::Done) => Phase::Printed,
                _ => Phase::Ready,
            };
            if phase == Phase::Slicing {
                if prefs::get(&state.db, user.id).await?.auto_refresh {
                    refresh_ms = Some(REFRESH_MS);
                }
            } else if let Some((j, _)) = &job {
                // Not running, so its panel has no timer of its own.
                job_html = Some(panel(state, user, j, None, None, false).await?);
            }
            let picked: Vec<String> = step
                .and_then(|s| s.result.as_deref())
                .map(|r| r.split(',').map(String::from).collect())
                .unwrap_or_default();
            let next = ctx.next_open(Some(test));
            let places = test.places();
            Some(Current {
                test,
                number: tests.iter().position(|t| *t == test).unwrap_or(0) + 1,
                title: test.title(),
                purpose: test.purpose(),
                reading: test.reading(),
                setting: test.setting(),
                kind: test.result_kind(),
                has_range: test.has_range(),
                unit: test.unit(),
                start: calibrate::gcode::fmt(start, places),
                end: calibrate::gcode::fmt(end, places),
                step: calibrate::gcode::fmt(st, places),
                phase,
                job: job.as_ref().map(|(j, _)| {
                    let (label, class) = badge(j.status());
                    (j.id, label, class, j.output_name.clone())
                }),
                job_error: job
                    .as_ref()
                    .filter(|(j, _)| matches!(j.status(), JobStatus::Failed | JobStatus::Cancelled))
                    .map(|(j, _)| j.error.clone().unwrap_or_else(|| j.status.clone())),
                choices: match (&job, phase) {
                    (Some((_, c)), Phase::Printed) => c.choices(&now),
                    _ => Vec::new(),
                },
                two_axes: test.axes() == 2,
                picked: picked.first().cloned(),
                picked_y: picked.get(1).cloned(),
                recorded: step
                    .and_then(|s| s.result.as_deref())
                    .map(|r| show_result(test, r)),
                klipper: step
                    .filter(|s| s.result.is_some())
                    .and_then(|s| calibrate::klipper_note(test, &s.changes)),
                skipped: step.is_some_and(|s| s.skipped),
                unavailable: calibrate::unavailable(test, &now.machine),
                next: next.map(|t| (t.slug(), t.title())),
                next_printable: next.is_some_and(|t| calibrate::unavailable(t, &now.machine).is_none()),
            })
        }
    };
    let html = RunTemplate {
        id: ctx.run.id,
        printer_name: ctx.printer.name.clone(),
        machine: ctx.machine.info.name.clone(),
        process: ctx.process.info.name.clone(),
        filament: ctx.filament.info.name.clone(),
        belt: calibrate::belt_tilt(&ctx.machine.settings)
            .ok()
            .flatten()
            .map(|a| calibrate::gcode::fmt(a, 1)),
        rows,
        done,
        total: tests.len(),
        finished,
        summary,
        current,
        unsaved: ctx
            .carry(None)
            .iter()
            .map(|c| format!("{}: {c}", c.kind))
            .collect(),
        operator: user.has(Role::Operator),
        admin: user.has(Role::Admin),
        job: job_html,
        refresh_ms,
        notice,
        error,
    }
    .render()?;
    page(hx, user, Tab::Calibrate, html)
}

/// Every form on the run page: each handler reads what it needs.
#[derive(Debug, Default, Deserialize)]
pub struct StepForm {
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
    /// The second axis's value (input shaping frequency).
    #[serde(default)]
    value_y: String,
    save: Option<String>,
    undo: Option<String>,
    /// After recording: `next` shows the next test, `print_next` also slices and prints it.
    then: Option<String>,
}

fn test_of(f: &StepForm, ctx: &Ctx) -> Result<Test, AppError> {
    f.test
        .as_deref()
        .and_then(Test::parse)
        .filter(|t| ctx.run.tests.contains(t))
        .ok_or_else(|| AppError::BadRequest("That test isn't part of this calibration.".into()))
}

fn number(raw: &str, what: &str) -> Result<f64, String> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("{what} must be a number."))
}

/// Slices a test's piece for the run's printer (and prints it, if asked and the printer is
/// idle). Returns the notice to show, or a problem with the request.
async fn queue_test(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    ctx: &Ctx,
    test: Test,
    range: (f64, f64, f64),
    print: bool,
) -> Result<Result<String, String>, AppError> {
    let s = slicing(state)?.clone();
    if !ctx.printer.enabled {
        return Ok(Err(format!("{} is disabled.", ctx.printer.name)));
    }
    if store::active_jobs_for_user(&state.db, user.id).await? >= MAX_ACTIVE_PER_USER {
        return Ok(Err(format!(
            "You already have {MAX_ACTIVE_PER_USER} jobs waiting or running. Let one finish or cancel it."
        )));
    }
    let mut c = match Calibration::new(test, range.0, range.1, range.2, &ctx.machine.settings) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    c.carry = ctx.carry(Some(test));
    let stem = c.file_stem();
    let output = options::output_name(&format!("{stem}_{}", ctx.filament.info.name), "", "", "")
        .ok_or_else(|| AppError::internal("no file name for a calibration"))?;
    let model_name = format!("{stem}.stl");
    let model = c.model(&ctx.machine.settings, &ctx.process.settings);

    let mut notes = Vec::new();
    let mut start_print = false;
    if print {
        match state.printer_client(&ctx.printer) {
            Ok(client) => match queue::printer_state(&client).await {
                Ok(st) if queue::is_idle(&st) => start_print = true,
                Ok(st) => notes.push(format!(
                    "{} is {st}, so it won't start printing by itself.",
                    ctx.printer.name
                )),
                Err(e) => notes.push(format!(
                    "Couldn't check {} ({}), so it won't start printing.",
                    ctx.printer.name,
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
            printer_id: ctx.printer.id,
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
    cal_db::set_job(&state.db, ctx.run.id, test, id).await?;
    s.queue.wake();
    record(
        state,
        ip,
        user,
        ctx.printer.id,
        "slice_job_create",
        format!(
            "job={id} run={} calibration={} range={}..{} step={} process={} filament={} output={}/{output} start_print={start_print}",
            ctx.run.id,
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
    let mut notice = format!(
        "Slicing the {} for {}{}.",
        test.title(),
        ctx.printer.name,
        if start_print {
            "; it prints when it's on the printer"
        } else {
            ""
        }
    );
    for n in notes {
        notice.push(' ');
        notice.push_str(&n);
    }
    Ok(Ok(notice))
}

/// Slices (and prints) the test's piece.
pub async fn slice(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    Form(f): Form<StepForm>,
) -> Result<Response, AppError> {
    let ctx = load(&state, id).await?;
    let test = test_of(&f, &ctx)?;
    let range = if test.has_range() {
        match (
            number(&f.start, "Start"),
            number(&f.end, "End"),
            number(&f.step, "Step"),
        ) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                return run_page(&state, hx, &user, id, Some(test), None, Some(e)).await;
            }
        }
    } else {
        (0.0, 0.0, 0.0)
    };
    match queue_test(&state, ip, &user, &ctx, test, range, f.start_print.is_some()).await? {
        Ok(notice) => run_page(&state, hx, &user, id, Some(test), Some(notice), None).await,
        Err(e) => run_page(&state, hx, &user, id, Some(test), None, Some(e)).await,
    }
}

/// Records what a test's print showed, then goes on: to the next test, or straight to printing
/// it. An admin can save results to the profiles at the same time.
pub async fn result(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    Form(f): Form<StepForm>,
) -> Result<Response, AppError> {
    let ctx = load(&state, id).await?;
    let test = test_of(&f, &ctx)?;
    let printed = step_job(&state, ctx.step(test))
        .await?
        .filter(|(j, _)| j.status() == JobStatus::Done);
    let Some((_, c)) = printed else {
        return run_page(
            &state,
            hx,
            &user,
            id,
            Some(test),
            None,
            Some("Slice and print this test first.".into()),
        )
        .await;
    };
    let picks: Vec<&str> = [f.value.as_str(), f.value_y.as_str()]
        .into_iter()
        .take(test.axes())
        .map(str::trim)
        .collect();
    let changes = match c.changes(&picks, &ctx.now(Some(test))) {
        Ok(ch) => ch,
        Err(e) => return run_page(&state, hx, &user, id, Some(test), None, Some(e)).await,
    };
    let value = picks.join(",");
    cal_db::set_result(&state.db, id, test, &value, &changes).await?;
    let list: Vec<String> = changes.iter().map(|c| c.to_string()).collect();
    record(
        &state,
        ip,
        &user,
        ctx.printer.id,
        "calibration_result",
        format!(
            "run={id} test={} value={value} filament={} {}",
            test.slug(),
            ctx.filament.info.name,
            list.join(", ")
        ),
    )
    .await;
    let mut notice = format!("Recorded {} for the {}.", show_result(test, &value), test.title());
    let mut ctx = load(&state, id).await?;
    if f.save.is_some() && user.has(Role::Admin) {
        notice.push(' ');
        notice.push_str(&save_all(&state, ip, &user, &ctx).await?);
        ctx = load(&state, id).await?;
    }
    let next = ctx.next_open(Some(test));
    let mut error = None;
    match (f.then.as_deref(), next) {
        (Some("print_next"), Some(n)) => {
            let now = ctx.now(Some(n));
            let range = calibrate::defaults(n, &now.machine, &now.filament);
            match queue_test(&state, ip, &user, &ctx, n, range, true).await? {
                Ok(more) => {
                    notice.push(' ');
                    notice.push_str(&more);
                }
                Err(e) => error = Some(e),
            }
        }
        (Some("next" | "print_next"), _) => {}
        // Stay on this test.
        _ => return run_page(&state, hx, &user, id, Some(test), Some(notice), None).await,
    }
    let res = run_page(&state, hx, &user, id, next, Some(notice), error).await?;
    Ok(push_url(res, &format!("/ui/calibrate/runs/{id}")))
}

/// Writes every unsaved result to its profile (filament, machine or process), in order.
async fn save_all(state: &AppState, ip: ClientIp, user: &CurrentUser, ctx: &Ctx) -> Result<String, AppError> {
    let changes = ctx.carry(None);
    if changes.is_empty() {
        return Ok("Nothing to save.".into());
    }
    let mut saved = Vec::new();
    for (kind, id) in [
        ("filament", ctx.filament.info.id),
        ("machine", ctx.machine.info.id),
        ("process", ctx.process.info.id),
    ] {
        let mine: Vec<Change> = changes.iter().filter(|c| c.kind == kind).cloned().collect();
        if mine.is_empty() {
            continue;
        }
        let mut p = store::get_profile(&state.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        calibrate::apply_changes(&mut p.settings, kind, &mine);
        store::update_settings(&state.db, p.info.id, &p.settings).await?;
        // The last value of each key is what the profile has now.
        let mut last: Vec<&Change> = Vec::new();
        for c in &mine {
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
                "profile={} kind={kind} name={} calibration run={} {list}",
                p.info.id, p.info.name, ctx.run.id
            ),
        )
        .await;
        saved.push(format!("Saved to {}: {list}.", p.info.name));
    }
    let tests: Vec<Test> = ctx.steps.iter().filter(|s| s.unsaved()).map(|s| s.test).collect();
    cal_db::mark_saved(&state.db, ctx.run.id, &tests).await?;
    Ok(saved.join(" "))
}

/// A result as the page shows it (`48,52` → `X 48 · Y 52`), with its unit.
fn show_result(test: Test, r: &str) -> String {
    let unit = match test.unit() {
        "" => String::new(),
        u => format!(" {u}"),
    };
    match r.split_once(',') {
        Some((x, y)) if test.axes() == 2 => format!("X {x} · Y {y}{unit}"),
        _ => format!("{r}{unit}"),
    }
}

/// Saves the run's unsaved results to the profiles (admin).
pub async fn save(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let ctx = load(&state, id).await?;
    let notice = save_all(&state, ip, &user, &ctx).await?;
    run_page(&state, hx, &user, id, None, Some(notice), None).await
}

/// Skips a test (or, with `undo`, takes that back).
pub async fn skip(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    Form(f): Form<StepForm>,
) -> Result<Response, AppError> {
    let ctx = load(&state, id).await?;
    let test = test_of(&f, &ctx)?;
    let skipped = f.undo.is_none();
    cal_db::set_skipped(&state.db, id, test, skipped).await?;
    let (focus, notice) = if skipped {
        (
            ctx.next_open(Some(test)),
            format!("Skipped the {}.", test.title()),
        )
    } else {
        (Some(test), format!("The {} is back in.", test.title()))
    };
    run_page(&state, hx, &user, id, focus, Some(notice), None).await
}

/// Deletes a run (its jobs and any saved results stay).
pub async fn delete(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    slicing(&state)?;
    let run = cal_db::get_run(&state.db, id).await?.ok_or(AppError::NotFound)?;
    cal_db::delete_run(&state.db, id).await?;
    record(
        &state,
        ip,
        &user,
        run.printer_id,
        "calibration_delete",
        format!("run={id}"),
    )
    .await;
    let res = home_page(
        &state,
        hx,
        &user,
        Some(run.printer_id),
        Some(format!("Deleted calibration #{id}.")),
        None,
    )
    .await?;
    Ok(push_url(res, PATH))
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
