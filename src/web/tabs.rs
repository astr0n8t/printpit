//! Tab handlers. Each tab is a lazily loaded partial; until a feature lands it shows a
//! placeholder panel. None of these touch a printer yet.

use askama::Template;
use axum::response::Response;

use crate::error::AppError;
use crate::web::layout::{HxRequest, Tab, page};

#[derive(Template)]
#[template(path = "tabs/placeholder.html")]
struct Placeholder {
    title: &'static str,
    milestone: &'static str,
    lines: &'static [&'static str],
}

fn placeholder(hx: HxRequest, tab: Tab, p: Placeholder) -> Result<Response, AppError> {
    page(hx, tab, p.render()?)
}

pub async fn dashboard(hx: HxRequest) -> Result<Response, AppError> {
    placeholder(
        hx,
        Tab::Dashboard,
        Placeholder {
            title: "dashboard",
            milestone: "M3",
            lines: &[
                "No printers configured yet.",
                "Add one from the [p] Printers tab once printer management lands (M2).",
            ],
        },
    )
}

pub async fn files(hx: HxRequest) -> Result<Response, AppError> {
    placeholder(
        hx,
        Tab::Files,
        Placeholder {
            title: "files",
            milestone: "M4",
            lines: &["Browse, upload, move and delete G-code files on a printer."],
        },
    )
}

pub async fn logs(hx: HxRequest) -> Result<Response, AppError> {
    placeholder(
        hx,
        Tab::Logs,
        Placeholder {
            title: "logs",
            milestone: "M5",
            lines: &["Tail and download klippy.log, moonraker.log and friends."],
        },
    )
}

pub async fn system(hx: HxRequest) -> Result<Response, AppError> {
    placeholder(
        hx,
        Tab::System,
        Placeholder {
            title: "system",
            milestone: "M6",
            lines: &["CPU, memory, disk, network and services on the printer host."],
        },
    )
}

pub async fn printers(hx: HxRequest) -> Result<Response, AppError> {
    placeholder(
        hx,
        Tab::Printers,
        Placeholder {
            title: "printers",
            milestone: "M2",
            lines: &["Add printers by Moonraker URL and API key."],
        },
    )
}
