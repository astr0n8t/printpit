//! Spoolman tracking: per-printer settings, which spool is in which tool, and the bookings
//! made from Moonraker's job history.

use super::{Db, now};
use crate::spoolman::plan::Part;

/// Most filament slots a printer can have (extruders or multi-material lanes).
pub const MAX_TOOLS: i64 = 16;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Settings {
    pub printer_id: i64,
    pub tracking: bool,
    pub tools: i64,
    pub since: f64,
    pub synced_at: Option<i64>,
    pub sync_error: Option<String>,
}

impl Settings {
    fn off(printer_id: i64) -> Self {
        Self {
            printer_id,
            tracking: false,
            tools: 1,
            since: 0.0,
            synced_at: None,
            sync_error: None,
        }
    }
}

pub async fn settings(db: &Db, printer_id: i64) -> Result<Settings, sqlx::Error> {
    let row: Option<Settings> = sqlx::query_as(
        "SELECT printer_id, tracking, tools, since, synced_at, sync_error FROM spoolman_printers
         WHERE printer_id = ?",
    )
    .bind(printer_id)
    .fetch_optional(db)
    .await?;
    Ok(row.unwrap_or_else(|| Settings::off(printer_id)))
}

/// Saves tracking and the tool count. Turning tracking on starts the history cursor at `now`,
/// so jobs from before (or from while it was off) are never booked. Slots past the tool count
/// are dropped. Returns whether tracking was just turned on.
pub async fn set_settings(db: &Db, printer_id: i64, tracking: bool, tools: i64) -> Result<bool, sqlx::Error> {
    let tools = tools.clamp(1, MAX_TOOLS);
    let mut tx = db.begin().await?;
    let was: Option<bool> = sqlx::query_scalar("SELECT tracking FROM spoolman_printers WHERE printer_id = ?")
        .bind(printer_id)
        .fetch_optional(&mut *tx)
        .await?;
    let turned_on = tracking && was != Some(true);
    sqlx::query(
        "INSERT INTO spoolman_printers (printer_id, tracking, tools, since) VALUES (?, ?, ?, ?)
         ON CONFLICT (printer_id) DO UPDATE SET tracking = excluded.tracking, tools = excluded.tools,
             since = CASE WHEN ? THEN excluded.since ELSE spoolman_printers.since END,
             sync_error = CASE WHEN excluded.tracking THEN spoolman_printers.sync_error ELSE NULL END",
    )
    .bind(printer_id)
    .bind(tracking)
    .bind(tools)
    .bind(now() as f64)
    .bind(turned_on)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM spoolman_slots WHERE printer_id = ? AND tool >= ?")
        .bind(printer_id)
        .bind(tools)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(turned_on)
}

/// Enabled printers with tracking on.
pub async fn tracked(db: &Db) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT s.printer_id FROM spoolman_printers s JOIN printers p ON p.id = s.printer_id
         WHERE s.tracking AND p.enabled ORDER BY s.printer_id",
    )
    .fetch_all(db)
    .await
}

/// The spool in each tool, `tools` long.
pub async fn slots(db: &Db, printer_id: i64, tools: i64) -> Result<Vec<Option<i64>>, sqlx::Error> {
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT tool, spool_id FROM spoolman_slots WHERE printer_id = ? AND tool < ?")
            .bind(printer_id)
            .bind(tools)
            .fetch_all(db)
            .await?;
    let mut out = vec![None; usize::try_from(tools.clamp(1, MAX_TOOLS)).unwrap_or(1)];
    for (tool, spool) in rows {
        if let Some(slot) = usize::try_from(tool).ok().and_then(|t| out.get_mut(t)) {
            *slot = Some(spool);
        }
    }
    Ok(out)
}

/// Replaces the assignments (`slots[i]` = the spool in tool `i`).
pub async fn set_slots(db: &Db, printer_id: i64, slots: &[Option<i64>]) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM spoolman_slots WHERE printer_id = ?")
        .bind(printer_id)
        .execute(&mut *tx)
        .await?;
    for (tool, spool) in slots.iter().enumerate() {
        if let Some(spool) = spool {
            sqlx::query("INSERT INTO spoolman_slots (printer_id, tool, spool_id) VALUES (?, ?, ?)")
                .bind(printer_id)
                .bind(tool as i64)
                .bind(spool)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await
}

/// Moves the history cursor forward (never back).
pub async fn advance_cursor(db: &Db, printer_id: i64, since: f64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE spoolman_printers SET since = MAX(since, ?) WHERE printer_id = ?")
        .bind(since)
        .bind(printer_id)
        .execute(db)
        .await
        .map(drop)
}

pub async fn set_sync_result(db: &Db, printer_id: i64, error: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE spoolman_printers SET synced_at = ?, sync_error = ? WHERE printer_id = ?")
        .bind(now())
        .bind(error)
        .bind(printer_id)
        .execute(db)
        .await
        .map(drop)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Booking {
    pub id: i64,
    pub printer_id: i64,
    pub job_id: String,
    pub filename: String,
    pub job_status: String,
    pub job_end: Option<f64>,
    pub used_mm: f64,
    pub status: String,
    pub parts: Vec<Part>,
    pub note: String,
    pub error: Option<String>,
    pub updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct BookingRow {
    id: i64,
    printer_id: i64,
    job_id: String,
    filename: String,
    job_status: String,
    job_end: Option<f64>,
    used_mm: f64,
    status: String,
    parts: String,
    note: String,
    error: Option<String>,
    updated_at: i64,
}

impl From<BookingRow> for Booking {
    fn from(r: BookingRow) -> Self {
        Self {
            id: r.id,
            printer_id: r.printer_id,
            job_id: r.job_id,
            filename: r.filename,
            job_status: r.job_status,
            job_end: r.job_end,
            used_mm: r.used_mm,
            status: r.status,
            parts: serde_json::from_str(&r.parts).unwrap_or_default(),
            note: r.note,
            error: r.error,
            updated_at: r.updated_at,
        }
    }
}

pub struct NewBooking<'a> {
    pub printer_id: i64,
    pub job_id: &'a str,
    pub job_start: f64,
    pub filename: &'a str,
    pub job_status: &'a str,
    pub job_end: Option<f64>,
    pub used_mm: f64,
    pub status: &'a str,
    pub parts: &'a [Part],
    pub note: &'a str,
}

/// Claims a job. `None` when it was already looked at, so nothing is booked twice.
pub async fn insert_booking(db: &Db, b: NewBooking<'_>) -> Result<Option<i64>, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO spoolman_bookings
             (printer_id, job_id, job_start, filename, job_status, job_end, used_mm, status, parts, note,
              created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (printer_id, job_id, job_start) DO NOTHING
         RETURNING id",
    )
    .bind(b.printer_id)
    .bind(b.job_id)
    .bind(b.job_start)
    .bind(b.filename)
    .bind(b.job_status)
    .bind(b.job_end)
    .bind(b.used_mm)
    .bind(b.status)
    .bind(serde_json::to_string(b.parts).unwrap_or_else(|_| "[]".into()))
    .bind(b.note)
    .bind(ts)
    .bind(ts)
    .fetch_optional(db)
    .await
}

/// Moves a booking on from `from` (so a stale writer can't overwrite a newer outcome).
pub async fn finish_booking(
    db: &Db,
    id: i64,
    from: &str,
    status: &str,
    parts: &[Part],
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE spoolman_bookings SET status = ?, parts = ?, error = ?, updated_at = ?
         WHERE id = ? AND status = ?",
    )
    .bind(status)
    .bind(serde_json::to_string(parts).unwrap_or_else(|_| "[]".into()))
    .bind(error)
    .bind(now())
    .bind(id)
    .bind(from)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Bookings that failed and are retried, oldest first.
pub async fn failed(db: &Db, printer_id: i64) -> Result<Vec<Booking>, sqlx::Error> {
    let rows: Vec<BookingRow> = sqlx::query_as(
        "SELECT id, printer_id, job_id, filename, job_status, job_end, used_mm, status, parts, note, error, updated_at
         FROM spoolman_bookings WHERE printer_id = ? AND status = 'failed' ORDER BY id",
    )
    .bind(printer_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Booking::from).collect())
}

pub async fn recent(db: &Db, printer_id: i64, limit: i64) -> Result<Vec<Booking>, sqlx::Error> {
    let rows: Vec<BookingRow> = sqlx::query_as(
        "SELECT id, printer_id, job_id, filename, job_status, job_end, used_mm, status, parts, note, error, updated_at
         FROM spoolman_bookings WHERE printer_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(printer_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Booking::from).collect())
}

/// At startup: a booking that was mid-send when printpit stopped may or may not have reached
/// Spoolman. It's marked `unknown` and not retried, so filament is never booked twice.
pub async fn interrupt_sending(db: &Db) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE spoolman_bookings SET status = 'unknown', updated_at = ?,
             error = 'printpit stopped while booking this; check the spool in Spoolman.'
         WHERE status = 'sending'",
    )
    .bind(now())
    .execute(db)
    .await?;
    Ok(res.rows_affected())
}
