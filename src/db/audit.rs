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
