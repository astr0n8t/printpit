//! A small fake Moonraker for integration tests: HTTP API, JSON-RPC WebSocket with periodic
//! `notify_status_update` pushes, webcam snapshot/MJPEG, and print actions. It records every
//! request and counts open WebSockets and camera streams.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
    /// Answer the WebSocket upgrade with 404 (forces the HTTP polling fallback).
    pub no_websocket: bool,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            required_key: None,
            klippy_state: "ready",
            webcam: true,
            no_websocket: false,
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
        let inner = Arc::new(Inner {
            opts,
            seen: Mutex::new(Vec::new()),
            print_state: Mutex::new("printing".into()),
            ws_open: AtomicUsize::new(0),
            ws_total: AtomicUsize::new(0),
            streams_open: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route("/server/info", get(server_info))
            .route("/printer/info", get(printer_info))
            .route("/printer/objects/list", get(objects_list))
            .route("/printer/objects/query", get(objects_query))
            .route("/server/webcams/list", get(webcams))
            .route("/server/files/list", get(files))
            .route("/printer/print/{action}", post(print_action))
            .route("/printer/emergency_stop", post(estop))
            .route("/webcam/", get(webcam))
            .route("/websocket", get(websocket))
            .layer(axum::middleware::from_fn_with_state(
                inner.clone(),
                record_and_auth,
            ))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
    "fan",
    "toolhead",
    "gcode_macro PRINT_START",
];

fn status_json(mock: &Mock, temperature: f64) -> Value {
    json!({
        "print_stats": {"state": mock.print_state.lock().unwrap().clone(), "filename": "benchy.gcode",
                        "print_duration": 600.0, "total_duration": 660.0, "message": "",
                        "info": {"current_layer": 12, "total_layer": 120}},
        "virtual_sdcard": {"progress": 0.25, "is_active": true, "file_position": 1000},
        "extruder": {"temperature": temperature, "target": 215.0, "power": 0.42},
        "heater_bed": {"temperature": 60.0, "target": 60.0, "power": 0.18},
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
                "snapshot_url": "/webcam/?action=snapshot", "stream_url": "/webcam/?action=stream"}])
    } else {
        json!([])
    };
    axum::Json(json!({"result": {"webcams": cams}}))
}

async fn files() -> impl IntoResponse {
    axum::Json(json!({"result": [
        {"path": "old.gcode", "modified": 1_600_000_000.0, "size": 1000},
        {"path": "parts/clip.gcode", "modified": 1_700_000_000.0, "size": 2_500_000},
    ]}))
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
        Some("snapshot") => (
            [(header::CONTENT_TYPE, "image/jpeg")],
            b"\xff\xd8fake-jpeg\xff\xd9".to_vec(),
        )
            .into_response(),
        Some("stream") => {
            mock.streams_open.fetch_add(1, Ordering::SeqCst);
            let guard = StreamGuard(mock.clone());
            let frames = async_stream::stream! {
                let _guard = guard;
                loop {
                    yield Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                        b"--frame\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8f\xff\xd9\r\n",
                    ));
                    tokio::time::sleep(Duration::from_millis(50)).await;
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
                    _ => json!(null),
                };
                let reply = json!({"jsonrpc": "2.0", "result": result, "id": req["id"]});
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
