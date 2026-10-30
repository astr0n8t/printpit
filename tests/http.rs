//! Shell, routing, static assets and health check.

mod common;

use axum::http::{StatusCode, header};

use common::TestApp;
use printpit::web;

#[tokio::test]
async fn healthz_is_ok_with_security_headers() {
    let app = TestApp::new().await;
    let res = app.client().get("/healthz").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, "ok");
    let csp = res.header("content-security-policy");
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert!(
        res.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "no CORS"
    );
}

#[tokio::test]
async fn root_redirects_to_dashboard() {
    let (_app, mut c) = TestApp::with_admin().await;
    let res = c.get("/").await;
    assert!(res.status.is_redirection());
    assert_eq!(res.location(), "/ui/dashboard");
}

#[tokio::test]
async fn tab_full_page_renders_shell() {
    let (_app, mut c) = TestApp::with_admin().await;
    let res = c.get("/ui/files").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.starts_with("<!doctype html>"));
    assert!(res.body.contains(r#"<main id="main""#));
    assert!(
        res.body
            .contains(r#"class="tab active" aria-current="page"><span class="key">[2]</span>Files"#)
    );
    assert!(!res.body.contains("hx-swap-oob"));
    assert_eq!(res.header("vary"), "HX-Request");
    assert_eq!(res.header("cache-control"), "no-store");
}

#[tokio::test]
async fn tab_htmx_request_returns_fragment_with_oob_tabs() {
    let (_app, mut c) = TestApp::with_admin().await;
    let res = c.get_hx("/ui/logs").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(!res.body.contains("<html"));
    assert!(res.body.contains(r#"hx-swap-oob="true""#));
    assert!(res.body.contains(r#"<span class="key">[3]</span>Logs"#));
}

#[tokio::test]
async fn history_restore_gets_full_page() {
    let (_app, mut c) = TestApp::with_admin().await;
    let res = c
        .send(
            axum::http::Method::GET,
            "/ui/system",
            &[("hx-request", "true"), ("hx-history-restore-request", "true")],
            None,
        )
        .await;
    assert!(res.body.starts_with("<!doctype html>"));
}

#[tokio::test]
async fn unknown_route_is_404_error_panel() {
    let (_app, mut c) = TestApp::with_admin().await;
    let res = c.get("/nope").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(res.body.starts_with("<!doctype html>"));
    assert!(res.body.contains("panel-err"));

    let res = c.get_hx("/nope").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(
        res.body
            .trim_start()
            .starts_with("<section class=\"panel panel-err\"")
    );
}

#[tokio::test]
async fn static_assets_are_hashed_and_cached() {
    // Static assets are public, even before setup.
    let app = TestApp::new().await;
    let mut c = app.client();
    let url = web::assets::url("css/tui.css");
    let res = c.get(&url).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.header("content-type").starts_with("text/css"));
    assert!(res.header("cache-control").contains("immutable"));
    assert!(res.body.contains("--accent"));

    let res = c.get("/static/0000000000000000/css/tui.css").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.header("cache-control"), "no-cache");

    let res = c.get("/static/0000000000000000/missing.js").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn shell_references_only_local_assets() {
    let (_app, mut c) = TestApp::with_admin().await;
    let body = c.get("/ui/dashboard").await.body;
    for asset in ["css/tui.css", "js/htmx.min.js", "js/app.js"] {
        assert!(body.contains(&web::assets::url(asset)), "missing {asset}");
    }
    assert!(
        !body.contains("http://") && !body.contains("https://"),
        "no external URLs in the shell"
    );
    assert!(!body.contains("<script>"), "no inline scripts (CSP)");
}

#[tokio::test]
async fn healthcheck_probe_against_live_server() {
    let app = TestApp::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.router.clone()).into_future());
    let result = tokio::task::spawn_blocking(move || printpit::healthcheck::probe(addr))
        .await
        .unwrap();
    assert_eq!(result, Ok(()));
}
