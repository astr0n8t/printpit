//! Egress control for printer traffic.
//!
//! * One shared `reqwest::Client`, optionally bound to `PRINTPIT_EGRESS_BIND` so printer traffic
//!   leaves through the printer network's interface.
//! * A DNS resolver that drops any resolved address outside `PRINTPIT_PRINTER_CIDRS`. The check
//!   runs on every connection, *after* resolution, so DNS rebinding can't steer it elsewhere.
//! * IP-literal hosts never reach the resolver, so [`Egress::check_url`] covers those.
//! * No proxies (env proxies would resolve names out of our sight) and no redirects.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use super::MoonrakerError;
use crate::config::Config;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Returned by the resolver when every address for a host is outside the allowlist.
#[derive(Debug, thiserror::Error)]
#[error("printer address is outside PRINTPIT_PRINTER_CIDRS")]
pub struct Blocked;

#[derive(Clone)]
pub struct Egress {
    client: reqwest::Client,
    allow: Arc<Vec<IpNet>>,
}

impl Egress {
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        // reqwest is built without a bundled crypto provider; use rustls' pure-Rust `ring`.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let allow = Arc::new(config.printer_cidrs.clone());
        let client = reqwest::Client::builder()
            .user_agent(concat!("printpit/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .local_address(config.egress_bind)
            .dns_resolver(AllowlistResolver { allow: allow.clone() })
            .build()?;
        Ok(Self { client, allow })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn is_allowed(&self, ip: IpAddr) -> bool {
        allowed(&self.allow, ip)
    }

    /// Connect-time check for IP-literal hosts (hostnames are checked by the resolver).
    pub fn check_url(&self, url: &Url) -> Result<(), MoonrakerError> {
        match literal_ip(url) {
            Some(ip) if !self.is_allowed(ip) => Err(MoonrakerError::Blocked),
            _ => Ok(()),
        }
    }

    /// Save-time check, for a friendly error in the form. IP literals must be allowed; hostnames
    /// must not resolve *only* to disallowed addresses. A name that doesn't resolve right now is
    /// accepted (the printer may be off) and is still enforced at connect time.
    pub async fn vet(&self, url: &Url) -> Result<(), &'static str> {
        const MSG: &str = "That address is outside the allowed printer networks (PRINTPIT_PRINTER_CIDRS).";
        if let Some(ip) = literal_ip(url) {
            return if self.is_allowed(ip) { Ok(()) } else { Err(MSG) };
        }
        let host = url.host_str().unwrap_or_default();
        match tokio::net::lookup_host((host, 0)).await {
            Ok(addrs) => {
                let addrs: Vec<SocketAddr> = addrs.collect();
                if addrs.is_empty() || addrs.iter().any(|a| self.is_allowed(a.ip())) {
                    Ok(())
                } else {
                    Err(MSG)
                }
            }
            Err(_) => Ok(()),
        }
    }
}

fn allowed(allow: &[IpNet], ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    allow.iter().any(|net| net.contains(&ip))
}

fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

struct AllowlistResolver {
    allow: Arc<Vec<IpNet>>,
}

impl Resolve for AllowlistResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow.clone();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| allowed(&allow, a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(Box::new(Blocked) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("Enter a URL like http://10.0.0.20:7125")]
    Invalid,
    #[error("Only http:// and https:// URLs are supported.")]
    Scheme,
    #[error("Don't put a username or password in the URL; use the API key field.")]
    Credentials,
    #[error("The URL can't have a query string or fragment.")]
    QueryOrFragment,
}

/// Normalises a user-entered Moonraker base URL: adds `http://` when no scheme is given and
/// strips trailing slashes. A path is allowed (Moonraker behind a reverse-proxy prefix).
pub fn parse_base_url(input: &str) -> Result<Url, UrlError> {
    let input = input.trim();
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("http://{input}")
    };
    let mut url = Url::parse(&with_scheme).map_err(|_| UrlError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(UrlError::Scheme);
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(UrlError::Invalid);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(UrlError::QueryOrFragment);
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url)
}

/// The base URL as a string without a trailing slash, ready for `base + "/server/info"`.
pub fn base_string(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn egress(cidrs: &str) -> Egress {
        let config = Config::from_lookup(|k| match k {
            "PRINTPIT_SECRET_KEY" => Some("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into()),
            "PRINTPIT_PRINTER_CIDRS" => Some(cidrs.into()),
            _ => None,
        })
        .unwrap();
        Egress::new(&config).unwrap()
    }

    #[test]
    fn base_url_normalisation() {
        let s = |i| parse_base_url(i).map(|u| base_string(&u));
        assert_eq!(s("10.0.0.5:7125"), Ok("http://10.0.0.5:7125".into()));
        assert_eq!(s(" http://voron.lan/ "), Ok("http://voron.lan".into()));
        assert_eq!(s("https://host/moonraker/"), Ok("https://host/moonraker".into()));
        assert_eq!(s("http://[fd00::5]:7125"), Ok("http://[fd00::5]:7125".into()));
        assert_eq!(s("ftp://host"), Err(UrlError::Scheme));
        assert_eq!(s("file:///etc/passwd"), Err(UrlError::Scheme));
        assert_eq!(s("http://user:pw@host"), Err(UrlError::Credentials));
        assert_eq!(s("http://host/?x=1"), Err(UrlError::QueryOrFragment));
        assert_eq!(s("http://"), Err(UrlError::Invalid));
        assert_eq!(s(""), Err(UrlError::Invalid));
    }

    #[test]
    fn literal_hosts_checked_against_allowlist() {
        let e = egress("10.0.0.0/8");
        assert!(e.check_url(&parse_base_url("10.1.2.3:7125").unwrap()).is_ok());
        assert!(e.check_url(&parse_base_url("192.168.1.5").unwrap()).is_err());
        assert!(e.check_url(&parse_base_url("127.0.0.1").unwrap()).is_err());
        assert!(
            e.check_url(&parse_base_url("[::ffff:10.0.0.1]").unwrap()).is_ok(),
            "mapped v6"
        );
        // Hostnames are left to the resolver.
        assert!(e.check_url(&parse_base_url("voron.lan").unwrap()).is_ok());
    }

    #[tokio::test]
    async fn resolver_filters_after_resolution() {
        let blocked = AllowlistResolver {
            allow: Arc::new(vec!["10.0.0.0/8".parse().unwrap()]),
        };
        let err = blocked
            .resolve(Name::from_str("localhost").unwrap())
            .await
            .err()
            .unwrap();
        assert!(err.is::<Blocked>());

        let open = AllowlistResolver {
            allow: Arc::new(vec!["127.0.0.0/8".parse().unwrap(), "::1/128".parse().unwrap()]),
        };
        let addrs: Vec<_> = open
            .resolve(Name::from_str("localhost").unwrap())
            .await
            .unwrap()
            .collect();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }

    #[tokio::test]
    async fn vet_reports_disallowed_hosts() {
        let e = egress("10.0.0.0/8");
        assert!(e.vet(&parse_base_url("localhost:7125").unwrap()).await.is_err());
        assert!(e.vet(&parse_base_url("127.0.0.1").unwrap()).await.is_err());
        assert!(e.vet(&parse_base_url("10.0.0.9").unwrap()).await.is_ok());
        assert!(
            e.vet(&parse_base_url("does-not-exist.invalid").unwrap())
                .await
                .is_ok()
        );
        let e = egress("127.0.0.0/8");
        assert!(e.vet(&parse_base_url("localhost:7125").unwrap()).await.is_ok());
    }
}
