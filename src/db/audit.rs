use super::{Db, now};

pub struct Entry<'a> {
    pub user_id: Option<i64>,
    pub printer_id: Option<i64>,
    pub action: &'a str,
    pub detail: Option<String>,
    pub ip: Option<String>,
}

/// Appends to the audit log. A failed write is logged but never fails the user's request.
pub async fn record(db: &Db, entry: Entry<'_>) {
    let res = sqlx::query(
        "INSERT INTO audit_log (user_id, printer_id, action, detail, ip, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(entry.user_id)
    .bind(entry.printer_id)
    .bind(entry.action)
    .bind(&entry.detail)
    .bind(&entry.ip)
    .bind(now())
    .execute(db)
    .await;
    if let Err(e) = res {
        tracing::error!(action = entry.action, error = %e, "failed to write audit log");
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub id: i64,
    pub created_at: i64,
    pub action: String,
    pub detail: Option<String>,
    pub ip: Option<String>,
    pub username: Option<String>,
    pub printer_id: Option<i64>,
    pub printer: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub action: Option<String>,
    pub user_id: Option<i64>,
    pub printer_id: Option<i64>,
    /// Only rows older than this id (paging).
    pub before: Option<i64>,
}

/// Newest first. Each filter is optional (`NULL` matches everything).
pub async fn list(db: &Db, f: &Filter, limit: i64) -> Result<Vec<Row>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.id, a.created_at, a.action, a.detail, a.ip, u.username, a.printer_id, p.name AS printer
         FROM audit_log a
         LEFT JOIN users u ON u.id = a.user_id
         LEFT JOIN printers p ON p.id = a.printer_id
         WHERE (?1 IS NULL OR a.action = ?1)
           AND (?2 IS NULL OR a.user_id = ?2)
           AND (?3 IS NULL OR a.printer_id = ?3)
           AND (?4 IS NULL OR a.id < ?4)
         ORDER BY a.id DESC
         LIMIT ?5",
    )
    .bind(&f.action)
    .bind(f.user_id)
    .bind(f.printer_id)
    .bind(f.before)
    .bind(limit)
    .fetch_all(db)
    .await
}

pub async fn actions(db: &Db) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT DISTINCT action FROM audit_log ORDER BY action")
        .fetch_all(db)
        .await
}
