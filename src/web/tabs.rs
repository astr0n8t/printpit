//! Tab handlers. Each tab is a lazily loaded partial; until a feature lands it shows a
//! placeholder panel. None of these touch a printer yet.

use askama::Template;
use axum::response::Response;

use crate::auth::CurrentUser;
use crate::error::AppError;
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
