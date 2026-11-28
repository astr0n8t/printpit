//! The calibration walkthrough, end to end: printpit, the real worker API (with its fake
//! `orca-slicer`), the mock Moonraker, and the queue running for real.

mod common;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::moonraker::MockMoonraker;
use common::slicer::Worker;
use common::{Client, TestApp};
use printpit::auth::Role;
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
    fn ids(&self) -> [(&'static str, String); 3] {
        [
            ("printer", self.printer.to_string()),
            ("process", self.process.to_string()),
            ("filament", self.filament.to_string()),
        ]
    }

    async fn post(&mut self, path: &str, extra: &[(&str, &str)]) -> common::Resp {
        let ids = self.ids();
        let mut fields: Vec<(&str, &str)> = ids.iter().map(|(k, v)| (*k, v.as_str())).collect();
        fields.extend_from_slice(extra);
        let csrf = self.csrf.clone();
        self.c.post_hx(path, &csrf, &fields).await
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

    async fn filament(&self) -> printpit::slicer::Profile {
        store::get_profile(&self.app.state.db, self.filament)
            .await
            .unwrap()
            .unwrap()
            .settings
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
async fn the_page_makes_no_calls_and_lists_the_tests() {
    let mut s = setup().await;
    s.worker.clear();
    let before = s.mock.paths().len();
    let res =
        s.c.get(&format!("/ui/slice/calibrate?printer={}", s.printer))
            .await;
    assert_eq!(res.status, StatusCode::OK);
    for t in [
        "temperature tower",
        "flow rate · coarse",
        "flow rate · fine",
        "pressure advance tower",
        "retraction test",
        "max volumetric speed",
    ] {
        assert!(res.body.contains(t), "{t}");
    }
    assert!(res.body.contains("progress: 0/6"));
    // The first test is open, with its default range for PLA.
    assert!(res.body.contains("1/6 · temperature tower"));
    assert!(
        res.body
            .contains(r#"name="start" type="number" step="any" inputmode="decimal" value="230""#)
    );
    assert!(s.worker.requests().is_empty(), "{:?}", s.worker.requests());
    assert_eq!(s.mock.paths().len(), before, "no printer calls");
    // The Slice tab links here.
    let tab = s.c.get_hx("/ui/slice").await;
    assert!(tab.body.contains("/ui/slice/calibrate?printer="));
}

#[tokio::test]
async fn a_walkthrough_tunes_the_filament_as_it_goes() {
    let mut s = setup().await;

    // 1. Temperature: sliced from printpit's own model, the G-code gets the tower's temperatures.
    let res = s
        .post(
            "/slice/calibrate/slice",
            &[
                ("test", "temperature"),
                ("start", "230"),
                ("end", "220"),
                ("step", "5"),
            ],
        )
        .await;
    assert!(res.body.contains("Queued the temperature tower"), "{}", res.body);
    let job = s.latest().await;
    assert_eq!(job.model_name, "calib_temperature_230-220.stl");
    let job = s.wait(job.id, JobStatus::Done).await;
    let gcode = s.uploaded(&job);
    assert!(
        gcode.contains(";Z:0.2\nM104 S230 ; printpit calibration: band 1\n"),
        "{gcode}"
    );
    assert_eq!(
        job.output_name.as_deref(),
        Some("printpit/calib_temperature_230-220_Generic_PLA_System.gcode")
    );

    // The page offers the tower's blocks; an admin records 225 and saves it.
    let page =
        s.c.get_hx(&format!(
            "/ui/slice/calibrate?printer={}&test=temperature",
            s.printer
        ))
        .await;
    assert!(page.body.contains("block 2 · 225 °C"), "{}", page.body);
    assert!(
        page.body.contains(&format!("id=\"job-{}\"", job.id)),
        "the job panel is shown"
    );
    let res = s
        .post(
            "/slice/calibrate/result",
            &[("test", "temperature"), ("value", "225"), ("save", "on")],
        )
        .await;
    assert!(res.body.contains("Recorded 225"), "{}", res.body);
    assert!(
        res.body
            .contains("Saved to Generic PLA @System: nozzle_temperature = 225"),
        "{}",
        res.body
    );
    // And moves on to the next test.
    assert!(res.body.contains("2/6 · flow rate · coarse"));
    let f = s.filament().await;
    assert_eq!(f["nozzle_temperature"], json!(["225"]));
    assert_eq!(f["nozzle_temperature_initial_layer"], json!(["225"]));

    // A value the print doesn't have is refused.
    let res = s
        .post(
            "/slice/calibrate/result",
            &[("test", "temperature"), ("value", "300")],
        )
        .await;
    assert!(res.body.contains("Pick one of the values"), "{}", res.body);

    // 2. Flow, recorded by an operator: not saved, but carried into the next tests.
    s.post("/slice/calibrate/slice", &[("test", "flow_coarse")]).await;
    let flow = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert_eq!(flow.model_name, "calib_flow_coarse.stl");
    s.app
        .create_user("otto", "operator password", Role::Operator)
        .await;
    let mut op = s.app.client();
    op.login("otto", "operator password").await;
    let csrf = op.session_csrf().await;
    let ids = s.ids();
    let mut fields: Vec<(&str, &str)> = ids.iter().map(|(k, v)| (*k, v.as_str())).collect();
    fields.extend([("test", "flow_coarse"), ("value", "+5"), ("save", "on")]);
    let res = op.post_hx("/slice/calibrate/result", &csrf, &fields).await;
    assert!(res.body.contains("Recorded +5"), "{}", res.body);
    assert!(
        !res.body.contains("Saved to"),
        "operators can't save to the profile"
    );
    assert!(res.body.contains("filament_flow_ratio = 1.05"), "{}", res.body);
    assert!(res.body.contains("An admin saves them"));
    assert!(!s.filament().await.contains_key("filament_flow_ratio"));
    assert_eq!(
        op.post_hx("/slice/calibrate/save", &csrf, &fields).await.status,
        StatusCode::FORBIDDEN
    );

    // The fine pass builds on it.
    let page =
        s.c.get_hx(&format!(
            "/ui/slice/calibrate?printer={}&test=flow_fine",
            s.printer
        ))
        .await;
    assert!(page.body.contains("3/6 · flow rate · fine"));
    s.post("/slice/calibrate/slice", &[("test", "flow_fine")]).await;
    let fine = s.latest().await;
    let carried = SliceOptions::from_json(&fine.options_json)
        .calibration
        .unwrap()
        .carry;
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].to_string(), "filament_flow_ratio = 1.05");
    s.wait(fine.id, JobStatus::Done).await;
    let page =
        s.c.get_hx(&format!(
            "/ui/slice/calibrate?printer={}&test=flow_fine",
            s.printer
        ))
        .await;
    assert!(page.body.contains("-2 → flow ratio 1.029"), "{}", page.body);
    s.post(
        "/slice/calibrate/result",
        &[("test", "flow_fine"), ("value", "-2")],
    )
    .await;

    // An admin saves both: the profile ends with the fine value.
    let res = s.post("/slice/calibrate/save", &[]).await;
    assert!(
        res.body
            .contains("Saved to Generic PLA @System: filament_flow_ratio = 1.029"),
        "{}",
        res.body
    );
    assert_eq!(s.filament().await["filament_flow_ratio"], json!(["1.029"]));
    assert!(res.body.contains("progress: 3/6"));

    // 3–5. The others slice too, and their G-code is changed.
    s.post(
        "/slice/calibrate/slice",
        &[
            ("test", "pressure_advance"),
            ("start", "0"),
            ("end", "0.01"),
            ("step", "0.005"),
        ],
    )
    .await;
    let pa = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&pa)
            .contains("SET_PRESSURE_ADVANCE ADVANCE=0 ; printpit calibration")
    );
    s.post(
        "/slice/calibrate/slice",
        &[
            ("test", "retraction"),
            ("start", "0.5"),
            ("end", "1"),
            ("step", "0.5"),
        ],
    )
    .await;
    let r = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(s.uploaded(&r).contains("G1 E-0.5 F1800\n"), "{}", s.uploaded(&r));
    s.post(
        "/slice/calibrate/slice",
        &[
            ("test", "volumetric_speed"),
            ("start", "5"),
            ("end", "10"),
            ("step", "1"),
        ],
    )
    .await;
    let v = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(s.uploaded(&v).contains("G1 X20 Y10 E.5 F"), "{}", s.uploaded(&v));

    let actions = s.actions().await;
    assert!(actions.iter().filter(|a| *a == "calibration_result").count() >= 3);
    assert!(actions.iter().filter(|a| *a == "slicer_profile_edit").count() >= 2);

    // Skipping, and starting over (saved values stay in the profile).
    let res = s.post("/slice/calibrate/skip", &[("test", "retraction")]).await;
    assert!(res.body.contains("Skipped the retraction test"));
    let res = s.post("/slice/calibrate/reset", &[]).await;
    assert!(res.body.contains("progress: 0/6"), "{}", res.body);
    assert_eq!(s.filament().await["filament_flow_ratio"], json!(["1.029"]));
    assert!(s.actions().await.contains(&"calibration_reset".to_string()));
}

#[tokio::test]
async fn ranges_and_order_are_checked() {
    let mut s = setup().await;
    let res = s
        .post(
            "/slice/calibrate/slice",
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
        .post(
            "/slice/calibrate/slice",
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
    let res = s
        .post(
            "/slice/calibrate/result",
            &[("test", "retraction"), ("value", "1")],
        )
        .await;
    assert!(res.body.contains("Print this test first"), "{}", res.body);
    let res = s.post("/slice/calibrate/slice", &[("test", "nope")]).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn viewers_follow_but_dont_act() {
    let s = setup().await;
    s.app.create_user("vic", "viewer password", Role::Viewer).await;
    let mut v = s.app.client();
    v.login("vic", "viewer password").await;
    let res = v.get(&format!("/ui/slice/calibrate?printer={}", s.printer)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("temperature tower"));
    assert!(!res.body.contains("/slice/calibrate/slice"));
    let csrf = v.session_csrf().await;
    let ids = s.ids();
    let mut fields: Vec<(&str, &str)> = ids.iter().map(|(k, v)| (*k, v.as_str())).collect();
    fields.push(("test", "flow_coarse"));
    let res = v.post_hx("/slice/calibrate/slice", &csrf, &fields).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn optional_tests_tune_the_printer_and_the_process() {
    let mut s = setup().await;
    let page =
        s.c.get(&format!("/ui/slice/calibrate?printer={}", s.printer))
            .await;
    assert!(page.body.contains("More tests (optional"), "{}", page.body);
    assert!(page.body.contains("input shaping · frequency"));
    assert!(page.body.contains("progress: 0/6"));
    let machine_id = store::printer_slicer(&s.app.state.db, s.printer)
        .await
        .unwrap()
        .unwrap()
        .machine_profile_id;
    let db = s.app.state.db.clone();
    let profile = async |id| store::get_profile(&db, id).await.unwrap().unwrap().settings;

    // Input shaping frequency: one value per axis, saved to the machine profile, with the
    // Klipper config to match.
    let page =
        s.c.get_hx(&format!(
            "/ui/slice/calibrate?printer={}&test=input_shaping_frequency",
            s.printer
        ))
        .await;
    assert!(page.body.contains("optional · input shaping · frequency"));
    assert!(page.body.contains("Sets the machine profile"));
    s.post(
        "/slice/calibrate/slice",
        &[
            ("test", "input_shaping_frequency"),
            ("start", "40"),
            ("end", "50"),
            ("step", "1"),
        ],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    let gcode = s.uploaded(&job);
    assert!(
        gcode.contains("SET_INPUT_SHAPER SHAPER_FREQ_X=40.00 SHAPER_FREQ_Y=40.00 ; printpit calibration"),
        "{gcode}"
    );
    let page =
        s.c.get_hx(&format!(
            "/ui/slice/calibrate?printer={}&test=input_shaping_frequency",
            s.printer
        ))
        .await;
    assert!(page.body.contains(r#"name="value_y""#), "two selects");
    let res = s
        .post(
            "/slice/calibrate/result",
            &[
                ("test", "input_shaping_frequency"),
                ("value", "44"),
                ("value_y", "47"),
                ("save", "on"),
            ],
        )
        .await;
    assert!(res.body.contains("Recorded X 44 · Y 47"), "{}", res.body);
    assert!(
        res.body
            .contains("input_shaping_freq_x = 44, input_shaping_freq_y = 47"),
        "{}",
        res.body
    );
    assert!(
        res.body.contains("shaper_freq_x: 44\nshaper_freq_y: 47"),
        "{}",
        res.body
    );
    // An optional test stays put after its result.
    assert!(res.body.contains("optional · input shaping · frequency"));
    let m = profile(machine_id).await;
    assert_eq!(m["input_shaping_freq_x"], json!("44"));
    assert_eq!(m["input_shaping_freq_y"], json!("47"));
    // One value missing is refused.
    let res = s
        .post(
            "/slice/calibrate/result",
            &[("test", "input_shaping_frequency"), ("value", "44")],
        )
        .await;
    assert!(res.body.contains("Pick one of the values"), "{}", res.body);

    // Damping starts from the saved frequencies.
    s.post(
        "/slice/calibrate/slice",
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

    // Tolerance: the G-code goes up as Orca wrote it; the result is the process's hole
    // compensation, recorded by an operator and saved later.
    s.post("/slice/calibrate/slice", &[("test", "tolerance")]).await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(!s.uploaded(&job).contains("printpit calibration"));
    let res = s
        .post(
            "/slice/calibrate/result",
            &[("test", "tolerance"), ("value", "0.1")],
        )
        .await;
    assert!(
        res.body.contains("process: xy_hole_compensation = 0.05"),
        "{}",
        res.body
    );
    let res = s.post("/slice/calibrate/save", &[]).await;
    assert!(
        res.body
            .contains("Saved to 0.20mm Standard @Acme: xy_hole_compensation = 0.05"),
        "{}",
        res.body
    );
    assert_eq!(profile(s.process).await["xy_hole_compensation"], json!("0.05"));

    // Cornering and VFA change the G-code per band.
    s.post(
        "/slice/calibrate/slice",
        &[("test", "cornering"), ("start", "4"), ("end", "8"), ("step", "1")],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job)
            .contains("SET_VELOCITY_LIMIT SQUARE_CORNER_VELOCITY=4 ; printpit calibration: band 1")
    );
    s.post(
        "/slice/calibrate/slice",
        &[("test", "vfa"), ("start", "40"), ("end", "60"), ("step", "10")],
    )
    .await;
    let job = s.wait(s.latest().await.id, JobStatus::Done).await;
    assert!(
        s.uploaded(&job).contains("G1 X20 Y10 E.5 F2400"),
        "{}",
        s.uploaded(&job)
    );

    // The core walkthrough's progress isn't changed by optional tests.
    let page =
        s.c.get_hx(&format!("/ui/slice/calibrate?printer={}", s.printer))
            .await;
    assert!(page.body.contains("progress: 0/6"));
    assert!(page.body.contains("1/6 · temperature tower"));
}

#[tokio::test]
async fn belt_printers_tip_the_pieces_back_and_skip_the_pa_lines() {
    let mut s = setup().await;
    // Make the printer's machine profile a 45° belt printer.
    let machine_id = store::printer_slicer(&s.app.state.db, s.printer)
        .await
        .unwrap()
        .unwrap()
        .machine_profile_id;
    let mut machine = store::get_profile(&s.app.state.db, machine_id)
        .await
        .unwrap()
        .unwrap()
        .settings;
    machine.insert("belt_printer".into(), json!("1"));
    machine.insert("belt_slice_rotation".into(), json!("x"));
    machine.insert("belt_slice_rotation_angle".into(), json!("45"));
    store::update_settings(&s.app.state.db, machine_id, &machine)
        .await
        .unwrap();

    let page =
        s.c.get(&format!("/ui/slice/calibrate?printer={}", s.printer))
            .await;
    assert!(page.body.contains("A belt printer"), "{}", page.body);
    assert!(
        page.body.contains("pressure advance pattern"),
        "listed with the others"
    );
    for t in ["pressure_advance_pattern", "pressure_advance_line"] {
        let page =
            s.c.get_hx(&format!("/ui/slice/calibrate?printer={}&test={t}", s.printer))
                .await;
        assert!(page.body.contains("Use the pressure advance tower"), "{t}");
        assert!(
            !page.body.contains("/slice/calibrate/slice"),
            "no slice form for {t}"
        );
        let res = s
            .post(
                "/slice/calibrate/slice",
                &[("test", t), ("start", "0"), ("end", "0.04"), ("step", "0.005")],
            )
            .await;
        assert!(
            res.body.contains("Use the pressure advance tower"),
            "{}",
            res.body
        );
    }
    assert!(
        store::recent_jobs(&s.app.state.db, None, 5)
            .await
            .unwrap()
            .is_empty()
    );

    // The PA tower is tipped back onto the belt, and its G-code still gets its values.
    s.post(
        "/slice/calibrate/slice",
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
    let process = store::get_profile(&s.app.state.db, s.process)
        .await
        .unwrap()
        .unwrap()
        .settings;
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
