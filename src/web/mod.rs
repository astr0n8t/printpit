//! HTTP layer: router assembly and middleware.

mod account;
pub mod assets;
mod auth;
pub mod layout;
mod printers;
pub mod security;
mod tabs;

use axum::Router;
use axum::middleware;
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use tower_http::compression::CompressionLayer;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::auth::{csrf, session, setup};
use crate::error;
use crate::state::AppState;

/// Default request body cap. Upload routes raise it explicitly.
const BODY_LIMIT: usize = 64 * 1024;

pub fn router(state: AppState) -> Router {
    // Everything here needs a session; non-GET requests also need the CSRF token.
    let protected = Router::new()
        .route("/", get(|| async { Redirect::to("/ui/dashboard") }))
        .route("/ui/dashboard", get(tabs::dashboard))
        .route("/ui/files", get(tabs::files))
        .route("/ui/logs", get(tabs::logs))
        .route("/ui/system", get(tabs::system))
        .route("/ui/printers", get(printers::list))
        .route("/ui/printers/new", get(printers::new_form))
        .route("/ui/printers/{id}", get(printers::edit_form))
        .route("/printers", post(printers::create))
        .route("/printers/{id}", post(printers::update))
        .route("/printers/{id}", delete(printers::delete))
        .route("/printers/{id}/toggle", post(printers::toggle))
        .route("/printers/{id}/move/{dir}", post(printers::reorder))
        .route("/printers/{id}/test", post(printers::test))
        .route("/ui/account", get(account::show))
        .route("/account/password", post(account::change_password))
        .route("/logout", post(auth::logout))
        .route_layer(middleware::from_fn(csrf::verify))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            session::require_auth,
        ));

    let public = Router::new()
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/setup", get(auth::setup_page).post(auth::setup))
        .route("/static/{hash}/{*path}", get(assets::serve));

    Router::new()
        .merge(public)
        .merge(protected)
        .fallback(error::not_found)
        .layer(middleware::from_fn(error::wrap_full_page))
        .layer(middleware::from_fn_with_state(state.clone(), setup::gate))
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
