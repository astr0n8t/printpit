//! First-run setup. While no users exist, every page redirects to `/setup`, which is guarded
//! by a one-time token printed to the server log, so whoever reaches the port first can't
//! claim the instance.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::crypto::{constant_time_eq, random_bytes};
use crate::db::{Db, users};
use crate::state::AppState;
use crate::web::layout::HxRequest;

#[derive(Default)]
pub struct SetupState {
    complete: AtomicBool,
    token: Mutex<Option<String>>,
}

impl SetupState {
    /// Checks the database; if there are no users, mints a token and logs it.
    pub async fn init(db: &Db) -> Result<Self, sqlx::Error> {
        let state = Self::default();
        if users::count(db).await? > 0 {
            state.complete.store(true, Ordering::Release);
        } else {
            let token = generate_token();
            tracing::warn!(
                "No users exist yet. Open /setup in your browser and enter this setup token: {token}"
            );
            *state.lock() = Some(token);
        }
        Ok(state)
    }

    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    pub fn mark_complete(&self) {
        self.complete.store(true, Ordering::Release);
        *self.lock() = None;
    }

    /// The current token, for tests and diagnostics.
    pub fn token(&self) -> Option<String> {
        self.lock().clone()
    }

    pub fn check_token(&self, submitted: &str) -> bool {
        let normalise = |s: &str| {
            s.chars()
                .filter(|c| c.is_ascii_hexdigit())
                .collect::<String>()
                .to_lowercase()
        };
        match self.lock().as_deref() {
            Some(expected) => {
                constant_time_eq(normalise(submitted).as_bytes(), normalise(expected).as_bytes())
            }
            None => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.token.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 80 random bits as five groups of four hex digits, easy to copy from a log.
fn generate_token() -> String {
    hex::encode(random_bytes::<10>())
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Sends everything except `/setup`, static assets and the health check to `/setup` until the
/// first admin exists.
pub async fn gate(State(state): State<AppState>, hx: HxRequest, req: Request, next: Next) -> Response {
    if state.setup.is_complete() {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path == "/setup" || path == "/healthz" || path.starts_with("/static/") {
        return next.run(req).await;
    }
    if hx.0 {
        let mut res = StatusCode::UNAUTHORIZED.into_response();
        res.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static("/setup"));
        res
    } else {
        Redirect::to("/setup").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_and_check() {
        let t = generate_token();
        assert_eq!(t.len(), 24);
        assert_eq!(t.matches('-').count(), 4);
        let s = SetupState::default();
        *s.lock() = Some(t.clone());
        assert!(s.check_token(&t));
        assert!(s.check_token(&format!("  {}  ", t.to_uppercase().replace('-', ""))));
        assert!(!s.check_token("0000-0000-0000-0000-0000"));
        assert!(!s.check_token(""));
        s.mark_complete();
        assert!(!s.check_token(&t));
        assert!(s.is_complete());
    }
}
