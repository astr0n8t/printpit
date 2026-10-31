//! The Logs tab: lazy loading, Range-based tails, the streaming fallback, download, safety.

mod common;

use axum::http::StatusCode;

use common::moonraker::{MockMoonraker, MockOptions, klippy_log};
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};

const ENV: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
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
            webcam_transform: None,
            enabled: true,
            notify: false,
        },
    )
    .await
    .unwrap();
    (app, c, mock, id)
}

fn line_count(body: &str) -> usize {
    body.matches("<span class=").count()
}

#[tokio::test]
async fn opening_the_tab_makes_no_printer_calls() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get("/ui/logs").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/logs" hx-trigger="load""#))
    );
    assert!(mock.paths().is_empty());

    // The list is one directory call; the viewer shell needs none.
    let list = c.get_hx(&format!("/p/{id}/logs")).await;
    assert!(
        list.body.find("klippy.log").unwrap() < list.body.find("moonraker.log").unwrap(),
        "newest first"
    );
    let view = c.get_hx(&format!("/p/{id}/logs/view?name=klippy.log")).await;
    assert!(view.body.contains(r#"hx-get="/p/1/logs/tail?name=klippy.log""#));
    assert!(view.body.contains(r#"<option value="500" selected>"#));
    assert_eq!(mock.paths(), ["/server/files/directory"]);
}

#[tokio::test]
async fn deep_link_preloads_the_viewer() {
    let (_app, mut c, _mock, id) = setup(MockOptions::default()).await;
    let res = c.get("/ui/logs?name=moonraker.log").await;
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/logs?name=moonraker.log""#))
    );
    let list = c.get_hx(&format!("/p/{id}/logs?name=moonraker.log")).await;
    assert!(
        list.body
            .contains(r#"<h2 class="panel-title">moonraker.log</h2>"#)
    );
}

#[tokio::test]
async fn tail_uses_a_range_request() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c
        .get_hx(&format!("/p/{id}/logs/tail?name=klippy.log&lines=200"))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(line_count(&res.body), 200);
    assert!(res.body.contains("last 200 lines"));
    assert!(res.body.contains("LAST LINE marker"));
    assert!(res.body.contains(r#"<span class="err">Traceback"#));
    assert!(res.body.contains(r#"<span class="warn">[WARNING]"#));
    assert!(res.body.contains(r#"<span class="dim">Stats"#));
    let total = klippy_log().len();
    assert!(res.body.contains(&printpit_size(total)));
    assert!(
        mock.log_bytes_sent() < total / 4,
        "only the end of the file was transferred"
    );

    for (n, expect) in [(500, 500), (2000, 2000), (7, 500)] {
        let res = c
            .get_hx(&format!("/p/{id}/logs/tail?name=klippy.log&lines={n}"))
            .await;
        assert_eq!(line_count(&res.body), expect, "lines={n}");
    }
}

fn printpit_size(bytes: usize) -> String {
    format!("{:.1} KB", bytes as f64 / 1024.0)
}

#[tokio::test]
async fn tail_falls_back_to_streaming_without_range_support() {
    let (_app, mut c, mock, id) = setup(MockOptions {
        no_range: true,
        ..MockOptions::default()
    })
    .await;
    let res = c
        .get_hx(&format!("/p/{id}/logs/tail?name=klippy.log&lines=200"))
        .await;
    assert_eq!(line_count(&res.body), 200);
    assert!(res.body.contains("LAST LINE marker"));
    assert_eq!(mock.log_bytes_sent(), klippy_log().len(), "whole file streamed");
}

#[tokio::test]
async fn short_empty_and_missing_logs() {
    let (_app, mut c, _mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/logs/tail?name=moonraker.log")).await;
    assert_eq!(line_count(&res.body), 2);
    assert!(
        res.body.contains("moonraker started"),
        "a file shorter than the window keeps its first line"
    );

    let res = c.get_hx(&format!("/p/{id}/logs/tail?name=empty.log")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("last 0 lines"));

    let res = c.get_hx(&format!("/p/{id}/logs/tail?name=nope.log")).await;
    assert!(res.body.contains("File does not exist"), "{}", res.body);
}

#[tokio::test]
async fn download_and_name_safety() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get(&format!("/p/{id}/logs/download?name=klippy.log")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body.len(), klippy_log().len());
    assert!(
        res.header("content-disposition")
            .contains(r#"filename="klippy.log""#)
    );
    assert_eq!(
        c.get(&format!("/p/{id}/logs/download?name=nope.log"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let before = mock.paths().len();
    for bad in ["../moonraker.conf", "/etc/passwd", "a//b", ""] {
        let q: String = url::form_urlencoded::byte_serialize(bad.as_bytes()).collect();
        assert_eq!(
            c.get_hx(&format!("/p/{id}/logs/tail?name={q}")).await.status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            c.get_hx(&format!("/p/{id}/logs/view?name={q}")).await.status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            c.get(&format!("/p/{id}/logs/download?name={q}")).await.status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(mock.paths().len(), before, "rejected before reaching the printer");
}

#[tokio::test]
async fn viewers_can_read_logs() {
    let (app, _admin, _mock, id) = setup(MockOptions::default()).await;
    app.create_user("viewer", "password123", Role::Viewer).await;
    let mut v = app.client();
    v.login("viewer", "password123").await;
    assert_eq!(
        v.get_hx(&format!("/p/{id}/logs/tail?name=klippy.log"))
            .await
            .status,
        StatusCode::OK
    );
}
