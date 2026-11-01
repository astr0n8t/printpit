//! TOTP second factor (RFC 6238: HMAC-SHA1, 6 digits, 30 s steps, ±1 step of clock drift),
//! single-use recovery codes, and the short-lived token that carries a half-finished login
//! from the password step to the code step.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use qrcode::QrCode;
use qrcode::render::svg;
use sha1::Sha1;

use crate::crypto::{Cipher, constant_time_eq, random_bytes, sha256};

pub const STEP_SECS: i64 = 30;
const DIGITS: u32 = 6;
/// Accept the previous and next step too, for phones whose clock is a little off.
const DRIFT: i64 = 1;
pub const RECOVERY_CODES: usize = 10;
/// How long the code step may take after the password step.
pub const PENDING_SECS: i64 = 300;
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A fresh 160-bit secret (the size RFC 4226 recommends for SHA-1).
pub fn new_secret() -> Vec<u8> {
    random_bytes::<20>().to_vec()
}

/// RFC 4648 base32 without padding, as authenticator apps expect.
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !matches!(c, b' ' | b'-' | b'=')) {
        let v = B32.iter().position(|&x| x == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// The secret grouped in fours for typing into an app by hand.
pub fn display_key(secret: &[u8]) -> String {
    base32(secret)
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn step(unix: i64) -> i64 {
    unix.div_euclid(STEP_SECS)
}

/// The HOTP value for one time step.
pub fn code_at(secret: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&(step as u64).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([hash[offset], hash[offset + 1], hash[offset + 2], hash[offset + 3]])
        & 0x7fff_ffff;
    bin % 10u32.pow(DIGITS)
}

/// What the user typed in the code box.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Code(String),
    Recovery(String),
    Invalid,
}

/// Six digits are a TOTP code; ten base32 characters (dashes and spaces ignored) a recovery
/// code.
pub fn parse_entry(input: &str) -> Entry {
    let s: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    if s.len() == DIGITS as usize && s.bytes().all(|b| b.is_ascii_digit()) {
        Entry::Code(s)
    } else if s.len() == 10
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    {
        Entry::Recovery(s)
    } else {
        Entry::Invalid
    }
}

/// The time step a code matches, if it matches one within the drift window that is later
/// than `after` (the last step already used). Callers must then record the step atomically.
pub fn verify(secret: &[u8], code: &str, unix: i64, after: Option<i64>) -> Option<i64> {
    let now = step(unix);
    (now - DRIFT..=now + DRIFT)
        .filter(|s| after.is_none_or(|a| *s > a))
        .find(|s| {
            let expected = format!("{:0width$}", code_at(secret, *s), width = DIGITS as usize);
            constant_time_eq(expected.as_bytes(), code.as_bytes())
        })
}

/// `otpauth://totp/printpit:alice?secret=…&issuer=printpit`
pub fn otpauth_uri(username: &str, secret: &[u8]) -> String {
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    format!(
        "otpauth://totp/printpit:{}?secret={}&issuer=printpit&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        enc(username).replace('+', "%20"),
        base32(secret)
    )
}

/// The enrolment QR code as an inline `<svg>` (no `style` attributes, so the CSP allows it).
pub fn qr_svg(data: &str) -> String {
    let Ok(code) = QrCode::new(data.as_bytes()) else {
        return String::new();
    };
    let svg = code
        .render::<svg::Color<'_>>()
        .min_dimensions(200, 200)
        .dark_color(svg::Color("#000"))
        .light_color(svg::Color("#fff"))
        .build();
    // Drop the XML prolog; the element goes straight into HTML.
    match svg.find("<svg") {
        Some(i) => svg[i..].to_string(),
        None => String::new(),
    }
}

/// Ten fresh recovery codes, formatted `abcde-fghij`.
pub fn new_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODES)
        .map(|_| {
            let raw = base32(&random_bytes::<7>()).to_ascii_lowercase();
            format!("{}-{}", &raw[..5], &raw[5..10])
        })
        .collect()
}

/// What's stored for a recovery code (given in [`parse_entry`]'s normalised form, or as shown).
pub fn recovery_hash(code: &str) -> Vec<u8> {
    let normal: String = code
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    sha256(format!("printpit-recovery:{normal}").as_bytes())
}

pub fn secret_context(user_id: i64) -> String {
    format!("totp-secret:{user_id}")
}

/// AAD for a secret that is being set up but not yet confirmed (kept sealed in the form).
pub fn enroll_context(user_id: i64) -> String {
    format!("totp-enroll:{user_id}")
}

pub fn seal_b64(cipher: &Cipher, plaintext: &str, context: &str) -> String {
    URL_SAFE_NO_PAD.encode(cipher.seal(plaintext, context))
}

pub fn open_b64(cipher: &Cipher, sealed: &str, context: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(sealed.trim()).ok()?;
    cipher.open(&bytes, context).ok()
}

/// A login that has passed the password step and still needs a code.
#[derive(Debug, PartialEq, Eq)]
pub struct Pending {
    pub user_id: i64,
    pub expires: i64,
    pub next: String,
}

const PENDING_CONTEXT: &str = "totp-pending";

impl Pending {
    /// Sealed (encrypted and authenticated), so the browser can't read or forge it.
    pub fn seal(&self, cipher: &Cipher) -> String {
        let plain = format!("{}\n{}\n{}", self.user_id, self.expires, self.next);
        seal_b64(cipher, &plain, PENDING_CONTEXT)
    }

    pub fn open(cipher: &Cipher, sealed: &str, now: i64) -> Option<Self> {
        let plain = open_b64(cipher, sealed, PENDING_CONTEXT)?;
        let mut parts = plain.splitn(3, '\n');
        let p = Pending {
            user_id: parts.next()?.parse().ok()?,
            expires: parts.next()?.parse().ok()?,
            next: parts.next()?.to_string(),
        };
        (p.expires > now).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretKey;

    /// RFC 6238 appendix B (SHA-1), last six digits.
    #[test]
    fn rfc6238_vectors() {
        let secret = b"12345678901234567890";
        for (t, expected) in [
            (59, 287_082),
            (1_111_111_109, 81_804),
            (1_111_111_111, 50_471),
            (1_234_567_890, 5_924),
            (2_000_000_000, 279_037),
            (20_000_000_000, 353_130),
        ] {
            assert_eq!(code_at(secret, step(t)), expected, "t={t}");
        }
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW 6YTB-OI").unwrap(), b"foobar");
        assert_eq!(base32_decode("not!base32"), None);
        let s = new_secret();
        assert_eq!(base32_decode(&base32(&s)).unwrap(), s);
        assert_eq!(base32(&s).len(), 32);
        assert_eq!(display_key(b"foobar"), "MZXW 6YTB OI");
    }

    #[test]
    fn verify_window_and_replay() {
        let secret = b"12345678901234567890";
        let t = 1_111_111_111;
        assert_eq!(verify(secret, "050471", t, None), Some(step(t)));
        // A code from the step before still works (clock drift)…
        let prev = format!("{:06}", code_at(secret, step(t) - 1));
        assert_eq!(verify(secret, &prev, t, None), Some(step(t) - 1));
        // …but not two steps back, and not once that step (or a later one) was used.
        let old = format!("{:06}", code_at(secret, step(t) - 2));
        assert_eq!(verify(secret, &old, t, None), None);
        assert_eq!(verify(secret, "050471", t, Some(step(t))), None);
        assert_eq!(verify(secret, &prev, t, Some(step(t) - 1)), None);
        assert_eq!(verify(secret, "000000", t, None), None);
    }

    #[test]
    fn entries() {
        assert_eq!(parse_entry(" 123 456 "), Entry::Code("123456".into()));
        assert_eq!(parse_entry("ABCDE-FGH27"), Entry::Recovery("abcdefgh27".into()));
        for bad in ["", "12345", "1234567", "abcde-fgh1j", "abcdefghijk"] {
            assert_eq!(parse_entry(bad), Entry::Invalid, "{bad}");
        }
    }

    #[test]
    fn recovery_codes() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODES);
        for c in &codes {
            assert_eq!(c.len(), 11);
            assert!(matches!(parse_entry(c), Entry::Recovery(_)), "{c}");
        }
        assert_eq!(recovery_hash("ABCDE-fghij"), recovery_hash("abcdefghij"));
        assert_ne!(recovery_hash(&codes[0]), recovery_hash(&codes[1]));
    }

    #[test]
    fn uri_and_qr() {
        let uri = otpauth_uri("al ice", b"foobar");
        assert!(uri.starts_with("otpauth://totp/printpit:al%20ice?secret=MZXW6YTBOI&issuer=printpit"));
        let svg = qr_svg(&uri);
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(!svg.contains("style="), "CSP forbids style attributes");
    }

    #[test]
    fn pending_tokens() {
        let cipher = Cipher::new(&SecretKey([7; 32]));
        let p = Pending {
            user_id: 3,
            expires: 1000,
            next: "/ui/files?printer=1".into(),
        };
        let sealed = p.seal(&cipher);
        assert_eq!(Pending::open(&cipher, &sealed, 999), Some(p));
        assert_eq!(Pending::open(&cipher, &sealed, 1000), None, "expired");
        let mut tampered = sealed.into_bytes();
        tampered[10] ^= 1;
        assert_eq!(
            Pending::open(&cipher, &String::from_utf8_lossy(&tampered), 0),
            None
        );
        let other = Cipher::new(&SecretKey([8; 32]));
        assert_eq!(Pending::open(&other, &p_sealed(&cipher), 0), None);
    }

    fn p_sealed(cipher: &Cipher) -> String {
        Pending {
            user_id: 1,
            expires: 10,
            next: "/".into(),
        }
        .seal(cipher)
    }
}
