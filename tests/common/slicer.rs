//! A slicer worker for tests: the real `slicer-worker` API, with its fake `orca-slicer` script and
//! fixture profiles (vendors `Acme`, `Broken` and `OrcaFilamentLibrary`), on a random port.
//! Every request it receives is recorded, so tests can prove a page made no worker calls.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::middleware::{self, Next};
use slicer_worker::api::{AppState as WorkerState, router};
use slicer_worker::config::{Config as WorkerConfig, LogFormat, Token};
use slicer_worker::library::Library;

pub const TOKEN: &str = "test-slicer-token-0123456789abcdef";

pub struct Worker {
    pub url: String,
    requests: Arc<Mutex<Vec<String>>>,
    _work: tempfile::TempDir,
}

fn worker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("slicer-worker")
}

impl Worker {
    pub async fn start() -> Self {
        let work = tempfile::tempdir().unwrap();
        let config = WorkerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Token::parse(TOKEN).unwrap(),
            orca: worker_dir().join("tests/fake-orca.sh"),
            profiles: worker_dir().join("tests/fixtures/profiles"),
            work_dir: work.path().to_path_buf(),
            timeout: Duration::from_secs(30),
            kill_grace: Duration::ZERO,
            max_model_bytes: 1024 * 1024,
            log_format: LogFormat::Pretty,
        };
        let library = Library::load(&config.profiles).unwrap();
        let version = slicer_worker::orca::probe_version(&config.orca, &config.work_dir).await;
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = requests.clone();
        let app = router(WorkerState::new(config, library, version)).layer(middleware::from_fn(
            move |req: Request, next: Next| {
                let log = log.clone();
                async move {
                    log.lock()
                        .unwrap()
                        .push(format!("{} {}", req.method(), req.uri().path()));
                    next.run(req).await
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            requests,
            _work: work,
        }
    }

    /// printpit env pointing at this worker.
    pub fn env(&self) -> [(&'static str, String); 3] {
        [
            ("PRINTPIT_SECURE_COOKIES", "false".into()),
            ("PRINTPIT_SLICER_URL", self.url.clone()),
            ("PRINTPIT_SLICER_TOKEN", TOKEN.into()),
        ]
    }

    /// `GET /v1/info`-style lines, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.requests.lock().unwrap().clear();
    }
}

/// An admin signed in to an app configured for `worker`.
pub async fn app_with(worker: &Worker) -> (super::TestApp, super::Client) {
    let env = worker.env();
    let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    super::TestApp::with_admin_env(&refs).await
}
