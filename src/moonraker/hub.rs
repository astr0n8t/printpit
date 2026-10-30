//! `PrinterHub`: at most one live Moonraker connection per printer, and only while someone is
//! watching.
//!
//! * The first [`Subscription`] for a printer spawns its connection task; later ones share the
//!   same `watch` channel (so updates are coalesced: a slow viewer just sees the latest state).
//! * Dropping the last subscription starts a grace timer; if nobody resubscribes before it
//!   fires, the task (and its WebSocket) is stopped. A generation counter makes the timer a
//!   no-op if viewers came and went in between.
//! * The task prefers the WebSocket (`printer.objects.subscribe`) and falls back to polling
//!   `/printer/objects/query` over HTTP when the WebSocket isn't available.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::rpc::{Message, Rpc};
use super::status::{History, Link, Snapshot, merge, subscription};
use super::types::{ObjectList, ObjectStatus, ServerInfo};
use super::{Egress, MoonrakerClient, MoonrakerError, client_for};
use crate::crypto::Cipher;
use crate::db::{Db, printers};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long to poll over HTTP before trying the WebSocket again.
const POLL_SPELL: Duration = Duration::from_secs(60);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Re-check `server.info` this often while Klipper isn't ready, in case a notification is missed.
const NOT_READY_RECHECK: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct HubDeps {
    pub db: Db,
    pub egress: Egress,
    pub cipher: Arc<Cipher>,
}

#[derive(Clone, Copy)]
pub struct HubConfig {
    /// Maximum rate at which snapshots are published to viewers.
    pub refresh: Duration,
    /// How long a connection outlives its last viewer.
    pub grace: Duration,
}

pub struct PrinterHub {
    deps: HubDeps,
    config: HubConfig,
    entries: Mutex<HashMap<i64, Entry>>,
}

struct Entry {
    viewers: usize,
    generation: u64,
    tx: watch::Sender<Arc<Snapshot>>,
    task: JoinHandle<()>,
}

/// A viewer's handle on a printer's live snapshot. Dropping it releases the viewer.
pub struct Subscription {
    pub rx: watch::Receiver<Arc<Snapshot>>,
    hub: Arc<PrinterHub>,
    printer_id: i64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.release(self.printer_id);
    }
}

impl PrinterHub {
    pub fn new(deps: HubDeps, config: HubConfig) -> Arc<Self> {
        Arc::new(Self {
            deps,
            config,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn subscribe(self: &Arc<Self>, printer_id: i64) -> Subscription {
        let mut entries = self.lock();
        let entry = entries.entry(printer_id).or_insert_with(|| {
            let (tx, _) = watch::channel(Arc::new(Snapshot::connecting()));
            let task = self.spawn(printer_id, tx.clone());
            Entry {
                viewers: 0,
                generation: 0,
                tx,
                task,
            }
        });
        entry.viewers += 1;
        entry.generation += 1;
        Subscription {
            rx: entry.tx.subscribe(),
            hub: self.clone(),
            printer_id,
        }
    }

    /// Restarts a printer's connection after its settings changed. Current viewers stay
    /// subscribed and see `Connecting` until the new connection is up.
    pub fn reset(self: &Arc<Self>, printer_id: i64) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&printer_id) else {
            return;
        };
        entry.task.abort();
        if entry.viewers == 0 {
            entries.remove(&printer_id);
            return;
        }
        entry.tx.send_replace(Arc::new(Snapshot::connecting()));
        entry.task = self.spawn(printer_id, entry.tx.clone());
    }

    /// Printers that currently have a connection task (for tests and diagnostics).
    pub fn active(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.lock().keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    fn spawn(&self, printer_id: i64, tx: watch::Sender<Arc<Snapshot>>) -> JoinHandle<()> {
        tokio::spawn(run(self.deps.clone(), self.config, printer_id, tx))
    }

    fn release(self: &Arc<Self>, printer_id: i64) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&printer_id) else {
            return;
        };
        entry.viewers = entry.viewers.saturating_sub(1);
        if entry.viewers > 0 {
            return;
        }
        let generation = entry.generation;
        drop(entries);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let hub = self.clone();
                let grace = self.config.grace;
                rt.spawn(async move {
                    tokio::time::sleep(grace).await;
                    hub.reap(printer_id, generation);
                });
            }
            Err(_) => self.reap(printer_id, generation),
        }
    }

    fn reap(&self, printer_id: i64, generation: u64) {
        let mut entries = self.lock();
        if entries
            .get(&printer_id)
            .is_some_and(|e| e.viewers == 0 && e.generation == generation)
            && let Some(entry) = entries.remove(&printer_id)
        {
            entry.task.abort();
            tracing::debug!(printer_id, "closed idle printer connection");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One-shot status over HTTP: used for manual refresh and by the polling fallback.
pub async fn poll_once(client: &MoonrakerClient) -> Result<(Link, Map<String, Value>), MoonrakerError> {
    let info = client.server_info().await?;
    if info.klippy_state != "ready" {
        let message = if info.klippy_connected {
            client
                .printer_info()
                .await
                .map(|i| i.state_message)
                .unwrap_or_default()
        } else {
            String::new()
        };
        return Ok((not_ready(&info, message), Map::new()));
    }
    let objects = client.objects_list().await?;
    let status = client.objects_query(&subscription(&objects)).await?;
    Ok((Link::Polling, status))
}

fn not_ready(info: &ServerInfo, message: String) -> Link {
    let state = if info.klippy_state.is_empty() {
        "disconnected".to_string()
    } else {
        info.klippy_state.clone()
    };
    let message = match message.trim() {
        "" if !info.klippy_connected => "Klipper isn't connected to Moonraker.".to_string(),
        "" => format!("Klipper is {state}."),
        m => m.to_string(),
    };
    Link::NotReady { state, message }
}

struct Publisher {
    tx: watch::Sender<Arc<Snapshot>>,
    status: Map<String, Value>,
    history: History,
}

impl Publisher {
    fn publish(&mut self, link: Link) {
        if matches!(link, Link::Live | Link::Polling) {
            self.history.record(&self.status);
        }
        self.tx.send_replace(Arc::new(Snapshot {
            link,
            status: self.status.clone(),
            history: self.history.snapshot(),
        }));
    }
}

async fn run(deps: HubDeps, config: HubConfig, printer_id: i64, tx: watch::Sender<Arc<Snapshot>>) {
    let mut out = Publisher {
        tx,
        status: Map::new(),
        history: History::default(),
    };
    let mut backoff = MIN_BACKOFF;
    loop {
        let client = match load_client(&deps, printer_id).await {
            Ok(c) => c,
            Err(e) => {
                out.publish(Link::Down(e));
                tokio::time::sleep(MAX_BACKOFF).await;
                continue;
            }
        };
        let started = Instant::now();
        let ws_error = session(&client, config, &mut out).await;
        if started.elapsed() > POLL_SPELL {
            backoff = MIN_BACKOFF;
        }
        tracing::debug!(printer_id, error = %ws_error, "printer WebSocket ended");
        // The WebSocket failed. If plain HTTP works (e.g. a proxy without WebSocket support),
        // poll for a while before trying the WebSocket again.
        match poll_for(&client, config, &mut out).await {
            None => backoff = MIN_BACKOFF,
            Some(e) => {
                out.publish(Link::Down(e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn load_client(deps: &HubDeps, printer_id: i64) -> Result<MoonrakerClient, MoonrakerError> {
    let printer = printers::get(&deps.db, printer_id)
        .await
        .map_err(|e| MoonrakerError::Config(format!("database error: {e}")))?
        .ok_or_else(|| MoonrakerError::Config("this printer was removed".into()))?;
    if !printer.enabled {
        return Err(MoonrakerError::Config("this printer is disabled".into()));
    }
    client_for(&deps.egress, &deps.cipher, &printer)
}

/// Polls over HTTP for one spell. `None` if every poll succeeded, else the first error.
async fn poll_for(
    client: &MoonrakerClient,
    config: HubConfig,
    out: &mut Publisher,
) -> Option<MoonrakerError> {
    let interval = config.refresh.max(MIN_POLL_INTERVAL);
    let end = Instant::now() + POLL_SPELL;
    loop {
        match poll_once(client).await {
            Ok((link, status)) => {
                out.status = status;
                out.publish(link);
            }
            Err(e) => return Some(e),
        }
        if Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Runs one WebSocket session until it fails; returns why.
async fn session(client: &MoonrakerClient, config: HubConfig, out: &mut Publisher) -> MoonrakerError {
    let mut rpc = match client.websocket().await {
        Ok(ws) => Rpc::new(ws),
        Err(e) => return e,
    };
    let Err(e) = subscribed(&mut rpc, config, out).await;
    rpc.close().await;
    e
}

async fn subscribed(
    rpc: &mut Rpc,
    config: HubConfig,
    out: &mut Publisher,
) -> Result<Infallible, MoonrakerError> {
    let mut ignore = |_: &str, _: &Value| {};
    'ready: loop {
        let info: ServerInfo = decode(rpc.call("server.info", json!({}), &mut ignore).await?)?;
        if info.klippy_state != "ready" {
            let message = if info.klippy_connected {
                rpc.call("printer.info", json!({}), &mut ignore)
                    .await
                    .ok()
                    .and_then(|v| v.get("state_message").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            out.publish(not_ready(&info, message));
            let recheck = tokio::time::sleep(NOT_READY_RECHECK);
            tokio::pin!(recheck);
            loop {
                tokio::select! {
                    msg = rpc.recv() => {
                        if let Message::Notification { method, .. } = msg?
                            && method == "notify_klippy_ready"
                        {
                            continue 'ready;
                        }
                    }
                    () = &mut recheck => continue 'ready,
                }
            }
        }

        let objects: ObjectList = decode(rpc.call("printer.objects.list", json!({}), &mut ignore).await?)?;
        let params = json!({ "objects": subscription(&objects.objects) });
        let initial: ObjectStatus =
            decode(rpc.call("printer.objects.subscribe", params, &mut ignore).await?)?;
        out.status = initial.status;
        out.publish(Link::Live);

        let mut tick = tokio::time::interval(config.refresh);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut dirty = false;
        loop {
            tokio::select! {
                msg = rpc.recv() => {
                    if let Message::Notification { method, params } = msg? {
                        match method.as_str() {
                            "notify_status_update" => {
                                if let Some(delta) = params.get(0).and_then(Value::as_object) {
                                    merge(&mut out.status, delta);
                                    dirty = true;
                                }
                            }
                            "notify_klippy_disconnected" | "notify_klippy_shutdown" => continue 'ready,
                            _ => {}
                        }
                    }
                }
                _ = tick.tick() => {
                    if dirty {
                        out.publish(Link::Live);
                        dirty = false;
                    }
                }
            }
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, MoonrakerError> {
    serde_json::from_value(v).map_err(|e| MoonrakerError::Decode(e.to_string()))
}
