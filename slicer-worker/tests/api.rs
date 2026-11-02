//! The worker's API end to end, on a real socket, with `tests/fake-orca.sh` standing in for Orca.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use slicer_worker::api::{AppState, router};
use slicer_worker::config::{Config, LogFormat, Token};
use slicer_worker::library::Library;
use slicer_worker::orca;

const TOKEN: &str = "test-token-0123456789abcdef0123456789";

struct Worker {
    url: String,
    work: tempfile::TempDir,
    rec: tempfile::TempDir,
    http: reqwest::Client,
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

async fn start_with(f: impl FnOnce(&mut Config)) -> Worker {
    let work = tempfile::tempdir().unwrap();
    let mut config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        token: Token::parse(TOKEN).unwrap(),
        orca: manifest().join("tests/fake-orca.sh"),
        profiles: manifest().join("tests/fixtures/profiles"),
        work_dir: work.path().to_path_buf(),
        timeout: Duration::from_secs(60),
        kill_grace: Duration::ZERO,
        max_model_bytes: 1024 * 1024,
        log_format: LogFormat::Pretty,
    };
    f(&mut config);
    let library = Library::load(&config.profiles).unwrap();
    let version = orca::probe_version(&config.orca, &config.work_dir).await;
    let app = router(AppState::new(config, library, version));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // See the note in Cargo.toml. Fails harmlessly when another test got there first.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Worker {
        url,
        work,
        rec: tempfile::tempdir().unwrap(),
        http: reqwest::Client::new(),
    }
}

async fn start() -> Worker {
    start_with(|_| {}).await
}

impl Worker {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.url)).bearer_auth(TOKEN)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(format!("{}{path}", self.url)).bearer_auth(TOKEN)
    }

    /// A fake model that makes fake-orca do `mode`, recording into `self.rec`.
    fn model(&self, mode: &str) -> Vec<u8> {
        format!("FAKE {mode} {}\n", self.rec.path().display()).into_bytes()
    }

    fn rec(&self, file: &str) -> String {
        std::fs::read_to_string(self.rec.path().join(file)).unwrap_or_default()
    }

    /// The standard request: Acme machine and process, shared-library PLA, then the model.
    fn form(&self, mode: &str) -> Form {
        Form::new()
            .text("machine", machine().to_string())
            .text("process", process().to_string())
            .text(
                "filament",
                json!({"name": "Generic PLA @System", "from": "system"}).to_string(),
            )
            .part("model", Part::bytes(self.model(mode)).file_name("benchy.STL"))
    }

    async fn slice(&self, form: Form) -> reqwest::Response {
        self.post("/v1/slice").multipart(form).send().await.unwrap()
    }

    /// No slice leaves anything behind in the work dir.
    async fn assert_work_dir_empty(&self) {
        wait_for("work dir to be cleaned up", || {
            std::fs::read_dir(self.work.path()).unwrap().next().is_none()
        })
        .await;
    }
}

fn machine() -> Value {
    json!({"name": "My Acme", "from": "User", "inherits": "Acme One 0.4 nozzle", "gcode_flavor": "klipper"})
}

fn process() -> Value {
    json!({"name": "0.20mm Standard @Acme", "from": "system", "compatible_printers": ["Someone Else"],
           "compatible_printers_condition": "nozzle_diameter[0]==0.6"})
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Gone, or a zombie nobody has reaped yet: either way, not running.
fn alive(pid: &str) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
    // `pid (comm) S ...`: the state follows the last ')'.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|s| s != "Z" && s != "X")
}

async fn wait_dead(w: &Worker) {
    let (me, child) = (w.rec("self.pid"), w.rec("child.pid"));
    assert!(!me.is_empty() && !child.is_empty(), "fake orca never started");
    wait_for("the slicer's process group to die", || {
        !alive(&me) && !alive(&child)
    })
    .await;
}

async fn error(res: reqwest::Response) -> Value {
    res.json::<Value>().await.unwrap()
}

#[tokio::test]
async fn every_route_needs_the_token() {
    let w = start().await;
    for path in ["/v1/health", "/v1/info", "/v1/vendors/Acme", "/v1/nope"] {
        let res = w.http.get(format!("{}{path}", w.url)).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(res.headers()["www-authenticate"], "Bearer");
        let res = w
            .http
            .get(format!("{}{path}", w.url))
            .bearer_auth("test-token-0123456789abcdef012345678X")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    let res = w
        .http
        .post(format!("{}/v1/slice", w.url))
        .multipart(w.form("ok"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(
        w.rec("args.txt").is_empty(),
        "the slicer must not run without the token"
    );
    assert_eq!(
        w.get("/v1/nope").send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn health_and_info() {
    let w = start().await;
    let h: Value = w.get("/v1/health").send().await.unwrap().json().await.unwrap();
    assert_eq!(h, json!({"status": "ok", "orca_version": "9.9.9"}));
    let i: Value = w.get("/v1/info").send().await.unwrap().json().await.unwrap();
    assert_eq!(i["orca_version"], "9.9.9");
    assert_eq!(i["busy"], false);
    assert_eq!(i["max_model_mb"], 1);
    let vendors: Vec<_> = i["vendors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].clone())
        .collect();
    assert_eq!(vendors, ["Acme", "Broken", "OrcaFilamentLibrary"]);
}

#[tokio::test]
async fn unhealthy_when_orca_cannot_run() {
    let w = start_with(|c| c.orca = "/nonexistent/orca-slicer".into()).await;
    let res = w.get("/v1/health").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = error(res).await;
    assert_eq!(body["status"], "error");
}

#[tokio::test]
async fn system_profiles() {
    let w = start().await;
    let idx: Value = w
        .get("/v1/vendors/Acme")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(idx["machines"][0]["name"], "Acme One 0.4 nozzle");
    assert_eq!(
        idx["processes"][0]["compatible_printers"],
        json!(["Acme One 0.4 nozzle"])
    );
    let p: Value = w
        .get("/v1/profiles/process/Acme/0.20mm%20Standard%20@Acme")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p["wall_loops"], "3");
    assert_eq!(p["sparse_infill_density"], "15%");
    assert!(p.get("inherits").is_none());
    for path in [
        "/v1/vendors/Nope",
        "/v1/profiles/process/Acme/Nope",
        "/v1/profiles/printer/Acme/x",
        "/v1/profiles/machine/Nope/x",
    ] {
        let res = w.get(path).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(error(res).await["kind"], "not_found");
    }
    let res = w
        .get("/v1/profiles/machine/Broken/Loop%20A")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn resolves_user_presets() {
    let w = start().await;
    let body = json!({"presets": [
        {"name": "My Acme", "from": "User", "inherits": "Acme One 0.4 nozzle", "printer_settings_id": "My Acme"},
        {"name": "Nameless?"},
    ]});
    let r: Value = w
        .post("/v1/profiles/resolve")
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["results"][0]["kind"], "machine");
    assert_eq!(r["results"][0]["profile"]["gcode_flavor"], "klipper");
    assert_eq!(r["results"][0]["profile"]["inherits"], "Acme One 0.4 nozzle");
    assert!(r["results"][1]["error"].as_str().unwrap().contains("Nameless?"));
    let res = w.post("/v1/profiles/resolve").body("{nope").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = w
        .post("/v1/profiles/resolve")
        .json(&json!({"presets": [1]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn slices_and_streams_gcode_with_stats() {
    let w = start().await;
    let res = w.slice(w.form("ok")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let h = res.headers().clone();
    assert_eq!(h["content-type"], "text/x-gcode");
    assert_eq!(h["x-slice-print-time-s"], "595");
    assert_eq!(h["x-slice-filament-mm"], "1304.61");
    assert_eq!(h["x-slice-filament-g"], "3.89");
    assert_eq!(h["x-slice-layers"], "100");
    assert_eq!(h["x-orca-version"], "9.9.9");
    let body = res.text().await.unwrap();
    assert!(body.starts_with("; HEADER_BLOCK_START"));
    assert_eq!(h["content-length"], body.len().to_string());

    // Fixed file names only, relative to the job dir: no user text on the command line.
    let args: Vec<String> = w.rec("args.txt").lines().map(str::to_string).collect();
    assert_eq!(args.last().unwrap(), "model.stl");
    assert!(
        args.windows(2)
            .any(|a| a == ["--load-settings", "machine.json;process.json"])
    );
    assert!(
        args.windows(2)
            .any(|a| a == ["--load-filaments", "filament.json"])
    );
    assert!(!args.iter().any(|a| a.contains("benchy")));
    // The environment is cleared: the token never reaches Orca.
    let env = w.rec("env.txt");
    assert!(!env.contains(TOKEN));
    assert!(!env.contains("PRINTPIT"));
    assert!(env.contains("LC_ALL=C"));
    // The user machine's system name is made acceptable to the process.
    let p: Value = serde_json::from_str(&w.rec("process.json")).unwrap();
    assert_eq!(
        p["compatible_printers"],
        json!(["Someone Else", "Acme One 0.4 nozzle"])
    );
    assert!(p.get("compatible_printers_condition").is_none());
    assert_eq!(p["type"], "process");
    let m: Value = serde_json::from_str(&w.rec("machine.json")).unwrap();
    assert_eq!(m["type"], "machine");
    let f: Value = serde_json::from_str(&w.rec("filament.json")).unwrap();
    assert_eq!(f["type"], "filament");
    w.assert_work_dir_empty().await;
}

#[tokio::test]
async fn options_reach_the_command_line() {
    let w = start().await;
    let form = Form::new()
        .text("machine", machine().to_string())
        .text(
            "options",
            json!({"orient": true, "plate": 2, "project_settings": true}).to_string(),
        )
        .part("model", Part::bytes(w.model("ok")).file_name("project.3mf"));
    let res = w.slice(form).await;
    assert_eq!(res.status(), StatusCode::OK);
    let args = w.rec("args.txt").replace('\n', " ");
    assert!(
        args.contains("--load-settings machine.json --arrange 1 --orient 1"),
        "{args}"
    );
    assert!(args.contains("--slice 2"), "{args}");
    assert!(args.ends_with("model.3mf "), "{args}");
    assert!(
        w.rec("process.json").is_empty(),
        "project settings: our process isn't sent"
    );
}

#[tokio::test]
async fn slicer_failures_are_422_with_a_scrubbed_log() {
    let w = start().await;
    let res = w.slice(w.form("fail")).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let e = error(res).await;
    assert_eq!(e["kind"], "model");
    assert_eq!(e["code"], -6);
    assert_eq!(
        e["error"],
        "The input model file to the slicer can not be parsed."
    );
    let log = e["log_tail"].to_string();
    assert!(log.contains("fake orca working in . on model.stl"), "{log}");
    assert!(!log.contains(&w.work.path().display().to_string()), "{log}");
    w.assert_work_dir_empty().await;

    for (mode, kind) in [
        ("crash", "crashed"),
        ("noresult", "slicer"),
        ("nogcode", "slicer"),
    ] {
        let res = w.slice(w.form(mode)).await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY, "{mode}");
        let e = error(res).await;
        assert_eq!(e["kind"], kind, "{mode}: {e}");
    }
    w.assert_work_dir_empty().await;
}

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let w = start_with(|c| c.timeout = Duration::from_millis(500)).await;
    let res = w.slice(w.form("sleep")).await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error(res).await["kind"], "timeout");
    wait_dead(&w).await;
    w.assert_work_dir_empty().await;
}

#[tokio::test]
async fn client_disconnect_kills_the_slicer() {
    let w = start().await;
    let req = w
        .post("/v1/slice")
        .multipart(w.form("sleep"))
        .timeout(Duration::from_millis(1500))
        .send();
    assert!(
        req.await.is_err(),
        "the slice should still be running when the client gives up"
    );
    wait_dead(&w).await;
    w.assert_work_dir_empty().await;
}

#[tokio::test]
async fn one_slice_at_a_time() {
    let w = start().await;
    let first = tokio::spawn(w.post("/v1/slice").multipart(w.form("sleep")).send());
    wait_for("the first slice to start", || !w.rec("self.pid").is_empty()).await;
    let info: Value = w.get("/v1/info").send().await.unwrap().json().await.unwrap();
    assert_eq!(info["busy"], true);
    let res = w.slice(w.form("ok")).await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(res.headers()["retry-after"], "5");
    assert_eq!(error(res).await["kind"], "busy");
    // Dropping the first request frees the slot.
    first.abort();
    wait_dead(&w).await;
    wait_for("the slot to free up", || true).await;
    let mut ok = false;
    for _ in 0..50 {
        if w.slice(w.form("ok")).await.status() == StatusCode::OK {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "the slot was never released");
    w.assert_work_dir_empty().await;
}

#[tokio::test]
async fn bad_uploads() {
    let w = start().await;
    let model = |name: &str| Part::bytes(w.model("ok")).file_name(name.to_string());
    let base = || {
        Form::new()
            .text("machine", machine().to_string())
            .text("process", process().to_string())
            .text("filament", json!({"name": "PLA"}).to_string())
    };
    let cases: Vec<(&str, Form, StatusCode)> = vec![
        ("no model", base(), StatusCode::BAD_REQUEST),
        (
            "wrong extension",
            base().part("model", model("benchy.exe")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "no extension",
            base().part("model", model("benchy")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "model not last",
            base().part("model", model("a.stl")).text("options", "{}"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown option",
            base()
                .text("options", r#"{"layer_height": 0.1}"#)
                .part("model", model("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "project settings on an STL",
            base()
                .text("options", r#"{"project_settings": true}"#)
                .part("model", model("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "no process",
            Form::new()
                .text("machine", machine().to_string())
                .text("filament", "{}")
                .part("model", model("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "profile not an object",
            base().text("machine", "[1]").part("model", model("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown part",
            base().text("evil", "x").part("model", model("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "empty model",
            base().part("model", Part::bytes(Vec::new()).file_name("a.stl")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "model too large",
            base().part(
                "model",
                Part::bytes(vec![b'x'; 1024 * 1024 + 1]).file_name("a.stl"),
            ),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ];
    for (what, form, status) in cases {
        let res = w.slice(form).await;
        assert_eq!(res.status(), status, "{what}");
        assert!(error(res).await["error"].is_string(), "{what}");
    }
    assert!(
        w.rec("args.txt").is_empty(),
        "the slicer must not run for a bad upload"
    );
    w.assert_work_dir_empty().await;
}

#[test]
fn fake_orca_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-orca.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "chmod +x tests/fake-orca.sh");
}
