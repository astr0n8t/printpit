//! Authentication and authorization: passwords, sessions, CSRF, roles, rate limiting and
//! first-run setup.

pub mod client_ip;
pub mod csrf;
pub mod password;
pub mod ratelimit;
pub mod session;
pub mod setup;

pub use crate::db::users::Role;
pub use client_ip::ClientIp;
pub use session::{Admin, CurrentUser, Operator, RequireRole};

/// Usernames: 1–32 chars of letters, digits, `.`, `_`, `-`.
pub fn validate_username(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 32 {
        return Err("Username must be 1–32 characters.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("Username may only contain letters, digits, '.', '_' and '-'.");
    }
    Ok(())
}

/// Returns `next` if it's a safe same-origin path, else `/`. Prevents open redirects.
pub fn safe_next(next: Option<&str>) -> &str {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.chars().any(char::is_control) =>
        {
            n
        }
        _ => "/",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert!(validate_username("nathan").is_ok());
        assert!(validate_username("a.b_c-1").is_ok());
        assert!(validate_username("").is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username(&"x".repeat(33)).is_err());
        assert!(validate_username("é").is_err());
    }

    #[test]
    fn next_paths() {
        assert_eq!(safe_next(Some("/ui/files?x=1")), "/ui/files?x=1");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("/\\evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(Some("/a\nb")), "/");
        assert_eq!(safe_next(None), "/");
    }
}
