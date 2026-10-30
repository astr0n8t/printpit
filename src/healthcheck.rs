//! `printpit healthcheck`: a tiny HTTP probe for Docker's HEALTHCHECK, so the image needs no curl.
//!
//! It reads only `PRINTPIT_BIND` (not the full config) so it works without secrets.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::DEFAULT_BIND;

pub fn run() -> Result<(), String> {
    let bind = std::env::var("PRINTPIT_BIND")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_string());
    let addr: SocketAddr = bind
        .trim()
        .parse()
        .map_err(|e| format!("bad PRINTPIT_BIND: {e}"))?;
    probe(target(addr))
}

/// A wildcard bind address isn't connectable; probe loopback instead.
fn target(bind: SocketAddr) -> SocketAddr {
    match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), bind.port()),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), bind.port()),
        _ => bind,
    }
}

pub fn probe(addr: SocketAddr) -> Result<(), String> {
    let timeout = Duration::from_secs(3);
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream
        .take(4096)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let status_line = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    if status_line.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("unhealthy: {status_line}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_maps_to_loopback() {
        assert_eq!(
            target("0.0.0.0:8080".parse().unwrap()),
            "127.0.0.1:8080".parse().unwrap()
        );
        assert_eq!(
            target("[::]:8080".parse().unwrap()),
            "[::1]:8080".parse().unwrap()
        );
        assert_eq!(
            target("10.0.0.5:80".parse().unwrap()),
            "10.0.0.5:80".parse().unwrap()
        );
    }
}
