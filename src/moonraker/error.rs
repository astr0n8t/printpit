//! Typed Moonraker failures. Messages are safe to show to any signed-in user: they never
//! contain the printer's URL, address or API key.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoonrakerError {
    #[error("printer address is outside the allowed printer networks")]
    Blocked,
    #[error("offline: {0}")]
    Offline(String),
    #[error("unauthorized: check the printer's API key")]
    Unauthorized,
    #[error("Klipper isn't ready: {0}")]
    KlippyNotReady(String),
    #[error("Moonraker returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unexpected response from Moonraker: {0}")]
    Decode(String),
    #[error("{0}")]
    Config(String),
}

impl MoonrakerError {
    /// Short state label for a TUI badge, e.g. `[OFFLINE]`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Blocked => "BLOCKED",
            Self::Offline(_) => "OFFLINE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::KlippyNotReady(_) => "NOT READY",
            Self::Http { .. } | Self::Decode(_) => "ERROR",
            Self::Config(_) => "CONFIG",
        }
    }

    /// Explanation to show next to the [`label`](Self::label) badge, without repeating it.
    pub fn detail(&self) -> String {
        match self {
            Self::Blocked => "the address is outside the allowed printer networks".into(),
            Self::Offline(why) => format!("can't reach Moonraker: {why}"),
            Self::Unauthorized => "Moonraker rejected the request; check the API key".into(),
            Self::KlippyNotReady(why) => why.clone(),
            Self::Http { status, message } => format!("HTTP {status}: {message}"),
            Self::Decode(why) => format!("unexpected response: {why}"),
            Self::Config(why) => why.clone(),
        }
    }

    /// Badge colour class.
    pub fn class(&self) -> &'static str {
        match self {
            Self::KlippyNotReady(_) => "badge-warn",
            _ => "badge-err",
        }
    }

    pub(crate) fn from_reqwest(err: reqwest::Error) -> Self {
        let err = err.without_url();
        let mut root: &dyn std::error::Error = &err;
        while let Some(next) = root.source() {
            if next.is::<super::net::Blocked>() {
                return Self::Blocked;
            }
            root = next;
        }
        if err.is_timeout() {
            Self::Offline("timed out".into())
        } else if err.is_decode() {
            Self::Decode(root.to_string())
        } else {
            Self::Offline(root.to_string())
        }
    }
}
