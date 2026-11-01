//! Heater targets and temperature presets (operator+), from a card's `temps` modal.
//!
//! Targets go to Klipper as G-code through `/printer/gcode/script`, so heater names are
//! checked against the printer's own objects and a strict charset before they are written
//! into a command: nothing a user types can add a second command.

use askama::Template;
use axum::Form;
use axum::extract::rejection::FormRejection;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::auth::{ClientIp, Operator, RequireRole, Role};
use crate::db::presets::{self, Preset};
use crate::db::printers::Printer;
use crate::error::AppError;
use crate::moonraker::hub::poll_once;
use crate::moonraker::status::Link;
use crate::moonraker::{MoonrakerClient, MoonrakerError};
use crate::state::AppState;
use crate::web::card::{display_name, sensor_rank};
use crate::web::dashboard::{Action, Flash, run_action};
use crate::web::select::enabled_printer;

pub const MAX_TARGET: f64 = 500.0;
const GCODE_SCRIPT: &str = "/printer/gcode/script";

pub struct Heater {
    pub object: String,
    pub label: String,
    pub temp: String,
    pub target: String,
}

pub struct PresetButton {
    pub id: i64,
    pub name: String,
    pub summary: String,
}

#[derive(Template)]
#[template(path = "partials/temps.html")]
struct TempsModal {
    id: i64,
    name: String,
    heaters: Vec<Heater>,
    presets: Vec<PresetButton>,
    is_admin: bool,
    error: Option<String>,
}

/// Klipper objects whose target printpit may set.
pub fn is_heater(object: &str) -> bool {
    match object.split_once(' ') {
        Some((kind, name)) => matches!(kind, "heater_generic" | "temperature_fan") && valid_name(name),
        None => {
            object == "heater_bed"
                || object
                    .strip_prefix("extruder")
                    .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        }
    }
}

/// A name that can sit in a G-code parameter as is: no spaces, quotes, `;` or newlines.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The G-code that sets `object`'s target, or `None` if it isn't a settable heater.
pub fn set_target_gcode(object: &str, target: f64) -> Option<String> {
    if !is_heater(object) || !(0.0..=MAX_TARGET).contains(&target) {
        return None;
    }
    let t = fmt_temp(target);
    Some(match object.split_once(' ') {
        Some(("temperature_fan", name)) => {
            format!("SET_TEMPERATURE_FAN_TARGET TEMPERATURE_FAN={name} TARGET={t}")
        }
        Some((_, name)) => format!("SET_HEATER_TEMPERATURE HEATER={name} TARGET={t}"),
        None => format!("SET_HEATER_TEMPERATURE HEATER={object} TARGET={t}"),
    })
}

/// `215`, or `215.5` when it isn't a whole number.
pub fn fmt_temp(t: f64) -> String {
    if t.fract() == 0.0 {
        format!("{t:.0}")
    } else {
        format!("{t:.1}")
    }
}

/// A target typed into a form: blank is an error here (presets treat blank as "unchanged").
pub fn parse_target(s: &str) -> Result<f64, String> {
    match s.trim().parse::<f64>() {
        Ok(t) if t.is_finite() && (0.0..=MAX_TARGET).contains(&t) => Ok(t),
        _ => Err(format!("Enter a temperature between 0 and {MAX_TARGET:.0} °C.")),
    }
}

/// `215/60 °C`, `—/60 °C`…
pub fn preset_summary(p: &Preset) -> String {
    let t = |v: Option<f64>| v.map_or_else(|| "—".to_string(), fmt_temp);
    format!("{}/{} °C", t(p.extruder), t(p.bed))
}

/// The heaters in a status map, hotends first.
fn heaters(status: &Map<String, Value>) -> Vec<Heater> {
    let mut names: Vec<&String> = status
        .iter()
        .filter(|(k, v)| is_heater(k) && v.get("target").is_some())
        .map(|(k, _)| k)
        .collect();
    names.sort_by_key(|n| sensor_rank(n));
    names
        .into_iter()
        .map(|object| {
            let num = |f: &str| status[object].get(f).and_then(Value::as_f64);
            Heater {
                object: object.clone(),
                label: display_name(object),
                temp: num("temperature").map(|t| format!("{t:.1}")).unwrap_or_default(),
                target: num("target").map(fmt_temp).unwrap_or_default(),
            }
        })
        .collect()
}

async fn current_status(state: &AppState, p: &Printer) -> Result<Map<String, Value>, MoonrakerError> {
    let client = state.printer_client(p)?;
    heater_status(state, p, &client).await
}

/// A printer error, shown in the card's message line like a failed action.
fn flash_error(e: &MoonrakerError) -> Result<Response, AppError> {
    Ok(Html(Flash::error(e).render()?).into_response())
}

/// Current heater status: the live connection's snapshot if the dashboard has one open,
/// otherwise one HTTP query.
async fn heater_status(
    state: &AppState,
    p: &Printer,
    client: &MoonrakerClient,
) -> Result<Map<String, Value>, MoonrakerError> {
    if let Some(snap) = state.hub.peek(p.id)
        && matches!(snap.link, Link::Live | Link::Polling)
    {
        return Ok(snap.status.clone());
    }
    match poll_once(client).await? {
        (Link::NotReady { message, .. }, _) => Err(MoonrakerError::KlippyNotReady(message)),
        (_, status) => Ok(status),
    }
}

pub async fn modal(
    State(state): State<AppState>,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let (heaters, error) = match current_status(&state, &p).await {
        Ok(status) => (heaters(&status), None),
        Err(e) => (Vec::new(), Some(format!("{}: {}", e.label(), e.detail()))),
    };
    let presets = presets::list(&state.db)
        .await?
        .into_iter()
        .map(|pr| PresetButton {
            summary: preset_summary(&pr),
            id: pr.id,
            name: pr.name,
        })
        .collect();
    let html = TempsModal {
        id,
        name: p.name,
        heaters,
        presets,
        is_admin: user.has(Role::Admin),
        error,
    }
    .render()?;
    Ok(Html(html).into_response())
}

#[derive(Deserialize)]
pub struct TargetForm {
    #[serde(default)]
    heater: String,
    #[serde(default)]
    target: String,
}

pub async fn set_target(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    form: Result<Form<TargetForm>, FormRejection>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let target = parse_target(&f.target).map_err(AppError::BadRequest)?;
    let unknown = || AppError::BadRequest("That printer has no such heater.".into());
    if !is_heater(&f.heater) {
        return Err(unknown());
    }
    let status = match current_status(&state, &p).await {
        Ok(s) => s,
        Err(e) => return flash_error(&e),
    };
    if !status.get(&f.heater).is_some_and(|v| v.get("target").is_some()) {
        return Err(unknown());
    }
    let script = set_target_gcode(&f.heater, target).ok_or_else(unknown)?;
    let label = display_name(&f.heater);
    let t = fmt_temp(target);
    let action = Action {
        path: GCODE_SCRIPT,
        query: vec![("script", script.as_str())],
        audit: "temp_set",
        detail: Some(format!("heater={} target={t}", f.heater)),
        done: if target == 0.0 {
            format!("{label} off.")
        } else {
            format!("{label} → {t} °C.")
        },
    };
    run_action(&state, ip, &user, &p, action).await
}

pub async fn apply_preset(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path((id, preset_id)): Path<(i64, i64)>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let preset = presets::get(&state.db, preset_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let status = match current_status(&state, &p).await {
        Ok(s) => s,
        Err(e) => return flash_error(&e),
    };
    // The preset's hotend temperature goes to the first extruder only.
    let script: Vec<String> = [("extruder", preset.extruder), ("heater_bed", preset.bed)]
        .into_iter()
        .filter(|(object, _)| status.contains_key(*object))
        .filter_map(|(object, t)| set_target_gcode(object, t?))
        .collect();
    if script.is_empty() {
        return Err(AppError::BadRequest(format!(
            "{} has none of the heaters in preset {}.",
            p.name, preset.name
        )));
    }
    let script = script.join("\n");
    let action = Action {
        path: GCODE_SCRIPT,
        query: vec![("script", script.as_str())],
        audit: "temp_preset",
        detail: Some(format!("preset={} {}", preset.name, preset_summary(&preset))),
        done: format!("Preset {} applied.", preset.name),
    };
    run_action(&state, ip, &user, &p, action).await
}

pub async fn cooldown(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let action = Action {
        path: GCODE_SCRIPT,
        query: vec![("script", "TURN_OFF_HEATERS")],
        audit: "temp_cooldown",
        detail: None,
        done: "All heaters off.".into(),
    };
    run_action(&state, ip, &user, &p, action).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heater_names() {
        for ok in [
            "extruder",
            "extruder1",
            "heater_bed",
            "heater_generic chamber",
            "temperature_fan exhaust",
        ] {
            assert!(is_heater(ok), "{ok}");
        }
        for bad in [
            "extruder_stepper belt",
            "fan",
            "temperature_sensor chamber",
            "heater_generic",
            "heater_generic a b",
            "heater_generic x\nM112",
            "heater_generic x;M112",
            "heater_bed ",
            "",
        ] {
            assert!(!is_heater(bad), "{bad:?}");
        }
    }

    #[test]
    fn gcode() {
        assert_eq!(
            set_target_gcode("extruder", 215.0).unwrap(),
            "SET_HEATER_TEMPERATURE HEATER=extruder TARGET=215"
        );
        assert_eq!(
            set_target_gcode("heater_generic chamber", 45.5).unwrap(),
            "SET_HEATER_TEMPERATURE HEATER=chamber TARGET=45.5"
        );
        assert_eq!(
            set_target_gcode("temperature_fan exhaust", 40.0).unwrap(),
            "SET_TEMPERATURE_FAN_TARGET TEMPERATURE_FAN=exhaust TARGET=40"
        );
        assert_eq!(set_target_gcode("extruder", 501.0), None);
        assert_eq!(set_target_gcode("extruder", -1.0), None);
        assert_eq!(set_target_gcode("fan", 10.0), None);
    }

    #[test]
    fn targets() {
        assert_eq!(parse_target(" 215 "), Ok(215.0));
        assert_eq!(parse_target("0"), Ok(0.0));
        for bad in ["", "abc", "-5", "501", "NaN", "inf"] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lists_heaters_hotends_first() {
        let status = json!({
            "heater_bed": {"temperature": 60.04, "target": 60.0},
            "extruder": {"temperature": 214.96, "target": 215.0},
            "temperature_sensor mcu": {"temperature": 40.0},
            "fan": {"speed": 0.5}
        });
        let hs = heaters(status.as_object().unwrap());
        let got: Vec<(&str, &str, &str)> = hs
            .iter()
            .map(|h| (h.label.as_str(), h.temp.as_str(), h.target.as_str()))
            .collect();
        assert_eq!(got, [("extruder", "215.0", "215"), ("bed", "60.0", "60")]);
    }

    #[test]
    fn summaries() {
        let p = Preset {
            id: 1,
            name: "PLA".into(),
            extruder: Some(215.0),
            bed: None,
        };
        assert_eq!(preset_summary(&p), "215/— °C");
    }
}
