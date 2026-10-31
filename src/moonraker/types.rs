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

/// `GET /machine/system_info` (only the parts printpit shows; serial numbers are ignored).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SystemInfoEnvelope {
    pub system_info: SystemInfo,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SystemInfo {
    pub cpu_info: CpuInfo,
    pub sd_info: SdInfo,
    pub distribution: Distribution,
    pub available_services: Vec<String>,
    pub service_state: std::collections::BTreeMap<String, ServiceState>,
    pub virtualization: Virtualization,
    pub python: PythonInfo,
    pub network: std::collections::BTreeMap<String, NetInterface>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CpuInfo {
    pub cpu_count: Option<u32>,
    pub bits: String,
    pub processor: String,
    pub cpu_desc: String,
    pub hardware_desc: String,
    pub model: String,
    pub total_memory: Option<u64>,
    pub memory_units: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SdInfo {
    pub manufacturer: String,
    pub product_name: String,
    pub capacity: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Distribution {
    pub name: String,
    pub codename: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServiceState {
    pub active_state: String,
    pub sub_state: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Virtualization {
    pub virt_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PythonInfo {
    pub version_string: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetInterface {
    pub mac_address: String,
    pub ip_addresses: Vec<IpAddressInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IpAddressInfo {
    pub family: String,
    pub address: String,
    pub is_link_local: bool,
}

/// `GET /machine/proc_stats`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProcStats {
    pub moonraker_stats: Vec<MoonrakerSample>,
    pub throttled_state: ThrottledState,
    pub cpu_temp: Option<f64>,
    pub network: std::collections::BTreeMap<String, NetStats>,
    pub system_cpu_usage: std::collections::BTreeMap<String, f64>,
    pub system_memory: SystemMemory,
    pub system_uptime: Option<f64>,
    pub websocket_connections: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MoonrakerSample {
    pub cpu_usage: f64,
    pub memory: u64,
    pub mem_units: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThrottledState {
    pub bits: u64,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Bytes per second.
    pub bandwidth: f64,
}

/// In kB.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SystemMemory {
    pub total: u64,
    pub available: u64,
    pub used: u64,
}

/// `GET /machine/update/status`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdateStatus {
    pub busy: bool,
    pub version_info: std::collections::BTreeMap<String, serde_json::Value>,
}

/// One job from `server.history`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HistoryJob {
    pub job_id: String,
    /// Whether the G-code file is still on the printer.
    pub exists: bool,
    pub filename: String,
    /// `completed`, `cancelled`, `error`, `klippy_shutdown`, `in_progress`, …
    pub status: String,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub print_duration: f64,
    pub total_duration: f64,
    /// Millimetres.
    pub filament_used: f64,
    pub metadata: FileMetadata,
}

/// `GET /server/history/list`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HistoryList {
    pub count: u64,
    pub jobs: Vec<HistoryJob>,
}

/// `GET /server/history/totals`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HistoryTotals {
    pub job_totals: JobTotals,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JobTotals {
    pub total_jobs: u64,
    pub total_print_time: f64,
    pub total_filament_used: f64,
    pub longest_print: f64,
}

/// `GET /server/history/job`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HistoryJobEnvelope {
    pub job: HistoryJob,
}
