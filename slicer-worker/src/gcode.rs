//! Print statistics from the comments OrcaSlicer writes near the end of a G-code file:
//!
//! ```text
//! ; filament used [mm] = 1304.61
//! ; filament used [g] = 3.89
//! ; total layers count = 100
//! ; estimated printing time (normal mode) = 9m 55s
//! ```
//!
//! The settings dump that follows is ~30 KB, so the last 1 MiB of the file always holds them.

use std::io::SeekFrom;
use std::path::Path;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const TAIL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Stats {
    pub print_time_s: Option<u64>,
    pub filament_mm: Option<f64>,
    pub filament_g: Option<f64>,
    pub layers: Option<u32>,
}

pub async fn read_stats(path: &Path) -> std::io::Result<Stats> {
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).await?;
    let mut buf = Vec::with_capacity(TAIL_BYTES.min(len) as usize);
    f.read_to_end(&mut buf).await?;
    Ok(parse(&String::from_utf8_lossy(&buf)))
}

pub fn parse(text: &str) -> Stats {
    let mut s = Stats::default();
    for line in text.lines() {
        let Some(comment) = line.strip_prefix(';') else {
            continue;
        };
        let Some((key, value)) = comment.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "estimated printing time (normal mode)" => s.print_time_s = parse_duration(value),
            "filament used [mm]" => s.filament_mm = sum(value),
            "filament used [g]" => s.filament_g = sum(value),
            "total layers count" => s.layers = value.parse().ok(),
            _ => {}
        }
    }
    s
}

/// Multi-extruder printers list one value per extruder: `12.3, 4.5`.
fn sum(value: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in value.split(',') {
        let n: f64 = part.trim().parse().ok()?;
        if !n.is_finite() {
            return None;
        }
        total += n;
    }
    Some(total)
}

/// `1d 2h 3m 4s`, `9m 55s`, `0.977574s` → whole seconds.
pub fn parse_duration(value: &str) -> Option<u64> {
    let mut total = 0.0f64;
    let mut any = false;
    for part in value.split_whitespace() {
        let (num, unit) = part.split_at(part.find(|c: char| c.is_ascii_alphabetic())?);
        let n: f64 = num.parse().ok()?;
        let scale = match unit {
            "d" => 86_400.0,
            "h" => 3_600.0,
            "m" => 60.0,
            "s" => 1.0,
            _ => return None,
        };
        if !n.is_finite() || n < 0.0 {
            return None;
        }
        total += n * scale;
        any = true;
    }
    any.then(|| total.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOOTER: &str = "G1 X1\n; filament used [mm] = 1304.61\n; filament used [cm3] = 3.14\n\
        ; filament used [g] = 3.89\n; total filament used [g] = 3.89\n; total layers count = 100\n\
        ; estimated printing time (normal mode) = 9m 55s\n\
        ; estimated first layer printing time (normal mode) = 0.977574s\n; filament_density = 1.24\n";

    #[test]
    fn parses_orca_footer() {
        assert_eq!(
            parse(FOOTER),
            Stats {
                print_time_s: Some(595),
                filament_mm: Some(1304.61),
                filament_g: Some(3.89),
                layers: Some(100),
            }
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("6h 21m 2s"), Some(22_862));
        assert_eq!(parse_duration("1d 0h 0m 1s"), Some(86_401));
        assert_eq!(parse_duration("0.977574s"), Some(1));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("5 minutes"), None);
        assert_eq!(parse_duration("3x"), None);
    }

    #[test]
    fn multi_extruder_and_garbage() {
        let s = parse("; filament used [mm] = 10.5, 2.5\n; filament used [g] = nope\n");
        assert_eq!(s.filament_mm, Some(13.0));
        assert_eq!(s.filament_g, None);
        assert_eq!(parse("no comments here"), Stats::default());
    }

    #[tokio::test]
    async fn reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.gcode");
        let mut body = "G1 X0 Y0\n".repeat(300_000); // ~2.7 MB of moves
        body.push_str(FOOTER);
        std::fs::write(&path, body).unwrap();
        assert_eq!(read_stats(&path).await.unwrap().layers, Some(100));
    }
}
