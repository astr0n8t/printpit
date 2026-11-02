//! `printpit-slicer healthcheck`: probes `GET /v1/health` for Docker's HEALTHCHECK, so the image
//! needs no curl. It reads only the bind address and the token.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::{self, ConfigError, Token};

pub fn run() -> Result<(), String> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let bind = config::bind_addr(&get).map_err(|e| e.to_string())?;
    let token = config::secret(&get, "PRINTPIT_SLICER_TOKEN")
        .and_then(|t| {
            t.ok_or(ConfigError::Missing {
                var: "PRINTPIT_SLICER_TOKEN",
            })
        })
        .and_then(|t| Token::parse(&t))
        .map_err(|e| e.to_string())?;
    probe(target(bind), &token)
}

/// A wildcard bind address isn't connectable; probe loopback instead.
fn target(bind: SocketAddr) -> SocketAddr {
    match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), bind.port()),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), bind.port()),
        _ => bind,
    }
}

pub fn probe(addr: SocketAddr, token: &Token) -> Result<(), String> {
    let timeout = Duration::from_secs(3);
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "GET /v1/health HTTP/1.0\r\nHost: localhost\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",
        token.bearer()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream
        .take(4096)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or_default();
    if status_line.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().trim();
        Err(format!("unhealthy: {status_line} {body}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_maps_to_loopback() {
        assert_eq!(
            target("0.0.0.0:8090".parse().unwrap()),
            "127.0.0.1:8090".parse().unwrap()
        );
        assert_eq!(
            target("[::]:8090".parse().unwrap()),
            "[::1]:8090".parse().unwrap()
        );
        assert_eq!(
            target("10.0.0.5:80".parse().unwrap()),
            "10.0.0.5:80".parse().unwrap()
        );
    }
}
