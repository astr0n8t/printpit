//! Calibration runs (migrations 0011, 0012): a calibration started on a printer with a process, a
//! filament and a list of tests, and each test's job, the value read off its print and whether
//! that's in the profiles yet.

use sqlx::Row as _;

use super::{Db, now};
use crate::slicer::calibrate::{Change, Test};

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: i64,
    pub printer_id: i64,
    /// `None` when the run's process was deleted (the printer's default is used then).
    pub process_id: Option<i64>,
    pub filament_id: i64,
    pub user_id: Option<i64>,
    /// The chosen tests, in walkthrough order.
    pub tests: Vec<Test>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

fn run(r: &sqlx::sqlite::SqliteRow) -> Result<Run, sqlx::Error> {
    let raw: String = r.try_get("tests")?;
    let mut tests: Vec<Test> = raw.split(',').filter_map(|s| Test::parse(s.trim())).collect();
    tests.sort_by_key(|t| Test::ALL.iter().position(|a| a == t));
    tests.dedup();
    Ok(Run {
        id: r.try_get("id")?,
        printer_id: r.try_get("printer_id")?,
        process_id: r.try_get("process_id")?,
        filament_id: r.try_get("filament_id")?,
        user_id: r.try_get("user_id")?,
        tests,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        finished_at: r.try_get("finished_at")?,
    })
}

pub struct NewRun<'a> {
    pub printer_id: i64,
    pub process_id: i64,
    pub filament_id: i64,
    pub user_id: i64,
    pub tests: &'a [Test],
}

pub async fn create_run(db: &Db, r: &NewRun<'_>) -> Result<i64, sqlx::Error> {
    let ts = now();
    let tests: Vec<&str> = r.tests.iter().map(|t| t.slug()).collect();
    sqlx::query_scalar(
        "INSERT INTO calibration_runs (printer_id, process_id, filament_id, user_id, tests, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(r.printer_id)
    .bind(r.process_id)
    .bind(r.filament_id)
    .bind(r.user_id)
    .bind(tests.join(","))
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

pub async fn get_run(db: &Db, id: i64) -> Result<Option<Run>, sqlx::Error> {
    sqlx::query("SELECT * FROM calibration_runs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .map(|r| run(&r))
        .transpose()
}

/// Runs still going (`finished` false) or finished, most recently touched first.
pub async fn runs(db: &Db, finished: bool, limit: i64) -> Result<Vec<Run>, sqlx::Error> {
    sqlx::query(
        "SELECT * FROM calibration_runs WHERE (finished_at IS NOT NULL) = ?
         ORDER BY updated_at DESC, id DESC LIMIT ?",
    )
    .bind(finished)
    .bind(limit)
    .fetch_all(db)
    .await?
    .iter()
    .map(run)
    .collect()
}

pub async fn delete_run(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM calibration_runs WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected()
        == 1)
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

    /// A result whose changes aren't in the profiles yet.
    pub fn unsaved(&self) -> bool {
        self.result.is_some() && self.saved_at.is_none() && !self.changes.is_empty()
    }
}

/// A run's steps that have been started, in walkthrough order.
pub async fn steps(db: &Db, run_id: i64) -> Result<Vec<Step>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT test, job_id, result, changes_json, saved_at, skipped FROM calibration_run_steps
         WHERE run_id = ?",
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let test: String = r.try_get("test")?;
        let Some(test) = Test::parse(&test) else {
            continue;
        };
        let changes: Option<String> = r.try_get("changes_json")?;
        out.push(Step {
            test,
            job_id: r.try_get("job_id")?,
            result: r.try_get("result")?,
            changes: changes
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default(),
            saved_at: r.try_get("saved_at")?,
            skipped: r.try_get("skipped")?,
        });
    }
    out.sort_by_key(|s| Test::ALL.iter().position(|t| *t == s.test));
    Ok(out)
}

/// Marks the run touched, and finished when every chosen test is done (or not, again).
async fn touch(db: &Db, run_id: i64) -> Result<(), sqlx::Error> {
    let Some(r) = get_run(db, run_id).await? else {
        return Ok(());
    };
    let steps = steps(db, run_id).await?;
    let finished = r
        .tests
        .iter()
        .all(|t| steps.iter().any(|s| s.test == *t && s.is_done()));
    let ts = now();
    sqlx::query("UPDATE calibration_runs SET updated_at = ?, finished_at = ? WHERE id = ?")
        .bind(ts)
        .bind(if finished {
            r.finished_at.or(Some(ts))
        } else {
            None
        })
        .bind(run_id)
        .execute(db)
        .await?;
    Ok(())
}

/// A new print for a test: its job, with any earlier result for it cleared.
pub async fn set_job(db: &Db, run_id: i64, test: Test, job: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO calibration_run_steps (run_id, test, job_id, updated_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (run_id, test) DO UPDATE SET
             job_id = excluded.job_id, result = NULL, changes_json = NULL, saved_at = NULL,
             skipped = 0, updated_at = excluded.updated_at",
    )
    .bind(run_id)
    .bind(test.slug())
    .bind(job)
    .bind(now())
    .execute(db)
    .await?;
    touch(db, run_id).await
}

/// The value read off a test's print and what it changes (not saved yet).
pub async fn set_result(
    db: &Db,
    run_id: i64,
    test: Test,
    result: &str,
    changes: &[Change],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO calibration_run_steps (run_id, test, result, changes_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (run_id, test) DO UPDATE SET
             result = excluded.result, changes_json = excluded.changes_json, saved_at = NULL,
             skipped = 0, updated_at = excluded.updated_at",
    )
    .bind(run_id)
    .bind(test.slug())
    .bind(result)
    .bind(serde_json::to_string(changes).unwrap_or_else(|_| "[]".into()))
    .bind(now())
    .execute(db)
    .await?;
    touch(db, run_id).await
}

/// Marks results as written to the profiles.
pub async fn mark_saved(db: &Db, run_id: i64, tests: &[Test]) -> Result<(), sqlx::Error> {
    let ts = now();
    for t in tests {
        sqlx::query(
            "UPDATE calibration_run_steps SET saved_at = ?, updated_at = ?
             WHERE run_id = ? AND test = ? AND result IS NOT NULL",
        )
        .bind(ts)
        .bind(ts)
        .bind(run_id)
        .bind(t.slug())
        .execute(db)
        .await?;
    }
    touch(db, run_id).await
}

/// Skips a test (or takes that back).
pub async fn set_skipped(db: &Db, run_id: i64, test: Test, skipped: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO calibration_run_steps (run_id, test, skipped, updated_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (run_id, test) DO UPDATE SET skipped = excluded.skipped, updated_at = excluded.updated_at",
    )
    .bind(run_id)
    .bind(test.slug())
    .bind(skipped)
    .bind(now())
    .execute(db)
    .await?;
    touch(db, run_id).await
}
