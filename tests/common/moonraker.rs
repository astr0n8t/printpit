//! A tiny fake Moonraker for integration tests. Records every request it receives.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct Seen {
    pub path: String,
    pub api_key: Option<String>,
}

#[derive(Clone)]
pub struct MockMoonraker {
    pub port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[derive(Clone)]
struct Mock {
    required_key: Option<String>,
    klippy_state: &'static str,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl MockMoonraker {
    pub async fn start() -> Self {
        Self::start_with(None, "ready").await
    }

    pub async fn start_with(required_key: Option<&str>, klippy_state: &'static str) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            required_key: required_key.map(str::to_string),
            klippy_state,
            seen: seen.clone(),
        };
        let app = Router::new()
            .route("/server/info", get(server_info))
            .route("/printer/info", get(printer_info))
            .layer(axum::middleware::from_fn_with_state(
                mock.clone(),
                record_and_auth,
            ))
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { port, seen }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn record_and_auth(State(mock): State<Mock>, req: Request, next: axum::middleware::Next) -> Response {
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    mock.seen.lock().unwrap().push(Seen {
        path: req.uri().path().to_string(),
        api_key: api_key.clone(),
    });
    if let Some(required) = &mock.required_key
        && api_key.as_ref() != Some(required)
    {
        let body = json!({"error": {"code": 401, "message": "Unauthorized"}});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }
    next.run(req).await
}

async fn server_info(State(mock): State<Mock>) -> impl IntoResponse {
    axum::Json(json!({"result": {
        "klippy_connected": mock.klippy_state != "disconnected",
        "klippy_state": mock.klippy_state,
        "moonraker_version": "v0.9.3-12",
        "api_version_string": "1.5.0",
        "components": ["file_manager"],
    }}))
}

async fn printer_info(State(mock): State<Mock>) -> Response {
    if mock.klippy_state == "disconnected" {
        let body = json!({"error": {"code": 503, "message": "Klippy Disconnected"}});
        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
    }
    let message = match mock.klippy_state {
        "ready" => "Printer is ready",
        "startup" => "Klipper is starting up",
        _ => "MCU 'mcu' shutdown: Timer too close",
    };
    axum::Json(json!({"result": {
        "state": mock.klippy_state,
        "state_message": message,
        "hostname": "voron",
        "software_version": "v0.12.0-301-g1f8e3c3d",
    }}))
    .into_response()
}
