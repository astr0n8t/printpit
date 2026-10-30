//! Moonraker file-manager calls for the `gcodes` (and, read-only, `logs`) roots.
//!
//! Paths passed in here are *relative to the root* and must already have passed
//! [`clean_path`]. Each segment is percent-encoded when it goes into a URL path.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;

use super::client::{REQUEST_TIMEOUT, decode};
use super::types::{DirListing, FileMetadata, UploadResult};
use super::{MoonrakerClient, MoonrakerError};

/// Everything except unreserved characters gets encoded inside a path segment.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\')
    .add(b'^')
    .add(b'|')
    .add(b'[')
    .add(b']')
    .add(b'&')
    .add(b'+')
    .add(b';')
    .add(b'=');

/// Validates a root-relative path: no absolute paths, no `.`/`..` segments, no empty
/// segments, no backslashes or control characters. `""` is the root itself.
pub fn clean_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Some(String::new());
    }
    if path.len() > 1024 || path.contains('\\') || path.chars().any(char::is_control) {
        return None;
    }
    let ok = path
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
    ok.then(|| path.to_string())
}

/// A single file or directory name (no slashes).
pub fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && !name.contains('/') && !name.starts_with('.'))
        .then(|| clean_path(name))
        .flatten()
}

/// `a/b c.gcode` → `a/b%20c.gcode`
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Parent directory of a root-relative path (`""` for top-level entries).
pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The end of a file, from [`MoonrakerClient::tail`].
#[derive(Debug, Default)]
pub struct Tail {
    pub data: Vec<u8>,
    /// Size of the whole file.
    pub total: u64,
    /// `data` starts mid-file (so its first line is probably partial).
    pub truncated: bool,
}

impl Tail {
    /// The last `n` complete lines.
    pub fn last_lines(&self, n: usize) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.data);
        let mut lines: Vec<&str> = text.lines().collect();
        if self.truncated && !lines.is_empty() {
            lines.remove(0);
        }
        let skip = lines.len().saturating_sub(n);
        lines[skip..].iter().map(|l| l.to_string()).collect()
    }
}

/// Directory listings cached for a few seconds, so double clicks, sort changes and several
/// users don't each hit the printer. Any file change on a printer drops its entries.
#[derive(Default)]
pub struct DirCache {
    entries: Mutex<HashMap<(i64, String), (Instant, DirListing)>>,
}

const DIR_TTL: Duration = Duration::from_secs(5);

impl DirCache {
    pub async fn get(
        &self,
        printer_id: i64,
        client: &MoonrakerClient,
        path: &str,
    ) -> Result<DirListing, MoonrakerError> {
        let key = (printer_id, path.to_string());
        if let Some((at, listing)) = self.lock().get(&key)
            && at.elapsed() < DIR_TTL
        {
            return Ok(listing.clone());
        }
        let listing = client.directory("gcodes", path).await?;
        self.lock().insert(key, (Instant::now(), listing.clone()));
        Ok(listing)
    }

    pub fn invalidate(&self, printer_id: i64) {
        self.lock().retain(|(id, _), _| *id != printer_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(i64, String), (Instant, DirListing)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl MoonrakerClient {
    pub async fn directory(&self, root: &str, path: &str) -> Result<DirListing, MoonrakerError> {
        let full = join(root, path);
        let res = self
            .request(reqwest::Method::GET, "/server/files/directory")
            .query(&[("path", full.as_str()), ("extended", "true")])
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        decode(res).await
    }

    pub async fn metadata(&self, filename: &str) -> Result<FileMetadata, MoonrakerError> {
        let res = self
            .request(reqwest::Method::GET, "/server/files/metadata")
            .query(&[("filename", filename)])
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        decode(res).await
    }

    /// Streams a file from a root. The caller forwards the body; no timeout is applied so
    /// large downloads aren't cut off.
    pub async fn download(&self, root: &str, path: &str) -> Result<reqwest::Response, MoonrakerError> {
        let url = format!("/server/files/{root}/{}", encode_path(path));
        let res = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status().as_u16();
        Err(decode::<Value>(res).await.err().unwrap_or(MoonrakerError::Http {
            status,
            message: "download failed".into(),
        }))
    }

    /// The last `window` bytes of a file. Uses a suffix `Range` request; if the server ignores
    /// it, streams the whole file keeping only the last `window` bytes in memory.
    pub async fn tail(&self, root: &str, path: &str, window: u64) -> Result<Tail, MoonrakerError> {
        let url = format!("/server/files/{root}/{}", encode_path(path));
        let mut res = self
            .request(reqwest::Method::GET, &url)
            .header(reqwest::header::RANGE, format!("bytes=-{window}"))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        match res.status() {
            reqwest::StatusCode::PARTIAL_CONTENT => {
                // Content-Range: bytes <start>-<end>/<total>
                let range = res
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes "))
                    .and_then(|v| v.split_once('/'))
                    .map(|(span, total)| {
                        let start = span.split('-').next().and_then(|s| s.parse::<u64>().ok());
                        (start.unwrap_or(0), total.parse::<u64>().ok())
                    });
                let (start, total) = range.unwrap_or((0, None));
                let mut data = Vec::new();
                while let Some(chunk) = res.chunk().await.map_err(MoonrakerError::from_reqwest)? {
                    data.extend_from_slice(&chunk);
                    if data.len() as u64 > window {
                        break; // misbehaving server: don't read more than asked for
                    }
                }
                Ok(Tail {
                    total: total.unwrap_or(start + data.len() as u64),
                    truncated: start > 0,
                    data,
                })
            }
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE => Ok(Tail::default()),
            s if s.is_success() => {
                let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
                let mut total: u64 = 0;
                while let Some(chunk) = res.chunk().await.map_err(MoonrakerError::from_reqwest)? {
                    total += chunk.len() as u64;
                    ring.extend(chunk.iter());
                    let excess = ring.len().saturating_sub(window as usize);
                    ring.drain(..excess);
                }
                Ok(Tail {
                    truncated: total > window,
                    total,
                    data: ring.into(),
                })
            }
            _ => decode::<Value>(res).await.map(|_| Tail::default()),
        }
    }

    pub async fn delete_file(&self, path: &str) -> Result<(), MoonrakerError> {
        let url = format!("/server/files/gcodes/{}", encode_path(path));
        self.send_ok(self.request(reqwest::Method::DELETE, &url)).await
    }

    pub async fn delete_dir(&self, path: &str) -> Result<(), MoonrakerError> {
        let full = join("gcodes", path);
        let req = self
            .request(reqwest::Method::DELETE, "/server/files/directory")
            .query(&[("path", full.as_str()), ("force", "true")]);
        self.send_ok(req).await
    }

    pub async fn make_dir(&self, path: &str) -> Result<(), MoonrakerError> {
        let full = join("gcodes", path);
        let req = self
            .request(reqwest::Method::POST, "/server/files/directory")
            .query(&[("path", full.as_str())]);
        self.send_ok(req).await
    }

    /// Moves (or renames) when `copy` is false; copies otherwise.
    pub async fn move_or_copy(&self, source: &str, dest: &str, copy: bool) -> Result<(), MoonrakerError> {
        let endpoint = if copy {
            "/server/files/copy"
        } else {
            "/server/files/move"
        };
        let (src, dst) = (join("gcodes", source), join("gcodes", dest));
        let req = self
            .request(reqwest::Method::POST, endpoint)
            .query(&[("source", src.as_str()), ("dest", dst.as_str())]);
        self.send_ok(req).await
    }

    /// Uploads into `gcodes/<dir>` from a byte stream, without buffering the file.
    pub async fn upload(
        &self,
        dir: &str,
        filename: &str,
        print: bool,
        body: reqwest::Body,
    ) -> Result<UploadResult, MoonrakerError> {
        let part = reqwest::multipart::Part::stream(body)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| MoonrakerError::Decode(e.to_string()))?;
        let form = reqwest::multipart::Form::new()
            .text("root", "gcodes")
            .text("path", dir.to_string())
            .text("print", if print { "true" } else { "false" })
            .part("file", part);
        let res = self
            .request(reqwest::Method::POST, "/server/files/upload")
            .multipart(form)
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        // Moonraker versions differ on wrapping the upload response in `result`.
        let status = res.status();
        if !status.is_success() {
            return decode::<UploadResult>(res).await;
        }
        let body = res.bytes().await.map_err(MoonrakerError::from_reqwest)?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| MoonrakerError::Decode(e.to_string()))?;
        let inner = v.get("result").cloned().unwrap_or(v);
        serde_json::from_value(inner).map_err(|e| MoonrakerError::Decode(e.to_string()))
    }

    async fn send_ok(&self, req: reqwest::RequestBuilder) -> Result<(), MoonrakerError> {
        let res = req
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(MoonrakerError::from_reqwest)?;
        decode::<Value>(res).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(clean_path(""), Some(String::new()));
        assert_eq!(clean_path("/"), Some(String::new()));
        assert_eq!(clean_path("a/b.gcode"), Some("a/b.gcode".into()));
        assert_eq!(clean_path("/a/b/"), Some("a/b".into()));
        assert_eq!(clean_path("../etc/passwd"), None);
        assert_eq!(clean_path("a/../b"), None);
        assert_eq!(clean_path("a/./b"), None);
        assert_eq!(clean_path("a//b"), None);
        assert_eq!(clean_path("a\\b"), None);
        assert_eq!(clean_path("a\nb"), None);
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("clip v2.gcode"), Some("clip v2.gcode".into()));
        assert_eq!(clean_name("a/b"), None);
        assert_eq!(clean_name(".hidden"), None);
        assert_eq!(clean_name(".."), None);
        assert_eq!(clean_name(""), None);
    }

    #[test]
    fn tail_lines() {
        let t = Tail {
            data: b"tial line\nline 2\nline 3\nline 4\n".to_vec(),
            total: 1000,
            truncated: true,
        };
        assert_eq!(
            t.last_lines(10),
            ["line 2", "line 3", "line 4"],
            "partial first line dropped"
        );
        assert_eq!(t.last_lines(2), ["line 3", "line 4"]);
        let whole = Tail {
            data: b"a\nb".to_vec(),
            total: 3,
            truncated: false,
        };
        assert_eq!(whole.last_lines(10), ["a", "b"]);
        assert!(Tail::default().last_lines(5).is_empty());
    }

    #[test]
    fn encoding_and_parents() {
        assert_eq!(encode_path("parts/clip v2#1.gcode"), "parts/clip%20v2%231.gcode");
        assert_eq!(encode_path("a?b&c=d"), "a%3Fb%26c%3Dd");
        assert_eq!(parent("a/b/c.gcode"), "a/b");
        assert_eq!(parent("c.gcode"), "");
        assert_eq!(join("", "x"), "x");
        assert_eq!(join("a", "x"), "a/x");
    }
}
