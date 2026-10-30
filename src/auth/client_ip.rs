//! Client IP resolution that only trusts `X-Forwarded-For` from configured proxies.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use ipnet::IpNet;

use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

impl ClientIp {
    pub fn to_string_opt(self) -> Option<String> {
        self.0.map(|ip| ip.to_string())
    }
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        let xff: Vec<&str> = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        Ok(Self(resolve(peer, &xff.join(","), &state.config.trusted_proxies)))
    }
}

/// If the peer is a trusted proxy, walk `X-Forwarded-For` from the right and return the first
/// address that isn't itself a trusted proxy. Otherwise the peer address is the client.
pub fn resolve(peer: Option<IpAddr>, xff: &str, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = peer?.to_canonical();
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|net| net.contains(ip));
    if !is_trusted(&peer) {
        return Some(peer);
    }
    let mut leftmost = None;
    for hop in xff.rsplit(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Ok(ip) = hop.parse::<IpAddr>().map(|ip| ip.to_canonical()) else {
            break;
        };
        if !is_trusted(&ip) {
            return Some(ip);
        }
        leftmost = Some(ip);
    }
    leftmost.or(Some(peer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn untrusted_peer_ignores_header() {
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        assert_eq!(resolve(ip("203.0.113.9"), "1.2.3.4", &trusted), ip("203.0.113.9"));
        assert_eq!(resolve(ip("203.0.113.9"), "", &[]), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_hop() {
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        assert_eq!(
            resolve(ip("10.0.0.2"), "6.6.6.6, 198.51.100.7, 10.0.0.3", &trusted),
            ip("198.51.100.7")
        );
        assert_eq!(resolve(ip("10.0.0.2"), "", &trusted), ip("10.0.0.2"));
        assert_eq!(resolve(ip("10.0.0.2"), "10.0.0.9", &trusted), ip("10.0.0.9"));
        assert_eq!(
            resolve(ip("10.0.0.2"), "garbage, 10.0.0.9", &trusted),
            ip("10.0.0.9")
        );
    }

    #[test]
    fn mapped_ipv6_is_canonicalised() {
        assert_eq!(resolve(ip("::ffff:192.0.2.1"), "", &[]), ip("192.0.2.1"));
        assert_eq!(resolve(None, "1.2.3.4", &[]), None);
    }
}
