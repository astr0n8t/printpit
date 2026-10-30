//! Printer management (admin only): add, edit, delete, reorder, enable/disable, test.
//!
//! This is the one place a printer's URL is shown, and only to admins. API keys are never
//! rendered back, not even masked.

use askama::Template;
use axum::Form;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::auth::{Admin, ClientIp, RequireRole};
use crate::db::audit;
use crate::db::printers::{self, API_KEY_CONTEXT, KeyChange, NewPrinter, Printer};
use crate::error::AppError;
use crate::moonraker::MoonrakerError;
use crate::moonraker::net::{base_string, parse_base_url};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};

const MAX_NAME: usize = 64;
const MAX_KEY: usize = 256;

struct Row {
    id: i64,
    name: String,
    base_url: String,
    key_label: &'static str,
    key_class: &'static str,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "tabs/printers.html")]
struct ListTemplate {
    rows: Vec<Row>,
    notice: Option<String>,
    cidrs: String,
}

#[derive(Template)]
#[template(path = "tabs/printer_form.html")]
struct FormTemplate {
    id: Option<i64>,
    action: String,
    csrf: String,
    name: String,
    base_url: String,
    webcam_override: String,
    enabled: bool,
    key_set: bool,
    key_dropped: bool,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/printer_test.html")]
struct TestResult {
    label: String,
    class: &'static str,
    detail: String,
}

#[derive(Deserialize)]
pub struct PrinterForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    clear_key: Option<String>,
    #[serde(default)]
    webcam_override: String,
    #[serde(default)]
    enabled: Option<String>,
}

struct Valid {
    name: String,
    base_url: String,
    webcam_override: Option<String>,
    api_key: Option<String>,
    enabled: bool,
}

fn row(state: &AppState, p: Printer) -> Row {
    let (key_label, key_class) = match &p.api_key_enc {
        None => ("none", "dim"),
        Some(_) if state.decrypt_api_key(&p).is_err() => ("unreadable", "err"),
        Some(_) => ("set", "ok"),
    };
    Row {
        id: p.id,
        name: p.name,
        base_url: p.base_url,
        key_label,
        key_class,
        enabled: p.enabled,
    }
}

async fn list_page(
    state: &AppState,
    hx: HxRequest,
    user: &RequireRole<Admin>,
    notice: Option<String>,
) -> Result<Response, AppError> {
    let rows = printers::list(&state.db)
        .await?
        .into_iter()
        .map(|p| row(state, p))
        .collect();
    let cidrs = state
        .config
        .printer_cidrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let html = ListTemplate { rows, notice, cidrs }.render()?;
    page(hx, user, Tab::Printers, html)
}

/// After a successful change: htmx gets the list (and the URL bar fixed); browsers a redirect.
async fn after_change(
    state: &AppState,
    hx: HxRequest,
    user: &RequireRole<Admin>,
    notice: String,
) -> Result<Response, AppError> {
    if !hx.0 {
        return Ok(Redirect::to("/ui/printers").into_response());
    }
    let mut res = list_page(state, hx, user, Some(notice)).await?;
    res.headers_mut().insert(
        "hx-push-url",
        axum::http::HeaderValue::from_static("/ui/printers"),
    );
    Ok(res)
}

pub async fn list(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Admin>,
) -> Result<Response, AppError> {
    list_page(&state, hx, &user, None).await
}

pub async fn new_form(hx: HxRequest, user: RequireRole<Admin>) -> Result<Response, AppError> {
    let html = FormTemplate {
        id: None,
        action: "/printers".into(),
        csrf: user.csrf_token.clone(),
        name: String::new(),
        base_url: String::new(),
        webcam_override: String::new(),
        enabled: true,
        key_set: false,
        key_dropped: false,
        error: None,
    }
    .render()?;
    page(hx, &user, Tab::Printers, html)
}

pub async fn edit_form(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let html = FormTemplate {
        id: Some(p.id),
        action: format!("/printers/{}", p.id),
        csrf: user.csrf_token.clone(),
        name: p.name,
        base_url: p.base_url,
        webcam_override: p.webcam_override.unwrap_or_default(),
        enabled: p.enabled,
        key_set: p.api_key_enc.is_some(),
        key_dropped: false,
        error: None,
    }
    .render()?;
    page(hx, &user, Tab::Printers, html)
}

async fn validate(state: &AppState, f: &PrinterForm, except: Option<i64>) -> Result<Valid, String> {
    let name = f.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("Name must be 1–{MAX_NAME} characters."));
    }
    if printers::name_taken(&state.db, name, except)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("Another printer already has that name.".into());
    }

    let url = parse_base_url(&f.base_url).map_err(|e| e.to_string())?;
    state.egress.vet(&url).await.map_err(str::to_string)?;

    let webcam = f.webcam_override.trim();
    let webcam_override = if webcam.is_empty() {
        None
    } else if webcam.starts_with('/') && !webcam.starts_with("//") {
        Some(webcam.to_string())
    } else {
        let parsed = url::Url::parse(webcam)
            .map_err(|_| "Webcam URL must be http(s)://… or a path like /webcam/?action=stream.")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("Webcam URL must be http(s)://… or a path like /webcam/?action=stream.".into());
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("Don't put credentials in the webcam URL.".into());
        }
        state
            .egress
            .vet(&parsed)
            .await
            .map_err(|_| "The webcam address is outside the allowed printer networks.")?;
        Some(webcam.to_string())
    };

    let key = f.api_key.trim();
    if key.len() > MAX_KEY || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(
            "The API key looks wrong: it can't contain spaces and must be under 256 characters.".into(),
        );
    }

    Ok(Valid {
        name: name.to_string(),
        base_url: base_string(&url),
        webcam_override,
        api_key: (!key.is_empty()).then(|| key.to_string()),
        enabled: f.enabled.is_some(),
    })
}

fn form_error(
    user: &RequireRole<Admin>,
    id: Option<i64>,
    key_set: bool,
    f: &PrinterForm,
    error: String,
) -> Result<Response, AppError> {
    let html = FormTemplate {
        id,
        action: id.map_or_else(|| "/printers".to_string(), |id| format!("/printers/{id}")),
        csrf: user.csrf_token.clone(),
        name: f.name.clone(),
        base_url: f.base_url.clone(),
        webcam_override: f.webcam_override.clone(),
        enabled: f.enabled.is_some(),
        key_set,
        key_dropped: !f.api_key.trim().is_empty(),
        error: Some(error),
    }
    .render()?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response())
}

pub async fn create(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    form: Result<Form<PrinterForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let v = match validate(&state, &f, None).await {
        Ok(v) => v,
        Err(e) => return form_error(&user, None, false, &f, e),
    };
    let id = printers::create(
        &state.db,
        NewPrinter {
            name: &v.name,
            base_url: &v.base_url,
            api_key_enc: v
                .api_key
                .as_deref()
                .map(|k| state.cipher.seal(k, API_KEY_CONTEXT)),
            webcam_override: v.webcam_override.as_deref(),
            enabled: v.enabled,
        },
    )
    .await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "printer_create",
            detail: Some(format!("name={}", v.name)),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    after_change(&state, hx, &user, format!("Added {}.", v.name)).await
}

pub async fn update(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
    form: Result<Form<PrinterForm>, FormRejection>,
) -> Result<Response, AppError> {
    let existing = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let key_set = existing.api_key_enc.is_some();
    let v = match validate(&state, &f, Some(id)).await {
        Ok(v) => v,
        Err(e) => return form_error(&user, Some(id), key_set, &f, e),
    };
    let key = match (&v.api_key, f.clear_key.is_some()) {
        (Some(k), _) => KeyChange::Set(state.cipher.seal(k, API_KEY_CONTEXT)),
        (None, true) => KeyChange::Clear,
        (None, false) => KeyChange::Keep,
    };
    let key_note = match key {
        KeyChange::Keep => "kept",
        KeyChange::Clear => "cleared",
        KeyChange::Set(_) => "replaced",
    };
    printers::update(
        &state.db,
        id,
        NewPrinter {
            name: &v.name,
            base_url: &v.base_url,
            api_key_enc: None,
            webcam_override: v.webcam_override.as_deref(),
            enabled: v.enabled,
        },
        key,
    )
    .await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "printer_update",
            detail: Some(format!("name={} api_key={key_note}", v.name)),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    state.printer_changed(id);
    after_change(&state, hx, &user, format!("Saved {}.", v.name)).await
}

pub async fn delete(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    printers::delete(&state.db, id).await?;
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: "printer_delete",
            detail: Some(format!("name={}", p.name)),
            ip: ip.to_string_opt(),
        },
    )
    .await;
    state.printer_changed(id);
    after_change(&state, hx, &user, format!("Deleted {}.", p.name)).await
}

pub async fn toggle(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    printers::set_enabled(&state.db, id, !p.enabled).await?;
    let verb = if p.enabled { "Disabled" } else { "Enabled" };
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(id),
            action: if p.enabled {
                "printer_disable"
            } else {
                "printer_enable"
            },
            detail: None,
            ip: ip.to_string_opt(),
        },
    )
    .await;
    state.printer_changed(id);
    after_change(&state, hx, &user, format!("{verb} {}.", p.name)).await
}

pub async fn reorder(
    State(state): State<AppState>,
    hx: HxRequest,
    user: RequireRole<Admin>,
    Path((id, dir)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let up = match dir.as_str() {
        "up" => true,
        "down" => false,
        _ => return Err(AppError::NotFound),
    };
    printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    printers::move_by_one(&state.db, id, up).await?;
    after_change(&state, hx, &user, "Order saved.".into()).await
}

/// Probes `server.info` (and `printer.info` when Klipper is up) and renders a one-line result.
pub async fn test(
    State(state): State<AppState>,
    _user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = printers::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let result = match probe(&state, &p).await {
        Ok(r) => r,
        Err(e) => TestResult {
            label: e.label().into(),
            class: e.class(),
            detail: e.detail(),
        },
    };
    Ok(Html(result.render()?).into_response())
}

async fn probe(state: &AppState, p: &Printer) -> Result<TestResult, MoonrakerError> {
    let client = state.printer_client(p)?;
    let server = client.server_info().await?;
    let moonraker = format!("Moonraker {}", server.moonraker_version);
    if !server.klippy_connected || server.klippy_state != "ready" {
        let state_name = if server.klippy_state.is_empty() {
            "disconnected".to_string()
        } else {
            server.klippy_state.clone()
        };
        let mut detail = format!("{moonraker} reachable; Klipper is {state_name}");
        if server.klippy_connected
            && let Ok(info) = client.printer_info().await
            && !info.state_message.is_empty()
        {
            detail = format!("{detail}: {}", info.state_message.trim());
        }
        return Ok(TestResult {
            label: state_name.to_uppercase(),
            class: "badge-warn",
            detail,
        });
    }
    let info = client.printer_info().await?;
    let mut detail = format!("Klipper {} · {moonraker}", info.software_version);
    if !info.hostname.is_empty() {
        detail = format!("{detail} · host {}", info.hostname);
    }
    Ok(TestResult {
        label: "READY".into(),
        class: "badge-ok",
        detail,
    })
}
