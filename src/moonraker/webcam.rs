//! Webcam discovery for the camera proxy, cached briefly so snapshot refreshes don't each cost
//! a `server.webcams.list` call.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use url::Url;

use super::types::WebcamInfo;
use super::{MoonrakerClient, MoonrakerError};

const TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
pub struct Webcam {
    pub snapshot: Option<Url>,
    pub stream: Option<Url>,
    pub transform: Transform,
}

/// How the camera image is turned for display (applied in CSS: flip first, then rotate).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transform {
    pub flip_h: bool,
    pub flip_v: bool,
    /// Clockwise degrees: 0, 90, 180 or 270.
    pub rotation: u16,
}

impl Transform {
    /// The admin's choices in the printer form: `(stored value, label)`. No stored value
    /// (NULL) means "use Moonraker's webcam settings".
    pub const CHOICES: [(&'static str, &'static str); 6] = [
        ("none", "none"),
        ("flip_h", "flip horizontally"),
        ("flip_v", "flip vertically"),
        ("rot_90", "rotate 90°"),
        ("rot_180", "rotate 180°"),
        ("rot_270", "rotate 270°"),
    ];

    pub fn from_setting(value: &str) -> Option<Self> {
        let t = Self::default();
        Some(match value {
            "none" => t,
            "flip_h" => Self { flip_h: true, ..t },
            "flip_v" => Self { flip_v: true, ..t },
            "rot_90" => Self { rotation: 90, ..t },
            "rot_180" => Self { rotation: 180, ..t },
            "rot_270" => Self { rotation: 270, ..t },
            _ => return None,
        })
    }

    /// Moonraker's `flip_horizontal`/`flip_vertical`/`rotation`; odd angles are ignored.
    pub fn from_info(info: &WebcamInfo) -> Self {
        Self {
            flip_h: info.flip_horizontal,
            flip_v: info.flip_vertical,
            rotation: match info.rotation.rem_euclid(360) {
                r @ (90 | 180 | 270) => r as u16,
                _ => 0,
            },
        }
    }

    /// CSS classes for the image (see `.cam-flip-h` etc. in `tui.css`).
    pub fn classes(&self) -> String {
        let mut out = Vec::new();
        if self.flip_h {
            out.push("cam-flip-h".to_string());
        }
        if self.flip_v {
            out.push("cam-flip-v".to_string());
        }
        if self.rotation != 0 {
            out.push(format!("cam-rot-{}", self.rotation));
        }
        out.join(" ")
    }
}

/// The admin's camera settings for a printer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CameraSettings<'a> {
    /// A stream URL replacing Moonraker's webcam list.
    pub url: Option<&'a str>,
    /// A [`Transform::CHOICES`] value replacing Moonraker's orientation.
    pub transform: Option<&'a str>,
}

/// Picks the printer's camera: the admin's override, else the first enabled webcam Moonraker
/// knows about. An override is a stream URL; the snapshot URL is derived from it for
/// mjpg-streamer/ustreamer-style `?action=stream` URLs.
pub async fn discover(
    client: &MoonrakerClient,
    settings: CameraSettings<'_>,
) -> Result<Option<Webcam>, MoonrakerError> {
    let chosen = settings.transform.and_then(Transform::from_setting);
    if let Some(stream) = settings.url.filter(|s| !s.is_empty()) {
        let snapshot = stream
            .contains("action=stream")
            .then(|| stream.replace("action=stream", "action=snapshot"));
        return Ok(Some(Webcam {
            stream: Some(client.resolve_url(stream)?),
            snapshot: snapshot.map(|s| client.resolve_url(&s)).transpose()?,
            transform: chosen.unwrap_or_default(),
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
        transform: chosen.unwrap_or_else(|| Transform::from_info(&cam)),
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
        settings: CameraSettings<'_>,
    ) -> Result<Option<Webcam>, MoonrakerError> {
        if let Some((at, cam)) = self.lock().get(&printer_id)
            && at.elapsed() < TTL
        {
            return Ok(cam.clone());
        }
        let cam = discover(client, settings).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms() {
        for (value, _) in Transform::CHOICES {
            assert!(Transform::from_setting(value).is_some(), "{value}");
        }
        assert_eq!(Transform::from_setting("auto"), None);
        assert_eq!(Transform::from_setting("rot_90").unwrap().classes(), "cam-rot-90");
        assert_eq!(Transform::from_setting("none").unwrap().classes(), "");

        let info = WebcamInfo {
            flip_horizontal: true,
            flip_vertical: true,
            rotation: -90,
            ..WebcamInfo::default()
        };
        let t = Transform::from_info(&info);
        assert_eq!(t.rotation, 270);
        assert_eq!(t.classes(), "cam-flip-h cam-flip-v cam-rot-270");
        let odd = WebcamInfo {
            rotation: 45,
            ..WebcamInfo::default()
        };
        assert_eq!(Transform::from_info(&odd), Transform::default());
    }
}
