//! Shared application state handed to every handler.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::ratelimit::LoginLimiter;
use crate::auth::setup::SetupState;
use crate::config::Config;
use crate::db::{self, Db, sessions};

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("cannot create data directory {path}: {source}")]
    DataDir { path: String, source: std::io::Error },
    #[error("cannot open database: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub setup: Arc<SetupState>,
    pub login_limiter: Arc<LoginLimiter>,
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
        Ok(Self {
            config: Arc::new(config),
            db,
            setup: Arc::new(setup),
            login_limiter: Arc::default(),
        })
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
