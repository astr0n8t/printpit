//! Page chrome: the shell, the tab strip, and the partial-vs-full-page decision.

use std::convert::Infallible;

use askama::Template;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};

use crate::auth::{CurrentUser, Role};
use crate::error::AppError;
use crate::web::assets;

/// `true` when the request came from htmx and wants a fragment, not a full page.
/// History-restore requests need the full page, so they count as `false`.
#[derive(Debug, Clone, Copy)]
pub struct HxRequest(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for HxRequest {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let is = |name: &str| parts.headers.get(name).is_some_and(|v| v == "true");
        Ok(Self(is("hx-request") && !is("hx-history-restore-request")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Files,
    Logs,
    System,
    Printers,
    Account,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Dashboard,
        Tab::Files,
        Tab::Logs,
        Tab::System,
        Tab::Printers,
        Tab::Account,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Tab::Dashboard => "/ui/dashboard",
            Tab::Files => "/ui/files",
            Tab::Logs => "/ui/logs",
            Tab::System => "/ui/system",
            Tab::Printers => "/ui/printers",
            Tab::Account => "/ui/account",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dash",
            Tab::Files => "Files",
            Tab::Logs => "Logs",
            Tab::System => "System",
            Tab::Printers => "Printers",
            Tab::Account => "Account",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Dashboard => "dashboard",
            Tab::Files => "files",
            Tab::Logs => "logs",
            Tab::System => "system",
            Tab::Printers => "printers",
            Tab::Account => "account",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Tab::Dashboard => "1",
            Tab::Files => "2",
            Tab::Logs => "3",
            Tab::System => "4",
            Tab::Printers => "p",
            Tab::Account => "a",
        }
    }

    /// Lowest role that sees this tab in the strip.
    pub fn min_role(self) -> Role {
        match self {
            Tab::Printers => Role::Admin,
            _ => Role::Viewer,
        }
    }

    /// Pushed to the right-hand end of the strip.
    pub fn is_secondary(self) -> bool {
        self == Tab::Account
    }
}

/// The signed-in user as shown in the chrome.
pub struct UserBadge {
    pub username: String,
    pub role: &'static str,
}

/// Data every full page needs.
pub struct Layout {
    pub title: &'static str,
    pub version: &'static str,
    pub user: Option<UserBadge>,
    /// Session CSRF token; emitted on `<body hx-headers>` and in plain forms.
    pub csrf: Option<String>,
}

impl Layout {
    pub fn new(title: &'static str) -> Self {
        Self {
            title,
            version: env!("CARGO_PKG_VERSION"),
            user: None,
            csrf: None,
        }
    }

    pub fn for_user(title: &'static str, user: &CurrentUser) -> Self {
        Self {
            user: Some(UserBadge {
                username: user.username.clone(),
                role: user.role.as_str(),
            }),
            csrf: Some(user.csrf_token.clone()),
            ..Self::new(title)
        }
    }

    pub fn asset(&self, path: &str) -> String {
        assets::url(path)
    }
}

#[derive(Template)]
#[template(path = "shell.html")]
struct ShellTemplate<'a> {
    layout: Layout,
    tabs: TabsTemplate,
    content: &'a str,
}

#[derive(Template)]
#[template(path = "partials/tabs.html")]
struct TabsTemplate {
    active: Tab,
    oob: bool,
    tabs: Vec<Tab>,
}

impl TabsTemplate {
    fn for_user(active: Tab, oob: bool, user: &CurrentUser) -> Self {
        Self {
            active,
            oob,
            tabs: Tab::ALL.into_iter().filter(|t| user.has(t.min_role())).collect(),
        }
    }

    fn is_active(&self, tab: &Tab) -> bool {
        *tab == self.active
    }
}

/// Renders a tab's content either as an htmx fragment (content plus an out-of-band tab strip
/// update) or wrapped in the full shell, so deep links and reloads work.
pub fn page(hx: HxRequest, user: &CurrentUser, tab: Tab, content: String) -> Result<Response, AppError> {
    let html = if hx.0 {
        content + &TabsTemplate::for_user(tab, true, user).render()?
    } else {
        ShellTemplate {
            layout: Layout::for_user(tab.title(), user),
            tabs: TabsTemplate::for_user(tab, false, user),
            content: &content,
        }
        .render()?
    };
    let mut res = Html(html).into_response();
    let headers = res.headers_mut();
    headers.insert(header::VARY, HeaderValue::from_static("HX-Request"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}
