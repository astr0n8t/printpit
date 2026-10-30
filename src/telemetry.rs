//! Logging setup. `RUST_LOG` controls the filter; `PRINTPIT_LOG_FORMAT` picks pretty or JSON.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

pub fn init(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Colour only on a real terminal; `docker logs` would otherwise show escape codes.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal());
    match format {
        LogFormat::Pretty => builder.init(),
        LogFormat::Json => builder.json().flatten_event(true).init(),
    }
}
