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
