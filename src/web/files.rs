//! The Files tab: browse, inspect, upload, move/copy, delete and print G-code files.
//!
//! `/ui/files` renders instantly (selector + an empty listing box); the listing itself is
//! fetched from Moonraker by `/p/{id}/files` when the box loads, so nothing touches a
//! printer until the tab is opened.

use askama::Template;
use axum::Form;
use axum::body::{Body, Bytes};
use axum::extract::rejection::FormRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::{ClientIp, CurrentUser, Operator, RequireRole, Role};
use crate::db::audit;
use crate::db::printers::Printer;
use crate::error::AppError;
use crate::moonraker::files::{clean_name, clean_path, join, parent};
use crate::moonraker::types::{DirListing, FileMetadata};
use crate::moonraker::{MoonrakerClient, MoonrakerError};
use crate::state::AppState;
use crate::web::layout::{HxRequest, Tab, page};
use crate::web::select::{self, enabled_printer};
use crate::web::widgets;

const THUMB_MAX_BYTES: usize = 2 << 20;

// ── Views ────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tabs/files.html")]
struct FilesTab {
    select: Option<String>,
    listing_url: String,
}

struct Crumb {
    name: String,
    path: String,
}

struct Row {
    name: String,
    path: String,
    is_dir: bool,
    size: String,
    modified: String,
    time: String,
    filament: String,
}

#[derive(Template)]
#[template(path = "partials/file_list.html")]
struct FileList {
    id: i64,
    path: String,
    crumbs: Vec<Crumb>,
    rows: Vec<Row>,
    sort: &'static str,
    desc: bool,
    disk: Option<String>,
    can_edit: bool,
    notice: Option<String>,
    error: Option<String>,
    max_upload: String,
}

impl FileList {
    /// Query string for a column header: clicking the active column flips the order.
    fn sort_link(&self, column: &str) -> String {
        let desc = if column == self.sort {
            !self.desc
        } else {
            column != "name"
        };
        format!(
            "/p/{}/files?path={}&sort={column}&order={}",
            self.id,
            urlencode(&self.path),
            if desc { "desc" } else { "asc" }
        )
    }

    fn arrow(&self, column: &str) -> &'static str {
        match (column == self.sort, self.desc) {
            (false, _) => "",
            (true, true) => " ▼",
            (true, false) => " ▲",
        }
    }

    fn dir_url(&self, path: &str) -> String {
        format!("/p/{}/files?path={}", self.id, urlencode(path))
    }

    fn push_url(&self, path: &str) -> String {
        format!("/ui/files?printer={}&path={}", self.id, urlencode(path))
    }

    fn meta_url(&self, row: &Row) -> String {
        format!(
            "/p/{}/files/meta?path={}&kind={}",
            self.id,
            urlencode(&row.path),
            if row.is_dir { "dir" } else { "file" }
        )
    }
}

#[derive(Template)]
#[template(path = "partials/file_meta.html")]
struct FileMeta {
    id: i64,
    path: String,
    name: String,
    is_dir: bool,
    can_edit: bool,
    thumbnail: Option<String>,
    facts: Vec<(&'static str, String)>,
    error: Option<String>,
}

impl FileMeta {
    fn download_url(&self) -> String {
        format!("/p/{}/files/download?path={}", self.id, urlencode(&self.path))
    }

    fn delete_url(&self) -> String {
        let kind = if self.is_dir { "dir" } else { "file" };
        format!("/p/{}/files?path={}&kind={kind}", self.id, urlencode(&self.path))
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ── Tab and listing ──────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct TabQuery {
    printer: Option<i64>,
    #[serde(default)]
    path: String,
}

pub async fn tab(
    State(state): State<AppState>,
    hx: HxRequest,
    user: CurrentUser,
    Query(q): Query<TabQuery>,
) -> Result<Response, AppError> {
    let selection = select::select(&state, user.id, q.printer, "/ui/files").await?;
    let path = clean_path(&q.path).unwrap_or_default();
    let html = match selection {
        None => FilesTab {
            select: None,
            listing_url: String::new(),
        },
        Some(sel) => FilesTab {
            listing_url: format!("/p/{}/files?path={}", sel.printer.id, urlencode(&path)),
            select: Some(sel.select.render()?),
        },
    }
    .render()?;
    page(hx, &user, Tab::Files, html)
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    path: String,
    sort: Option<String>,
    order: Option<String>,
}

fn rows(listing: &DirListing, dir: &str, sort: &str, desc: bool) -> Vec<Row> {
    let mut dirs: Vec<Row> = listing
        .dirs
        .iter()
        .filter(|d| !d.dirname.starts_with('.'))
        .map(|d| Row {
            name: d.dirname.clone(),
            path: join(dir, &d.dirname),
            is_dir: true,
            size: String::new(),
            modified: widgets::age(d.modified),
            time: String::new(),
            filament: String::new(),
        })
        .collect();
    let mut files: Vec<(&crate::moonraker::types::DirFile, Row)> = listing
        .files
        .iter()
        .filter(|f| !f.filename.starts_with('.'))
        .map(|f| {
            (
                f,
                Row {
                    name: f.filename.clone(),
                    path: join(dir, &f.filename),
                    is_dir: false,
                    size: widgets::human_size(f.size),
                    modified: widgets::age(f.modified),
                    time: f.estimated_time.map(widgets::duration).unwrap_or_default(),
                    filament: f
                        .filament_total
                        .map(|mm| format!("{:.1} m", mm / 1000.0))
                        .unwrap_or_default(),
                },
            )
        })
        .collect();
    files.sort_by(|(a, _), (b, _)| {
        let ord = match sort {
            "size" => a.size.cmp(&b.size),
            "modified" => a.modified.total_cmp(&b.modified),
            "time" => a
                .estimated_time
                .unwrap_or(0.0)
                .total_cmp(&b.estimated_time.unwrap_or(0.0)),
            _ => a.filename.to_lowercase().cmp(&b.filename.to_lowercase()),
        };
        if desc { ord.reverse() } else { ord }
    });
    dirs.sort_by_key(|d| d.name.to_lowercase());
    dirs.extend(files.into_iter().map(|(_, r)| r));
    dirs
}

fn crumbs(path: &str) -> Vec<Crumb> {
    let mut out = vec![Crumb {
        name: "gcodes".into(),
        path: String::new(),
    }];
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc = join(&acc, seg);
        out.push(Crumb {
            name: seg.to_string(),
            path: acc.clone(),
        });
    }
    out
}

async fn render_list(
    state: &AppState,
    user: &CurrentUser,
    p: &Printer,
    path: &str,
    sort: Option<&str>,
    order: Option<&str>,
    notice: Option<String>,
) -> Result<Response, AppError> {
    let sort = match sort {
        Some("size") => "size",
        Some("modified") => "modified",
        Some("time") => "time",
        _ => "name",
    };
    let desc = order == Some("desc");
    let listing = match state.printer_client(p) {
        Ok(client) => state.dirs.get(p.id, &client, path).await,
        Err(e) => Err(e),
    };
    let (rows, disk, error) = match listing {
        Ok(l) => (
            rows(&l, path, sort, desc),
            l.disk_usage.map(|d| {
                format!(
                    "{} free of {}",
                    widgets::human_size(d.free),
                    widgets::human_size(d.total)
                )
            }),
            None,
        ),
        Err(e) => (Vec::new(), None, Some(format!("{}: {}", e.label(), e.detail()))),
    };
    let html = FileList {
        id: p.id,
        path: path.to_string(),
        crumbs: crumbs(path),
        rows,
        sort,
        desc,
        disk,
        can_edit: user.has(Role::Operator),
        notice,
        error,
        max_upload: widgets::human_size(state.config.max_upload_bytes),
    }
    .render()?;
    Ok(Html(html).into_response())
}

fn bad_path() -> AppError {
    AppError::BadRequest("That path isn't valid.".into())
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let path = clean_path(&q.path).ok_or_else(bad_path)?;
    render_list(
        &state,
        &user,
        &p,
        &path,
        q.sort.as_deref(),
        q.order.as_deref(),
        None,
    )
    .await
}

// ── Details, thumbnails, downloads ───────────────────────────────────

#[derive(Deserialize)]
pub struct MetaQuery {
    #[serde(default)]
    path: String,
    kind: Option<String>,
}

fn facts(m: &FileMetadata) -> Vec<(&'static str, String)> {
    let mut f = vec![
        ("size", widgets::human_size(m.size)),
        ("modified", widgets::age(m.modified)),
    ];
    let mut push = |label, v: Option<String>| {
        if let Some(v) = v {
            f.push((label, v));
        }
    };
    push("print time", m.estimated_time.map(widgets::duration));
    push(
        "filament",
        m.filament_total.map(|mm| match m.filament_weight_total {
            Some(g) => format!("{:.1} m · {g:.0} g", mm / 1000.0),
            None => format!("{:.1} m", mm / 1000.0),
        }),
    );
    push("layer", m.layer_height.map(|h| format!("{h:.2} mm")));
    push("height", m.object_height.map(|h| format!("{h:.1} mm")));
    push("nozzle", m.nozzle_diameter.map(|d| format!("{d:.2} mm")));
    push(
        "temps",
        match (m.first_layer_extr_temp, m.first_layer_bed_temp) {
            (Some(e), Some(b)) => Some(format!("{e:.0} / {b:.0} °C")),
            _ => None,
        },
    );
    push(
        "slicer",
        m.slicer.as_ref().map(|s| {
            format!("{s} {}", m.slicer_version.as_deref().unwrap_or(""))
                .trim()
                .to_string()
        }),
    );
    f
}

pub async fn meta(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<MetaQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let path = clean_path(&q.path)
        .filter(|p| !p.is_empty())
        .ok_or_else(bad_path)?;
    let is_dir = q.kind.as_deref() == Some("dir");
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let (thumbnail, facts, error) = if is_dir {
        (None, Vec::new(), None)
    } else {
        match state.printer_client(&p) {
            Ok(client) => match client.metadata(&path).await {
                Ok(m) => {
                    // Largest thumbnail up to 400 px wide; its path is relative to the file's dir.
                    let thumb = m
                        .thumbnails
                        .iter()
                        .filter(|t| t.width <= 400 && !t.relative_path.is_empty())
                        .max_by_key(|t| t.width)
                        .and_then(|t| clean_path(&join(parent(&path), &t.relative_path)))
                        .map(|t| format!("/p/{id}/files/thumb?path={}", urlencode(&t)));
                    (thumb, facts(&m), None)
                }
                Err(e) => (None, Vec::new(), Some(format!("{}: {}", e.label(), e.detail()))),
            },
            Err(e) => (None, Vec::new(), Some(e.detail())),
        }
    };
    let html = FileMeta {
        id,
        path,
        name,
        is_dir,
        can_edit: user.has(Role::Operator),
        thumbnail,
        facts,
        error,
    }
    .render()?;
    Ok(Html(html).into_response())
}

fn upstream_error(e: &MoonrakerError) -> Response {
    let status = match e {
        MoonrakerError::Http { status: 404, .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, e.detail()).into_response()
}

async fn client_for(state: &AppState, id: i64) -> Result<MoonrakerClient, Box<Response>> {
    let p = enabled_printer(state, id)
        .await
        .map_err(|e| Box::new(e.into_response()))?;
    state.printer_client(&p).map_err(|e| Box::new(upstream_error(&e)))
}

#[derive(Deserialize)]
pub struct PathQuery {
    #[serde(default)]
    path: String,
}

/// Proxies a slicer thumbnail (PNG/JPEG only).
pub async fn thumb(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<PathQuery>,
) -> Response {
    let Some(path) = clean_path(&q.path).filter(|p| {
        let lower = p.to_lowercase();
        lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let client = match client_for(&state, id).await {
        Ok(c) => c,
        Err(res) => return *res,
    };
    let res = match client.download("gcodes", &path).await {
        Ok(r) => r,
        Err(e) => return upstream_error(&e),
    };
    if res.content_length().is_some_and(|l| l as usize > THUMB_MAX_BYTES) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let Ok(bytes) = res.bytes().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if bytes.len() > THUMB_MAX_BYTES {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mime = if path.to_lowercase().ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        bytes,
    )
        .into_response()
}

/// Streams a file to the browser as an attachment.
pub async fn download(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<PathQuery>,
) -> Response {
    let Some(path) = clean_path(&q.path).filter(|p| !p.is_empty()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let client = match client_for(&state, id).await {
        Ok(c) => c,
        Err(res) => return *res,
    };
    let res = match client.download("gcodes", &path).await {
        Ok(r) => r,
        Err(e) => return upstream_error(&e),
    };
    attachment(res, &path)
}

/// Wraps an upstream file response as a streamed download named after `path`.
pub(crate) fn attachment(res: reqwest::Response, path: &str) -> Response {
    let length = res.content_length();
    let name = path.rsplit('/').next().unwrap_or("download");
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        crate::moonraker::files::encode_path(name)
    );
    let mut out = Response::new(Body::from_stream(res.bytes_stream()));
    let h = out.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = length {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

// ── Changes (operator) ───────────────────────────────────────────────

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    p: &Printer,
    action: &'static str,
    detail: String,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: Some(p.id),
            action,
            detail: Some(detail),
            ip: ip.to_string_opt(),
        },
    )
    .await;
}

/// Re-renders the listing of `dir` after a change, with a notice or the error.
async fn after_change(
    state: &AppState,
    user: &CurrentUser,
    p: &Printer,
    dir: &str,
    result: Result<String, MoonrakerError>,
) -> Result<Response, AppError> {
    state.dirs.invalidate(p.id);
    let notice = match result {
        Ok(msg) => Some(format!("✓ {msg}")),
        Err(e) => Some(format!("! {}: {}", e.label(), e.detail())),
    };
    render_list(state, user, p, dir, None, None, notice).await
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    path: String,
    kind: Option<String>,
}

pub async fn delete(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let path = clean_path(&q.path)
        .filter(|p| !p.is_empty())
        .ok_or_else(bad_path)?;
    let is_dir = q.kind.as_deref() == Some("dir");
    let result = match state.printer_client(&p) {
        Ok(client) if is_dir => client.delete_dir(&path).await,
        Ok(client) => client.delete_file(&path).await,
        Err(e) => Err(e),
    };
    if result.is_ok() {
        let action = if is_dir { "dir_delete" } else { "file_delete" };
        record(&state, ip, &user, &p, action, format!("path={path}")).await;
    }
    let result = result.map(|()| format!("Deleted {path}."));
    after_change(&state, &user, &p, parent(&path), result).await
}

#[derive(Deserialize)]
pub struct MkdirForm {
    #[serde(default)]
    dir: String,
    #[serde(default)]
    name: String,
}

pub async fn mkdir(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    form: Result<Form<MkdirForm>, FormRejection>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let dir = clean_path(&f.dir).ok_or_else(bad_path)?;
    let name = clean_name(&f.name).ok_or_else(|| AppError::BadRequest("Enter a folder name.".into()))?;
    let path = join(&dir, &name);
    let result = match state.printer_client(&p) {
        Ok(client) => client.make_dir(&path).await,
        Err(e) => Err(e),
    };
    if result.is_ok() {
        record(&state, ip, &user, &p, "dir_create", format!("path={path}")).await;
    }
    after_change(
        &state,
        &user,
        &p,
        &dir,
        result.map(|()| format!("Created {name}/.")),
    )
    .await
}

#[derive(Deserialize)]
pub struct MoveForm {
    #[serde(default)]
    source: String,
    #[serde(default)]
    dest: String,
    copy: Option<String>,
}

pub async fn move_or_copy(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    form: Result<Form<MoveForm>, FormRejection>,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let source = clean_path(&f.source)
        .filter(|p| !p.is_empty())
        .ok_or_else(bad_path)?;
    let dest = clean_path(&f.dest)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AppError::BadRequest("Enter a destination path, like parts/new-name.gcode.".into()))?;
    let copy = f.copy.is_some();
    let result = match state.printer_client(&p) {
        Ok(client) => client.move_or_copy(&source, &dest, copy).await,
        Err(e) => Err(e),
    };
    if result.is_ok() {
        let action = if copy { "file_copy" } else { "file_move" };
        record(
            &state,
            ip,
            &user,
            &p,
            action,
            format!("source={source} dest={dest}"),
        )
        .await;
    }
    let verb = if copy { "Copied" } else { "Moved" };
    let result = result.map(|()| format!("{verb} {source} → {dest}."));
    after_change(&state, &user, &p, parent(&dest), result).await
}

/// Streams a browser upload straight through to Moonraker. Form fields must come before the
/// file (the form puts the file input last).
pub async fn upload(
    State(state): State<AppState>,
    ip: ClientIp,
    user: RequireRole<Operator>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    let p = enabled_printer(&state, id).await?;
    let client = state.printer_client(&p);
    let max = state.config.max_upload_bytes;
    let mut dir = String::new();
    let mut print = false;
    let bad = |m: &str| AppError::BadRequest(m.to_string());

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| bad("The upload was interrupted."))?
    {
        match field.name() {
            Some("dir") => {
                let text = field.text().await.map_err(|_| bad("Malformed upload."))?;
                dir = clean_path(&text).ok_or_else(bad_path)?;
            }
            Some("print") => print = true,
            Some("file") => {
                let filename = field
                    .file_name()
                    .and_then(clean_name)
                    .ok_or_else(|| bad("Choose a G-code file to upload."))?;
                let client = match &client {
                    Ok(c) => c.clone(),
                    Err(e) => return after_change(&state, &user, &p, &dir, Err(e.clone())).await,
                };
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
                let body = reqwest::Body::wrap_stream(ReceiverStream::new(rx));
                let pump = async move {
                    let mut total: u64 = 0;
                    loop {
                        match field.chunk().await {
                            Ok(Some(chunk)) => {
                                total += chunk.len() as u64;
                                if total > max {
                                    let _ = tx.send(Err(std::io::Error::other("too large"))).await;
                                    return Err("The file is larger than PRINTPIT_MAX_UPLOAD_MB allows.");
                                }
                                if tx.send(Ok(chunk)).await.is_err() {
                                    return Err("Moonraker stopped reading the upload.");
                                }
                            }
                            Ok(None) => return Ok(total),
                            Err(_) => {
                                let _ = tx.send(Err(std::io::Error::other("aborted"))).await;
                                return Err("The upload was interrupted or is too large.");
                            }
                        }
                    }
                };
                let (sent, pumped) = tokio::join!(client.upload(&dir, &filename, print, body), pump);
                let path = join(&dir, &filename);
                let result = match (sent, pumped) {
                    (_, Err(msg)) => Err(MoonrakerError::Config(msg.into())),
                    (Err(e), _) => Err(e),
                    (Ok(r), Ok(bytes)) => {
                        record(
                            &state,
                            ip,
                            &user,
                            &p,
                            "file_upload",
                            format!("path={path} bytes={bytes} print={}", r.print_started),
                        )
                        .await;
                        Ok(if r.print_started {
                            format!("Uploaded {filename} and started printing.")
                        } else {
                            format!("Uploaded {filename}.")
                        })
                    }
                };
                return after_change(&state, &user, &p, &dir, result).await;
            }
            _ => {
                // Drain unknown fields.
                while let Ok(Some(_)) = field.chunk().await {}
            }
        }
    }
    Err(bad("Choose a G-code file to upload."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moonraker::types::{DirEntry, DirFile};

    fn listing() -> DirListing {
        DirListing {
            dirs: vec![
                DirEntry {
                    dirname: "parts".into(),
                    ..Default::default()
                },
                DirEntry {
                    dirname: ".thumbs".into(),
                    ..Default::default()
                },
            ],
            files: vec![
                DirFile {
                    filename: "b.gcode".into(),
                    size: 10,
                    modified: 2.0,
                    estimated_time: Some(60.0),
                    filament_total: Some(1500.0),
                },
                DirFile {
                    filename: "A.gcode".into(),
                    size: 20,
                    modified: 1.0,
                    ..Default::default()
                },
            ],
            disk_usage: None,
        }
    }

    #[test]
    fn rows_sort_dirs_first_and_hide_dotfiles() {
        let names = |r: Vec<Row>| r.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(
            names(rows(&listing(), "", "name", false)),
            ["parts", "A.gcode", "b.gcode"]
        );
        assert_eq!(
            names(rows(&listing(), "", "size", true)),
            ["parts", "A.gcode", "b.gcode"]
        );
        assert_eq!(
            names(rows(&listing(), "", "modified", true)),
            ["parts", "b.gcode", "A.gcode"]
        );
        let r = rows(&listing(), "sub", "name", false);
        assert_eq!(r[2].path, "sub/b.gcode");
        assert_eq!(r[2].filament, "1.5 m");
        assert_eq!(r[2].time, "01:00");
    }

    #[test]
    fn breadcrumbs() {
        let c = crumbs("a/b");
        let paths: Vec<&str> = c.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["", "a", "a/b"]);
        assert_eq!(c[0].name, "gcodes");
    }
}
