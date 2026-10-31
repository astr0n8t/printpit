//! Shared application state handed to every handler.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::ratelimit::LoginLimiter;
use crate::auth::setup::SetupState;
use crate::config::Config;
use crate::crypto::Cipher;
use crate::db::printers::Printer;
use crate::db::{self, Db, sessions};
use crate::moonraker::files::DirCache;
use crate::moonraker::hub::{HubConfig, HubDeps, PrinterHub};
use crate::moonraker::status::SlicerTimes;
use crate::moonraker::webcam::WebcamCache;
use crate::moonraker::{self, Egress, MoonrakerClient, MoonrakerError};

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("cannot create data directory {path}: {source}")]
    DataDir { path: String, source: std::io::Error },
    #[error("cannot open database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("cannot build the printer HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub setup: Arc<SetupState>,
    pub login_limiter: Arc<LoginLimiter>,
    /// The only way to reach printers: allowlisted, optionally interface-bound.
    pub egress: Egress,
    pub cipher: Arc<Cipher>,
    /// Live, viewer-gated printer connections for the dashboard.
    pub hub: Arc<PrinterHub>,
    pub webcams: Arc<WebcamCache>,
    pub dirs: Arc<DirCache>,
    pub slicer: Arc<SlicerTimes>,
}

impl AppState {
    /// Creates the data directory, opens and migrates the database, and checks setup state.
    pub async fn init(config: Config) -> Result<Self, StartupError> {
        std::fs::create_dir_all(&config.data_dir).map_err(|source| StartupError::DataDir {
            path: config.data_dir.display().to_string(),
            source,
        })?;
        let db = db::open(&config.data_dir.join("printpit.db")).await?;
        let setup = SetupState::init(&db).await?;
        let egress = Egress::new(&config)?;
        let cipher = Arc::new(Cipher::new(&config.secret_key));
        let slicer = Arc::new(SlicerTimes::default());
        let hub = PrinterHub::new(
            HubDeps {
                db: db.clone(),
                egress: egress.clone(),
                cipher: cipher.clone(),
                slicer: slicer.clone(),
            },
            HubConfig {
                refresh: config.ui_refresh,
                grace: config.ws_idle_grace,
            },
        );
        Ok(Self {
            config: Arc::new(config),
            db,
            setup: Arc::new(setup),
            login_limiter: Arc::default(),
            egress,
            cipher,
            hub,
            webcams: Arc::default(),
            dirs: Arc::default(),
            slicer,
        })
    }

    pub fn decrypt_api_key(&self, printer: &Printer) -> Result<Option<String>, MoonrakerError> {
        moonraker::decrypt_api_key(&self.cipher, printer)
    }

    /// A Moonraker client for `printer`, with its API key decrypted.
    pub fn printer_client(&self, printer: &Printer) -> Result<MoonrakerClient, MoonrakerError> {
        moonraker::client_for(&self.egress, &self.cipher, printer)
    }

    /// Drops cached connections and webcam info after a printer's settings change.
    pub fn printer_changed(&self, printer_id: i64) {
        self.hub.reset(printer_id);
        self.webcams.invalidate(printer_id);
        self.dirs.invalidate(printer_id);
        self.slicer.invalidate(printer_id);
    }

    /// Periodically purges expired sessions and stale rate-limit windows.
    pub fn spawn_maintenance(&self) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                match sessions::delete_expired(&state.db, state.config.session_idle).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(removed = n, "purged expired sessions"),
                    Err(e) => tracing::warn!(error = %e, "session cleanup failed"),
                }
                state.login_limiter.prune();
            }
        });
    }
}
