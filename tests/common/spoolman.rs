//! A small fake Spoolman (`/api/v1/info`, `/api/v1/spool`, `PUT /api/v1/spool/{id}/use`),
//! served under a path prefix like a Spoolman behind a reverse proxy. Records every request.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use serde_json::{Value, json};

pub const PREFIX: &str = "/spoolman";

#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub auth: Option<String>,
}

#[derive(Default)]
struct Inner {
    calls: Mutex<Vec<Call>>,
    /// spool id → (filament name, material, remaining g)
    spools: Mutex<BTreeMap<i64, (String, String, f64)>>,
    /// (spool id, use body) for every accepted use.
    uses: Mutex<Vec<(i64, Value)>>,
    down: AtomicBool,
    /// Answer this many `use` calls with 500 before accepting.
    fail_uses: AtomicUsize,
    required_token: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct MockSpoolman {
    pub port: u16,
    inner: Arc<Inner>,
}

type Mock = Arc<Inner>;

impl MockSpoolman {
    /// Spools #1 (PLA), #2 (PETG) and #3 (TPU, 50 g left).
    pub async fn start() -> Self {
        Self::start_on(0).await
    }

    /// Binds 127.0.0.1:`port` (0 = any free port).
    pub async fn start_on(port: u16) -> Self {
        let inner = Arc::new(Inner::default());
        {
            let mut spools = inner.spools.lock().unwrap();
            spools.insert(1, ("PolyLite Black".into(), "PLA".into(), 900.0));
            spools.insert(2, ("Prusament Galaxy".into(), "PETG".into(), 650.5));
            spools.insert(3, ("NinjaFlex".into(), "TPU".into(), 50.0));
        }
        let api = Router::new()
            .route("/api/v1/info", get(info))
            .route("/api/v1/spool", get(list))
            .route("/api/v1/spool/{id}/use", put(use_spool));
        let app = Router::new()
            .nest(PREFIX, api)
            .layer(axum::middleware::from_fn_with_state(inner.clone(), record))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { port, inner }
    }

    /// The base URL to configure, with the path prefix.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}{PREFIX}", self.port)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls.lock().unwrap().clone()
    }

    /// Accepted uses: (spool id, `{"use_length": …}` or `{"use_weight": …}`).
    pub fn uses(&self) -> Vec<(i64, Value)> {
        self.inner.uses.lock().unwrap().clone()
    }

    pub fn set_down(&self, down: bool) {
        self.inner.down.store(down, Ordering::SeqCst);
    }

    pub fn fail_next_uses(&self, n: usize) {
        self.inner.fail_uses.store(n, Ordering::SeqCst);
    }

    pub fn require_token(&self, token: &str) {
        *self.inner.required_token.lock().unwrap() = Some(format!("Bearer {token}"));
    }

    pub fn remove_spool(&self, id: i64) {
        self.inner.spools.lock().unwrap().remove(&id);
    }
}

async fn record(State(mock): State<Mock>, req: Request, next: axum::middleware::Next) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    mock.calls.lock().unwrap().push(Call {
        method: req.method().to_string(),
        path: req.uri().path().to_string(),
        auth: auth.clone(),
    });
    if mock.down.load(Ordering::SeqCst) {
        return (StatusCode::BAD_GATEWAY, "upstream down").into_response();
    }
    let required = mock.required_token.lock().unwrap().clone();
    if required.is_some() && required != auth {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

async fn info() -> Response {
    axum::Json(json!({"version": "0.22.1", "debug_mode": false})).into_response()
}

async fn list(State(mock): State<Mock>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    assert_eq!(q.get("allow_archived").map(String::as_str), Some("false"));
    let spools: Vec<Value> = mock
        .spools
        .lock()
        .unwrap()
        .iter()
        .map(|(id, (name, material, left))| {
            json!({"id": id, "registered": "2026-01-01T00:00:00Z", "archived": false,
                   "remaining_weight": left, "used_weight": 0.0, "used_length": 0.0, "location": "shelf",
                   "filament": {"id": id + 100, "name": name, "material": material, "density": 1.24,
                                "diameter": 1.75, "vendor": {"id": 1, "name": "Acme"}}})
        })
        .collect();
    axum::Json(spools).into_response()
}

async fn use_spool(
    State(mock): State<Mock>,
    Path(id): Path<i64>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if mock
        .fail_uses
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"message": "database is locked"})),
        )
            .into_response();
    }
    let mut spools = mock.spools.lock().unwrap();
    let Some(spool) = spools.get_mut(&id) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message": "Item not found"})),
        )
            .into_response();
    };
    if let Some(g) = body.get("use_weight").and_then(Value::as_f64) {
        spool.2 -= g;
    } else if let Some(mm) = body.get("use_length").and_then(Value::as_f64) {
        // 1.75 mm at 1.24 g/cm³.
        spool.2 -= mm * std::f64::consts::PI * 0.875 * 0.875 * 1.24 / 1000.0;
    } else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"message": "Either use_weight or use_length must be specified."})),
        )
            .into_response();
    }
    mock.uses.lock().unwrap().push((id, body));
    axum::Json(json!({"id": id})).into_response()
}
