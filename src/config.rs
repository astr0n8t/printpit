//! Runtime configuration, loaded only from `PRINTPIT_*` environment variables.
//!
//! Secrets may also be supplied as `<NAME>_FILE` pointing at a file (Docker secrets).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use ipnet::IpNet;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var} is required{hint}")]
    Missing { var: &'static str, hint: &'static str },
    #[error("{var} is invalid: {reason}")]
    Invalid { var: &'static str, reason: String },
    #[error("{var} and {var}_FILE are both set; use only one")]
    Ambiguous { var: &'static str },
    #[error("could not read {var}_FILE ({path}): {source}")]
    File {
        var: &'static str,
        path: String,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

/// 32-byte key used to encrypt secrets at rest. `Debug` never prints the bytes.
#[derive(Clone)]
pub struct SecretKey(pub [u8; 32]);

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// Where print notifications are POSTed. Webhook URLs usually embed a token, so `Debug` (and
/// therefore logs) never print it.
#[derive(Clone)]
pub struct WebhookUrl(pub url::Url);

impl fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookUrl(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub secret_key: SecretKey,
    pub secure_cookies: bool,
    pub session_idle: Duration,
    pub session_absolute: Duration,
    pub printer_cidrs: Vec<IpNet>,
    pub egress_bind: Option<IpAddr>,
    pub ui_refresh: Duration,
    pub ws_idle_grace: Duration,
    pub system_refresh: Duration,
    pub max_upload_bytes: u64,
    pub trusted_proxies: Vec<IpNet>,
    pub log_format: LogFormat,
    pub notify_webhook: Option<WebhookUrl>,
}

pub const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_PRINTER_CIDRS: &str = "10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Builds the config from an arbitrary variable lookup, so tests don't touch the process env.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |k: &str| get(k).filter(|v| !v.trim().is_empty());

        let bind = parse_or("PRINTPIT_BIND", get("PRINTPIT_BIND"), DEFAULT_BIND)?;
        let data_dir = PathBuf::from(get("PRINTPIT_DATA_DIR").unwrap_or_else(|| "/data".into()));

        let secret_raw = secret(&get, "PRINTPIT_SECRET_KEY")?.ok_or(ConfigError::Missing {
            var: "PRINTPIT_SECRET_KEY",
            hint: " (32 random bytes, base64; generate with `openssl rand -base64 32`)",
        })?;
        let secret_key = parse_secret_key(&secret_raw)?;

        let secure_cookies = parse_bool("PRINTPIT_SECURE_COOKIES", get("PRINTPIT_SECURE_COOKIES"), true)?;
        let session_idle = Duration::from_secs(
            60 * parse_or::<u64>(
                "PRINTPIT_SESSION_IDLE_MINS",
                get("PRINTPIT_SESSION_IDLE_MINS"),
                "720",
            )?,
        );
        let session_absolute = Duration::from_secs(
            3600 * parse_or::<u64>(
                "PRINTPIT_SESSION_ABSOLUTE_HOURS",
                get("PRINTPIT_SESSION_ABSOLUTE_HOURS"),
                "168",
            )?,
        );

        let printer_cidrs = parse_cidrs(
            "PRINTPIT_PRINTER_CIDRS",
            get("PRINTPIT_PRINTER_CIDRS")
                .as_deref()
                .unwrap_or(DEFAULT_PRINTER_CIDRS),
        )?;
        if printer_cidrs.is_empty() {
            return Err(ConfigError::Invalid {
                var: "PRINTPIT_PRINTER_CIDRS",
                reason: "must list at least one CIDR".into(),
            });
        }
        let egress_bind = get("PRINTPIT_EGRESS_BIND")
            .map(|v| {
                v.trim().parse::<IpAddr>().map_err(|e| ConfigError::Invalid {
                    var: "PRINTPIT_EGRESS_BIND",
                    reason: e.to_string(),
                })
            })
            .transpose()?;

        let ui_refresh = Duration::from_millis(parse_or(
            "PRINTPIT_UI_REFRESH_MS",
            get("PRINTPIT_UI_REFRESH_MS"),
            "1000",
        )?);
        let ws_idle_grace = Duration::from_secs(parse_or(
            "PRINTPIT_WS_IDLE_GRACE_SECS",
            get("PRINTPIT_WS_IDLE_GRACE_SECS"),
            "30",
        )?);
        let system_refresh = Duration::from_secs(parse_or(
            "PRINTPIT_SYSTEM_REFRESH_SECS",
            get("PRINTPIT_SYSTEM_REFRESH_SECS"),
            "5",
        )?);
        if ui_refresh.is_zero() || system_refresh.is_zero() {
            return Err(ConfigError::Invalid {
                var: "PRINTPIT_UI_REFRESH_MS/PRINTPIT_SYSTEM_REFRESH_SECS",
                reason: "must be greater than zero".into(),
            });
        }
        let max_upload_mb: u64 = parse_or("PRINTPIT_MAX_UPLOAD_MB", get("PRINTPIT_MAX_UPLOAD_MB"), "1024")?;

        let trusted_proxies = parse_cidrs(
            "PRINTPIT_TRUSTED_PROXIES",
            get("PRINTPIT_TRUSTED_PROXIES").as_deref().unwrap_or(""),
        )?;

        let notify_webhook = secret(&get, "PRINTPIT_NOTIFY_WEBHOOK_URL")?
            .map(|raw| parse_webhook(&raw))
            .transpose()?;

        let log_format = match get("PRINTPIT_LOG_FORMAT").as_deref().map(str::trim) {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    var: "PRINTPIT_LOG_FORMAT",
                    reason: format!("expected `pretty` or `json`, got `{other}`"),
                });
            }
        };

        Ok(Self {
            bind,
            data_dir,
            secret_key,
            secure_cookies,
            session_idle,
            session_absolute,
            printer_cidrs,
            egress_bind,
            ui_refresh,
            ws_idle_grace,
            system_refresh,
            max_upload_bytes: max_upload_mb * 1024 * 1024,
            trusted_proxies,
            log_format,
            notify_webhook,
        })
    }
}

/// Reads `var` directly or from the file named by `<var>_FILE`.
fn secret(get: &impl Fn(&str) -> Option<String>, var: &'static str) -> Result<Option<String>, ConfigError> {
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

fn parse_webhook(raw: &str) -> Result<WebhookUrl, ConfigError> {
    // The reason never quotes the value: it may hold a token.
    let invalid = |reason: &str| ConfigError::Invalid {
        var: "PRINTPIT_NOTIFY_WEBHOOK_URL",
        reason: reason.into(),
    };
    let url = url::Url::parse(raw.trim()).map_err(|_| invalid("not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("must be an http:// or https:// URL"));
    }
    Ok(WebhookUrl(url))
}

fn parse_secret_key(raw: &str) -> Result<SecretKey, ConfigError> {
    let invalid = |reason: String| ConfigError::Invalid {
        var: "PRINTPIT_SECRET_KEY",
        reason,
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .map_err(|e| invalid(format!("not valid base64 ({e})")))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|b: Vec<u8>| invalid(format!("must decode to 32 bytes, got {}", b.len())))?;
    Ok(SecretKey(key))
}

fn parse_or<T>(var: &'static str, value: Option<String>, default: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    let v = value.unwrap_or_else(|| default.to_string());
    v.trim().parse().map_err(|e: T::Err| ConfigError::Invalid {
        var,
        reason: format!("`{v}`: {e}"),
    })
}

fn parse_bool(var: &'static str, value: Option<String>, default: bool) -> Result<bool, ConfigError> {
    match value.as_deref().map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) => match v.as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(ConfigError::Invalid {
                var,
                reason: format!("expected true/false, got `{v}`"),
            }),
        },
    }
}

fn parse_cidrs(var: &'static str, value: &str) -> Result<Vec<IpNet>, ConfigError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Accept bare addresses as single-host networks.
            s.parse::<IpNet>()
                .or_else(|_| s.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| ConfigError::Invalid {
                    var,
                    reason: format!("`{s}` is not a CIDR or IP address"),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_with_only_secret() {
        let c = load(&[("PRINTPIT_SECRET_KEY", KEY)]).unwrap();
        assert_eq!(c.bind, DEFAULT_BIND.parse().unwrap());
        assert_eq!(c.data_dir, PathBuf::from("/data"));
        assert!(c.secure_cookies);
        assert_eq!(c.printer_cidrs.len(), 3);
        assert_eq!(c.egress_bind, None);
        assert_eq!(c.ui_refresh, Duration::from_millis(1000));
        assert_eq!(c.system_refresh, Duration::from_secs(5));
        assert_eq!(c.max_upload_bytes, 1024 * 1024 * 1024);
        assert_eq!(c.secret_key.0[31], 31);
        assert_eq!(c.log_format, LogFormat::Pretty);
    }

    #[test]
    fn missing_secret_is_an_error() {
        let err = load(&[]).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Missing {
                var: "PRINTPIT_SECRET_KEY",
                ..
            }
        ));
    }

    #[test]
    fn short_secret_is_rejected() {
        let err = load(&[("PRINTPIT_SECRET_KEY", "c2hvcnQ=")]).unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "{err}");
    }

    #[test]
    fn secret_from_file() {
        let dir = std::env::temp_dir().join(format!("printpit-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, format!("{KEY}\n")).unwrap();
        let c = load(&[("PRINTPIT_SECRET_KEY_FILE", path.to_str().unwrap())]).unwrap();
        assert_eq!(c.secret_key.0[0], 0);
        let err = load(&[
            ("PRINTPIT_SECRET_KEY", KEY),
            ("PRINTPIT_SECRET_KEY_FILE", path.to_str().unwrap()),
        ])
        .unwrap_err();
        assert!(matches!(err, ConfigError::Ambiguous { .. }));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_overrides() {
        let c = load(&[
            ("PRINTPIT_SECRET_KEY", KEY),
            ("PRINTPIT_BIND", "127.0.0.1:9000"),
            ("PRINTPIT_SECURE_COOKIES", "false"),
            ("PRINTPIT_PRINTER_CIDRS", "10.20.0.0/24, 192.168.1.50"),
            ("PRINTPIT_EGRESS_BIND", "10.20.0.2"),
            ("PRINTPIT_LOG_FORMAT", "json"),
        ])
        .unwrap();
        assert_eq!(c.bind.port(), 9000);
        assert!(!c.secure_cookies);
        assert_eq!(c.printer_cidrs[1], "192.168.1.50/32".parse::<IpNet>().unwrap());
        assert_eq!(c.egress_bind, Some("10.20.0.2".parse().unwrap()));
        assert_eq!(c.log_format, LogFormat::Json);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(load(&[("PRINTPIT_SECRET_KEY", KEY), ("PRINTPIT_PRINTER_CIDRS", "nope")]).is_err());
        assert!(load(&[("PRINTPIT_SECRET_KEY", KEY), ("PRINTPIT_SECURE_COOKIES", "maybe")]).is_err());
        assert!(load(&[("PRINTPIT_SECRET_KEY", KEY), ("PRINTPIT_UI_REFRESH_MS", "0")]).is_err());
        assert!(load(&[("PRINTPIT_SECRET_KEY", KEY), ("PRINTPIT_LOG_FORMAT", "xml")]).is_err());
    }

    #[test]
    fn debug_redacts_secret() {
        let c = load(&[("PRINTPIT_SECRET_KEY", KEY)]).unwrap();
        assert!(format!("{c:?}").contains("<redacted>"));
    }

    #[test]
    fn webhook_url() {
        let token = "https://hooks.example.com/services/T000/SECRET-TOKEN";
        let c = load(&[
            ("PRINTPIT_SECRET_KEY", KEY),
            ("PRINTPIT_NOTIFY_WEBHOOK_URL", token),
        ])
        .unwrap();
        assert_eq!(c.notify_webhook.as_ref().unwrap().0.as_str(), token);
        assert!(!format!("{c:?}").contains("SECRET-TOKEN"));
        assert!(
            load(&[("PRINTPIT_SECRET_KEY", KEY)])
                .unwrap()
                .notify_webhook
                .is_none()
        );
        for bad in ["not a url", "ftp://example.com/x", "file:///etc/passwd"] {
            let err =
                load(&[("PRINTPIT_SECRET_KEY", KEY), ("PRINTPIT_NOTIFY_WEBHOOK_URL", bad)]).unwrap_err();
            assert!(!err.to_string().contains(bad), "{err}");
        }
    }
}
