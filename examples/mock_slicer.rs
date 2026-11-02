//! Runs the slicer worker for UI development, with its fake `orca-slicer` and the test
//! profiles (vendors `Acme` and `OrcaFilamentLibrary`), no OrcaSlicer needed:
//!
//!   cargo run --example mock_slicer
//!
//! Then start printpit with
//!   PRINTPIT_SLICER_URL=http://127.0.0.1:8091 PRINTPIT_SLICER_TOKEN=mock-slicer-token-0123456789abcdef
//!
//! The model's first line picks what fake-orca does: `FAKE ok`, `FAKE fail`, `FAKE sleep`
//! (stalls until cancelled; a second slice meanwhile is "busy"), `FAKE crash`. An OBJ such as
//! `FAKE ok\nv 0 0 0\n` passes printpit's model check. Anything else fails like a bad model.

use std::path::PathBuf;
use std::time::Duration;

use slicer_worker::api::{AppState, router};
use slicer_worker::config::{Config, LogFormat, Token};
use slicer_worker::library::Library;

const TOKEN: &str = "mock-slicer-token-0123456789abcdef";

#[tokio::main]
async fn main() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("slicer-worker");
    let work = tempfile::tempdir().expect("temp dir");
    let config = Config {
        bind: "127.0.0.1:8091".parse().unwrap(),
        token: Token::parse(TOKEN).unwrap(),
        orca: dir.join("tests/fake-orca.sh"),
        profiles: dir.join("tests/fixtures/profiles"),
        work_dir: work.path().to_path_buf(),
        timeout: Duration::from_secs(300),
        kill_grace: Duration::from_secs(2),
        max_model_bytes: 256 * 1024 * 1024,
        log_format: LogFormat::Pretty,
    };
    let library = Library::load(&config.profiles).expect("fixture profiles");
    let version = slicer_worker::orca::probe_version(&config.orca, &config.work_dir).await;
    let app = router(AppState::new(config, library, version));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8091")
        .await
        .expect("port 8091");
    println!("mock slicer on http://127.0.0.1:8091 (token {TOKEN})");
    axum::serve(listener, app).await.unwrap();
}
