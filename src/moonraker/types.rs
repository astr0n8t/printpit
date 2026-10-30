//! Serde models for the Moonraker responses printpit uses. Every field defaults, so a newer or
//! older Moonraker with extra/missing fields still parses.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct Envelope<T> {
    pub result: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorEnvelope {
    pub error: RpcError,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcError {
    #[serde(default)]
    pub message: String,
}

/// `GET /server/info`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub klippy_connected: bool,
    /// `ready`, `startup`, `shutdown`, `error` or `disconnected`.
    pub klippy_state: String,
    pub moonraker_version: String,
    pub api_version_string: String,
    pub warnings: Vec<String>,
    pub failed_components: Vec<String>,
}

/// `GET /printer/info`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PrinterInfo {
    pub state: String,
    pub state_message: String,
    pub hostname: String,
    pub software_version: String,
}

/// `GET /printer/objects/list`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ObjectList {
    pub objects: Vec<String>,
}

/// `GET /printer/objects/query` and the `printer.objects.subscribe` result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ObjectStatus {
    pub status: serde_json::Map<String, serde_json::Value>,
}

/// One entry of `GET /server/webcams/list`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebcamInfo {
    pub name: String,
    pub enabled: Option<bool>,
    pub service: String,
    pub snapshot_url: String,
    pub stream_url: String,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    pub rotation: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WebcamList {
    pub webcams: Vec<WebcamInfo>,
}

/// One entry of `GET /server/files/list`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileEntry {
    pub path: String,
    pub modified: f64,
    pub size: u64,
}

/// `GET /server/files/directory?extended=true`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DirListing {
    pub dirs: Vec<DirEntry>,
    pub files: Vec<DirFile>,
    pub disk_usage: Option<DiskUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DirEntry {
    pub dirname: String,
    pub modified: f64,
    pub size: u64,
}

/// A file in a directory listing; slicer metadata is present when Moonraker has parsed it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DirFile {
    pub filename: String,
    pub modified: f64,
    pub size: u64,
    pub estimated_time: Option<f64>,
    pub filament_total: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct DiskUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// `GET /server/files/metadata`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: f64,
    pub slicer: Option<String>,
    pub slicer_version: Option<String>,
    pub estimated_time: Option<f64>,
    pub filament_total: Option<f64>,
    pub filament_weight_total: Option<f64>,
    pub layer_height: Option<f64>,
    pub first_layer_height: Option<f64>,
    pub object_height: Option<f64>,
    pub nozzle_diameter: Option<f64>,
    pub first_layer_extr_temp: Option<f64>,
    pub first_layer_bed_temp: Option<f64>,
    pub thumbnails: Vec<Thumbnail>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub size: u64,
    /// Relative to the G-code file's directory.
    pub relative_path: String,
}

/// `POST /server/files/upload`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UploadResult {
    pub print_started: bool,
}
