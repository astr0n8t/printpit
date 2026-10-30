//! The System tab: the printer host's hardware, OS, services, versions and live load.
//!
//! Two parts load separately when the tab opens: `info` (static: hardware, services,
//! versions, updates) and `stats` (CPU, memory, temperature, network, disk). Only `stats`
//! auto-refreshes, every `PRINTPIT_SYSTEM_REFRESH_SECS`, and only if the user's auto-refresh
//! pref is on. IP addresses and the hostname identify the printer host, so only admins see them.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{CurrentUser, Role};
use crate::db::prefs;
use crate::error::AppError;
use crate::moonraker::types::{ProcStats, SystemInfo, UpdateStatus};
use crate::moonraker::{MoonrakerClient, MoonrakerError};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::widgets;

#[derive(Template)]
#[template(path = "tabs/system.html")]
struct SystemTab {
    select: Option<String>,
    id: i64,
}

pub struct Meter {
    pub label: String,
    pub filled: String,
    pub empty: String,
    pub value: String,
    pub class: &'static str,
}

fn meter(label: impl Into<String>, fraction: f64, value: String) -> Meter {
    let (filled, empty) = widgets::bar(fraction);
    Meter {
        label: label.into(),
        filled,
        empty,
        value,
        class: match fraction {
            f if f >= 0.9 => "err",
            f if f >= 0.75 => "warn",
            _ => "",
        },
    }
}

struct Net {
    name: String,
    rate: String,
    total: String,
    addresses: Vec<String>,
}

#[derive(Template)]
#[template(path = "partials/system_stats.html")]
struct Stats {
    id: i64,
    error: Option<String>,
    meters: Vec<Meter>,
    cores: Vec<Meter>,
    facts: Vec<(&'static str, String)>,
    throttled: Vec<String>,
    networks: Vec<Net>,
    refresh_ms: Option<u64>,
}

struct Service {
    name: String,
    state: String,
    class: &'static str,
}

struct Component {
    name: String,
    version: String,
    remote: String,
    status: &'static str,
    class: &'static str,
}

#[derive(Template)]
#[template(path = "partials/system_info.html")]
struct Info {
    id: i64,
    error: Option<String>,
    host: Vec<(&'static str, String)>,
    services: Vec<Service>,
    versions: Vec<(&'static str, String)>,
    components: Vec<Component>,
    updates_error: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    let html = match select::select(&state, user.id, q.printer, "/ui/system").await? {
        None => SystemTab { select: None, id: 0 },
        Some(sel) => SystemTab {
            id: sel.printer.id,
            select: Some(sel.select.render()?),
        },
    }
    .render()?;
    page(hx, &user, Tab::System, html)
}

fn fail(e: &MoonrakerError) -> String {
    format!("{}: {}", e.label(), e.detail())
}

async fn client(state: &AppState, id: i64) -> Result<Result<MoonrakerClient, MoonrakerError>, AppError> {
    let p = enabled_printer(state, id).await?;
    Ok(state.printer_client(&p))
}

// ── Live stats ───────────────────────────────────────────────────────

fn build_stats(
    id: i64,
    s: &ProcStats,
    disk: Option<(u64, u64)>,
    is_admin: bool,
    info: Option<&SystemInfo>,
) -> Stats {
    let mut meters = Vec::new();
    if let Some(cpu) = s.system_cpu_usage.get("cpu") {
        meters.push(meter("cpu", cpu / 100.0, format!("{cpu:.0}%")));
    }
    let mem = &s.system_memory;
    if mem.total > 0 {
        let used = mem.total.saturating_sub(mem.available);
        meters.push(meter(
            "memory",
            used as f64 / mem.total as f64,
            format!(
                "{} / {}",
                widgets::human_size(used * 1024),
                widgets::human_size(mem.total * 1024)
            ),
        ));
    }
    if let Some((used, total)) = disk.filter(|(_, t)| *t > 0) {
        meters.push(meter(
            "disk",
            used as f64 / total as f64,
            format!("{} / {}", widgets::human_size(used), widgets::human_size(total)),
        ));
    }
    let mut cores: Vec<(&String, &f64)> = s.system_cpu_usage.iter().filter(|(k, _)| *k != "cpu").collect();
    cores.sort_by_key(|(k, _)| k.trim_start_matches("cpu").parse::<u32>().unwrap_or(u32::MAX));
    let cores = cores
        .into_iter()
        .map(|(k, v)| meter(k.clone(), v / 100.0, format!("{v:.0}%")))
        .collect();

    let mut facts = Vec::new();
    if let Some(t) = s.cpu_temp {
        facts.push(("cpu temp", format!("{t:.1} °C")));
    }
    if let Some(up) = s.system_uptime {
        facts.push(("uptime", widgets::uptime(up)));
    }
    if let Some(m) = s.moonraker_stats.last() {
        let kb = if m.mem_units.eq_ignore_ascii_case("kb") || m.mem_units.is_empty() {
            1024
        } else {
            1
        };
        facts.push((
            "moonraker",
            format!(
                "cpu {:.1}% · mem {}",
                m.cpu_usage,
                widgets::human_size(m.memory * kb)
            ),
        ));
    }
    if let Some(n) = s.websocket_connections {
        facts.push(("websockets", n.to_string()));
    }

    let networks = s
        .network
        .iter()
        .filter(|(name, _)| *name != "lo")
        .map(|(name, n)| Net {
            name: name.clone(),
            rate: format!("{}/s", widgets::human_size(n.bandwidth.max(0.0) as u64)),
            total: format!(
                "rx {} · tx {}",
                widgets::human_size(n.rx_bytes),
                widgets::human_size(n.tx_bytes)
            ),
            addresses: if is_admin {
                info.and_then(|i| i.network.get(name))
                    .map(|iface| {
                        iface
                            .ip_addresses
                            .iter()
                            .filter(|a| !a.is_link_local)
                            .map(|a| a.address.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        })
        .collect();

    Stats {
        id,
        error: None,
        meters,
        cores,
        facts,
        throttled: s.throttled_state.flags.clone(),
        networks,
        refresh_ms: None,
    }
}

pub async fn stats(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let client = client(&state, id).await?;
    let is_admin = user.has(Role::Admin);
    let result = match &client {
        Ok(c) => {
            let disk = async {
                state
                    .dirs
                    .get(id, c, "")
                    .await
                    .ok()
                    .and_then(|l| l.disk_usage)
                    .map(|d| (d.used, d.total))
            };
            // Admins also see interface addresses, which live in system_info.
            let info = async {
                if is_admin {
                    c.system_info().await.ok()
                } else {
                    None
                }
            };
            let (stats, disk, info) = tokio::join!(c.proc_stats(), disk, info);
            stats.map(|s| build_stats(id, &s, disk, is_admin, info.as_ref()))
        }
        Err(e) => Err(e.clone()),
    };
    let mut view = result.unwrap_or_else(|e| Stats {
        id,
        error: Some(fail(&e)),
        meters: Vec::new(),
        cores: Vec::new(),
        facts: Vec::new(),
        throttled: Vec::new(),
        networks: Vec::new(),
        refresh_ms: None,
    });
    if prefs::get(&state.db, user.id).await?.auto_refresh {
        view.refresh_ms = Some(state.config.system_refresh.as_millis() as u64);
    }
    Ok(Html(view.render()?).into_response())
}

// ── Static info ──────────────────────────────────────────────────────

fn host_facts(i: &SystemInfo, hostname: Option<String>) -> Vec<(&'static str, String)> {
    let c = &i.cpu_info;
    let mut f = Vec::new();
    let mut push = |label, v: String| {
        if !v.trim().is_empty() {
            f.push((label, v.trim().to_string()));
        }
    };
    push("hostname", hostname.unwrap_or_default());
    push(
        "model",
        if c.model.is_empty() {
            c.hardware_desc.clone()
        } else {
            c.model.clone()
        },
    );
    push(
        "cpu",
        format!(
            "{}{}",
            if c.cpu_desc.is_empty() {
                &c.processor
            } else {
                &c.cpu_desc
            },
            c.cpu_count.map(|n| format!(" × {n}")).unwrap_or_default()
        ),
    );
    if let Some(total) = c.total_memory {
        let bytes = if c.memory_units.eq_ignore_ascii_case("kb") {
            total * 1024
        } else {
            total
        };
        push("memory", widgets::human_size(bytes));
    }
    push("os", i.distribution.name.clone());
    let sd = &i.sd_info;
    push(
        "storage",
        [
            sd.manufacturer.as_str(),
            sd.product_name.as_str(),
            sd.capacity.as_str(),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" "),
    );
    push(
        "python",
        i.python
            .version_string
            .split(' ')
            .next()
            .unwrap_or("")
            .to_string(),
    );
    if !i.virtualization.virt_type.is_empty() && i.virtualization.virt_type != "none" {
        push("virtualized", i.virtualization.virt_type.clone());
    }
    f
}

fn services(i: &SystemInfo) -> Vec<Service> {
    let mut names = i.available_services.clone();
    for name in i.service_state.keys() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
        .into_iter()
        .map(|name| {
            let state = i.service_state.get(&name);
            let (text, class) = match state {
                Some(s) if s.active_state == "active" => (s.sub_state.clone(), "badge-ok"),
                Some(s) if s.active_state == "failed" => ("failed".to_string(), "badge-err"),
                Some(s) => (s.active_state.clone(), "badge-warn"),
                None => ("unknown".to_string(), "badge-dim"),
            };
            Service {
                name,
                state: text.to_uppercase(),
                class,
            }
        })
        .collect()
}

fn components(u: &UpdateStatus) -> Vec<Component> {
    u.version_info
        .iter()
        .filter(|(name, _)| *name != "system")
        .map(|(name, v)| {
            let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let (version, remote) = (get("version"), get("remote_version"));
            let valid = v.get("is_valid").and_then(Value::as_bool).unwrap_or(true);
            let dirty = v.get("is_dirty").and_then(Value::as_bool).unwrap_or(false);
            let (status, class) = if !valid {
                ("INVALID", "badge-err")
            } else if dirty {
                ("DIRTY", "badge-warn")
            } else if !remote.is_empty() && remote != "?" && remote != version {
                ("UPDATE", "badge-info")
            } else {
                ("OK", "badge-ok")
            };
            Component {
                name: name.clone(),
                version,
                remote,
                status,
                class,
            }
        })
        .chain(u.version_info.get("system").map(|v| {
            let n = v.get("package_count").and_then(Value::as_u64).unwrap_or(0);
            Component {
                name: "system packages".into(),
                version: String::new(),
                remote: if n > 0 {
                    format!("{n} upgradable")
                } else {
                    String::new()
                },
                status: if n > 0 { "UPDATE" } else { "OK" },
                class: if n > 0 { "badge-info" } else { "badge-ok" },
            }
        }))
        .collect()
}

pub async fn info(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let client = client(&state, id).await?;
    let is_admin = user.has(Role::Admin);
    let view = match &client {
        Err(e) => Info {
            id,
            error: Some(fail(e)),
            host: Vec::new(),
            services: Vec::new(),
            versions: Vec::new(),
            components: Vec::new(),
            updates_error: None,
        },
        Ok(c) => {
            let (system, server, updates) = tokio::join!(c.system_info(), c.server_info(), c.update_status());
            let printer = match &server {
                Ok(s) if s.klippy_connected => c.printer_info().await.ok(),
                _ => None,
            };
            let mut versions = Vec::new();
            if let Ok(s) = &server {
                versions.push(("moonraker", s.moonraker_version.clone()));
                versions.push(("api", s.api_version_string.clone()));
                versions.push(("klippy", s.klippy_state.clone()));
                if !s.failed_components.is_empty() {
                    versions.push(("failed", s.failed_components.join(", ")));
                }
            }
            if let Some(p) = &printer {
                versions.insert(0, ("klipper", p.software_version.clone()));
            }
            versions.retain(|(_, v)| !v.is_empty());
            let hostname = printer.as_ref().filter(|_| is_admin).map(|p| p.hostname.clone());
            match system {
                Ok(sys) => Info {
                    id,
                    error: None,
                    host: host_facts(&sys, hostname),
                    services: services(&sys),
                    versions,
                    components: updates.as_ref().map(components).unwrap_or_default(),
                    updates_error: updates.err().map(|e| match e {
                        MoonrakerError::Http { status: 404, .. } => {
                            "The update manager isn't enabled.".into()
                        }
                        e => fail(&e),
                    }),
                },
                Err(e) => Info {
                    id,
                    error: Some(fail(&e)),
                    host: Vec::new(),
                    services: Vec::new(),
                    versions,
                    components: Vec::new(),
                    updates_error: None,
                },
            }
        }
    };
    Ok(Html(view.render()?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meter_classes() {
        assert_eq!(meter("cpu", 0.5, String::new()).class, "");
        assert_eq!(meter("cpu", 0.8, String::new()).class, "warn");
        assert_eq!(meter("cpu", 0.95, String::new()).class, "err");
    }

    #[test]
    fn update_components() {
        let u: UpdateStatus = serde_json::from_value(json!({"version_info": {
            "moonraker": {"version": "v0.9.3-12", "remote_version": "v0.9.3-20", "is_valid": true},
            "klipper": {"version": "v0.12.0", "remote_version": "v0.12.0"},
            "mainsail": {"version": "v2.12.0", "remote_version": "v2.12.0", "is_dirty": true},
            "system": {"package_count": 3}
        }}))
        .unwrap();
        let c = components(&u);
        let by = |n: &str| c.iter().find(|c| c.name == n).unwrap().status;
        assert_eq!(by("moonraker"), "UPDATE");
        assert_eq!(by("klipper"), "OK");
        assert_eq!(by("mainsail"), "DIRTY");
        assert_eq!(by("system packages"), "UPDATE");
    }
}
