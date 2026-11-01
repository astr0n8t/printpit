//! The worker's HTTP API (`/v1/*`). Every route, including unknown ones, needs the bearer token.
//!
//! ```text
//! GET  /v1/health                            200 {status, orca_version} | 503 when Orca can't run
//! GET  /v1/info                              worker/Orca versions, limits, busy, vendor list
//! GET  /v1/vendors/{vendor}                  that vendor's selectable machines/processes/filaments
//! GET  /v1/profiles/{kind}/{vendor}/{name}   one system profile, flattened
//! POST /v1/profiles/resolve                  {presets: [...]} → user presets, flattened
//! POST /v1/slice                             multipart → G-code (stats in X-Slice-* headers)
//! ```
//!
//! Errors are JSON: `{error, kind, code?, log_tail?}`. `kind` is one of `unauthorized`, `not_found`,
//! `bad_request`, `too_large`, `busy`, `model`, `profile`, `timeout`, `resources`, `slicer`, `crashed`,
//! `internal`.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::gcode;
use crate::library::{Kind, LibError, Library, Profile, text};
use crate::orca::{self, Finished, Options, Running};

/// JSON parts (profiles) are a few hundred KB at most.
const JSON_PART_MAX: usize = 8 * 1024 * 1024;
const OPTIONS_MAX: usize = 64 * 1024;
const RESOLVE_BODY_MAX: usize = 32 * 1024 * 1024;
/// How long printpit should wait before retrying a busy worker.
const RETRY_AFTER_SECS: u64 = 5;

pub type App = Arc<AppState>;

pub struct AppState {
    pub config: Config,
    pub library: Library,
    /// The version `orca-slicer --help` reported at startup, or why it couldn't run.
    pub orca: Result<String, String>,
    /// One slice at a time.
    slot: Arc<Semaphore>,
}

impl AppState {
    pub fn new(config: Config, library: Library, orca: Result<String, String>) -> App {
        Arc::new(Self {
            config,
            library,
            orca,
            slot: Arc::new(Semaphore::new(1)),
        })
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/info", get(info))
        .route("/v1/vendors/{vendor}", get(vendor))
        .route("/v1/profiles/{kind}/{vendor}/{name}", get(profile))
        .route(
            "/v1/profiles/resolve",
            post(resolve).layer(DefaultBodyLimit::max(RESOLVE_BODY_MAX)),
        )
        // The model's own limit is enforced while streaming it to disk.
        .route("/v1/slice", post(slice).layer(DefaultBodyLimit::disable()))
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route") })
        .layer(middleware::from_fn_with_state(app.clone(), require_token))
        .with_state(app)
}

async fn require_token(State(app): State<App>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|t| app.config.token.matches(t));
    if ok {
        next.run(req).await
    } else {
        let mut res =
            ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "missing or wrong token").into_response();
        res.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        res
    }
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    error: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log_tail: Vec<String>,
}

impl ApiError {
    fn new(status: StatusCode, kind: &'static str, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            kind,
            code: None,
            log_tail: Vec::new(),
        }
    }

    fn bad_request(error: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", error)
    }

    fn slicer(
        kind: &'static str,
        error: impl Into<String>,
        code: Option<i32>,
        log_tail: Vec<String>,
    ) -> Self {
        Self {
            code,
            log_tail,
            ..Self::new(StatusCode::UNPROCESSABLE_ENTITY, kind, error)
        }
    }

    fn internal(what: &str, e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, "{what}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", what)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let mut res = (status, Json(&self)).into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECS));
        }
        res
    }
}

impl From<LibError> for ApiError {
    fn from(e: LibError) -> Self {
        let status = match e {
            LibError::UnknownVendor(_) | LibError::NotFound { .. } => StatusCode::NOT_FOUND,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        let kind = if status == StatusCode::NOT_FOUND {
            "not_found"
        } else {
            "profile"
        };
        Self::new(status, kind, e.to_string())
    }
}

impl From<axum::extract::multipart::MultipartError> for ApiError {
    fn from(e: axum::extract::multipart::MultipartError) -> Self {
        Self::bad_request(format!("malformed upload: {}", e.body_text()))
    }
}

async fn health(State(app): State<App>) -> Response {
    match &app.orca {
        Ok(v) => Json(json!({"status": "ok", "orca_version": v})).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "error", "error": format!("OrcaSlicer can't run: {e}")})),
        )
            .into_response(),
    }
}

async fn info(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "worker_version": env!("CARGO_PKG_VERSION"),
        "orca_version": app.orca.as_ref().ok(),
        "busy": app.slot.available_permits() == 0,
        "timeout_secs": app.config.timeout.as_secs(),
        "max_model_mb": app.config.max_model_bytes / (1024 * 1024),
        "vendors": app.library.vendors(),
    }))
}

async fn vendor(State(app): State<App>, UrlPath(vendor): UrlPath<String>) -> Result<Response, ApiError> {
    // Reads a few dozen small files; off the async threads.
    let app2 = app.clone();
    let index = tokio::task::spawn_blocking(move || app2.library.vendor_index(&vendor))
        .await
        .map_err(|e| ApiError::internal("vendor index failed", e))??;
    Ok(Json(index).into_response())
}

async fn profile(
    State(app): State<App>,
    UrlPath((kind, vendor, name)): UrlPath<(String, String, String)>,
) -> Result<Json<Profile>, ApiError> {
    let kind = Kind::parse(&kind).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "kind must be machine, process or filament",
        )
    })?;
    Ok(Json(app.library.system(&vendor, kind, &name)?))
}

#[derive(Deserialize)]
struct ResolveRequest {
    presets: Vec<Value>,
}

async fn resolve(State(app): State<App>, body: axum::body::Bytes) -> Result<Json<Value>, ApiError> {
    let req: ResolveRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("bad JSON: {e}")))?;
    if req.presets.len() > 500 {
        return Err(ApiError::bad_request("at most 500 presets per request"));
    }
    let presets = req
        .presets
        .into_iter()
        .map(|v| match v {
            Value::Object(m) => Ok(m),
            _ => Err(ApiError::bad_request("each preset must be a JSON object")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let results: Vec<Value> = app
        .library
        .resolve(presets)
        .into_iter()
        .map(|r| match r {
            Ok(p) => json!({"name": p.name, "kind": p.kind, "profile": p.profile}),
            Err(e) => json!({"error": e.to_string()}),
        })
        .collect();
    Ok(Json(json!({ "results": results })))
}

/// What a slice request carried.
struct Upload {
    machine: Profile,
    process: Option<Profile>,
    filament: Option<Profile>,
    options: Options,
    model: String,
}

async fn slice(State(app): State<App>, mut multipart: Multipart) -> Result<Response, ApiError> {
    // Busy is decided before reading the body, so printpit doesn't upload a big model for nothing.
    let permit = app.slot.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "already slicing; retry later",
        )
    })?;
    // 0700, removed when `dir` drops, whatever happens below (including the client going away).
    let dir = tempfile::Builder::new()
        .prefix("slice-")
        .tempdir_in(&app.config.work_dir)
        .map_err(|e| ApiError::internal("cannot create a work dir", e))?;
    let up = read_upload(&mut multipart, dir.path(), app.config.max_model_bytes).await?;

    let Upload {
        mut machine,
        process,
        filament,
        options,
        model,
    } = up;
    let system_name = prepare_machine(&mut machine)?;
    write_json(dir.path(), orca::MACHINE, &machine).await?;
    if let (Some(mut process), Some(filament)) = (process, filament) {
        make_compatible(&mut process, &system_name);
        set_type(&mut process, Kind::Process);
        let mut filament = filament;
        set_type(&mut filament, Kind::Filament);
        write_json(dir.path(), orca::PROCESS, &process).await?;
        write_json(dir.path(), orca::FILAMENT, &filament).await?;
    }

    let timeout = app.config.timeout;
    let args = orca::args(&model, &options, timeout.as_secs());
    let started = std::time::Instant::now();
    let mut running = Running::spawn(&app.config.orca, dir.path(), &args)
        .map_err(|e| ApiError::internal("cannot start the slicer", e))?;
    // Orca's own `--mstpp` limit gets a head start; this is the hard stop.
    let finished = running
        .wait(timeout + app.config.kill_grace)
        .await
        .map_err(|e| ApiError::internal("waiting for the slicer failed", e))?;
    drop(running);
    drop(permit);
    let secs = started.elapsed().as_secs_f32();
    let tail = || orca::log_tail(dir.path(), &app.config.orca);

    let status = match finished {
        Finished::TimedOut => {
            tracing::warn!(secs, "slice timed out");
            return Err(ApiError::slicer(
                "timeout",
                format!("slicing took longer than {} s", timeout.as_secs()),
                None,
                tail(),
            ));
        }
        Finished::Exited(status) => status,
    };
    let Some(result) = orca::read_result(dir.path()) else {
        use std::os::unix::process::ExitStatusExt;
        let (kind, error) = match status.signal() {
            Some(sig) => ("crashed", format!("the slicer crashed (signal {sig})")),
            None => ("slicer", format!("the slicer exited ({status}) without a result")),
        };
        tracing::warn!(secs, %error, "slice failed");
        return Err(ApiError::slicer(kind, error, None, tail()));
    };
    if result.return_code != 0 {
        let kind = orca::classify(result.return_code);
        tracing::info!(secs, code = result.return_code, kind, "slice rejected");
        let error = if result.error_string.trim().is_empty() {
            format!("the slicer failed (code {})", result.return_code)
        } else {
            result.error_string.trim().to_string()
        };
        return Err(ApiError::slicer(kind, error, Some(result.return_code), tail()));
    }
    let gcode = orca::gcode_path(dir.path(), options.plate);
    let Ok(meta) = tokio::fs::metadata(&gcode).await else {
        return Err(ApiError::slicer(
            "slicer",
            format!(
                "the slicer reported success but wrote no G-code for plate {}",
                options.plate
            ),
            None,
            tail(),
        ));
    };
    let stats = gcode::read_stats(&gcode)
        .await
        .map_err(|e| ApiError::internal("cannot read the G-code", e))?;
    tracing::info!(secs, bytes = meta.len(), ?stats, "sliced");

    let file = tokio::fs::File::open(&gcode)
        .await
        .map_err(|e| ApiError::internal("cannot open the G-code", e))?;
    // The temp dir lives as long as the body stream, then is removed.
    let body = tokio_util::io::ReaderStream::new(file).map(move |chunk| {
        let _keep = &dir;
        chunk
    });
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-gcode"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
    if let Ok(v) = &app.orca
        && let Ok(v) = HeaderValue::from_str(v)
    {
        headers.insert("x-orca-version", v);
    }
    let mut put = |name: &'static str, v: Option<String>| {
        if let Some(v) = v.and_then(|v| HeaderValue::from_str(&v).ok()) {
            headers.insert(name, v);
        }
    };
    put("x-slice-print-time-s", stats.print_time_s.map(|v| v.to_string()));
    put(
        "x-slice-filament-mm",
        stats.filament_mm.map(|v| format!("{v:.2}")),
    );
    put("x-slice-filament-g", stats.filament_g.map(|v| format!("{v:.2}")));
    put("x-slice-layers", stats.layers.map(|v| v.to_string()));
    Ok((headers, axum::body::Body::from_stream(body)).into_response())
}

async fn read_upload(multipart: &mut Multipart, dir: &Path, max_model: u64) -> Result<Upload, ApiError> {
    let (mut machine, mut process, mut filament, mut options, mut model) = (None, None, None, None, None);
    while let Some(mut field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if model.is_some() {
            return Err(ApiError::bad_request("the model must be the last part"));
        }
        match name.as_str() {
            "machine" | "process" | "filament" => {
                let raw = read_small(&mut field, JSON_PART_MAX, &name).await?;
                let p = match serde_json::from_slice::<Value>(&raw) {
                    Ok(Value::Object(m)) => m,
                    _ => return Err(ApiError::bad_request(format!("`{name}` must be a JSON object"))),
                };
                let slot = match name.as_str() {
                    "machine" => &mut machine,
                    "process" => &mut process,
                    _ => &mut filament,
                };
                if slot.replace(p).is_some() {
                    return Err(ApiError::bad_request(format!("`{name}` given twice")));
                }
            }
            "options" => {
                let raw = read_small(&mut field, OPTIONS_MAX, &name).await?;
                let o: Options = serde_json::from_slice(&raw)
                    .map_err(|e| ApiError::bad_request(format!("bad options: {e}")))?;
                options = Some(o);
            }
            "model" => {
                let ext = field
                    .file_name()
                    .and_then(|f| Path::new(f).extension())
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .filter(|e| orca::MODEL_EXTENSIONS.contains(&e.as_str()))
                    .ok_or_else(|| {
                        ApiError::bad_request("the model must be an .stl, .3mf, .obj, .step or .stp file")
                    })?;
                let file_name = format!("model.{ext}");
                let mut out = tokio::fs::File::create(dir.join(&file_name))
                    .await
                    .map_err(|e| ApiError::internal("cannot store the model", e))?;
                let mut size = 0u64;
                while let Some(chunk) = field.chunk().await? {
                    size += chunk.len() as u64;
                    if size > max_model {
                        return Err(ApiError::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "too_large",
                            format!("the model is larger than {} MB", max_model / (1024 * 1024)),
                        ));
                    }
                    out.write_all(&chunk)
                        .await
                        .map_err(|e| ApiError::internal("cannot store the model", e))?;
                }
                out.flush()
                    .await
                    .map_err(|e| ApiError::internal("cannot store the model", e))?;
                if size == 0 {
                    return Err(ApiError::bad_request("the model is empty"));
                }
                model = Some(file_name);
            }
            other => return Err(ApiError::bad_request(format!("unexpected part `{other}`"))),
        }
    }
    let model = model.ok_or_else(|| ApiError::bad_request("no `model` part"))?;
    let machine = machine.ok_or_else(|| ApiError::bad_request("no `machine` profile"))?;
    let options = options.unwrap_or_default();
    let ext = model.rsplit('.').next().unwrap_or_default();
    options.validate(ext).map_err(ApiError::bad_request)?;
    if !options.project_settings && (process.is_none() || filament.is_none()) {
        return Err(ApiError::bad_request(
            "`process` and `filament` profiles are required",
        ));
    }
    if options.project_settings {
        // The project's own are used; don't half-apply ours.
        process = None;
        filament = None;
    }
    Ok(Upload {
        machine,
        process,
        filament,
        options,
        model,
    })
}

async fn read_small(
    field: &mut axum::extract::multipart::Field<'_>,
    max: usize,
    name: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut buf = Vec::new();
    while let Some(chunk) = field.chunk().await? {
        if buf.len() + chunk.len() > max {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("`{name}` is too large"),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Makes the machine loadable as-is and returns its *system name*, the name Orca checks a
/// process's `compatible_printers` against: its own for a system profile, else its `inherits`.
fn prepare_machine(machine: &mut Profile) -> Result<String, ApiError> {
    let name = text(machine, "name");
    if name.trim().is_empty() {
        return Err(ApiError::bad_request("the machine profile has no `name`"));
    }
    set_type(machine, Kind::Machine);
    if text(machine, "from").eq_ignore_ascii_case("system") {
        return Ok(name);
    }
    let inherits = text(machine, "inherits");
    if inherits.trim().is_empty() {
        // A machine defined from scratch: it is its own system printer.
        machine.insert("inherits".into(), Value::String(name.clone()));
        Ok(name)
    } else {
        Ok(inherits)
    }
}

/// printpit's choice of process is deliberate, so it is made compatible with the machine rather
/// than refused by Orca (-17) for not listing it.
fn make_compatible(process: &mut Profile, system_name: &str) {
    let list = process
        .entry("compatible_printers")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    let arr = list.as_array_mut().expect("just made an array");
    if !arr.iter().any(|v| v.as_str() == Some(system_name)) {
        arr.push(Value::String(system_name.to_string()));
    }
    process.remove("compatible_printers_condition");
}

fn set_type(p: &mut Profile, kind: Kind) {
    p.insert("type".into(), Value::String(kind.as_str().into()));
}

async fn write_json(dir: &Path, file: &str, p: &Profile) -> Result<(), ApiError> {
    let raw = serde_json::to_vec(p).map_err(|e| ApiError::internal("cannot encode a profile", e))?;
    tokio::fs::write(dir.join(file), raw)
        .await
        .map_err(|e| ApiError::internal("cannot write a profile", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Profile {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn machine_system_names() {
        let mut m = obj(json!({"name": "Voron 2.4 350 0.4 nozzle", "from": "system"}));
        assert_eq!(prepare_machine(&mut m).unwrap(), "Voron 2.4 350 0.4 nozzle");
        assert_eq!(text(&m, "type"), "machine");
        let mut m = obj(json!({"name": "My Voron", "from": "User", "inherits": "Voron 2.4 350 0.4 nozzle"}));
        assert_eq!(prepare_machine(&mut m).unwrap(), "Voron 2.4 350 0.4 nozzle");
        let mut m = obj(json!({"name": "Scratch Built", "from": "User"}));
        assert_eq!(prepare_machine(&mut m).unwrap(), "Scratch Built");
        assert_eq!(text(&m, "inherits"), "Scratch Built");
        assert!(prepare_machine(&mut obj(json!({"from": "system"}))).is_err());
    }

    #[test]
    fn processes_are_made_compatible() {
        let mut p = obj(json!({"compatible_printers": ["A"], "compatible_printers_condition": "x"}));
        make_compatible(&mut p, "B");
        assert_eq!(p["compatible_printers"], json!(["A", "B"]));
        assert!(!p.contains_key("compatible_printers_condition"));
        make_compatible(&mut p, "A");
        assert_eq!(p["compatible_printers"], json!(["A", "B"]));
        let mut p = obj(json!({"compatible_printers": "garbage"}));
        make_compatible(&mut p, "B");
        assert_eq!(p["compatible_printers"], json!(["B"]));
    }
}
