//! Authentication, sessions, CSRF, roles and rate limiting.

mod common;

use axum::http::{Method, StatusCode};

use common::{ADMIN, ADMIN_PW, TestApp};
use printpit::auth::Role;
use printpit::auth::ratelimit::MAX_PER_USER;

/// Every route that requires a session. Keep in sync with `web::router`.
const PROTECTED_GET: &[&str] = &[
    "/",
    "/ui/dashboard",
    "/ui/files",
    "/ui/logs",
    "/ui/system",
    "/ui/printers",
    "/ui/printers/new",
    "/ui/printers/1",
    "/ui/account",
    "/ui/dashboard/stream",
    "/p/1/status",
    "/p/1/start",
    "/p/1/camera/snapshot",
    "/p/1/camera/stream",
    "/p/1/files",
    "/p/1/files/meta?path=a.gcode",
    "/p/1/files/thumb?path=a.png",
    "/p/1/files/download?path=a.gcode",
    "/p/1/logs",
    "/p/1/logs/view?name=klippy.log",
    "/p/1/logs/tail?name=klippy.log",
    "/p/1/logs/download?name=klippy.log",
];
const PROTECTED_POST: &[&str] = &[
    "/logout",
    "/account/password",
    "/printers",
    "/printers/1",
    "/printers/1/toggle",
    "/printers/1/move/up",
    "/printers/1/test",
    "/account/prefs",
    "/p/1/print/pause",
    "/p/1/estop",
    "/p/1/files/mkdir",
    "/p/1/files/move",
    "/p/1/files/upload",
];
const PROTECTED_DELETE: &[&str] = &["/printers/1", "/p/1/files?path=a.gcode"];

// ── First-run setup ──────────────────────────────────────────────────

#[tokio::test]
async fn fresh_instance_sends_everything_to_setup() {
    let app = TestApp::new().await;
    let mut c = app.client();
    for path in ["/", "/ui/dashboard", "/login", "/ui/account"] {
        let res = c.get(path).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(res.location(), "/setup", "{path}");
    }
    let res = c.get_hx("/ui/files").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert_eq!(res.header("hx-redirect"), "/setup");
    assert_eq!(c.get("/healthz").await.status, StatusCode::OK);
    let page = c.get("/setup").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("setup token"));
}

#[tokio::test]
async fn setup_rejects_bad_token_csrf_and_input() {
    let app = TestApp::new().await;
    let mut c = app.client();
    let csrf = c.get("/setup").await.csrf();
    let token = app.state.setup.token().unwrap();
    let post = |token: String, csrf: String, user: &'static str, pw: &'static str, confirm: &'static str| {
        vec![
            ("_csrf".to_string(), csrf),
            ("token".to_string(), token),
            ("username".to_string(), user.to_string()),
            ("password".to_string(), pw.to_string()),
            ("confirm".to_string(), confirm.to_string()),
        ]
    };
    let cases = [
        (
            post(
                "0000-0000-0000-0000-0000".into(),
                csrf.clone(),
                "admin",
                ADMIN_PW,
                ADMIN_PW,
            ),
            StatusCode::FORBIDDEN,
            "Wrong setup token",
        ),
        (
            post(token.clone(), "forged".into(), "admin", ADMIN_PW, ADMIN_PW),
            StatusCode::FORBIDDEN,
            "expired",
        ),
        (
            post(token.clone(), csrf.clone(), "bad name", ADMIN_PW, ADMIN_PW),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Username",
        ),
        (
            post(token.clone(), csrf.clone(), "admin", "short", "short"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "at least 8",
        ),
        (
            post(token.clone(), csrf.clone(), "admin", ADMIN_PW, "different pw"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "match",
        ),
    ];
    for (fields, status, msg) in cases {
        let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let res = c.post("/setup", &fields).await;
        assert_eq!(res.status, status, "{msg}: {}", res.body);
        assert!(res.body.contains(msg), "expected `{msg}` in {}", res.body);
    }
    assert_eq!(app.scalar("SELECT COUNT(*) FROM users").await, 0);
    assert!(!app.state.setup.is_complete());
}

#[tokio::test]
async fn setup_creates_admin_and_signs_in() {
    let app = TestApp::new().await;
    let mut c = app.client();
    let res = c.setup(ADMIN, ADMIN_PW).await;
    assert_eq!(res.location(), "/");
    assert!(app.state.setup.is_complete());
    assert_eq!(app.state.setup.token(), None);
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM users WHERE role = 'admin'")
            .await,
        1
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'setup'")
            .await,
        1
    );

    let dash = c.get("/ui/dashboard").await;
    assert_eq!(dash.status, StatusCode::OK);
    assert!(dash.body.contains("admin <span class=\"dim\">(admin)</span>"));

    // Setup is closed for good, even for anonymous visitors.
    let mut anon = app.client();
    assert_eq!(anon.get("/setup").await.location(), "/login");
    assert_eq!(anon.post("/setup", &[("token", "x")]).await.location(), "/login");
}

#[tokio::test]
async fn first_admin_creation_is_atomic() {
    let app = TestApp::new().await;
    let db = &app.state.db;
    assert!(
        printpit::db::users::create_first_admin(db, "a", "h")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        printpit::db::users::create_first_admin(db, "b", "h")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn setup_state_survives_restart() {
    let app = TestApp::new().await;
    app.client().setup(ADMIN, ADMIN_PW).await;
    let reloaded = printpit::auth::setup::SetupState::init(&app.state.db)
        .await
        .unwrap();
    assert!(reloaded.is_complete());
    assert_eq!(reloaded.token(), None);
}

// ── Authentication required ──────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_requests_are_rejected_everywhere() {
    let (app, _admin) = TestApp::with_admin().await;
    let mut anon = app.client();
    for path in PROTECTED_GET {
        let res = anon.get(path).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}");
        assert!(
            res.location().starts_with("/login"),
            "{path} -> {}",
            res.location()
        );
        let res = anon.get_hx(path).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(res.header("hx-redirect"), "/login", "{path}");
    }
    for path in PROTECTED_POST {
        let res = anon.post(path, &[("_csrf", "x")]).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}");
        assert!(res.location().starts_with("/login"), "{path}");
    }
    for path in PROTECTED_DELETE {
        let res = anon.delete_hx(path, "x").await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    // A garbage session cookie is treated the same way, and cleared.
    anon.cookies.insert("printpit_session".into(), "garbage".into());
    let res = anon.get("/ui/files").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(!anon.cookies.contains_key("printpit_session"));
}

#[tokio::test]
async fn login_redirect_preserves_destination() {
    let (app, _admin) = TestApp::with_admin().await;
    let mut c = app.client();
    let res = c.get("/ui/logs?printer=3").await;
    assert_eq!(res.location(), "/login?next=%2Fui%2Flogs%3Fprinter%3D3");
    let page = c.get(res.location()).await;
    assert!(page.body.contains(r#"name="next" value="/ui/logs?printer=3""#));
    let res = c.login_next(ADMIN, ADMIN_PW, Some("/ui/logs?printer=3")).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/ui/logs?printer=3");
}

#[tokio::test]
async fn login_blocks_open_redirects() {
    let (app, _admin) = TestApp::with_admin().await;
    for evil in ["//evil.example", "https://evil.example", "/\\evil.example"] {
        let mut c = app.client();
        let res = c.login_next(ADMIN, ADMIN_PW, Some(evil)).await;
        assert_eq!(res.location(), "/", "{evil}");
    }
}

#[tokio::test]
async fn login_success_and_failure() {
    let (app, _admin) = TestApp::with_admin().await;
    let mut c = app.client();

    let res = c.login(ADMIN, "wrong password").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(res.body.contains("Invalid username or password."));
    let res = c.login("nobody", "wrong password").await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(
        res.body.contains("Invalid username or password."),
        "same message for unknown users"
    );
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'login_failed'")
            .await,
        2
    );

    // Usernames are case-insensitive.
    let res = c.login("ADMIN", ADMIN_PW).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/");
    assert_eq!(c.get("/ui/dashboard").await.status, StatusCode::OK);

    // Visiting /login while signed in goes straight through.
    assert_eq!(c.get("/login").await.location(), "/");
}

#[tokio::test]
async fn login_requires_pre_session_csrf() {
    let (app, _admin) = TestApp::with_admin().await;
    let mut c = app.client();
    // No cookie at all.
    let res = c
        .post(
            "/login",
            &[("_csrf", "x"), ("username", ADMIN), ("password", ADMIN_PW)],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(res.body.contains("expired"));
    // Cookie present (set by that response) but the form value doesn't match.
    let res = c
        .post(
            "/login",
            &[("_csrf", "forged"), ("username", ADMIN), ("password", ADMIN_PW)],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(!c.cookies.contains_key("printpit_session"));
}

#[tokio::test]
async fn login_replaces_existing_session() {
    let (app, mut c) = TestApp::with_admin().await;
    // Fetch a login form as if signed out, then submit it while still holding the old cookie.
    let old = c.cookies.remove("printpit_session").unwrap();
    let csrf = c.get("/login").await.csrf();
    c.cookies.insert("printpit_session".into(), old.clone());
    let res = c
        .post(
            "/login",
            &[("_csrf", &csrf), ("username", ADMIN), ("password", ADMIN_PW)],
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_ne!(c.cookies["printpit_session"], old);
    assert_eq!(app.scalar("SELECT COUNT(*) FROM sessions").await, 1);
}

// ── Cookies ──────────────────────────────────────────────────────────

#[tokio::test]
async fn session_cookie_flags_when_secure() {
    let app = TestApp::with_env(&[]).await; // secure cookies on (the default)
    let mut c = app.client();
    let page = c.get("/setup").await;
    let pre = page.set_cookies().join("\n");
    assert!(pre.contains("__Host-printpit_csrf="), "{pre}");
    let res = c.setup(ADMIN, ADMIN_PW).await;
    let cookie = res
        .set_cookies()
        .into_iter()
        .find(|c| c.starts_with("__Host-printpit_session="))
        .expect("session cookie");
    for flag in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/", "Max-Age="] {
        assert!(cookie.contains(flag), "missing {flag} in {cookie}");
    }
    assert!(!cookie.contains("Domain"), "__Host- cookies must not set Domain");
}

#[tokio::test]
async fn session_token_is_stored_hashed() {
    let (app, c) = TestApp::with_admin().await;
    let token = c.cookies["printpit_session"].clone();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT id_hash FROM sessions")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token.as_bytes());
    assert_eq!(stored, printpit::crypto::sha256(token.as_bytes()));
}

// ── CSRF ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn csrf_required_on_state_changes() {
    let (app, mut c) = TestApp::with_admin().await;
    let token = c.session_csrf().await;

    // Missing / wrong token.
    let res = c.post("/logout", &[]).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(res.body.contains("Security token"));
    assert_eq!(
        c.post("/logout", &[("_csrf", "wrong")]).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        c.post_with("/logout", &[], &[("x-csrf-token", "wrong")])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // JSON / multipart without the header is refused rather than parsed.
    let res = c
        .send(
            Method::POST,
            "/logout",
            &[("content-type", "application/json")],
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // A correct token from a cross-site context is still refused.
    let res = c
        .post_with(
            "/logout",
            &[("_csrf", &token)],
            &[("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM sessions").await,
        1,
        "still logged in"
    );

    // Header (htmx) path works.
    let res = c
        .post_with(
            "/logout",
            &[],
            &[("x-csrf-token", &token), ("hx-request", "true")],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.header("hx-redirect"), "/login");
    assert_eq!(app.scalar("SELECT COUNT(*) FROM sessions").await, 0);

    // Form-field path works.
    c.login(ADMIN, ADMIN_PW).await;
    let token = c.session_csrf().await;
    let res = c.post("/logout", &[("_csrf", &token)]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/login");
    assert_eq!(c.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn csrf_token_is_on_every_page_for_htmx() {
    let (_app, mut c) = TestApp::with_admin().await;
    let token = c.session_csrf().await;
    let body = c.get("/ui/dashboard").await.body;
    assert!(body.contains(&format!(r#"hx-headers='{{"X-CSRF-Token":"{token}"}}'"#)));
}

// ── Session expiry ───────────────────────────────────────────────────

#[tokio::test]
async fn idle_session_expires() {
    let (app, mut c) = TestApp::with_admin().await;
    app.exec("UPDATE sessions SET last_seen_at = last_seen_at - 720 * 60")
        .await;
    let res = c.get("/ui/dashboard").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM sessions").await,
        0,
        "deleted on sight"
    );
}

#[tokio::test]
async fn absolute_session_lifetime_expires() {
    let (app, mut c) = TestApp::with_admin().await;
    app.exec("UPDATE sessions SET expires_at = unixepoch() - 1").await;
    assert_eq!(c.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn activity_extends_idle_window() {
    let (app, mut c) = TestApp::with_admin().await;
    app.exec("UPDATE sessions SET last_seen_at = last_seen_at - 600")
        .await;
    assert_eq!(c.get("/ui/dashboard").await.status, StatusCode::OK);
    let age = app
        .scalar("SELECT unixepoch() - last_seen_at FROM sessions")
        .await;
    assert!(age < 5, "last_seen_at refreshed, age {age}");
}

#[tokio::test]
async fn expired_sessions_are_purged() {
    let (app, _c) = TestApp::with_admin().await;
    app.exec("UPDATE sessions SET expires_at = 0").await;
    let removed = printpit::db::sessions::delete_expired(&app.state.db, app.state.config.session_idle)
        .await
        .unwrap();
    assert_eq!(removed, 1);
}

// ── Rate limiting ────────────────────────────────────────────────────

#[tokio::test]
async fn repeated_failures_lock_the_username() {
    let (app, _admin) = TestApp::with_admin().await;
    let mut c = app.client();
    for _ in 0..MAX_PER_USER {
        assert_eq!(
            c.login(ADMIN, "wrong password").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    // Even the right password is refused while locked.
    let res = c.login(ADMIN, ADMIN_PW).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(res.body.contains("Too many failed attempts"));
}

// ── Account / password change ────────────────────────────────────────

#[tokio::test]
async fn password_change_revokes_other_sessions() {
    let (app, mut c) = TestApp::with_admin().await;
    let mut other = app.client();
    other.login(ADMIN, ADMIN_PW).await;
    assert_eq!(app.scalar("SELECT COUNT(*) FROM sessions").await, 2);

    let token = c.session_csrf().await;
    let hx = [("x-csrf-token", token.as_str()), ("hx-request", "true")];
    let res = c
        .post_with(
            "/account/password",
            &[
                ("current", "nope nope"),
                ("new", "new password!"),
                ("confirm", "new password!"),
            ],
            &hx,
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(res.body.contains("Current password is wrong."));

    let res = c
        .post_with(
            "/account/password",
            &[("current", ADMIN_PW), ("new", "short"), ("confirm", "short")],
            &hx,
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);

    let res = c
        .post_with(
            "/account/password",
            &[
                ("current", ADMIN_PW),
                ("new", "new password!"),
                ("confirm", "new password!"),
            ],
            &hx,
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Password changed."));

    assert_eq!(
        c.get("/ui/account").await.status,
        StatusCode::OK,
        "own session kept"
    );
    assert_eq!(
        other.get("/ui/account").await.status,
        StatusCode::SEE_OTHER,
        "other session revoked"
    );
    let mut fresh = app.client();
    assert_eq!(
        fresh.login(ADMIN, ADMIN_PW).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fresh.login(ADMIN, "new password!").await.status,
        StatusCode::SEE_OTHER
    );
}

// ── Roles and disabled users ─────────────────────────────────────────

#[tokio::test]
async fn viewer_cannot_open_admin_tabs() {
    let (app, _admin) = TestApp::with_admin().await;
    app.create_user("viewer", "viewer password", Role::Viewer).await;
    let mut v = app.client();
    v.login("viewer", "viewer password").await;

    let dash = v.get("/ui/dashboard").await;
    assert_eq!(dash.status, StatusCode::OK);
    assert!(
        !dash.body.contains("/ui/printers"),
        "printers tab hidden from viewers"
    );
    assert_eq!(v.get("/ui/printers").await.status, StatusCode::FORBIDDEN);
    assert_eq!(v.get_hx("/ui/printers").await.status, StatusCode::FORBIDDEN);
    assert_eq!(v.get("/ui/account").await.status, StatusCode::OK);
}

#[tokio::test]
async fn admin_sees_printers_tab() {
    let (_app, mut c) = TestApp::with_admin().await;
    assert!(c.get("/ui/dashboard").await.body.contains("/ui/printers"));
    assert_eq!(c.get("/ui/printers").await.status, StatusCode::OK);
}

#[tokio::test]
async fn disabled_user_is_locked_out() {
    let (app, _admin) = TestApp::with_admin().await;
    let id = app.create_user("op", "operator password", Role::Operator).await;
    let mut op = app.client();
    op.login("op", "operator password").await;
    assert_eq!(op.get("/ui/dashboard").await.status, StatusCode::OK);

    printpit::db::users::set_disabled(&app.state.db, id, true)
        .await
        .unwrap();
    assert_eq!(op.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        op.login("op", "operator password").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[cfg(unix)]
#[tokio::test]
async fn database_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let app = TestApp::new().await;
    for name in ["printpit.db", "printpit.db-wal", "printpit.db-shm"] {
        let mode = std::fs::metadata(app.state.config.data_dir.join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}
