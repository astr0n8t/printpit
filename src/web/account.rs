//! The account tab: who you are, and changing your own password.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::extract::rejection::FormRejection;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::{ClientIp, CurrentUser, password};
use crate::db::{audit, sessions, users};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};

#[derive(Template)]
#[template(path = "tabs/account.html")]
struct AccountTemplate<'a> {
    username: &'a str,
    role: &'static str,
    password_form: String,
}

#[derive(Template)]
#[template(path = "partials/password_form.html")]
struct PasswordForm<'a> {
    csrf: &'a str,
    error: Option<&'a str>,
    success: Option<&'a str>,
}

#[derive(Deserialize)]
pub struct ChangePassword {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new: String,
    #[serde(default)]
    confirm: String,
}

pub async fn show(hx: HxRequest, user: CurrentUser) -> Result<Response, AppError> {
    let content = AccountTemplate {
        username: &user.username,
        role: user.role.as_str(),
        password_form: PasswordForm {
            csrf: &user.csrf_token,
            error: None,
            success: None,
        }
        .render()?,
    }
    .render()?;
    page(hx, &user, Tab::Account, content)
}

pub async fn change_password(
    State(state): State<AppState>,
    ip: ClientIp,
    user: CurrentUser,
    form: Result<Form<ChangePassword>, FormRejection>,
) -> Result<Response, AppError> {
    let respond =
        |status: StatusCode, error: Option<&str>, success: Option<&str>| -> Result<Response, AppError> {
            let html = PasswordForm {
                csrf: &user.csrf_token,
                error,
                success,
            }
            .render()?;
            Ok((status, Html(html)).into_response())
        };
    let Ok(Form(form)) = form else {
        return respond(StatusCode::BAD_REQUEST, Some("Malformed form."), None);
    };

    // A stolen session shouldn't allow unlimited guessing of the current password.
    if let Err(retry) = state.login_limiter.check(ip.0, &user.username) {
        let msg = format!(
            "Too many failed attempts. Try again in {} minute(s).",
            retry.as_secs().div_ceil(60)
        );
        return respond(StatusCode::TOO_MANY_REQUESTS, Some(&msg), None);
    }
    let Some(record) = users::by_id(&state.db, user.id).await? else {
        return Err(AppError::internal("session user vanished"));
    };
    if !password::verify_async(form.current, Some(record.password_hash)).await {
        state.login_limiter.record_failure(ip.0, &user.username);
        return respond(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Current password is wrong."),
            None,
        );
    }
    if let Err(msg) = password::validate_new(&form.new, &form.confirm) {
        return respond(StatusCode::UNPROCESSABLE_ENTITY, Some(msg), None);
    }

    let hash = password::hash_async(form.new).await.map_err(AppError::internal)?;
    users::set_password(&state.db, user.id, &hash).await?;
    let revoked = sessions::delete_others(&state.db, user.id, &user.session_hash).await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: None,
            action: "password_change",
            detail: Some(format!("revoked_sessions={revoked}")),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    respond(
        StatusCode::OK,
        None,
        Some("Password changed. Your other sessions were signed out."),
    )
}
