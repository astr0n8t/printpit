//! Print notifications: a webhook POST when a watched printer finishes, fails or cancels a
//! print.
//!
//! **This is the one deliberate exception to the lazy-loading rule.** For every enabled printer
//! an admin has opted in (`printers.notify`), the [`Notifier`] holds a hub [`Subscription`]
//! for as long as printpit runs, so that printer's Moonraker connection stays open with or
//! without viewers. Printers that haven't opted in are untouched.
//!
//! The webhook URL comes only from `PRINTPIT_NOTIFY_WEBHOOK_URL` and is sent through its own
//! HTTP client, not the printer egress client: it is usually on the internet, not the printer
//! network. It is never logged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::WebhookUrl;
use crate::db::{Db, printers};
use crate::moonraker::hub::{PrinterHub, Subscription};
use crate::moonraker::status::{Link, Snapshot};
use crate::web::widgets;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Waits before the second and third delivery attempts.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// A finished, failed or cancelled print.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// `complete`, `error` or `cancelled`.
    pub kind: &'static str,
    pub filename: String,
    /// Seconds spent printing.
    pub duration: f64,
    pub message: String,
}

/// Turns a printer's stream of snapshots into [`Event`]s.
///
/// Only a transition out of `printing`/`paused` fires, so the first state seen after printpit
/// starts is just a baseline. The last state survives reconnects: a print that finished while
/// the connection was down is still reported once it is back, and a reconnect that finds the
/// same finished job doesn't report it twice.
#[derive(Debug, Default)]
pub struct Tracker {
    last: Option<String>,
    filename: String,
    duration: f64,
}

impl Tracker {
    fn active(&self) -> bool {
        matches!(self.last.as_deref(), Some("printing" | "paused"))
    }

    pub fn observe(&mut self, snap: &Snapshot) -> Option<Event> {
        match &snap.link {
            Link::Live | Link::Polling => {
                let stats = snap.status.get("print_stats")?;
                let state = stats.get("state")?.as_str()?;
                let was_active = self.active();
                if let Some(f) = stats.get("filename").and_then(Value::as_str) {
                    self.filename = f.to_string();
                }
                if let Some(d) = stats.get("print_duration").and_then(Value::as_f64) {
                    self.duration = d;
                }
                self.last = Some(state.to_string());
                let kind = match state {
                    "complete" => "complete",
                    "error" => "error",
                    "cancelled" => "cancelled",
                    _ => return None,
                };
                was_active.then(|| Event {
                    kind,
                    filename: self.filename.clone(),
                    duration: self.duration,
                    message: stats
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            }
            // Klipper shutting down mid-print ends the job without print_stats saying so.
            Link::NotReady { state, message } if matches!(state.as_str(), "shutdown" | "error") => {
                if !self.active() {
                    return None;
                }
                self.last = Some("error".into());
                Some(Event {
                    kind: "error",
                    filename: self.filename.clone(),
                    duration: self.duration,
                    message: message.clone(),
                })
            }
            _ => None,
        }
    }
}

/// The JSON body: structured fields plus `text` (Slack-style) and `content` (Discord-style)
/// summaries, so common chat webhooks work as they are. Names the printer, never its address.
pub fn payload(printer: &str, e: &Event) -> Value {
    let took = widgets::duration(e.duration);
    let mut text = match e.kind {
        "complete" => format!("{printer}: finished {} in {took}", e.filename),
        "cancelled" => format!("{printer}: cancelled {} after {took}", e.filename),
        _ => format!("{printer}: {} failed after {took}", e.filename),
    };
    if e.kind == "error" && !e.message.trim().is_empty() {
        text = format!("{text}: {}", e.message.trim());
    }
    json!({
        "printer": printer,
        "event": e.kind,
        "filename": e.filename,
        "duration": e.duration,
        "message": e.message,
        "text": text,
        "content": text,
    })
}

/// Keeps one watch task per opted-in printer and delivers their events.
pub struct Notifier {
    db: Db,
    hub: Arc<PrinterHub>,
    sender: Option<Sender>,
    watches: Mutex<HashMap<i64, JoinHandle<()>>>,
}

#[derive(Clone)]
struct Sender {
    http: reqwest::Client,
    url: WebhookUrl,
}

impl Notifier {
    /// Without a webhook URL the notifier never watches anything.
    pub fn new(db: Db, hub: Arc<PrinterHub>, url: Option<WebhookUrl>) -> Result<Arc<Self>, reqwest::Error> {
        let sender = match url {
            Some(url) => {
                let _ = rustls::crypto::ring::default_provider().install_default();
                let http = reqwest::Client::builder()
                    .user_agent(concat!("printpit/", env!("CARGO_PKG_VERSION")))
                    .timeout(TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                Some(Sender { http, url })
            }
            None => None,
        };
        Ok(Arc::new(Self {
            db,
            hub,
            sender,
            watches: Mutex::new(HashMap::new()),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.sender.is_some()
    }

    /// Printers currently watched (for tests and diagnostics).
    pub async fn watching(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.watches.lock().await.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Starts watching newly opted-in printers and stops watching the rest. Called at startup
    /// and after every printer change.
    pub async fn sync(self: &Arc<Self>) {
        let Some(sender) = &self.sender else {
            return;
        };
        // Held across the read so overlapping syncs apply in order.
        let mut watches = self.watches.lock().await;
        let wanted: Vec<i64> = match printers::list(&self.db).await {
            Ok(list) => list
                .into_iter()
                .filter(|p| p.enabled && p.notify)
                .map(|p| p.id)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't load printers for notifications");
                return;
            }
        };
        watches.retain(|id, task| {
            let keep = wanted.contains(id) && !task.is_finished();
            if !keep {
                task.abort();
                tracing::info!(printer_id = id, "stopped watching for notifications");
            }
            keep
        });
        for id in wanted {
            watches.entry(id).or_insert_with(|| {
                tracing::info!(printer_id = id, "watching for notifications");
                tokio::spawn(watch(self.db.clone(), self.hub.subscribe(id), id, sender.clone()))
            });
        }
    }
}

async fn watch(db: Db, mut sub: Subscription, printer_id: i64, sender: Sender) {
    let mut tracker = Tracker::default();
    loop {
        let event = {
            let snap = sub.rx.borrow_and_update();
            tracker.observe(&snap)
        };
        if let Some(event) = event {
            let name = match printers::get(&db, printer_id).await {
                Ok(Some(p)) => p.name,
                _ => format!("printer {printer_id}"),
            };
            tracing::info!(printer = %name, event = event.kind, "print notification");
            let sender = sender.clone();
            tokio::spawn(async move { sender.deliver(payload(&name, &event)).await });
        }
        if sub.rx.changed().await.is_err() {
            return;
        }
    }
}

impl Sender {
    /// Up to three attempts. Failures are logged without the URL.
    async fn deliver(&self, body: Value) {
        for attempt in 0..=RETRY_DELAYS.len() {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAYS[attempt - 1]).await;
            }
            match self.http.post(self.url.0.clone()).json(&body).send().await {
                Ok(res) if res.status().is_success() => return,
                Ok(res) => tracing::warn!(status = %res.status(), attempt, "webhook delivery failed"),
                Err(e) => tracing::warn!(error = %root_cause(e), attempt, "webhook delivery failed"),
            }
        }
        tracing::warn!("giving up on a print notification");
    }
}

/// The innermost error ("Connection refused"), not reqwest's generic "error sending request".
/// The URL is stripped first so it can't appear in the message.
fn root_cause(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut cause: &dyn std::error::Error = &e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moonraker::MoonrakerError;
    use std::collections::BTreeMap;

    fn snap(link: Link, state: &str) -> Snapshot {
        let status = json!({"print_stats": {"state": state, "filename": "benchy.gcode",
                                            "print_duration": 3600.0, "message": "Heater extruder not heating"}});
        Snapshot {
            link,
            status: status.as_object().unwrap().clone(),
            history: BTreeMap::new(),
            estimated_time: None,
        }
    }

    fn kinds(t: &mut Tracker, steps: &[Snapshot]) -> Vec<&'static str> {
        steps
            .iter()
            .filter_map(|s| t.observe(s))
            .map(|e| e.kind)
            .collect()
    }

    #[test]
    fn fires_on_leaving_printing() {
        let mut t = Tracker::default();
        let got = kinds(
            &mut t,
            &[
                snap(Link::Live, "printing"),
                snap(Link::Live, "printing"),
                snap(Link::Live, "complete"),
                snap(Link::Live, "complete"),
                snap(Link::Live, "standby"),
                snap(Link::Live, "printing"),
                snap(Link::Live, "paused"),
                snap(Link::Live, "cancelled"),
                snap(Link::Polling, "printing"),
                snap(Link::Polling, "error"),
            ],
        );
        assert_eq!(got, ["complete", "cancelled", "error"]);
    }

    #[test]
    fn first_state_is_a_baseline() {
        let mut t = Tracker::default();
        assert!(kinds(&mut t, &[snap(Link::Live, "complete"), snap(Link::Live, "error")]).is_empty());
    }

    #[test]
    fn survives_reconnects() {
        let mut t = Tracker::default();
        let down = snap(Link::Down(MoonrakerError::Offline("gone".into())), "printing");
        let got = kinds(
            &mut t,
            &[
                snap(Link::Live, "printing"),
                down.clone(),
                snap(Link::Connecting, ""),
                // Finished while we were away: reported once it's back.
                snap(Link::Live, "complete"),
                down,
                // Same finished job after another reconnect: not again.
                snap(Link::Live, "complete"),
            ],
        );
        assert_eq!(got, ["complete"]);
    }

    #[test]
    fn klipper_shutdown_mid_print_is_a_failure() {
        let mut t = Tracker::default();
        let shutdown = Link::NotReady {
            state: "shutdown".into(),
            message: "MCU 'mcu' shutdown: Timer too close".into(),
        };
        t.observe(&snap(Link::Live, "printing"));
        let e = t.observe(&snap(shutdown.clone(), "printing")).unwrap();
        assert_eq!(e.kind, "error");
        assert_eq!(e.message, "MCU 'mcu' shutdown: Timer too close");
        assert_eq!(e.filename, "benchy.gcode");
        // Still shut down, then restarted into standby: nothing more.
        assert!(t.observe(&snap(shutdown, "")).is_none());
        assert!(t.observe(&snap(Link::Live, "standby")).is_none());
        // A shutdown while idle isn't a failed print.
        let mut idle = Tracker::default();
        idle.observe(&snap(Link::Live, "standby"));
        let link = Link::NotReady {
            state: "shutdown".into(),
            message: "x".into(),
        };
        assert!(idle.observe(&snap(link, "")).is_none());
    }

    #[test]
    fn payloads() {
        let e = Event {
            kind: "complete",
            filename: "benchy.gcode".into(),
            duration: 3725.0,
            message: String::new(),
        };
        let p = payload("Voron", &e);
        assert_eq!(p["text"], "Voron: finished benchy.gcode in 1:02:05");
        assert_eq!(p["content"], p["text"]);
        assert_eq!(p["event"], "complete");
        let failed = Event {
            kind: "error",
            message: "Heater extruder not heating".into(),
            ..e
        };
        assert_eq!(
            payload("Voron", &failed)["text"],
            "Voron: benchy.gcode failed after 1:02:05: Heater extruder not heating"
        );
    }
}
