//! Print notifications: opt-in per printer, the permanent connection that implies, edge
//! detection, retries, and keeping the webhook URL out of pages.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::Value;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::db::printers::{self, NewPrinter};
use printpit::moonraker::status::Link;

const TOKEN: &str = "T0KEN-sekr1t";

/// A webhook endpoint that records bodies and fails the first `fail_first` requests.
#[derive(Clone)]
struct Receiver {
    bodies: Arc<Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
    fail_first: usize,
    url: String,
}

impl Receiver {
    async fn start(fail_first: usize) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let r = Receiver {
            bodies: Arc::default(),
            calls: Arc::default(),
            fail_first,
            url: format!("http://127.0.0.1:{port}/hooks/{TOKEN}"),
        };
        let app = Router::new()
            .route(&format!("/hooks/{TOKEN}"), post(receive))
            .with_state(r.clone());
        tokio::spawn(axum::serve(listener, app).into_future());
        r
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

async fn receive(State(r): State<Receiver>, axum::Json(body): axum::Json<Value>) -> StatusCode {
    if r.calls.fetch_add(1, Ordering::SeqCst) < r.fail_first {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    r.bodies.lock().unwrap().push(body);
    StatusCode::NO_CONTENT
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn setup(webhook: Option<&str>, notify: bool) -> (TestApp, Client, MockMoonraker, i64) {
    let mock = MockMoonraker::start().await;
    let mut env = vec![
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
        ("PRINTPIT_UI_REFRESH_MS", "50"),
        ("PRINTPIT_WS_IDLE_GRACE_SECS", "1"),
    ];
    if let Some(url) = webhook {
        env.push(("PRINTPIT_NOTIFY_WEBHOOK_URL", url));
    }
    let (app, c) = TestApp::with_admin_env(&env).await;
    let id = printers::create(
        &app.state.db,
        NewPrinter {
            name: "Voron",
            base_url: &mock.url(),
            api_key_enc: None,
            webcam_override: None,
            webcam_transform: None,
            enabled: true,
            notify,
        },
    )
    .await
    .unwrap();
    app.state.start_notifications().await;
    (app, c, mock, id)
}

/// Waits until the watcher has seen a live snapshot (its baseline).
async fn wait_live(app: &TestApp, id: i64) {
    let hub = app.state.hub.clone();
    eventually("live connection", async || {
        hub.peek(id).is_some_and(|s| s.link == Link::Live)
    })
    .await;
    // One more update so the watcher has certainly observed it.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn finished_print_is_reported_once() {
    let hook = Receiver::start(0).await;
    let (app, _c, mock, id) = setup(Some(&hook.url), true).await;
    assert_eq!(app.state.notifier.watching().await, vec![id]);
    wait_live(&app, id).await;
    assert!(
        hook.bodies().is_empty(),
        "the state at startup is only a baseline"
    );

    mock.set_print_state("complete");
    eventually("webhook delivery", async || !hook.bodies().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bodies = hook.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let b = &bodies[0];
    assert_eq!(b["printer"], "Voron");
    assert_eq!(b["event"], "complete");
    assert_eq!(b["filename"], "benchy.gcode");
    assert_eq!(b["text"], "Voron: finished benchy.gcode in 10:00");
    assert!(!b.to_string().contains(&mock.url()), "no printer address");

    // Another print, cancelled this time.
    mock.set_print_state("printing");
    tokio::time::sleep(Duration::from_millis(300)).await;
    mock.set_print_state("cancelled");
    eventually("second delivery", async || hook.bodies().len() == 2).await;
    assert_eq!(hook.bodies()[1]["event"], "cancelled");
}

#[tokio::test]
async fn watched_printer_stays_connected_without_viewers() {
    let hook = Receiver::start(0).await;
    let (app, _c, mock, id) = setup(Some(&hook.url), true).await;
    wait_live(&app, id).await;
    // Well past the 1 s idle grace, with nobody looking at the dashboard.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(mock.ws_open(), 1);
    assert_eq!(app.state.hub.active(), vec![id]);

    // Turning notifications off lets the connection go.
    sqlx::query("UPDATE printers SET notify = 0 WHERE id = ?")
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();
    app.state.printer_changed(id);
    let notifier = app.state.notifier.clone();
    eventually("watch stopped", async || notifier.watching().await.is_empty()).await;
    eventually("connection closed", async || mock.ws_open() == 0).await;
}

#[tokio::test]
async fn only_enabled_opted_in_printers_are_watched() {
    let hook = Receiver::start(0).await;
    let (app, _c, mock, id) = setup(Some(&hook.url), false).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(app.state.notifier.watching().await.is_empty());
    assert!(mock.paths().is_empty(), "printers without notify stay lazy");

    sqlx::query("UPDATE printers SET notify = 1, enabled = 0 WHERE id = ?")
        .bind(id)
        .execute(&app.state.db)
        .await
        .unwrap();
    app.state.start_notifications().await;
    assert!(app.state.notifier.watching().await.is_empty(), "disabled");
}

#[tokio::test]
async fn without_a_webhook_nothing_is_watched() {
    let (app, mut c, mock, id) = setup(None, true).await;
    assert!(!app.state.notifier.enabled());
    assert!(app.state.notifier.watching().await.is_empty());
    let form = c.get_hx(&format!("/ui/printers/{id}")).await.body;
    assert!(!form.contains(r#"name="notify""#));
    // Saving the form without the checkbox keeps the stored choice.
    let csrf = c.session_csrf().await;
    let res = c
        .post_hx(
            &format!("/printers/{id}"),
            &csrf,
            &[("name", "Voron"), ("base_url", &mock.url()), ("enabled", "on")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(printers::get(&app.state.db, id).await.unwrap().unwrap().notify);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(app.state.notifier.watching().await.is_empty());
}

#[tokio::test]
async fn admin_turns_notifications_on_in_the_form() {
    let hook = Receiver::start(0).await;
    let (app, mut c, mock, id) = setup(Some(&hook.url), false).await;
    let form = c.get_hx(&format!("/ui/printers/{id}")).await.body;
    assert!(
        form.contains(r#"<input type="checkbox" name="notify">"#),
        "{form}"
    );
    let csrf = c.session_csrf().await;
    c.post_hx(
        &format!("/printers/{id}"),
        &csrf,
        &[
            ("name", "Voron"),
            ("base_url", &mock.url()),
            ("enabled", "on"),
            ("notify", "on"),
        ],
    )
    .await;
    let notifier = app.state.notifier.clone();
    eventually("watch started", async || notifier.watching().await == vec![id]).await;
    let list = c.get_hx("/ui/printers").await.body;
    assert!(list.contains(">NOTIFY</span>"));
    let detail: String =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'printer_update' ORDER BY id DESC")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert!(detail.ends_with("notify=on"), "{detail}");
    for body in [list, c.get_hx(&format!("/ui/printers/{id}")).await.body] {
        assert!(!body.contains(TOKEN), "webhook URL leaked");
    }
}

#[tokio::test]
async fn failed_deliveries_are_retried() {
    let hook = Receiver::start(1).await;
    let (app, _c, mock, id) = setup(Some(&hook.url), true).await;
    wait_live(&app, id).await;
    mock.set_print_state("error");
    eventually("retried delivery", async || !hook.bodies().is_empty()).await;
    assert_eq!(hook.calls.load(Ordering::SeqCst), 2);
    assert_eq!(hook.bodies()[0]["event"], "error");
}
