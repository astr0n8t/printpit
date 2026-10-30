//! Webcam discovery for the camera proxy, cached briefly so snapshot refreshes don't each cost
//! a `server.webcams.list` call.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use url::Url;

use super::{MoonrakerClient, MoonrakerError};

const TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct Webcam {
    pub snapshot: Option<Url>,
    pub stream: Option<Url>,
}

/// Picks the printer's camera: the admin's override, else the first enabled webcam Moonraker
/// knows about. An override is a stream URL; the snapshot URL is derived from it for
/// mjpg-streamer/ustreamer-style `?action=stream` URLs.
pub async fn discover(
    client: &MoonrakerClient,
    override_url: Option<&str>,
) -> Result<Option<Webcam>, MoonrakerError> {
    if let Some(stream) = override_url.filter(|s| !s.is_empty()) {
        let snapshot = stream
            .contains("action=stream")
            .then(|| stream.replace("action=stream", "action=snapshot"));
        return Ok(Some(Webcam {
            stream: Some(client.resolve_url(stream)?),
            snapshot: snapshot.map(|s| client.resolve_url(&s)).transpose()?,
        }));
    }
    let list = client.webcams().await?;
    let Some(cam) = list.webcams.into_iter().find(|c| c.enabled != Some(false)) else {
        return Ok(None);
    };
    let mjpeg = cam.service.is_empty() || cam.service.starts_with("mjpegstreamer");
    let url = |s: &str| (!s.is_empty()).then(|| client.resolve_url(s)).transpose();
    Ok(Some(Webcam {
        snapshot: url(&cam.snapshot_url)?,
        stream: if mjpeg { url(&cam.stream_url)? } else { None },
    }))
}

#[derive(Default)]
pub struct WebcamCache {
    entries: Mutex<HashMap<i64, (Instant, Option<Webcam>)>>,
}

impl WebcamCache {
    pub async fn get(
        &self,
        printer_id: i64,
        client: &MoonrakerClient,
        override_url: Option<&str>,
    ) -> Result<Option<Webcam>, MoonrakerError> {
        if let Some((at, cam)) = self.lock().get(&printer_id)
            && at.elapsed() < TTL
        {
            return Ok(cam.clone());
        }
        let cam = discover(client, override_url).await?;
        self.lock().insert(printer_id, (Instant::now(), cam.clone()));
        Ok(cam)
    }

    pub fn invalidate(&self, printer_id: i64) {
        self.lock().remove(&printer_id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, (Instant, Option<Webcam>)>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}
