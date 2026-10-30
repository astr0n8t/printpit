use serde::Deserialize;

use super::{Db, now};

/// Ordered so that `Viewer < Operator < Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: Role,
    pub disabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

pub async fn count(db: &Db) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db)
        .await
}

pub async fn by_username(db: &Db, username: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(db)
        .await
}

pub async fn by_id(db: &Db, id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

/// Creates the first admin, atomically refusing if any user already exists.
/// Returns `None` when someone else won the race.
pub async fn create_first_admin(
    db: &Db,
    username: &str,
    password_hash: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO users (username, password_hash, role, created_at, updated_at)
         SELECT ?, ?, 'admin', ?, ? WHERE NOT EXISTS (SELECT 1 FROM users)
         RETURNING id",
    )
    .bind(username)
    .bind(password_hash)
    .bind(ts)
    .bind(ts)
    .fetch_optional(db)
    .await
}

pub async fn create(db: &Db, username: &str, password_hash: &str, role: Role) -> Result<i64, sqlx::Error> {
    let ts = now();
    sqlx::query_scalar(
        "INSERT INTO users (username, password_hash, role, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(ts)
    .bind(ts)
    .fetch_one(db)
    .await
}

pub async fn set_password(db: &Db, id: i64, password_hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?")
        .bind(password_hash)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_disabled(db: &Db, id: i64, disabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET disabled = ?, updated_at = ? WHERE id = ?")
        .bind(disabled)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// A user row for the admin list, with their live session count.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserSummary {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub disabled: bool,
    pub created_at: i64,
    pub sessions: i64,
}

pub async fn list(db: &Db) -> Result<Vec<UserSummary>, sqlx::Error> {
    sqlx::query_as(
        "SELECT u.id, u.username, u.role, u.disabled, u.created_at,
                (SELECT COUNT(*) FROM sessions s WHERE s.user_id = u.id) AS sessions
         FROM users u ORDER BY u.username COLLATE NOCASE",
    )
    .fetch_all(db)
    .await
}

/// Enabled admins other than `except`, to stop the last admin locking everyone out.
pub async fn other_active_admins(db: &Db, except: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0 AND id != ?")
        .bind(except)
        .fetch_one(db)
        .await
}

pub async fn set_role(db: &Db, id: i64, role: Role) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET role = ?, updated_at = ? WHERE id = ?")
        .bind(role)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete(db: &Db, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
