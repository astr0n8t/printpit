//! Heater targets and temperature presets: the admin presets panel, the card's temps modal,
//! G-code validation and role gating.

mod common;

use axum::http::StatusCode;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};

const ENV: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
    ("PRINTPIT_UI_REFRESH_MS", "50"),
];

async fn setup() -> (TestApp, Client, String, MockMoonraker, i64) {
    let mock = MockMoonraker::start().await;
    let (app, mut c) = TestApp::with_admin_env(ENV).await;
    let id = printers::create(
        &app.state.db,
        NewPrinter {
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
    let csrf = c.session_csrf().await;
    (app, c, csrf, mock, id)
}

async fn user(app: &TestApp, name: &str, role: Role) -> (Client, String) {
    app.create_user(name, "password123", role).await;
    let mut c = app.client();
    c.login(name, "password123").await;
    let csrf = c.session_csrf().await;
    (c, csrf)
}

async fn audit_actions(app: &TestApp, like: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE ? ORDER BY id")
        .bind(like)
        .fetch_all(&app.state.db)
        .await
        .unwrap()
}

async fn add_preset(c: &mut Client, csrf: &str, name: &str, extruder: &str, bed: &str) -> common::Resp {
    c.post_hx(
        "/presets",
        csrf,
        &[("name", name), ("extruder", extruder), ("bed", bed)],
    )
    .await
}

async fn preset_id(app: &TestApp, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM temp_presets WHERE name = ?")
        .bind(name)
        .fetch_one(&app.state.db)
        .await
        .unwrap()
}

// ── Presets panel (admin) ────────────────────────────────────────────

#[tokio::test]
async fn admin_manages_presets() {
    let (app, mut c, csrf, _mock, _id) = setup().await;

    let res = add_preset(&mut c, &csrf, "PETG", "240", "80").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Added preset PETG."));
    add_preset(&mut c, &csrf, "PLA", "215", "60").await;
    add_preset(&mut c, &csrf, "Bed warm", "", "55.5").await;

    // Coolest hotend first; bed-only presets last.
    let page = c.get_hx("/ui/printers").await.body;
    let pos = |s: &str| page.find(s).unwrap_or_else(|| panic!("{s} missing from {page}"));
    assert!(pos(r#"value="PLA""#) < pos(r#"value="PETG""#));
    assert!(pos(r#"value="PETG""#) < pos(r#"value="Bed warm""#));
    assert!(page.contains(r#"value="55.5""#));

    let cases: &[(&str, &str, &str, &str)] = &[
        ("", "200", "", "1–32 characters"),
        ("Empty", "", "", "hotend or bed temperature"),
        ("Hot", "501", "", "hotend temperature must be"),
        ("Cold", "", "-1", "bed temperature must be"),
        ("Bad", "abc", "", "hotend temperature must be"),
        ("pla", "200", "", "already has that name"),
    ];
    for (name, extruder, bed, msg) in cases {
        let res = add_preset(&mut c, &csrf, name, extruder, bed).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{msg}");
        assert!(res.body.contains(msg), "expected `{msg}` in {}", res.body);
    }

    let pla = preset_id(&app, "PLA").await;
    let res = c
        .post_hx(
            &format!("/presets/{pla}"),
            &csrf,
            &[("name", "PLA+"), ("extruder", "220"), ("bed", "")],
        )
        .await;
    assert!(res.body.contains("Saved preset PLA+."), "{}", res.body);
    let row: (String, Option<f64>, Option<f64>) =
        sqlx::query_as("SELECT name, extruder, bed FROM temp_presets WHERE id = ?")
            .bind(pla)
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(row, ("PLA+".to_string(), Some(220.0), None));

    let res = c.delete_hx(&format!("/presets/{pla}"), &csrf).await;
    assert!(res.body.contains("Deleted preset PLA+."));
    assert_eq!(
        c.delete_hx(&format!("/presets/{pla}"), &csrf).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        audit_actions(&app, "preset_%").await,
        [
            "preset_create",
            "preset_create",
            "preset_create",
            "preset_update",
            "preset_delete"
        ]
    );
}

// ── Temps modal ──────────────────────────────────────────────────────

#[tokio::test]
async fn modal_lists_heaters_and_presets() {
    let (app, mut admin, csrf, mock, id) = setup().await;
    add_preset(&mut admin, &csrf, "PLA", "215", "60").await;
    let (mut op, _) = user(&app, "op", Role::Operator).await;

    let res = op.get_hx(&format!("/p/{id}/temps")).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = res.body;
    let pos = |s: &str| body.find(s).unwrap_or_else(|| panic!("{s} missing from {body}"));
    assert!(pos(r#"name="heater" value="extruder""#) < pos(r#"name="heater" value="heater_bed""#));
    assert!(body.contains(r#"name="heater" value="heater_generic chamber""#));
    assert!(body.contains("PLA") && body.contains("215/60 °C"));
    assert!(body.contains("cooldown"));
    assert!(!body.contains("Printers page"), "the hint is for admins");
    assert!(!body.contains(&mock.url()));
    // No live connection to borrow from: one HTTP snapshot.
    assert!(mock.paths().contains(&"/printer/objects/query".to_string()));

    let (mut viewer, _) = user(&app, "viewer", Role::Viewer).await;
    assert_eq!(
        viewer.get_hx(&format!("/p/{id}/temps")).await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn modal_reuses_the_live_connection() {
    let (_app, mut c, _csrf, mock, id) = setup().await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    let res = c.get_hx(&format!("/p/{id}/temps")).await;
    assert!(res.body.contains(r#"value="extruder""#));
    assert!(
        mock.paths()
            .iter()
            .all(|p| p.starts_with("ws:") || p == "/websocket"),
        "no HTTP calls while the dashboard is live: {:?}",
        mock.paths()
    );
}

// ── Setting targets ──────────────────────────────────────────────────

#[tokio::test]
async fn operator_sets_targets() {
    let (app, _admin, _csrf, mock, id) = setup().await;
    let (mut op, csrf) = user(&app, "op", Role::Operator).await;

    let res = op
        .post_hx(
            &format!("/p/{id}/temp"),
            &csrf,
            &[("heater", "extruder"), ("target", "215")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("extruder → 215 °C."), "{}", res.body);
    assert_eq!(res.header("hx-trigger"), "printpit:close-modal");

    op.post_hx(
        &format!("/p/{id}/temp"),
        &csrf,
        &[("heater", "heater_generic chamber"), ("target", "45.5")],
    )
    .await;
    let res = op
        .post_hx(
            &format!("/p/{id}/temp"),
            &csrf,
            &[("heater", "heater_bed"), ("target", "0")],
        )
        .await;
    assert!(res.body.contains("bed off."));
    assert_eq!(
        mock.scripts(),
        [
            "SET_HEATER_TEMPERATURE HEATER=extruder TARGET=215",
            "SET_HEATER_TEMPERATURE HEATER=chamber TARGET=45.5",
            "SET_HEATER_TEMPERATURE HEATER=heater_bed TARGET=0",
        ]
    );
    let detail: String =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'temp_set' ORDER BY id LIMIT 1")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert_eq!(detail, "heater=extruder target=215");
    assert_eq!(audit_actions(&app, "temp_%").await.len(), 3);
}

#[tokio::test]
async fn targets_are_validated() {
    let (app, mut c, csrf, mock, id) = setup().await;
    let cases: &[(&str, &str, &str)] = &[
        ("extruder", "", "between 0 and 500"),
        ("extruder", "999", "between 0 and 500"),
        ("extruder", "-5", "between 0 and 500"),
        ("fan", "50", "no such heater"),
        ("extruder1", "200", "no such heater"),
        ("extruder\nM112", "200", "no such heater"),
        ("heater_generic chamber;M112", "200", "no such heater"),
        (
            "heater_generic chamber\nFIRMWARE_RESTART",
            "200",
            "no such heater",
        ),
        ("SET_HEATER_TEMPERATURE HEATER=extruder", "200", "no such heater"),
    ];
    for (heater, target, msg) in cases {
        let res = c
            .post_hx(
                &format!("/p/{id}/temp"),
                &csrf,
                &[("heater", heater), ("target", target)],
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{heater:?} {target}");
        assert!(res.body.contains(msg), "expected `{msg}` in {}", res.body);
    }
    assert!(mock.scripts().is_empty());

    // Klipper's own limit (max_temp) is reported, and nothing is audited.
    let res = c
        .post_hx(
            &format!("/p/{id}/temp"),
            &csrf,
            &[("heater", "extruder"), ("target", "350")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("out of range"), "{}", res.body);
    assert!(res.headers.get("hx-trigger").is_none(), "the modal stays open");
    assert!(audit_actions(&app, "temp_%").await.is_empty());
}

#[tokio::test]
async fn presets_and_cooldown() {
    let (app, mut admin, csrf, mock, id) = setup().await;
    add_preset(&mut admin, &csrf, "PLA", "215", "60").await;
    add_preset(&mut admin, &csrf, "Bed only", "", "55").await;
    let (mut op, op_csrf) = user(&app, "op", Role::Operator).await;

    let pla = preset_id(&app, "PLA").await;
    let res = op.post_hx(&format!("/p/{id}/preset/{pla}"), &op_csrf, &[]).await;
    assert!(res.body.contains("Preset PLA applied."), "{}", res.body);
    let bed = preset_id(&app, "Bed only").await;
    op.post_hx(&format!("/p/{id}/preset/{bed}"), &op_csrf, &[]).await;
    let res = op.post_hx(&format!("/p/{id}/cooldown"), &op_csrf, &[]).await;
    assert!(res.body.contains("All heaters off."));
    assert_eq!(
        mock.scripts(),
        [
            "SET_HEATER_TEMPERATURE HEATER=extruder TARGET=215\nSET_HEATER_TEMPERATURE HEATER=heater_bed TARGET=60",
            "SET_HEATER_TEMPERATURE HEATER=heater_bed TARGET=55",
            "TURN_OFF_HEATERS",
        ]
    );
    assert_eq!(
        audit_actions(&app, "temp_%").await,
        ["temp_preset", "temp_preset", "temp_cooldown"]
    );
    assert_eq!(
        op.post_hx(&format!("/p/{id}/preset/9999"), &op_csrf, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Viewers can't change temperatures.
    let (mut viewer, v_csrf) = user(&app, "viewer", Role::Viewer).await;
    for path in [
        format!("/p/{id}/temp"),
        format!("/p/{id}/preset/{pla}"),
        format!("/p/{id}/cooldown"),
    ] {
        let res = viewer
            .post_hx(&path, &v_csrf, &[("heater", "extruder"), ("target", "200")])
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(mock.scripts().len(), 3);
}

#[tokio::test]
async fn card_offers_temps_to_operators_only() {
    let (app, _admin, _csrf, _mock, id) = setup().await;
    let (mut op, _) = user(&app, "op", Role::Operator).await;
    let (mut viewer, _) = user(&app, "viewer", Role::Viewer).await;
    let temps = format!(r#"hx-get="/p/{id}/temps""#);
    assert!(op.get_hx(&format!("/p/{id}/status")).await.body.contains(&temps));
    assert!(
        !viewer
            .get_hx(&format!("/p/{id}/status"))
            .await
            .body
            .contains(&temps)
    );
}
