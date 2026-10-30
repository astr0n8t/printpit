//! HTTP client for one printer's Moonraker API.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use url::Url;

use super::MoonrakerError;
use super::net::{self, Egress};
use super::status::query_string;
use super::types::{
    Envelope, ErrorEnvelope, FileEntry, ObjectList, ObjectStatus, PrinterInfo, ProcStats, ServerInfo,
    SystemInfo, SystemInfoEnvelope, UpdateStatus, WebcamList,
};

pub type MoonrakerSocket = WebSocketStream<reqwest::Upgraded>;

/// Upper bound for ordinary JSON calls (uploads and streams set their own).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct MoonrakerClient {
    http: reqwest::Client,
    egress: Egress,
    base: String,
    api_key: Option<String>,
}

impl fmt::Debug for MoonrakerClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoonrakerClient")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl MoonrakerClient {
    pub fn new(egress: &Egress, base_url: &str, api_key: Option<String>) -> Result<Self, MoonrakerError> {
        let url = net::parse_base_url(base_url)
            .map_err(|_| MoonrakerError::Config("the printer's stored URL is invalid".into()))?;
        egress.check_url(&url)?;
        Ok(Self {
            http: egress.client().clone(),
            egress: egress.clone(),
            base: net::base_string(&url),
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    pub(crate) fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(key) = &self.api_key {
            req = req.header("X-Api-Key", key);
        }
        req
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, MoonrakerError> {
        let res = self
            .request(reqwest::Method::GET, path)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        decode(res).await
    }

    pub async fn server_info(&self) -> Result<ServerInfo, MoonrakerError> {
        self.get_json("/server/info").await
    }

    pub async fn printer_info(&self) -> Result<PrinterInfo, MoonrakerError> {
        self.get_json("/printer/info").await
    }

    pub async fn objects_list(&self) -> Result<Vec<String>, MoonrakerError> {
        Ok(self
            .get_json::<ObjectList>("/printer/objects/list")
            .await?
            .objects)
    }

    pub async fn objects_query(
        &self,
        sub: &Map<String, Value>,
    ) -> Result<Map<String, Value>, MoonrakerError> {
        let path = format!("/printer/objects/query?{}", query_string(sub));
        Ok(self.get_json::<ObjectStatus>(&path).await?.status)
    }

    pub async fn system_info(&self) -> Result<SystemInfo, MoonrakerError> {
        Ok(self
            .get_json::<SystemInfoEnvelope>("/machine/system_info")
            .await?
            .system_info)
    }

    pub async fn proc_stats(&self) -> Result<ProcStats, MoonrakerError> {
        self.get_json("/machine/proc_stats").await
    }

    /// Cached update status; `refresh=false` so viewing never triggers an update check.
    pub async fn update_status(&self) -> Result<UpdateStatus, MoonrakerError> {
        self.get_json("/machine/update/status?refresh=false").await
    }

    pub async fn webcams(&self) -> Result<WebcamList, MoonrakerError> {
        self.get_json("/server/webcams/list").await
    }

    pub async fn gcode_files(&self) -> Result<Vec<FileEntry>, MoonrakerError> {
        self.get_json("/server/files/list?root=gcodes").await
    }

    /// POSTs a printer action such as `/printer/print/pause` (Moonraker answers `"ok"`).
    pub async fn post_action(&self, path: &str, query: &[(&str, &str)]) -> Result<(), MoonrakerError> {
        let res = self
            .request(reqwest::Method::POST, path)
            .query(query)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        decode::<Value>(res).await.map(|_| ())
    }

    /// Resolves a webcam URL from Moonraker. Relative paths are served by the printer's web
    /// frontend (nginx on the default port), not by Moonraker itself, so Moonraker's usual
    /// port 7125 is dropped for them.
    pub fn resolve_url(&self, url: &str) -> Result<Url, MoonrakerError> {
        let bad = || MoonrakerError::Config("the webcam URL is invalid".into());
        let resolved = if url.starts_with("http://") || url.starts_with("https://") {
            Url::parse(url).map_err(|_| bad())?
        } else {
            let mut base = Url::parse(&self.base).map_err(|_| bad())?;
            if base.port() == Some(7125) {
                let _ = base.set_port(None);
            }
            base.set_path("/");
            base.join(url.trim_start_matches(['.'])).map_err(|_| bad())?
        };
        if !matches!(resolved.scheme(), "http" | "https") {
            return Err(bad());
        }
        self.egress.check_url(&resolved)?;
        Ok(resolved)
    }

    /// Fetches an arbitrary (webcam) URL through the egress client. The API key is not sent:
    /// it belongs to Moonraker, not to the camera service.
    pub async fn fetch(
        &self,
        url: Url,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, MoonrakerError> {
        self.egress.check_url(&url)?;
        let mut req = self.http.get(url);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let res = req.send().await.map_err(MoonrakerError::from_reqwest)?;
        match res.status() {
            s if s.is_success() => Ok(res),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(MoonrakerError::Unauthorized),
            s => Err(MoonrakerError::Http {
                status: s.as_u16(),
                message: s.canonical_reason().unwrap_or("error").into(),
            }),
        }
    }

    /// Opens Moonraker's WebSocket by upgrading a request on the egress client, so the socket
    /// gets the same allowlist, bind address and TLS as every other printer request.
    pub async fn websocket(&self) -> Result<MoonrakerSocket, MoonrakerError> {
        let key = generate_key();
        let send = self
            .request(reqwest::Method::GET, "/websocket")
            .header(CONNECTION, "Upgrade")
            .header(UPGRADE, "websocket")
            .header(SEC_WEBSOCKET_VERSION, "13")
            .header(SEC_WEBSOCKET_KEY, &key)
            .send();
        let res = tokio::time::timeout(REQUEST_TIMEOUT, send)
            .await
            .map_err(|_| MoonrakerError::Offline("timed out".into()))?
            .map_err(MoonrakerError::from_reqwest)?;
        match res.status() {
            StatusCode::SWITCHING_PROTOCOLS => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(MoonrakerError::Unauthorized),
            s => {
                return Err(MoonrakerError::Http {
                    status: s.as_u16(),
                    message: "WebSocket upgrade refused".into(),
                });
            }
        }
        let accept = res
            .headers()
            .get(SEC_WEBSOCKET_ACCEPT)
            .and_then(|v| v.to_str().ok());
        if accept != Some(derive_accept_key(key.as_bytes()).as_str()) {
            return Err(MoonrakerError::Decode("bad WebSocket handshake".into()));
        }
        let upgraded = res.upgrade().await.map_err(MoonrakerError::from_reqwest)?;
        let config = WebSocketConfig::default().max_message_size(Some(16 << 20));
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await)
    }
}

/// Maps a Moonraker response to `T` or a typed error.
pub(crate) async fn decode<T: DeserializeOwned>(res: reqwest::Response) -> Result<T, MoonrakerError> {
    let status = res.status();
    let body = res.bytes().await.map_err(MoonrakerError::from_reqwest)?;
    if status.is_success() {
        return serde_json::from_slice::<Envelope<T>>(&body)
            .map(|e| e.result)
            .map_err(|e| MoonrakerError::Decode(e.to_string()));
    }
    let message = serde_json::from_slice::<ErrorEnvelope>(&body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| status.canonical_reason().unwrap_or("error").to_string());
    Err(match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => MoonrakerError::Unauthorized,
        StatusCode::SERVICE_UNAVAILABLE => MoonrakerError::KlippyNotReady(message),
        _ => MoonrakerError::Http {
            status: status.as_u16(),
            message,
        },
    })
}
