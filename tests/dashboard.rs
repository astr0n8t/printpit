//! The dashboard: lazy loading, the shared live connection, auto-refresh, controls and camera.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use http_body_util::BodyExt;

use common::moonraker::{MockMoonraker, MockOptions};
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};

const ENV: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
    ("PRINTPIT_UI_REFRESH_MS", "50"),
    ("PRINTPIT_WS_IDLE_GRACE_SECS", "1"),
];

async fn setup(opts: MockOptions) -> (TestApp, Client, MockMoonraker, i64) {
    let mock = MockMoonraker::start_opts(opts).await;
    let (app, c) = TestApp::with_admin_env(ENV).await;
    let id = add_printer(&app, "Voron", &mock.url()).await;
    (app, c, mock, id)
}

async fn add_printer(app: &TestApp, name: &str, url: &str) -> i64 {
    printers::create(
        &app.state.db,
        NewPrinter {
            name,
            base_url: url,
            api_key_enc: None,
            webcam_override: None,
            enabled: true,
        },
    )
    .await
    .unwrap()
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn user(app: &TestApp, name: &str, role: Role) -> Client {
    app.create_user(name, "password123", role).await;
    let mut c = app.client();
    c.login(name, "password123").await;
    c
}

// ── Lazy loading ─────────────────────────────────────────────────────

#[tokio::test]
async fn opening_the_dashboard_makes_no_printer_calls() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    for body in [
        c.get("/ui/dashboard").await.body,
        c.get_hx("/ui/dashboard").await.body,
    ] {
        assert!(body.contains(r#"data-sse="/ui/dashboard/stream""#));
        assert!(body.contains(&format!(r#"id="live-{id}""#)));
        assert!(body.contains(&format!("/p/{id}/camera/snapshot")));
        assert!(!body.contains(&mock.url()), "printer address leaked");
        assert!(!body.contains("/webcam/"), "webcam URL leaked");
    }
    // Other tabs are placeholders and stay silent too.
    for tab in ["/ui/files", "/ui/logs", "/ui/system"] {
        c.get_hx(tab).await;
    }
    assert!(
        mock.paths().is_empty(),
        "unexpected printer calls: {:?}",
        mock.paths()
    );
    assert_eq!(mock.ws_total(), 0);
}

#[tokio::test]
async fn live_stream_renders_cards_over_one_shared_connection() {
    let (app, mut admin, mock, id) = setup(MockOptions::default()).await;
    let mut viewer = user(&app, "viewer", Role::Viewer).await;

    let mut a = admin.open_stream("/ui/dashboard/stream").await;
    assert_eq!(a.status, StatusCode::OK);
    let live = a.wait_for(&format!("live-{id}"), "PRINTING").await;
    assert!(live.contains("benchy.gcode") && live.contains("25.0%") && live.contains("layer"));
    assert!(live.contains("extruder") && live.contains("bed"));
    let controls = a.wait_for(&format!("ctl-{id}"), "pause").await;
    assert!(controls.contains("cancel") && controls.contains("E-STOP"));
    // Temperatures keep flowing.
    a.wait_for(&format!("live-{id}"), "°C").await;

    let mut v = viewer.open_stream("/ui/dashboard/stream").await;
    let vlive = v.wait_for(&format!("live-{id}"), "PRINTING").await;
    assert!(!vlive.contains(&mock.url()));
    // Viewers get an (empty) controls event: no buttons.
    let mut saw_controls = false;
    for _ in 0..20 {
        if let Some(e) = v.next_event(Duration::from_secs(2)).await
            && let Some(html) = e.strip_prefix(&format!("ctl-{id}\n"))
        {
            assert!(!html.contains("button"), "viewer got controls: {html}");
            saw_controls = true;
            break;
        }
    }
    assert!(saw_controls);

    assert_eq!(mock.ws_total(), 1, "viewers share one Moonraker connection");
    assert_eq!(mock.ws_open(), 1);
    assert!(
        mock.paths()
            .iter()
            .all(|p| p.starts_with("ws:") || p == "/websocket"),
        "{:?}",
        mock.paths()
    );
    assert_eq!(app.state.hub.active(), vec![id]);
}

#[tokio::test]
async fn connection_closes_after_last_viewer_leaves() {
    let (app, mut c, mock, id) = setup(MockOptions::default()).await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    assert_eq!(mock.ws_open(), 1);
    drop(s);
    // Grace is 1 s in this test.
    eventually("WebSocket closed", || mock.ws_open() == 0).await;
    assert!(app.state.hub.active().is_empty());
}

#[tokio::test]
async fn quick_return_reuses_the_connection() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    drop(s);
    // Back within the grace period (tab switch): same connection.
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(mock.ws_total(), 1);
    assert_eq!(mock.ws_open(), 1);
}

// ── Auto-refresh preference ──────────────────────────────────────────

#[tokio::test]
async fn auto_refresh_off_means_manual_snapshots_only() {
    let (app, mut c, mock, id) = setup(MockOptions::default()).await;
    let csrf = c.session_csrf().await;

    let res = c.post_hx("/account/prefs", &csrf, &[]).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(!res.body.contains("data-sse"));
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/status" hx-trigger="load""#))
    );
    assert!(!res.body.contains("checked"));

    let s = c.open_stream("/ui/dashboard/stream").await;
    assert_eq!(
        s.status,
        StatusCode::NO_CONTENT,
        "no stream when auto-refresh is off"
    );

    let status = c.get_hx(&format!("/p/{id}/status")).await;
    assert!(status.body.contains("PRINTING") && status.body.contains("refresh [r]"));
    assert!(
        status
            .body
            .contains(&format!(r#"id="ctl-{id}" class="controls" hx-swap-oob="true""#))
    );
    assert_eq!(mock.ws_total(), 0);
    assert_eq!(
        mock.paths(),
        ["/server/info", "/printer/objects/list", "/printer/objects/query"]
    );
    assert!(app.state.hub.active().is_empty());

    // Remembered across page loads, and can be switched back on.
    assert!(!c.get("/ui/dashboard").await.body.contains("data-sse"));
    let res = c
        .post_hx("/account/prefs", &csrf, &[("auto_refresh", "on")])
        .await;
    assert!(res.body.contains("data-sse") && res.body.contains("checked"));
}

#[tokio::test]
async fn prefs_are_per_user() {
    let (app, mut admin, _mock, _id) = setup(MockOptions::default()).await;
    let csrf = admin.session_csrf().await;
    admin.post_hx("/account/prefs", &csrf, &[]).await;
    let mut other = user(&app, "other", Role::Viewer).await;
    assert!(other.get("/ui/dashboard").await.body.contains("data-sse"));
}

// ── Printer states ───────────────────────────────────────────────────

#[tokio::test]
async fn klipper_not_ready_is_shown() {
    let (_app, mut c, _mock, id) = setup(MockOptions {
        klippy_state: "shutdown",
        ..MockOptions::default()
    })
    .await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    let live = s.wait_for(&format!("live-{id}"), "SHUTDOWN").await;
    assert!(live.contains("Timer too close"));
    let controls = s.wait_for(&format!("ctl-{id}"), "E-STOP").await;
    assert!(
        !controls.contains("pause"),
        "only e-stop while Klipper isn't ready"
    );
}

#[tokio::test]
async fn offline_printer_is_shown() {
    let (app, mut c) = TestApp::with_admin_env(ENV).await;
    let id = add_printer(&app, "Gone", "http://127.0.0.1:1").await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    let live = s.wait_for(&format!("live-{id}"), "OFFLINE").await;
    assert!(
        live.contains("can&#39;t reach Moonraker") || live.contains("can't reach Moonraker"),
        "{live}"
    );
    assert!(!live.contains("127.0.0.1"));
}

#[tokio::test]
async fn falls_back_to_http_polling_without_websocket() {
    let (_app, mut c, mock, id) = setup(MockOptions {
        no_websocket: true,
        ..MockOptions::default()
    })
    .await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    let live = s.wait_for(&format!("live-{id}"), "(polling)").await;
    assert!(live.contains("PRINTING"));
    assert!(mock.paths().contains(&"/printer/objects/query".to_string()));
}

#[tokio::test]
async fn disabled_printers_are_left_out() {
    let (app, mut c, mock, id) = setup(MockOptions::default()).await;
    printers::set_enabled(&app.state.db, id, false).await.unwrap();
    assert!(!c.get("/ui/dashboard").await.body.contains(&format!("live-{id}")));
    assert_eq!(
        c.get_hx(&format!("/p/{id}/status")).await.status,
        StatusCode::BAD_REQUEST
    );
    assert!(mock.paths().is_empty());
}

#[tokio::test]
async fn editing_a_printer_reconnects() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let csrf = c.session_csrf().await;
    let mut s = c.open_stream("/ui/dashboard/stream").await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    c.post_hx(
        &format!("/printers/{id}"),
        &csrf,
        &[("name", "Voron"), ("base_url", &mock.url()), ("enabled", "on")],
    )
    .await;
    s.wait_for(&format!("live-{id}"), "PRINTING").await;
    eventually("reconnected", || mock.ws_total() == 2).await;
}

// ── Controls ─────────────────────────────────────────────────────────

#[tokio::test]
async fn operator_controls_reach_moonraker_and_are_audited() {
    let (app, _admin, mock, id) = setup(MockOptions::default()).await;
    let mut op = user(&app, "op", Role::Operator).await;
    let csrf = op.session_csrf().await;

    let res = op.post_hx(&format!("/p/{id}/print/pause"), &csrf, &[]).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Pause sent."));
    assert_eq!(res.header("hx-trigger"), "printpit:close-modal");
    assert_eq!(mock.print_state(), "paused");

    op.post_hx(&format!("/p/{id}/print/resume"), &csrf, &[]).await;
    assert_eq!(mock.print_state(), "printing");
    op.post_hx(&format!("/p/{id}/print/cancel"), &csrf, &[]).await;
    assert_eq!(mock.print_state(), "cancelled");

    let res = op
        .post_hx(
            &format!("/p/{id}/print/start"),
            &csrf,
            &[("filename", "parts/clip.gcode")],
        )
        .await;
    assert!(res.body.contains("Print started."), "{}", res.body);
    assert_eq!(mock.print_state(), "printing");

    let res = op.post_hx(&format!("/p/{id}/estop"), &csrf, &[]).await;
    assert!(res.body.contains("Emergency stop sent."));

    let actions: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT action, detail FROM audit_log WHERE printer_id = ? ORDER BY id")
            .bind(id)
            .fetch_all(&app.state.db)
            .await
            .unwrap();
    let names: Vec<&str> = actions.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(
        names,
        [
            "print_pause",
            "print_resume",
            "print_cancel",
            "print_start",
            "emergency_stop"
        ]
    );
    assert_eq!(actions[3].1.as_deref(), Some("file=parts/clip.gcode"));
}

#[tokio::test]
async fn controls_are_validated_and_role_gated() {
    let (app, mut admin, mock, id) = setup(MockOptions::default()).await;
    let csrf = admin.session_csrf().await;
    for bad in ["", "../secret.gcode", "/abs.gcode"] {
        let res = admin
            .post_hx(&format!("/p/{id}/print/start"), &csrf, &[("filename", bad)])
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    assert_eq!(
        admin
            .post_hx(&format!("/p/{id}/print/explode"), &csrf, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let mut viewer = user(&app, "viewer", Role::Viewer).await;
    let vcsrf = viewer.session_csrf().await;
    for path in [format!("/p/{id}/print/pause"), format!("/p/{id}/estop")] {
        assert_eq!(
            viewer.post_hx(&path, &vcsrf, &[]).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        viewer.get_hx(&format!("/p/{id}/start")).await.status,
        StatusCode::FORBIDDEN
    );
    // Missing CSRF is refused before anything reaches the printer.
    assert_eq!(
        admin.post(&format!("/p/{id}/print/pause"), &[]).await.status,
        StatusCode::FORBIDDEN
    );
    assert!(mock.paths().is_empty(), "{:?}", mock.paths());
}

#[tokio::test]
async fn start_picker_lists_newest_files_first() {
    let (_app, mut c, _mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/start")).await;
    assert_eq!(res.status, StatusCode::OK);
    let clip = res.body.find("parts/clip.gcode").unwrap();
    let old = res.body.find("old.gcode").unwrap();
    assert!(clip < old);
    assert!(res.body.contains("2.4 MB"));
    assert!(
        res.body
            .contains(r#"hx-confirm="Start parts/clip.gcode on Voron?""#)
    );
}

// ── Camera ───────────────────────────────────────────────────────────

#[tokio::test]
async fn camera_snapshot_and_stream_are_proxied() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;

    let snap = c.get(&format!("/p/{id}/camera/snapshot")).await;
    assert_eq!(snap.status, StatusCode::OK);
    assert_eq!(snap.header("content-type"), "image/jpeg");
    assert_eq!(snap.header("cache-control"), "no-store");
    assert!(snap.body.contains("fake-jpeg"));
    // A second snapshot reuses the cached webcam lookup.
    c.get(&format!("/p/{id}/camera/snapshot")).await;
    assert_eq!(
        mock.paths()
            .iter()
            .filter(|p| *p == "/server/webcams/list")
            .count(),
        1
    );

    let mut stream = c.open_stream(&format!("/p/{id}/camera/stream")).await;
    assert_eq!(stream.status, StatusCode::OK);
    let mut got = Vec::new();
    while got.len() < 100 {
        let frame = stream_frame(&mut stream).await;
        got.extend_from_slice(&frame);
    }
    assert!(String::from_utf8_lossy(&got).contains("--frame"));
    assert_eq!(mock.streams_open(), 1);
    drop(stream);
    eventually("upstream camera stream closed", || mock.streams_open() == 0).await;
}

/// Reads one raw body frame from a streaming response.
async fn stream_frame(s: &mut common::SseStream) -> Vec<u8> {
    let frame = tokio::time::timeout(Duration::from_secs(5), s.body_mut().frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    frame.into_data().map(|b| b.to_vec()).unwrap_or_default()
}

#[tokio::test]
async fn no_camera_is_a_404() {
    let (_app, mut c, _mock, id) = setup(MockOptions {
        webcam: false,
        ..MockOptions::default()
    })
    .await;
    assert_eq!(
        c.get(&format!("/p/{id}/camera/snapshot")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.get(&format!("/p/{id}/camera/stream")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn unauthenticated_event_stream_gets_401() {
    let (app, _c, _mock, _id) = setup(MockOptions::default()).await;
    let mut anon = app.client();
    let s = anon.open_stream("/ui/dashboard/stream").await;
    assert_eq!(
        s.status,
        StatusCode::UNAUTHORIZED,
        "EventSource must not be redirected"
    );
}
