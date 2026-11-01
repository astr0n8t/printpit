//! Storage for the TOTP second factor (see `auth::totp`).

use super::Db;

/// A user's sealed secret and last used step; `None` when 2FA is off.
pub async fn get(db: &Db, user_id: i64) -> Result<Option<(Vec<u8>, Option<i64>)>, sqlx::Error> {
    let row: Option<(Option<Vec<u8>>, Option<i64>)> =
        sqlx::query_as("SELECT totp_secret_enc, totp_last_step FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(secret, step)| secret.map(|s| (s, step))))
}

pub async fn is_enabled(db: &Db, user_id: i64) -> Result<bool, sqlx::Error> {
    Ok(get(db, user_id).await?.is_some())
}

/// Turns 2FA on with a confirmed secret, the step that confirmed it, and fresh recovery codes.
pub async fn enable(
    db: &Db,
    user_id: i64,
    secret_enc: &[u8],
    step: i64,
    recovery: &[Vec<u8>],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE users SET totp_secret_enc = ?, totp_last_step = ? WHERE id = ?")
        .bind(secret_enc)
        .bind(step)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    replace_codes(&mut tx, user_id, recovery).await?;
    tx.commit().await
}

pub async fn disable(db: &Db, user_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE users SET totp_secret_enc = NULL, totp_last_step = NULL WHERE id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    replace_codes(&mut tx, user_id, &[]).await?;
    tx.commit().await
}

/// Records `step` as used, only if it is later than the last one. `false` means the code was
/// already used (or a concurrent request beat this one to it).
pub async fn claim_step(db: &Db, user_id: i64, step: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE users SET totp_last_step = ?
         WHERE id = ? AND totp_secret_enc IS NOT NULL AND (totp_last_step IS NULL OR totp_last_step < ?)",
    )
    .bind(step)
    .bind(user_id)
    .bind(step)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Uses up a recovery code. `false` if it isn't one of the user's remaining codes.
pub async fn use_recovery(db: &Db, user_id: i64, code_hash: &[u8]) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM totp_recovery WHERE user_id = ? AND code_hash = ?")
        .bind(user_id)
        .bind(code_hash)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn recovery_left(db: &Db, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM totp_recovery WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(db)
        .await
}

pub async fn replace_recovery(db: &Db, user_id: i64, recovery: &[Vec<u8>]) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    replace_codes(&mut tx, user_id, recovery).await?;
    tx.commit().await
}

async fn replace_codes(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: i64,
    recovery: &[Vec<u8>],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM totp_recovery WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    for hash in recovery {
        sqlx::query("INSERT INTO totp_recovery (user_id, code_hash) VALUES (?, ?)")
            .bind(user_id)
            .bind(hash)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
