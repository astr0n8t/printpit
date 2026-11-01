//! HTTP client for the slicer worker (`slicer-worker/`, API in PLAN §15.2).
//!
//! It has its own `reqwest::Client`, not the printer egress client: rule 4 is about printer
//! traffic, and the worker isn't a printer. No redirects, no proxy (the token goes only to the
//! configured worker), a short connect timeout. Dropping a [`SlicerClient::slice`] future drops
//! the request, and the worker then kills the slice.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, RETRY_AFTER};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{Kind, Profile, SlicerError, Storage};
use crate::config::SlicerConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Everything but a slice is quick.
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// Bodies read into memory (JSON). A big vendor's index is a few hundred KB.
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub orca_version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Info {
    pub worker_version: String,
    pub orca_version: Option<String>,
    pub busy: bool,
    pub timeout_secs: u64,
    pub max_model_mb: u64,
    pub vendors: Vec<VendorSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VendorSummary {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VendorIndex {
    pub name: String,
    #[serde(default)]
    pub machines: Vec<MachineEntry>,
    #[serde(default)]
    pub processes: Vec<CompatEntry>,
    #[serde(default)]
    pub filaments: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineEntry {
    pub name: String,
    #[serde(default)]
    pub printer_model: String,
    #[serde(default)]
    pub nozzle: String,
}

/// A process or filament, with what it says it fits.
#[derive(Debug, Clone, Deserialize)]
pub struct CompatEntry {
    pub name: String,
    #[serde(default)]
    pub layer_height: String,
    #[serde(default)]
    pub filament_type: String,
    #[serde(default)]
    pub compatible_printers: Vec<String>,
    #[serde(default)]
    pub compatible_printers_condition: String,
}

/// One user preset after the worker flattened it.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub kind: Kind,
    pub profile: Profile,
}

/// The CLI switches the worker takes (`options` part). Settings overrides are applied to the
/// process profile before it is sent, not here.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct WorkerOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrange: Option<bool>,
    pub orient: bool,
    pub plate: u32,
    pub project_settings: bool,
}

/// What a slice needs. The model is streamed from disk.
pub struct SliceRequest<'a> {
    pub machine: &'a Profile,
    /// `None` with `project_settings` (a 3MF's own settings are used).
    pub process: Option<&'a Profile>,
    pub filament: Option<&'a Profile>,
    pub options: &'a WorkerOptions,
    pub model: &'a Path,
}

/// Print statistics the worker read from the G-code footer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SliceStats {
    pub print_time_s: Option<i64>,
    pub filament_mm: Option<f64>,
    pub filament_g: Option<f64>,
    pub layers: Option<i64>,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct SlicerClient {
    http: reqwest::Client,
    base: url::Url,
    bearer: String,
    timeout: Duration,
}

impl std::fmt::Debug for SlicerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SlicerClient(<redacted>)")
    }
}

impl SlicerClient {
    pub fn new(config: &SlicerConfig) -> Result<Self, reqwest::Error> {
        // An https:// worker needs a TLS provider; the printer egress installs the same one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("printpit/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(Self {
            http,
            base: config.url.0.clone(),
            bearer: format!("Bearer {}", config.token.secret()),
            timeout: config.timeout,
        })
    }

    pub async fn health(&self) -> Result<Health, SlicerError> {
        self.get_json(&["v1", "health"]).await
    }

    pub async fn info(&self) -> Result<Info, SlicerError> {
        self.get_json(&["v1", "info"]).await
    }

    pub async fn vendor(&self, vendor: &str) -> Result<VendorIndex, SlicerError> {
        self.get_json(&["v1", "vendors", vendor]).await
    }

    /// One system profile, flattened.
    pub async fn profile(&self, kind: Kind, vendor: &str, name: &str) -> Result<Profile, SlicerError> {
        self.get_json(&["v1", "profiles", kind.as_str(), vendor, name])
            .await
    }

    /// Flattens user presets (exported from Orca, or a bundle's files). One result per preset,
    /// in order; a preset that can't be resolved is an `Err` with the worker's reason.
    pub async fn resolve(&self, presets: &[Profile]) -> Result<Vec<Result<Resolved, String>>, SlicerError> {
        #[derive(Deserialize)]
        struct Out {
            results: Vec<Value>,
        }
        let req = self
            .http
            .post(self.url(&["v1", "profiles", "resolve"]))
            .json(&serde_json::json!({ "presets": presets }));
        let out: Out = self.send_json(req).await?;
        if out.results.len() != presets.len() {
            return Err(SlicerError::Decode("wrong number of results".into()));
        }
        Ok(out
            .results
            .into_iter()
            .map(|r| {
                if let Some(e) = r.get("error").and_then(Value::as_str) {
                    return Err(e.to_string());
                }
                let name = r.get("name").and_then(Value::as_str).unwrap_or_default();
                let kind = r.get("kind").and_then(Value::as_str).and_then(Kind::parse);
                match (kind, r.get("profile").and_then(Value::as_object)) {
                    (Some(kind), Some(profile)) if !name.is_empty() => Ok(Resolved {
                        name: name.to_string(),
                        kind,
                        profile: profile.clone(),
                    }),
                    _ => Err("the slicer returned an incomplete profile".into()),
                }
            })
            .collect())
    }

    /// Slices `req.model` and writes the G-code to `storage.output_path(job)` (via a partial file
    /// that is only renamed into place once complete).
    pub async fn slice(
        &self,
        req: &SliceRequest<'_>,
        storage: &Storage,
        job: i64,
    ) -> Result<SliceStats, SlicerError> {
        let io = |what: &str, e: std::io::Error| SlicerError::Io(format!("{what}: {}", e.kind()));
        let file = tokio::fs::File::open(req.model)
            .await
            .map_err(|e| io("can't read the model", e))?;
        let len = file
            .metadata()
            .await
            .map_err(|e| io("can't read the model", e))?
            .len();
        let file_name = req
            .model
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("model.stl")
            .to_string();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let to_json = |p: &Profile| serde_json::to_string(p).unwrap_or_default();
        // Fields before the file: the worker reads them in order and wants the model last.
        let mut form = reqwest::multipart::Form::new().text("machine", to_json(req.machine));
        if let Some(p) = req.process {
            form = form.text("process", to_json(p));
        }
        if let Some(f) = req.filament {
            form = form.text("filament", to_json(f));
        }
        let options = serde_json::to_string(req.options).unwrap_or_else(|_| "{}".into());
        form = form.text("options", options).part(
            "model",
            reqwest::multipart::Part::stream_with_length(body, len)
                .file_name(file_name)
                .mime_str("application/octet-stream")
                .map_err(SlicerError::from_reqwest)?,
        );

        let res = match self
            .http
            .post(self.url(&["v1", "slice"]))
            .header(AUTHORIZATION, &self.bearer)
            .timeout(self.timeout)
            .multipart(form)
            .send()
            .await
        {
            Ok(res) => res,
            Err(e) if e.is_timeout() => return Err(SlicerError::Timeout),
            Err(e) => return Err(self.explain_send_failure(SlicerError::from_reqwest(e), len).await),
        };
        let res = Self::check(res).await?;
        let mut stats = stats_from(res.headers());

        let partial = storage.partial_output_path(job);
        let mut out = storage
            .create(&partial)
            .await
            .map_err(|e| io("can't store the G-code", e))?;
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(SlicerError::from_reqwest)?;
            stats.bytes += chunk.len() as u64;
            out.write_all(&chunk)
                .await
                .map_err(|e| io("can't store the G-code", e))?;
        }
        out.flush().await.map_err(|e| io("can't store the G-code", e))?;
        drop(out);
        if stats.bytes == 0 {
            return Err(SlicerError::Decode("the slicer returned no G-code".into()));
        }
        storage
            .commit_output(job)
            .await
            .map_err(|e| io("can't store the G-code", e))?;
        Ok(stats)
    }

    /// The worker turns a request down (busy, wrong token, too large) without reading the rest of
    /// the model, and closes the connection; we may then see only the failed write ("broken
    /// pipe"), not its answer. Ask it what it would have said, so the job gets the right status.
    async fn explain_send_failure(&self, err: SlicerError, model_len: u64) -> SlicerError {
        match self.info().await {
            Err(SlicerError::Unauthorized) => SlicerError::Unauthorized,
            Ok(info) if info.busy => SlicerError::Busy { retry_after: None },
            Ok(info) if model_len > info.max_model_mb.saturating_mul(1024 * 1024) => SlicerError::Failed {
                kind: "too_large".into(),
                message: format!(
                    "the model is larger than the slicer's limit of {} MB",
                    info.max_model_mb
                ),
                code: None,
                log_tail: Vec::new(),
            },
            _ => err,
        }
    }

    fn url(&self, segments: &[&str]) -> url::Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has a path")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn get_json<T: DeserializeOwned>(&self, segments: &[&str]) -> Result<T, SlicerError> {
        self.send_json(self.http.get(self.url(segments))).await
    }

    async fn send_json<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, SlicerError> {
        let res = req
            .header(AUTHORIZATION, &self.bearer)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(SlicerError::from_reqwest)?;
        let res = Self::check(res).await?;
        let body = read_capped(res, MAX_JSON_BYTES).await?;
        serde_json::from_slice(&body).map_err(|e| SlicerError::Decode(e.to_string()))
    }

    /// Passes a 2xx response through; turns anything else into a [`SlicerError`].
    async fn check(res: reqwest::Response) -> Result<reqwest::Response, SlicerError> {
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let retry_after = res
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let body = read_capped(res, MAX_ERROR_BYTES).await.unwrap_or_default();
        Err(SlicerError::from_response(status.as_u16(), retry_after, &body))
    }
}

async fn read_capped(res: reqwest::Response, max: usize) -> Result<Vec<u8>, SlicerError> {
    let mut buf = Vec::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(SlicerError::from_reqwest)?;
        if buf.len() + chunk.len() > max {
            return Err(SlicerError::Decode("response too large".into()));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn stats_from(h: &HeaderMap) -> SliceStats {
    fn get<T: std::str::FromStr>(h: &HeaderMap, name: &str) -> Option<T> {
        h.get(name)?.to_str().ok()?.trim().parse().ok()
    }
    SliceStats {
        print_time_s: get(h, "x-slice-print-time-s"),
        filament_mm: get::<f64>(h, "x-slice-filament-mm").filter(|v| v.is_finite()),
        filament_g: get::<f64>(h, "x-slice-filament-g").filter(|v| v.is_finite()),
        layers: get(h, "x-slice-layers"),
        bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SlicerToken, SlicerUrl};

    pub(crate) fn client(url: &str) -> SlicerClient {
        let config = SlicerConfig {
            url: SlicerUrl(url.parse().unwrap()),
            token: test_token(),
            timeout: Duration::from_secs(5),
            max_model_bytes: 1024,
            retention: Duration::from_secs(3600),
        };
        SlicerClient::new(&config).unwrap()
    }

    fn test_token() -> SlicerToken {
        // Built through the config parser so the type stays opaque elsewhere.
        let get = |k: &str| match k {
            "PRINTPIT_SECRET_KEY" => Some("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into()),
            "PRINTPIT_SLICER_URL" => Some("http://slicer:8090".into()),
            "PRINTPIT_SLICER_TOKEN" => Some("slicer-token-0123456789abcdef0123".into()),
            _ => None,
        };
        crate::config::Config::from_lookup(get)
            .unwrap()
            .slicer
            .unwrap()
            .token
    }

    #[test]
    fn builds_encoded_urls() {
        let c = client("http://slicer:8090/");
        assert_eq!(c.url(&["v1", "health"]).as_str(), "http://slicer:8090/v1/health");
        assert_eq!(
            c.url(&["v1", "profiles", "process", "Voron", "0.20mm Standard @Voron"])
                .as_str(),
            "http://slicer:8090/v1/profiles/process/Voron/0.20mm%20Standard%20@Voron"
        );
        // A name can't climb out of its segment.
        assert_eq!(
            c.url(&["v1", "vendors", "../../admin?x=1#y"]).as_str(),
            "http://slicer:8090/v1/vendors/..%2F..%2Fadmin%3Fx=1%23y"
        );
        let c = client("https://tools.lan/slicer/");
        assert_eq!(
            c.url(&["v1", "info"]).as_str(),
            "https://tools.lan/slicer/v1/info"
        );
    }

    #[test]
    fn debug_hides_everything() {
        let c = client("http://slicer:8090/");
        let d = format!("{c:?}");
        assert!(!d.contains("slicer:8090") && !d.contains("0123456789"), "{d}");
    }

    #[test]
    fn stats_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-slice-print-time-s", "595".parse().unwrap());
        h.insert("x-slice-filament-mm", "1304.61".parse().unwrap());
        h.insert("x-slice-filament-g", "NaN".parse().unwrap());
        h.insert("x-slice-layers", "100".parse().unwrap());
        let s = stats_from(&h);
        assert_eq!(s.print_time_s, Some(595));
        assert_eq!(s.filament_mm, Some(1304.61));
        assert_eq!(s.filament_g, None);
        assert_eq!(s.layers, Some(100));
    }

    #[tokio::test]
    async fn offline_errors_name_no_address() {
        // A port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = client(&format!("http://127.0.0.1:{port}/"))
            .health()
            .await
            .unwrap_err();
        assert_eq!(err.label(), "OFFLINE");
        let text = format!("{err} {}", err.detail());
        assert!(
            !text.contains("127.0.0.1") && !text.contains(&port.to_string()),
            "{text}"
        );
    }
}
