pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod healthcheck;
pub mod moonraker;
pub mod notify;
pub mod slicer;
pub mod state;
pub mod telemetry;
pub mod web;

/// The version shown in the UI, logs and the User-Agent. Release builds set `PRINTPIT_VERSION`
/// at compile time (the Docker build arg, from the git tag); otherwise it's `Cargo.toml`'s.
pub const VERSION: &str = match option_env!("PRINTPIT_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};
