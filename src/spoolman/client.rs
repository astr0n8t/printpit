//! Spoolman's REST API (`/api/v1`): list spools, use filament from one. Its own HTTP client,
//! never the printer egress: Spoolman isn't a printer. The base URL and token never appear in
//! errors, pages or logs.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::config::SpoolmanConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MESSAGE: usize = 300;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpoolmanError {
    #[error("offline: {0}")]
    Offline(String),
    /// 401/403, usually from a proxy in front of Spoolman.
    #[error("Spoolman (or the proxy in front of it) refused printpit's token")]
    Unauthorized,
    #[error("spool #{0} isn't in Spoolman")]
    NoSuchSpool(i64),
    #[error("Spoolman returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unexpected response from Spoolman: {0}")]
    Decode(String),
}

impl SpoolmanError {
    /// Short state label for a TUI badge.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Offline(_) => "OFFLINE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::NoSuchSpool(_) => "NO SUCH SPOOL",
            Self::Http { .. } => "ERROR",
            Self::Decode(_) => "BAD REPLY",
        }
    }

    /// The safe-to-show explanation.
    pub fn detail(&self) -> String {
        match self {
            Self::Offline(cause) => format!("Spoolman isn't reachable: {cause}"),
            e => e.to_string(),
        }
    }

    /// Worth trying again later (Spoolman down or overloaded), as opposed to a request it refused.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Offline(_) => true,
            Self::Http { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }

    fn from_reqwest(err: reqwest::Error) -> Self {
        let err = err.without_url();
        if err.is_timeout() {
            return Self::Offline("no answer in time".into());
        }
        let mut root: &dyn std::error::Error = &err;
        while let Some(next) = root.source() {
            root = next;
        }
        if err.is_decode() || err.is_body() {
            Self::Decode(root.to_string())
        } else {
            Self::Offline(root.to_string())
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Vendor {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Filament {
    pub name: Option<String>,
    pub vendor: Option<Vendor>,
    pub material: Option<String>,
    pub color_hex: Option<String>,
}

/// A spool, with only what printpit shows.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Spool {
    pub id: i64,
    pub filament: Filament,
    pub remaining_weight: Option<f64>,
    pub remaining_length: Option<f64>,
    pub location: Option<String>,
    pub archived: bool,
}

impl Spool {
    /// `#12 Polymaker PolyLite PLA`, adding the material when the name doesn't mention it.
    pub fn label(&self) -> String {
        let f = &self.filament;
        let mut words = vec![format!("#{}", self.id)];
        if let Some(v) = f.vendor.as_ref().map(|v| v.name.trim()).filter(|v| !v.is_empty()) {
            words.push(v.to_string());
        }
        let name = f.name.as_deref().map(str::trim).unwrap_or_default();
        if !name.is_empty() {
            words.push(name.to_string());
        }
        if let Some(m) = f.material.as_deref().map(str::trim).filter(|m| !m.is_empty())
            && !name.to_lowercase().contains(&m.to_lowercase())
        {
            words.push(m.to_string());
        }
        words.join(" ")
    }

    /// `734 g left · shelf 2`
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(w) = self.remaining_weight.filter(|w| w.is_finite()) {
            parts.push(format!("{w:.0} g left"));
        } else if let Some(l) = self.remaining_length.filter(|l| l.is_finite()) {
            parts.push(format!("{:.1} m left", l / 1000.0));
        }
        if let Some(loc) = self.location.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            parts.push(loc.to_string());
        }
        if self.archived {
            parts.push("archived".into());
        }
        parts.join(" · ")
    }
}

/// How much filament to take off a spool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Amount {
    /// Millimetres; Spoolman turns it into grams from the filament's diameter and density.
    Length(f64),
    Grams(f64),
}

impl Amount {
    pub fn describe(self) -> String {
        match self {
            Amount::Length(mm) => format!("{:.2} m", mm / 1000.0),
            Amount::Grams(g) => format!("{g:.1} g"),
        }
    }
}

#[derive(Clone)]
pub struct SpoolmanClient {
    http: reqwest::Client,
    base: url::Url,
    bearer: Option<String>,
}

impl SpoolmanClient {
    pub fn new(config: &SpoolmanConfig) -> Result<Self, reqwest::Error> {
        // An https:// Spoolman needs a TLS provider; the printer egress installs the same one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(format!("printpit/{}", crate::VERSION))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(Self {
            http,
            base: config.url.0.clone(),
            bearer: config.token.as_ref().map(|t| format!("Bearer {}", t.secret())),
        })
    }

    fn url(&self, path: &str) -> url::Url {
        // `base` always ends in `/`, so a relative join keeps any path prefix.
        self.base
            .join(path)
            .expect("a relative API path joins onto the base URL")
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer {
            Some(b) => req.header(reqwest::header::AUTHORIZATION, b),
            None => req,
        }
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        spool: Option<i64>,
    ) -> Result<reqwest::Response, SpoolmanError> {
        let res = self.auth(req).send().await.map_err(SpoolmanError::from_reqwest)?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        Err(match (status.as_u16(), spool) {
            (401 | 403, _) => SpoolmanError::Unauthorized,
            (404, Some(id)) => SpoolmanError::NoSuchSpool(id),
            (code, _) => {
                // Spoolman's errors are `{"message": "..."}`; anything else is summarised.
                let text = res.text().await.unwrap_or_default();
                let message = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
                    .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
                SpoolmanError::Http {
                    status: code,
                    message: message.chars().take(MAX_MESSAGE).collect(),
                }
            }
        })
    }

    /// Spoolman's version, as a connection test.
    pub async fn info(&self) -> Result<String, SpoolmanError> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Info {
            version: String,
        }
        let res = self.send(self.http.get(self.url("api/v1/info")), None).await?;
        let info: Info = res.json().await.map_err(SpoolmanError::from_reqwest)?;
        Ok(info.version)
    }

    /// Every spool that isn't archived.
    pub async fn spools(&self) -> Result<Vec<Spool>, SpoolmanError> {
        let req = self
            .http
            .get(self.url("api/v1/spool"))
            .query(&[("allow_archived", "false")]);
        let res = self.send(req, None).await?;
        let mut spools: Vec<Spool> = res.json().await.map_err(SpoolmanError::from_reqwest)?;
        spools.sort_by_key(|s| s.id);
        Ok(spools)
    }

    /// `PUT /api/v1/spool/{id}/use`.
    pub async fn use_filament(&self, spool_id: i64, amount: Amount) -> Result<(), SpoolmanError> {
        let body = match amount {
            Amount::Length(mm) => json!({ "use_length": mm }),
            Amount::Grams(g) => json!({ "use_weight": g }),
        };
        let req = self
            .http
            .put(self.url(&format!("api/v1/spool/{spool_id}/use")))
            .json(&body);
        self.send(req, Some(spool_id)).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spool(name: Option<&str>, material: Option<&str>) -> Spool {
        Spool {
            id: 12,
            filament: Filament {
                name: name.map(str::to_string),
                vendor: Some(Vendor {
                    name: "Polymaker".into(),
                }),
                material: material.map(str::to_string),
                color_hex: None,
            },
            remaining_weight: Some(733.6),
            location: Some("shelf 2".into()),
            ..Spool::default()
        }
    }

    #[test]
    fn labels() {
        assert_eq!(
            spool(Some("PolyLite Black"), Some("PLA")).label(),
            "#12 Polymaker PolyLite Black PLA"
        );
        assert_eq!(
            spool(Some("PolyLite PLA"), Some("PLA")).label(),
            "#12 Polymaker PolyLite PLA"
        );
        assert_eq!(spool(None, None).label(), "#12 Polymaker");
        assert_eq!(spool(None, None).summary(), "734 g left · shelf 2");
    }

    #[test]
    fn amounts() {
        assert_eq!(Amount::Length(1520.5).describe(), "1.52 m");
        assert_eq!(Amount::Grams(4.56).describe(), "4.6 g");
    }
}
