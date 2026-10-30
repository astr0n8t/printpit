//! The Files tab: lazy listing, metadata/thumbnails, download, upload, file operations, roles.

mod common;

use axum::http::StatusCode;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, NewPrinter};

const ENV: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
    ("PRINTPIT_MAX_UPLOAD_MB", "1"),
];

async fn setup() -> (TestApp, Client, MockMoonraker, i64, String) {
    let mock = MockMoonraker::start().await;
    let (app, mut c) = TestApp::with_admin_env(ENV).await;
    let id = add_printer(&app, "Voron", &mock.url()).await;
    let csrf = c.session_csrf().await;
    (app, c, mock, id, csrf)
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

async fn audit_actions(app: &TestApp) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT action, COALESCE(detail, '') FROM audit_log WHERE printer_id IS NOT NULL ORDER BY id",
    )
    .fetch_all(&app.state.db)
    .await
    .unwrap()
}

fn dir_calls(mock: &MockMoonraker) -> usize {
    mock.paths()
        .iter()
        .filter(|p| *p == "/server/files/directory")
        .count()
}

// ── Lazy loading and the printer selector ────────────────────────────

#[tokio::test]
async fn opening_the_tab_makes_no_printer_calls() {
    let (_app, mut c, mock, id, _) = setup().await;
    let res = c.get("/ui/files").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body
            .contains(&format!(r#"hx-get="/p/{id}/files?path=" hx-trigger="load""#))
    );
    assert!(res.body.contains(r#"<option value="1" selected>Voron</option>"#));
    assert!(mock.paths().is_empty(), "{:?}", mock.paths());
}

#[tokio::test]
async fn printer_choice_is_remembered() {
    let (app, mut c, _mock, _id, _) = setup().await;
    let other = MockMoonraker::start().await;
    let second = add_printer(&app, "Ender", &other.url()).await;
    let res = c.get(&format!("/ui/files?printer={second}")).await;
    assert!(
        res.body
            .contains(&format!(r#"<option value="{second}" selected>Ender</option>"#))
    );
    let res = c.get("/ui/files").await;
    assert!(
        res.body
            .contains(&format!(r#"<option value="{second}" selected>Ender</option>"#)),
        "remembered"
    );
    // Disabled printers drop out and the selection falls back.
    printers::set_enabled(&app.state.db, second, false).await.unwrap();
    let res = c.get("/ui/files").await;
    assert!(res.body.contains(r#"<option value="1" selected>Voron</option>"#));
    assert!(!res.body.contains("Ender"));
}

#[tokio::test]
async fn no_printers_message() {
    let (_app, mut c) = TestApp::with_admin_env(ENV).await;
    assert!(
        c.get("/ui/files")
            .await
            .body
            .contains("No printers configured yet.")
    );
}

// ── Listing ──────────────────────────────────────────────────────────

#[tokio::test]
async fn lists_folders_then_files_with_metadata() {
    let (_app, mut c, mock, id, _) = setup().await;
    let res = c.get_hx(&format!("/p/{id}/files")).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = &res.body;
    assert!(
        body.find("▸ parts/").unwrap() < body.find("old.gcode").unwrap(),
        "folders first"
    );
    assert!(
        body.contains("1:02:05") && body.contains("1.5 m"),
        "slicer estimates"
    );
    assert!(body.contains("18.6 GB free of 27.9 GB"));
    assert!(body.contains(r#"hx-push-url="/ui/files?printer=1&#38;path=parts""#));
    assert!(!body.contains(&mock.url()));

    let res = c.get_hx(&format!("/p/{id}/files?path=parts")).await;
    assert!(res.body.contains("clip.gcode"));
    assert!(!res.body.contains(".thumbs"), "hidden folders are hidden");
    assert!(
        res.body.contains(">gcodes</a><span class=\"dim\"> / </span><a"),
        "breadcrumb"
    );

    let res = c.get_hx(&format!("/p/{id}/files?path=nope")).await;
    assert!(res.body.contains("Directory does not exist"), "{}", res.body);
}

#[tokio::test]
async fn sorting_by_column() {
    let (_app, mut c, _mock, id, csrf) = setup().await;
    c.post_hx(
        &format!("/p/{id}/files/move"),
        &csrf,
        &[
            ("source", "parts/clip.gcode"),
            ("dest", "big.gcode"),
            ("copy", "on"),
        ],
    )
    .await;
    let order = |body: &str| (body.find("big.gcode").unwrap(), body.find("old.gcode").unwrap());
    let (big, old) = order(
        &c.get_hx(&format!("/p/{id}/files?sort=size&order=desc"))
            .await
            .body,
    );
    assert!(big < old);
    let (big, old) = order(&c.get_hx(&format!("/p/{id}/files?sort=size&order=asc")).await.body);
    assert!(old < big);
    let body = c
        .get_hx(&format!("/p/{id}/files?sort=size&order=desc"))
        .await
        .body;
    assert!(body.contains("size ▼"));
}

#[tokio::test]
async fn listings_are_cached_briefly_and_invalidated_on_change() {
    let (_app, mut c, mock, id, csrf) = setup().await;
    c.get_hx(&format!("/p/{id}/files")).await;
    c.get_hx(&format!("/p/{id}/files?sort=size")).await;
    assert_eq!(dir_calls(&mock), 1, "second listing served from cache");
    c.post_hx(
        &format!("/p/{id}/files/mkdir"),
        &csrf,
        &[("dir", ""), ("name", "new")],
    )
    .await;
    // The mock records paths without the method: one POST (mkdir) plus one fresh listing.
    assert_eq!(dir_calls(&mock), 3, "change re-lists");
    let body = c.get_hx(&format!("/p/{id}/files")).await.body;
    assert!(body.contains("▸ new/"));
}

#[tokio::test]
async fn unsafe_paths_are_rejected_before_reaching_the_printer() {
    let (_app, mut c, mock, id, csrf) = setup().await;
    for path in ["../etc", "parts/../../x", "a//b", "a\\b"] {
        let q = url_encode(path);
        assert_eq!(
            c.get_hx(&format!("/p/{id}/files?path={q}")).await.status,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
        assert_eq!(
            c.get_hx(&format!("/p/{id}/files/meta?path={q}")).await.status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            c.get(&format!("/p/{id}/files/download?path={q}")).await.status,
            StatusCode::NOT_FOUND
        );
        let res = c.delete_hx(&format!("/p/{id}/files?path={q}"), &csrf).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST);
    }
    let res = c
        .post_hx(
            &format!("/p/{id}/files/move"),
            &csrf,
            &[("source", "old.gcode"), ("dest", "../x.gcode")],
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = c
        .post_hx(
            &format!("/p/{id}/files/mkdir"),
            &csrf,
            &[("dir", ""), ("name", "a/b")],
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(mock.paths().is_empty(), "{:?}", mock.paths());
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ── Details, thumbnails, downloads ───────────────────────────────────

#[tokio::test]
async fn file_details_and_thumbnail() {
    let (_app, mut c, _mock, id, _) = setup().await;
    let res = c
        .get_hx(&format!("/p/{id}/files/meta?path=parts/clip.gcode&kind=file"))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    for fact in [
        "1:02:05",
        "1.5 m · 5 g",
        "0.20 mm",
        "215 / 60 °C",
        "PrusaSlicer 2.8.1",
    ] {
        assert!(res.body.contains(fact), "missing {fact}");
    }
    // The largest thumbnail ≤ 400 px, resolved relative to the file's folder.
    let thumb = "/p/1/files/thumb?path=parts%2F.thumbs%2Fclip-300x300.png";
    assert!(res.body.contains(thumb), "{}", res.body);
    let img = c.get(thumb).await;
    assert_eq!(img.status, StatusCode::OK);
    assert_eq!(img.header("content-type"), "image/png");
    // The thumbnail endpoint only serves images.
    assert_eq!(
        c.get(&format!("/p/{id}/files/thumb?path=old.gcode")).await.status,
        StatusCode::NOT_FOUND
    );

    let res = c.get_hx(&format!("/p/{id}/files/meta?path=parts&kind=dir")).await;
    assert!(res.body.contains("folder") && res.body.contains("and everything in it"));
}

#[tokio::test]
async fn download_streams_an_attachment() {
    let (_app, mut c, _mock, id, _) = setup().await;
    let res = c
        .get(&format!("/p/{id}/files/download?path=parts/clip.gcode"))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body.len(), 2_500_000);
    assert!(
        res.header("content-disposition")
            .starts_with(r#"attachment; filename="clip.gcode""#)
    );
    assert_eq!(res.header("content-length"), "2500000");
    assert_eq!(
        c.get(&format!("/p/{id}/files/download?path=missing.gcode"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ── Changes ──────────────────────────────────────────────────────────

#[tokio::test]
async fn upload_streams_to_moonraker() {
    let (app, mut c, mock, id, csrf) = setup().await;
    let data = vec![b'G'; 300_000];
    let res = c
        .post_multipart(
            &format!("/p/{id}/files/upload"),
            &csrf,
            &[("dir", "parts")],
            Some(("new part.gcode", &data)),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("✓ Uploaded new part.gcode."), "{}", res.body);
    assert!(
        res.body.contains("new part.gcode</a>"),
        "listing of the target folder"
    );
    assert_eq!(mock.file_len("parts/new part.gcode"), Some(300_000));
    assert_eq!(mock.print_state(), "printing");

    let res = c
        .post_multipart(
            &format!("/p/{id}/files/upload"),
            &csrf,
            &[("dir", ""), ("print", "on")],
            Some(("go.gcode", b"G28\n")),
        )
        .await;
    assert!(res.body.contains("and started printing"), "{}", res.body);

    let audit = audit_actions(&app).await;
    assert_eq!(audit[0].0, "file_upload");
    assert_eq!(audit[0].1, "path=parts/new part.gcode bytes=300000 print=false");
    assert!(audit[1].1.ends_with("print=true"));
}

#[tokio::test]
async fn upload_limits_and_validation() {
    let (app, mut c, mock, id, csrf) = setup().await;
    let big = vec![b'G'; 1_500_000]; // limit is 1 MB in this test
    let res = c
        .post_multipart(
            &format!("/p/{id}/files/upload"),
            &csrf,
            &[("dir", "")],
            Some(("big.gcode", &big)),
        )
        .await;
    assert!(!mock.has_file("big.gcode"), "oversized upload must not land");
    assert!(
        res.status == StatusCode::PAYLOAD_TOO_LARGE
            || res.body.contains("larger")
            || res.body.contains("too large"),
        "{} {}",
        res.status,
        res.body
    );

    for name in [".hidden.gcode", "../x.gcode"] {
        let res = c
            .post_multipart(
                &format!("/p/{id}/files/upload"),
                &csrf,
                &[("dir", "")],
                Some((name, b"G28")),
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{name}");
    }
    let res = c
        .post_multipart(
            &format!("/p/{id}/files/upload"),
            &csrf,
            &[("dir", "../up")],
            Some(("a.gcode", b"G28")),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    // Uploads need the CSRF header like everything else.
    let res = c
        .post_multipart(
            &format!("/p/{id}/files/upload"),
            "wrong",
            &[("dir", "")],
            Some(("a.gcode", b"G28")),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(audit_actions(&app).await.is_empty());
}

#[tokio::test]
async fn folder_move_copy_and_delete() {
    let (app, mut c, mock, id, csrf) = setup().await;
    let res = c
        .post_hx(
            &format!("/p/{id}/files/mkdir"),
            &csrf,
            &[("dir", "parts"), ("name", "brackets")],
        )
        .await;
    assert!(res.body.contains("✓ Created brackets/."));
    assert!(mock.has_dir("parts/brackets"));

    let res = c
        .post_hx(
            &format!("/p/{id}/files/move"),
            &csrf,
            &[("source", "old.gcode"), ("dest", "parts/brackets/renamed.gcode")],
        )
        .await;
    assert!(
        res.body
            .contains("✓ Moved old.gcode → parts/brackets/renamed.gcode.")
    );
    assert!(
        res.body.contains("renamed.gcode</a>"),
        "lists the destination folder"
    );
    assert!(!mock.has_file("old.gcode") && mock.has_file("parts/brackets/renamed.gcode"));

    c.post_hx(
        &format!("/p/{id}/files/move"),
        &csrf,
        &[
            ("source", "parts/clip.gcode"),
            ("dest", "clip-copy.gcode"),
            ("copy", "on"),
        ],
    )
    .await;
    assert!(mock.has_file("parts/clip.gcode") && mock.has_file("clip-copy.gcode"));

    let res = c
        .delete_hx(&format!("/p/{id}/files?path=clip-copy.gcode&kind=file"), &csrf)
        .await;
    assert!(res.body.contains("✓ Deleted clip-copy.gcode."));
    assert!(!mock.has_file("clip-copy.gcode"));

    let res = c
        .delete_hx(&format!("/p/{id}/files?path=parts&kind=dir"), &csrf)
        .await;
    assert!(res.body.contains("✓ Deleted parts."));
    assert!(!mock.has_dir("parts") && !mock.has_file("parts/clip.gcode"));

    let res = c
        .delete_hx(&format!("/p/{id}/files?path=ghost.gcode&kind=file"), &csrf)
        .await;
    assert!(
        res.body.contains("! ERROR") && res.body.contains("File does not exist"),
        "{}",
        res.body
    );

    let actions: Vec<String> = audit_actions(&app).await.into_iter().map(|(a, _)| a).collect();
    assert_eq!(
        actions,
        [
            "dir_create",
            "file_move",
            "file_copy",
            "file_delete",
            "dir_delete"
        ]
    );
}

#[tokio::test]
async fn print_from_the_file_details() {
    let (_app, mut c, mock, id, csrf) = setup().await;
    let meta = c
        .get_hx(&format!("/p/{id}/files/meta?path=parts/clip.gcode&kind=file"))
        .await;
    assert!(meta.body.contains(r#"hx-post="/p/1/print/start""#));
    c.post_hx(&format!("/p/{id}/print/cancel"), &csrf, &[]).await;
    let res = c
        .post_hx(
            &format!("/p/{id}/print/start"),
            &csrf,
            &[("filename", "parts/clip.gcode")],
        )
        .await;
    assert!(res.body.contains("Print started."));
    assert_eq!(mock.print_state(), "printing");
}

// ── Roles ────────────────────────────────────────────────────────────

#[tokio::test]
async fn viewers_can_browse_but_not_change() {
    let (app, _admin, mock, id, _) = setup().await;
    app.create_user("viewer", "password123", Role::Viewer).await;
    let mut v = app.client();
    v.login("viewer", "password123").await;
    let csrf = v.session_csrf().await;

    let list = v.get_hx(&format!("/p/{id}/files")).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(!list.body.contains("upload"), "no upload form for viewers");
    let meta = v
        .get_hx(&format!("/p/{id}/files/meta?path=parts/clip.gcode"))
        .await;
    assert!(
        meta.body.contains("download") && !meta.body.contains("delete") && !meta.body.contains(">print<")
    );
    assert_eq!(
        v.get(&format!("/p/{id}/files/download?path=old.gcode"))
            .await
            .status,
        StatusCode::OK
    );

    let forbidden = [
        v.post_hx(
            &format!("/p/{id}/files/mkdir"),
            &csrf,
            &[("dir", ""), ("name", "x")],
        )
        .await,
        v.post_hx(
            &format!("/p/{id}/files/move"),
            &csrf,
            &[("source", "old.gcode"), ("dest", "y.gcode")],
        )
        .await,
        v.delete_hx(&format!("/p/{id}/files?path=old.gcode"), &csrf).await,
        v.post_multipart(
            &format!("/p/{id}/files/upload"),
            &csrf,
            &[("dir", "")],
            Some(("a.gcode", b"G28")),
        )
        .await,
    ];
    for res in forbidden {
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }
    assert!(mock.has_file("old.gcode"));
}
