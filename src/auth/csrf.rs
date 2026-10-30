//! CSRF protection.
//!
//! * Authenticated requests: a per-session token, sent by htmx as `X-CSRF-Token` (set on
//!   `<body hx-headers>`) or by plain forms as a `_csrf` field. Checked on every non-safe method.
//! * Login and setup (no session yet): a double-submit cookie whose value must match the form.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::Cookie;
use serde::Deserialize;

use super::CurrentUser;
use super::session::cookie;
use crate::config::Config;
use crate::crypto::{constant_time_eq, random_token};
use crate::error::AppError;

pub const HEADER: &str = "x-csrf-token";
pub const FIELD: &str = "_csrf";
const FORM_LIMIT: usize = 64 * 1024;

#[derive(Deserialize)]
struct CsrfField {
    #[serde(rename = "_csrf")]
    csrf: Option<String>,
}

/// Rejects state-changing requests without a valid token. Runs after `require_auth`.
pub async fn verify(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    // Defence in depth: browsers mark cross-site requests; refuse them outright.
    if req
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site")
    {
        return AppError::Csrf.into_response();
    }
    let Some(expected) = req
        .extensions()
        .get::<CurrentUser>()
        .map(|u| u.csrf_token.clone())
    else {
        return AppError::internal("csrf::verify ran without a CurrentUser").into_response();
    };

    if let Some(sent) = req.headers().get(HEADER) {
        return if constant_time_eq(sent.as_bytes(), expected.as_bytes()) {
            next.run(req).await
        } else {
            AppError::Csrf.into_response()
        };
    }

    let is_form = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    if !is_form {
        return AppError::Csrf.into_response();
    }
    let (parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, FORM_LIMIT).await else {
        return AppError::BadRequest("Request body too large.".into()).into_response();
    };
    let sent = serde_urlencoded::from_bytes::<CsrfField>(&bytes)
        .ok()
        .and_then(|f| f.csrf)
        .unwrap_or_default();
    if !constant_time_eq(sent.as_bytes(), expected.as_bytes()) {
        return AppError::Csrf.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Cookie name for the pre-session (login/setup) double-submit token.
pub fn pre_session_cookie_name(config: &Config) -> &'static str {
    if config.secure_cookies {
        "__Host-printpit_csrf"
    } else {
        "printpit_csrf"
    }
}

/// Reuses the existing pre-session token if there is one, else mints a new cookie.
pub fn pre_session_token(
    config: &Config,
    existing: Option<&Cookie<'_>>,
) -> (String, Option<Cookie<'static>>) {
    match existing {
        Some(c) if !c.value().is_empty() => (c.value().to_string(), None),
        _ => {
            let token = random_token();
            let c = cookie(config, pre_session_cookie_name(config), token.clone(), 3600);
            (token, Some(c))
        }
    }
}

pub fn pre_session_valid(cookie: Option<&Cookie<'_>>, submitted: &str) -> bool {
    cookie.is_some_and(|c| {
        !c.value().is_empty() && constant_time_eq(c.value().as_bytes(), submitted.as_bytes())
    })
}
