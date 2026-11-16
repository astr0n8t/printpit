//! Egress control for printer traffic.
//!
//! * One shared `reqwest::Client`, optionally bound to `PRINTPIT_EGRESS_BIND` so printer traffic
//!   leaves through the printer network's interface.
//! * A DNS resolver that drops any resolved address outside `PRINTPIT_PRINTER_CIDRS`. The check
//!   runs on every connection, *after* resolution, so DNS rebinding can't steer it elsewhere.
//! * IP-literal hosts never reach the resolver, so [`Egress::check_url`] covers those.
//! * No proxies (env proxies would resolve names out of our sight) and no redirects.
//! * A printer can have its own certificates ([`parse_ca`]): its self-signed certificate (pinned)
//!   or a CA. It gets a client built the same way but trusting only those ([`super::tls`]),
//!   cached by the PEM's hash so connections are still pooled.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ipnet::IpNet;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::{self, SectionKind};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use super::MoonrakerError;
use crate::config::Config;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Returned by the resolver when every address for a host is outside the allowlist.
#[derive(Debug, thiserror::Error)]
#[error("printer address is outside PRINTPIT_PRINTER_CIDRS")]
pub struct Blocked;

/// Most distinct CAs kept with a built client; past this the cache starts over (edits leave
/// stale entries behind, and a client is cheap to rebuild).
const MAX_CA_CLIENTS: usize = 32;

#[derive(Clone)]
pub struct Egress {
    client: reqwest::Client,
    allow: Arc<Vec<IpNet>>,
    bind: Option<IpAddr>,
    ca_clients: Arc<Mutex<HashMap<[u8; 32], reqwest::Client>>>,
}

impl Egress {
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        // reqwest is built without a bundled crypto provider; use rustls' pure-Rust `ring`.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let allow = Arc::new(config.printer_cidrs.clone());
        let client = builder(&allow, config.egress_bind).build()?;
        Ok(Self {
            client,
            allow,
            bind: config.egress_bind,
            ca_clients: Arc::default(),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The egress client for a printer: the shared one (system roots), one that trusts only the
    /// printer's own certificates, pinned or as CAs, or one that accepts any certificate
    /// ([`super::tls`]). All have the same resolver, bind address, timeouts and redirect policy.
    pub fn client_for(&self, trust: Trust<'_>) -> Result<reqwest::Client, MoonrakerError> {
        let hash: [u8; 32] = match trust {
            Trust::System => return Ok(self.client.clone()),
            Trust::Custom(pem) if pem.trim().is_empty() => return Ok(self.client.clone()),
            Trust::Custom(pem) => Sha256::digest(pem.as_bytes()).into(),
            // Not a PEM digest any bundle could produce: PEM text never contains a NUL.
            Trust::Insecure => Sha256::digest(b"\0insecure").into(),
        };
        let mut cache = self.ca_clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = cache.get(&hash) {
            return Ok(client.clone());
        }
        let bad = |e: CaError| MoonrakerError::Config(format!("the printer's certificate: {e}"));
        let tls = match trust {
            Trust::Custom(pem) => {
                let certs = parse_ca(pem).map_err(bad)?;
                super::tls::client_config(certs).map_err(|_| bad(CaError::Invalid))?
            }
            _ => super::tls::insecure_config().map_err(|_| bad(CaError::Invalid))?,
        };
        // reqwest takes a preconfigured rustls config as `dyn Any`; a rustls version mismatch
        // would fail here at build time (and in tests/printer_ca.rs), not silently.
        let client = builder(&self.allow, self.bind)
            .tls_backend_preconfigured(tls)
            .build()
            .map_err(|_| bad(CaError::Invalid))?;
        if cache.len() >= MAX_CA_CLIENTS {
            cache.clear();
        }
        cache.insert(hash, client.clone());
        Ok(client)
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

/// Which certificates a printer's https:// connections accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust<'a> {
    /// The system roots, with normal host name checks.
    System,
    /// Only these PEM certificates, pinned or as CAs (`printers.ca_cert`).
    Custom(&'a str),
    /// Any certificate (`printers.tls_skip_verify`): encrypted, but not authenticated.
    Insecure,
}

fn builder(allow: &Arc<Vec<IpNet>>, bind: Option<IpAddr>) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("printpit/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .local_address(bind)
        .dns_resolver(AllowlistResolver { allow: allow.clone() })
}

/// Largest CA bundle accepted (a few certificates is all anyone needs).
pub const MAX_CA_BYTES: usize = 64 * 1024;
const MAX_CA_CERTS: usize = 16;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaError {
    #[error("paste one or more PEM certificates (-----BEGIN CERTIFICATE-----).")]
    NoCertificate,
    #[error("that contains a private key. Paste only the certificate, and treat that key as leaked.")]
    PrivateKey,
    #[error("only PEM certificates are accepted (no keys, CSRs or CRLs).")]
    OtherSection,
    #[error("it isn't valid PEM, or a certificate in it can't be read.")]
    Invalid,
    #[error("it's too large (at most {MAX_CA_CERTS} certificates, 64 KB).")]
    TooLarge,
    /// The key can't be verified with ring (RSA < 2048 bits, P-521, …); see `tls::check_key`.
    #[error("{0}")]
    UnusableKey(String),
}

/// Parses and checks a pasted bundle (the printer's own certificate and/or CAs): PEM certificates
/// only, each one parseable.
/// Text outside PEM blocks (e.g. `openssl x509 -text` output) is ignored, but any other PEM
/// section is refused, private keys loudly.
pub fn parse_ca(pem_text: &str) -> Result<Vec<CertificateDer<'static>>, CaError> {
    if pem_text.len() > MAX_CA_BYTES {
        return Err(CaError::TooLarge);
    }
    let mut rd = pem_text.as_bytes();
    let mut certs = Vec::new();
    loop {
        match pem::from_buf(&mut rd) {
            Ok(None) => break,
            Ok(Some((SectionKind::Certificate, der))) => certs.push(CertificateDer::from(der)),
            Ok(Some((
                SectionKind::PrivateKey | SectionKind::RsaPrivateKey | SectionKind::EcPrivateKey,
                _,
            ))) => {
                return Err(CaError::PrivateKey);
            }
            Ok(Some(_)) => return Err(CaError::OtherSection),
            // A key with a label rustls doesn't know (e.g. ENCRYPTED PRIVATE KEY) lands here.
            Err(_) if pem_text.contains("PRIVATE KEY-----") => return Err(CaError::PrivateKey),
            Err(_) => return Err(CaError::Invalid),
        }
        if certs.len() > MAX_CA_CERTS {
            return Err(CaError::TooLarge);
        }
    }
    if pem_text.contains("PRIVATE KEY-----") {
        return Err(CaError::PrivateKey);
    }
    if certs.is_empty() {
        return Err(CaError::NoCertificate);
    }
    for der in &certs {
        super::tls::check_key(der).map_err(|why| {
            if why == "it can't be read" {
                CaError::Invalid
            } else {
                CaError::UnusableKey(why)
            }
        })?;
    }
    Ok(certs)
}

/// The bundle as stored: trimmed, Unix newlines, `None` when empty.
pub fn normalise_ca(input: &str) -> Option<String> {
    let text = input.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    (!text.is_empty()).then(|| format!("{text}\n"))
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

    const CA: &str = include_str!("../../tests/fixtures/tls/ca.pem");
    const OTHER_CA: &str = include_str!("../../tests/fixtures/tls/other_ca.pem");

    #[test]
    fn ca_bundles_are_checked() {
        assert_eq!(parse_ca(CA).unwrap().len(), 1);
        assert_eq!(parse_ca(&format!("{CA}{OTHER_CA}")).unwrap().len(), 2);
        // Text around the blocks (`openssl x509 -text` output) is fine.
        assert_eq!(parse_ca(&format!("Subject: CN=x\n{CA}")).unwrap().len(), 1);
        assert_eq!(parse_ca(""), Err(CaError::NoCertificate));
        assert_eq!(parse_ca("hello"), Err(CaError::NoCertificate));
        let key = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n";
        assert_eq!(parse_ca(&format!("{CA}{key}")), Err(CaError::PrivateKey));
        let enc = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n";
        assert_eq!(parse_ca(enc), Err(CaError::PrivateKey));
        let garbage = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        assert_eq!(parse_ca(garbage), Err(CaError::Invalid));
        assert_eq!(parse_ca(&CA.repeat(17)), Err(CaError::TooLarge));
        assert_eq!(parse_ca(&"x".repeat(MAX_CA_BYTES + 1)), Err(CaError::TooLarge));
    }

    #[test]
    fn ca_text_is_normalised() {
        assert_eq!(normalise_ca("  \r\n "), None);
        assert_eq!(normalise_ca("\r\na\r\nb\r\n\r\n").as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn ca_clients_are_cached_per_bundle() {
        let e = egress("10.0.0.0/8");
        e.client_for(Trust::System).unwrap();
        e.client_for(Trust::Custom("  ")).unwrap();
        assert!(
            e.ca_clients.lock().unwrap().is_empty(),
            "no CA uses the shared client"
        );
        e.client_for(Trust::Custom(CA)).unwrap();
        e.client_for(Trust::Custom(CA)).unwrap();
        e.client_for(Trust::Custom(OTHER_CA)).unwrap();
        assert_eq!(e.ca_clients.lock().unwrap().len(), 2);
        e.client_for(Trust::Insecure).unwrap();
        e.client_for(Trust::Insecure).unwrap();
        assert_eq!(e.ca_clients.lock().unwrap().len(), 3);
        let err = e.client_for(Trust::Custom("nope")).unwrap_err();
        assert!(matches!(err, MoonrakerError::Config(_)), "{err:?}");
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
