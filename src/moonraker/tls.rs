//! TLS trust for a printer with its own certificates (`printers.ca_cert`).
//!
//! Each pasted certificate counts two ways:
//! * **Pinned:** if the server presents exactly that certificate (byte for byte), it is trusted.
//!   That is how a self-signed printer certificate works: it usually has `CA:TRUE`, no matching
//!   host name and a long or lapsed validity, all of which a CA check refuses. Old recipes also
//!   make version 1 certificates, which webpki won't parse as a server certificate, so a pin's
//!   handshake signature is checked against the key read from the pin itself
//!   ([`webpki::anchor_from_trusted_cert`] reads v1). That still proves the server holds the key.
//! * **CA:** otherwise the chain is checked against the pasted certificates as the only roots,
//!   with the usual host name and validity checks.
//!
//! The system roots are never used for such a printer. [`check_key`] refuses, at save time, keys
//! that ring can't verify (RSA under 2048 bits, P-521, …), which would otherwise only fail on
//! connect with an obscure "BadSignature" or handshake alert.

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
    verify_tls13_signature_with_raw_key,
};
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, PeerMisbehaved, RootCertStore,
    SignatureScheme,
};
use sha2::{Digest, Sha256};

/// A rustls client config that trusts only `certs`, pinned or as CAs (see the module docs).
pub fn client_config(certs: Vec<CertificateDer<'static>>) -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(certs.iter().cloned());
    // No usable root just means pin-only.
    let ca = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .ok();
    let pins = certs
        .into_iter()
        .map(|der| {
            let spki = spki_der(&der);
            Pin { der, spki }
        })
        .collect();
    let verifier = Arc::new(PinnedOrCa {
        pins,
        ca,
        provider: provider.clone(),
    });
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

/// A rustls client config that accepts any server certificate and skips the handshake signature
/// check (`printers.tls_skip_verify`). The traffic is still encrypted, but anyone on the path can
/// impersonate the printer. Skipping the signature check too is deliberate: it is what makes this
/// work for keys ring can't verify (RSA < 2048, P-521), which is when people reach for it.
pub fn insecure_config() -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(AcceptAny {
        provider: provider.clone(),
    });
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[derive(Debug)]
struct AcceptAny {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Why a certificate's key can't be used, or `Ok` when ring can verify signatures made with it.
pub fn check_key(cert: &CertificateDer<'_>) -> Result<(), String> {
    let anchor = webpki::anchor_from_trusted_cert(cert).map_err(|_| "it can't be read".to_string())?;
    let (alg_id, key) = split_spki(&anchor.subject_public_key_info).ok_or("its public key can't be read")?;
    let algs = rustls::crypto::ring::default_provider().signature_verification_algorithms;
    if !algs.all.iter().any(|a| a.public_key_alg_id().as_ref() == alg_id) {
        let what = if alg_id.windows(EC_P521.len()).any(|w| w == EC_P521) {
            "an ECDSA P-521 key"
        } else {
            "a key type"
        };
        return Err(format!(
            "it has {what} printpit can't use. Use RSA (2048 bits or more), ECDSA P-256/P-384 or \
             Ed25519, e.g. regenerate it with `openssl req -x509 -newkey rsa:2048 …`."
        ));
    }
    if alg_id.starts_with(RSA_ENCRYPTION) {
        let bits = rsa_bits(key).ok_or("its RSA key can't be read")?;
        if bits < 2048 {
            return Err(format!(
                "its RSA key is {bits} bits, too weak to verify (2048 or more is needed). Regenerate \
                 it, e.g. `openssl req -x509 -newkey rsa:2048 …`."
            ));
        }
    }
    Ok(())
}

/// `SHA-256 ab:cd:…`, as `openssl x509 -fingerprint -sha256` prints it (no names or addresses).
pub fn fingerprint(cert: &[u8]) -> String {
    let hex: Vec<String> = Sha256::digest(cert).iter().map(|b| format!("{b:02X}")).collect();
    format!("SHA-256 {}", hex.join(":"))
}

#[derive(Debug)]
struct Pin {
    der: CertificateDer<'static>,
    /// The pin's public key as full SPKI DER; `None` if webpki can't read it (then the standard
    /// signature check runs, and fails the same way it would without the pin).
    spki: Option<SubjectPublicKeyInfoDer<'static>>,
}

#[derive(Debug)]
struct PinnedOrCa {
    pins: Vec<Pin>,
    ca: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
}

impl PinnedOrCa {
    fn pin_key(&self, cert: &CertificateDer<'_>) -> Option<&SubjectPublicKeyInfoDer<'static>> {
        self.pins
            .iter()
            .find(|p| p.der.as_ref() == cert.as_ref())
            .and_then(|p| p.spki.as_ref())
    }

    fn algs(&self) -> &WebPkiSupportedAlgorithms {
        &self.provider.signature_verification_algorithms
    }
}

impl ServerCertVerifier for PinnedOrCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if self
            .pins
            .iter()
            .any(|pin| pin.der.as_ref() == end_entity.as_ref())
        {
            return Ok(ServerCertVerified::assertion());
        }
        let ca_error = match &self.ca {
            Some(ca) => {
                match ca.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
                    Ok(ok) => return Ok(ok),
                    // "Unknown issuer" or "CA used as end entity" just mean it isn't from these CAs
                    // (a self-signed certificate that isn't saved); other reasons are worth showing.
                    Err(Error::InvalidCertificate(
                        CertificateError::UnknownIssuer | CertificateError::Other(_),
                    )) => String::new(),
                    Err(e) => format!("{e}; "),
                }
            }
            None => String::new(),
        };
        // Say plainly that it isn't the saved certificate, with a fingerprint the admin can
        // compare against `openssl x509 -noout -fingerprint -sha256 -in <file>`.
        Err(Error::General(format!(
            "{ca_error}the printer sent a certificate that isn't one of the saved ones ({})",
            fingerprint(end_entity)
        )))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        match self.pin_key(cert) {
            Some(spki) => verify_tls12_with_raw_key(message, spki, dss, self.algs()),
            None => verify_tls12_signature(message, cert, dss, self.algs()),
        }
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        match self.pin_key(cert) {
            Some(spki) => verify_tls13_signature_with_raw_key(message, spki, dss, self.algs()),
            None => verify_tls13_signature(message, cert, dss, self.algs()),
        }
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs().supported_schemes()
    }
}

/// rustls' TLS 1.2 check (`verify_tls12_signature`), but against a bare public key.
fn verify_tls12_with_raw_key(
    message: &[u8],
    spki: &SubjectPublicKeyInfoDer<'_>,
    dss: &DigitallySignedStruct,
    algs: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, Error> {
    let bad = || Error::InvalidCertificate(CertificateError::BadSignature);
    let candidates = algs
        .mapping
        .iter()
        .find(|(scheme, _)| *scheme == dss.scheme)
        .map(|(_, a)| *a)
        .ok_or(PeerMisbehaved::SignedHandshakeWithUnadvertisedSigScheme)?;
    let key = webpki::RawPublicKeyEntity::try_from(spki).map_err(|_| bad())?;
    for alg in candidates {
        match key.verify_signature(*alg, message, dss.signature()) {
            Ok(()) => return Ok(HandshakeSignatureValid::assertion()),
            Err(webpki::Error::UnsupportedSignatureAlgorithmForPublicKeyContext(_)) => continue,
            Err(_) => return Err(bad()),
        }
    }
    Err(bad())
}

// DER contents (no tag/length) of the AlgorithmIdentifier OIDs we look at.
const RSA_ENCRYPTION: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const EC_P521: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23];

/// The certificate's SubjectPublicKeyInfo as a full DER SEQUENCE (webpki hands out its contents).
fn spki_der(cert: &CertificateDer<'_>) -> Option<SubjectPublicKeyInfoDer<'static>> {
    let anchor = webpki::anchor_from_trusted_cert(cert).ok()?;
    Some(SubjectPublicKeyInfoDer::from(der_wrap(
        0x30,
        &anchor.subject_public_key_info,
    )))
}

/// Splits SPKI contents into the AlgorithmIdentifier's contents and the key's BIT STRING bytes.
fn split_spki(spki: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, alg_id, rest) = der_read(spki)?;
    let (bit_tag, bits, _) = der_read(rest)?;
    (tag == 0x30 && bit_tag == 0x03 && !bits.is_empty()).then(|| (alg_id, &bits[1..]))
}

/// Modulus size of an RSAPublicKey (`SEQUENCE { INTEGER n, INTEGER e }`).
fn rsa_bits(key: &[u8]) -> Option<usize> {
    let (tag, seq, _) = der_read(key)?;
    let (int_tag, n, _) = der_read(seq)?;
    if tag != 0x30 || int_tag != 0x02 {
        return None;
    }
    let n = &n[n.iter().position(|&b| b != 0)?..];
    Some(n.len() * 8 - n[0].leading_zeros() as usize)
}

/// One DER TLV: (tag, contents, rest). Single-byte tags, definite lengths only.
fn der_read(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize);
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

fn der_wrap(tag: u8, contents: &[u8]) -> Vec<u8> {
    let len = contents.len();
    let mut out = vec![tag];
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(contents);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    fn cert(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    #[test]
    fn usable_keys_pass_the_check() {
        for pem in [
            include_str!("../../tests/fixtures/tls/ca.pem"),
            include_str!("../../tests/fixtures/tls/selfsigned.pem"),
            include_str!("../../tests/fixtures/tls/v1.pem"),
        ] {
            assert_eq!(check_key(&cert(pem)), Ok(()));
        }
    }

    #[test]
    fn unusable_keys_are_explained() {
        let err = check_key(&cert(include_str!("../../tests/fixtures/tls/rsa1024.pem"))).unwrap_err();
        assert!(err.contains("1024 bits"), "{err}");
        let err = check_key(&cert(include_str!("../../tests/fixtures/tls/p521.pem"))).unwrap_err();
        assert!(err.contains("P-521"), "{err}");
    }

    #[test]
    fn der_round_trip() {
        for len in [0, 5, 127, 128, 300, 70_000] {
            let contents = vec![7u8; len];
            let wrapped = der_wrap(0x30, &contents);
            assert_eq!(der_read(&wrapped), Some((0x30, &contents[..], &[][..])));
        }
    }

    #[test]
    fn fingerprint_matches_openssl_format() {
        let fp = fingerprint(b"");
        assert!(fp.starts_with("SHA-256 E3:B0:C4:42:"), "{fp}");
        assert_eq!(fp.len(), "SHA-256 ".len() + 32 * 3 - 1);
    }
}
