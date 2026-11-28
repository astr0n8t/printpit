//! Calibration runs (the `[c]Calibrate` tab), end to end: printpit, the real worker API (with its
//! fake `orca-slicer`), the mock Moonraker, and the queue running for real.

mod common;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::moonraker::MockMoonraker;
use common::slicer::Worker;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::calibration as cal_db;
use printpit::db::slicer::{self as store, Job, JobStatus};
use printpit::db::{audit, printers};
use printpit::slicer::Kind;
use printpit::slicer::options::SliceOptions;
use serde_json::json;

struct S {
    app: TestApp,
    c: Client,
    csrf: String,
    worker: Worker,
    mock: MockMoonraker,
    printer: i64,
    process: i64,
    filament: i64,
}

async fn setup() -> S {
    let worker = Worker::start().await;
    let mock = MockMoonraker::start().await;
    let mut env: Vec<(String, String)> = worker
        .env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    env.push(("PRINTPIT_PRINTER_CIDRS".into(), "127.0.0.0/8".into()));
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
            ca_cert: None,
            tls_skip_verify: false,
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
    }
}

impl S {
    /// Starts a run with these tests on the setup's printer, process and filament.
    async fn start(&mut self, tests: &[&str]) -> (i64, common::Resp) {
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
        fields.extend(tests.iter().map(|t| ("test", *t)));
        let csrf = self.csrf.clone();
        let res = self.c.post_hx("/calibrate/runs", &csrf, &fields).await;
        let run = cal_db::runs(&self.app.state.db, false, 1)
            .await
            .unwrap()
            .first()
            .map_or(0, |r| r.id);
        (run, res)
    }

    /// POSTs to `/calibrate/runs/{run}/{action}`.
    async fn act(&mut self, run: i64, action: &str, fields: &[(&str, &str)]) -> common::Resp {
        let csrf = self.csrf.clone();
        self.c
            .post_hx(&format!("/calibrate/runs/{run}/{action}"), &csrf, fields)
            .await
    }

    async fn page(&mut self, run: i64, test: &str) -> common::Resp {
        self.c
            .get_hx(&format!("/ui/calibrate/runs/{run}?test={test}"))
            .await
    }

    async fn latest(&self) -> Job {
        store::recent_jobs(&self.app.state.db, None, 1)
            .await
            .unwrap()
            .remove(0)
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

    async fn profile(&self, id: i64) -> printpit::slicer::Profile {
        store::get_profile(&self.app.state.db, id)
            .await
            .unwrap()
            .unwrap()
            .settings
    }

    async fn machine_id(&self) -> i64 {
        store::printer_slicer(&self.app.state.db, self.printer)
            .await
            .unwrap()
            .unwrap()
            .machine_profile_id
    }

    async fn actions(&self) -> Vec<String> {
        audit::list(&self.app.state.db, &audit::Filter::default(), 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect()
    }

    /// The G-code the printer got for a job.
    fn uploaded(&self, job: &Job) -> String {
        let name = SliceOptions::from_json(&job.options_json).output.unwrap();
        String::from_utf8(
            self.mock
                .file_data(&format!("printpit/{name}"))
                .expect("uploaded"),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn the_tab_starts_runs_without_calls() {
    let mut s = setup().await;
    s.worker.clear();
    let before = s.mock.paths().len();
    let res = s.c.get(&format!("/ui/calibrate?printer={}", s.printer)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("start a calibration"));
    assert!(
        res.body.contains("[c]") || res.body.contains("Calibrate"),
        "the tab is in the strip"
    );
    // Core tests ticked, optional ones not.
    assert!(
        res.body.contains(r#"name="test" value="temperature" checked"#),
        "{}",
        res.body
    );
    assert!(res.body.contains(r#"name="test" value="vfa">"#), "{}", res.body);
    assert!(res.body.contains("in progress"));
    // The Slice tab links here.
    let tab = s.c.get_hx("/ui/slice").await;
    assert!(tab.body.contains("/ui/calibrate?printer="));

    let (run, res) = s.start(&["temperature", "flow_coarse"]).await;
    assert!(run > 0);
    assert!(
        res.body
            .contains("Started calibrating Generic PLA @System on Voron: 2 tests."),
        "{}",
        res.body
    );
    assert_eq!(res.header("hx-push-url"), format!("/ui/calibrate/runs/{run}"));
    assert!(res.body.contains("1/2 · temperature tower"));
    assert!(res.body.contains("slice and print"));
    assert!(
        res.body
            .contains(r#"name="start" type="number" step="any" inputmode="decimal" value="230""#)
    );
    let tab = s.c.get_hx("/ui/calibrate").await;
    assert!(tab.body.contains(&format!("/ui/calibrate/runs/{run}")));
    assert!(
        tab.body.contains("0/2") && tab.body.contains("next: temperature tower"),
        "{}",
        tab.body
    );
    assert!(s.worker.requests().is_empty(), "{:?}", s.worker.requests());
    assert_eq!(s.mock.paths().len(), before, "no printer calls");
    assert!(s.actions().await.contains(&"calibration_start".to_string()));

    // A run needs tests, and known ones.
    let (_, res) = s.start(&[]).await;
    assert!(res.body.contains("Pick at least one test."), "{}", res.body);
    let (_, res) = s.start(&["nope"]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_run_goes_print_by_print() {
    let mut s = setup().await;
    let (run, _) = s
        .start(&["temperature", "flow_coarse", "flow_fine", "pressure_advance"])
        .await;

    // Nothing to pick before there's a print.
    let res = s
        .act(
            run,
            "result",
            &[("test", "temperature"), ("value", "225"), ("then", "next")],
        )
        .await;
    assert!(
        res.body.contains("Slice and print this test first."),
        "{}",
        res.body
    );

    // 1. Temperature: slice and print, then the page asks what the print showed.
    let res = s
        .act(
            run,
            "slice",
            &[
                ("test", "temperature"),
                ("start", "230"),
                ("end", "220"),
                ("step", "5"),
                ("start_print", "on"),
            ],
        )
        .await;
    assert!(
        res.body.contains("Slicing the temperature tower for Voron"),
        "{}",
        res.body
    );
    // The mock printer is busy printing, so it won't start by itself.
    assert!(
        res.body
            .contains("Voron is printing, so it won&#39;t start printing by itself."),
        "{}",
        res.body
    );
    let job = s.latest().await;
    assert_eq!(job.model_name, "calib_temperature_230-220.stl");
    let page = s.page(run, "temperature").await;
    if !page.body.contains("what did the print show?") {
        assert!(page.body.contains("is being sliced"), "{}", page.body);
    }
    let job = s.wait(job.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job)
            .contains(";Z:0.2\nM104 S230 ; printpit calibration: band 1\n")
    );
    let page = s.page(run, "temperature").await;
    assert!(page.body.contains("what did the print show?"), "{}", page.body);
    assert!(page.body.contains("block 2 · 225 °C"));
    assert!(page.body.contains("record, then print the flow rate · coarse"));
    assert!(
        page.body.contains(&format!(r#"id="job-{}""#, job.id)),
        "the job panel"
    );

    // Record and go on: saved (admin), and the next test is up.
    let res = s
        .act(
            run,
            "result",
            &[
                ("test", "temperature"),
                ("value", "225"),
                ("save", "on"),
                ("then", "next"),
            ],
        )
        .await;
    assert!(
        res.body.contains("Recorded 225 °C for the temperature tower."),
        "{}",
        res.body
    );
    assert!(
        res.body
            .contains("Saved to Generic PLA @System: nozzle_temperature = 225"),
        "{}",
        res.body
    );
    assert!(res.body.contains("2/4 · flow rate · coarse"), "{}", res.body);
    assert!(res.body.contains("progress</dt><dd>1/4"), "{}", res.body);
    let f = s.profile(s.filament).await;
    assert_eq!(f["nozzle_temperature"], json!(["225"]));
    // A value the print doesn't have is refused.
    let res = s
        .act(
            run,
            "result",
            &[("test", "temperature"), ("value", "300"), ("then", "next")],
        )
        .await;
    assert!(res.body.contains("Pick one of the values"), "{}", res.body);

    // 2. Flow coarse, recorded by an operator with "record, then print the next": not saved,
    // and the fine pass is sliced straight away, building on it.
    s.act(run, "slice", &[("test", "flow_coarse")]).await;
    s.wait(s.latest().await.id, JobStatus::Done).await;
    s.app
        .create_user("otto", "operator password", Role::Operator)
        .await;
    let mut op = s.app.client();
    op.login("otto", "operator password").await;
    let csrf = op.session_csrf().await;
    let res = op
        .post_hx(
            &format!("/calibrate/runs/{run}/result"),
            &csrf,
            &[
                ("test", "flow_coarse"),
                ("value", "+5"),
                ("save", "on"),
                ("then", "print_next"),
            ],
        )
        .await;
    assert!(
        res.body.contains("Recorded +5 % for the flow rate · coarse."),
        "{}",
        res.body
    );
    assert!(!res.body.contains("Saved to"), "operators can't save");
    assert!(
        res.body.contains("Slicing the flow rate · fine for Voron"),
        "{}",
        res.body
    );
    assert!(res.body.contains("3/4 · flow rate · fine"), "{}", res.body);
    assert!(res.body.contains("filament: filament_flow_ratio = 1.05"));
    assert!(res.body.contains("An admin saves them"));
    assert!(!s.profile(s.filament).await.contains_key("filament_flow_ratio"));
    assert_eq!(
        op.post_hx(&format!("/calibrate/runs/{run}/save"), &csrf, &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let fine = s.latest().await;
    assert_eq!(fine.model_name, "calib_flow_fine.stl");
    let carried = SliceOptions::from_json(&fine.options_json)
        .calibration
        .unwrap()
        .carry;
    assert_eq!(carried[0].to_string(), "filament_flow_ratio = 1.05");
    s.wait(fine.id, JobStatus::Done).await;

    // 3. Fine: the choices build on the coarse result; an admin saves everything.
    let page = s.page(run, "flow_fine").await;
    assert!(page.body.contains("-2 → flow ratio 1.029"), "{}", page.body);
    s.act(
        run,
        "result",
        &[("test", "flow_fine"), ("value", "-2"), ("then", "next")],
    )
    .await;
    let res = s.act(run, "save", &[]).await;
    assert!(
        res.body
            .contains("Saved to Generic PLA @System: filament_flow_ratio = 1.029"),
        "{}",
        res.body
    );
    assert_eq!(
        s.profile(s.filament).await["filament_flow_ratio"],
        json!(["1.029"])
    );

    // 4. Skipping the last test finishes the run, which then shows its results.
    let res = s.act(run, "skip", &[("test", "pressure_advance")]).await;
    assert!(
        res.body.contains("Skipped the pressure advance tower."),
        "{}",
        res.body
    );
    assert!(res.body.contains("4/4 · finished"), "{}", res.body);
    assert!(res.body.contains("results"));
    assert!(
        res.body
            .contains("225 °C → nozzle_temperature = 225, nozzle_temperature_initial_layer = 225")
    );
    let tab = s.c.get_hx("/ui/calibrate").await;
    assert!(tab.body.contains("finished"));
    assert!(
        cal_db::get_run(&s.app.state.db, run)
            .await
            .unwrap()
            .unwrap()
            .finished_at
            .is_some()
    );
    // Taking the skip back reopens it.
    let res = s
        .act(run, "skip", &[("test", "pressure_advance"), ("undo", "1")])
        .await;
    assert!(res.body.contains("4/4 · pressure advance tower"), "{}", res.body);
    assert!(
        cal_db::get_run(&s.app.state.db, run)
            .await
            .unwrap()
            .unwrap()
            .finished_at
            .is_none()
    );

    let actions = s.actions().await;
    assert!(actions.iter().filter(|a| *a == "calibration_result").count() >= 3);
    assert!(actions.iter().filter(|a| *a == "slicer_profile_edit").count() >= 2);

    // Deleting the run keeps the profiles and the jobs.
    let csrf = s.csrf.clone();
    let res = s.c.delete_hx(&format!("/calibrate/runs/{run}"), &csrf).await;
    assert!(
        res.body.contains(&format!("Deleted calibration #{run}.")),
        "{}",
        res.body
    );
    assert!(cal_db::get_run(&s.app.state.db, run).await.unwrap().is_none());
    assert_eq!(
        s.profile(s.filament).await["filament_flow_ratio"],
        json!(["1.029"])
    );
    assert!(store::get_job(&s.app.state.db, fine.id).await.unwrap().is_some());
    assert_eq!(
        s.c.get_hx(&format!("/ui/calibrate/runs/{run}")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn ranges_and_tests_are_checked() {
    let mut s = setup().await;
    let (run, _) = s.start(&["temperature", "pressure_advance"]).await;
    let res = s
        .act(
            run,
            "slice",
            &[
                ("test", "temperature"),
                ("start", "190"),
                ("end", "230"),
                ("step", "5"),
            ],
        )
        .await;
    assert!(res.body.contains("start must be above end"), "{}", res.body);
    let res = s
        .act(
            run,
            "slice",
            &[
                ("test", "pressure_advance"),
                ("start", "x"),
                ("end", "1"),
                ("step", "1"),
            ],
        )
        .await;
    assert!(res.body.contains("Start must be a number"), "{}", res.body);
    assert!(
        store::recent_jobs(&s.app.state.db, None, 5)
            .await
            .unwrap()
            .is_empty()
    );
    // Tests that aren't part of the run.
    let res = s.act(run, "slice", &[("test", "flow_coarse")]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = s.act(run, "slice", &[("test", "nope")]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        s.act(999, "slice", &[("test", "temperature")]).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn viewers_follow_but_dont_act() {
    let mut s = setup().await;
    let (run, _) = s.start(&["temperature"]).await;
    s.app.create_user("vic", "viewer password", Role::Viewer).await;
    let mut v = s.app.client();
    v.login("vic", "viewer password").await;
    let res = v.get("/ui/calibrate").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Viewers can follow calibrations"));
    assert!(!res.body.contains(r#"hx-post="/calibrate/runs""#));
    assert!(res.body.contains(&format!("/ui/calibrate/runs/{run}")));
    let res = v.get(&format!("/ui/calibrate/runs/{run}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("temperature tower"));
    assert!(
        !res.body
            .contains(&format!(r#"hx-post="/calibrate/runs/{run}/slice""#)),
        "no slice form"
    );
    let csrf = v.session_csrf().await;
    let res = v
        .post_hx(
            &format!("/calibrate/runs/{run}/slice"),
            &csrf,
            &[("test", "temperature")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        v.delete_hx(&format!("/calibrate/runs/{run}"), &csrf).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn optional_tests_tune_the_printer_and_the_process() {
    let mut s = setup().await;
    let (run, _) = s
        .start(&[
            "tolerance",
            "cornering",
            "input_shaping_frequency",
            "input_shaping_damping",
            "vfa",
        ])
        .await;
    let machine = s.machine_id().await;

    // Tolerance: no rewrite; the result is the process's hole compensation.
    s.act(run, "slice", &[("test", "tolerance")]).await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(!s.uploaded(&job).contains("printpit calibration"));
    let res = s
        .act(
            run,
            "result",
            &[("test", "tolerance"), ("value", "0.1"), ("then", "next")],
        )
        .await;
    assert!(
        res.body.contains("process: xy_hole_compensation = 0.05"),
        "{}",
        res.body
    );
    assert!(res.body.contains("2/5 · cornering"), "{}", res.body);

    // Cornering changes the G-code per band.
    s.act(
        run,
        "slice",
        &[("test", "cornering"), ("start", "4"), ("end", "8"), ("step", "1")],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job)
            .contains("SET_VELOCITY_LIMIT SQUARE_CORNER_VELOCITY=4 ; printpit calibration: band 1")
    );
    s.act(run, "skip", &[("test", "cornering")]).await;

    // Input shaping frequency: one value per axis, to the machine profile, with Klipper's lines.
    s.act(
        run,
        "slice",
        &[
            ("test", "input_shaping_frequency"),
            ("start", "40"),
            ("end", "50"),
            ("step", "1"),
        ],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job)
            .contains("SET_INPUT_SHAPER SHAPER_FREQ_X=40.00 SHAPER_FREQ_Y=40.00")
    );
    let page = s.page(run, "input_shaping_frequency").await;
    assert!(page.body.contains(r#"name="value_y""#), "two selects");
    assert!(page.body.contains("Sets the machine profile"));
    let res = s
        .act(
            run,
            "result",
            &[
                ("test", "input_shaping_frequency"),
                ("value", "44"),
                ("value_y", "47"),
                ("save", "on"),
            ],
        )
        .await;
    assert!(res.body.contains("Recorded X 44 · Y 47 Hz"), "{}", res.body);
    // Without "then" it stays on the test, showing Klipper's lines.
    assert!(
        res.body.contains("shaper_freq_x: 44\nshaper_freq_y: 47"),
        "{}",
        res.body
    );
    let m = s.profile(machine).await;
    assert_eq!(
        (
            m["input_shaping_freq_x"].clone(),
            m["input_shaping_freq_y"].clone()
        ),
        (json!("44"), json!("47"))
    );

    // Damping starts from those frequencies.
    s.act(
        run,
        "slice",
        &[
            ("test", "input_shaping_damping"),
            ("start", "0.05"),
            ("end", "0.1"),
            ("step", "0.01"),
        ],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job)
            .contains("SET_INPUT_SHAPER SHAPER_FREQ_X=44.00 SHAPER_FREQ_Y=47.00 ; printpit calibration\n")
    );

    // VFA: outer wall speeds per band; the result is the process's outer wall speed.
    s.act(
        run,
        "slice",
        &[("test", "vfa"), ("start", "40"), ("end", "60"), ("step", "10")],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job).contains("G1 X20 Y10 E.5 F2400"),
        "{}",
        s.uploaded(&job)
    );
    s.act(
        run,
        "result",
        &[("test", "vfa"), ("value", "50"), ("save", "on"), ("then", "next")],
    )
    .await;
    assert_eq!(s.profile(s.process).await["outer_wall_speed"], json!(["50"]));
    assert_eq!(
        s.profile(s.process).await["xy_hole_compensation"],
        json!("0.05"),
        "saved with it"
    );
}

#[tokio::test]
async fn belt_printers_tip_the_pieces_back_and_skip_the_pa_lines() {
    let mut s = setup().await;
    let machine_id = s.machine_id().await;
    let mut machine = s.profile(machine_id).await;
    machine.insert("belt_printer".into(), json!("1"));
    machine.insert("belt_slice_rotation".into(), json!("x"));
    machine.insert("belt_slice_rotation_angle".into(), json!("45"));
    store::update_settings(&s.app.state.db, machine_id, &machine)
        .await
        .unwrap();

    let tab = s.c.get(&format!("/ui/calibrate?printer={}", s.printer)).await;
    assert!(tab.body.contains("A belt printer"), "{}", tab.body);
    assert!(
        tab.body
            .contains(r#"name="test" value="pressure_advance_pattern" disabled"#),
        "{}",
        tab.body
    );
    let (_, res) = s.start(&["pressure_advance", "pressure_advance_line"]).await;
    assert!(
        res.body.contains("Use the pressure advance tower"),
        "{}",
        res.body
    );
    assert!(cal_db::runs(&s.app.state.db, false, 5).await.unwrap().is_empty());

    let (run, _) = s.start(&["pressure_advance"]).await;
    s.act(
        run,
        "slice",
        &[
            ("test", "pressure_advance"),
            ("start", "0"),
            ("end", "0.01"),
            ("step", "0.005"),
        ],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    let model = std::fs::read(
        s.app
            .state
            .slicing
            .as_ref()
            .unwrap()
            .storage
            .model_path(job.id, "stl")
            .unwrap(),
    )
    .unwrap();
    let process = s.profile(s.process).await;
    let c = SliceOptions::from_json(&job.options_json).calibration.unwrap();
    assert_eq!(model, c.model(&machine, &process), "the tipped-back tower");
    let mut flat = machine.clone();
    flat.remove("belt_printer");
    assert_ne!(model, c.model(&flat, &process));
    assert!(
        s.uploaded(&job)
            .contains("SET_PRESSURE_ADVANCE ADVANCE=0 ; printpit calibration: band 1")
    );
}

/// Migration 0012 turns the first walkthrough's progress into runs.
#[tokio::test]
async fn old_progress_becomes_runs() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            "sqlite::memory:"
                .parse::<SqliteConnectOptions>()
                .unwrap()
                .foreign_keys(true),
        )
        .await
        .unwrap();
    for sql in [
        include_str!("../migrations/0001_auth.sql"),
        include_str!("../migrations/0002_printers.sql"),
        include_str!("../migrations/0003_webcam_transform.sql"),
        include_str!("../migrations/0004_temp_presets.sql"),
        include_str!("../migrations/0005_printer_notify.sql"),
        include_str!("../migrations/0006_totp.sql"),
        include_str!("../migrations/0007_slicer.sql"),
        include_str!("../migrations/0008_printer_ca.sql"),
        include_str!("../migrations/0009_printer_tls_skip_verify.sql"),
        include_str!("../migrations/0010_spoolman.sql"),
        include_str!("../migrations/0011_calibration.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO printers (id, name, base_url, enabled, created_at, updated_at, sort_order)
             VALUES (1, 'Voron', 'http://x', 1, 0, 0, 0);
         INSERT INTO slicer_profiles (id, kind, name, source, settings_json, created_at, updated_at) VALUES
             (1, 'machine', 'M', 'user', '{}', 0, 0), (2, 'process', 'P', 'user', '{}', 0, 0),
             (3, 'filament', 'F', 'user', '{}', 0, 0);
         INSERT INTO printer_slicer (printer_id, machine_profile_id, default_process_id, updated_at)
             VALUES (1, 1, 2, 0);
         INSERT INTO calibration_steps (printer_id, filament_id, test, result, changes_json, updated_at) VALUES
             (1, 3, 'temperature', '215', '[{\"key\":\"nozzle_temperature\",\"value\":\"215\"}]', 100),
             (1, 3, 'vfa', NULL, NULL, 200);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0012_calibration_runs.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let run = cal_db::get_run(&pool, 1).await.unwrap().unwrap();
    assert_eq!((run.printer_id, run.process_id, run.filament_id), (1, Some(2), 3));
    assert_eq!(run.tests.len(), 7, "the core six and the started optional one");
    assert_eq!((run.created_at, run.updated_at), (100, 200));
    let steps = cal_db::steps(&pool, 1).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].result.as_deref(), Some("215"));
    assert_eq!(steps[0].changes[0].kind, "filament");
    let gone: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM sqlite_master WHERE name = 'calibration_steps'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(gone, None);
}
