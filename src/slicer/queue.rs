//! The slice queue (PLAN §15.5): one background task that takes queued jobs oldest first,
//! slices each on the worker, uploads the G-code to its printer and, if asked, starts the print.
//!
//! Every job is something a user submitted and can follow on the Slice tab, so this isn't
//! polling in the sense of rule 2. When there's nothing queued the task just waits for a submit
//! (or its 10-minute retention sweep).
//!
//! A job's status in the database is the truth. Every step is guarded by the status it expects
//! (`db::slicer`), so a cancel from the web always wins: the handler sets `cancelled` and drops
//! the running step through [`Queue::cancel`], which drops the worker request (the worker then
//! kills Orca) or the upload.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::{Notify, oneshot};

use super::client::{SliceRequest, SliceStats};
use super::options::SliceOptions;
use super::{SlicerError, Slicing, storage};
use crate::db::slicer::{self as store, Job, JobStats, JobStatus};
use crate::db::{audit, now, printers};
use crate::moonraker::MoonrakerError;
use crate::state::AppState;

/// The folder under `gcodes/` that sliced files go to.
pub const OUTPUT_DIR: &str = "printpit";
/// Unfinished jobs one user may have at a time (a phone retrying can't fill the disk).
pub const MAX_ACTIVE_PER_USER: i64 = 5;
const SWEEP_EVERY: Duration = Duration::from_secs(600);
const INCOMING_MAX_AGE: Duration = Duration::from_secs(3600);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// `print_stats.state` values in which a new print may start.
const IDLE: [&str; 4] = ["standby", "complete", "cancelled", "error"];

pub struct Queue {
    wake: Notify,
    /// Cancel handles for the steps running now, by job id.
    running: std::sync::Mutex<HashMap<i64, oneshot::Sender<()>>>,
    started: AtomicBool,
    retry_ms: AtomicU64,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            wake: Notify::new(),
            running: std::sync::Mutex::new(HashMap::new()),
            started: AtomicBool::new(false),
            retry_ms: AtomicU64::new(5000),
        }
    }
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Queue")
    }
}

/// How a step ended, short of success.
#[derive(Debug)]
enum Stop {
    /// The job was cancelled, or its status changed under us. Nothing more to do.
    Cancelled,
    /// Try again later: the worker was busy or unreachable.
    Transient(String),
    Failed {
        error: String,
        log_tail: Vec<String>,
    },
}

impl Stop {
    fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
            log_tail: Vec::new(),
        }
    }
}

impl From<MoonrakerError> for Stop {
    fn from(e: MoonrakerError) -> Self {
        Self::failed(format!(
            "upload to the printer failed: {} ({})",
            e.detail(),
            e.label()
        ))
    }
}

impl From<sqlx::Error> for Stop {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "slice queue database error");
        Self::failed("a database error (see the server log)")
    }
}

impl Queue {
    /// Sets how long the first retry waits after a busy or unreachable worker (it doubles up to
    /// a minute). Tests shorten it.
    pub fn set_retry_delay(&self, d: Duration) {
        self.retry_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// Something was queued: look now rather than at the next sweep.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Drops the running step of `job`, if there is one. The caller sets the status.
    pub fn cancel(&self, job: i64) -> bool {
        match self.lock().remove(&job) {
            Some(tx) => {
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// Whether a step of `job` is running right now (for tests and diagnostics).
    pub fn is_running(&self, job: i64) -> bool {
        self.lock().contains_key(&job)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<()>>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the queue once: jobs a restart cut short are marked failed, queued ones resume.
    pub async fn start(self: &Arc<Self>, state: AppState) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        match store::interrupt_running(&state.db).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(jobs = n, "marked slice jobs interrupted by the restart as failed"),
            Err(e) => tracing::error!(error = %e, "couldn't mark interrupted slice jobs"),
        }
        let queue = self.clone();
        tokio::spawn(async move { queue.run(state).await });
    }

    /// Re-uploads a finished job's G-code ("upload again") without slicing it again. The
    /// caller has moved the job to `uploading`.
    pub fn spawn_upload(self: &Arc<Self>, state: AppState, job: i64) {
        let queue = self.clone();
        tokio::spawn(async move {
            let outcome = queue.cancellable(job, upload(&state, job)).await;
            finish(&state, job, outcome).await;
        });
    }

    async fn run(self: Arc<Self>, state: AppState) {
        let Some(slicing) = state.slicing.clone() else {
            return;
        };
        let mut last_sweep: Option<Instant> = None;
        let mut delay = self.retry_delay();
        loop {
            if last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
                sweep(&state, &slicing).await;
                last_sweep = Some(Instant::now());
            }
            let job = match store::claim_next(&state.db).await {
                Ok(Some(job)) => job,
                Ok(None) => {
                    tokio::select! {
                        () = self.wake.notified() => {}
                        () = tokio::time::sleep(SWEEP_EVERY) => {}
                    }
                    continue;
                }
                Err(e) => {
                    tracing::error!(error = %e, "slice queue can't read jobs");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let id = job.id;
            tracing::info!(job = id, model = %job.model_name, "slicing");
            let outcome = self.cancellable(id, process(&state, &slicing, job)).await;
            if let Err(Stop::Transient(note)) = &outcome {
                // Back in the queue, still first in line; wait before asking again.
                if let Err(e) = store::requeue(&state.db, id, Some(note)).await {
                    tracing::error!(error = %e, job = id, "couldn't requeue a slice job");
                }
                tracing::info!(job = id, %note, delay_ms = delay.as_millis() as u64, "slicer unavailable; will retry");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RETRY_DELAY);
                continue;
            }
            delay = self.retry_delay();
            finish(&state, id, outcome).await;
        }
    }

    fn retry_delay(&self) -> Duration {
        Duration::from_millis(self.retry_ms.load(Ordering::Relaxed))
    }

    /// Runs a step that [`Queue::cancel`] can drop.
    async fn cancellable(&self, job: i64, step: impl Future<Output = Result<(), Stop>>) -> Result<(), Stop> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(job, tx);
        let out = tokio::select! {
            r = step => r,
            _ = rx => Err(Stop::Cancelled),
        };
        self.lock().remove(&job);
        out
    }
}

/// Records how a job ended and tells the webhook.
async fn finish(state: &AppState, id: i64, outcome: Result<(), Stop>) {
    match outcome {
        Ok(()) => event(state, id, "slice_done").await,
        Err(Stop::Cancelled) => tracing::info!(job = id, "slice job stopped (cancelled)"),
        Err(Stop::Transient(note)) => {
            // Only uploads end up here (slices are requeued): treat as a failure.
            let _ = store::fail(&state.db, id, &note, None).await;
            event(state, id, "slice_failed").await;
        }
        Err(Stop::Failed { error, log_tail }) => {
            tracing::info!(job = id, %error, "slice job failed");
            let tail = (!log_tail.is_empty()).then(|| log_tail.join("\n"));
            match store::fail(&state.db, id, &error, tail.as_deref()).await {
                Ok(true) => event(state, id, "slice_failed").await,
                Ok(false) => {} // cancelled meanwhile
                Err(e) => tracing::error!(error = %e, job = id, "couldn't record a failed slice job"),
            }
        }
    }
}

/// Slices a claimed job, then uploads it.
async fn process(state: &AppState, slicing: &Slicing, job: Job) -> Result<(), Stop> {
    let printer = match job.printer_id {
        Some(id) => printers::get(&state.db, id).await?,
        None => None,
    }
    .ok_or_else(|| Stop::failed("its printer was deleted"))?;
    if !printer.enabled {
        return Err(Stop::failed(format!("{} is disabled", printer.name)));
    }
    let options = SliceOptions::from_json(&job.options_json);
    let profile = async |id: Option<i64>, what: &str| -> Result<store::StoredProfile, Stop> {
        match id {
            Some(id) => store::get_profile(&state.db, id).await?,
            None => None,
        }
        .ok_or_else(|| Stop::failed(format!("its {what} profile was deleted")))
    };
    let machine = profile(job.machine_profile_id, "machine").await?;
    let (process, filament) = if options.project_settings {
        (None, None)
    } else {
        let mut p = profile(job.process_id, "process").await?.settings;
        options.apply(&mut p, &machine.settings).map_err(Stop::failed)?;
        (
            Some(p),
            Some(profile(job.filament_id, "filament").await?.settings),
        )
    };
    let ext = storage::model_extension(&job.model_name).ok_or_else(|| Stop::failed("unknown model type"))?;
    let model = slicing
        .storage
        .model_path(job.id, &ext)
        .filter(|p| p.exists())
        .ok_or_else(|| Stop::failed("the model file is gone (removed after the retention period)"))?;
    let worker_options = options.worker();
    let req = SliceRequest {
        machine: &machine.settings,
        process: process.as_ref(),
        filament: filament.as_ref(),
        options: &worker_options,
        model: &model,
    };
    let stats = match slicing.client.slice(&req, &slicing.storage, job.id).await {
        Ok(s) => s,
        Err(e) if e.is_transient() => return Err(Stop::Transient(waiting(&e))),
        Err(e) => {
            return Err(Stop::Failed {
                error: format!("{} ({})", e.detail(), e.label()),
                log_tail: e.log_tail().to_vec(),
            });
        }
    };
    if !store::sliced(&state.db, job.id, &job_stats(&stats)).await? {
        return Err(Stop::Cancelled);
    }
    upload(state, job.id).await
}

fn waiting(e: &SlicerError) -> String {
    match e {
        SlicerError::Busy { .. } => "waiting: the slicer is busy with another job".into(),
        _ => format!("waiting for the slicer: {}", e.detail()),
    }
}

fn job_stats(s: &SliceStats) -> JobStats {
    JobStats {
        est_seconds: s.print_time_s,
        filament_mm: s.filament_mm,
        filament_g: s.filament_g,
        layers: s.layers,
        gcode_size: i64::try_from(s.bytes).ok(),
    }
}

/// Uploads a sliced job's G-code to `gcodes/printpit/` and starts the print if asked.
async fn upload(state: &AppState, id: i64) -> Result<(), Stop> {
    let slicing = state.slicing.clone().ok_or(Stop::Cancelled)?;
    let job = store::get_job(&state.db, id).await?.ok_or(Stop::Cancelled)?;
    if job.status() != JobStatus::Uploading {
        return Err(Stop::Cancelled);
    }
    let printer = match job.printer_id {
        Some(pid) => printers::get(&state.db, pid).await?,
        None => None,
    }
    .ok_or_else(|| Stop::failed("its printer was deleted"))?;
    let options = SliceOptions::from_json(&job.options_json);
    let name = options
        .output
        .clone()
        .or_else(|| super::options::output_name("", &job.model_name, "", ""))
        .ok_or_else(|| Stop::failed("no file name for the G-code"))?;
    let client = state.printer_client(&printer)?;
    let file = tokio::fs::File::open(slicing.storage.output_path(id))
        .await
        .map_err(|_| Stop::failed("the G-code is gone (removed after the retention period)"))?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    client.upload(OUTPUT_DIR, &name, false, body).await?;
    state.dirs.invalidate(printer.id);
    let path = format!("{OUTPUT_DIR}/{name}");
    if !store::uploaded(&state.db, id, &path).await? {
        return Err(Stop::Cancelled);
    }
    record(state, &job, "slice_job_upload", format!("job={id} file={path}")).await;

    if job.start_print {
        let note = match printer_state(&client).await {
            Ok(s) if IDLE.contains(&s.as_str()) => {
                match client
                    .post_action("/printer/print/start", &[("filename", path.as_str())])
                    .await
                {
                    Ok(()) => {
                        record(state, &job, "slice_job_print", format!("job={id} file={path}")).await;
                        None
                    }
                    Err(e) => Some(format!("not started: {}", e.detail())),
                }
            }
            Ok(s) => Some(format!("not started: the printer was {s}")),
            Err(e) => Some(format!("not started: {}", e.detail())),
        };
        if let Some(note) = note {
            store::note(&state.db, id, &note).await?;
        }
    }
    Ok(())
}

/// `print_stats.state`: `standby`, `printing`, `paused`, `complete`, `cancelled` or `error`.
pub async fn printer_state(client: &crate::moonraker::MoonrakerClient) -> Result<String, MoonrakerError> {
    let mut sub = Map::new();
    sub.insert("print_stats".into(), json!(["state"]));
    let status = client.objects_query(&sub).await?;
    Ok(status
        .get("print_stats")
        .and_then(|p| p.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string())
}

pub fn is_idle(state: &str) -> bool {
    IDLE.contains(&state)
}

async fn record(state: &AppState, job: &Job, action: &'static str, detail: String) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: job.user_id,
            printer_id: job.printer_id,
            action,
            detail: Some(detail),
            ip: None,
        },
    )
    .await;
}

/// `slice_done` / `slice_failed` for the webhook (when one is configured).
async fn event(state: &AppState, id: i64, kind: &'static str) {
    if !state.notifier.enabled() {
        return;
    }
    let Ok(Some(job)) = store::get_job(&state.db, id).await else {
        return;
    };
    let printer = match job.printer_id {
        Some(pid) => printers::get(&state.db, pid).await.ok().flatten().map(|p| p.name),
        None => None,
    }
    .unwrap_or_else(|| "a deleted printer".into());
    state.notifier.send(payload(kind, &printer, &job));
}

/// The webhook body: structured fields plus `text`/`content` for chat webhooks. Names the
/// printer, never its address.
pub fn payload(kind: &str, printer: &str, job: &Job) -> Value {
    let text = if kind == "slice_done" {
        let mut t = format!(
            "{printer}: sliced {} → {}",
            job.model_name,
            job.output_name.as_deref().unwrap_or("?")
        );
        if let Some(s) = job.est_seconds {
            t.push_str(&format!(" ({})", crate::web::widgets::duration(s as f64)));
        }
        if let Some(note) = job.error.as_deref().filter(|n| !n.is_empty()) {
            t.push_str(&format!("; {note}"));
        }
        t
    } else {
        format!(
            "{printer}: slicing {} failed: {}",
            job.model_name,
            job.error.as_deref().unwrap_or("unknown error")
        )
    };
    json!({
        "event": kind,
        "printer": printer,
        "job": job.id,
        "model": job.model_name,
        "output": job.output_name,
        "estimated_seconds": job.est_seconds,
        "text": text,
        "content": text,
    })
}

/// Deletes files of jobs past the retention period (and orphaned or half-uploaded ones).
async fn sweep(state: &AppState, slicing: &Slicing) {
    let cutoff = now() - slicing.config.retention.as_secs() as i64;
    let keep = match store::retained_job_ids(&state.db, cutoff).await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "retention sweep skipped");
            return;
        }
    };
    let mut removed = 0usize;
    match slicing.storage.job_ids().await {
        Ok(ids) => {
            for id in ids.into_iter().filter(|id| keep.binary_search(id).is_err()) {
                match slicing.storage.remove_job(id).await {
                    Ok(()) => removed += 1,
                    Err(e) => tracing::warn!(error = %e, job = id, "couldn't remove old slice files"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "retention sweep couldn't list slice files"),
    }
    let incoming = slicing
        .storage
        .sweep_incoming(INCOMING_MAX_AGE)
        .await
        .unwrap_or(0);
    if removed + incoming > 0 {
        tracing::info!(jobs = removed, incoming, "removed old slice files");
    }
}

/// Runs the retention sweep now (tests; the queue runs it every 10 minutes).
pub async fn sweep_now(state: &AppState) {
    if let Some(s) = state.slicing.clone() {
        sweep(state, &s).await;
    }
}
