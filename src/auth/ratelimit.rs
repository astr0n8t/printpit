//! In-memory limiter for failed login attempts, keyed per client IP and per username.
//!
//! Only failures count, so normal use never trips it. A successful login clears the
//! username's counter but not the IP's, so a valid account can't be used to reset an
//! attacker's IP budget.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(15 * 60);
pub const MAX_PER_IP: u32 = 20;
pub const MAX_PER_USER: u32 = 10;

#[derive(Default)]
pub struct LoginLimiter {
    windows: Mutex<HashMap<String, Window>>,
}

struct Window {
    start: Instant,
    failures: u32,
}

fn ip_key(ip: Option<IpAddr>) -> String {
    match ip {
        Some(ip) => format!("ip:{ip}"),
        None => "ip:unknown".into(),
    }
}

fn user_key(username: &str) -> String {
    format!("user:{}", username.to_ascii_lowercase())
}

impl LoginLimiter {
    /// `Err(retry_after)` if either the IP or the username is over its budget.
    pub fn check(&self, ip: Option<IpAddr>, username: &str) -> Result<(), Duration> {
        self.check_at(Instant::now(), ip, username)
    }

    pub fn record_failure(&self, ip: Option<IpAddr>, username: &str) {
        self.fail_at(Instant::now(), ip, username);
    }

    pub fn record_success(&self, username: &str) {
        self.lock().remove(&user_key(username));
    }

    /// Drops expired windows; called from the periodic cleanup task.
    pub fn prune(&self) {
        let now = Instant::now();
        self.lock().retain(|_, w| now.duration_since(w.start) < WINDOW);
    }

    fn check_at(&self, now: Instant, ip: Option<IpAddr>, username: &str) -> Result<(), Duration> {
        let windows = self.lock();
        let over = |key: String, max: u32| {
            windows.get(&key).and_then(|w| {
                let age = now.duration_since(w.start);
                (age < WINDOW && w.failures >= max).then(|| WINDOW - age)
            })
        };
        match over(ip_key(ip), MAX_PER_IP).max(over(user_key(username), MAX_PER_USER)) {
            Some(retry) => Err(retry),
            None => Ok(()),
        }
    }

    fn fail_at(&self, now: Instant, ip: Option<IpAddr>, username: &str) {
        let mut windows = self.lock();
        for key in [ip_key(ip), user_key(username)] {
            let w = windows.entry(key).or_insert(Window {
                start: now,
                failures: 0,
            });
            if now.duration_since(w.start) >= WINDOW {
                *w = Window {
                    start: now,
                    failures: 0,
                };
            }
            w.failures += 1;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Window>> {
        self.windows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn username_budget() {
        let l = LoginLimiter::default();
        let t = Instant::now();
        for _ in 0..MAX_PER_USER {
            assert!(l.check_at(t, ip("1.1.1.1"), "Admin").is_ok());
            l.fail_at(t, ip("1.1.1.1"), "Admin");
        }
        // Case-insensitive, and applies from any IP.
        assert!(l.check_at(t, ip("2.2.2.2"), "admin").is_err());
        assert!(l.check_at(t, ip("2.2.2.2"), "other").is_ok());
        // Window expiry.
        assert!(l.check_at(t + WINDOW, ip("2.2.2.2"), "admin").is_ok());
    }

    #[test]
    fn ip_budget_survives_success() {
        let l = LoginLimiter::default();
        let t = Instant::now();
        for i in 0..MAX_PER_IP {
            l.fail_at(t, ip("3.3.3.3"), &format!("user{i}"));
        }
        l.record_success("someone");
        let retry = l
            .check_at(t + Duration::from_secs(60), ip("3.3.3.3"), "fresh")
            .unwrap_err();
        assert_eq!(retry, WINDOW - Duration::from_secs(60));
    }

    #[test]
    fn success_clears_username() {
        let l = LoginLimiter::default();
        let t = Instant::now();
        for _ in 0..MAX_PER_USER {
            l.fail_at(t, ip("4.4.4.4"), "bob");
        }
        l.record_success("bob");
        assert!(l.check_at(t, ip("5.5.5.5"), "bob").is_ok());
    }
}
