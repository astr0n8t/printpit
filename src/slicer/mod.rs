//! Slicing with the optional OrcaSlicer worker (PLAN §15). All of it is off unless
//! `PRINTPIT_SLICER_URL` is set; then [`AppState::slicing`](crate::state::AppState) holds a
//! [`Slicing`].
//!
//! - [`client`]: the worker's HTTP API (own client, never the printer egress).
//! - [`storage`]: `<data>/slicer/<job>/` for models and G-code (0700/0600).
//! - [`error`]: [`SlicerError`], shown like `MoonrakerError` (badge + detail, address-free).

pub mod client;
pub mod error;
pub mod import;
pub mod storage;

pub use client::SlicerClient;
pub use error::SlicerError;
pub use storage::Storage;

use crate::config::SlicerConfig;

/// An Orca profile as JSON (the worker sends and takes them flattened).
pub type Profile = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Machine,
    Process,
    Filament,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Machine, Kind::Process, Kind::Filament];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "machine" => Some(Self::Machine),
            "process" => Some(Self::Process),
            "filament" => Some(Self::Filament),
            _ => None,
        }
    }

    /// The name in URLs, the DB and the worker's API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Process => "process",
            Self::Filament => "filament",
        }
    }
}

/// Everything slicing needs at runtime.
#[derive(Debug, Clone)]
pub struct Slicing {
    pub config: SlicerConfig,
    pub client: SlicerClient,
    pub storage: Storage,
}

#[derive(Debug, thiserror::Error)]
pub enum SlicingError {
    #[error("cannot build the slicer's HTTP client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("cannot create the slicer storage directory: {0}")]
    Storage(#[from] std::io::Error),
}

impl Slicing {
    pub fn new(config: SlicerConfig, data_dir: &std::path::Path) -> Result<Self, SlicingError> {
        Ok(Self {
            client: SlicerClient::new(&config)?,
            storage: Storage::open(data_dir)?,
            config,
        })
    }
}
