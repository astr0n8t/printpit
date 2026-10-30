//! Printer management, encrypted API keys, the egress allowlist and "test connection".

mod common;

use axum::http::StatusCode;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::auth::Role;
use printpit::db::printers::{self, API_KEY_CONTEXT, NewPrinter};

const SECRET_KEY: &str = "SUPERSECRET-moonraker-key-123";
/// Loopback allowed so tests can reach the mock Moonraker.
const LOCAL: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
];

async fn admin() -> (TestApp, Client, String) {
    let (app, mut c) = TestApp::with_admin_env(LOCAL).await;
    let csrf = c.session_csrf().await;
    (app, c, csrf)
}

async fn add(c: &mut Client, csrf: &str, name: &str, url: &str, key: &str) -> common::Resp {
    c.post_hx(
        "/printers",
        csrf,
        &[
            ("name", name),
            ("base_url", url),
            ("api_key", key),
            ("enabled", "on"),
        ],
    )
    .await
}

async fn only_printer(app: &TestApp) -> printers::Printer {
    let mut all = printers::list(&app.state.db).await.unwrap();
    assert_eq!(all.len(), 1);
    all.remove(0)
}

// ── CRUD ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_edit_and_delete_a_printer() {
    let (app, mut c, csrf) = admin().await;

    let res = add(&mut c, &csrf, "Voron 2.4", "127.0.0.1:7125/", "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.header("hx-push-url"), "/ui/printers");
    assert!(res.body.contains("Added Voron 2.4."));
    let p = only_printer(&app).await;
    assert_eq!(p.base_url, "http://127.0.0.1:7125", "normalised");
    assert!(p.api_key_enc.is_none());
    assert!(p.enabled);

    let res = c
        .post_hx(
            &format!("/printers/{}", p.id),
            &csrf,
            &[("name", "Voron Trident"), ("base_url", "http://127.0.0.1:7126")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let p = only_printer(&app).await;
    assert_eq!(p.name, "Voron Trident");
    assert!(!p.enabled, "unchecked checkbox disables");

    let res = c.delete_hx(&format!("/printers/{}", p.id), &csrf).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Deleted Voron Trident."));
    assert!(printers::list(&app.state.db).await.unwrap().is_empty());

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'printer_%' ORDER BY id")
            .fetch_all(&app.state.db)
            .await
            .unwrap();
    assert_eq!(actions, ["printer_create", "printer_update", "printer_delete"]);
}

#[tokio::test]
async fn plain_form_posts_redirect() {
    let (app, mut c, csrf) = admin().await;
    let res = c
        .post(
            "/printers",
            &[("_csrf", &csrf), ("name", "P"), ("base_url", "127.0.0.1:7125")],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/ui/printers");
    assert_eq!(printers::list(&app.state.db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn validation_errors_rerender_the_form() {
    let (app, mut c, csrf) = admin().await;
    add(&mut c, &csrf, "Taken", "127.0.0.1:7125", "").await;

    let cases: &[(&str, &str, &str, &str)] = &[
        ("", "127.0.0.1", "", "Name must be"),
        ("Taken", "127.0.0.1", "", "already has that name"),
        ("TAKEN", "127.0.0.1", "", "already has that name"),
        ("New", "ftp://127.0.0.1", "", "Only http"),
        ("New", "http://user:pw@127.0.0.1", "", "username or password"),
        (
            "New",
            "http://10.0.0.5:7125",
            "",
            "outside the allowed printer networks",
        ),
        ("New", "127.0.0.1", "has space", "API key looks wrong"),
    ];
    for (name, url, key, msg) in cases {
        let res = add(&mut c, &csrf, name, url, key).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{msg}");
        assert!(res.body.contains(msg), "expected `{msg}` in {}", res.body);
    }
    assert_eq!(printers::list(&app.state.db).await.unwrap().len(), 1);

    // A typed key is never echoed back on error, but the user is told.
    let res = add(&mut c, &csrf, "Taken", "127.0.0.1", SECRET_KEY).await;
    assert!(!res.body.contains(SECRET_KEY));
    assert!(res.body.contains("wasn't saved"));
}

#[tokio::test]
async fn default_allowlist_blocks_loopback() {
    // Default PRINTPIT_PRINTER_CIDRS (private ranges) doesn't include 127.0.0.0/8.
    let (_app, mut c) = TestApp::with_admin().await;
    let csrf = c.session_csrf().await;
    for url in ["127.0.0.1:8080", "localhost:8080", "[::1]:8080"] {
        let res = add(&mut c, &csrf, "Self", url, "").await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{url}");
    }
    let res = add(&mut c, &csrf, "LAN", "192.168.1.50:7125", "").await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn reorder_and_toggle() {
    let (app, mut c, csrf) = admin().await;
    for name in ["A", "B", "C"] {
        add(&mut c, &csrf, name, "127.0.0.1:7125", "").await;
    }
    let names = |app: &TestApp| {
        let db = app.state.db.clone();
        async move {
            printers::list(&db)
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        }
    };
    let ids: Vec<i64> = printers::list(&app.state.db)
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();

    c.post_hx(&format!("/printers/{}/move/up", ids[2]), &csrf, &[])
        .await;
    assert_eq!(names(&app).await, ["A", "C", "B"]);
    c.post_hx(&format!("/printers/{}/move/up", ids[0]), &csrf, &[])
        .await;
    assert_eq!(names(&app).await, ["A", "C", "B"], "top can't move up");
    c.post_hx(&format!("/printers/{}/move/down", ids[0]), &csrf, &[])
        .await;
    assert_eq!(names(&app).await, ["C", "A", "B"]);
    assert_eq!(
        c.post_hx(&format!("/printers/{}/move/sideways", ids[0]), &csrf, &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let res = c
        .post_hx(&format!("/printers/{}/toggle", ids[1]), &csrf, &[])
        .await;
    assert!(res.body.contains("Disabled B."));
    assert!(res.body.contains("DISABLED"));
    let dash = c.get("/ui/dashboard").await.body;
    assert!(
        dash.contains("▸ A") && !dash.contains("▸ B"),
        "disabled printers hidden from dashboard"
    );
}

#[tokio::test]
async fn only_admins_manage_printers() {
    let (app, _c, _csrf) = admin().await;
    app.create_user("op", "operator password", Role::Operator).await;
    let mut op = app.client();
    op.login("op", "operator password").await;
    let csrf = op.session_csrf().await;
    assert_eq!(op.get("/ui/printers").await.status, StatusCode::FORBIDDEN);
    assert_eq!(op.get("/ui/printers/new").await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        add(&mut op, &csrf, "Sneaky", "127.0.0.1", "").await.status,
        StatusCode::FORBIDDEN
    );
    assert!(printers::list(&app.state.db).await.unwrap().is_empty());
}

// ── API keys ─────────────────────────────────────────────────────────

#[tokio::test]
async fn api_key_is_encrypted_and_never_rendered() {
    let mock = MockMoonraker::start_with(Some(SECRET_KEY), "ready").await;
    let (app, mut c, csrf) = admin().await;
    add(&mut c, &csrf, "Voron", &mock.url(), SECRET_KEY).await;
    let p = only_printer(&app).await;

    let sealed = p.api_key_enc.clone().unwrap();
    assert!(
        !sealed
            .windows(SECRET_KEY.len())
            .any(|w| w == SECRET_KEY.as_bytes())
    );
    assert_eq!(
        app.state.cipher.open(&sealed, API_KEY_CONTEXT).unwrap(),
        SECRET_KEY
    );

    let mut bodies = Vec::new();
    for path in [
        "/ui/printers",
        &format!("/ui/printers/{}", p.id),
        "/ui/dashboard",
        "/ui/account",
    ] {
        bodies.push(c.get(path).await.body);
        bodies.push(c.get_hx(path).await.body);
    }
    let test = c.post_hx(&format!("/printers/{}/test", p.id), &csrf, &[]).await;
    assert!(test.body.contains("READY"), "{}", test.body);
    bodies.push(test.body);
    for body in &bodies {
        assert!(!body.contains(SECRET_KEY), "API key leaked into a response");
    }
    let edit = c.get(&format!("/ui/printers/{}", p.id)).await.body;
    assert!(edit.contains("set; leave blank to keep"));

    // ...but it is sent upstream.
    assert!(
        mock.seen()
            .iter()
            .all(|s| s.api_key.as_deref() == Some(SECRET_KEY))
    );
}

#[tokio::test]
async fn api_key_keep_replace_clear() {
    let (app, mut c, csrf) = admin().await;
    add(&mut c, &csrf, "P", "127.0.0.1", "first-key").await;
    let id = only_printer(&app).await.id;
    let key = |app: &TestApp| {
        let state = app.state.clone();
        async move {
            let p = printers::get(&state.db, id).await.unwrap().unwrap();
            state.decrypt_api_key(&p).unwrap()
        }
    };
    let url = format!("/printers/{id}");
    let base = [("name", "P"), ("base_url", "127.0.0.1")];

    c.post_hx(&url, &csrf, &base).await;
    assert_eq!(key(&app).await.as_deref(), Some("first-key"), "blank keeps");

    c.post_hx(&url, &csrf, &[base[0], base[1], ("api_key", "second-key")])
        .await;
    assert_eq!(key(&app).await.as_deref(), Some("second-key"), "replaced");

    c.post_hx(&url, &csrf, &[base[0], base[1], ("clear_key", "on")])
        .await;
    assert_eq!(key(&app).await, None, "cleared");

    let detail: String =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'printer_update' ORDER BY id DESC")
            .fetch_one(&app.state.db)
            .await
            .unwrap();
    assert!(detail.contains("api_key=cleared"));
}

#[tokio::test]
async fn undecryptable_key_is_reported() {
    let (app, mut c, csrf) = admin().await;
    let id = printers::create(
        &app.state.db,
        NewPrinter {
            name: "Old",
            base_url: "http://127.0.0.1:1",
            api_key_enc: Some(vec![1, 2, 3]),
            webcam_override: None,
            enabled: true,
        },
    )
    .await
    .unwrap();
    assert!(c.get("/ui/printers").await.body.contains("unreadable"));
    let res = c.post_hx(&format!("/printers/{id}/test"), &csrf, &[]).await;
    assert!(
        res.body.contains("CONFIG") && res.body.contains("PRINTPIT_SECRET_KEY"),
        "{}",
        res.body
    );
}

// ── Test connection ──────────────────────────────────────────────────

#[tokio::test]
async fn test_connection_reports_state() {
    let (app, mut c, csrf) = admin().await;
    let ready = MockMoonraker::start().await;
    let startup = MockMoonraker::start_with(None, "startup").await;
    let locked = MockMoonraker::start_with(Some("right-key"), "ready").await;

    add(&mut c, &csrf, "ready", &ready.url(), "").await;
    add(&mut c, &csrf, "startup", &startup.url(), "").await;
    add(&mut c, &csrf, "nokey", &locked.url(), "").await;
    add(&mut c, &csrf, "wrongkey", &locked.url(), "wrong-key").await;
    add(&mut c, &csrf, "offline", "127.0.0.1:1", "").await;

    let expect = [
        (
            "ready",
            "READY",
            "Klipper v0.12.0-301-g1f8e3c3d · Moonraker v0.9.3-12 · host voron",
        ),
        ("startup", "STARTUP", "Klipper is startup: Klipper is starting up"),
        ("nokey", "UNAUTHORIZED", "API key"),
        ("wrongkey", "UNAUTHORIZED", "API key"),
        ("offline", "OFFLINE", "refused"),
    ];
    let all = printers::list(&app.state.db).await.unwrap();
    for (name, label, detail) in expect {
        let p = all.iter().find(|p| p.name == name).unwrap();
        let res = c.post_hx(&format!("/printers/{}/test", p.id), &csrf, &[]).await;
        assert_eq!(res.status, StatusCode::OK);
        assert!(res.body.contains(&format!(">{label}<")), "{name}: {}", res.body);
        assert!(res.body.contains(detail), "{name}: {}", res.body);
        assert!(
            !res.body.contains("127.0.0.1"),
            "{name}: address leaked: {}",
            res.body
        );
    }
    assert_eq!(
        ready.seen().iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        ["/server/info", "/printer/info"]
    );
}

#[tokio::test]
async fn egress_allowlist_enforced_at_connect_time() {
    // Allow only 10/8, then plant printers that skipped save-time validation: a loopback
    // literal, and a hostname that resolves to loopback (the DNS-rebinding case).
    let mock = MockMoonraker::start().await;
    let (app, mut c) = TestApp::with_admin_env(&[
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_PRINTER_CIDRS", "10.0.0.0/8"),
    ])
    .await;
    let csrf = c.session_csrf().await;
    for (name, url) in [
        ("literal", format!("http://127.0.0.1:{}", mock.port)),
        ("rebind", format!("http://localhost:{}", mock.port)),
    ] {
        let id = printers::create(
            &app.state.db,
            NewPrinter {
                name,
                base_url: &url,
                api_key_enc: None,
                webcam_override: None,
                enabled: true,
            },
        )
        .await
        .unwrap();
        let res = c.post_hx(&format!("/printers/{id}/test"), &csrf, &[]).await;
        assert!(res.body.contains(">BLOCKED<"), "{name}: {}", res.body);
    }
    assert!(
        mock.seen().is_empty(),
        "no request may reach a disallowed address"
    );
}

#[tokio::test]
async fn egress_bind_address_is_used() {
    let mock = MockMoonraker::start().await;
    for (bind, expect) in [("127.0.0.1", ">READY<"), ("192.0.2.1", ">OFFLINE<")] {
        let (_app, mut c) = TestApp::with_admin_env(&[
            ("PRINTPIT_SECURE_COOKIES", "false"),
            ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
            ("PRINTPIT_EGRESS_BIND", bind),
        ])
        .await;
        let csrf = c.session_csrf().await;
        add(&mut c, &csrf, "P", &mock.url(), "").await;
        // A non-local bind address can't be assigned, proving the setting is applied.
        let res = c.post_hx("/printers/1/test", &csrf, &[]).await;
        assert!(res.body.contains(expect), "bind {bind}: {}", res.body);
    }
}
