//! Server-side sessions: cookie handling, the `require_auth` middleware and the
//! `CurrentUser` / `RequireRole` extractors.

use std::marker::PhantomData;
use std::ops::Deref;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};

use super::Role;
use crate::config::Config;
use crate::db::{self, sessions};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::HxRequest;

/// Only bump `last_seen_at` this often, to keep writes down.
const TOUCH_INTERVAL_SECS: i64 = 60;

/// `__Host-` binds the cookie to this exact origin, but browsers only accept it with `Secure`.
pub fn cookie_name(config: &Config) -> &'static str {
    if config.secure_cookies {
        "__Host-printpit_session"
    } else {
        "printpit_session"
    }
}

/// Builds a cookie with printpit's standard flags. `Lax` (not `Strict`) so that following a
/// link to printpit from elsewhere keeps you logged in; cross-site POSTs still carry no cookie,
/// and every state change also requires a CSRF token.
pub fn cookie(config: &Config, name: &'static str, value: String, max_age_secs: i64) -> Cookie<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .secure(config.secure_cookies)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(::cookie::time::Duration::seconds(max_age_secs))
        .build()
}

pub fn session_cookie(config: &Config, token: String) -> Cookie<'static> {
    cookie(
        config,
        cookie_name(config),
        token,
        config.session_absolute.as_secs() as i64,
    )
}

pub fn removal(config: &Config, name: &'static str) -> Cookie<'static> {
    let mut c = cookie(config, name, String::new(), 0);
    c.make_removal();
    c
}

/// Colour scheme preference. `Auto` follows the browser's `prefers-color-scheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
}

impl Theme {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("dark") => Theme::Dark,
            Some("light") => Theme::Light,
            _ => Theme::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }
}

/// The authenticated user for this request. Inserted by [`require_auth`].
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub csrf_token: String,
    pub session_hash: Vec<u8>,
    pub theme: Theme,
}

impl CurrentUser {
    pub fn has(&self, role: Role) -> bool {
        self.role >= role
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentUser>()
            .cloned()
            .ok_or_else(|| AppError::internal("CurrentUser used on a route without require_auth"))
    }
}

pub trait MinRole {
    const ROLE: Role;
}

pub struct Admin;
pub struct Operator;

impl MinRole for Admin {
    const ROLE: Role = Role::Admin;
}

impl MinRole for Operator {
    const ROLE: Role = Role::Operator;
}

/// Extractor that rejects with 403 unless the user has at least role `R`.
pub struct RequireRole<R: MinRole>(pub CurrentUser, PhantomData<R>);

impl<R: MinRole> Deref for RequireRole<R> {
    type Target = CurrentUser;

    fn deref(&self) -> &CurrentUser {
        &self.0
    }
}

impl<S: Send + Sync, R: MinRole> FromRequestParts<S> for RequireRole<R> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = CurrentUser::from_request_parts(parts, state).await?;
        if user.has(R::ROLE) {
            Ok(Self(user, PhantomData))
        } else {
            Err(AppError::Forbidden)
        }
    }
}

/// Outcome of checking the session cookie.
pub enum SessionCheck {
    Valid(Box<CurrentUser>),
    /// No cookie at all.
    Missing,
    /// A cookie that no longer maps to a live session; it should be cleared.
    Invalid,
}

/// Resolves the session cookie, enforcing idle/absolute expiry and disabled accounts.
/// Expired sessions are deleted on sight.
pub async fn check(state: &AppState, jar: &CookieJar) -> Result<SessionCheck, sqlx::Error> {
    let config = &state.config;
    let Some(token) = jar.get(cookie_name(config)).map(|c| c.value().to_owned()) else {
        return Ok(SessionCheck::Missing);
    };
    let hash = sessions::hash_token(&token);
    let Some(s) = sessions::lookup(&state.db, &hash).await? else {
        return Ok(SessionCheck::Invalid);
    };

    let now = db::now();
    let idle_expired = s.last_seen_at + config.session_idle.as_secs() as i64 <= now;
    if s.disabled || s.expires_at <= now || idle_expired {
        sessions::delete(&state.db, &hash).await?;
        return Ok(SessionCheck::Invalid);
    }
    if now - s.last_seen_at >= TOUCH_INTERVAL_SECS {
        sessions::touch(&state.db, &hash).await?;
    }
    Ok(SessionCheck::Valid(Box::new(CurrentUser {
        id: s.user_id,
        username: s.username,
        role: s.role,
        csrf_token: s.csrf_token,
        session_hash: hash,
        theme: Theme::parse(s.theme.as_deref()),
    })))
}

/// Whether a session is still live. Long-lived responses (SSE) call this periodically so a
/// logout, password change or account disable also ends open streams.
pub async fn still_valid(state: &AppState, session_hash: &[u8]) -> bool {
    match sessions::lookup(&state.db, session_hash).await {
        Ok(Some(s)) => {
            let now = db::now();
            !s.disabled
                && s.expires_at > now
                && s.last_seen_at + state.config.session_idle.as_secs() as i64 > now
        }
        _ => false,
    }
}

/// Resolves the session cookie to a user, or sends the client to the login page.
pub async fn require_auth(
    State(state): State<AppState>,
    hx: HxRequest,
    jar: CookieJar,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    match check(&state, &jar).await? {
        SessionCheck::Valid(user) => {
            req.extensions_mut().insert(*user);
            Ok(next.run(req).await)
        }
        SessionCheck::Missing => Ok(unauthenticated(hx, &req, None)),
        SessionCheck::Invalid => {
            let clear = removal(&state.config, cookie_name(&state.config));
            Ok(unauthenticated(hx, &req, Some(clear)))
        }
    }
}

/// htmx can't follow a redirect into a full page, so it gets 401 + `HX-Redirect`. Browsers get
/// a 303 to `/login?next=<where they were going>`.
fn unauthenticated(hx: HxRequest, req: &Request, clear: Option<Cookie<'static>>) -> Response {
    let wants_sse = req
        .headers()
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"));
    let mut res = if wants_sse {
        // EventSource treats a non-200 answer as final and stops reconnecting.
        StatusCode::UNAUTHORIZED.into_response()
    } else if hx.0 {
        let mut r = StatusCode::UNAUTHORIZED.into_response();
        r.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/login"));
        r
    } else {
        let target = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let location = if target == "/" {
            "/login".to_string()
        } else {
            format!(
                "/login?{}",
                serde_urlencoded::to_string([("next", target)]).unwrap_or_default()
            )
        };
        Redirect::to(&location).into_response()
    };
    if let Some(c) = clear
        && let Ok(v) = HeaderValue::from_str(&c.to_string())
    {
        res.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    res
}
