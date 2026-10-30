//! User management and the audit log viewer.

mod common;

use axum::http::StatusCode;

use common::{ADMIN, Client, TestApp};
use printpit::auth::Role;

async fn admin() -> (TestApp, Client, String) {
    let (app, mut c) = TestApp::with_admin().await;
    let csrf = c.session_csrf().await;
    (app, c, csrf)
}

async fn user_id(app: &TestApp, name: &str) -> i64 {
    printpit::db::users::by_username(&app.state.db, name)
        .await
        .unwrap()
        .unwrap()
        .id
}

// ── Users ────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_user_and_sign_in() {
    let (app, mut c, csrf) = admin().await;
    let res = c
        .post_hx(
            "/users",
            &csrf,
            &[
                ("username", "bob"),
                ("role", "operator"),
                ("password", "bob password"),
                ("confirm", "bob password"),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("Created bob (operator)."));
    let mut bob = app.client();
    assert_eq!(
        bob.login("bob", "bob password").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        bob.get("/ui/users").await.status,
        StatusCode::FORBIDDEN,
        "operators can't manage users"
    );

    for (fields, msg) in [
        (
            [
                ("username", "bob"),
                ("role", "viewer"),
                ("password", "x password"),
                ("confirm", "x password"),
            ],
            "taken",
        ),
        (
            [
                ("username", "BOB"),
                ("role", "viewer"),
                ("password", "x password"),
                ("confirm", "x password"),
            ],
            "taken",
        ),
        (
            [
                ("username", "bad name"),
                ("role", "viewer"),
                ("password", "x password"),
                ("confirm", "x password"),
            ],
            "Username",
        ),
        (
            [
                ("username", "carol"),
                ("role", "root"),
                ("password", "x password"),
                ("confirm", "x password"),
            ],
            "Pick a role",
        ),
        (
            [
                ("username", "carol"),
                ("role", "viewer"),
                ("password", "short"),
                ("confirm", "short"),
            ],
            "at least 8",
        ),
    ] {
        let res = c.post_hx("/users", &csrf, &fields).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{msg}");
        assert!(res.body.contains(msg), "expected {msg}");
    }
}

#[tokio::test]
async fn role_changes_apply_on_the_next_request() {
    let (app, mut c, csrf) = admin().await;
    app.create_user("dave", "dave password", Role::Viewer).await;
    let id = user_id(&app, "dave").await;
    let mut dave = app.client();
    dave.login("dave", "dave password").await;
    assert_eq!(dave.get("/ui/printers").await.status, StatusCode::FORBIDDEN);

    let res = c
        .post_hx(&format!("/users/{id}/role"), &csrf, &[("role", "admin")])
        .await;
    assert!(res.body.contains("dave is now admin."));
    assert_eq!(
        dave.get("/ui/printers").await.status,
        StatusCode::OK,
        "same session, new role"
    );
}

#[tokio::test]
async fn disable_and_reset_revoke_sessions() {
    let (app, mut c, csrf) = admin().await;
    app.create_user("erin", "erin password", Role::Operator).await;
    let id = user_id(&app, "erin").await;
    let mut erin = app.client();
    erin.login("erin", "erin password").await;

    let res = c.post_hx(&format!("/users/{id}/toggle"), &csrf, &[]).await;
    assert!(res.body.contains("Disabled erin."));
    assert_eq!(erin.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        erin.login("erin", "erin password").await.status,
        StatusCode::UNAUTHORIZED
    );

    c.post_hx(&format!("/users/{id}/toggle"), &csrf, &[]).await;
    erin.login("erin", "erin password").await;
    assert_eq!(erin.get("/ui/dashboard").await.status, StatusCode::OK);

    let res = c
        .post_hx(
            &format!("/users/{id}/password"),
            &csrf,
            &[("password", "fresh password"), ("confirm", "fresh password")],
        )
        .await;
    assert!(res.body.contains("Reset erin"), "{}", res.body);
    assert_eq!(
        erin.get("/ui/dashboard").await.status,
        StatusCode::SEE_OTHER,
        "signed out"
    );
    assert_eq!(
        erin.login("erin", "erin password").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        erin.login("erin", "fresh password").await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn admins_cannot_lock_themselves_or_everyone_out() {
    let (app, mut c, csrf) = admin().await;
    let me = user_id(&app, ADMIN).await;
    let list = c.get("/ui/users").await.body;
    assert!(list.contains("(you)"));
    for (path, fields) in [
        (format!("/users/{me}/role"), vec![("role", "viewer")]),
        (format!("/users/{me}/toggle"), vec![]),
        (
            format!("/users/{me}/password"),
            vec![("password", "whatever123"), ("confirm", "whatever123")],
        ),
    ] {
        let res = c.post_hx(&path, &csrf, &fields).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert!(res.body.contains("your own account"));
    }
    let res = c.delete_hx(&format!("/users/{me}"), &csrf).await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Another admin can be demoted (there are two), and operators can't touch users at all.
    app.create_user("frank", "frank password", Role::Admin).await;
    let frank = user_id(&app, "frank").await;
    let res = c
        .post_hx(&format!("/users/{frank}/role"), &csrf, &[("role", "operator")])
        .await;
    assert!(res.body.contains("frank is now operator."));
    let mut f = app.client();
    f.login("frank", "frank password").await;
    let fcsrf = f.session_csrf().await;
    assert_eq!(
        f.post_hx(&format!("/users/{me}/toggle"), &fcsrf, &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn disabled_admins_do_not_count_as_remaining() {
    let (app, mut c, csrf) = admin().await;
    app.create_user("ghost", "ghost password", Role::Admin).await;
    let ghost = user_id(&app, "ghost").await;
    printpit::db::users::set_disabled(&app.state.db, ghost, true)
        .await
        .unwrap();
    let me = user_id(&app, ADMIN).await;
    assert_eq!(
        printpit::db::users::other_active_admins(&app.state.db, me)
            .await
            .unwrap(),
        0
    );
    // Removing a disabled admin never removes an enabled one, so it's allowed.
    let res = c.delete_hx(&format!("/users/{ghost}"), &csrf).await;
    assert!(res.body.contains("Deleted ghost."));
}

#[tokio::test]
async fn delete_user() {
    let (app, mut c, csrf) = admin().await;
    app.create_user("hank", "hank password", Role::Viewer).await;
    let id = user_id(&app, "hank").await;
    let mut hank = app.client();
    hank.login("hank", "hank password").await;
    let res = c.delete_hx(&format!("/users/{id}"), &csrf).await;
    assert!(res.body.contains("Deleted hank."));
    assert_eq!(hank.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        app.scalar("SELECT COUNT(*) FROM sessions WHERE user_id NOT IN (SELECT id FROM users)")
            .await,
        0
    );
}

// ── Audit log ────────────────────────────────────────────────────────

#[tokio::test]
async fn audit_log_lists_and_filters() {
    let (app, mut c, csrf) = admin().await;
    c.post_hx(
        "/users",
        &csrf,
        &[
            ("username", "ivy"),
            ("role", "viewer"),
            ("password", "ivy password"),
            ("confirm", "ivy password"),
        ],
    )
    .await;
    let mut ivy = app.client();
    ivy.login("ivy", "wrong password").await;

    let res = c.get("/ui/audit").await;
    assert_eq!(res.status, StatusCode::OK);
    let b = &res.body;
    assert!(
        b.contains("<td>setup</td>")
            && b.contains("<td>user_create</td>")
            && b.contains("<td>login_failed</td>")
    );
    assert!(
        b.find("login_failed").unwrap() < b.find("<td>setup</td>").unwrap(),
        "newest first"
    );
    assert!(b.contains("username=ivy role=viewer"));
    assert!(b.contains(r#"<option value="user_create">user_create</option>"#));

    let res = c.get_hx("/ui/audit?action=user_create&user=&printer=").await;
    assert!(res.body.contains("<td>user_create</td>") && !res.body.contains("<td>setup</td>"));
    assert!(res.body.contains(r#"<option value="user_create" selected>"#));

    let ivy_id = user_id(&app, "ivy").await;
    let res = c.get_hx(&format!("/ui/audit?user={ivy_id}")).await;
    assert!(res.body.contains("login_failed") && !res.body.contains("user_create</td>"));

    assert_eq!(
        ivy.get("/ui/audit").await.status,
        StatusCode::SEE_OTHER,
        "ivy isn't signed in"
    );
}

#[tokio::test]
async fn audit_log_pages_by_id() {
    let (app, mut c, _csrf) = admin().await;
    for i in 0..120 {
        printpit::db::audit::record(
            &app.state.db,
            printpit::db::audit::Entry {
                user_id: None,
                printer_id: None,
                action: "test_event",
                detail: Some(format!("n={i}")),
                ip: None,
            },
        )
        .await;
    }
    let first = c.get("/ui/audit?action=test_event").await.body;
    assert_eq!(first.matches("<td>test_event</td>").count(), 50);
    assert!(first.contains("n=119") && !first.contains("n=69<"));
    let older = first
        .split(r#"hx-get="/ui/audit?"#)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&#38;", "&");
    let second = c.get(&format!("/ui/audit?{older}")).await.body;
    assert!(second.contains("n=69") && !second.contains("n=70<"));
    assert_eq!(second.matches("<td>test_event</td>").count(), 50);
}

#[tokio::test]
async fn admin_tabs_are_hidden_from_others() {
    let (app, mut c, _csrf) = admin().await;
    let strip = c.get("/ui/dashboard").await.body;
    assert!(strip.contains("[u]</span>Users") && strip.contains("[l]</span>Audit"));
    app.create_user("op", "operator password", Role::Operator).await;
    let mut op = app.client();
    op.login("op", "operator password").await;
    let strip = op.get("/ui/dashboard").await.body;
    assert!(!strip.contains("/ui/users") && !strip.contains("/ui/audit"));
    assert_eq!(op.get("/ui/audit").await.status, StatusCode::FORBIDDEN);
}

// ── Preferences ──────────────────────────────────────────────────────

#[tokio::test]
async fn theme_preference_is_remembered() {
    let (_app, mut c, csrf) = admin().await;
    assert!(
        c.get("/ui/dashboard")
            .await
            .body
            .starts_with("<!doctype html>\n<html lang=\"en\">"),
        "auto by default"
    );
    let res = c.post_hx("/account/theme", &csrf, &[("theme", "light")]).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(res.header("hx-refresh"), "true");
    assert!(
        c.get("/ui/dashboard")
            .await
            .body
            .contains(r#"<html lang="en" data-theme="light">"#)
    );
    assert!(
        c.get("/ui/account")
            .await
            .body
            .contains(r#"<option value="light" selected>"#)
    );
    c.post_hx("/account/theme", &csrf, &[("theme", "auto")]).await;
    assert!(!c.get("/ui/dashboard").await.body.contains("data-theme"));
    assert_eq!(
        c.post_hx("/account/theme", &csrf, &[("theme", "neon")])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn help_overlay_lists_role_appropriate_keys() {
    let (app, mut c, _csrf) = admin().await;
    let body = c.get("/ui/dashboard").await.body;
    assert!(body.contains(r#"<dialog id="help""#) && body.contains("printers · users · audit log"));
    app.create_user("vic", "viewer password", Role::Viewer).await;
    let mut v = app.client();
    v.login("vic", "viewer password").await;
    assert!(
        !v.get("/ui/dashboard")
            .await
            .body
            .contains("printers · users · audit log")
    );
}
