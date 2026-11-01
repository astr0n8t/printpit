//! Temperature presets (admin only), managed in a panel on the Printers page.

use axum::Form;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use serde::Deserialize;

use crate::auth::{Admin, ClientIp, RequireRole};
use crate::db::{audit, presets};
use crate::error::AppError;
use crate::state::AppState;
use crate::web::layout::HxRequest;
use crate::web::printers::{after_change, preset_error};
use crate::web::temps::{MAX_TARGET, fmt_temp};

const MAX_NAME: usize = 32;

#[derive(Deserialize)]
pub struct PresetForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    extruder: String,
    #[serde(default)]
    bed: String,
}

struct Valid {
    name: String,
    extruder: Option<f64>,
    bed: Option<f64>,
}

/// A blank temperature means "leave that heater alone".
fn temp(field: &str, value: &str) -> Result<Option<f64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<f64>() {
        Ok(t) if t.is_finite() && (0.0..=MAX_TARGET).contains(&t) => Ok(Some(t)),
        _ => Err(format!(
            "The {field} temperature must be 0–{MAX_TARGET:.0} °C, or blank."
        )),
    }
}

async fn validate(state: &AppState, f: &PresetForm, except: Option<i64>) -> Result<Valid, String> {
    let name = f.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("Preset names are 1–{MAX_NAME} characters."));
    }
    let extruder = temp("hotend", &f.extruder)?;
    let bed = temp("bed", &f.bed)?;
    if extruder.is_none() && bed.is_none() {
        return Err("Give the preset a hotend or bed temperature.".into());
    }
    if presets::name_taken(&state.db, name, except)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("Another preset already has that name.".into());
    }
    Ok(Valid {
        name: name.to_string(),
        extruder,
        bed,
    })
}

fn detail(v: &Valid) -> String {
    let t = |x: Option<f64>| x.map_or_else(|| "-".to_string(), fmt_temp);
    format!("name={} extruder={} bed={}", v.name, t(v.extruder), t(v.bed))
}

pub async fn create(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    form: Result<Form<PresetForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let v = match validate(&state, &f, None).await {
        Ok(v) => v,
        Err(e) => return preset_error(&state, hx, &user, e).await,
    };
    let id = presets::create(&state.db, &v.name, v.extruder, v.bed).await?;
    record(&state, ip, &user, "preset_create", id, detail(&v)).await;
    after_change(&state, hx, &user, format!("Added preset {}.", v.name)).await
}

pub async fn update(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
    form: Result<Form<PresetForm>, FormRejection>,
) -> Result<Response, AppError> {
    presets::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let v = match validate(&state, &f, Some(id)).await {
        Ok(v) => v,
        Err(e) => return preset_error(&state, hx, &user, e).await,
    };
    presets::update(&state.db, id, &v.name, v.extruder, v.bed).await?;
    record(&state, ip, &user, "preset_update", id, detail(&v)).await;
    after_change(&state, hx, &user, format!("Saved preset {}.", v.name)).await
}

pub async fn delete(
    State(state): State<AppState>,
    hx: HxRequest,
    ip: ClientIp,
    user: RequireRole<Admin>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = presets::get(&state.db, id).await?.ok_or(AppError::NotFound)?;
    presets::delete(&state.db, id).await?;
    record(&state, ip, &user, "preset_delete", id, format!("name={}", p.name)).await;
    after_change(&state, hx, &user, format!("Deleted preset {}.", p.name)).await
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &RequireRole<Admin>,
    action: &'static str,
    id: i64,
    detail: String,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: None,
            action,
            detail: Some(format!("preset={id} {detail}")),
            ip: ip.to_string_opt(),
        },
    )
    .await;
}
