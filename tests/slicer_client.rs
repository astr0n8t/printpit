//! Contract tests: printpit's `SlicerClient` against the real worker API (`slicer-worker`), with
//! the worker's fake `orca-slicer` script standing in for Orca. If either side changes the API,
//! this breaks.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use printpit::slicer::client::{SliceRequest, WorkerOptions};
use printpit::slicer::{Kind, Profile, SlicerError, Slicing};
use serde_json::json;
use slicer_worker::api::{AppState as WorkerState, router};
use slicer_worker::config::{Config as WorkerConfig, LogFormat, Token};
use slicer_worker::library::Library;

const TOKEN: &str = "contract-token-0123456789abcdef0123";

fn worker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("slicer-worker")
}

struct Setup {
    app: common::TestApp,
    slicing: Arc<Slicing>,
    work: tempfile::TempDir,
}

/// A worker on a random port and a printpit app configured to use it with `token`.
async fn setup_with(token: &str) -> Setup {
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
    let app = router(WorkerState::new(config, library, version));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let app = common::TestApp::with_env(&[
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_SLICER_URL", &url),
        ("PRINTPIT_SLICER_TOKEN", token),
    ])
    .await;
    let slicing = app.state.slicing.clone().expect("slicing is configured");
    Setup { app, slicing, work }
}

async fn setup() -> Setup {
    setup_with(TOKEN).await
}

fn obj(v: serde_json::Value) -> Profile {
    v.as_object().unwrap().clone()
}

/// A fake model in job `job`'s storage; its first line tells fake-orca what to do.
async fn model(s: &Setup, job: i64, mode: &str) -> PathBuf {
    s.slicing.storage.create_job_dir(job).unwrap();
    let path = s.slicing.storage.model_path(job, "stl").unwrap();
    tokio::fs::write(&path, format!("FAKE {mode}\n")).await.unwrap();
    path
}

#[tokio::test]
async fn health_info_and_profiles() {
    let s = setup().await;
    let c = &s.slicing.client;
    assert_eq!(c.health().await.unwrap().orca_version, "9.9.9");
    let info = c.info().await.unwrap();
    assert_eq!(info.orca_version.as_deref(), Some("9.9.9"));
    assert!(!info.busy);
    assert!(info.vendors.iter().any(|v| v.name == "Acme"));

    let v = c.vendor("Acme").await.unwrap();
    assert_eq!(v.machines[0].name, "Acme One 0.4 nozzle");
    assert_eq!(v.machines[0].nozzle, "0.4");
    assert_eq!(v.processes[0].compatible_printers, ["Acme One 0.4 nozzle"]);
    assert_eq!(v.filaments[0].filament_type, "PLA");

    let p = c
        .profile(Kind::Process, "Acme", "0.20mm Standard @Acme")
        .await
        .unwrap();
    assert_eq!(p["wall_loops"], "3");
    let err = c.vendor("Nope").await.unwrap_err();
    assert_eq!(err.label(), "NOT FOUND");
    assert!(err.detail().contains("Nope"), "{}", err.detail());
    // Names with spaces, `@` and slashes stay one path segment.
    assert!(matches!(
        c.profile(Kind::Machine, "Acme", "../Acme/x").await,
        Err(SlicerError::NotFound(_))
    ));
}

#[tokio::test]
async fn resolves_user_presets() {
    let s = setup().await;
    let out = s
        .slicing
        .client
        .resolve(&[
            obj(
                json!({"name": "My Acme", "from": "User", "inherits": "Acme One 0.4 nozzle",
                       "printer_settings_id": "My Acme"}),
            ),
            obj(json!({"name": "Orphan", "type": "filament", "inherits": "Unobtainium"})),
        ])
        .await
        .unwrap();
    let m = out[0].as_ref().unwrap();
    assert_eq!(m.kind, Kind::Machine);
    assert_eq!(m.name, "My Acme");
    assert_eq!(m.profile["gcode_flavor"], "klipper");
    assert!(out[1].as_ref().unwrap_err().contains("Unobtainium"));
}

#[tokio::test]
async fn slices_into_storage() {
    let s = setup().await;
    let path = model(&s, 1, "ok").await;
    let machine = obj(json!({"name": "Acme One 0.4 nozzle", "from": "system"}));
    let process = obj(json!({"name": "0.20mm Standard @Acme", "from": "system"}));
    let filament = obj(json!({"name": "Generic PLA @System", "from": "system"}));
    let options = WorkerOptions {
        plate: 1,
        ..WorkerOptions::default()
    };
    let req = SliceRequest {
        machine: &machine,
        process: Some(&process),
        filament: Some(&filament),
        options: &options,
        model: &path,
    };
    let stats = s.slicing.client.slice(&req, &s.slicing.storage, 1).await.unwrap();
    assert_eq!(stats.print_time_s, Some(595));
    assert_eq!(stats.filament_mm, Some(1304.61));
    assert_eq!(stats.filament_g, Some(3.89));
    assert_eq!(stats.layers, Some(100));
    let gcode = std::fs::read_to_string(s.slicing.storage.output_path(1)).unwrap();
    assert!(gcode.starts_with("; HEADER_BLOCK_START"));
    assert_eq!(stats.bytes, gcode.len() as u64);
    assert!(!s.slicing.storage.partial_output_path(1).exists());
}

#[tokio::test]
async fn slicer_errors_come_through() {
    let s = setup().await;
    let machine = obj(json!({"name": "Acme One 0.4 nozzle", "from": "system"}));
    let p = obj(json!({"name": "p"}));
    let options = WorkerOptions {
        plate: 1,
        ..WorkerOptions::default()
    };
    let path = model(&s, 2, "fail").await;
    let req = SliceRequest {
        machine: &machine,
        process: Some(&p),
        filament: Some(&p),
        options: &options,
        model: &path,
    };
    let err = s
        .slicing
        .client
        .slice(&req, &s.slicing.storage, 2)
        .await
        .unwrap_err();
    assert_eq!(err.label(), "BAD MODEL");
    assert_eq!(
        err.detail(),
        "The input model file to the slicer can not be parsed."
    );
    assert!(!err.log_tail().is_empty());
    assert!(!err.is_transient());
    // Nothing was written for a failed slice.
    assert!(!s.slicing.storage.output_path(2).exists());
    assert!(!s.slicing.storage.partial_output_path(2).exists());

    // The worker's own limit on the model's size.
    s.slicing.storage.create_job_dir(3).unwrap();
    let big = s.slicing.storage.model_path(3, "stl").unwrap();
    // Far over the limit, so the worker stops reading long before printpit stops sending.
    tokio::fs::write(&big, vec![b'x'; 8 * 1024 * 1024]).await.unwrap();
    let req = SliceRequest { model: &big, ..req };
    let err = s
        .slicing
        .client
        .slice(&req, &s.slicing.storage, 3)
        .await
        .unwrap_err();
    assert_eq!(err.label(), "TOO LARGE");
    // The worker cleaned up after both.
    assert!(std::fs::read_dir(s.work.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn busy_worker_is_transient() {
    let s = setup().await;
    let machine = obj(json!({"name": "Acme One 0.4 nozzle", "from": "system"}));
    let p = obj(json!({"name": "p"}));
    let options = WorkerOptions {
        plate: 1,
        ..WorkerOptions::default()
    };
    let slow = model(&s, 4, "sleep").await;
    let quick = model(&s, 5, "ok").await;
    let slicing = s.slicing.clone();
    let (m, pp) = (machine.clone(), p.clone());
    let opts = options.clone();
    let first = tokio::spawn(async move {
        let req = SliceRequest {
            machine: &m,
            process: Some(&pp),
            filament: Some(&pp),
            options: &opts,
            model: &slow,
        };
        slicing.client.slice(&req, &slicing.storage, 4).await
    });
    // Wait until the worker reports the slot taken.
    let mut busy = false;
    for _ in 0..100 {
        if s.slicing.client.info().await.unwrap().busy {
            busy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(busy, "the first slice never started");
    let req = SliceRequest {
        machine: &machine,
        process: Some(&p),
        filament: Some(&p),
        options: &options,
        model: &quick,
    };
    let err = s
        .slicing
        .client
        .slice(&req, &s.slicing.storage, 5)
        .await
        .unwrap_err();
    // A clean 503, or (when the worker closed before printpit finished sending) found out
    // through /v1/info: busy either way.
    assert!(matches!(err, SlicerError::Busy { .. }), "{err:?}");
    assert_eq!(err.label(), "BUSY");
    assert!(err.is_transient());
    // Dropping the request is how printpit cancels; the worker then frees the slot.
    first.abort();
    let mut free = false;
    for _ in 0..200 {
        if !s.slicing.client.info().await.unwrap().busy {
            free = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(free, "cancelling didn't free the worker");
    drop(s.app);
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let s = setup_with("not-the-workers-token-0123456789").await;
    let err = s.slicing.client.health().await.unwrap_err();
    assert_eq!(err, SlicerError::Unauthorized);
    assert_eq!(err.label(), "UNAUTHORIZED");
    assert!(!err.detail().contains(TOKEN));

    // A slice is turned down before the worker reads the model; still reported as unauthorized.
    let path = model(&s, 6, "ok").await;
    let m = obj(json!({"name": "m"}));
    let options = WorkerOptions {
        plate: 1,
        ..WorkerOptions::default()
    };
    let req = SliceRequest {
        machine: &m,
        process: Some(&m),
        filament: Some(&m),
        options: &options,
        model: &path,
    };
    let err = s
        .slicing
        .client
        .slice(&req, &s.slicing.storage, 6)
        .await
        .unwrap_err();
    assert_eq!(err, SlicerError::Unauthorized);
}

#[tokio::test]
async fn storage_is_created_private() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup().await;
    let mode = std::fs::metadata(s.slicing.storage.root())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert!(s.slicing.storage.root().ends_with("slicer"));
}

#[tokio::test]
async fn slicing_is_off_without_a_url() {
    let app = common::TestApp::new().await;
    assert!(app.state.slicing.is_none());
}
