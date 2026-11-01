//! Two-factor sign-in on the account tab: set up (QR code + confirm with a code), turn off,
//! and replace recovery codes. Also the code check shared with the login page.

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::extract::rejection::FormRejection;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::auth::totp::{self, Entry};
use crate::auth::{ClientIp, CurrentUser, password};
use crate::db::{self, audit, sessions, users};
use crate::error::AppError;
use crate::state::AppState;

pub struct Setup {
    pub qr: String,
    pub key: String,
    /// The unconfirmed secret, sealed, round-tripped through the form.
    pub sealed: String,
}

#[derive(Template)]
#[template(path = "partials/totp_panel.html")]
pub struct TotpPanel {
    pub csrf: String,
    pub enabled: bool,
    pub recovery_left: i64,
    pub setup: Option<Setup>,
    /// Freshly issued recovery codes, shown exactly once.
    pub codes: Vec<String>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl TotpPanel {
    pub async fn load(state: &AppState, user: &CurrentUser) -> Result<Self, AppError> {
        let enabled = db::totp::is_enabled(&state.db, user.id).await?;
        Ok(Self {
            csrf: user.csrf_token.clone(),
            enabled,
            recovery_left: if enabled {
                db::totp::recovery_left(&state.db, user.id).await?
            } else {
                0
            },
            setup: None,
            codes: Vec::new(),
            notice: None,
            error: None,
        })
    }

    fn respond(self, status: StatusCode) -> Result<Response, AppError> {
        Ok((status, Html(self.render()?)).into_response())
    }
}

/// How a second-factor entry was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    Code,
    Recovery,
}

/// Checks a TOTP code or recovery code for a user with 2FA on, using it up (the code's time
/// step is claimed atomically, a recovery code is deleted). `None` if it isn't valid.
pub async fn check_entry(state: &AppState, user_id: i64, input: &str) -> Result<Option<Accepted>, AppError> {
    let Some((sealed, last_step)) = db::totp::get(&state.db, user_id).await? else {
        return Ok(None);
    };
    match totp::parse_entry(input) {
        Entry::Code(code) => {
            let secret = match state.cipher.open(&sealed, &totp::secret_context(user_id)) {
                Ok(b32) => totp::base32_decode(&b32).unwrap_or_default(),
                Err(_) => {
                    tracing::warn!(
                        user_id,
                        "TOTP secret is unreadable (did PRINTPIT_SECRET_KEY change?)"
                    );
                    return Ok(None);
                }
            };
            let Some(step) = totp::verify(&secret, &code, db::now(), last_step) else {
                return Ok(None);
            };
            let claimed = db::totp::claim_step(&state.db, user_id, step).await?;
            Ok(claimed.then_some(Accepted::Code))
        }
        Entry::Recovery(code) => {
            let used = db::totp::use_recovery(&state.db, user_id, &totp::recovery_hash(&code)).await?;
            Ok(used.then_some(Accepted::Recovery))
        }
        Entry::Invalid => Ok(None),
    }
}

async fn record(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    action: &'static str,
    detail: Option<String>,
) {
    audit::record(
        &state.db,
        audit::Entry {
            user_id: Some(user.id),
            printer_id: None,
            action,
            detail,
            ip: ip.to_string_opt(),
        },
    )
    .await;
}

fn setup_for(state: &AppState, user: &CurrentUser, secret: &[u8]) -> Setup {
    Setup {
        qr: totp::qr_svg(&totp::otpauth_uri(&user.username, secret)),
        key: totp::display_key(secret),
        sealed: totp::seal_b64(
            &state.cipher,
            &totp::base32(secret),
            &totp::enroll_context(user.id),
        ),
    }
}

/// Starts setup: a new secret, shown as a QR code and a typed key, not stored yet.
pub async fn setup(State(state): State<AppState>, user: CurrentUser) -> Result<Response, AppError> {
    let mut panel = TotpPanel::load(&state, &user).await?;
    if panel.enabled {
        panel.error = Some("Two-factor sign-in is already on.".into());
        return panel.respond(StatusCode::CONFLICT);
    }
    panel.setup = Some(setup_for(&state, &user, &totp::new_secret()));
    panel.respond(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct EnableForm {
    #[serde(default)]
    enroll: String,
    #[serde(default)]
    code: String,
}

/// Confirms setup with a code from the app, then shows the recovery codes once.
pub async fn enable(
    State(state): State<AppState>,
    ip: ClientIp,
    user: CurrentUser,
    form: Result<Form<EnableForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let mut panel = TotpPanel::load(&state, &user).await?;
    if panel.enabled {
        panel.error = Some("Two-factor sign-in is already on.".into());
        return panel.respond(StatusCode::CONFLICT);
    }
    let Some(secret) = totp::open_b64(&state.cipher, &f.enroll, &totp::enroll_context(user.id))
        .and_then(|b32| totp::base32_decode(&b32))
        .filter(|s| s.len() >= 16)
    else {
        panel.error = Some("That setup expired. Start again.".into());
        return panel.respond(StatusCode::UNPROCESSABLE_ENTITY);
    };
    let step = match totp::parse_entry(&f.code) {
        Entry::Code(code) => totp::verify(&secret, &code, db::now(), None),
        _ => None,
    };
    let Some(step) = step else {
        panel.setup = Some(setup_for(&state, &user, &secret));
        panel.error =
            Some("That code didn't match. Check the time on your phone and try the next one.".into());
        return panel.respond(StatusCode::UNPROCESSABLE_ENTITY);
    };
    let sealed = state
        .cipher
        .seal(&totp::base32(&secret), &totp::secret_context(user.id));
    let codes = totp::new_recovery_codes();
    let hashes: Vec<Vec<u8>> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
    db::totp::enable(&state.db, user.id, &sealed, step, &hashes).await?;
    let revoked = sessions::delete_others(&state.db, user.id, &user.session_hash).await?;
    record(
        &state,
        ip,
        &user,
        "totp_enable",
        Some(format!("revoked_sessions={revoked}")),
    )
    .await;
    panel.enabled = true;
    panel.recovery_left = codes.len() as i64;
    panel.codes = codes;
    panel.notice = Some("Two-factor sign-in is on. Your other sessions were signed out.".into());
    panel.respond(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

/// Turning 2FA off or replacing recovery codes needs the password *and* a code, so a stolen
/// session alone can't weaken the account. Failures count towards the login rate limit.
async fn confirm(
    state: &AppState,
    ip: ClientIp,
    user: &CurrentUser,
    f: ConfirmForm,
) -> Result<Result<Accepted, (StatusCode, String)>, AppError> {
    if let Err(retry) = state.login_limiter.check(ip.0, &user.username) {
        let mins = retry.as_secs().div_ceil(60);
        return Ok(Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("Too many failed attempts. Try again in {mins} minute(s)."),
        )));
    }
    let record_hash = users::by_id(&state.db, user.id).await?.map(|u| u.password_hash);
    if !password::verify_async(f.password, record_hash).await {
        state.login_limiter.record_failure(ip.0, &user.username);
        return Ok(Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "Your password is wrong.".into(),
        )));
    }
    match check_entry(state, user.id, &f.code).await? {
        Some(a) => Ok(Ok(a)),
        None => {
            state.login_limiter.record_failure(ip.0, &user.username);
            record(state, ip, user, "totp_failed", Some("where=account".into())).await;
            Ok(Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "That code didn't work.".into(),
            )))
        }
    }
}

pub async fn disable(
    State(state): State<AppState>,
    ip: ClientIp,
    user: CurrentUser,
    form: Result<Form<ConfirmForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let mut panel = TotpPanel::load(&state, &user).await?;
    if !panel.enabled {
        return panel.respond(StatusCode::OK);
    }
    if let Err((status, msg)) = confirm(&state, ip, &user, f).await? {
        panel.error = Some(msg);
        return panel.respond(status);
    }
    db::totp::disable(&state.db, user.id).await?;
    record(&state, ip, &user, "totp_disable", None).await;
    panel.enabled = false;
    panel.recovery_left = 0;
    panel.notice = Some("Two-factor sign-in is off.".into());
    panel.respond(StatusCode::OK)
}

pub async fn new_recovery(
    State(state): State<AppState>,
    ip: ClientIp,
    user: CurrentUser,
    form: Result<Form<ConfirmForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Ok(Form(f)) = form else {
        return Err(AppError::BadRequest("Malformed form.".into()));
    };
    let mut panel = TotpPanel::load(&state, &user).await?;
    if !panel.enabled {
        return panel.respond(StatusCode::OK);
    }
    if let Err((status, msg)) = confirm(&state, ip, &user, f).await? {
        panel.error = Some(msg);
        return panel.respond(status);
    }
    let codes = totp::new_recovery_codes();
    let hashes: Vec<Vec<u8>> = codes.iter().map(|c| totp::recovery_hash(c)).collect();
    db::totp::replace_recovery(&state.db, user.id, &hashes).await?;
    record(&state, ip, &user, "totp_recovery_reset", None).await;
    panel.recovery_left = codes.len() as i64;
    panel.codes = codes;
    panel.notice = Some("New recovery codes. The old ones no longer work.".into());
    panel.respond(StatusCode::OK)
}
