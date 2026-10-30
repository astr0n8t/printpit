use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use printpit::config::Config;
use printpit::state::AppState;
use printpit::web;

const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn app() -> Router {
    let config = Config::from_lookup(|k| (k == "PRINTPIT_SECRET_KEY").then(|| KEY.to_string())).unwrap();
    web::router(AppState::new(config))
}

async fn get(uri: &str, headers: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::get(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn healthz_is_ok_with_security_headers() {
    let (status, headers, body) = get("/healthz", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "no CORS"
    );
}

#[tokio::test]
async fn root_redirects_to_dashboard() {
    let (status, headers, _) = get("/", &[]).await;
    assert!(status.is_redirection());
    assert_eq!(headers[header::LOCATION], "/ui/dashboard");
}

#[tokio::test]
async fn tab_full_page_renders_shell() {
    let (status, headers, body) = get("/ui/files", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("<!doctype html>"));
    assert!(body.contains(r#"<main id="main""#));
    assert!(body.contains(r#"class="tab active" aria-current="page"><span class="key">[2]</span>Files"#));
    assert!(!body.contains("hx-swap-oob"));
    assert_eq!(headers[header::VARY], "HX-Request");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn tab_htmx_request_returns_fragment_with_oob_tabs() {
    let (status, _, body) = get("/ui/logs", &[("HX-Request", "true")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<html"));
    assert!(body.contains(r#"hx-swap-oob="true""#));
    assert!(body.contains(r#"<span class="key">[3]</span>Logs"#));
}

#[tokio::test]
async fn history_restore_gets_full_page() {
    let (_, _, body) = get(
        "/ui/system",
        &[("HX-Request", "true"), ("HX-History-Restore-Request", "true")],
    )
    .await;
    assert!(body.starts_with("<!doctype html>"));
}

#[tokio::test]
async fn unknown_route_is_404_error_panel() {
    let (status, _, body) = get("/nope", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.starts_with("<!doctype html>"));
    assert!(body.contains("panel-err"));

    let (status, _, body) = get("/nope", &[("HX-Request", "true")]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.trim_start()
            .starts_with("<section class=\"panel panel-err\"")
    );
}

#[tokio::test]
async fn static_assets_are_hashed_and_cached() {
    let url = web::assets::url("css/tui.css");
    let (status, headers, body) = get(&url, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(
        headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(body.contains("--accent"));

    let (status, headers, _) = get("/static/0000000000000000/css/tui.css", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");

    let (status, _, _) = get("/static/0000000000000000/missing.js", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn shell_references_only_local_assets() {
    let (_, _, body) = get("/ui/dashboard", &[]).await;
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app()).into_future());
    let result = tokio::task::spawn_blocking(move || printpit::healthcheck::probe(addr))
        .await
        .unwrap();
    assert_eq!(result, Ok(()));
}
