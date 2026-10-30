//! Text widgets for the TUI look: progress bars, sparklines, durations.

const BAR_WIDTH: usize = 20;
const SPARKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Minimum vertical range of a sparkline, so ±0.1° jitter doesn't look dramatic.
const SPARK_MIN_SPAN: f64 = 4.0;

/// `(filled, empty)` halves of a `[█████░░░░░]` bar for `fraction` in 0..=1.
pub fn bar(fraction: f64) -> (String, String) {
    let filled = ((fraction.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    ("█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

pub fn sparkline(samples: &[f64]) -> String {
    let (Some(min), Some(max)) = (
        samples.iter().copied().reduce(f64::min),
        samples.iter().copied().reduce(f64::max),
    ) else {
        return String::new();
    };
    let span = (max - min).max(SPARK_MIN_SPAN);
    let floor = min - (span - (max - min)) / 2.0;
    samples
        .iter()
        .map(|v| {
            let idx = (((v - floor) / span) * (SPARKS.len() - 1) as f64).round() as usize;
            SPARKS[idx.min(SPARKS.len() - 1)]
        })
        .collect()
}

/// `1:02:03`, or `02:03` under an hour.
pub fn duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `512 B`, `1.5 KB`, `2.4 MB`…
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Coarse age of a unix timestamp: `just now`, `5m ago`, `3h ago`, `12d ago`.
pub fn age(unix: f64) -> String {
    let secs = (crate::db::now() as f64 - unix).max(0.0) as i64;
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// `12d 3h`, `3h 07m`, `5m`.
pub fn uptime(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (d, h, m) = (total / 86_400, (total % 86_400) / 3600, (total % 3600) / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m:02}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars() {
        assert_eq!(bar(0.0), (String::new(), "░".repeat(20)));
        assert_eq!(bar(0.5), ("█".repeat(10), "░".repeat(10)));
        assert_eq!(bar(1.7), ("█".repeat(20), String::new()));
        assert_eq!(bar(-1.0).0, "");
    }

    #[test]
    fn sparklines() {
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[20.0, 120.0, 220.0]), "▁▅█");
        // Flat or jittery readings stay mid-height instead of swinging the full range.
        let flat = sparkline(&[215.0, 215.1, 214.9]);
        assert!(flat.chars().all(|c| "▃▄▅".contains(c)), "{flat}");
    }

    #[test]
    fn sizes_and_ages() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        let now = crate::db::now() as f64;
        assert_eq!(age(now), "just now");
        assert_eq!(age(now - 7200.0), "2h ago");
        assert_eq!(age(now - 3.0 * 86_400.0), "3d ago");
    }

    #[test]
    fn uptimes() {
        assert_eq!(uptime(59.0), "0m");
        assert_eq!(uptime(3.0 * 3600.0 + 7.0 * 60.0), "3h 07m");
        assert_eq!(uptime(12.0 * 86_400.0 + 3.0 * 3600.0), "12d 3h");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0.0), "00:00");
        assert_eq!(duration(125.4), "02:05");
        assert_eq!(duration(3723.0), "1:02:03");
        assert_eq!(duration(-5.0), "00:00");
    }
}
