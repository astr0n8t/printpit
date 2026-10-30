//! Random tokens, hashing helpers, and AES-256-GCM sealing of secrets at rest (printer API keys).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::SecretKey;

/// Fills a buffer from the OS CSPRNG. An RNG failure is unrecoverable for a security
/// product, so this panics rather than continuing with weak randomness.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random number generator failed");
    buf
}

/// 256-bit random token, URL-safe base64 without padding (43 chars).
pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Seals secrets with AES-256-GCM under a subkey derived (HKDF-SHA256) from
/// `PRINTPIT_SECRET_KEY`, so the master key is never used directly.
///
/// Sealed format: `version (1) || nonce (12) || ciphertext+tag`.
pub struct Cipher {
    aead: Aes256Gcm,
}

const SEALED_V1: u8 = 1;
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
#[error("cannot decrypt secret (was PRINTPIT_SECRET_KEY changed?)")]
pub struct OpenError;

impl Cipher {
    pub fn new(master: &SecretKey) -> Self {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(b"printpit"), &master.0);
        let mut key = [0u8; 32];
        hk.expand(b"printpit secrets-at-rest v1", &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self {
            aead: Aes256Gcm::new_from_slice(&key).expect("32-byte key"),
        }
    }

    /// `context` is bound as associated data, so a sealed value can't be replayed into a
    /// different field.
    pub fn seal(&self, plaintext: &str, context: &str) -> Vec<u8> {
        let nonce_bytes = random_bytes::<NONCE_LEN>();
        let ciphertext = self
            .aead
            .encrypt(
                &Nonce::from(nonce_bytes),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: context.as_bytes(),
                },
            )
            .expect("AES-GCM encryption cannot fail for small inputs");
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
        out.push(SEALED_V1);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        out
    }

    pub fn open(&self, sealed: &[u8], context: &str) -> Result<String, OpenError> {
        let (&version, rest) = sealed.split_first().ok_or(OpenError)?;
        if version != SEALED_V1 || rest.len() < NONCE_LEN {
            return Err(OpenError);
        }
        let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| OpenError)?;
        let plain = self
            .aead
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| OpenError)?;
        String::from_utf8(plain).map_err(|_| OpenError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(byte: u8) -> Cipher {
        Cipher::new(&SecretKey([byte; 32]))
    }

    #[test]
    fn seal_round_trip() {
        let c = cipher(7);
        let sealed = c.seal("my-api-key", "printer-api-key");
        assert_eq!(sealed[0], SEALED_V1);
        assert!(
            !sealed.windows(10).any(|w| w == b"my-api-key"),
            "plaintext must not leak"
        );
        assert_eq!(c.open(&sealed, "printer-api-key").unwrap(), "my-api-key");
        assert_ne!(sealed, c.seal("my-api-key", "printer-api-key"), "random nonce");
    }

    #[test]
    fn open_rejects_wrong_key_context_or_tampering() {
        let sealed = cipher(7).seal("secret", "ctx");
        assert!(cipher(8).open(&sealed, "ctx").is_err());
        assert!(cipher(7).open(&sealed, "other").is_err());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(cipher(7).open(&bad, "ctx").is_err());
        assert!(cipher(7).open(&[], "ctx").is_err());
        assert!(cipher(7).open(&[2, 0, 0], "ctx").is_err());
    }

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
