//! Two-factor sign-in: enrolment, the two-step login, replay protection, recovery codes,
//! turning it off, admin reset and rate limiting.

mod common;

use axum::http::StatusCode;

use common::{ADMIN, ADMIN_PW, Client, TestApp};
use printpit::auth::Role;
use printpit::auth::totp::{base32_decode, code_at, step};
use printpit::db;

/// What `enroll` set up. Codes are relative to the step that confirmed setup (which is then
/// used up), so the tests don't depend on where in a 30 s window they run.
struct Enrolled {
    secret: Vec<u8>,
    codes: Vec<String>,
    step: i64,
}

impl Enrolled {
    fn code(&self, offset: i64) -> String {
        format!("{:06}", code_at(&self.secret, self.step + offset))
    }
}

/// Text between `start` and the next `end` in `body`.
fn between<'a>(body: &'a str, start: &str, end: &str) -> &'a str {
    let i = body
        .find(start)
        .unwrap_or_else(|| panic!("`{start}` not in {body}"))
        + start.len();
    &body[i..i + body[i..].find(end).unwrap()]
}

/// Sets up 2FA for the signed-in client, confirming with the current step's code, so later
/// logins in the same test use `code(1)`.
async fn enroll(c: &mut Client) -> Enrolled {
    let csrf = c.session_csrf().await;
    let setup = c.get_hx("/account/totp/setup").await;
    assert_eq!(setup.status, StatusCode::OK);
    assert!(setup.body.contains("<svg"), "QR code");
    let key = between(&setup.body, r#"<code class="totp-key">"#, "</code>");
    let secret = base32_decode(key).unwrap();
    assert_eq!(secret.len(), 20);
    let enroll = between(&setup.body, r#"name="enroll" value=""#, "\"").to_string();

    let wrong = c
        .post_hx(
            "/account/totp/enable",
            &csrf,
            &[("enroll", &enroll), ("code", "000000")],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wrong.body.contains("didn&#39;t match") || wrong.body.contains("didn't match"));

    let step = step(db::now());
    let res = c
        .post_hx(
            "/account/totp/enable",
            &csrf,
            &[
                ("enroll", &enroll),
                ("code", &format!("{:06}", code_at(&secret, step))),
            ],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let codes: Vec<String> = between(&res.body, r#"<pre class="recovery-codes">"#, "</pre>")
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(codes.len(), 10);
    Enrolled { secret, codes, step }
}

/// The password step for a 2FA user: redirected to the code page, no session yet.
async fn password_step(c: &mut Client, user: &str, pw: &str, next: Option<&str>) {
    let res = c.login_next(user, pw, next).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/login/totp");
}

async fn code_step(c: &mut Client, entry: &str) -> common::Resp {
    let page = c.get("/login/totp").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let csrf = page.csrf();
    c.post("/login/totp", &[("_csrf", &csrf), ("code", entry)]).await
}

async fn audit_actions(app: &TestApp, like: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE ? ORDER BY id")
        .bind(like)
        .fetch_all(&app.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn enrol_then_sign_in_with_a_code() {
    let (app, mut c) = TestApp::with_admin().await;
    let mut other = app.client();
    other.login(ADMIN, ADMIN_PW).await;
    let e = enroll(&mut c).await;

    // Turning it on signs out other sessions, and the account page no longer shows the key.
    assert_eq!(other.get("/ui/dashboard").await.status, StatusCode::SEE_OTHER);
    let account = c.get_hx("/ui/account").await.body;
    assert!(account.contains(">ON</span>") && account.contains("10 recovery codes left"));
    assert!(!account.contains("totp-key") && !account.contains("<svg"));

    let mut fresh = app.client();
    password_step(&mut fresh, ADMIN, ADMIN_PW, Some("/ui/files")).await;
    // Half-way through, there is no session.
    let res = fresh.get("/ui/dashboard").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(res.location().starts_with("/login"));

    let res = code_step(&mut fresh, &e.code(1)).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
    assert_eq!(res.location(), "/ui/files", "next survives the code step");
    assert_eq!(fresh.get("/ui/dashboard").await.status, StatusCode::OK);
    assert!(
        !fresh.cookies.contains_key("printpit_2fa"),
        "pending cookie cleared"
    );

    let logins: Vec<Option<String>> =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'login' ORDER BY id DESC LIMIT 1")
            .fetch_all(&app.state.db)
            .await
            .unwrap();
    assert_eq!(logins, [Some("2fa=code".to_string())]);
    assert_eq!(audit_actions(&app, "totp_enable").await.len(), 1);
}

#[tokio::test]
async fn codes_cannot_be_replayed() {
    let (app, mut c) = TestApp::with_admin().await;
    let e = enroll(&mut c).await;

    // The code that confirmed setup is already used.
    let mut a = app.client();
    password_step(&mut a, ADMIN, ADMIN_PW, None).await;
    let res = code_step(&mut a, &e.code(0)).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    let next = e.code(1);
    assert_eq!(code_step(&mut a, &next).await.status, StatusCode::SEE_OTHER);

    // The same code again, and an older one, both fail.
    let mut b = app.client();
    password_step(&mut b, ADMIN, ADMIN_PW, None).await;
    assert_eq!(code_step(&mut b, &next).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        code_step(&mut b, &e.code(-1)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        code_step(&mut b, "not a code").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(audit_actions(&app, "totp_failed").await.len(), 4);
}

#[tokio::test]
async fn recovery_codes_work_once() {
    let (app, mut c) = TestApp::with_admin().await;
    let e = enroll(&mut c).await;
    let codes = &e.codes;

    let mut a = app.client();
    password_step(&mut a, ADMIN, ADMIN_PW, None).await;
    let res = code_step(&mut a, &codes[3].to_uppercase()).await;
    assert_eq!(
        res.status,
        StatusCode::SEE_OTHER,
        "case-insensitive: {}",
        res.body
    );
    assert_eq!(audit_actions(&app, "totp_recovery_used").await.len(), 1);

    let mut b = app.client();
    password_step(&mut b, ADMIN, ADMIN_PW, None).await;
    assert_eq!(
        code_step(&mut b, &codes[3]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        a.get_hx("/ui/account")
            .await
            .body
            .contains("9 recovery codes left")
    );
}

#[tokio::test]
async fn code_page_needs_a_pending_login() {
    let (app, _c) = TestApp::with_admin().await;
    let mut anon = app.client();
    let res = anon.get("/login/totp").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.location(), "/login");

    // A forged pending cookie is ignored.
    anon.cookies.insert("printpit_2fa".into(), "AAAA".into());
    assert_eq!(anon.get("/login/totp").await.location(), "/login");
    let page = anon.get("/login").await;
    let res = anon
        .post("/login/totp", &[("_csrf", &page.csrf()), ("code", "123456")])
        .await;
    assert_eq!(res.location(), "/login");

    // Users without 2FA skip the code page entirely.
    let mut plain = app.client();
    let res = plain.login(ADMIN, ADMIN_PW).await;
    assert_eq!(res.location(), "/");
}

#[tokio::test]
async fn wrong_codes_are_rate_limited() {
    let (app, mut c) = TestApp::with_admin().await;
    let e = enroll(&mut c).await;
    let mut a = app.client();
    password_step(&mut a, ADMIN, ADMIN_PW, None).await;
    let mut limited = false;
    for _ in 0..12 {
        let res = code_step(&mut a, "000000").await;
        if res.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited, "code guessing must hit the login limiter");
    // Even the right code is refused while limited.
    let res = code_step(&mut a, &e.code(1)).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn turning_off_and_new_recovery_codes_need_password_and_code() {
    let (app, mut c) = TestApp::with_admin().await;
    let e = enroll(&mut c).await;
    let codes = &e.codes;
    let csrf = c.session_csrf().await;

    let res = c
        .post_hx(
            "/account/totp/recovery",
            &csrf,
            &[("password", "wrong password"), ("code", &codes[0])],
        )
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(res.body.contains("password is wrong"));

    let res = c
        .post_hx(
            "/account/totp/recovery",
            &csrf,
            &[("password", ADMIN_PW), ("code", &e.code(1))],
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let new_codes = between(&res.body, r#"<pre class="recovery-codes">"#, "</pre>").to_string();
    assert!(!new_codes.contains(&codes[1]), "old codes replaced");

    // Old recovery codes no longer work for turning it off; a new one does.
    let res = c
        .post_hx(
            "/account/totp/disable",
            &csrf,
            &[("password", ADMIN_PW), ("code", &codes[1])],
        )
        .await;
    assert!(res.body.contains("didn&#39;t work") || res.body.contains("didn't work"));
    let fresh = new_codes.lines().next().unwrap();
    let res = c
        .post_hx(
            "/account/totp/disable",
            &csrf,
            &[("password", ADMIN_PW), ("code", fresh)],
        )
        .await;
    assert!(res.body.contains("Two-factor sign-in is off."), "{}", res.body);
    let mut fresh = app.client();
    assert_eq!(
        fresh.login(ADMIN, ADMIN_PW).await.location(),
        "/",
        "password only again"
    );
    assert_eq!(
        audit_actions(&app, "totp_%").await,
        [
            "totp_enable",
            "totp_recovery_reset",
            "totp_failed",
            "totp_disable"
        ]
    );
}

#[tokio::test]
async fn admin_resets_a_users_2fa() {
    let (app, mut admin) = TestApp::with_admin().await;
    let id = app.create_user("op", "operator password", Role::Operator).await;
    let mut op = app.client();
    op.login("op", "operator password").await;
    enroll(&mut op).await;

    let list = admin.get_hx("/ui/users").await.body;
    assert!(list.contains(&format!(r#"hx-post="/users/{id}/totp/reset""#)));
    let csrf = admin.session_csrf().await;
    let res = admin
        .post_hx(&format!("/users/{id}/totp/reset"), &csrf, &[])
        .await;
    assert!(
        res.body.contains("Turned off two-factor sign-in for op"),
        "{}",
        res.body
    );
    assert_eq!(
        op.get("/ui/dashboard").await.status,
        StatusCode::SEE_OTHER,
        "signed out"
    );
    assert_eq!(op.login("op", "operator password").await.location(), "/");
    assert_eq!(audit_actions(&app, "totp_reset").await.len(), 1);

    // Not on yourself: that's what the account tab is for.
    let me: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(ADMIN)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let res = admin
        .post_hx(&format!("/users/{me}/totp/reset"), &csrf, &[])
        .await;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn secret_is_stored_sealed() {
    let (app, mut c) = TestApp::with_admin().await;
    let e = enroll(&mut c).await;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT totp_secret_enc FROM users WHERE username = ?")
        .bind(ADMIN)
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    let b32 = printpit::auth::totp::base32(&e.secret);
    assert!(!String::from_utf8_lossy(&stored).contains(&b32));
    assert!(!stored.windows(e.secret.len()).any(|w| w == e.secret.as_slice()));
}
