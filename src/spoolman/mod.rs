//! Filament tracking in Spoolman, without the printer ever talking to Spoolman.
//!
//! printpit reads each tracked printer's Moonraker job history and books every finished job's
//! filament to the spool(s) an operator said are loaded ([`plan`] works out the amounts).
//! Moonraker's own `[spoolman]` component stays off.
//!
//! **Like notifications, tracking is an exception to the lazy-loading rule.** For every enabled
//! printer an admin has turned tracking on for, [`Spoolman`] holds a hub [`Subscription`] for
//! as long as printpit runs, so it sees jobs end. It then reads the history (one HTTP call)
//! a few seconds later, and again once the connection comes back after an outage or a restart.
//! The history is the source of truth: nothing is lost while printpit is down, and the
//! bookings table makes sure no job is booked twice.

pub mod client;
pub mod plan;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::SpoolmanConfig;
use crate::crypto::Cipher;
use crate::db::spoolman::{self as store, NewBooking};
use crate::db::{Db, audit, printers};
use crate::moonraker::hub::{PrinterHub, Subscription};
use crate::moonraker::status::Link;
use crate::moonraker::types::HistoryJob;
use crate::moonraker::{self, Egress, MoonrakerClient, MoonrakerError};
use crate::notify::Tracker;
use client::{SpoolmanClient, SpoolmanError};
use plan::Part;

/// History page size while catching up.
const PAGE: u64 = 50;
/// Never read more than this many jobs in one sync (a first sync after a very long outage
/// catches up over several).
const MAX_JOBS_PER_SYNC: u64 = 1000;
/// Moonraker writes the history entry as the job ends; reading it straight away can still find
/// it `in_progress`. The watcher reads after this, and once more after ten times this.
const SETTLE: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("filament tracking is off for this printer")]
    Off,
    #[error("{}: {}", .0.label(), .0.detail())]
    Moonraker(MoonrakerError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

impl SyncError {
    /// Safe to show: Moonraker errors carry no addresses.
    pub fn detail(&self) -> String {
        match self {
            Self::Moonraker(MoonrakerError::Http { status: 404, .. }) => {
                "Job history is off on this printer. Add a [history] section to moonraker.conf.".into()
            }
            Self::Db(_) => "database error".into(),
            e => e.to_string(),
        }
    }
}

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncReport {
    /// Jobs booked (or partly booked) to Spoolman.
    pub booked: usize,
    /// Jobs with nothing to book (no filament, no spool assigned, can't split).
    pub skipped: usize,
    /// Jobs Spoolman didn't take (retried next time if it was down).
    pub failed: usize,
}

impl SyncReport {
    pub fn describe(&self) -> String {
        if *self == Self::default() {
            return "Nothing new to book.".into();
        }
        let mut parts = Vec::new();
        let plural = |n: usize| if n == 1 { "job" } else { "jobs" };
        if self.booked > 0 {
            parts.push(format!("booked {} {}", self.booked, plural(self.booked)));
        }
        if self.skipped > 0 {
            parts.push(format!("{} with nothing to book", self.skipped));
        }
        if self.failed > 0 {
            parts.push(format!("{} not taken by Spoolman", self.failed));
        }
        let mut s = parts.join(", ");
        if let Some(first) = s.get(..1) {
            s = first.to_uppercase() + &s[1..];
        }
        s + "."
    }
}

pub struct Spoolman {
    db: Db,
    hub: Arc<PrinterHub>,
    egress: Egress,
    cipher: Arc<Cipher>,
    pub client: SpoolmanClient,
    watches: Mutex<HashMap<i64, JoinHandle<()>>>,
    /// One sync at a time per printer.
    locks: StdMutex<HashMap<i64, Arc<Mutex<()>>>>,
    settle_ms: AtomicU64,
}

impl Spoolman {
    pub fn new(
        config: &SpoolmanConfig,
        db: Db,
        hub: Arc<PrinterHub>,
        egress: Egress,
        cipher: Arc<Cipher>,
    ) -> Result<Arc<Self>, reqwest::Error> {
        Ok(Arc::new(Self {
            client: SpoolmanClient::new(config)?,
            db,
            hub,
            egress,
            cipher,
            watches: Mutex::new(HashMap::new()),
            locks: StdMutex::new(HashMap::new()),
            settle_ms: AtomicU64::new(SETTLE.as_millis() as u64),
        }))
    }

    /// Tests use a short delay.
    pub fn set_settle_delay(&self, d: Duration) {
        self.settle_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    fn settle(&self) -> Duration {
        Duration::from_millis(self.settle_ms.load(Ordering::Relaxed))
    }

    /// At startup: bookings cut short by a restart are marked `unknown`, then the tracked
    /// printers are watched.
    pub async fn start(self: &Arc<Self>) {
        match store::interrupt_sending(&self.db).await {
            Ok(0) => {}
            Ok(n) => tracing::warn!(bookings = n, "spoolman bookings interrupted by a restart"),
            Err(e) => tracing::warn!(error = %e, "couldn't check interrupted spoolman bookings"),
        }
        self.sync_watches().await;
    }

    /// Printers currently watched (for tests and diagnostics).
    pub async fn watching(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.watches.lock().await.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Starts watching newly tracked printers and stops watching the rest. Called at startup,
    /// after every printer change and when tracking is turned on or off.
    pub async fn sync_watches(self: &Arc<Self>) {
        let mut watches = self.watches.lock().await;
        let wanted = match store::tracked(&self.db).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't load printers for spoolman tracking");
                return;
            }
        };
        watches.retain(|id, task| {
            let keep = wanted.contains(id) && !task.is_finished();
            if !keep {
                task.abort();
                tracing::info!(printer_id = id, "stopped tracking filament");
            }
            keep
        });
        for id in wanted {
            watches.entry(id).or_insert_with(|| {
                tracing::info!(printer_id = id, "tracking filament");
                tokio::spawn(watch(self.clone(), self.hub.subscribe(id), id))
            });
        }
    }

    fn lock_for(&self, printer_id: i64) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(printer_id).or_default().clone()
    }

    /// Retries failed bookings, then books every job that ended since the last sync.
    pub async fn sync(&self, printer_id: i64) -> Result<SyncReport, SyncError> {
        let lock = self.lock_for(printer_id);
        let _held = lock.lock().await;
        self.sync_locked(printer_id).await
    }

    /// Books what's finished to the spools loaded until now, then records the new spools. A
    /// failed sync doesn't stop the change; its error is returned alongside.
    pub async fn assign(
        &self,
        printer_id: i64,
        slots: &[Option<i64>],
    ) -> Result<Option<SyncError>, sqlx::Error> {
        let lock = self.lock_for(printer_id);
        let _held = lock.lock().await;
        let synced = self.sync_locked(printer_id).await;
        store::set_slots(&self.db, printer_id, slots).await?;
        Ok(match synced {
            Err(SyncError::Off) | Ok(_) => None,
            Err(e) => Some(e),
        })
    }

    async fn sync_locked(&self, printer_id: i64) -> Result<SyncReport, SyncError> {
        let settings = store::settings(&self.db, printer_id).await?;
        let printer = printers::get(&self.db, printer_id).await?;
        let Some(printer) = printer.filter(|p| settings.tracking && p.enabled) else {
            return Err(SyncError::Off);
        };
        let mut report = SyncReport::default();
        for b in store::failed(&self.db, printer_id).await? {
            let ok = self
                .send(printer_id, b.id, "failed", &b.job_id, &b.filename, b.parts)
                .await?;
            if !ok {
                report.failed += 1;
            }
        }
        let slots = store::slots(&self.db, printer_id, settings.tools).await?;
        let result = match moonraker::client_for(&self.egress, &self.cipher, &printer) {
            Ok(client) => {
                self.book_new(&client, printer_id, settings.since, &slots, &mut report)
                    .await
            }
            Err(e) => Err(SyncError::Moonraker(e)),
        };
        let error = result.as_ref().err().map(SyncError::detail);
        store::set_sync_result(&self.db, printer_id, error.as_deref()).await?;
        result.map(|()| report)
    }

    async fn book_new(
        &self,
        client: &MoonrakerClient,
        printer_id: i64,
        since: f64,
        slots: &[Option<i64>],
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        let mut start = 0;
        // The cursor moves past a settled prefix only: never past a job still printing.
        let mut cursor = None;
        let mut blocked = false;
        loop {
            let page = client
                .history_since(since, start, PAGE)
                .await
                .map_err(SyncError::Moonraker)?;
            let n = page.jobs.len() as u64;
            for job in page.jobs {
                if job.status == "in_progress" {
                    blocked = true;
                    continue;
                }
                if !blocked {
                    cursor = Some(job.start_time);
                }
                self.book_job(printer_id, &job, slots, report).await?;
            }
            start += n;
            if n < PAGE || start >= MAX_JOBS_PER_SYNC {
                break;
            }
        }
        // Moonraker's `since` is strict (`start_time > since`), so a cursor at the last job's own
        // start would hide a later job that started in the same instant. Keeping it a second
        // behind re-reads that job; the bookings table ignores it.
        if let Some(c) = cursor {
            store::advance_cursor(&self.db, printer_id, c - 1.0).await?;
        }
        Ok(())
    }

    async fn book_job(
        &self,
        printer_id: i64,
        job: &HistoryJob,
        slots: &[Option<i64>],
        report: &mut SyncReport,
    ) -> Result<(), SyncError> {
        let plan = plan::plan(job.filament_used, &job.metadata, slots);
        let status = if plan.parts.is_empty() {
            "skipped"
        } else {
            "sending"
        };
        let note = plan.notes.join(" ");
        let Some(id) = store::insert_booking(
            &self.db,
            NewBooking {
                printer_id,
                job_id: &job.job_id,
                job_start: job.start_time,
                filename: &job.filename,
                job_status: &job.status,
                job_end: job.end_time,
                used_mm: job.filament_used,
                status,
                parts: &plan.parts,
                note: &note,
            },
        )
        .await?
        else {
            return Ok(()); // already looked at
        };
        if plan.parts.is_empty() {
            report.skipped += 1;
            return Ok(());
        }
        if self
            .send(printer_id, id, "sending", &job.job_id, &job.filename, plan.parts)
            .await?
        {
            report.booked += 1;
        } else {
            report.failed += 1;
        }
        Ok(())
    }

    /// Sends the parts not sent yet. `true` when all of them are now in Spoolman.
    async fn send(
        &self,
        printer_id: i64,
        booking_id: i64,
        from: &str,
        job_id: &str,
        filename: &str,
        mut parts: Vec<Part>,
    ) -> Result<bool, sqlx::Error> {
        let mut errors = Vec::new();
        let mut retry = true;
        let mut sent = Vec::new();
        let multi = parts.iter().any(|p| p.tool > 0);
        for p in parts.iter_mut().filter(|p| !p.done) {
            match self.client.use_filament(p.spool_id, p.amount()).await {
                Ok(()) => {
                    p.done = true;
                    sent.push(describe_part(p, multi));
                }
                Err(e) => {
                    // A spool that's gone or a request Spoolman refuses won't work next time.
                    retry &= e.is_transient() || e == SpoolmanError::Unauthorized;
                    errors.push(format!("{}: {}", describe_part(p, multi), e.detail()));
                }
            }
        }
        let status = match (errors.is_empty(), retry) {
            (true, _) => "booked",
            (false, true) => "failed",
            (false, false) => "error",
        };
        let error = (!errors.is_empty()).then(|| errors.join("; "));
        store::finish_booking(&self.db, booking_id, from, status, &parts, error.as_deref()).await?;
        if !sent.is_empty() {
            audit::record(
                &self.db,
                audit::Entry {
                    user_id: None,
                    printer_id: Some(printer_id),
                    action: "spoolman_use",
                    detail: Some(format!("job {job_id} {filename}: {}", sent.join(", "))),
                    ip: None,
                },
            )
            .await;
        }
        if let Some(e) = &error {
            tracing::warn!(printer_id, job_id, error = %e, "spoolman booking failed");
        }
        Ok(errors.is_empty())
    }
}

/// `T1 → #12 1.52 m`, or `#12 1.52 m` on a one-tool printer.
pub fn describe_part(p: &Part, multi: bool) -> String {
    let amount = p.amount().describe();
    if multi {
        format!("T{} → #{} {amount}", p.tool, p.spool_id)
    } else {
        format!("#{} {amount}", p.spool_id)
    }
}

/// Holds the printer's connection open and syncs when a job ends or the connection comes back.
async fn watch(sp: Arc<Spoolman>, mut sub: Subscription, printer_id: i64) {
    let mut tracker = Tracker::default();
    let mut was_up = false;
    loop {
        let (ended, up) = {
            let snap = sub.rx.borrow_and_update();
            let up = matches!(snap.link, Link::Live | Link::Polling | Link::NotReady { .. });
            (tracker.observe(&snap).is_some(), up)
        };
        if ended || (up && !was_up) {
            let sp = sp.clone();
            tokio::spawn(async move {
                let settle = sp.settle();
                // Twice: the second catches a job whose history entry wasn't final yet.
                for wait in [settle, settle * 9] {
                    tokio::time::sleep(wait).await;
                    match sp.sync(printer_id).await {
                        Ok(r) if r != SyncReport::default() => {
                            tracing::info!(printer_id, result = %r.describe(), "spoolman sync");
                        }
                        Ok(_) | Err(SyncError::Off) => {}
                        Err(e) => tracing::warn!(printer_id, error = %e, "spoolman sync failed"),
                    }
                }
            });
        }
        was_up = up;
        if sub.rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports() {
        assert_eq!(SyncReport::default().describe(), "Nothing new to book.");
        let r = SyncReport {
            booked: 2,
            skipped: 1,
            failed: 0,
        };
        assert_eq!(r.describe(), "Booked 2 jobs, 1 with nothing to book.");
        let r = SyncReport {
            booked: 0,
            skipped: 0,
            failed: 1,
        };
        assert_eq!(r.describe(), "1 not taken by Spoolman.");
    }
}
