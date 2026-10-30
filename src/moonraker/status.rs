//! Live printer status: which Klipper objects to watch, merging Moonraker's partial updates,
//! and the snapshot that the hub publishes to viewers.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::MoonrakerError;

/// How the hub is currently connected to the printer.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Connecting,
    /// Subscribed over the WebSocket.
    Live,
    /// WebSocket unavailable; polling `/printer/objects/query` over HTTP.
    Polling,
    /// Moonraker is up but Klipper isn't ready (`startup`, `shutdown`, `error`, `disconnected`).
    NotReady {
        state: String,
        message: String,
    },
    Down(MoonrakerError),
}

/// What viewers see: connection state, the merged object status, and short temperature
/// histories for sparklines.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub link: Link,
    pub status: Map<String, Value>,
    pub history: BTreeMap<String, Vec<f64>>,
}

impl Snapshot {
    pub fn connecting() -> Self {
        Self {
            link: Link::Connecting,
            status: Map::new(),
            history: BTreeMap::new(),
        }
    }
}

/// Fields printpit reads from each kind of Klipper object. Objects not listed are not watched.
pub fn fields_for(object: &str) -> Option<&'static [&'static str]> {
    let mut parts = object.split_whitespace();
    let kind = parts.next()?;
    // Klipper convention: names starting with "_" are hidden.
    if parts.next().is_some_and(|name| name.starts_with('_')) {
        return None;
    }
    const HEATER: &[&str] = &["temperature", "target", "power"];
    Some(match kind {
        "print_stats" => &[
            "filename",
            "total_duration",
            "print_duration",
            "filament_used",
            "state",
            "message",
            "info",
        ],
        "virtual_sdcard" => &["progress", "is_active", "file_position"],
        "display_status" => &["progress", "message"],
        "webhooks" => &["state", "state_message"],
        "idle_timeout" => &["state"],
        "heater_bed" | "heater_generic" => HEATER,
        k if k == "extruder"
            || k.strip_prefix("extruder")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) =>
        {
            HEATER
        }
        "temperature_sensor" => &["temperature"],
        "temperature_fan" => &["temperature", "target", "speed"],
        "fan" | "heater_fan" | "controller_fan" | "fan_generic" => &["speed"],
        _ => return None,
    })
}

/// The `{object: [fields]}` map for `printer.objects.subscribe` / `query`.
pub fn subscription(objects: &[String]) -> Map<String, Value> {
    objects
        .iter()
        .filter_map(|o| fields_for(o).map(|f| (o.clone(), Value::from(f.to_vec()))))
        .collect()
}

/// Query string for `GET /printer/objects/query` (`?extruder=temperature,target&...`).
pub fn query_string(sub: &Map<String, Value>) -> String {
    sub.iter()
        .map(|(object, fields)| {
            let fields = fields
                .as_array()
                .map(|f| f.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            format!(
                "{}={}",
                url::form_urlencoded::byte_serialize(object.as_bytes()).collect::<String>(),
                url::form_urlencoded::byte_serialize(fields.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Merges a `notify_status_update` delta. Moonraker only sends changed fields, so objects
/// (and nested objects like `print_stats.info`) are merged rather than replaced.
pub fn merge(state: &mut Map<String, Value>, delta: &Map<String, Value>) {
    for (key, value) in delta {
        match (state.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(update)) => merge(existing, update),
            _ => {
                state.insert(key.clone(), value.clone());
            }
        }
    }
}

const HISTORY_LEN: usize = 40;
const HISTORY_EVERY: Duration = Duration::from_secs(2);

/// Samples every object's `temperature` at most every 2 s into short ring buffers.
#[derive(Default)]
pub struct History {
    last: Option<Instant>,
    series: BTreeMap<String, VecDeque<f64>>,
}

impl History {
    pub fn record(&mut self, status: &Map<String, Value>) {
        let now = Instant::now();
        if self.last.is_some_and(|t| now.duration_since(t) < HISTORY_EVERY) {
            return;
        }
        self.last = Some(now);
        for (name, obj) in status {
            if let Some(t) = obj.get("temperature").and_then(Value::as_f64) {
                let series = self.series.entry(name.clone()).or_default();
                if series.len() == HISTORY_LEN {
                    series.pop_front();
                }
                series.push_back(t);
            }
        }
    }

    pub fn snapshot(&self) -> BTreeMap<String, Vec<f64>> {
        self.series
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().copied().collect()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_interesting_objects() {
        let objects: Vec<String> = [
            "print_stats",
            "extruder",
            "extruder1",
            "extruder_stepper belt",
            "heater_bed",
            "temperature_sensor chamber",
            "temperature_sensor _internal",
            "fan",
            "gcode_macro PRINT_START",
            "toolhead",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let sub = subscription(&objects);
        let keys: Vec<&str> = sub.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "extruder",
                "extruder1",
                "fan",
                "heater_bed",
                "print_stats",
                "temperature_sensor chamber"
            ]
        );
        assert_eq!(sub["fan"], json!(["speed"]));
    }

    #[test]
    fn query_string_encodes_names_and_fields() {
        let sub = subscription(&["temperature_sensor chamber".to_string(), "fan".to_string()]);
        assert_eq!(
            query_string(&sub),
            "fan=speed&temperature_sensor+chamber=temperature"
        );
    }

    #[test]
    fn merge_is_deep() {
        let mut state = json!({
            "extruder": {"temperature": 20.0, "target": 0.0},
            "print_stats": {"state": "printing", "info": {"current_layer": 1, "total_layer": 10}}
        })
        .as_object()
        .unwrap()
        .clone();
        let delta = json!({
            "extruder": {"temperature": 21.5},
            "print_stats": {"info": {"current_layer": 2}},
            "fan": {"speed": 0.5}
        });
        merge(&mut state, delta.as_object().unwrap());
        assert_eq!(state["extruder"], json!({"temperature": 21.5, "target": 0.0}));
        assert_eq!(
            state["print_stats"]["info"],
            json!({"current_layer": 2, "total_layer": 10})
        );
        assert_eq!(state["print_stats"]["state"], "printing");
        assert_eq!(state["fan"]["speed"], 0.5);
    }

    #[test]
    fn history_samples_and_caps() {
        let mut h = History::default();
        for i in 0..(HISTORY_LEN + 5) {
            h.last = None; // bypass the 2 s spacing
            let status = json!({"extruder": {"temperature": i as f64}, "fan": {"speed": 1.0}});
            h.record(status.as_object().unwrap());
        }
        let snap = h.snapshot();
        assert_eq!(snap.len(), 1, "only objects with a temperature");
        assert_eq!(snap["extruder"].len(), HISTORY_LEN);
        assert_eq!(snap["extruder"][0], 5.0);
    }
}
