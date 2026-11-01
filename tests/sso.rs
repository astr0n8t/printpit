//! Trusted-header SSO: who is trusted, which users can sign in, switching users, and that
//! CSRF, TOTP and logout behave.

mod common;

use axum::http::StatusCode;

use common::{ADMIN, ADMIN_PW, Client, TestApp};
use printpit::auth::Role;

const PROXY: &str = "172.30.0.2:40000";
const STRANGER: &str = "10.9.9.9:40000";

async fn app_with(extra: &[(&str, &str)]) -> TestApp {
    let mut env = vec![
        ("PRINTPIT_SECURE_COOKIES", "false"),
        ("PRINTPIT_AUTH_HEADER", "Remote-User"),
        ("PRINTPIT_AUTH_PROXY_CIDRS", "172.30.0.2"),
    ];
    env.extend_from_slice(extra);
    let (app, _admin) = TestApp::with_admin_env(&env).await;
    app.create_user("op", "operator password", Role::Operator).await;
    app
}

/// A GET carrying the auth proxy's header.
async fn get_as(c: &mut Client, uri: &str, user: &str) -> common::Resp {
    c.send(axum::http::Method::GET, uri, &[("remote-user", user)], None)
        .await
}

fn via(app: &TestApp, peer: &str) -> Client {
    let mut c = app.client();
    c.peer = Some(peer.parse().unwrap());
    c
}

async fn last_login_detail(app: &TestApp) -> Option<String> {
    sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'login' ORDER BY id DESC LIMIT 1")
        .fetch_one(&app.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn proxy_header_signs_in_an_existing_user() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    let res = get_as(&mut c, "/ui/account", "op").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("<dd>op</dd>"));
    assert!(
        c.cookies.contains_key("printpit_session"),
        "a normal session is created"
    );
    assert_eq!(last_login_detail(&app).await.as_deref(), Some("sso"));

    // The session carries on; the next request doesn't create another.
    let sessions = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions")
            .fetch_one(&app.state.db)
            .await
            .unwrap()
    };
    let before = sessions().await;
    assert_eq!(get_as(&mut c, "/ui/dashboard", "op").await.status, StatusCode::OK);
    assert_eq!(sessions().await, before);
    // Names match case-insensitively, like usernames.
    assert_eq!(get_as(&mut c, "/ui/dashboard", "OP").await.status, StatusCode::OK);
    assert_eq!(sessions().await, before);
}

#[tokio::test]
async fn header_from_anyone_else_is_ignored() {
    let app = app_with(&[("PRINTPIT_TRUSTED_PROXIES", "10.9.9.9")]).await;
    let mut c = via(&app, STRANGER);
    let res = get_as(&mut c, "/ui/dashboard", ADMIN).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"));
    // Not even with X-Forwarded-For claiming to come through the proxy: only the direct peer
    // counts, even when that peer is a trusted proxy for client-IP purposes.
    let res = c
        .send(
            axum::http::Method::GET,
            "/ui/dashboard",
            &[("remote-user", ADMIN), ("x-forwarded-for", "172.30.0.2")],
            None,
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    // No peer at all (e.g. a unix socket): ignored too.
    let mut none = app.client();
    assert_eq!(
        get_as(&mut none, "/ui/dashboard", ADMIN).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn unknown_and_disabled_users_are_refused() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    let res = get_as(&mut c, "/ui/dashboard", "mallory").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(res.body.contains("no enabled user by that name"), "{}", res.body);
    assert!(!c.cookies.contains_key("printpit_session"));
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(users, 2, "never creates users");

    sqlx::query("UPDATE users SET disabled = 1 WHERE username = 'op'")
        .execute(&app.state.db)
        .await
        .unwrap();
    assert_eq!(
        get_as(&mut c, "/ui/dashboard", "op").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get_as(&mut c, "/ui/dashboard", "bad name!").await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_different_header_user_replaces_the_session() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    get_as(&mut c, "/ui/dashboard", ADMIN).await;
    let admin_cookie = c.cookies["printpit_session"].clone();

    let res = get_as(&mut c, "/ui/account", "op").await;
    assert!(res.body.contains("<dd>op</dd>"), "{}", res.body);
    assert_ne!(c.cookies["printpit_session"], admin_cookie);
    // The admin's session is gone, not just shadowed.
    let mut replay = app.client();
    replay.cookies.insert("printpit_session".into(), admin_cookie);
    assert_eq!(replay.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn without_the_header_password_login_still_works() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    let res = c.get("/ui/dashboard").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"));
    assert_eq!(c.login(ADMIN, ADMIN_PW).await.location(), "/");
    assert_eq!(c.get("/ui/dashboard").await.status, StatusCode::OK);
    assert_eq!(last_login_detail(&app).await, None);
}

#[tokio::test]
async fn login_page_skips_the_form_for_header_users() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    let res = get_as(&mut c, "/login?next=/ui/files", "op").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/ui/files");
    // From anyone else the form is shown as usual.
    let mut s = via(&app, STRANGER);
    assert_eq!(get_as(&mut s, "/login", "op").await.status, StatusCode::OK);
}

#[tokio::test]
async fn csrf_is_still_required() {
    let app = app_with(&[]).await;
    let mut c = via(&app, PROXY);
    // A cross-site POST that arrives with the proxy's header still has no CSRF token.
    let res = c
        .post_with("/account/theme", &[("theme", "dark")], &[("remote-user", "op")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let page = get_as(&mut c, "/ui/account", "op").await;
    let res = c
        .post_with(
            "/account/theme",
            &[("theme", "dark")],
            &[("remote-user", "op"), ("x-csrf-token", &page.csrf())],
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn header_sign_in_skips_totp() {
    let app = app_with(&[]).await;
    // Pretend op has 2FA on (the sealed secret's contents don't matter here).
    sqlx::query("UPDATE users SET totp_secret_enc = x'01' WHERE username = 'op'")
        .execute(&app.state.db)
        .await
        .unwrap();
    let mut c = via(&app, PROXY);
    assert_eq!(get_as(&mut c, "/ui/dashboard", "op").await.status, StatusCode::OK);
    // A password login for the same user still asks for the code.
    let mut p = via(&app, STRANGER);
    assert_eq!(p.login("op", "operator password").await.location(), "/login/totp");
}

#[tokio::test]
async fn logout_goes_to_the_proxy() {
    let app = app_with(&[("PRINTPIT_AUTH_LOGOUT_URL", "https://auth.example.com/logout")]).await;
    let mut c = via(&app, PROXY);
    let page = get_as(&mut c, "/ui/account", "op").await;
    let res = c.post_with("/logout", &[("_csrf", &page.csrf())], &[]).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "https://auth.example.com/logout");
    let res = c
        .post_with("/logout", &[], &[("hx-request", "true"), ("remote-user", "op")])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "no CSRF token");
}
