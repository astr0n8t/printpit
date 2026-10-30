//! User management (admin only): create, change role, disable/enable, reset password, delete.
//!
//! Guardrails: admins can't demote, disable or delete themselves (they'd lock themselves out
//! mid-request), and nobody can remove the last enabled admin. Disabling, resetting a
//! password or deleting revokes that user's sessions immediately; a role change applies on
//! their next request because roles are read fresh with every session lookup.

use askama::Template;
use axum::Form;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use crate::auth::{Admin, ClientIp, RequireRole, Role, password, validate_username};
use crate::db::users::{self, UserSummary};
use crate::db::{audit, sessions};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::widgets;

struct Row {
    id: i64,
    username: String,
    role: &'static str,
    disabled: bool,
    sessions: i64,
    created: String,
    is_self: bool,
}

#[derive(Template)]
#[template(path = "tabs/users.html")]
struct UsersTab {
    rows: Vec<Row>,
    notice: Option<String>,
    error: Option<String>,
    roles: [&'static str; 3],
}

impl UsersTab {
    fn is_role(&self, row: &Row, role: &&str) -> bool {
        row.role == *role
    }
}

async fn render(
    state: &AppState,
    hx: HxRequest,
    admin: &RequireRole<Admin>,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Response, AppError> {
    let rows = users::list(&state.db)
        .await?
        .into_iter()
        .map(|u: UserSummary| Row {
            is_self: u.id == admin.id,
            id: u.id,
            username: u.username,
            role: u.role.as_str(),
            disabled: u.disabled,
            sessions: u.sessions,
            created: widgets::age(u.created_at as f64),
        })
        .collect();
    let failed = error.is_some();
    let html = UsersTab {
        rows,
        notice,
        error,
        roles: ["viewer", "operator", "admin"],
    }
    .render()?;
    let mut res = page(hx, admin, Tab::Users, html)?;
    if failed {
        *res.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    }
    Ok(res)
}

pub async fn list(
    State(state): State<AppState>,
    hx: HxRequest,
    admin: RequireRole<Admin>,
) -> Result<Response, AppError> {
    render(&state, hx, &admin, None, None).await
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "viewer" => Some(Role::Viewer),
        "operator" => Some(Role::Operator),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    admin: &RequireRole<Admin>,
    action: &'static str,
    detail: String,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(admin.id),
            printer_id: None,
            action,
            detail: Some(detail),
            ip: ip.to_string_opt(),
        },
    )
    .await;
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

pub async fn create(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    admin: RequireRole<Admin>,
    form: Result<Form<CreateForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let username = f.username.trim();
    let fail = |msg: &str| render(&state, hx, &admin, None, Some(msg.to_string()));
    if let Err(msg) = validate_username(username) {
        return fail(msg).await;
    }
    let Some(role) = parse_role(&f.role) else {
        return fail("Pick a role.").await;
    };
    if let Err(msg) = password::validate_new(&f.password, &f.confirm) {
        return fail(msg).await;
    }
    if users::by_username(&state.db, username).await?.is_some() {
        return fail("That username is taken.").await;
    }
    let hash = password::hash_async(f.password)
        .await
        .map_err(AppError::internal)?;
    users::create(&state.db, username, &hash, role).await?;
    record(
        &state,
        ip,
        &admin,
        "user_create",
        format!("username={username} role={}", role.as_str()),
    )
    .await;
    render(
        &state,
        hx,
        &admin,
        Some(format!("Created {username} ({}).", role.as_str())),
        None,
    )
    .await
}

/// Loads the target user and refuses actions that would lock admins out.
async fn target(
    state: &AppState,
    admin: &RequireRole<Admin>,
    id: i64,
    removes_admin: bool,
) -> Result<Result<users::User, &'static str>, AppError> {
    let user = users::by_id(&state.db, id).await?.ok_or(AppError::NotFound)?;
    if user.id == admin.id {
        return Ok(Err(
            "You can't do that to your own account. Use the Account tab for your password.",
        ));
    }
    // The acting admin is always another enabled admin, so this only trips on races (two
    // admins demoting each other at once); self-protection above covers the normal case.
    if removes_admin
        && user.role == Role::Admin
        && !user.disabled
        && users::other_active_admins(&state.db, user.id).await? == 0
    {
        return Ok(Err("That's the last enabled admin. Promote someone else first."));
    }
    Ok(Ok(user))
}

#[derive(Deserialize)]
pub struct RoleForm {
    #[serde(default)]
    role: String,
}

pub async fn set_role(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    admin: RequireRole<Admin>,
    Path(id): Path<i64>,
    form: Result<Form<RoleForm>, FormRejection>,
) -> Result<Response, AppError> {
    let role = form
        .ok()
        .and_then(|Form(f)| parse_role(&f.role))
        .ok_or_else(|| AppError::BadRequest("Pick a role.".into()))?;
    let user = match target(&state, &admin, id, role != Role::Admin).await? {
        Ok(u) => u,
        Err(msg) => return render(&state, hx, &admin, None, Some(msg.into())).await,
    };
    users::set_role(&state.db, id, role).await?;
    record(
        &state,
        ip,
        &admin,
        "user_role",
        format!("username={} role={}", user.username, role.as_str()),
    )
    .await;
    render(
        &state,
        hx,
        &admin,
        Some(format!("{} is now {}.", user.username, role.as_str())),
        None,
    )
    .await
}

pub async fn toggle(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    admin: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let current = users::by_id(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let disabling = !current.disabled;
    let user = match target(&state, &admin, id, disabling).await? {
        Ok(u) => u,
        Err(msg) => return render(&state, hx, &admin, None, Some(msg.into())).await,
    };
    users::set_disabled(&state.db, id, disabling).await?;
    if disabling {
        sessions::delete_all_for_user(&state.db, id).await?;
    }
    let (action, verb) = if disabling {
        ("user_disable", "Disabled")
    } else {
        ("user_enable", "Enabled")
    };
    record(&state, ip, &admin, action, format!("username={}", user.username)).await;
    render(
        &state,
        hx,
        &admin,
        Some(format!("{verb} {}.", user.username)),
        None,
    )
    .await
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

pub async fn reset_password(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    admin: RequireRole<Admin>,
    Path(id): Path<i64>,
    form: Result<Form<PasswordForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let user = match target(&state, &admin, id, false).await? {
        Ok(u) => u,
        Err(msg) => return render(&state, hx, &admin, None, Some(msg.into())).await,
    };
    if let Err(msg) = password::validate_new(&f.password, &f.confirm) {
        return render(&state, hx, &admin, None, Some(msg.into())).await;
    }
    let hash = password::hash_async(f.password)
        .await
        .map_err(AppError::internal)?;
    users::set_password(&state.db, id, &hash).await?;
    let revoked = sessions::delete_all_for_user(&state.db, id).await?;
    record(
        &state,
        ip,
        &admin,
        "user_password_reset",
        format!("username={} revoked_sessions={revoked}", user.username),
    )
    .await;
    render(
        &state,
        hx,
        &admin,
        Some(format!("Reset {}'s password and signed them out.", user.username)),
        None,
    )
    .await
}

pub async fn delete(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    admin: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let user = match target(&state, &admin, id, true).await? {
        Ok(u) => u,
        Err(msg) => return render(&state, hx, &admin, None, Some(msg.into())).await,
    };
    users::delete(&state.db, id).await?;
    record(
        &state,
        ip,
        &admin,
        "user_delete",
        format!("username={}", user.username),
    )
    .await;
    render(
        &state,
        hx,
        &admin,
        Some(format!("Deleted {}.", user.username)),
        None,
    )
    .await
}
