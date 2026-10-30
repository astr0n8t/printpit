use std::net::SocketAddr;
use std::process::ExitCode;

use printpit::config::Config;
use printpit::state::AppState;
use printpit::{healthcheck, telemetry, web};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => {}
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
            println!("printpit {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("unknown argument `{other}`\nusage: printpit [healthcheck | --version]");
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
    telemetry::init(config.log_format);

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
    if !config.secure_cookies {
        tracing::warn!("PRINTPIT_SECURE_COOKIES=false: session cookies will be sent over plain HTTP");
    }
    let bind = config.bind;
    let app = web::router(AppState::new(config));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, version = env!("CARGO_PKG_VERSION"), "printpit listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
