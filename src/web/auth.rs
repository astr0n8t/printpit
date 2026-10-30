//! Login, logout and first-run setup pages.

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, extract::rejection::FormRejection};
use axum_extra::extract::CookieJar;
use serde::Deserialize;

use crate::auth::session::{self, SessionCheck};
use crate::auth::{ClientIp, CurrentUser, csrf, password, safe_next, validate_username};
use crate::db::{audit, sessions, users};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::{HxRequest, Layout};

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    layout: Layout,
    csrf: String,
    next: Option<String>,
    username: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    layout: Layout,
    csrf: String,
    username: String,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(rename = "_csrf", default)]
    csrf: String,
    next: Option<String>,
}

#[derive(Deserialize)]
pub struct SetupForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
    #[serde(rename = "_csrf", default)]
    csrf: String,
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
}

/// Renders the login page, minting a pre-session CSRF cookie if needed.
fn login_response(
    state: &AppState,
    jar: CookieJar,
    status: StatusCode,
    next: Option<&str>,
    username: &str,
    error: Option<String>,
) -> Result<Response, AppError> {
    let (token, cookie) = csrf::pre_session_token(
        &state.config,
        jar.get(csrf::pre_session_cookie_name(&state.config)),
    );
    let jar = match cookie {
        Some(c) => jar.add(c),
        None => jar,
    };
    let html = LoginTemplate {
        layout: Layout::new("login"),
        csrf: token,
        next: next.map(|n| safe_next(Some(n)).to_string()).filter(|n| n != "/"),
        username: username.to_string(),
        error,
    }
    .render()?;
    Ok((status, jar, Html(html)).into_response())
}

pub async fn login_page(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(q): Query<NextQuery>,
) -> Result<Response, AppError> {
    if let SessionCheck::Valid(_) = session::check(&state, &jar).await? {
        return Ok(Redirect::to(safe_next(q.next.as_deref())).into_response());
    }
    login_response(&state, jar, StatusCode::OK, q.next.as_deref(), "", None)
}

pub async fn login(
    State(state): State<AppState>,
    ip: ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    form: Result<Form<LoginForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(form)) = form else {
        return Err(AppError::BadRequest("Malformed login form.".into()));
    };
    let next = form.next.as_deref();
    let pre_cookie = jar.get(csrf::pre_session_cookie_name(&state.config));
    if !csrf::pre_session_valid(pre_cookie, &form.csrf) {
        let msg = "Your login form expired. Please try again.".to_string();
        return login_response(
            &state,
            jar,
            StatusCode::FORBIDDEN,
            next,
            &form.username,
            Some(msg),
        );
    }

    let username = form.username.trim();
    if let Err(retry) = state.login_limiter.check(ip.0, username) {
        let mins = retry.as_secs().div_ceil(60);
        let msg = format!("Too many failed attempts. Try again in {mins} minute(s).");
        return login_response(
            &state,
            jar,
            StatusCode::TOO_MANY_REQUESTS,
            next,
            username,
            Some(msg),
        );
    }

    let user = users::by_username(&state.db, username).await?;
    let hash = user.as_ref().map(|u| u.password_hash.clone());
    let verified = password::verify_async(form.password, hash).await;
    let user = match user {
        Some(u) if verified && !u.disabled => u,
        other => {
            state.login_limiter.record_failure(ip.0, username);
            audit::record(
                &state.db,
                audit::Entry {
                    user_id: other.map(|u| u.id),
                    printer_id: None,
                    action: "login_failed",
                    detail: Some(format!("username={username}")),
                    ip: ip.to_string_opt(),
                },
            )
            .await;
            let msg = "Invalid username or password.".to_string();
            return login_response(&state, jar, StatusCode::UNAUTHORIZED, next, username, Some(msg));
        }
    };
    state.login_limiter.record_success(username);

    // Never reuse a pre-existing session ID (fixation); drop any old one.
    if let Some(old) = jar.get(session::cookie_name(&state.config)) {
        sessions::delete(&state.db, &sessions::hash_token(old.value())).await?;
    }
    let ip_str = ip.to_string_opt();
    let new = sessions::create(
        &state.db,
        user.id,
        state.config.session_absolute,
        user_agent(&headers),
        ip_str.as_deref(),
    )
    .await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: None,
            action: "login",
            detail: None,
            ip: ip_str,
        },
    )
    .await;
    tracing::info!(user = %user.username, "login");

    let jar = jar
        .add(session::session_cookie(&state.config, new.token))
        .add(session::removal(
            &state.config,
            csrf::pre_session_cookie_name(&state.config),
        ));
    Ok((jar, Redirect::to(safe_next(next))).into_response())
}

pub async fn logout(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: CurrentUser,
    jar: CookieJar,
) -> Result<Response, AppError> {
    sessions::delete(&state.db, &user.session_hash).await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: None,
            action: "logout",
            detail: None,
            ip: ip.to_string_opt(),
        },
    )
    .await;
    let jar = jar.add(session::removal(
        &state.config,
        session::cookie_name(&state.config),
    ));
    if hx.0 {
        Ok((jar, [("hx-redirect", "/login")]).into_response())
    } else {
        Ok((jar, Redirect::to("/login")).into_response())
    }
}

fn setup_response(
    state: &AppState,
    jar: CookieJar,
    status: StatusCode,
    username: &str,
    error: Option<String>,
) -> Result<Response, AppError> {
    let (token, cookie) = csrf::pre_session_token(
        &state.config,
        jar.get(csrf::pre_session_cookie_name(&state.config)),
    );
    let jar = match cookie {
        Some(c) => jar.add(c),
        None => jar,
    };
    let html = SetupTemplate {
        layout: Layout::new("setup"),
        csrf: token,
        username: username.to_string(),
        error,
    }
    .render()?;
    Ok((status, jar, Html(html)).into_response())
}

pub async fn setup_page(State(state): State<AppState>, jar: CookieJar) -> Result<Response, AppError> {
    if state.setup.is_complete() {
        return Ok(Redirect::to("/login").into_response());
    }
    setup_response(&state, jar, StatusCode::OK, "admin", None)
}

pub async fn setup(
    State(state): State<AppState>,
    ip: ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    form: Result<Form<SetupForm>, FormRejection>,
) -> Result<Response, AppError> {
    if state.setup.is_complete() {
        return Ok(Redirect::to("/login").into_response());
    }
    let Ok(Form(form)) = form else {
        return Err(AppError::BadRequest("Malformed setup form.".into()));
    };
    let username = form.username.trim();
    let fail = |jar, status, msg: &str| setup_response(&state, jar, status, username, Some(msg.to_string()));

    if !csrf::pre_session_valid(jar.get(csrf::pre_session_cookie_name(&state.config)), &form.csrf) {
        return fail(
            jar,
            StatusCode::FORBIDDEN,
            "Your setup form expired. Please try again.",
        );
    }
    if !state.setup.check_token(&form.token) {
        tracing::warn!(ip = ?ip.0, "setup attempted with a wrong token");
        return fail(
            jar,
            StatusCode::FORBIDDEN,
            "Wrong setup token. Check the server log.",
        );
    }
    if let Err(msg) = validate_username(username) {
        return fail(jar, StatusCode::UNPROCESSABLE_ENTITY, msg);
    }
    if let Err(msg) = password::validate_new(&form.password, &form.confirm) {
        return fail(jar, StatusCode::UNPROCESSABLE_ENTITY, msg);
    }

    let hash = password::hash_async(form.password)
        .await
        .map_err(AppError::internal)?;
    let Some(user_id) = users::create_first_admin(&state.db, username, &hash).await? else {
        state.setup.mark_complete();
        return Ok(Redirect::to("/login").into_response());
    };
    state.setup.mark_complete();
    tracing::info!(user = %username, "setup complete: admin account created");

    let ip_str = ip.to_string_opt();
    let new = sessions::create(
        &state.db,
        user_id,
        state.config.session_absolute,
        user_agent(&headers),
        ip_str.as_deref(),
    )
    .await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user_id),
            printer_id: None,
            action: "setup",
            detail: Some(format!("admin={username}")),
            ip: ip_str,
        },
    )
    .await;
    let jar = jar
        .add(session::session_cookie(&state.config, new.token))
        .add(session::removal(
            &state.config,
            csrf::pre_session_cookie_name(&state.config),
        ));
    Ok((jar, Redirect::to("/")).into_response())
}
