//! HTTP client for one printer's Moonraker API.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use serde::de::DeserializeOwned;

use super::MoonrakerError;
use super::net::{self, Egress};
use super::types::{Envelope, ErrorEnvelope, PrinterInfo, ServerInfo};

/// Upper bound for ordinary JSON calls (uploads and streams set their own).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct MoonrakerClient {
    http: reqwest::Client,
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
            base: net::base_string(&url),
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(key) = &self.api_key {
            req = req.header("X-Api-Key", key);
        }
        req
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, MoonrakerError> {
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
}

/// Maps a Moonraker response to `T` or a typed error.
async fn decode<T: DeserializeOwned>(res: reqwest::Response) -> Result<T, MoonrakerError> {
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
