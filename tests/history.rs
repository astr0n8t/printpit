//! The History tab: lazy loading, paging, job details, reprint gating, history disabled.

mod common;

use axum::http::StatusCode;

use common::moonraker::{MockMoonraker, MockOptions};
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
        },
    )
    .await
    .unwrap();
    (app, c, mock, id)
}

async fn user(app: &TestApp, name: &str, role: Role) -> Client {
    app.create_user(name, "password123", role).await;
    let mut c = app.client();
    c.login(name, "password123").await;
    c
}

#[tokio::test]
async fn opening_the_tab_makes_no_printer_calls() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    for body in [
        c.get("/ui/history").await.body,
        c.get_hx("/ui/history").await.body,
    ] {
        assert!(
            body.contains(&format!(r#"hx-get="/p/{id}/history" hx-trigger="load""#)),
            "{body}"
        );
        assert!(body.contains("[5]</span>History"));
    }
    assert!(mock.paths().is_empty(), "{:?}", mock.paths());
}

#[tokio::test]
async fn lists_pages_of_jobs_newest_first() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    let res = c.get_hx(&format!("/p/{id}/history")).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = res.body;
    assert_eq!(body.matches("<tr data-name=").count(), 50);
    assert!(
        body.contains("60 jobs · 60:29:30 printing · 91.2 m filament · longest 1:00:59"),
        "{body}"
    );
    assert!(body.contains("1–50 of 60"));
    // Newest (job 59, a deleted file that ended in a shutdown) comes first.
    let first = body.find("<tr data-name=").unwrap();
    assert!(body[first..].starts_with(r#"<tr data-name="gone.gcode">"#));
    assert!(body.contains("KLIPPY SHUTDOWN") && body.contains("COMPLETED") && body.contains("CANCELLED"));
    assert!(body.contains(&format!(r#"hx-get="/p/{id}/history/job?uid=00003B""#)));
    assert!(body.contains(&format!(
        r#"hx-push-url="/ui/history?printer={id}&#38;job=00003B""#
    )));
    assert!(
        body.contains(r##"hx-get="" hx-target="#history" disabled>‹ newer"##),
        "no newer page"
    );
    assert!(body.contains(&format!(r#"hx-get="/p/{id}/history?start=50""#)));
    assert!(!body.contains(&mock.url()));

    let page2 = c.get_hx(&format!("/p/{id}/history?start=50")).await.body;
    assert_eq!(page2.matches("<tr data-name=").count(), 10);
    assert!(page2.contains("51–60 of 60"));
    assert!(page2.contains(&format!(r#"hx-get="/p/{id}/history?start=0""#)));
    assert!(page2.contains(r##"hx-target="#history" disabled>older ›"##));
    assert_eq!(
        mock.paths()
            .iter()
            .filter(|p| *p == "/server/history/list")
            .count(),
        2
    );
}

#[tokio::test]
async fn job_details_and_reprint() {
    let (app, mut admin, mock, id) = setup(MockOptions::default()).await;

    // Job 57: parts/clip.gcode, completed, still on the printer, with a thumbnail.
    let res = admin.get_hx(&format!("/p/{id}/history/job?uid=000039")).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = res.body;
    assert!(body.contains("parts/clip.gcode") && body.contains("COMPLETED"));
    assert!(body.contains(&format!(
        r#"src="/p/{id}/files/thumb?path=parts%2F.thumbs%2Fclip-300x300.png""#
    )));
    assert!(body.contains("<dt>printing</dt><dd>1:00:57</dd>"), "{body}");
    assert!(
        body.contains("<dt>print time</dt><dd>1:02:05</dd>"),
        "slicer estimate"
    );
    assert!(body.contains(r#"hx-confirm="Print parts/clip.gcode again?""#));
    assert!(body.contains(r#"<input type="hidden" name="filename" value="parts/clip.gcode">"#));

    // Job 59: the file has gone, so no thumbnail and no reprint.
    let gone = admin
        .get_hx(&format!("/p/{id}/history/job?uid=00003B"))
        .await
        .body;
    assert!(gone.contains("no longer on the printer"));
    assert!(!gone.contains("print again") && !gone.contains("<img"));

    // Viewers see details but can't print.
    let mut viewer = user(&app, "viewer", Role::Viewer).await;
    let body = viewer
        .get_hx(&format!("/p/{id}/history/job?uid=000039"))
        .await
        .body;
    assert!(body.contains("COMPLETED") && !body.contains("print again"));

    // Reprint uses the normal start-print route.
    let csrf = admin.session_csrf().await;
    let res = admin
        .post_hx(
            &format!("/p/{id}/print/start"),
            &csrf,
            &[("filename", "parts/clip.gcode")],
        )
        .await;
    assert!(res.body.contains("Print started."));
    assert!(mock.paths().contains(&"/printer/print/start".to_string()));
}

#[tokio::test]
async fn bad_and_unknown_job_ids() {
    let (_app, mut c, mock, id) = setup(MockOptions::default()).await;
    for uid in ["", "1%26limit%3D1", "..%2Fx", "a%20b"] {
        let res = c.get_hx(&format!("/p/{id}/history/job?uid={uid}")).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{uid}");
    }
    assert!(mock.paths().is_empty(), "invalid ids never reach Moonraker");
    let res = c.get_hx(&format!("/p/{id}/history/job?uid=0000FF")).await;
    assert!(res.body.contains("isn&#39;t in the history") || res.body.contains("isn't in the history"));
}

#[tokio::test]
async fn deep_link_preloads_the_job() {
    let (_app, mut c, _mock, id) = setup(MockOptions::default()).await;
    let page = c.get(&format!("/ui/history?printer={id}&job=000039")).await.body;
    assert!(
        page.contains(&format!(r#"hx-get="/p/{id}/history?job=000039""#)),
        "{page}"
    );
    // A bad id in the URL is dropped rather than passed on.
    let page = c.get(&format!("/ui/history?printer={id}&job=x%26y")).await.body;
    assert!(page.contains(&format!(r#"hx-get="/p/{id}/history" hx-trigger"#)));

    let list = c.get_hx(&format!("/p/{id}/history?job=000039")).await.body;
    assert!(list.contains(r#"<section class="panel" id="job-view">"#));
    assert!(list.contains("parts/clip.gcode <span class=\"badge badge-ok\">COMPLETED"));
}

#[tokio::test]
async fn history_disabled_in_moonraker() {
    let (_app, mut c, _mock, id) = setup(MockOptions {
        no_history: true,
        ..MockOptions::default()
    })
    .await;
    let body = c.get_hx(&format!("/p/{id}/history")).await.body;
    assert!(body.contains("Job history is off on this printer"), "{body}");
    assert!(!body.contains("<table"));
}
