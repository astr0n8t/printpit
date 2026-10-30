//! The System tab: lazy loading, static info vs live stats, auto-refresh gating, privacy.

mod common;

use axum::http::StatusCode;

use common::moonraker::{MockMoonraker, MockOptions};
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};

const ENV: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
    ("PRINTPIT_SYSTEM_REFRESH_SECS", "7"),
];

async fn setup(opts: MockOptions) -> (TestApp, Client, MockMoonraker, i64) {
    let mock = MockMoonraker::start_opts(opts).await;
    let (app, c) = TestApp::with_admin_env(ENV).await;
    let id = printers::create(
        &app.state.db,
        NewPrinter {
            name: "Voron",
            base_url: &mock.url(),
            api_key_enc: None,
            webcam_override: None,
            enabled: true,
        },
    )
    .await
    .unwrap();
    (app, c, mock, id)
}

async fn viewer(app: &TestApp) -> Client {
    app.create_user("viewer", "password123", Role::Viewer).await;
    let mut v = app.client();
    v.login("viewer", "password123").await;
    v
}

#[tokio::test]
async fn opening_the_tab_makes_no_printer_calls() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get("/ui/system").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/system/stats" hx-trigger="load""#))
    );
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/system/info" hx-trigger="load""#))
    );
    assert!(mock.paths().is_empty());
}

#[tokio::test]
async fn live_stats_render_meters() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/system/stats")).await;
    assert_eq!(res.status, StatusCode::OK);
    let b = &res.body;
    assert!(b.contains(r#"<span class="meter-label dim">cpu</span>"#) && b.contains("42%"));
    assert!(b.contains("976.6 MB / 3.7 GB"), "memory used = total - available");
    assert!(b.contains("9.3 GB / 27.9 GB"), "disk from the gcodes root");
    assert!(b.contains(r#"<span class="bar err">"#), "cpu3 at 95% is red");
    assert!(b.contains("52.3 °C") && b.contains("12d 0h"));
    assert!(b.contains("Previously Under-Volted"));
    assert!(b.contains("cpu 2.5% · mem 40.0 MB"));
    assert!(b.contains("wlan0") && b.contains("2.0 KB/s") && !b.contains(">lo<"));
    // Live stats don't fetch the static info (for non-admins it isn't needed at all).
    let paths = mock.paths();
    assert!(paths.contains(&"/machine/proc_stats".to_string()));
    assert!(!paths.contains(&"/machine/update/status".to_string()));
}

#[tokio::test]
async fn static_info_renders_host_services_and_updates() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/system/info")).await;
    let b = &res.body;
    for want in [
        "Raspberry Pi 4 Model B Rev 1.4",
        "Cortex-A72 × 4",
        "3.7 GB",
        "Debian GNU/Linux 12 (bookworm)",
        "SanDisk SN32G 29.7 GiB",
        "3.11.2",
        "v0.12.0-301-g1f8e3c3d",
        "v0.9.3-12",
    ] {
        assert!(b.contains(want), "missing {want}");
    }
    assert!(b.contains(r#"<td>crowsnest</td><td><span class="badge badge-err">FAILED</span>"#));
    assert!(b.contains(r#"<td>klipper</td><td><span class="badge badge-ok">RUNNING</span>"#));
    assert!(b.contains("v0.9.3-20") && b.contains(">UPDATE<") && b.contains("2 upgradable"));
    assert!(!b.contains("SHOULD-NOT-SHOW"), "serial numbers are never shown");
    assert!(
        mock.paths().contains(&"/machine/update/status".to_string()),
        "update status read (refresh=false)"
    );
}

#[tokio::test]
async fn host_addresses_are_admin_only() {
    let (app, mut admin, _mock, id) = setup(MockOptions::default()).await;
    let a_stats = admin.get_hx(&format!("/p/{id}/system/stats")).await.body;
    let a_info = admin.get_hx(&format!("/p/{id}/system/info")).await.body;
    assert!(a_stats.contains("10.20.0.11"), "admins see interface addresses");
    assert!(!a_stats.contains("fe80::1"), "link-local addresses are skipped");
    assert!(a_info.contains("voron"), "admins see the hostname");

    let mut v = viewer(&app).await;
    let v_stats = v.get_hx(&format!("/p/{id}/system/stats")).await.body;
    let v_info = v.get_hx(&format!("/p/{id}/system/info")).await.body;
    assert!(v_stats.contains("wlan0"));
    for body in [&v_stats, &v_info] {
        assert!(
            !body.contains("10.20.0.11") && !body.contains("dc:a6:32"),
            "no addresses for viewers"
        );
    }
    assert!(!v_info.contains("<dt>hostname</dt>"));
}

#[tokio::test]
async fn auto_refresh_pref_controls_the_timer() {
    let (_app, mut c, _mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/system/stats")).await;
    assert!(
        res.body.contains(r#"data-every="7000""#),
        "auto-refresh on by default"
    );

    let csrf = c.session_csrf().await;
    c.post_hx("/account/prefs", &csrf, &[]).await;
    let res = c.get_hx(&format!("/p/{id}/system/stats")).await;
    assert!(!res.body.contains("data-every"), "no timer with auto-refresh off");
    assert!(res.body.contains("manual (auto-refresh is off)"));
    assert!(res.body.contains("data-refresh"), "manual refresh still there");
    // The static half never auto-refreshes.
    let info = c.get_hx(&format!("/p/{id}/system/info")).await;
    assert!(!info.body.contains("data-every"));
}

#[tokio::test]
async fn missing_update_manager_and_offline_printer() {
    let (app, mut c, _mock, id) = setup(MockOptions {
        no_update_manager: true,
        ..MockOptions::default()
    })
    .await;
    let res = c.get_hx(&format!("/p/{id}/system/info")).await;
    assert!(
        res.body.contains("The update manager isn&#39;t enabled.")
            || res.body.contains("update manager isn't enabled")
    );
    assert!(res.body.contains("Raspberry Pi 4"), "the rest still renders");

    let gone = printers::create(
        &app.state.db,
        NewPrinter {
            name: "Gone",
            base_url: "http://127.0.0.1:1",
            api_key_enc: None,
            webcam_override: None,
            enabled: true,
        },
    )
    .await
    .unwrap();
    for part in ["stats", "info"] {
        let res = c.get_hx(&format!("/p/{gone}/system/{part}")).await;
        assert_eq!(res.status, StatusCode::OK);
        assert!(res.body.contains("! OFFLINE"), "{part}: {}", res.body);
        assert!(!res.body.contains("127.0.0.1"));
    }
}
