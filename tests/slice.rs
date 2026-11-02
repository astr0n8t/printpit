//! The Slice tab and the job queue, end to end: printpit, the real worker API (with its fake
//! `orca-slicer`), the mock Moonraker, and the queue running for real.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::moonraker::MockMoonraker;
use common::slicer::Worker;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::slicer::{self as store, Job, JobStatus};
use printpit::db::{audit, printers};
use printpit::slicer::Kind;
use serde_json::Value;

struct S {
    app: TestApp,
    c: Client,
    csrf: String,
    worker: Worker,
    mock: MockMoonraker,
    printer: i64,
    process: i64,
    filament: i64,
    rec: tempfile::TempDir,
}

const OUTPUT: &str = "printpit/part_0.20mm_Standard_Acme_Generic_PLA_System.gcode";

async fn setup_with(extra: &[(&str, &str)]) -> S {
    let worker = Worker::start().await;
    let mock = MockMoonraker::start().await;
    let mut env: Vec<(String, String)> = worker
        .env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    env.push(("PRINTPIT_PRINTER_CIDRS".into(), "127.0.0.0/8".into()));
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (app, mut c) = TestApp::with_admin_env(&refs).await;
    let csrf = c.session_csrf().await;
    let printer = printers::create(
        &app.state.db,
        printers::NewPrinter {
            name: "Voron",
            base_url: &mock.url(),
            api_key_enc: None,
            webcam_override: None,
            webcam_transform: None,
            enabled: true,
            notify: false,
        },
    )
    .await
    .unwrap();
    let res = c
        .post_hx(
            "/slicer/import/system",
            &csrf,
            &[
                ("vendor", "Acme"),
                ("machine", "Acme One 0.4 nozzle"),
                ("process", "0.20mm Standard @Acme"),
                ("filament", "Generic PLA @System"),
            ],
        )
        .await;
    assert!(res.body.contains("3 added"), "{}", res.body);
    let id = async |k, n| {
        store::find_profile(&app.state.db, k, n)
            .await
            .unwrap()
            .unwrap()
            .id
    };
    let machine = id(Kind::Machine, "Acme One 0.4 nozzle").await;
    let process = id(Kind::Process, "0.20mm Standard @Acme").await;
    let filament = id(Kind::Filament, "Generic PLA @System").await;
    c.post_hx(
        &format!("/printers/{printer}/slicer"),
        &csrf,
        &[
            ("machine", &machine.to_string()),
            ("process", &process.to_string()),
            ("filament", &filament.to_string()),
        ],
    )
    .await;
    let slicing = app.state.slicing.clone().unwrap();
    slicing.queue.set_retry_delay(Duration::from_millis(100));
    app.state.start_slicing().await;
    S {
        app,
        c,
        csrf,
        worker,
        mock,
        printer,
        process,
        filament,
        rec: tempfile::tempdir().unwrap(),
    }
}

async fn setup() -> S {
    setup_with(&[]).await
}

impl S {
    /// An OBJ (passes printpit's check) whose first line tells fake-orca what to do.
    fn model(&self, mode: &str) -> Vec<u8> {
        format!(
            "FAKE {mode} {}\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n",
            self.rec.path().display()
        )
        .into_bytes()
    }

    fn rec(&self, file: &str) -> String {
        std::fs::read_to_string(self.rec.path().join(file)).unwrap_or_default()
    }

    async fn submit(&mut self, extra: &[(&str, &str)], name: &str, body: &[u8]) -> common::Resp {
        let (p, pr, f) = (
            self.printer.to_string(),
            self.process.to_string(),
            self.filament.to_string(),
        );
        let mut fields = vec![
            ("printer", p.as_str()),
            ("process", pr.as_str()),
            ("filament", f.as_str()),
        ];
        fields.extend_from_slice(extra);
        self.c
            .post_files("/slice/jobs", &self.csrf, &fields, "model", &[(name, body)])
            .await
    }

    async fn latest(&self) -> Option<Job> {
        store::recent_jobs(&self.app.state.db, None, 1)
            .await
            .unwrap()
            .into_iter()
            .next()
    }

    async fn wait(&self, id: i64, status: JobStatus) -> Job {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let job = store::get_job(&self.app.state.db, id).await.unwrap().unwrap();
            if job.status() == status {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} is {} (wanted {status:?}): {:?}",
                job.status,
                job.error
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn actions(&self) -> Vec<String> {
        audit::list(&self.app.state.db, &audit::Filter::default(), 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect()
    }

    fn storage(&self) -> printpit::slicer::Storage {
        self.app.state.slicing.as_ref().unwrap().storage.clone()
    }
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn alive(pid: &str) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|s| s != "Z" && s != "X")
}

#[tokio::test]
async fn off_without_a_worker() {
    let (app, mut c) = TestApp::with_admin().await;
    let dash = c.get("/ui/dashboard").await;
    assert!(!dash.body.contains(r#"data-key="6""#));
    assert!(!dash.body.contains("<kbd>6</kbd>"));
    assert_eq!(c.get("/ui/slice").await.status, StatusCode::NOT_FOUND);
    assert_eq!(c.get("/slice/jobs/1").await.status, StatusCode::NOT_FOUND);
    let csrf = c.session_csrf().await;
    for path in [
        "/slice/jobs/1/cancel",
        "/slice/jobs/1/upload",
        "/slice/jobs/1/print",
        "/slice/jobs/1/reslice",
    ] {
        assert_eq!(
            c.post_hx(path, &csrf, &[]).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    let res = c
        .post_files("/slice/jobs", &csrf, &[], "model", &[("a.stl", b"solid")])
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    drop(app);
}

#[tokio::test]
async fn the_tab_makes_no_printer_or_worker_calls() {
    let mut s = setup().await;
    let res = s.submit(&[], "part.obj", &s.model("ok")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let job = s.latest().await.unwrap();
    s.wait(job.id, JobStatus::Done).await;

    s.worker.clear();
    let before = s.mock.paths().len();
    let dash = s.c.get("/ui/dashboard").await;
    assert!(dash.body.contains(r#"data-key="6""#), "Slice tab shown");
    assert!(dash.body.contains("<kbd>6</kbd>"));
    for path in [
        "/ui/slice",
        &format!("/ui/slice?printer={}", s.printer),
        &format!("/slice/jobs/{}", job.id),
    ] {
        let res = s.c.get(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
        let res = s.c.get_hx(path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
    }
    // The dashboard opens its own stream; only count what the Slice pages did.
    let new: Vec<String> = s.mock.paths()[before..]
        .iter()
        .filter(|p| !p.starts_with("/websocket") && !p.contains("server/info") && !p.contains("objects"))
        .cloned()
        .collect();
    assert!(new.is_empty(), "printer calls: {new:?}");
    assert!(
        s.worker.requests().is_empty(),
        "worker calls: {:?}",
        s.worker.requests()
    );
}

#[tokio::test]
async fn slices_uploads_and_reports() {
    let mut s = setup().await;
    let res = s
        .submit(&[("infill", "40"), ("walls", "4")], "part.obj", &s.model("ok"))
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Queued part.obj for Voron."), "{}", res.body);
    assert!(res.body.contains("job #"), "the new job's panel opens");
    let job = s.latest().await.unwrap();
    let job = s.wait(job.id, JobStatus::Done).await;

    // The G-code is on the printer, under printpit/, with a name from model + profiles.
    assert_eq!(job.output_name.as_deref(), Some(OUTPUT));
    assert!(s.mock.has_file(OUTPUT), "uploaded");
    assert_eq!(job.est_seconds, Some(595));
    assert_eq!(job.layers, Some(100));
    assert_eq!(job.filament_g, Some(3.89));
    // The overrides reached the process profile the worker got.
    let p: Value = serde_json::from_str(&s.rec("process.json")).unwrap();
    assert_eq!(p["sparse_infill_density"], "40%");
    assert_eq!(p["wall_loops"], "4");
    // Fixed names only on the worker's side, too.
    assert!(s.rec("args.txt").lines().any(|l| l == "model.obj"));

    let panel = s.c.get_hx(&format!("/slice/jobs/{}", job.id)).await;
    assert!(panel.body.contains("DONE"), "{}", panel.body);
    assert!(panel.body.contains("09:55"), "{}", panel.body);
    assert!(panel.body.contains("40 % infill · 4 walls"));
    assert!(panel.body.contains("/print"), "print button");
    assert!(!panel.body.contains("data-every"), "no timer once it's done");
    // The refresh also updates the job's badge in the recent-jobs list.
    assert!(
        panel.body.contains(&format!(
            r#"id="job-badge-{}" class="badge badge-ok" hx-swap-oob="true">DONE"#,
            job.id
        )),
        "{}",
        panel.body
    );
    assert!(!panel.body.contains(&s.mock.url()), "never the printer's address");
    let a = s.actions().await;
    assert!(a.contains(&"slice_job_create".to_string()) && a.contains(&"slice_job_upload".to_string()));
    assert!(!a.contains(&"slice_job_print".to_string()));
}

#[tokio::test]
async fn starts_printing_only_when_idle() {
    let mut s = setup().await;
    // The mock is printing: the job is queued but won't start anything.
    let res = s
        .submit(&[("start_print", "on")], "part.obj", &s.model("ok"))
        .await;
    assert!(
        res.body
            .contains("Voron is printing, so it won&#39;t start printing by itself."),
        "{}",
        res.body
    );
    let job = s.latest().await.unwrap();
    assert!(!job.start_print);
    s.wait(job.id, JobStatus::Done).await;

    s.mock.set_print_state("standby");
    let res = s
        .submit(
            &[("start_print", "on"), ("replace", "on")],
            "part.obj",
            &s.model("ok"),
        )
        .await;
    assert!(!res.body.contains("start printing by itself"), "{}", res.body);
    let job = s.latest().await.unwrap();
    assert!(job.start_print);
    s.wait(job.id, JobStatus::Done).await;
    eventually("the print to start", || s.mock.print_state() == "printing").await;
    assert!(s.actions().await.contains(&"slice_job_print".to_string()));
}

#[tokio::test]
async fn failures_show_the_slicer_log() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("fail")).await;
    let job = s.latest().await.unwrap();
    let job = s.wait(job.id, JobStatus::Failed).await;
    assert!(
        job.error.as_deref().unwrap().contains("can not be parsed"),
        "{:?}",
        job.error
    );
    assert!(job.log_tail.as_deref().unwrap().contains("fake orca working"));
    assert!(!s.mock.has_file(OUTPUT));
    let panel = s.c.get_hx(&format!("/slice/jobs/{}", job.id)).await;
    assert!(
        panel.body.contains("FAILED") && panel.body.contains("slicer log"),
        "{}",
        panel.body
    );
    assert!(panel.body.contains("slice again"), "can reslice after a failure");
    assert!(!panel.body.contains("upload again"), "nothing to upload");
}

#[tokio::test]
async fn cancel_while_slicing_kills_the_slice() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("sleep")).await;
    let job = s.latest().await.unwrap();
    s.wait(job.id, JobStatus::Slicing).await;
    eventually("the slicer to start", || !s.rec("child.pid").is_empty()).await;
    let panel = s.c.get_hx(&format!("/slice/jobs/{}", job.id)).await;
    assert!(
        panel.body.contains("data-every=\"2000\""),
        "running jobs refresh: {}",
        panel.body
    );

    let res =
        s.c.post_hx(&format!("/slice/jobs/{}/cancel", job.id), &s.csrf, &[])
            .await;
    assert!(res.body.contains("Cancelled."), "{}", res.body);
    s.wait(job.id, JobStatus::Cancelled).await;
    let (me, child) = (s.rec("self.pid"), s.rec("child.pid"));
    eventually("the worker to kill the slicer", || !alive(&me) && !alive(&child)).await;
    assert!(s.actions().await.contains(&"slice_job_cancel".to_string()));

    // The queue carries on with the next job.
    let next = s.submit(&[], "part.obj", &s.model("ok")).await;
    assert_eq!(next.status, StatusCode::OK);
    let j = s.latest().await.unwrap();
    s.wait(j.id, JobStatus::Done).await;
}

#[tokio::test]
async fn auto_refresh_off_means_no_timer() {
    let mut s = setup().await;
    s.c.post_hx("/account/prefs", &s.csrf, &[]).await; // unticked: auto-refresh off
    s.submit(&[], "part.obj", &s.model("sleep")).await;
    let job = s.latest().await.unwrap();
    s.wait(job.id, JobStatus::Slicing).await;
    let panel = s.c.get_hx(&format!("/slice/jobs/{}", job.id)).await;
    assert!(panel.body.contains("refresh [r]"));
    assert!(!panel.body.contains("data-every"), "{}", panel.body);
    s.c.post_hx(&format!("/slice/jobs/{}/cancel", job.id), &s.csrf, &[])
        .await;
}

#[tokio::test]
async fn bad_uploads_are_refused_before_queueing() {
    let mut s = setup_with(&[("PRINTPIT_SLICER_MAX_MODEL_MB", "1")]).await;
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("part.gcode", b"G28\n".to_vec(), "Upload an .stl"),
        ("part.stl", b"hello, not a mesh".to_vec(), "look like an STL"),
        ("part.3mf", b"solid nope".to_vec(), "a 3MF (zip) archive"),
        ("part.obj", b"\x89PNG....".to_vec(), "OBJ"),
        ("part.obj", vec![b'v'; 2 * 1024 * 1024], "larger than 1 MB"),
        ("part.obj", Vec::new(), "empty"),
    ];
    for (name, body, msg) in cases {
        let res = s.submit(&[], name, &body).await;
        assert_eq!(res.status, StatusCode::OK, "{name}");
        assert!(res.body.contains(msg), "{name}: wanted {msg:?} in {}", res.body);
    }
    let res = s.submit(&[("infill", "150")], "part.obj", &s.model("ok")).await;
    assert!(res.body.contains("Infill must be"), "{}", res.body);
    // The fixture machine sets no layer limits of its own, so the global range applies.
    let res = s
        .submit(&[("layer_height", "1.5")], "part.obj", &s.model("ok"))
        .await;
    assert!(res.body.contains("Layer height must be 0.04–1"), "{}", res.body);
    assert!(s.latest().await.is_none(), "nothing was queued");
    // Nothing left behind, not even half-written uploads.
    let leftovers: Vec<_> = std::fs::read_dir(s.storage().root()).unwrap().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(s.worker.requests().iter().all(|r| !r.contains("slice")));
}

#[tokio::test]
async fn existing_files_need_replace() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("ok")).await;
    let first = s.latest().await.unwrap();
    s.wait(first.id, JobStatus::Done).await;
    let res = s.submit(&[], "part.obj", &s.model("ok")).await;
    assert!(res.body.contains("is already on Voron"), "{}", res.body);
    assert_eq!(s.latest().await.unwrap().id, first.id, "not queued");
    let res = s
        .submit(&[("output", "Other Name")], "part.obj", &s.model("ok"))
        .await;
    assert!(res.body.contains("Queued"), "{}", res.body);
    let job = s.wait(s.latest().await.unwrap().id, JobStatus::Done).await;
    assert_eq!(job.output_name.as_deref(), Some("printpit/Other_Name.gcode"));
    let res = s.submit(&[("replace", "on")], "part.obj", &s.model("ok")).await;
    assert!(res.body.contains("Queued"), "{}", res.body);
}

#[tokio::test]
async fn per_user_cap() {
    let mut s = setup().await;
    // Five unfinished jobs the queue never claims (it only takes queued ones).
    for _ in 0..5 {
        s.app
            .exec(
                "INSERT INTO slice_jobs (user_id, printer_id, model_name, model_size, status, created_at)
                 VALUES ((SELECT id FROM users WHERE username = 'admin'), (SELECT MIN(id) FROM printers),
                         'x.obj', 1, 'uploading', 0)",
            )
            .await;
    }
    let res = s.submit(&[], "part.obj", &s.model("ok")).await;
    assert!(
        res.body.contains("You already have 5 jobs waiting or running."),
        "{}",
        res.body
    );
    s.app.exec("UPDATE slice_jobs SET status = 'failed'").await;
    let res = s.submit(&[], "part.obj", &s.model("ok")).await;
    assert!(res.body.contains("Queued"), "{}", res.body);
}

#[tokio::test]
async fn viewers_follow_jobs_but_dont_slice() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("ok")).await;
    let job = s.latest().await.unwrap();
    s.wait(job.id, JobStatus::Done).await;
    s.app.create_user("vic", "viewer password", Role::Viewer).await;
    let mut v = s.app.client();
    v.login("vic", "viewer password").await;
    let tab = v.get("/ui/slice").await;
    assert_eq!(tab.status, StatusCode::OK);
    assert!(tab.body.contains("Viewers can follow jobs"), "{}", tab.body);
    assert!(!tab.body.contains("/slice/jobs\""), "no form");
    assert!(tab.body.contains("part.obj"), "jobs are listed");
    let panel = v.get_hx(&format!("/slice/jobs/{}", job.id)).await;
    assert!(panel.body.contains("DONE"));
    assert!(
        !panel.body.contains("/print\"") && !panel.body.contains("hx-delete"),
        "no actions"
    );
    let csrf = v.session_csrf().await;
    let res = v
        .post_files("/slice/jobs", &csrf, &[], "model", &[("a.obj", b"v 0 0 0")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    for action in ["cancel", "reslice", "upload", "print"] {
        let res = v
            .post_hx(&format!("/slice/jobs/{}/{action}", job.id), &csrf, &[])
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{action}");
    }
    assert_eq!(
        v.delete_hx(&format!("/slice/jobs/{}", job.id), &csrf)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn print_reslice_upload_again_and_delete() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("ok")).await;
    let id = s.latest().await.unwrap().id;
    s.wait(id, JobStatus::Done).await;

    // Print: refused while busy, started when idle.
    let res =
        s.c.post_hx(&format!("/slice/jobs/{id}/print"), &s.csrf, &[])
            .await;
    assert!(res.body.contains("Voron is printing."), "{}", res.body);
    s.mock.set_print_state("standby");
    let res =
        s.c.post_hx(&format!("/slice/jobs/{id}/print"), &s.csrf, &[])
            .await;
    assert!(
        res.body.contains(&format!("Printing {OUTPUT} on Voron.")),
        "{}",
        res.body
    );
    assert_eq!(s.mock.print_state(), "printing");

    // Reslice with other settings: same model, same output name.
    let res =
        s.c.post_hx(
            &format!("/slice/jobs/{id}/reslice"),
            &s.csrf,
            &[("layer_height", "0.12")],
        )
        .await;
    assert!(res.body.contains("Queued to slice again."), "{}", res.body);
    let job = s.wait(id, JobStatus::Done).await;
    assert_eq!(job.output_name.as_deref(), Some(OUTPUT));
    let p: Value = serde_json::from_str(&s.rec("process.json")).unwrap();
    assert_eq!(p["layer_height"], "0.12");

    // Upload again (the file on the printer is replaced).
    s.mock.set_print_state("standby");
    let res =
        s.c.post_hx(&format!("/slice/jobs/{id}/upload"), &s.csrf, &[])
            .await;
    assert!(res.body.contains("Uploading."), "{}", res.body);
    s.wait(id, JobStatus::Done).await;

    // Delete: row and files gone.
    let res = s.c.delete_hx(&format!("/slice/jobs/{id}"), &s.csrf).await;
    assert!(res.body.contains("Deleted the job for part.obj."), "{}", res.body);
    assert!(store::get_job(&s.app.state.db, id).await.unwrap().is_none());
    assert!(!s.storage().job_dir(id).exists());
    let a = s.actions().await;
    for want in [
        "slice_job_print",
        "slice_job_create",
        "slice_job_upload",
        "slice_job_delete",
    ] {
        assert!(a.iter().any(|x| x == want), "{want} in {a:?}");
    }
    assert_eq!(
        s.c.get_hx(&format!("/slice/jobs/{id}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn retention_sweep_removes_old_files() {
    let mut s = setup().await;
    s.submit(&[], "part.obj", &s.model("ok")).await;
    let id = s.latest().await.unwrap().id;
    s.wait(id, JobStatus::Done).await;
    let storage = s.storage();
    assert!(storage.output_path(id).exists());
    printpit::slicer::queue::sweep_now(&s.app.state).await;
    assert!(storage.output_path(id).exists(), "recent files are kept");
    s.app.exec("UPDATE slice_jobs SET finished_at = 1").await;
    // An orphan directory (no job) goes too.
    storage.create_job_dir(999).unwrap();
    printpit::slicer::queue::sweep_now(&s.app.state).await;
    assert!(!storage.job_dir(id).exists());
    assert!(!storage.job_dir(999).exists());
    // The job's row stays, without its files: no reslice or upload offered.
    let panel = s.c.get_hx(&format!("/slice/jobs/{id}")).await;
    assert!(panel.body.contains("DONE"));
    assert!(!panel.body.contains("slice again") && !panel.body.contains("upload again"));
}

#[tokio::test]
async fn interrupted_jobs_fail_on_restart() {
    // Before the queue starts: one job mid-slice, one queued.
    let worker = Worker::start().await;
    let env = worker.env();
    let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (app, _c) = TestApp::with_admin_env(&refs).await;
    let db = app.state.db.clone();
    let admin = printpit::db::users::by_username(&db, common::ADMIN)
        .await
        .unwrap()
        .unwrap()
        .id;
    let pid = printers::create(
        &db,
        printers::NewPrinter {
            name: "P",
            base_url: "http://10.0.0.9:7125",
            api_key_enc: None,
            webcam_override: None,
            webcam_transform: None,
            enabled: true,
            notify: false,
        },
    )
    .await
    .unwrap();
    let new = || store::NewJob {
        user_id: admin,
        printer_id: pid,
        model_name: "x.obj",
        model_size: 1,
        machine_profile_id: None,
        process_id: None,
        filament_id: None,
        options_json: "{}",
        start_print: false,
    };
    let running = store::create_job(&db, &new()).await.unwrap();
    store::claim_next(&db).await.unwrap();
    let queued = store::create_job(&db, &new()).await.unwrap();
    app.state.start_slicing().await;
    let j = store::get_job(&db, running).await.unwrap().unwrap();
    assert_eq!(j.status(), JobStatus::Failed);
    assert_eq!(j.error.as_deref(), Some(store::INTERRUPTED));
    // The queued one is picked up (and fails: it has no machine profile).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let j = store::get_job(&db, queued).await.unwrap().unwrap();
        if j.status() == JobStatus::Failed {
            assert!(j.error.unwrap().contains("profile"), "resumed and processed");
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn webhook_events() {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = bodies.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(v);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(axum::serve(l, app).into_future());

    let mut s = setup_with(&[("PRINTPIT_NOTIFY_WEBHOOK_URL", &url)]).await;
    s.submit(&[], "part.obj", &s.model("ok")).await;
    s.wait(s.latest().await.unwrap().id, JobStatus::Done).await;
    s.submit(&[("replace", "on")], "part.obj", &s.model("fail")).await;
    s.wait(s.latest().await.unwrap().id, JobStatus::Failed).await;
    eventually("two webhook events", || bodies.lock().unwrap().len() == 2).await;
    let b = bodies.lock().unwrap().clone();
    assert_eq!(b[0]["event"], "slice_done");
    assert_eq!(b[0]["printer"], "Voron");
    assert!(
        b[0]["text"].as_str().unwrap().contains("sliced part.obj"),
        "{}",
        b[0]
    );
    assert_eq!(b[1]["event"], "slice_failed");
    assert!(
        b[1]["text"].as_str().unwrap().contains("can not be parsed"),
        "{}",
        b[1]
    );
    let all = serde_json::to_string(&b).unwrap();
    assert!(!all.contains(&s.mock.url()), "never the printer's address");
}

#[tokio::test]
async fn a_busy_worker_keeps_the_job_queued() {
    let mut s = setup().await;
    // Someone else's slice holds the worker's one slot.
    let slicing = s.app.state.slicing.clone().unwrap();
    slicing.storage.create_job_dir(999).unwrap();
    let other = slicing.storage.model_path(999, "obj").unwrap();
    std::fs::write(&other, s.model("sleep")).unwrap();
    let held = tokio::spawn({
        let slicing = slicing.clone();
        async move {
            let m = serde_json::json!({"name": "Other", "from": "system"})
                .as_object()
                .unwrap()
                .clone();
            let o = printpit::slicer::client::WorkerOptions {
                plate: 1,
                ..Default::default()
            };
            let req = printpit::slicer::client::SliceRequest {
                machine: &m,
                process: Some(&m),
                filament: Some(&m),
                options: &o,
                model: &other,
            };
            slicing.client.slice(&req, &slicing.storage, 999).await
        }
    });
    eventually("the other slice to start", || !s.rec("self.pid").is_empty()).await;

    // A fresh record dir for our own job.
    let ours = tempfile::tempdir().unwrap();
    let model = format!("FAKE ok {}\nv 0 0 0\n", ours.path().display()).into_bytes();
    s.submit(&[], "part.obj", &model).await;
    let id = s.latest().await.unwrap().id;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let j = store::get_job(&s.app.state.db, id).await.unwrap().unwrap();
        if j.status() == JobStatus::Queued && j.error.as_deref().is_some_and(|e| e.contains("busy")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never waited: {} {:?}",
            j.status,
            j.error
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let panel = s.c.get_hx(&format!("/slice/jobs/{id}")).await;
    assert!(
        panel.body.contains("QUEUED") && panel.body.contains("busy with another job"),
        "{}",
        panel.body
    );

    // The slot frees up; the job goes through.
    held.abort();
    let job = s.wait(id, JobStatus::Done).await;
    assert!(job.error.is_none(), "the note is cleared: {:?}", job.error);
}
