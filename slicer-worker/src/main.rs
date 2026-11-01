use std::process::ExitCode;

use slicer_worker::api::{self, AppState};
use slicer_worker::config::Config;
use slicer_worker::library::Library;
use slicer_worker::{healthcheck, init_logging, orca};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => {}
        Some("healthcheck") => {
            return match healthcheck::run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("--version" | "-V") => {
            println!("printpit-slicer {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("unknown argument `{other}`\nusage: printpit-slicer [serve | healthcheck | --version]");
            return ExitCode::from(2);
        }
    }

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    init_logging(config.log_format);
    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    match runtime.block_on(serve(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(config: Config) -> std::io::Result<()> {
    let library = Library::load(&config.profiles).map_err(|e| {
        std::io::Error::other(format!(
            "cannot read profiles at {}: {e}",
            config.profiles.display()
        ))
    })?;
    let orca = orca::probe_version(&config.orca, &config.work_dir).await;
    match &orca {
        Ok(v) => tracing::info!(orca_version = %v, vendors = library.vendors().len(), "OrcaSlicer ready"),
        // Keep serving, so /v1/health can say what's wrong.
        Err(e) => tracing::error!(error = %e, "OrcaSlicer can't run; /v1/health reports unhealthy"),
    }
    let bind = config.bind;
    let app = api::router(AppState::new(config, library, orca));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, version = env!("CARGO_PKG_VERSION"), "printpit-slicer listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
