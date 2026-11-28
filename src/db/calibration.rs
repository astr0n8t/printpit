//! The calibration walkthrough's progress (migration 0011): per printer and filament profile,
//! each test's job, the value read off the print and whether it's in the profile yet.

use super::{Db, now};
use crate::slicer::calibrate::{Change, Test};

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    test: String,
    job_id: Option<i64>,
    result: Option<String>,
    changes_json: Option<String>,
    saved_at: Option<i64>,
    skipped: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub test: Test,
    pub job_id: Option<i64>,
    pub result: Option<String>,
    pub changes: Vec<Change>,
    pub saved_at: Option<i64>,
    pub skipped: bool,
}

impl Step {
    /// A value was read off the print (or the test was skipped).
    pub fn is_done(&self) -> bool {
        self.result.is_some() || self.skipped
    }

    /// A result whose changes aren't in the profile yet.
    pub fn unsaved(&self) -> bool {
        self.result.is_some() && self.saved_at.is_none() && !self.changes.is_empty()
    }
}

/// The steps recorded so far, in walkthrough order (tests never started are left out).
pub async fn steps(db: &Db, printer_id: i64, filament_id: i64) -> Result<Vec<Step>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT test, job_id, result, changes_json, saved_at, skipped FROM calibration_steps
         WHERE printer_id = ? AND filament_id = ?",
    )
    .bind(printer_id)
    .bind(filament_id)
    .fetch_all(db)
    .await?;
    let mut out: Vec<Step> = rows
        .into_iter()
        .filter_map(|r| {
            Some(Step {
                test: Test::parse(&r.test)?,
                job_id: r.job_id,
                result: r.result,
                changes: r
                    .changes_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str(j).ok())
                    .unwrap_or_default(),
                saved_at: r.saved_at,
                skipped: r.skipped,
            })
        })
        .collect();
    out.sort_by_key(|s| Test::ALL.iter().position(|t| *t == s.test));
    Ok(out)
}

/// A new print for a test: its job, with any earlier result cleared.
pub async fn set_job(
    db: &Db,
    printer_id: i64,
    filament_id: i64,
    test: Test,
    job: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO calibration_steps (printer_id, filament_id, test, job_id, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (printer_id, filament_id, test) DO UPDATE SET
             job_id = excluded.job_id, result = NULL, changes_json = NULL, saved_at = NULL,
             skipped = 0, updated_at = excluded.updated_at",
    )
    .bind(printer_id)
    .bind(filament_id)
    .bind(test.slug())
    .bind(job)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

/// The value read off a test's print and what it changes. `saved` when the changes were written
/// to the profile at the same time.
pub async fn set_result(
    db: &Db,
    printer_id: i64,
    filament_id: i64,
    test: Test,
    result: &str,
    changes: &[Change],
    saved: bool,
) -> Result<(), sqlx::Error> {
    let ts = now();
    sqlx::query(
        "INSERT INTO calibration_steps (printer_id, filament_id, test, result, changes_json, saved_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (printer_id, filament_id, test) DO UPDATE SET
             result = excluded.result, changes_json = excluded.changes_json, saved_at = excluded.saved_at,
             skipped = 0, updated_at = excluded.updated_at",
    )
    .bind(printer_id)
    .bind(filament_id)
    .bind(test.slug())
    .bind(result)
    .bind(serde_json::to_string(changes).unwrap_or_else(|_| "[]".into()))
    .bind(saved.then_some(ts))
    .bind(ts)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks results as written to the profile.
pub async fn mark_saved(
    db: &Db,
    printer_id: i64,
    filament_id: i64,
    tests: &[Test],
) -> Result<(), sqlx::Error> {
    let ts = now();
    for t in tests {
        sqlx::query(
            "UPDATE calibration_steps SET saved_at = ?, updated_at = ?
             WHERE printer_id = ? AND filament_id = ? AND test = ? AND result IS NOT NULL",
        )
        .bind(ts)
        .bind(ts)
        .bind(printer_id)
        .bind(filament_id)
        .bind(t.slug())
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Skips a test (or takes the skip back).
pub async fn set_skipped(
    db: &Db,
    printer_id: i64,
    filament_id: i64,
    test: Test,
    skipped: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO calibration_steps (printer_id, filament_id, test, skipped, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (printer_id, filament_id, test) DO UPDATE SET
             skipped = excluded.skipped, updated_at = excluded.updated_at",
    )
    .bind(printer_id)
    .bind(filament_id)
    .bind(test.slug())
    .bind(skipped)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

/// Starts the walkthrough over for this printer and filament (jobs are kept).
pub async fn reset(db: &Db, printer_id: i64, filament_id: i64) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM calibration_steps WHERE printer_id = ? AND filament_id = ?")
            .bind(printer_id)
            .bind(filament_id)
            .execute(db)
            .await?
            .rows_affected(),
    )
}
