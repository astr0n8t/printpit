use std::time::Duration;

use super::users::Role;
use super::{Db, now};
use crate::crypto;

/// A freshly created session. `token` goes in the cookie and is never stored.
pub struct NewSession {
    pub token: String,
    pub id_hash: Vec<u8>,
    pub csrf_token: String,
}

/// A session row joined with its user.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionUser {
    pub id_hash: Vec<u8>,
    pub csrf_token: String,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub user_id: i64,
    pub username: String,
    pub role: Role,
    pub disabled: bool,
}

pub fn hash_token(token: &str) -> Vec<u8> {
    crypto::sha256(token.as_bytes())
}

pub async fn create(
    db: &Db,
    user_id: i64,
    absolute_ttl: Duration,
    user_agent: Option<&str>,
    ip: Option<&str>,
) -> Result<NewSession, sqlx::Error> {
    let token = crypto::random_token();
    let id_hash = hash_token(&token);
    let csrf_token = crypto::random_token();
    let ts = now();
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, csrf_token, created_at, last_seen_at, expires_at, user_agent, ip)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id_hash)
    .bind(user_id)
    .bind(&csrf_token)
    .bind(ts)
    .bind(ts)
    .bind(ts + absolute_ttl.as_secs() as i64)
    .bind(user_agent.map(|ua| ua.chars().take(256).collect::<String>()))
    .bind(ip)
    .execute(db)
    .await?;
    Ok(NewSession {
        token,
        id_hash,
        csrf_token,
    })
}

pub async fn lookup(db: &Db, id_hash: &[u8]) -> Result<Option<SessionUser>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.id_hash, s.csrf_token, s.last_seen_at, s.expires_at,
                u.id AS user_id, u.username, u.role, u.disabled
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.id_hash = ?",
    )
    .bind(id_hash)
    .fetch_optional(db)
    .await
}

pub async fn touch(db: &Db, id_hash: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id_hash = ?")
        .bind(now())
        .bind(id_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete(db: &Db, id_hash: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE id_hash = ?")
        .bind(id_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Revokes every session of a user except `keep` (the caller's own).
pub async fn delete_others(db: &Db, user_id: i64, keep: &[u8]) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM sessions WHERE user_id = ? AND id_hash != ?")
        .bind(user_id)
        .bind(keep)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

/// Removes sessions past their absolute expiry or idle timeout.
pub async fn delete_expired(db: &Db, idle: Duration) -> Result<u64, sqlx::Error> {
    let ts = now();
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at <= ? OR last_seen_at <= ?")
        .bind(ts)
        .bind(ts - idle.as_secs() as i64)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}
