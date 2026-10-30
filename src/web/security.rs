//! Security headers applied to every response.

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; \
frame-ancestors 'none'";

const HEADERS: [(HeaderName, &str); 6] = [
    (header::CONTENT_SECURITY_POLICY, CSP),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_FRAME_OPTIONS, "DENY"),
    (
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
    ),
    (
        HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    ),
];

/// One year. No `includeSubDomains`: printpit shouldn't decide that for the whole domain.
const HSTS: &str = "max-age=31536000";

pub async fn headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in HEADERS {
        h.entry(name).or_insert(HeaderValue::from_static(value));
    }
    // Secure cookies mean printpit is served over TLS (directly or via a proxy).
    if state.config.secure_cookies {
        h.entry(header::STRICT_TRANSPORT_SECURITY)
            .or_insert(HeaderValue::from_static(HSTS));
    }
    res
}
