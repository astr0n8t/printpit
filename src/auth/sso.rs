//! Trusted-header single sign-on: an auth proxy (Authelia, authentik, oauth2-proxy…) in front
//! of printpit signs the user in and passes their username in a header such as `Remote-User`.
//!
//! The header is believed only when the *direct* TCP peer is one of `PRINTPIT_AUTH_PROXY_CIDRS`
//! (never via `X-Forwarded-For`, which a client can write). It only ever signs in an existing,
//! enabled printpit user; it never creates one. Header sign-ins skip TOTP: the proxy does MFA.

use std::net::IpAddr;

use axum::http::HeaderMap;

use crate::config::Config;

/// The username the proxy vouches for: `None` when SSO is off, the peer isn't a configured
/// proxy, or the header is missing or empty.
pub fn header_user(config: &Config, headers: &HeaderMap, peer: Option<IpAddr>) -> Option<String> {
    let name = config.auth_header.as_ref()?;
    let peer = peer?.to_canonical();
    if !config.auth_proxies.iter().any(|net| net.contains(&peer)) {
        return None;
    }
    // Two copies of the header are ambiguous: refuse rather than guess which one counts.
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let user = value.to_str().ok()?.trim();
    (!user.is_empty()).then(|| user.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(header: bool) -> Config {
        let mut vars = HashMap::from([
            (
                "PRINTPIT_SECRET_KEY",
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
            ),
            ("PRINTPIT_AUTH_PROXY_CIDRS", "10.0.0.2, fd00::/64"),
        ]);
        if header {
            vars.insert("PRINTPIT_AUTH_HEADER", "Remote-User");
        }
        Config::from_lookup(|k| vars.get(k).map(|v| v.to_string())).unwrap()
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("remote-user", v.parse().unwrap());
        }
        h
    }

    #[test]
    fn trusts_only_the_configured_proxy() {
        let c = config(true);
        let proxy: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(
            header_user(&c, &headers(&[" alice "]), Some(proxy)),
            Some("alice".into())
        );
        assert_eq!(
            header_user(&c, &headers(&["alice"]), Some("::ffff:10.0.0.2".parse().unwrap())),
            Some("alice".into()),
            "IPv4-mapped IPv6 peer"
        );
        assert_eq!(
            header_user(&c, &headers(&["alice"]), Some("fd00::5".parse().unwrap())),
            Some("alice".into())
        );
        assert_eq!(
            header_user(&c, &headers(&["alice"]), Some("10.0.0.3".parse().unwrap())),
            None
        );
        assert_eq!(header_user(&c, &headers(&["alice"]), None), None);
        assert_eq!(header_user(&c, &headers(&[]), Some(proxy)), None);
        assert_eq!(header_user(&c, &headers(&["  "]), Some(proxy)), None);
        assert_eq!(header_user(&c, &headers(&["alice", "admin"]), Some(proxy)), None);
        assert_eq!(
            header_user(&config(false), &headers(&["alice"]), Some(proxy)),
            None
        );
    }
}
