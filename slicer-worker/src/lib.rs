//! printpit-slicer: a stateless HTTP API around the OrcaSlicer CLI, run as printpit's optional
//! sidecar. It never talks to printers and needs no network beyond printpit. See PLAN.md §15.

pub mod api;
pub mod config;
pub mod gcode;
pub mod healthcheck;
pub mod library;
pub mod orca;

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

pub fn init_logging(format: config::LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal());
    match format {
        config::LogFormat::Pretty => builder.init(),
        config::LogFormat::Json => builder.json().flatten_event(true).init(),
    }
}
