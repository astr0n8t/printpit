//! HTTP layer: router assembly and middleware.

mod account;
pub mod assets;
mod audit;
mod auth;
pub mod card;
mod dashboard;
pub(crate) mod files;
pub mod layout;
mod logs;
mod printers;
pub mod security;
mod select;
mod system;
mod users;
pub mod widgets;

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
    // Multipart framing on top of the file itself.
    let upload_limit = usize::try_from(state.config.max_upload_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(1 << 20);
    // Everything here needs a session; non-GET requests also need the CSRF token.
    let protected = Router::new()
        .route("/", get(|| async { Redirect::to("/ui/dashboard") }))
        .route("/ui/dashboard", get(dashboard::show))
        .route("/ui/dashboard/stream", get(dashboard::stream))
        .route("/account/prefs", post(dashboard::set_prefs))
        .route("/p/{id}/status", get(dashboard::status))
        .route("/p/{id}/start", get(dashboard::start_picker))
        .route("/p/{id}/print/{action}", post(dashboard::print_action))
        .route("/p/{id}/estop", post(dashboard::estop))
        .route("/p/{id}/camera/snapshot", get(dashboard::camera_snapshot))
        .route("/p/{id}/camera/stream", get(dashboard::camera_stream))
        .route("/ui/files", get(files::tab))
        .route("/p/{id}/files", get(files::list).delete(files::delete))
        .route("/p/{id}/files/meta", get(files::meta))
        .route("/p/{id}/files/thumb", get(files::thumb))
        .route("/p/{id}/files/download", get(files::download))
        .route("/p/{id}/files/mkdir", post(files::mkdir))
        .route("/p/{id}/files/move", post(files::move_or_copy))
        .route(
            "/p/{id}/files/upload",
            post(files::upload).layer(axum::extract::DefaultBodyLimit::max(upload_limit)),
        )
        .route("/ui/logs", get(logs::tab))
        .route("/p/{id}/logs", get(logs::list))
        .route("/p/{id}/logs/view", get(logs::view))
        .route("/p/{id}/logs/tail", get(logs::tail))
        .route("/p/{id}/logs/download", get(logs::download))
        .route("/ui/system", get(system::tab))
        .route("/p/{id}/system/info", get(system::info))
        .route("/p/{id}/system/stats", get(system::stats))
        .route("/ui/printers", get(printers::list))
        .route("/ui/printers/new", get(printers::new_form))
        .route("/ui/printers/{id}", get(printers::edit_form))
        .route("/printers", post(printers::create))
        .route("/printers/{id}", post(printers::update))
        .route("/printers/{id}", delete(printers::delete))
        .route("/printers/{id}/toggle", post(printers::toggle))
        .route("/printers/{id}/move/{dir}", post(printers::reorder))
        .route("/printers/{id}/test", post(printers::test))
        .route("/ui/users", get(users::list))
        .route("/users", post(users::create))
        .route("/users/{id}", delete(users::delete))
        .route("/users/{id}/role", post(users::set_role))
        .route("/users/{id}/toggle", post(users::toggle))
        .route("/users/{id}/password", post(users::reset_password))
        .route("/ui/audit", get(audit::list))
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
