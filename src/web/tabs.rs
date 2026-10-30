//! Tab handlers. Each tab is a lazily loaded partial; until a feature lands it shows a
//! placeholder panel. None of these touch a printer yet.

use askama::Template;
use axum::response::Response;

use axum::extract::State;

use crate::auth::{CurrentUser, Role};
use crate::db::printers;
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};

#[derive(Template)]
#[template(path = "tabs/placeholder.html")]
struct Placeholder {
    title: &'static str,
    milestone: &'static str,
    lines: &'static [&'static str],
}

fn placeholder(hx: HxRequest, user: &CurrentUser, tab: Tab, p: Placeholder) -> Result<Response, AppError> {
    page(hx, user, tab, p.render()?)
}

#[derive(Template)]
#[template(path = "tabs/dashboard.html")]
struct Dashboard {
    printers: Vec<String>,
    is_admin: bool,
}

/// Lists configured printers from the database only; live status (and the first Moonraker
/// traffic) arrives with M3.
pub async fn dashboard(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
) -> Result<Response, AppError> {
    let printers = printers::list(&state.db)
        .await?
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| p.name)
        .collect();
    let html = Dashboard {
        printers,
        is_admin: user.has(Role::Admin),
    }
    .render()?;
    page(hx, &user, Tab::Dashboard, html)
}

pub async fn files(hx: HxRequest, user: CurrentUser) -> Result<Response, AppError> {
    placeholder(
        hx,
        &user,
        Tab::Files,
        Placeholder {
            title: "files",
            milestone: "M4",
            lines: &["Browse, upload, move and delete G-code files on a printer."],
        },
    )
}

pub async fn logs(hx: HxRequest, user: CurrentUser) -> Result<Response, AppError> {
    placeholder(
        hx,
        &user,
        Tab::Logs,
        Placeholder {
            title: "logs",
            milestone: "M5",
            lines: &["Tail and download klippy.log, moonraker.log and friends."],
        },
    )
}

pub async fn system(hx: HxRequest, user: CurrentUser) -> Result<Response, AppError> {
    placeholder(
        hx,
        &user,
        Tab::System,
        Placeholder {
            title: "system",
            milestone: "M6",
            lines: &["CPU, memory, disk, network and services on the printer host."],
        },
    )
}
