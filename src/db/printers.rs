use super::{Db, now};

/// Associated-data context for sealed API keys (see `crypto::Cipher`).
pub const API_KEY_CONTEXT: &str = "printer-api-key";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Printer {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key_enc: Option<Vec<u8>>,
    pub webcam_override: Option<String>,
    pub enabled: bool,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct NewPrinter<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub api_key_enc: Option<Vec<u8>>,
    pub webcam_override: Option<&'a str>,
    pub enabled: bool,
}

/// What to do with the stored API key on update.
pub enum KeyChange {
    Keep,
    Clear,
    Set(Vec<u8>),
}

pub async fn list(db: &Db) -> Result<Vec<Printer>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM printers ORDER BY sort_order, id")
        .fetch_all(db)
        .await
}

pub async fn get(db: &Db, id: i64) -> Result<Option<Printer>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM printers WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

pub async fn name_taken(db: &Db, name: &str, except: Option<i64>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM printers WHERE name = ? AND id != ?)")
        .bind(name)
        .bind(except.unwrap_or(-1))
        .fetch_one(db)
        .await
}

pub async fn create(db: &Db, p: NewPrinter<'_>) -> Result<i64, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO printers (name, base_url, api_key_enc, webcam_override, enabled, sort_order, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, (SELECT COALESCE(MAX(sort_order), 0) + 1 FROM printers), ?, ?)
         RETURNING id",
    )
    .bind(p.name)
    .bind(p.base_url)
    .bind(p.api_key_enc)
    .bind(p.webcam_override)
    .bind(p.enabled)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

pub async fn update(db: &Db, id: i64, p: NewPrinter<'_>, key: KeyChange) -> Result<bool, sqlx::Error> {
    let res = match key {
        KeyChange::Keep => {
            sqlx::query(
                "UPDATE printers SET name = ?, base_url = ?, webcam_override = ?, enabled = ?, updated_at = ?
             WHERE id = ?",
            )
            .bind(p.name)
            .bind(p.base_url)
            .bind(p.webcam_override)
            .bind(p.enabled)
            .bind(now())
            .bind(id)
            .execute(db)
            .await?
        }
        KeyChange::Clear | KeyChange::Set(_) => {
            let enc = match key {
                KeyChange::Set(enc) => Some(enc),
                _ => None,
            };
            sqlx::query(
                "UPDATE printers SET name = ?, base_url = ?, api_key_enc = ?, webcam_override = ?, enabled = ?,
                 updated_at = ? WHERE id = ?",
            )
            .bind(p.name)
            .bind(p.base_url)
            .bind(enc)
            .bind(p.webcam_override)
            .bind(p.enabled)
            .bind(now())
            .bind(id)
            .execute(db)
            .await?
        }
    };
    Ok(res.rows_affected() == 1)
}

pub async fn set_enabled(db: &Db, id: i64, enabled: bool) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE printers SET enabled = ?, updated_at = ? WHERE id = ?")
        .bind(enabled)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn delete(db: &Db, id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM printers WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    sqlx::query("UPDATE user_prefs SET selected_printer_id = NULL WHERE selected_printer_id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Swaps a printer's position with its neighbour above (`up`) or below.
pub async fn move_by_one(db: &Db, id: i64, up: bool) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let ordered: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, sort_order FROM printers ORDER BY sort_order, id")
            .fetch_all(&mut *tx)
            .await?;
    let Some(pos) = ordered.iter().position(|(pid, _)| *pid == id) else {
        return Ok(());
    };
    let other = if up {
        pos.checked_sub(1)
    } else {
        Some(pos + 1).filter(|&i| i < ordered.len())
    };
    if let Some(other) = other {
        // Renumber densely first so equal sort_orders can't make the swap a no-op.
        for (i, (pid, _)) in ordered.iter().enumerate() {
            let order = if i == pos {
                other
            } else if i == other {
                pos
            } else {
                i
            };
            sqlx::query("UPDATE printers SET sort_order = ? WHERE id = ?")
                .bind(order as i64 + 1)
                .bind(pid)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await
}
