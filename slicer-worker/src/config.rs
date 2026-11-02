//! Worker configuration, from `PRINTPIT_SLICER_*` env vars only.
//!
//! The token may also come from `PRINTPIT_SLICER_TOKEN_FILE` (Docker secrets). The worker refuses to
//! start without one: it is the only thing standing between the network and a C++ model parser.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use subtle::ConstantTimeEq;

pub const DEFAULT_BIND: &str = "127.0.0.1:8090";
pub const DEFAULT_ORCA: &str = "/opt/orca/bin/orca-slicer";
pub const DEFAULT_PROFILES: &str = "/opt/orca/resources/profiles";
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_MAX_MODEL_MB: u64 = 256;
/// Long enough that guessing is hopeless (`openssl rand -hex 32` gives 64).
pub const TOKEN_MIN_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is required (generate one with `openssl rand -hex 32`)")]
    Missing { var: &'static str },
    #[error("{var} and {var}_FILE are both set; use only one")]
    Ambiguous { var: &'static str },
    #[error("could not read {var}_FILE ({path}): {source}")]
    File {
        var: &'static str,
        path: String,
        source: std::io::Error,
    },
    #[error("{var}: {reason}")]
    Invalid { var: &'static str, reason: String },
}

/// The shared bearer token. Never printed.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let invalid = |reason: String| ConfigError::Invalid {
            var: "PRINTPIT_SLICER_TOKEN",
            reason,
        };
        let t = raw.trim();
        if t.len() < TOKEN_MIN_LEN {
            return Err(invalid(format!("must be at least {TOKEN_MIN_LEN} characters")));
        }
        if !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid("must be printable ASCII without spaces".into()));
        }
        Ok(Self(t.to_string()))
    }

    /// Constant-time comparison (the length is not secret).
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.0.as_bytes().ct_eq(presented).into()
    }

    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub token: Token,
    /// The `orca-slicer` executable.
    pub orca: PathBuf,
    /// Orca's bundled `resources/profiles` (vendor index files plus their folders).
    pub profiles: PathBuf,
    /// Per-request temp dirs go here (a tmpfs in the image).
    pub work_dir: PathBuf,
    pub timeout: Duration,
    /// How long after `timeout` (Orca's own `--mstpp` limit) the process group is killed.
    pub kill_grace: Duration,
    pub max_model_bytes: u64,
    pub log_format: LogFormat,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::load(&|k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn load(get: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let token = secret(get, "PRINTPIT_SLICER_TOKEN")?
            .ok_or(ConfigError::Missing {
                var: "PRINTPIT_SLICER_TOKEN",
            })
            .and_then(|t| Token::parse(&t))?;
        let bind = bind_addr(get)?;
        let path = |var: &str, default: &str| PathBuf::from(get(var).unwrap_or_else(|| default.to_string()));
        let timeout = number(
            get,
            "PRINTPIT_SLICER_TIMEOUT_SECS",
            DEFAULT_TIMEOUT_SECS,
            10..=86_400,
        )?;
        let max_mb = number(
            get,
            "PRINTPIT_SLICER_MAX_MODEL_MB",
            DEFAULT_MAX_MODEL_MB,
            1..=4096,
        )?;
        let log_format = match get("PRINTPIT_LOG_FORMAT").as_deref().map(str::trim) {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(_) => {
                return Err(ConfigError::Invalid {
                    var: "PRINTPIT_LOG_FORMAT",
                    reason: "must be `pretty` or `json`".into(),
                });
            }
        };
        Ok(Self {
            bind,
            token,
            orca: path("PRINTPIT_SLICER_ORCA", DEFAULT_ORCA),
            profiles: path("PRINTPIT_SLICER_PROFILES", DEFAULT_PROFILES),
            work_dir: get("PRINTPIT_SLICER_WORK_DIR").map_or_else(std::env::temp_dir, PathBuf::from),
            timeout: Duration::from_secs(timeout),
            kill_grace: Duration::from_secs(15),
            max_model_bytes: max_mb * 1024 * 1024,
            log_format,
        })
    }
}

pub fn bind_addr(get: &impl Fn(&str) -> Option<String>) -> Result<SocketAddr, ConfigError> {
    get("PRINTPIT_SLICER_BIND")
        .unwrap_or_else(|| DEFAULT_BIND.to_string())
        .trim()
        .parse()
        .map_err(|e| ConfigError::Invalid {
            var: "PRINTPIT_SLICER_BIND",
            reason: format!("{e}"),
        })
}

/// Reads `var` directly or from the file named by `<var>_FILE`.
pub fn secret(
    get: &impl Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<Option<String>, ConfigError> {
    let file_var = format!("{var}_FILE");
    match (get(var), get(&file_var)) {
        (Some(_), Some(_)) => Err(ConfigError::Ambiguous { var }),
        (Some(v), None) => Ok(Some(v)),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(|s| Some(s.trim().to_string()))
            .map_err(|source| ConfigError::File { var, path, source }),
        (None, None) => Ok(None),
    }
}

fn number(
    get: &impl Fn(&str) -> Option<String>,
    var: &'static str,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> Result<u64, ConfigError> {
    let Some(raw) = get(var) else { return Ok(default) };
    match raw.trim().parse::<u64>() {
        Ok(n) if range.contains(&n) => Ok(n),
        _ => Err(ConfigError::Invalid {
            var,
            reason: format!("must be a whole number from {} to {}", range.start(), range.end()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::load(&|k: &str| map.get(k).cloned())
    }

    #[test]
    fn token_is_required_and_checked() {
        assert!(matches!(load(&[]), Err(ConfigError::Missing { .. })));
        assert!(matches!(
            load(&[("PRINTPIT_SLICER_TOKEN", "short")]),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            load(&[("PRINTPIT_SLICER_TOKEN", "has a space in it, which is not allowed")]),
            Err(ConfigError::Invalid { .. })
        ));
        let c = load(&[("PRINTPIT_SLICER_TOKEN", TOKEN)]).unwrap();
        assert!(c.token.matches(TOKEN.as_bytes()));
        assert!(!c.token.matches(b"0123456789abcdef0123456789abcdeX"));
        assert!(!c.token.matches(b""));
    }

    #[test]
    fn token_never_shows_in_debug() {
        let c = load(&[("PRINTPIT_SLICER_TOKEN", TOKEN)]).unwrap();
        assert!(!format!("{c:?}").contains(TOKEN));
    }

    #[test]
    fn token_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
        let c = load(&[("PRINTPIT_SLICER_TOKEN_FILE", path.to_str().unwrap())]).unwrap();
        assert!(c.token.matches(TOKEN.as_bytes()));
        assert!(matches!(
            load(&[
                ("PRINTPIT_SLICER_TOKEN_FILE", path.to_str().unwrap()),
                ("PRINTPIT_SLICER_TOKEN", TOKEN)
            ]),
            Err(ConfigError::Ambiguous { .. })
        ));
    }

    #[test]
    fn defaults_and_limits() {
        let c = load(&[("PRINTPIT_SLICER_TOKEN", TOKEN)]).unwrap();
        assert_eq!(c.bind, DEFAULT_BIND.parse().unwrap());
        assert_eq!(c.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(c.max_model_bytes, DEFAULT_MAX_MODEL_MB * 1024 * 1024);
        assert!(
            load(&[
                ("PRINTPIT_SLICER_TOKEN", TOKEN),
                ("PRINTPIT_SLICER_TIMEOUT_SECS", "0")
            ])
            .is_err()
        );
        assert!(
            load(&[
                ("PRINTPIT_SLICER_TOKEN", TOKEN),
                ("PRINTPIT_SLICER_MAX_MODEL_MB", "x")
            ])
            .is_err()
        );
        assert!(load(&[("PRINTPIT_SLICER_TOKEN", TOKEN), ("PRINTPIT_SLICER_BIND", "nope")]).is_err());
    }
}
