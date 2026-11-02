//! What can go wrong talking to the slicer worker, in the same shape as `MoonrakerError`: a
//! badge [`label`](SlicerError::label) plus a [`detail`](SlicerError::detail) that is safe to show.
//! No message carries the worker's address or the token.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SlicerError {
    #[error("offline: {0}")]
    Offline(String),
    #[error("the slicer rejected printpit's token")]
    Unauthorized,
    #[error("the slicer is busy")]
    Busy { retry_after: Option<u64> },
    /// No answer within `PRINTPIT_SLICER_TIMEOUT_SECS`.
    #[error("no answer from the slicer in time")]
    Timeout,
    /// The slicer ran and said no: a bad model, a profile problem, its own time limit, a crash.
    /// `kind` is the worker's (`model`, `profile`, `timeout`, `too_large`, `crashed`, …).
    #[error("{kind}: {message}")]
    Failed {
        kind: String,
        message: String,
        code: Option<i32>,
        log_tail: Vec<String>,
    },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("slicer returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unexpected response from the slicer: {0}")]
    Decode(String),
    /// A local problem around a slice (writing the model or the G-code).
    #[error("{0}")]
    Io(String),
}

/// The worker's error body: `{error, kind, code?, log_tail?}`.
#[derive(Debug, Deserialize)]
pub(crate) struct ErrorBody {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub log_tail: Vec<String>,
}

const MAX_MESSAGE: usize = 500;
const MAX_LOG_LINES: usize = 40;

impl SlicerError {
    /// Short state label for a TUI badge, e.g. `[OFFLINE]`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Offline(_) => "OFFLINE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Busy { .. } => "BUSY",
            Self::Timeout => "TIMEOUT",
            Self::Failed { kind, .. } => match kind.as_str() {
                "model" => "BAD MODEL",
                "profile" => "PROFILE",
                "timeout" => "TIMEOUT",
                "too_large" => "TOO LARGE",
                "crashed" => "CRASHED",
                _ => "FAILED",
            },
            Self::NotFound(_) => "NOT FOUND",
            Self::Http { .. } | Self::Decode(_) => "ERROR",
            Self::Io(_) => "ERROR",
        }
    }

    /// Explanation to show next to the [`label`](Self::label) badge, without repeating it.
    pub fn detail(&self) -> String {
        match self {
            Self::Offline(why) => format!("can't reach the slicer: {why}"),
            Self::Unauthorized => "the slicer rejected printpit's token; check PRINTPIT_SLICER_TOKEN".into(),
            Self::Busy { .. } => "the slicer is busy with another job".into(),
            Self::Timeout => "the slicer didn't finish in time (PRINTPIT_SLICER_TIMEOUT_SECS)".into(),
            Self::Failed { message, .. } => message.clone(),
            Self::NotFound(what) => what.clone(),
            Self::Http { status, message } => format!("HTTP {status}: {message}"),
            Self::Decode(why) => format!("unexpected response: {why}"),
            Self::Io(why) => why.clone(),
        }
    }

    /// Badge colour class.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Busy { .. } => "badge-warn",
            _ => "badge-err",
        }
    }

    /// Worth retrying later without changing anything (the queue keeps such a job queued).
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Offline(_) | Self::Busy { .. })
    }

    /// The slicer's log lines, when it got far enough to have any.
    pub fn log_tail(&self) -> &[String] {
        match self {
            Self::Failed { log_tail, .. } => log_tail,
            _ => &[],
        }
    }

    /// Maps a non-2xx worker response.
    pub(crate) fn from_response(status: u16, retry_after: Option<u64>, body: &[u8]) -> Self {
        let parsed: Option<ErrorBody> = serde_json::from_slice(body).ok();
        let message = parsed
            .as_ref()
            .map(|b| clip(&b.error))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| format!("HTTP {status}"));
        match status {
            401 => Self::Unauthorized,
            503 => Self::Busy { retry_after },
            404 => Self::NotFound(message),
            413 | 422 => {
                let b = parsed.unwrap_or(ErrorBody {
                    error: String::new(),
                    kind: String::new(),
                    code: None,
                    log_tail: Vec::new(),
                });
                Self::Failed {
                    kind: if b.kind.is_empty() {
                        if status == 413 { "too_large" } else { "slicer" }.into()
                    } else {
                        clip(&b.kind)
                    },
                    message,
                    code: b.code,
                    log_tail: b
                        .log_tail
                        .iter()
                        .rev()
                        .take(MAX_LOG_LINES)
                        .rev()
                        .map(|l| clip(l))
                        .collect(),
                }
            }
            _ => Self::Http { status, message },
        }
    }

    /// Maps a transport error, dropping the URL and keeping only the root cause.
    pub(crate) fn from_reqwest(err: reqwest::Error) -> Self {
        let err = err.without_url();
        if err.is_timeout() {
            return Self::Timeout;
        }
        let mut root: &dyn std::error::Error = &err;
        while let Some(next) = root.source() {
            root = next;
        }
        if err.is_decode() || err.is_body() {
            Self::Decode(root.to_string())
        } else {
            Self::Offline(root.to_string())
        }
    }
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_MESSAGE {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_MESSAGE).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_worker_errors() {
        assert_eq!(
            SlicerError::from_response(401, None, b"{}"),
            SlicerError::Unauthorized
        );
        assert_eq!(
            SlicerError::from_response(503, Some(5), br#"{"error":"busy","kind":"busy"}"#),
            SlicerError::Busy { retry_after: Some(5) }
        );
        let e = SlicerError::from_response(
            422,
            None,
            br#"{"error":"The input model file to the slicer can not be parsed.","kind":"model","code":-6,
                 "log_tail":["a","b"]}"#,
        );
        assert_eq!(e.label(), "BAD MODEL");
        assert_eq!(
            e.detail(),
            "The input model file to the slicer can not be parsed."
        );
        assert_eq!(e.log_tail(), ["a", "b"]);
        assert!(!e.is_transient());
        let e = SlicerError::from_response(413, None, b"not json");
        assert_eq!(e.label(), "TOO LARGE");
        assert_eq!(e.detail(), "HTTP 413");
        assert_eq!(
            SlicerError::from_response(404, None, br#"{"error":"unknown vendor `X`","kind":"not_found"}"#),
            SlicerError::NotFound("unknown vendor `X`".into())
        );
        assert!(matches!(
            SlicerError::from_response(400, None, br#"{"error":"bad options"}"#),
            SlicerError::Http { status: 400, .. }
        ));
        assert!(SlicerError::Busy { retry_after: None }.is_transient());
        assert!(SlicerError::Offline("x".into()).is_transient());
    }

    #[test]
    fn clips_what_it_shows() {
        let long = "x".repeat(2000);
        let body = serde_json::json!({"error": long, "kind": "slicer", "log_tail": vec!["l"; 100]});
        let e = SlicerError::from_response(422, None, body.to_string().as_bytes());
        assert_eq!(e.detail().chars().count(), MAX_MESSAGE + 1);
        assert_eq!(e.log_tail().len(), MAX_LOG_LINES);
    }
}
