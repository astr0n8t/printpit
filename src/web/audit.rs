//! The audit log viewer (admin only): newest first, filterable by action, user and printer,
//! paged by id.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;
use serde::Deserialize;

use crate::auth::{Admin, RequireRole};
use crate::db::audit::{self, Filter};
use crate::db::{printers, users};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::widgets;

const PAGE: i64 = 50;

struct Entry {
    time: String,
    user: String,
    action: String,
    printer: String,
    detail: String,
    ip: String,
}

#[derive(Template)]
#[template(path = "tabs/audit.html")]
struct AuditTab {
    entries: Vec<Entry>,
    actions: Vec<String>,
    users: Vec<(i64, String)>,
    printers: Vec<(i64, String)>,
    action: String,
    user_id: Option<i64>,
    printer_id: Option<i64>,
    older: Option<String>,
}

impl AuditTab {
    fn action_selected(&self, a: &str) -> bool {
        self.action == a
    }

    fn user_selected(&self, id: &i64) -> bool {
        self.user_id == Some(*id)
    }

    fn printer_selected(&self, id: &i64) -> bool {
        self.printer_id == Some(*id)
    }
}

/// Filter values arrive as strings; empty means "any".
#[derive(Deserialize, Default)]
pub struct AuditQuery {
    #[serde(default)]
    action: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    printer: String,
    before: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    hx: HxRequest,
    admin: RequireRole<Admin>,
    Query(q): Query<AuditQuery>,
) -> Result<Response, AppError> {
    let filter = Filter {
        action: Some(q.action.trim().to_string()).filter(|a| !a.is_empty()),
        user_id: q.user.parse().ok(),
        printer_id: q.printer.parse().ok(),
        before: q.before,
    };
    let rows = audit::list(&state.db, &filter, PAGE).await?;
    let older = (rows.len() as i64 == PAGE).then(|| {
        let last = rows.last().map(|r| r.id).unwrap_or_default();
        let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        format!(
            "/ui/audit?action={}&user={}&printer={}&before={last}",
            enc(&q.action),
            enc(&q.user),
            enc(&q.printer)
        )
    });
    let entries = rows
        .into_iter()
        .map(|r| Entry {
            time: widgets::utc(r.created_at),
            user: r.username.unwrap_or_else(|| "—".into()),
            action: r.action,
            printer: match (r.printer, r.printer_id) {
                (Some(name), _) => name,
                (None, Some(id)) => format!("#{id} (deleted)"),
                (None, None) => String::new(),
            },
            detail: r.detail.unwrap_or_default(),
            ip: r.ip.unwrap_or_default(),
        })
        .collect();
    let html = AuditTab {
        entries,
        actions: audit::actions(&state.db).await?,
        users: users::list(&state.db)
            .await?
            .into_iter()
            .map(|u| (u.id, u.username))
            .collect(),
        printers: printers::list(&state.db)
            .await?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect(),
        action: filter.action.clone().unwrap_or_default(),
        user_id: filter.user_id,
        printer_id: filter.printer_id,
        older,
    }
    .render()?;
    page(hx, &admin, Tab::Audit, html)
}
