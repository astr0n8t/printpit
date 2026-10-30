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
