//! A small fake Moonraker for integration tests: HTTP API, JSON-RPC WebSocket with periodic
//! `notify_status_update` pushes, webcam snapshot/MJPEG, and print actions. It records every
//! request and counts open WebSockets and camera streams.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub api_key: Option<String>,
}

#[derive(Clone, Copy)]
pub struct MockOptions {
    pub required_key: Option<&'static str>,
    pub klippy_state: &'static str,
    pub webcam: bool,
    /// The webcam's configured orientation (`flip_horizontal`, `rotation`).
    pub webcam_flip_h: bool,
    pub webcam_rotation: i32,
    /// Answer the WebSocket upgrade with 404 (forces the HTTP polling fallback).
    pub no_websocket: bool,
    /// Ignore `Range` headers on log downloads (forces the streaming tail fallback).
    pub no_range: bool,
    /// Answer `/machine/update/status` with 404, as when `[update_manager]` isn't configured.
    pub no_update_manager: bool,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            required_key: None,
            klippy_state: "ready",
            webcam: true,
            webcam_flip_h: false,
            webcam_rotation: 0,
            no_websocket: false,
            no_range: false,
            no_update_manager: false,
        }
    }
}

#[derive(Clone)]
pub struct MockMoonraker {
    pub port: u16,
    inner: Arc<Inner>,
}

struct Inner {
    opts: MockOptions,
    seen: Mutex<Vec<Seen>>,
    print_state: Mutex<String>,
    ws_open: AtomicUsize,
    ws_total: AtomicUsize,
    streams_open: AtomicUsize,
    log_bytes_sent: AtomicUsize,
    fs: Mutex<MockFs>,
    scripts: Mutex<Vec<String>>,
}

/// In-memory `gcodes` root: file path → (bytes, modified), plus directories.
pub struct MockFs {
    pub files: BTreeMap<String, (Vec<u8>, f64)>,
    pub dirs: BTreeSet<String>,
    /// The read-only `logs` root.
    pub logs: BTreeMap<String, (Vec<u8>, f64)>,
}

/// A klippy.log with 6000 lines: mostly Stats, some errors, ending in a known line.
pub fn klippy_log() -> Vec<u8> {
    let mut out = String::new();
    for i in 0..6000 {
        match i % 50 {
            0 => out.push_str(&format!("Traceback (most recent call last): line {i}\n")),
            1 => out.push_str(&format!("[WARNING] heater lag at line {i}\n")),
            2 => out.push('\n'),
            _ => out.push_str(&format!("Stats {i}.0: gcodein=0 mcu: mcu_awake=0.002 line {i}\n")),
        }
    }
    out.push_str("LAST LINE marker\n");
    out.into_bytes()
}

impl Default for MockFs {
    fn default() -> Self {
        let mut files = BTreeMap::new();
        files.insert("old.gcode".to_string(), (vec![b'G'; 1000], 1_600_000_000.0));
        files.insert(
            "parts/clip.gcode".to_string(),
            (vec![b'G'; 2_500_000], 1_700_000_000.0),
        );
        files.insert(
            "parts/.thumbs/clip-300x300.png".to_string(),
            (TESTCARD.to_vec(), 1_700_000_000.0),
        );
        let dirs = ["parts", "parts/.thumbs"].into_iter().map(String::from).collect();
        let mut logs = BTreeMap::new();
        logs.insert("klippy.log".to_string(), (klippy_log(), 1_800_000_000.0));
        logs.insert(
            "moonraker.log".to_string(),
            (b"moonraker started\nall good\n".to_vec(), 1_700_000_000.0),
        );
        logs.insert("empty.log".to_string(), (Vec::new(), 1_600_000_000.0));
        Self { files, dirs, logs }
    }
}

type Mock = Arc<Inner>;

impl MockMoonraker {
    pub async fn start() -> Self {
        Self::start_opts(MockOptions::default()).await
    }

    pub async fn start_with(required_key: Option<&'static str>, klippy_state: &'static str) -> Self {
        Self::start_opts(MockOptions {
            required_key,
            klippy_state,
            ..MockOptions::default()
        })
        .await
    }

    pub async fn start_opts(opts: MockOptions) -> Self {
        Self::start_on(0, opts).await
    }

    /// Binds 127.0.0.1:`port` (0 = any free port).
    pub async fn start_on(port: u16, opts: MockOptions) -> Self {
        let inner = Arc::new(Inner {
            opts,
            seen: Mutex::new(Vec::new()),
            print_state: Mutex::new("printing".into()),
            ws_open: AtomicUsize::new(0),
            ws_total: AtomicUsize::new(0),
            streams_open: AtomicUsize::new(0),
            log_bytes_sent: AtomicUsize::new(0),
            fs: Mutex::new(MockFs::default()),
            scripts: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/server/info", get(server_info))
            .route("/printer/info", get(printer_info))
            .route("/printer/objects/list", get(objects_list))
            .route("/printer/objects/query", get(objects_query))
            .route("/server/webcams/list", get(webcams))
            .route("/server/files/list", get(files))
            .route(
                "/server/files/directory",
                get(directory).post(mkdir).delete(rmdir),
            )
            .route("/server/files/metadata", get(metadata))
            .route("/server/files/move", post(move_item))
            .route("/server/files/copy", post(copy_item))
            .route("/server/files/upload", post(upload))
            .route(
                "/server/files/gcodes/{*path}",
                get(download).merge(delete(delete_file)),
            )
            .route("/server/files/logs/{*path}", get(log_file))
            .route("/machine/system_info", get(system_info))
            .route("/machine/proc_stats", get(proc_stats))
            .route("/machine/update/status", get(update_status))
            .route("/printer/print/{action}", post(print_action))
            .route("/printer/emergency_stop", post(estop))
            .route("/printer/gcode/script", post(gcode_script))
            .route("/webcam/", get(webcam))
            .route("/websocket", get(websocket))
            .layer(axum::middleware::from_fn_with_state(
                inner.clone(),
                record_and_auth,
            ))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { port, inner }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.inner.seen.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.path).collect()
    }

    pub fn ws_open(&self) -> usize {
        self.inner.ws_open.load(Ordering::SeqCst)
    }

    pub fn ws_total(&self) -> usize {
        self.inner.ws_total.load(Ordering::SeqCst)
    }

    pub fn streams_open(&self) -> usize {
        self.inner.streams_open.load(Ordering::SeqCst)
    }

    pub fn log_bytes_sent(&self) -> usize {
        self.inner.log_bytes_sent.load(Ordering::SeqCst)
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.inner.fs.lock().unwrap().files.contains_key(path)
    }

    pub fn file_len(&self, path: &str) -> Option<usize> {
        self.inner
            .fs
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|(d, _)| d.len())
    }

    pub fn has_dir(&self, path: &str) -> bool {
        self.inner.fs.lock().unwrap().dirs.contains(path)
    }

    /// G-code scripts received on `/printer/gcode/script`, in order.
    pub fn scripts(&self) -> Vec<String> {
        self.inner.scripts.lock().unwrap().clone()
    }

    pub fn print_state(&self) -> String {
        self.inner.print_state.lock().unwrap().clone()
    }
}

async fn record_and_auth(State(mock): State<Mock>, req: Request, next: axum::middleware::Next) -> Response {
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let path = match req.uri().query() {
        Some(q) if req.uri().path() == "/webcam/" => format!("/webcam/?{q}"),
        _ => req.uri().path().to_string(),
    };
    mock.seen.lock().unwrap().push(Seen {
        path,
        api_key: api_key.clone(),
    });
    let is_camera = req.uri().path() == "/webcam/";
    if let Some(required) = mock.opts.required_key
        && !is_camera
        && api_key.as_deref() != Some(required)
    {
        let body = json!({"error": {"code": 401, "message": "Unauthorized"}});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }
    next.run(req).await
}

fn server_info_json(mock: &Mock) -> Value {
    json!({
        "klippy_connected": mock.opts.klippy_state != "disconnected",
        "klippy_state": mock.opts.klippy_state,
        "moonraker_version": "v0.9.3-12",
        "api_version_string": "1.5.0",
    })
}

fn printer_info_json(mock: &Mock) -> Value {
    let message = match mock.opts.klippy_state {
        "ready" => "Printer is ready",
        "startup" => "Klipper is starting up",
        _ => "MCU 'mcu' shutdown: Timer too close",
    };
    json!({
        "state": mock.opts.klippy_state,
        "state_message": message,
        "hostname": "voron",
        "software_version": "v0.12.0-301-g1f8e3c3d",
    })
}

const OBJECTS: &[&str] = &[
    "print_stats",
    "virtual_sdcard",
    "extruder",
    "heater_bed",
    "heater_generic chamber",
    "fan",
    "toolhead",
    "gcode_macro PRINT_START",
];

fn status_json(mock: &Mock, temperature: f64) -> Value {
    json!({
        "print_stats": {"state": mock.print_state.lock().unwrap().clone(), "filename": PRINTING_FILE,
                        "print_duration": 600.0, "total_duration": 660.0, "message": "",
                        "info": {"current_layer": 12, "total_layer": 120}},
        "virtual_sdcard": {"progress": 0.25, "is_active": true, "file_position": 1000},
        "extruder": {"temperature": temperature, "target": 215.0, "power": 0.42},
        "heater_bed": {"temperature": 60.0, "target": 60.0, "power": 0.18},
        "heater_generic chamber": {"temperature": 38.0, "target": 0.0, "power": 0.0},
        "fan": {"speed": 0.5},
    })
}

async fn server_info(State(mock): State<Mock>) -> impl IntoResponse {
    axum::Json(json!({"result": server_info_json(&mock)}))
}

async fn printer_info(State(mock): State<Mock>) -> Response {
    if mock.opts.klippy_state == "disconnected" {
        let body = json!({"error": {"code": 503, "message": "Klippy Disconnected"}});
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
    }
    axum::Json(json!({"result": printer_info_json(&mock)})).into_response()
}

async fn objects_list() -> impl IntoResponse {
    axum::Json(json!({"result": {"objects": OBJECTS}}))
}

async fn objects_query(State(mock): State<Mock>) -> impl IntoResponse {
    axum::Json(json!({"result": {"eventtime": 1.0, "status": status_json(&mock, 210.0)}}))
}

async fn webcams(State(mock): State<Mock>) -> impl IntoResponse {
    let cams = if mock.opts.webcam {
        json!([{"name": "cam", "enabled": true, "service": "mjpegstreamer",
                "snapshot_url": "/webcam/?action=snapshot", "stream_url": "/webcam/?action=stream",
                "flip_horizontal": mock.opts.webcam_flip_h, "flip_vertical": false,
                "rotation": mock.opts.webcam_rotation}])
    } else {
        json!([])
    };
    axum::Json(json!({"result": {"webcams": cams}}))
}

async fn files(State(mock): State<Mock>) -> impl IntoResponse {
    let fs = mock.fs.lock().unwrap();
    let list: Vec<Value> = fs
        .files
        .iter()
        .filter(|(p, _)| !p.contains("/."))
        .map(|(p, (d, m))| json!({"path": p, "modified": m, "size": d.len()}))
        .collect();
    axum::Json(json!({"result": list}))
}

fn not_found(msg: &str) -> Response {
    let body = json!({"error": {"code": 404, "message": msg}});
    (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
}

fn ok(result: Value) -> Response {
    axum::Json(json!({"result": result})).into_response()
}

/// Strips the `gcodes/` root prefix Moonraker's directory endpoints use.
fn rel(path: &str) -> String {
    path.trim_start_matches("gcodes").trim_matches('/').to_string()
}

fn child_of(dir: &str, path: &str) -> Option<String> {
    let rest = if dir.is_empty() {
        path
    } else {
        path.strip_prefix(&format!("{dir}/"))?
    };
    (!rest.contains('/')).then(|| rest.to_string())
}

async fn directory(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let full = q.get("path").map(String::as_str).unwrap_or("gcodes");
    let fs = mock.fs.lock().unwrap();
    if full.trim_matches('/') == "logs" {
        let files: Vec<Value> = fs
            .logs
            .iter()
            .map(|(name, (d, m))| json!({"filename": name, "modified": m, "size": d.len()}))
            .collect();
        return ok(json!({"dirs": [], "files": files}));
    }
    let dir = rel(full);
    if !dir.is_empty() && !fs.dirs.contains(&dir) {
        return not_found(&format!("Directory does not exist ({dir})"));
    }
    let dirs: Vec<Value> = fs
        .dirs
        .iter()
        .filter_map(|d| child_of(&dir, d))
        .map(|name| json!({"dirname": name, "modified": 1_700_000_000.0, "size": 4096}))
        .collect();
    let files: Vec<Value> = fs
        .files
        .iter()
        .filter_map(|(p, (d, m))| child_of(&dir, p).map(|name| (name, d.len(), *m)))
        .map(|(name, size, m)| {
            let mut f = json!({"filename": name, "modified": m, "size": size});
            if name.ends_with(".gcode") {
                f["estimated_time"] = json!(3725.0);
                f["filament_total"] = json!(1520.5);
            }
            f
        })
        .collect();
    ok(json!({"dirs": dirs, "files": files,
              "disk_usage": {"total": 30_000_000_000u64, "used": 10_000_000_000u64, "free": 20_000_000_000u64}}))
}

/// The file the mock is "printing" (see `status_json`). It isn't in the `gcodes` root, so the
/// file listings stay as they are, but it has metadata like a real job would.
pub const PRINTING_FILE: &str = "benchy.gcode";
pub const PRINTING_ESTIMATE: f64 = 3000.0;

fn metadata_json(mock: &Mock, name: &str) -> Option<Value> {
    if name == PRINTING_FILE {
        return Some(
            json!({"size": 123_456, "modified": 1_700_000_000.0, "slicer": "OrcaSlicer",
                           "estimated_time": PRINTING_ESTIMATE, "thumbnails": []}),
        );
    }
    let fs = mock.fs.lock().unwrap();
    let (data, modified) = fs.files.get(name)?;
    let thumbs = if name == "parts/clip.gcode" {
        json!([{"width": 32, "height": 32, "size": 900, "relative_path": ".thumbs/clip-32x32.png"},
               {"width": 300, "height": 300, "size": 1768, "relative_path": ".thumbs/clip-300x300.png"}])
    } else {
        json!([])
    };
    Some(
        json!({"size": data.len(), "modified": modified, "slicer": "PrusaSlicer", "slicer_version": "2.8.1",
                "estimated_time": 3725.0, "filament_total": 1520.5, "filament_weight_total": 4.6,
                "layer_height": 0.2, "object_height": 12.4, "nozzle_diameter": 0.4,
                "first_layer_extr_temp": 215.0, "first_layer_bed_temp": 60.0, "thumbnails": thumbs}),
    )
}

async fn metadata(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let name = q.get("filename").cloned().unwrap_or_default();
    match metadata_json(&mock, &name) {
        Some(m) => ok(m),
        None => not_found("Metadata not available"),
    }
}

async fn download(State(mock): State<Mock>, Path(path): Path<String>) -> Response {
    match mock.fs.lock().unwrap().files.get(&path) {
        Some((data, _)) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], data.clone()).into_response()
        }
        None => not_found("File does not exist"),
    }
}

async fn system_info() -> Response {
    ok(json!({"system_info": {
        "provider": "systemd_dbus",
        "cpu_info": {"cpu_count": 4, "bits": "64bit", "processor": "aarch64", "cpu_desc": "Cortex-A72",
                     "serial_number": "SERIAL-SHOULD-NOT-SHOW", "hardware_desc": "BCM2835",
                     "model": "Raspberry Pi 4 Model B Rev 1.4", "total_memory": 3884916, "memory_units": "kB"},
        "sd_info": {"manufacturer": "SanDisk", "product_name": "SN32G", "capacity": "29.7 GiB",
                    "serial_number": "SD-SERIAL-SHOULD-NOT-SHOW"},
        "distribution": {"name": "Debian GNU/Linux 12 (bookworm)", "id": "debian", "codename": "bookworm"},
        "available_services": ["klipper", "moonraker", "crowsnest"],
        "service_state": {
            "klipper": {"active_state": "active", "sub_state": "running"},
            "moonraker": {"active_state": "active", "sub_state": "running"},
            "crowsnest": {"active_state": "failed", "sub_state": "failed"}
        },
        "virtualization": {"virt_type": "none", "virt_identifier": "none"},
        "python": {"version": [3, 11, 2, "final", 0], "version_string": "3.11.2 (main, Mar 13 2023)"},
        "network": {"wlan0": {"mac_address": "dc:a6:32:00:00:01", "ip_addresses": [
            {"family": "ipv4", "address": "10.20.0.11", "is_link_local": false},
            {"family": "ipv6", "address": "fe80::1", "is_link_local": true}]}}
    }}))
}

async fn proc_stats() -> Response {
    ok(json!({
        "moonraker_stats": [{"time": 1.0, "cpu_usage": 2.5, "memory": 40960, "mem_units": "kB"}],
        "throttled_state": {"bits": 327680, "flags": ["Previously Under-Volted", "Previously Throttled"]},
        "cpu_temp": 52.3,
        "network": {
            "lo": {"rx_bytes": 1, "tx_bytes": 1, "bandwidth": 10.0},
            "wlan0": {"rx_bytes": 52_428_800u64, "tx_bytes": 10_485_760u64, "bandwidth": 2048.0}
        },
        "system_cpu_usage": {"cpu": 41.5, "cpu0": 80.0, "cpu1": 20.0, "cpu2": 33.0, "cpu3": 95.0},
        "system_memory": {"total": 3884916, "available": 2884916, "used": 1000000},
        "system_uptime": 1_036_800.0,
        "websocket_connections": 3
    }))
}

async fn update_status(State(mock): State<Mock>) -> Response {
    if mock.opts.no_update_manager {
        return not_found("No registered endpoint");
    }
    ok(json!({"busy": false, "version_info": {
        "moonraker": {"version": "v0.9.3-12", "remote_version": "v0.9.3-20", "is_valid": true, "is_dirty": false},
        "klipper": {"version": "v0.12.0-301", "remote_version": "v0.12.0-301", "is_valid": true, "is_dirty": false},
        "system": {"package_count": 2, "package_list": ["libc6", "openssl"]}
    }}))
}

/// Serves a log, honouring a suffix `Range: bytes=-N` unless `no_range` is set.
async fn log_file(
    State(mock): State<Mock>,
    Path(path): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some((data, _)) = mock.fs.lock().unwrap().logs.get(&path).cloned() else {
        return not_found("File does not exist");
    };
    let suffix = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes=-"))
        .and_then(|n| n.parse::<usize>().ok());
    match suffix {
        Some(_) if data.is_empty() && !mock.opts.no_range => {
            StatusCode::RANGE_NOT_SATISFIABLE.into_response()
        }
        Some(n) if !mock.opts.no_range => {
            let start = data.len().saturating_sub(n);
            let part = data[start..].to_vec();
            mock.log_bytes_sent.fetch_add(part.len(), Ordering::SeqCst);
            let range = format!("bytes {start}-{}/{}", data.len() - 1, data.len());
            (
                StatusCode::PARTIAL_CONTENT,
                [(header::CONTENT_RANGE, range)],
                part,
            )
                .into_response()
        }
        _ => {
            mock.log_bytes_sent.fetch_add(data.len(), Ordering::SeqCst);
            ([(header::CONTENT_TYPE, "text/plain")], data).into_response()
        }
    }
}

async fn delete_file(State(mock): State<Mock>, Path(path): Path<String>) -> Response {
    match mock.fs.lock().unwrap().files.remove(&path) {
        Some(_) => ok(json!({"item": {"path": path, "root": "gcodes"}, "action": "delete_file"})),
        None => not_found("File does not exist"),
    }
}

async fn mkdir(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let dir = rel(q.get("path").map(String::as_str).unwrap_or_default());
    mock.fs.lock().unwrap().dirs.insert(dir.clone());
    ok(json!({"item": {"path": dir, "root": "gcodes"}, "action": "create_dir"}))
}

async fn rmdir(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let dir = rel(q.get("path").map(String::as_str).unwrap_or_default());
    let mut fs = mock.fs.lock().unwrap();
    if !fs.dirs.remove(&dir) {
        return not_found("Directory does not exist");
    }
    let prefix = format!("{dir}/");
    fs.files.retain(|p, _| !p.starts_with(&prefix));
    fs.dirs.retain(|d| !d.starts_with(&prefix));
    ok(json!({"item": {"path": dir, "root": "gcodes"}, "action": "delete_dir"}))
}

fn transfer(mock: &Mock, q: &BTreeMap<String, String>, keep_source: bool) -> Response {
    let (src, dst) = (
        rel(q.get("source").map(String::as_str).unwrap_or_default()),
        rel(q.get("dest").map(String::as_str).unwrap_or_default()),
    );
    let mut fs = mock.fs.lock().unwrap();
    let Some(entry) = (if keep_source {
        fs.files.get(&src).cloned()
    } else {
        fs.files.remove(&src)
    }) else {
        return not_found("File does not exist");
    };
    fs.files.insert(dst.clone(), entry);
    ok(json!({"item": {"path": dst, "root": "gcodes"}}))
}

async fn move_item(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    transfer(&mock, &q, false)
}

async fn copy_item(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    transfer(&mock, &q, true)
}

async fn upload(State(mock): State<Mock>, mut form: Multipart) -> Response {
    let (mut dir, mut print, mut file) = (String::new(), false, None);
    while let Ok(Some(field)) = form.next_field().await {
        match field.name() {
            Some("path") => dir = field.text().await.unwrap_or_default(),
            Some("print") => print = field.text().await.unwrap_or_default() == "true",
            Some("file") => {
                let name = field.file_name().unwrap_or("upload.gcode").to_string();
                match field.bytes().await {
                    Ok(bytes) => file = Some((name, bytes.to_vec())),
                    Err(_) => return (StatusCode::BAD_REQUEST, "upload aborted").into_response(),
                }
            }
            _ => {}
        }
    }
    let Some((name, data)) = file else {
        return (StatusCode::BAD_REQUEST, "no file").into_response();
    };
    let path = if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    };
    mock.fs
        .lock()
        .unwrap()
        .files
        .insert(path.clone(), (data, 1_800_000_000.0));
    if print {
        *mock.print_state.lock().unwrap() = "printing".into();
    }
    // Real Moonraker answers 201 with an unwrapped body on some versions.
    (
        StatusCode::CREATED,
        axum::Json(json!({"item": {"path": path, "root": "gcodes"},
        "print_started": print, "action": "create_file"})),
    )
        .into_response()
}

async fn print_action(
    State(mock): State<Mock>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Response {
    let next = match action.as_str() {
        "pause" => "paused",
        "resume" | "start" => "printing",
        "cancel" => "cancelled",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    *mock.print_state.lock().unwrap() = next.into();
    axum::Json(json!({"result": "ok"})).into_response()
}

async fn estop() -> impl IntoResponse {
    axum::Json(json!({"result": "ok"}))
}

/// A 320×180 colour-bar PNG, so the camera tile shows something in the browser.
pub const TESTCARD: &[u8] = include_bytes!("../fixtures/testcard.png");

struct StreamGuard(Mock);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.streams_open.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn webcam(
    State(mock): State<Mock>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    match q.get("action").map(String::as_str) {
        Some("snapshot") => ([(header::CONTENT_TYPE, "image/png")], TESTCARD).into_response(),
        Some("stream") => {
            mock.streams_open.fetch_add(1, Ordering::SeqCst);
            let guard = StreamGuard(mock.clone());
            let frames = async_stream::stream! {
                let _guard = guard;
                loop {
                    let mut part = format!(
                        "--frame\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
                        TESTCARD.len()
                    )
                    .into_bytes();
                    part.extend_from_slice(TESTCARD);
                    part.extend_from_slice(b"\r\n");
                    yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(part));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            };
            let mut res = Response::new(Body::from_stream(frames));
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=frame".parse().unwrap(),
            );
            res
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Records the script; like Klipper, refuses targets above the heater's `max_temp` (300 here).
async fn gcode_script(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let script = q.get("script").cloned().unwrap_or_default();
    for line in script.lines() {
        if let Some(t) = line.split("TARGET=").nth(1).and_then(|t| t.parse::<f64>().ok())
            && t > 300.0
        {
            let msg = format!("Requested temperature ({t:.1}) out of range (0.0:300.0)");
            let body = json!({"error": {"code": 400, "message": msg}});
            return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }
    }
    mock.scripts.lock().unwrap().push(script);
    ok(json!("ok"))
}

async fn websocket(State(mock): State<Mock>, ws: WebSocketUpgrade) -> Response {
    if mock.opts.no_websocket {
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| session(mock, socket))
}

async fn session(mock: Mock, mut socket: WebSocket) {
    mock.ws_open.fetch_add(1, Ordering::SeqCst);
    mock.ws_total.fetch_add(1, Ordering::SeqCst);
    let mut subscribed = false;
    let mut temperature = 200.0;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            msg = socket.recv() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    if matches!(msg, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break; }
                    continue;
                };
                let req: Value = serde_json::from_str(text.as_str()).unwrap();
                let method = req["method"].as_str().unwrap_or_default().to_string();
                mock.seen.lock().unwrap().push(Seen { path: format!("ws:{method}"), api_key: None });
                let result = match method.as_str() {
                    "server.info" => server_info_json(&mock),
                    "printer.info" => printer_info_json(&mock),
                    "printer.objects.list" => json!({"objects": OBJECTS}),
                    "printer.objects.subscribe" => {
                        subscribed = true;
                        json!({"eventtime": 1.0, "status": status_json(&mock, temperature)})
                    }
                    "server.files.metadata" => {
                        metadata_json(&mock, req["params"]["filename"].as_str().unwrap_or_default())
                            .unwrap_or(Value::Null)
                    }
                    _ => json!(null),
                };
                let reply = if result.is_null() && method == "server.files.metadata" {
                    json!({"jsonrpc": "2.0", "error": {"code": 404, "message": "Metadata not available"},
                           "id": req["id"]})
                } else {
                    json!({"jsonrpc": "2.0", "result": result, "id": req["id"]})
                };
                if socket.send(Message::Text(reply.to_string().into())).await.is_err() { break; }
            }
            _ = tick.tick(), if subscribed => {
                temperature += 1.0;
                let state = mock.print_state.lock().unwrap().clone();
                let note = json!({"jsonrpc": "2.0", "method": "notify_status_update", "params": [
                    {"extruder": {"temperature": temperature}, "print_stats": {"state": state}}, 2.0]});
                if socket.send(Message::Text(note.to_string().into())).await.is_err() { break; }
            }
        }
    }
    mock.ws_open.fetch_sub(1, Ordering::SeqCst);
}
