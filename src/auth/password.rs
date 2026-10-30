//! Argon2id password hashing. Hashing is CPU-heavy, so the async helpers run it on the
//! blocking pool.

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

pub const MIN_LEN: usize = 8;
/// Caps the work an attacker can force per login attempt.
pub const MAX_LEN: usize = 256;

pub fn validate_new(password: &str, confirm: &str) -> Result<(), &'static str> {
    let len = password.chars().count();
    if len < MIN_LEN {
        return Err("Password must be at least 8 characters.");
    }
    if len > MAX_LEN {
        return Err("Password must be at most 256 characters.");
    }
    if password != confirm {
        return Err("Passwords don't match.");
    }
    Ok(())
}

pub fn hash(password: &str) -> Result<String, argon2::password_hash::Error> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
}

pub fn verify(password: &str, hash: &str) -> bool {
    password.len() <= MAX_LEN * 4
        && Argon2::default()
            .verify_password(password.as_bytes(), hash)
            .is_ok()
}

/// A real hash to verify against when the username doesn't exist, so response timing
/// doesn't reveal which usernames are valid.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash("printpit-dummy-password").expect("hashing works"));

pub async fn hash_async(password: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || hash(&password).map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

/// Verifies against `hash`, or against a dummy hash when `None` (always returns false then).
pub async fn verify_async(password: String, hash: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || match hash {
        Some(h) => verify(&password, &h),
        None => {
            verify(&password, &DUMMY_HASH);
            false
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify("correct horse", &h));
        assert!(!verify("wrong horse", &h));
        assert!(!verify("correct horse", "not a hash"));
    }

    #[test]
    fn hashes_are_salted() {
        assert_ne!(hash("same").unwrap(), hash("same").unwrap());
    }

    #[test]
    fn validation() {
        assert!(validate_new("short", "short").is_err());
        assert!(validate_new("long enough", "different").is_err());
        assert!(validate_new(&"x".repeat(257), &"x".repeat(257)).is_err());
        assert!(validate_new("long enough", "long enough").is_ok());
    }

    #[tokio::test]
    async fn unknown_user_never_verifies() {
        assert!(!verify_async("printpit-dummy-password".into(), None).await);
    }
}
