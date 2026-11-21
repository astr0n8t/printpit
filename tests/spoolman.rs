//! Spoolman: printpit books finished jobs' filament from Moonraker's history to the spools an
//! operator loaded. The printer never talks to Spoolman, every job is booked once, and the
//! Spools tab stays lazy.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use common::moonraker::{MockMoonraker, MockOptions};
use common::spoolman::MockSpoolman;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};
use printpit::db::spoolman as store;

const TOKEN: &str = "proxy-T0KEN-s3cret";

struct Setup {
    app: TestApp,
    admin: Client,
    csrf: String,
    moonraker: MockMoonraker,
    spoolman: MockSpoolman,
    id: i64,
}

async fn setup() -> Setup {
    let moonraker = MockMoonraker::start().await;
    let spoolman = MockSpoolman::start().await;
    spoolman.require_token(TOKEN);
    let url = spoolman.url();
    let (app, mut admin) = TestApp::with_admin_env(&[
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
        ("PRINTPIT_UI_REFRESH_MS", "50"),
        ("PRINTPIT_WS_IDLE_GRACE_SECS", "1"),
        ("PRINTPIT_SPOOLMAN_URL", &url),
        ("PRINTPIT_SPOOLMAN_TOKEN", TOKEN),
    ])
    .await;
    let id = printers::create(
        &app.state.db,
        NewPrinter {
            name: "Voron",
            base_url: &moonraker.url(),
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
    let sp = app.state.spoolman.clone().unwrap();
    sp.set_settle_delay(Duration::from_millis(50));
    app.state.start_spoolman().await;
    let csrf = admin.session_csrf().await;
    Setup {
        app,
        admin,
        csrf,
        moonraker,
        spoolman,
        id,
    }
}

impl Setup {
    async fn track(&mut self, on: bool, tools: &str) -> String {
        let mut fields = vec![("tools", tools)];
        if on {
            fields.push(("tracking", "on"));
        }
        let res = self
            .admin
            .post_hx(&format!("/p/{}/spools/settings", self.id), &self.csrf, &fields)
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        res.body
    }

    async fn assign(&mut self, fields: &[(&str, &str)]) -> String {
        let res = self
            .admin
            .post_hx(&format!("/p/{}/spools/assign", self.id), &self.csrf, fields)
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        res.body
    }

    async fn sync(&mut self) -> String {
        let res = self
            .admin
            .post_hx(&format!("/p/{}/spools/sync", self.id), &self.csrf, &[])
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        res.body
    }

    /// A job that started just now (after tracking was turned on).
    fn job(&self, extra: Value) -> String {
        let now = printpit::db::now() as f64;
        let mut job = json!({"status": "completed", "start_time": now + 1.0, "end_time": now + 2.0,
                             "filament_used": 1520.5});
        for (k, v) in extra.as_object().unwrap() {
            job[k] = v.clone();
        }
        self.moonraker.add_job(job)
    }

    fn history_calls(&self) -> usize {
        self.moonraker
            .paths()
            .iter()
            .filter(|p| p.starts_with("/server/history/list"))
            .count()
    }

    async fn audit(&self, action: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT COALESCE(detail, '') FROM audit_log WHERE action = ? ORDER BY id")
            .bind(action)
            .fetch_all(&self.app.state.db)
            .await
            .unwrap()
    }
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

#[tokio::test]
async fn off_without_a_spoolman_url() {
    let (app, mut c) = TestApp::with_admin().await;
    assert!(app.state.spoolman.is_none());
    let dash = c.get("/ui/dashboard").await.body;
    assert!(!dash.contains("/ui/spools"), "no tab");
    let csrf = c.session_csrf().await;
    assert_eq!(c.get("/ui/spools").await.status, StatusCode::NOT_FOUND);
    assert_eq!(c.get_hx("/p/1/spools").await.status, StatusCode::NOT_FOUND);
    for path in ["/p/1/spools/sync", "/p/1/spools/assign", "/p/1/spools/settings"] {
        let res = c.post_hx(path, &csrf, &[("tools", "1")]).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn the_tab_is_lazy_and_keeps_addresses_out() {
    let mut s = setup().await;
    let tab = s.admin.get("/ui/spools").await;
    assert_eq!(tab.status, StatusCode::OK);
    assert!(tab.body.contains("[7]"), "the tab is in the strip");
    assert!(tab.body.contains(r#"hx-get="/p/1/spools""#));
    assert!(
        s.spoolman.calls().is_empty(),
        "the tab itself doesn't ask Spoolman"
    );
    assert!(s.moonraker.paths().is_empty(), "nor the printer");

    let panel = s.admin.get_hx(&format!("/p/{}/spools", s.id)).await.body;
    assert!(
        panel.contains("#2 Acme Prusament Galaxy PETG · 650 g left · shelf"),
        "{panel}"
    );
    let calls = s.spoolman.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/spoolman/api/v1/spool");
    assert_eq!(calls[0].auth.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
    assert!(
        s.moonraker.paths().is_empty(),
        "the picker doesn't touch the printer"
    );
    for page in [&tab.body, &panel] {
        assert!(!page.contains(&s.spoolman.url()) && !page.contains(TOKEN));
        assert!(!page.contains(&format!(":{}", s.spoolman.port)));
    }

    // Spoolman down: the picker says so, and offers no form.
    s.spoolman.set_down(true);
    let panel = s.admin.get_hx(&format!("/p/{}/spools", s.id)).await.body;
    assert!(panel.contains("! ERROR: Spoolman returned 502"), "{panel}");
    assert!(!panel.contains("<form"));
}

#[tokio::test]
async fn finished_jobs_are_booked_once() {
    let mut s = setup().await;
    let body = s.track(true, "1").await;
    assert!(body.contains("Tracking filament for Voron"), "{body}");
    s.assign(&[("tool0", "1")]).await;
    let job = s.job(json!({}));
    s.job(json!({"filament_used": 0.0, "status": "cancelled"}));

    let body = s.sync().await;
    assert!(body.contains("Booked 1 job, 1 with nothing to book."), "{body}");
    assert_eq!(s.spoolman.uses(), vec![(1, json!({"use_length": 1520.5}))]);
    assert!(body.contains("BOOKED") && body.contains("#1 1.52 m"), "{body}");
    assert!(body.contains("SKIPPED") && body.contains("No filament used."));

    // The 60 old jobs in the mock's history started before tracking: never booked.
    let body = s.sync().await;
    assert!(body.contains("Nothing new to book."), "{body}");
    assert_eq!(s.spoolman.uses().len(), 1);
    let rows = store::recent(&s.app.state.db, s.id, 100).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].job_id, job);

    let audit = s.audit("spoolman_use").await;
    assert_eq!(audit, [format!("job {job} benchy.gcode: #1 1.52 m")]);
    assert_eq!(s.audit("spoolman_settings").await, ["tracking=on tools=1"]);
    assert_eq!(s.audit("spoolman_assign").await, ["T0=#1 (was T0=-)"]);
    assert_eq!(s.audit("spoolman_sync").await.len(), 2);
}

#[tokio::test]
async fn a_print_ending_is_booked_without_anyone_looking() {
    let mut s = setup().await;
    s.track(true, "1").await;
    s.assign(&[("tool0", "2")]).await;
    let sp = s.app.state.spoolman.clone().unwrap();
    assert_eq!(sp.watching().await, vec![s.id]);
    // The watcher holds the connection open with no viewers.
    eventually("connected", || s.moonraker.ws_open() == 1).await;

    let job = s.job(json!({"status": "in_progress", "end_time": null}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.spoolman.uses().is_empty());
    s.moonraker
        .update_job(&job, json!({"status": "completed", "filament_used": 800.0}));
    s.moonraker.set_print_state("complete");
    eventually("booked", || !s.spoolman.uses().is_empty()).await;
    assert_eq!(s.spoolman.uses(), vec![(2, json!({"use_length": 800.0}))]);
    // The second read a little later books nothing more.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(s.spoolman.uses().len(), 1);

    // Off: the watch and the connection go.
    s.track(false, "1").await;
    assert!(sp.watching().await.is_empty());
    eventually("disconnected", || s.moonraker.ws_open() == 0).await;
}

#[tokio::test]
async fn untracked_printers_stay_lazy() {
    let s = setup().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.app.state.spoolman.clone().unwrap().watching().await.is_empty());
    assert!(s.moonraker.paths().is_empty());
    // A disabled printer isn't watched even with tracking on.
    store::set_settings(&s.app.state.db, s.id, true, 1).await.unwrap();
    sqlx::query("UPDATE printers SET enabled = 0")
        .execute(&s.app.state.db)
        .await
        .unwrap();
    s.app.state.printer_changed(s.id);
    s.app.state.start_spoolman().await;
    assert!(s.app.state.spoolman.clone().unwrap().watching().await.is_empty());
}

#[tokio::test]
async fn a_running_job_holds_the_cursor() {
    let mut s = setup().await;
    s.track(true, "1").await;
    s.assign(&[("tool0", "1")]).await;
    let t0 = store::settings(&s.app.state.db, s.id).await.unwrap().since;
    let running = s.job(json!({"status": "in_progress", "end_time": null, "start_time": t0 + 10.0}));
    s.job(json!({"filament_used": 100.0, "start_time": t0 + 20.0}));
    s.sync().await;
    assert_eq!(s.spoolman.uses(), vec![(1, json!({"use_length": 100.0}))]);
    let since = store::settings(&s.app.state.db, s.id).await.unwrap().since;
    assert_eq!(since, t0, "not moved past the running job");

    s.moonraker
        .update_job(&running, json!({"status": "cancelled", "filament_used": 50.0}));
    s.sync().await;
    assert_eq!(s.spoolman.uses().len(), 2);
    assert_eq!(s.spoolman.uses()[1], (1, json!({"use_length": 50.0})));
    let since = store::settings(&s.app.state.db, s.id).await.unwrap().since;
    assert_eq!(
        since,
        t0 + 19.0,
        "moved on once settled (a second behind the last job)"
    );
    // Jobs read again near the cursor aren't booked twice.
    s.sync().await;
    assert_eq!(s.spoolman.uses().len(), 2);
}

#[tokio::test]
async fn multi_tool_jobs_are_split_by_the_slicers_weights() {
    let mut s = setup().await;
    s.track(true, "3").await;
    let panel = s.admin.get_hx(&format!("/p/{}/spools", s.id)).await.body;
    assert!(
        panel.contains(r#"name="tool2""#) && panel.contains(">T2<"),
        "{panel}"
    );
    s.assign(&[("tool0", "1"), ("tool1", "2"), ("tool2", "")]).await;
    // Cancelled halfway: half of each tool's planned weight.
    let meta = json!({"filament_total": 4000.0, "filament_weights": [6.0, 2.0, 1.0]});
    s.job(json!({"status": "cancelled", "filament_used": 2000.0, "metadata": meta}));
    // Used only T1.
    let meta = json!({"filament_total": 900.0, "filament_weights": [0.0, 3.0]});
    s.job(json!({"filament_used": 900.0, "metadata": meta}));

    let body = s.sync().await;
    assert_eq!(
        s.spoolman.uses(),
        vec![
            (1, json!({"use_weight": 3.0})),
            (2, json!({"use_weight": 1.0})),
            (2, json!({"use_length": 900.0})),
        ]
    );
    assert!(body.contains("T0 → #1 3.0 g, T1 → #2 1.0 g"), "{body}");
    assert!(
        body.contains("T2 has no spool assigned; 0.5 g not booked."),
        "{body}"
    );
    assert!(body.contains("50% of the planned filament"));

    // Fewer tools drops the extra slots.
    s.track(true, "1").await;
    assert_eq!(
        store::slots(&s.app.state.db, s.id, 3).await.unwrap(),
        [Some(1), None, None]
    );
}

#[tokio::test]
async fn spoolman_being_down_is_retried_and_refusals_are_not() {
    let mut s = setup().await;
    s.track(true, "1").await;
    s.assign(&[("tool0", "1")]).await;
    s.job(json!({}));
    s.spoolman.fail_next_uses(1);
    let body = s.sync().await;
    assert!(body.contains("1 not taken by Spoolman."), "{body}");
    assert!(
        body.contains("RETRYING") && body.contains("database is locked"),
        "{body}"
    );
    assert!(s.spoolman.uses().is_empty());

    let body = s.sync().await;
    assert!(body.contains("BOOKED"), "{body}");
    assert_eq!(s.spoolman.uses().len(), 1);

    // A spool that's gone fails for good.
    s.spoolman.remove_spool(1);
    s.job(json!({}));
    let body = s.sync().await;
    assert!(
        body.contains("FAILED") && body.contains("spool #1 isn&#39;t in Spoolman"),
        "{body}"
    );
    let calls = s.spoolman.calls().len();
    s.sync().await;
    assert_eq!(s.spoolman.calls().len(), calls, "not retried");
}

#[tokio::test]
async fn changing_spools_books_finished_jobs_to_the_old_ones() {
    let mut s = setup().await;
    s.track(true, "1").await;
    s.assign(&[("tool0", "1")]).await;
    s.job(json!({"filament_used": 300.0}));
    let before = s.history_calls();
    let body = s.assign(&[("tool0", "3")]).await;
    assert!(body.contains("Saved the spools loaded in Voron."), "{body}");
    assert_eq!(s.history_calls(), before + 1);
    assert_eq!(s.spoolman.uses(), vec![(1, json!({"use_length": 300.0}))]);
    s.job(json!({"filament_used": 200.0}));
    s.sync().await;
    assert_eq!(s.spoolman.uses()[1], (3, json!({"use_length": 200.0})));
}

#[tokio::test]
async fn history_off_and_printer_offline_are_reported() {
    let mut s = setup().await;
    s.track(true, "1").await;
    sqlx::query("UPDATE printers SET base_url = 'http://127.0.0.1:1'")
        .execute(&s.app.state.db)
        .await
        .unwrap();
    let body = s.sync().await;
    assert!(body.contains("! OFFLINE"), "{body}");
    assert!(!body.contains("127.0.0.1:1"), "no printer address");
    let st = store::settings(&s.app.state.db, s.id).await.unwrap();
    assert!(st.sync_error.unwrap().starts_with("OFFLINE"));
    let tab = s.admin.get_hx("/ui/spools").await.body;
    assert!(tab.contains("! last sync"), "{tab}");

    // Moonraker without [history].
    let no_history = MockMoonraker::start_opts(MockOptions {
        no_history: true,
        ..MockOptions::default()
    })
    .await;
    sqlx::query("UPDATE printers SET base_url = ?")
        .bind(no_history.url())
        .execute(&s.app.state.db)
        .await
        .unwrap();
    let body = s.sync().await;
    assert!(body.contains("Job history is off on this printer"), "{body}");
}

#[tokio::test]
async fn roles() {
    let mut s = setup().await;
    s.track(true, "1").await;
    s.app.create_user("viewer", "viewer password", Role::Viewer).await;
    s.app.create_user("op", "operator password", Role::Operator).await;

    let mut viewer = s.app.client();
    viewer.login("viewer", "viewer password").await;
    let csrf = viewer.session_csrf().await;
    let tab = viewer.get_hx("/ui/spools").await.body;
    assert!(!tab.contains("sync now") && !tab.contains("save tracking"));
    let panel = viewer.get_hx(&format!("/p/{}/spools", s.id)).await.body;
    assert!(!panel.contains("<form"), "{panel}");
    for path in ["assign", "sync", "settings"] {
        let res = viewer
            .post_hx(&format!("/p/{}/spools/{path}", s.id), &csrf, &[("tools", "1")])
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }

    let mut op = s.app.client();
    op.login("op", "operator password").await;
    let csrf = op.session_csrf().await;
    let tab = op.get_hx("/ui/spools").await.body;
    assert!(tab.contains("sync now") && !tab.contains("save tracking"));
    let res = op
        .post_hx(&format!("/p/{}/spools/assign", s.id), &csrf, &[("tool0", "2")])
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = op
        .post_hx(&format!("/p/{}/spools/settings", s.id), &csrf, &[("tools", "2")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // Bad input.
    let res = op
        .post_hx(&format!("/p/{}/spools/assign", s.id), &csrf, &[("tool0", "abc")])
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = s
        .admin
        .post_hx(
            &format!("/p/{}/spools/settings", s.id),
            &s.csrf,
            &[("tools", "99")],
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn interrupted_bookings_are_not_retried() {
    let s = setup().await;
    let db = &s.app.state.db;
    store::set_settings(db, s.id, true, 1).await.unwrap();
    let id = store::insert_booking(
        db,
        store::NewBooking {
            printer_id: s.id,
            job_id: "0000AA",
            job_start: 1.0,
            filename: "x.gcode",
            job_status: "completed",
            job_end: Some(2.0),
            used_mm: 10.0,
            status: "sending",
            parts: &[],
            note: "",
        },
    )
    .await
    .unwrap()
    .unwrap();
    // The same job again is refused.
    let again = store::insert_booking(
        db,
        store::NewBooking {
            printer_id: s.id,
            job_id: "0000AA",
            job_start: 1.0,
            filename: "x.gcode",
            job_status: "completed",
            job_end: Some(2.0),
            used_mm: 10.0,
            status: "sending",
            parts: &[],
            note: "",
        },
    )
    .await
    .unwrap();
    assert!(again.is_none());
    s.app.state.start_spoolman().await;
    let rows = store::recent(db, s.id, 10).await.unwrap();
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].status, "unknown");
    assert!(store::failed(db, s.id).await.unwrap().is_empty());
}
