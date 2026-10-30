//! Turns a printer [`Snapshot`] into the dashboard card a particular viewer sees.
//!
//! The card is rendered in two parts: `live` (state, job, sensors; changes every refresh) and
//! `controls` (only changes when the print state does). Keeping them apart means a button
//! isn't replaced under the user's cursor every second.

use askama::Template;
use serde_json::{Map, Value};

use crate::moonraker::status::{Link, Snapshot};
use crate::web::widgets;

#[derive(Template)]
#[template(path = "partials/card_live.html")]
pub struct Live {
    pub id: i64,
    pub label: String,
    pub class: &'static str,
    pub message: Option<String>,
    pub job: Option<Job>,
    pub sensors: Vec<Sensor>,
    pub fans: Vec<(String, String)>,
    /// Auto-refresh is off: show a manual refresh button.
    pub manual: bool,
    pub polling: bool,
}

#[derive(Template)]
#[template(path = "partials/card_controls.html")]
pub struct Controls {
    pub id: i64,
    pub name: String,
    pub start: bool,
    pub pause: bool,
    pub resume: bool,
    pub cancel: bool,
    pub estop: bool,
}

impl Controls {
    pub fn any(&self) -> bool {
        self.start || self.pause || self.resume || self.cancel || self.estop
    }
}

pub struct Job {
    pub filename: String,
    pub filled: String,
    pub empty: String,
    pub percent: String,
    pub elapsed: String,
    pub eta: Option<String>,
    pub layer: Option<String>,
}

pub struct Sensor {
    pub name: String,
    pub temp: String,
    pub target: Option<String>,
    pub power: Option<String>,
    pub spark: String,
}

pub struct Card {
    pub live: Live,
    pub controls: Controls,
}

fn num(obj: Option<&Value>, field: &str) -> Option<f64> {
    obj?.get(field)?.as_f64()
}

fn text<'a>(obj: Option<&'a Value>, field: &str) -> Option<&'a str> {
    obj?.get(field)?.as_str()
}

/// Display name for a Klipper object: `temperature_sensor chamber` → `chamber`.
fn display_name(object: &str) -> String {
    match object.split_once(' ') {
        Some((_, name)) => name.to_string(),
        None => match object {
            "heater_bed" => "bed".into(),
            "fan" => "part fan".into(),
            other => other.into(),
        },
    }
}

/// Sort key so hotends come first, then the bed, then everything else by name.
fn sensor_rank(object: &str) -> (u8, &str) {
    let kind = object.split(' ').next().unwrap_or(object);
    let rank = match kind {
        k if k.starts_with("extruder") => 0,
        "heater_bed" => 1,
        "heater_generic" => 2,
        "temperature_fan" => 3,
        _ => 4,
    };
    (rank, object)
}

pub fn build(id: i64, name: &str, snap: &Snapshot, can_control: bool, manual: bool) -> Card {
    let status = &snap.status;
    let print_state = text(status.get("print_stats"), "state").unwrap_or("standby");
    let (label, class, message) = match &snap.link {
        Link::Connecting => ("CONNECTING".to_string(), "badge-dim", None),
        Link::Down(e) => (e.label().to_string(), e.class(), Some(e.detail())),
        Link::NotReady { state, message } => (state.to_uppercase(), "badge-warn", Some(message.clone())),
        Link::Live | Link::Polling => {
            let (label, class) = match print_state {
                "printing" => ("PRINTING", "badge-ok"),
                "paused" => ("PAUSED", "badge-warn"),
                "complete" => ("COMPLETE", "badge-info"),
                "cancelled" => ("CANCELLED", "badge-dim"),
                "error" => ("ERROR", "badge-err"),
                _ => ("READY", "badge-info"),
            };
            let msg = text(status.get("print_stats"), "message").filter(|m| !m.is_empty());
            (label.to_string(), class, msg.map(str::to_string))
        }
    };
    let ready = matches!(snap.link, Link::Live | Link::Polling);
    let (job, sensors, fans) = if ready {
        (job(status, print_state), sensors(status, snap), fans(status))
    } else {
        (None, Vec::new(), Vec::new())
    };

    let active = matches!(print_state, "printing" | "paused");
    let can = can_control && ready;
    Card {
        live: Live {
            id,
            label,
            class,
            message,
            job,
            sensors,
            fans,
            manual,
            polling: matches!(snap.link, Link::Polling) && !manual,
        },
        controls: Controls {
            id,
            name: name.to_string(),
            start: can && !active,
            pause: can && print_state == "printing",
            resume: can && print_state == "paused",
            cancel: can && active,
            // E-stop stays available whenever Klipper is reachable, even if not "ready".
            estop: can_control && matches!(snap.link, Link::Live | Link::Polling | Link::NotReady { .. }),
        },
    }
}

fn job(status: &Map<String, Value>, print_state: &str) -> Option<Job> {
    let stats = status.get("print_stats");
    let filename = text(stats, "filename").filter(|f| !f.is_empty())?;
    if !matches!(print_state, "printing" | "paused" | "complete") {
        return None;
    }
    let progress = num(status.get("display_status"), "progress")
        .filter(|p| *p > 0.0)
        .or_else(|| num(status.get("virtual_sdcard"), "progress"))
        .unwrap_or(0.0);
    let progress = if print_state == "complete" { 1.0 } else { progress };
    let print_duration = num(stats, "print_duration").unwrap_or(0.0);
    let total_duration = num(stats, "total_duration").unwrap_or(print_duration);
    let eta = (print_state != "complete" && progress > 0.01)
        .then(|| widgets::duration(print_duration / progress - print_duration));
    let info = stats.and_then(|s| s.get("info"));
    let layer = match (num(info, "current_layer"), num(info, "total_layer")) {
        (Some(c), Some(t)) if t > 0.0 => Some(format!("{c:.0}/{t:.0}")),
        _ => None,
    };
    let (filled, empty) = widgets::bar(progress);
    Some(Job {
        filename: filename.to_string(),
        filled,
        empty,
        percent: format!("{:.1}%", progress * 100.0),
        elapsed: widgets::duration(total_duration),
        eta,
        layer,
    })
}

fn sensors(status: &Map<String, Value>, snap: &Snapshot) -> Vec<Sensor> {
    let mut names: Vec<&String> = status
        .iter()
        .filter(|(_, v)| v.get("temperature").and_then(Value::as_f64).is_some())
        .map(|(k, _)| k)
        .collect();
    names.sort_by_key(|n| sensor_rank(n));
    names
        .into_iter()
        .map(|object| {
            let obj = status.get(object);
            let temp = num(obj, "temperature").unwrap_or(0.0);
            Sensor {
                name: display_name(object),
                temp: format!("{temp:.1}"),
                target: num(obj, "target").map(|t| format!("{t:.0}")),
                power: num(obj, "power")
                    .or_else(|| num(obj, "speed"))
                    .map(|p| format!("{:.0}%", p * 100.0)),
                spark: snap
                    .history
                    .get(object)
                    .map(|h| widgets::sparkline(h))
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn fans(status: &Map<String, Value>) -> Vec<(String, String)> {
    let mut fans: Vec<(String, String)> = status
        .iter()
        .filter(|(k, _)| {
            let kind = k.split(' ').next().unwrap_or(k);
            matches!(kind, "fan" | "heater_fan" | "controller_fan" | "fan_generic")
        })
        .filter_map(|(k, v)| {
            let speed = v.get("speed")?.as_f64()?;
            Some((display_name(k), format!("{:.0}%", speed * 100.0)))
        })
        .collect();
    fans.sort();
    fans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moonraker::MoonrakerError;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn snap(link: Link, status: Value) -> Snapshot {
        Snapshot {
            link,
            status: status.as_object().unwrap().clone(),
            history: BTreeMap::from([("extruder".to_string(), vec![200.0, 210.0, 215.0])]),
        }
    }

    fn printing() -> Value {
        json!({
            "print_stats": {"state": "printing", "filename": "benchy.gcode", "print_duration": 600.0,
                            "total_duration": 660.0, "info": {"current_layer": 12, "total_layer": 120}},
            "virtual_sdcard": {"progress": 0.25},
            "extruder": {"temperature": 214.96, "target": 215.0, "power": 0.42},
            "heater_bed": {"temperature": 60.04, "target": 60.0, "power": 0.18},
            "temperature_sensor chamber": {"temperature": 38.2},
            "fan": {"speed": 0.65},
            "heater_fan hotend_fan": {"speed": 1.0}
        })
    }

    #[test]
    fn printing_card() {
        let card = build(1, "Voron", &snap(Link::Live, printing()), true, false);
        assert_eq!(card.live.label, "PRINTING");
        let job = card.live.job.unwrap();
        assert_eq!(job.percent, "25.0%");
        assert_eq!(job.filled.chars().count(), 5);
        assert_eq!(job.elapsed, "11:00");
        assert_eq!(job.eta.as_deref(), Some("30:00"));
        assert_eq!(job.layer.as_deref(), Some("12/120"));
        let names: Vec<&str> = card.live.sensors.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["extruder", "bed", "chamber"]);
        assert_eq!(card.live.sensors[0].temp, "215.0");
        assert_eq!(card.live.sensors[0].power.as_deref(), Some("42%"));
        assert_eq!(card.live.sensors[0].spark.chars().count(), 3);
        assert_eq!(
            card.live.fans,
            [
                ("hotend_fan".into(), "100%".into()),
                ("part fan".into(), "65%".into())
            ]
        );
        let c = card.controls;
        assert!(c.pause && c.cancel && c.estop && !c.start && !c.resume);
    }

    #[test]
    fn display_progress_preferred() {
        let mut s = printing();
        s["display_status"] = json!({"progress": 0.5});
        let card = build(1, "P", &snap(Link::Live, s), true, false);
        assert_eq!(card.live.job.unwrap().percent, "50.0%");
    }

    #[test]
    fn viewer_gets_no_controls() {
        let card = build(1, "Voron", &snap(Link::Live, printing()), false, false);
        assert!(!card.controls.any());
    }

    #[test]
    fn idle_paused_and_down_states() {
        let idle = build(
            1,
            "P",
            &snap(Link::Live, json!({"print_stats": {"state": "standby"}})),
            true,
            false,
        );
        assert_eq!(idle.live.label, "READY");
        assert!(idle.live.job.is_none());
        assert!(idle.controls.start && !idle.controls.cancel);

        let mut s = printing();
        s["print_stats"]["state"] = json!("paused");
        let paused = build(1, "P", &snap(Link::Live, s), true, false);
        assert_eq!(paused.live.label, "PAUSED");
        assert!(paused.controls.resume && paused.controls.cancel && !paused.controls.pause);

        let down = build(
            1,
            "P",
            &snap(Link::Down(MoonrakerError::Unauthorized), printing()),
            true,
            false,
        );
        assert_eq!(down.live.label, "UNAUTHORIZED");
        assert!(down.live.job.is_none() && down.live.sensors.is_empty());
        assert!(!down.controls.any());

        let link = Link::NotReady {
            state: "shutdown".into(),
            message: "MCU shutdown".into(),
        };
        let nr = build(1, "P", &snap(link, json!({})), true, false);
        assert_eq!(nr.live.label, "SHUTDOWN");
        assert_eq!(nr.live.message.as_deref(), Some("MCU shutdown"));
        assert!(nr.controls.estop && !nr.controls.start);
    }
}
