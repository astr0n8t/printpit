//! HTTP layer: router assembly and middleware.

pub mod assets;
pub mod layout;
pub mod security;
mod tabs;

use axum::Router;
use axum::middleware;
use axum::response::Redirect;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::error;
use crate::state::AppState;

/// Default request body cap. Upload routes raise it explicitly.
const BODY_LIMIT: usize = 64 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/ui/dashboard") }))
        .route("/ui/dashboard", get(tabs::dashboard))
        .route("/ui/files", get(tabs::files))
        .route("/ui/logs", get(tabs::logs))
        .route("/ui/system", get(tabs::system))
        .route("/ui/printers", get(tabs::printers))
        .route("/static/{hash}/{*path}", get(assets::serve))
        .fallback(error::not_found)
        .layer(middleware::from_fn(error::wrap_full_page))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .layer(CompressionLayer::new())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        // Added after the trace layer so Docker's 30 s health probes don't flood the log.
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn(security::headers))
        .with_state(state)
}
