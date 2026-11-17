//! The app is installable: a public web app manifest with icons, linked from every page.

mod common;

use axum::http::StatusCode;
use common::TestApp;

#[tokio::test]
async fn manifest_and_icons_are_public_and_linked() {
    // Before setup and without a session: browsers fetch manifests without cookies.
    let app = TestApp::new().await;
    let mut anon = app.client();
    let res = anon.get("/static/manifest.webmanifest").await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.header("content-type"), "application/manifest+json");
    let m: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(m["display"], "standalone");
    assert_eq!(m["start_url"], "/");
    let icons = m["icons"].as_array().unwrap();
    for size in ["192x192", "512x512"] {
        assert!(
            icons.iter().any(|i| i["sizes"] == size && i["purpose"] == "any"),
            "{size}"
        );
    }
    assert!(icons.iter().any(|i| i["purpose"] == "maskable"));
    for icon in icons {
        let res = anon.get(icon["src"].as_str().unwrap()).await;
        assert_eq!(res.status, StatusCode::OK, "{icon}");
        assert_eq!(res.header("content-type"), "image/png", "{icon}");
        assert!(
            res.header("cache-control").contains("immutable"),
            "versioned: {icon}"
        );
    }

    let (app, mut c) = TestApp::with_admin().await;
    let link = r#"<link rel="manifest" href="/static/manifest.webmanifest">"#;
    assert!(app.client().get("/login").await.body.contains(link), "login page");
    let page = c.get("/ui/dashboard").await;
    assert!(page.body.contains(link), "app shell");
    let touch = page
        .body
        .split(r#"rel="apple-touch-icon" href=""#)
        .nth(1)
        .expect("touch icon");
    let touch = &touch[..touch.find('"').unwrap()];
    let res = c.get(touch).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.header("content-type"), "image/png");
}
