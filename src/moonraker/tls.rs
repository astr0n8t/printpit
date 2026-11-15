//! TLS trust for a printer with its own certificates (`printers.ca_cert`).
//!
//! Each pasted certificate counts two ways:
//! * **Pinned:** if the server presents exactly that certificate (byte for byte), it is trusted.
//!   That is how a self-signed printer certificate works: it usually has `CA:TRUE`, no matching
//!   host name and a long or lapsed validity, all of which a CA check refuses. The handshake still
//!   proves the server holds the matching private key.
//! * **CA:** otherwise the chain is checked against the pasted certificates as the only roots,
//!   with the usual host name and validity checks.
//!
//! The system roots are never used for such a printer.

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme};

/// A rustls client config that trusts only `certs`, pinned or as CAs (see the module docs).
pub fn client_config(certs: Vec<CertificateDer<'static>>) -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(certs.iter().cloned());
    // No usable root just means pin-only.
    let ca = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .ok();
    let verifier = Arc::new(PinnedOrCa {
        pins: certs,
        ca,
        provider: provider.clone(),
    });
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[derive(Debug)]
struct PinnedOrCa {
    pins: Vec<CertificateDer<'static>>,
    ca: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
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
        if self.pins.iter().any(|pin| pin.as_ref() == end_entity.as_ref()) {
            return Ok(ServerCertVerified::assertion());
        }
        match &self.ca {
            Some(ca) => ca.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now),
            None => Err(Error::InvalidCertificate(CertificateError::UnknownIssuer)),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
