//! `[s]Slicer` (admin): the profile library, system and preset imports, the delete guard and
//! per-printer slicing settings, against the real worker API (`common::slicer::Worker`).

mod common;

use std::io::Write;

use axum::http::StatusCode;
use common::slicer::{Worker, app_with};
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::slicer as store;
use printpit::db::{audit, printers};
use printpit::slicer::Kind;

async fn printer(app: &TestApp, name: &str) -> i64 {
    printers::create(
        &app.state.db,
        printers::NewPrinter {
            name,
            base_url: "http://10.0.0.5:7125",
            api_key_enc: None,
            webcam_override: None,
            webcam_transform: None,
            enabled: true,
            notify: false,
        },
    )
    .await
    .unwrap()
}

async fn actions(app: &TestApp) -> Vec<String> {
    audit::list(&app.state.db, &audit::Filter::default(), 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.action)
        .collect()
}

/// Imports Acme One + its process + both filaments through the UI.
async fn import_acme(c: &mut Client, csrf: &str) -> common::Resp {
    c.post_hx(
        "/slicer/import/system",
        csrf,
        &[
            ("vendor", "Acme"),
            ("machine", "Acme One 0.4 nozzle"),
            ("process", "0.20mm Standard @Acme"),
            ("filament", "Acme PLA"),
            ("filament", "Generic PLA @System"),
        ],
    )
    .await
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[tokio::test]
async fn off_without_a_worker() {
    let (app, mut c) = TestApp::with_admin().await;
    let id = printer(&app, "Voron").await;
    let dash = c.get("/ui/dashboard").await;
    assert!(!dash.body.contains("/ui/slicer"), "no Slicer tab");
    assert!(!dash.body.contains("<kbd>s</kbd>"), "no help entry");
    for path in ["/ui/slicer", "/ui/slicer/system"] {
        assert_eq!(c.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    let csrf = c.session_csrf().await;
    for path in [
        "/slicer/test",
        "/slicer/import/system",
        &format!("/printers/{id}/slicer"),
    ] {
        assert_eq!(
            c.post_hx(path, &csrf, &[]).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        c.delete_hx("/slicer/profiles/1", &csrf).await.status,
        StatusCode::NOT_FOUND
    );
    let edit = c.get(&format!("/ui/printers/{id}")).await;
    assert!(!edit.body.contains("printer-slicer"), "no slicing panel");
}

#[tokio::test]
async fn tab_is_admin_only_and_makes_no_worker_calls() {
    let worker = Worker::start().await;
    let (app, mut c) = app_with(&worker).await;
    let id = printer(&app, "Voron").await;
    worker.clear();

    let dash = c.get("/ui/dashboard").await;
    assert!(dash.body.contains(r#"data-key="s""#), "Slicer tab for admins");
    assert!(dash.body.contains("<kbd>s</kbd>"), "help lists it");
    let res = c.get("/ui/slicer").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("profile library"));
    let res = c.get_hx("/ui/slicer").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        c.get(&format!("/ui/printers/{id}"))
            .await
            .body
            .contains("printer-slicer")
    );
    assert!(
        worker.requests().is_empty(),
        "opening pages called the worker: {:?}",
        worker.requests()
    );

    app.create_user("op", "operator password", Role::Operator).await;
    let mut op = app.client();
    op.login("op", "operator password").await;
    assert!(!op.get("/ui/dashboard").await.body.contains(r#"data-key="s""#));
    assert_eq!(op.get("/ui/slicer").await.status, StatusCode::FORBIDDEN);
    assert_eq!(op.get("/ui/slicer/system").await.status, StatusCode::FORBIDDEN);
    let csrf = op.session_csrf().await;
    for path in [
        "/slicer/test",
        "/slicer/import/system",
        "/slicer/import/user",
        &format!("/printers/{id}/slicer"),
    ] {
        assert_eq!(
            op.post_hx(path, &csrf, &[]).await.status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert_eq!(
        op.delete_hx("/slicer/profiles/1", &csrf).await.status,
        StatusCode::FORBIDDEN
    );
    assert!(worker.requests().is_empty(), "{:?}", worker.requests());
}

#[tokio::test]
async fn test_button() {
    let worker = Worker::start().await;
    let (_app, mut c) = app_with(&worker).await;
    let csrf = c.session_csrf().await;
    let res = c.post_hx("/slicer/test", &csrf, &[]).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("badge-ok"), "{}", res.body);
    assert!(res.body.contains("OrcaSlicer 9.9.9"), "{}", res.body);
    assert_eq!(worker.requests(), ["GET /v1/health", "GET /v1/info"]);
    assert!(
        !res.body.contains(&worker.url),
        "never shows the worker's address"
    );
}

#[tokio::test]
async fn test_button_reports_a_dead_worker() {
    // A port nothing listens on.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    drop(l);
    let (_app, mut c) = TestApp::with_admin_env(&[
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_SLICER_URL", &url),
        ("PRINTPIT_SLICER_TOKEN", common::slicer::TOKEN),
    ])
    .await;
    let csrf = c.session_csrf().await;
    let res = c.post_hx("/slicer/test", &csrf, &[]).await;
    assert!(res.body.contains("OFFLINE"), "{}", res.body);
    assert!(!res.body.contains("127.0.0.1"), "{}", res.body);
}

#[tokio::test]
async fn browse_system_profiles_step_by_step() {
    let worker = Worker::start().await;
    let (_app, mut c) = app_with(&worker).await;

    let res = c.get_hx("/ui/slicer/system").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(r#"<option value="Acme">"#), "{}", res.body);
    assert!(
        !res.body.contains("OrcaFilamentLibrary"),
        "the shared library isn't a printer vendor"
    );

    let res = c.get_hx("/ui/slicer/system?vendor=Acme").await;
    assert!(res.body.contains("Acme One · 0.4 mm nozzle"), "{}", res.body);

    let res = c
        .get_hx("/ui/slicer/system?vendor=Acme&machine=Acme%20One%200.4%20nozzle")
        .await;
    assert!(
        res.body.contains(r#"value="0.20mm Standard @Acme""#),
        "{}",
        res.body
    );
    assert!(res.body.contains("0.2 mm layers"));
    assert!(res.body.contains(r#"value="Acme PLA""#));
    assert!(
        res.body.contains(r#"value="Generic PLA @System""#),
        "generic filaments are offered"
    );
    assert!(!res.body.contains("HAVE"), "nothing imported yet");

    let res = c.get_hx("/ui/slicer/system?vendor=Acme&machine=Nope").await;
    assert!(res.body.contains("NOT FOUND"), "{}", res.body);
    let res = c.get_hx("/ui/slicer/system?vendor=Nope").await;
    assert!(res.body.contains("NOT FOUND"), "{}", res.body);
}

#[tokio::test]
async fn import_system_profiles_flattened_and_audited() {
    let worker = Worker::start().await;
    let (app, mut c) = app_with(&worker).await;
    let csrf = c.session_csrf().await;

    let res = import_acme(&mut c, &csrf).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("4 added, 0 updated, 0 skipped"), "{}", res.body);

    let m = store::find_profile(&app.state.db, Kind::Machine, "Acme One 0.4 nozzle")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((m.source.as_str(), m.vendor.as_deref()), ("system", Some("Acme")));
    let m = store::get_profile(&app.state.db, m.id).await.unwrap().unwrap();
    // Flattened: values from the whole chain, no `inherits`.
    assert_eq!(m.settings["gcode_flavor"], "klipper");
    assert!(m.settings.get("inherits").is_none());
    let f = store::find_profile(&app.state.db, Kind::Filament, "Generic PLA @System")
        .await
        .unwrap()
        .unwrap();
    let f = store::get_profile(&app.state.db, f.id).await.unwrap().unwrap();
    assert_eq!(f.settings["filament_type"][0], "PLA");

    // Importing again updates in place.
    let res = import_acme(&mut c, &csrf).await;
    assert!(res.body.contains("0 added, 4 updated, 0 skipped"), "{}", res.body);
    assert_eq!(store::list_profiles(&app.state.db, None).await.unwrap().len(), 4);
    let a = actions(&app).await;
    assert_eq!(a.iter().filter(|x| *x == "slicer_profile_create").count(), 4);
    assert_eq!(a.iter().filter(|x| *x == "slicer_profile_update").count(), 4);

    // The picker now shows what's already in the library.
    let res = c
        .get_hx("/ui/slicer/system?vendor=Acme&machine=Acme%20One%200.4%20nozzle")
        .await;
    assert!(res.body.contains("HAVE"));

    // And the tab lists them by kind.
    let tab = c.get("/ui/slicer").await;
    assert!(
        tab.body.contains("machines <span class=\"dim\">(1)"),
        "{}",
        tab.body
    );
    assert!(tab.body.contains("filaments <span class=\"dim\">(2)"));
    assert!(tab.body.contains("orca: Acme"));

    // A name the worker doesn't know is skipped, not fatal.
    let res = c
        .post_hx(
            "/slicer/import/system",
            &csrf,
            &[
                ("vendor", "Acme"),
                ("machine", "Acme One 0.4 nozzle"),
                ("process", "Nope"),
            ],
        )
        .await;
    assert!(res.body.contains("0 added, 1 updated, 1 skipped"), "{}", res.body);
    let res = c
        .post_hx("/slicer/import/system", &csrf, &[("vendor", "Acme")])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn import_presets_and_bundles() {
    let worker = Worker::start().await;
    let (app, mut c) = app_with(&worker).await;
    let csrf = c.session_csrf().await;

    let machine = br#"{"name": "My Acme", "from": "User", "inherits": "Acme One 0.4 nozzle",
                       "printer_settings_id": "My Acme", "retraction_length": ["0.6"]}"#;
    let bundle = zip(&[
        (
            "bundle_structure.json",
            br#"{"bundle_type": "printer config bundle"}"#,
        ),
        (
            "process/Fine.json",
            br#"{"name": "Fine", "from": "User", "inherits": "0.20mm Standard @Acme",
                                   "print_settings_id": "Fine", "layer_height": "0.12"}"#,
        ),
        (
            "filament/My PLA.json",
            br#"{"name": "My PLA", "from": "User", "inherits": "Generic PLA @System",
                                      "filament_settings_id": ["My PLA"], "nozzle_temperature": ["212"]}"#,
        ),
        ("filament/cover.png", b"\x89PNG not really"),
        ("readme.txt", b"junk"),
    ]);
    let files: Vec<(&str, &[u8])> = vec![
        ("My Acme.json", machine),
        ("Acme.orca_printer", &bundle),
        ("broken.json", b"{ not json"),
        ("weird.json", br#"{"name": "Weird", "type": "gizmo"}"#),
        (
            "orphan.json",
            br#"{"name": "Orphan", "type": "filament", "inherits": "Unobtainium"}"#,
        ),
        ("model.stl", b"solid x"),
    ];
    let res = c
        .post_files("/slicer/import/user", &csrf, &[], "files", &files)
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("3 added, 0 updated, 4 skipped"), "{}", res.body);
    assert!(
        res.body
            .contains("Acme.orca_printer: 2 non-JSON entries left out")
    );
    assert!(res.body.contains("not valid JSON"));
    assert!(res.body.contains("Weird"), "wrong type is reported: {}", res.body);
    assert!(res.body.contains("Unobtainium"));
    assert!(res.body.contains("Orca bundle"), "the STL is reported");

    let m = store::find_profile(&app.state.db, Kind::Machine, "My Acme")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.source, "user");
    let m = store::get_profile(&app.state.db, m.id).await.unwrap().unwrap();
    assert_eq!(m.settings["retraction_length"][0], "0.6");
    assert_eq!(
        m.settings["gcode_flavor"], "klipper",
        "filled in from the system parent"
    );
    assert_eq!(
        m.settings["inherits"], "Acme One 0.4 nozzle",
        "keeps its system parent"
    );
    let p = store::find_profile(&app.state.db, Kind::Process, "Fine")
        .await
        .unwrap()
        .unwrap();
    let p = store::get_profile(&app.state.db, p.id).await.unwrap().unwrap();
    assert_eq!(p.settings["layer_height"], "0.12");
    assert_eq!(p.settings["compatible_printers"][0], "Acme One 0.4 nozzle");
    assert!(
        store::find_profile(&app.state.db, Kind::Filament, "My PLA")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        worker.requests(),
        ["POST /v1/profiles/resolve"],
        "one resolve for the lot"
    );

    // Nothing chosen.
    let res = c
        .post_files("/slicer/import/user", &csrf, &[], "files", &[])
        .await;
    assert!(res.body.contains("Choose a file"), "{}", res.body);
    // Too many files.
    let many: Vec<(String, Vec<u8>)> = (0..21).map(|i| (format!("{i}.json"), b"{}".to_vec())).collect();
    let refs: Vec<(&str, &[u8])> = many.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
    let res = c
        .post_files("/slicer/import/user", &csrf, &[], "files", &refs)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn printer_settings_and_the_delete_guard() {
    let worker = Worker::start().await;
    let (app, mut c) = app_with(&worker).await;
    let csrf = c.session_csrf().await;
    let id = printer(&app, "Voron").await;

    // No machine profiles yet: the panel says where to get one.
    let edit = c.get(&format!("/ui/printers/{id}")).await;
    assert!(edit.body.contains("no machine profiles yet"), "{}", edit.body);

    import_acme(&mut c, &csrf).await;
    let find = async |k, n| {
        store::find_profile(&app.state.db, k, n)
            .await
            .unwrap()
            .unwrap()
            .id
    };
    let machine = find(Kind::Machine, "Acme One 0.4 nozzle").await;
    let process = find(Kind::Process, "0.20mm Standard @Acme").await;
    let filament = find(Kind::Filament, "Acme PLA").await;
    let spare = find(Kind::Filament, "Generic PLA @System").await;

    let res = c
        .post_hx(
            &format!("/printers/{id}/slicer"),
            &csrf,
            &[
                ("machine", &machine.to_string()),
                ("process", &process.to_string()),
                ("filament", ""),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Saved slicing settings for Voron."));
    let s = store::printer_slicer(&app.state.db, id).await.unwrap().unwrap();
    assert_eq!(
        (s.machine_profile_id, s.default_process_id, s.default_filament_id),
        (machine, Some(process), None)
    );
    // The form shows what's saved.
    assert!(
        res.body
            .contains(&format!(r#"<option value="{machine}" selected>"#)),
        "{}",
        res.body
    );

    // Wrong kinds are refused.
    let res = c
        .post_hx(
            &format!("/printers/{id}/slicer"),
            &csrf,
            &[("machine", &filament.to_string())],
        )
        .await;
    assert!(res.body.contains("right kind"), "{}", res.body);

    // In use: can't delete; the message names the printer.
    let res = c.delete_hx(&format!("/slicer/profiles/{machine}"), &csrf).await;
    assert!(res.body.contains("still used by printer Voron"), "{}", res.body);
    assert!(
        store::get_profile(&app.state.db, machine)
            .await
            .unwrap()
            .is_some()
    );
    // Unused: deleted.
    let res = c.delete_hx(&format!("/slicer/profiles/{spare}"), &csrf).await;
    assert!(
        res.body.contains("Deleted filament Generic PLA @System."),
        "{}",
        res.body
    );
    assert_eq!(
        c.delete_hx(&format!("/slicer/profiles/{spare}"), &csrf)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Clearing the machine frees it.
    let res = c
        .post_hx(&format!("/printers/{id}/slicer"), &csrf, &[("machine", "")])
        .await;
    assert!(
        res.body.contains("no longer has a slicer profile"),
        "{}",
        res.body
    );
    assert!(store::printer_slicer(&app.state.db, id).await.unwrap().is_none());
    let res = c.delete_hx(&format!("/slicer/profiles/{machine}"), &csrf).await;
    assert!(res.body.contains("Deleted machine"), "{}", res.body);

    let a = actions(&app).await;
    assert_eq!(a.iter().filter(|x| *x == "printer_slicer_update").count(), 2);
    assert_eq!(a.iter().filter(|x| *x == "slicer_profile_delete").count(), 2);
    assert_eq!(
        c.post_hx("/printers/999/slicer", &csrf, &[("machine", "")])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
