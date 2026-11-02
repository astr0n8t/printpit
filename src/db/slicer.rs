//! Slicing tables (migration 0007): the profile library, each printer's machine profile and
//! defaults, and slice jobs.
//!
//! Job status changes are guarded by the status they expect to leave, so a job cancelled while
//! the queue works on it stays cancelled: the queue's next step finds the status changed and
//! does nothing.

use sqlx::Row;

use super::{Db, now};
use crate::slicer::{Kind, Profile};

// ── profiles ─────────────────────────────────────────────────────────────────────────────

/// A library entry, without its settings (for lists).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProfileInfo {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub source: String,
    pub vendor: Option<String>,
    pub updated_at: i64,
}

impl ProfileInfo {
    pub fn kind(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }
}

/// A library entry with its flattened settings.
#[derive(Debug, Clone)]
pub struct StoredProfile {
    pub info: ProfileInfo,
    pub settings: Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// From the worker's bundled Orca library.
    System,
    /// An imported preset or bundle.
    User,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }
}

pub struct NewProfile<'a> {
    pub kind: Kind,
    pub name: &'a str,
    pub source: Source,
    pub vendor: Option<&'a str>,
    pub settings: &'a Profile,
}

pub async fn list_profiles(db: &Db, kind: Option<Kind>) -> Result<Vec<ProfileInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, kind, name, source, vendor, updated_at FROM slicer_profiles
         WHERE ?1 IS NULL OR kind = ?1
         ORDER BY CASE kind WHEN 'machine' THEN 0 WHEN 'process' THEN 1 ELSE 2 END, name COLLATE NOCASE",
    )
    .bind(kind.map(Kind::as_str))
    .fetch_all(db)
    .await
}

pub async fn get_profile(db: &Db, id: i64) -> Result<Option<StoredProfile>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, kind, name, source, vendor, updated_at, settings_json FROM slicer_profiles WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    row.map(|r| stored(&r)).transpose()
}

/// Every profile of a kind with its settings, by name (for the Slice form's compatibility filter).
pub async fn profiles_with_settings(db: &Db, kind: Kind) -> Result<Vec<StoredProfile>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, kind, name, source, vendor, updated_at, settings_json FROM slicer_profiles
         WHERE kind = ? ORDER BY name COLLATE NOCASE",
    )
    .bind(kind.as_str())
    .fetch_all(db)
    .await?;
    rows.iter().map(stored).collect()
}

pub async fn find_profile(db: &Db, kind: Kind, name: &str) -> Result<Option<ProfileInfo>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, kind, name, source, vendor, updated_at FROM slicer_profiles WHERE kind = ? AND name = ?",
    )
    .bind(kind.as_str())
    .bind(name)
    .fetch_optional(db)
    .await
}

fn stored(r: &sqlx::sqlite::SqliteRow) -> Result<StoredProfile, sqlx::Error> {
    let raw: String = r.try_get("settings_json")?;
    let settings: Profile = serde_json::from_str(&raw).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(StoredProfile {
        info: ProfileInfo {
            id: r.try_get("id")?,
            kind: r.try_get("kind")?,
            name: r.try_get("name")?,
            source: r.try_get("source")?,
            vendor: r.try_get("vendor")?,
            updated_at: r.try_get("updated_at")?,
        },
        settings,
    })
}

fn encode(p: &Profile) -> Result<String, sqlx::Error> {
    serde_json::to_string(p).map_err(|e| sqlx::Error::Encode(Box::new(e)))
}

/// Adds a profile. A name that exists for that kind is a unique violation; use
/// [`find_profile`] + [`update_profile`] to replace one.
pub async fn create_profile(db: &Db, p: &NewProfile<'_>) -> Result<i64, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO slicer_profiles (kind, name, source, vendor, settings_json, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(p.kind.as_str())
    .bind(p.name)
    .bind(p.source.as_str())
    .bind(p.vendor)
    .bind(encode(p.settings)?)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

/// Replaces a profile's settings (and where it came from), e.g. on re-import.
pub async fn update_profile(
    db: &Db,
    id: i64,
    source: Source,
    vendor: Option<&str>,
    settings: &Profile,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slicer_profiles SET source = ?, vendor = ?, settings_json = ?, updated_at = ? WHERE id = ?",
    )
    .bind(source.as_str())
    .bind(vendor)
    .bind(encode(settings)?)
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeleteProfile {
    Deleted,
    NotFound,
    /// Still used: printer names, plus a count of unfinished jobs.
    InUse {
        printers: Vec<String>,
        jobs: i64,
    },
}

/// Deletes a profile unless a printer uses it (as its machine or a default) or an unfinished
/// job does. Checked and deleted in one transaction.
pub async fn delete_profile(db: &Db, id: i64) -> Result<DeleteProfile, sqlx::Error> {
    let mut tx = db.begin().await?;
    let printers: Vec<String> = sqlx::query_scalar(
        "SELECT p.name FROM printer_slicer s JOIN printers p ON p.id = s.printer_id
         WHERE ?1 IN (s.machine_profile_id, s.default_process_id, s.default_filament_id)
         ORDER BY p.name",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM slice_jobs
         WHERE status IN ('queued', 'slicing', 'uploading')
           AND ?1 IN (machine_profile_id, process_id, filament_id)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !printers.is_empty() || jobs > 0 {
        return Ok(DeleteProfile::InUse { printers, jobs });
    }
    let res = sqlx::query("DELETE FROM slicer_profiles WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(if res.rows_affected() == 1 {
        DeleteProfile::Deleted
    } else {
        DeleteProfile::NotFound
    })
}

// ── per-printer settings ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PrinterSlicer {
    pub printer_id: i64,
    pub machine_profile_id: i64,
    pub default_process_id: Option<i64>,
    pub default_filament_id: Option<i64>,
}

pub async fn printer_slicer(db: &Db, printer_id: i64) -> Result<Option<PrinterSlicer>, sqlx::Error> {
    sqlx::query_as(
        "SELECT printer_id, machine_profile_id, default_process_id, default_filament_id
         FROM printer_slicer WHERE printer_id = ?",
    )
    .bind(printer_id)
    .fetch_optional(db)
    .await
}

/// Sets a printer's machine profile and defaults. Returns `false` (and changes nothing) if a
/// profile doesn't exist or is of the wrong kind.
pub async fn set_printer_slicer(db: &Db, s: &PrinterSlicer) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "INSERT INTO printer_slicer (printer_id, machine_profile_id, default_process_id, default_filament_id,
                                     updated_at)
         SELECT ?1, ?2, ?3, ?4, ?5
         WHERE EXISTS (SELECT 1 FROM printers WHERE id = ?1)
           AND EXISTS (SELECT 1 FROM slicer_profiles WHERE id = ?2 AND kind = 'machine')
           AND (?3 IS NULL OR EXISTS (SELECT 1 FROM slicer_profiles WHERE id = ?3 AND kind = 'process'))
           AND (?4 IS NULL OR EXISTS (SELECT 1 FROM slicer_profiles WHERE id = ?4 AND kind = 'filament'))
         ON CONFLICT (printer_id) DO UPDATE SET
             machine_profile_id = excluded.machine_profile_id,
             default_process_id = excluded.default_process_id,
             default_filament_id = excluded.default_filament_id,
             updated_at = excluded.updated_at",
    )
    .bind(s.printer_id)
    .bind(s.machine_profile_id)
    .bind(s.default_process_id)
    .bind(s.default_filament_id)
    .bind(now())
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn clear_printer_slicer(db: &Db, printer_id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM printer_slicer WHERE printer_id = ?")
        .bind(printer_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

// ── jobs ─────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Slicing,
    Uploading,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "slicing" => Self::Slicing,
            "uploading" => Self::Uploading,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Slicing => "slicing",
            Self::Uploading => "uploading",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Not finished yet: the job still holds its files, its profiles and a place in the queue.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Slicing | Self::Uploading)
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub user_id: Option<i64>,
    pub printer_id: Option<i64>,
    pub model_name: String,
    pub model_size: i64,
    pub status: String,
    pub machine_profile_id: Option<i64>,
    pub process_id: Option<i64>,
    pub filament_id: Option<i64>,
    pub options_json: String,
    pub start_print: bool,
    pub output_name: Option<String>,
    pub est_seconds: Option<i64>,
    pub filament_mm: Option<f64>,
    pub filament_g: Option<f64>,
    pub layers: Option<i64>,
    pub gcode_size: Option<i64>,
    pub error: Option<String>,
    pub log_tail: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

impl Job {
    /// The status; an unknown value (impossible by the CHECK constraint) reads as failed.
    pub fn status(&self) -> JobStatus {
        JobStatus::parse(&self.status).unwrap_or(JobStatus::Failed)
    }
}

pub struct NewJob<'a> {
    pub user_id: i64,
    pub printer_id: i64,
    pub model_name: &'a str,
    pub model_size: i64,
    pub machine_profile_id: Option<i64>,
    pub process_id: Option<i64>,
    pub filament_id: Option<i64>,
    pub options_json: &'a str,
    pub start_print: bool,
}

/// What the slicer reported, stored when a job moves on to uploading.
#[derive(Debug, Clone, Default)]
pub struct JobStats {
    pub est_seconds: Option<i64>,
    pub filament_mm: Option<f64>,
    pub filament_g: Option<f64>,
    pub layers: Option<i64>,
    pub gcode_size: Option<i64>,
}

/// The reason on jobs a restart cut short.
pub const INTERRUPTED: &str = "interrupted: printpit restarted while this job was running";

/// Creates a queued job. Pass a transaction to put its model in place before the queue can
/// see it (the web submit does).
pub async fn create_job<'e>(
    db: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    j: &NewJob<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO slice_jobs (user_id, printer_id, model_name, model_size, status, machine_profile_id,
                                 process_id, filament_id, options_json, start_print, created_at)
         VALUES (?, ?, ?, ?, 'queued', ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(j.user_id)
    .bind(j.printer_id)
    .bind(j.model_name)
    .bind(j.model_size)
    .bind(j.machine_profile_id)
    .bind(j.process_id)
    .bind(j.filament_id)
    .bind(j.options_json)
    .bind(j.start_print)
    .bind(now())
    .fetch_one(db)
    .await
}

pub async fn get_job(db: &Db, id: i64) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM slice_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

/// Newest first, for one printer or all.
pub async fn recent_jobs(db: &Db, printer_id: Option<i64>, limit: i64) -> Result<Vec<Job>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM slice_jobs WHERE ?1 IS NULL OR printer_id = ?1 ORDER BY id DESC LIMIT ?2")
        .bind(printer_id)
        .bind(limit)
        .fetch_all(db)
        .await
}

/// A user's unfinished jobs, for the per-user cap.
pub async fn active_jobs_for_user(db: &Db, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM slice_jobs WHERE user_id = ? AND status IN ('queued', 'slicing', 'uploading')",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

/// Takes the oldest queued job and marks it slicing, atomically.
pub async fn claim_next(db: &Db) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as(
        "UPDATE slice_jobs SET status = 'slicing', started_at = ?, error = NULL, log_tail = NULL
         WHERE id = (SELECT id FROM slice_jobs WHERE status = 'queued' ORDER BY id LIMIT 1)
           AND status = 'queued'
         RETURNING *",
    )
    .bind(now())
    .fetch_optional(db)
    .await
}

/// Puts a slicing job back in the queue (the worker was busy or unreachable).
/// `note` says why it's waiting (shown on the job; cleared when it's claimed again).
pub async fn requeue(db: &Db, id: i64, note: Option<&str>) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'queued', started_at = NULL, error = ? WHERE id = ? AND status = 'slicing'",
    )
    .bind(note)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// A finished job whose G-code is still on disk goes back to uploading ("upload again").
pub async fn start_upload(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'uploading', error = NULL, log_tail = NULL, output_name = NULL,
                               finished_at = NULL
         WHERE id = ? AND status IN ('done', 'failed', 'cancelled') AND gcode_size IS NOT NULL",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// A remark on a job, e.g. why a finished one didn't start printing.
pub async fn note(db: &Db, id: i64, note: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE slice_jobs SET error = ? WHERE id = ?")
        .bind(note)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// slicing → uploading, with what the slicer reported.
pub async fn sliced(db: &Db, id: i64, stats: &JobStats) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'uploading', est_seconds = ?, filament_mm = ?, filament_g = ?,
                               layers = ?, gcode_size = ?
         WHERE id = ? AND status = 'slicing'",
    )
    .bind(stats.est_seconds)
    .bind(stats.filament_mm)
    .bind(stats.filament_g)
    .bind(stats.layers)
    .bind(stats.gcode_size)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// uploading → done, recording where the G-code went.
pub async fn uploaded(db: &Db, id: i64, output_name: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'done', output_name = ?, finished_at = ?
         WHERE id = ? AND status = 'uploading'",
    )
    .bind(output_name)
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Any unfinished status → failed.
pub async fn fail(db: &Db, id: i64, error: &str, log_tail: Option<&str>) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'failed', error = ?, log_tail = ?, finished_at = ?
         WHERE id = ? AND status IN ('queued', 'slicing', 'uploading')",
    )
    .bind(error)
    .bind(log_tail)
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Any unfinished status → cancelled. `false` if the job had already finished.
pub async fn cancel(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'cancelled', finished_at = ?
         WHERE id = ? AND status IN ('queued', 'slicing', 'uploading')",
    )
    .bind(now())
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// A finished job back into the queue (reslice, upload again), with fresh options.
pub async fn retry(db: &Db, id: i64, options_json: &str, start_print: bool) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'queued', options_json = ?, start_print = ?, output_name = NULL,
                               est_seconds = NULL, filament_mm = NULL, filament_g = NULL, layers = NULL,
                               gcode_size = NULL, error = NULL, log_tail = NULL, started_at = NULL,
                               finished_at = NULL
         WHERE id = ? AND status IN ('done', 'failed', 'cancelled')",
    )
    .bind(options_json)
    .bind(start_print)
    .bind(id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// At startup: jobs that were mid-slice or mid-upload can't be resumed. Queued ones stay queued.
pub async fn interrupt_running(db: &Db) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE slice_jobs SET status = 'failed', error = ?, finished_at = ?
         WHERE status IN ('slicing', 'uploading')",
    )
    .bind(INTERRUPTED)
    .bind(now())
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}

/// Deletes a finished job's row. Unfinished jobs must be cancelled first.
pub async fn delete_job(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    let res =
        sqlx::query("DELETE FROM slice_jobs WHERE id = ? AND status IN ('done', 'failed', 'cancelled')")
            .bind(id)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Jobs whose files must stay: unfinished ones, and those finished at or after `cutoff`. The
/// retention sweep deletes every other job directory, orphans included.
pub async fn retained_job_ids(db: &Db, cutoff: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM slice_jobs
         WHERE status IN ('queued', 'slicing', 'uploading') OR finished_at IS NULL OR finished_at >= ?
         ORDER BY id",
    )
    .bind(cutoff)
    .fetch_all(db)
    .await
}
