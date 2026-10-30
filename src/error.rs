//! Application error type. Every error renders as a TUI error panel.
//!
//! Handlers don't know whether they're answering htmx or a full page load, so `AppError` always
//! renders just the panel and tags the response with [`ErrorPanel`]. The `wrap_full_page`
//! middleware then wraps it in the page chrome for non-htmx requests.

use askama::Template;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};

use crate::web::layout::{HxRequest, Layout};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("forbidden")]
    Forbidden,
    #[error("csrf check failed")]
    Csrf,
    /// Details are logged, never shown to the user.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal(err.to_string())
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Forbidden | Self::Csrf => StatusCode::FORBIDDEN,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn public_message(&self) -> String {
        match self {
            Self::NotFound => "Nothing here.".into(),
            Self::BadRequest(msg) => msg.clone(),
            Self::Forbidden => "You don't have permission to do that.".into(),
            Self::Csrf => "Security token missing or invalid. Reload the page and try again.".into(),
            Self::Internal(_) => "Something went wrong. Details are in the server log.".into(),
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal(err)
    }
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        Self::internal(err)
    }
}

#[derive(Template)]
#[template(path = "partials/error.html")]
struct ErrorPanelTemplate {
    code: u16,
    reason: &'static str,
    message: String,
}

#[derive(Template)]
#[template(path = "error_page.html")]
struct ErrorPageTemplate<'a> {
    layout: Layout,
    panel: &'a str,
}

/// Marker extension carrying the rendered panel so the middleware can wrap it.
#[derive(Clone)]
struct ErrorPanel(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if let Self::Internal(detail) = &self {
            tracing::error!(%detail, "request failed");
        }
        let panel = ErrorPanelTemplate {
            code: status.as_u16(),
            reason: status.canonical_reason().unwrap_or("Error"),
            message: self.public_message(),
        }
        .render()
        .unwrap_or_else(|_| format!("<pre>{}</pre>", status));
        let mut res = (status, Html(panel.clone())).into_response();
        res.extensions_mut().insert(ErrorPanel(panel));
        res
    }
}

pub async fn wrap_full_page(hx: HxRequest, req: Request, next: Next) -> Response {
    let res = next.run(req).await;
    if hx.0 {
        return res;
    }
    let Some(ErrorPanel(panel)) = res.extensions().get::<ErrorPanel>().cloned() else {
        return res;
    };
    let page = ErrorPageTemplate {
        layout: Layout::new("error"),
        panel: &panel,
    };
    match page.render() {
        Ok(html) => (res.status(), Html(html)).into_response(),
        Err(_) => res,
    }
}

pub async fn not_found() -> AppError {
    AppError::NotFound
}
