//! Per-printer certificates: form validation, storage, and that https:// printers are trusted only
//! through their own certificates, pinned or as a CA. The mock Moonraker is put behind a TLS front
//! (fixtures in `tests/fixtures/tls/`, valid for 100 years): `server.pem` is signed by `ca.pem`
//! (SANs 127.0.0.1 and localhost); `selfsigned.pem` is what printers usually have: self-signed,
//! `CA:TRUE`, CN=mainsail and no SANs, so it matches neither host name.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::db::printers;

const CA: &str = include_str!("fixtures/tls/ca.pem");
const OTHER_CA: &str = include_str!("fixtures/tls/other_ca.pem");
const SERVER_CERT: &str = include_str!("fixtures/tls/server.pem");
const SERVER_KEY: &str = include_str!("fixtures/tls/server.key");
const SELF_SIGNED: &str = include_str!("fixtures/tls/selfsigned.pem");
const SELF_SIGNED_KEY: &str = include_str!("fixtures/tls/selfsigned.key");
/// A version 1 certificate (`openssl x509 -req -signkey`, no extensions), as old guides make.
const V1: &str = include_str!("fixtures/tls/v1.pem");
const V1_KEY: &str = include_str!("fixtures/tls/v1.key");

const LOCAL: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
];

/// Terminates TLS on a free port with `cert`/`key` and forwards the plaintext to `backend`.
/// Returns the port.
async fn tls_front(backend: u16, cert: &str, key: &str) -> u16 {
    tls_front_versions(backend, cert, key, rustls::DEFAULT_VERSIONS).await
}

/// [`tls_front`] limited to some TLS versions (TLS 1.2 checks signatures differently).
async fn tls_front_versions(
    backend: u16,
    cert: &str,
    key: &str,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
    let signing_key = provider.key_provider.load_private_key(key).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        // A fixed resolver, not `with_single_cert`: that one refuses v1 certificates and
        // mismatched keys, which these tests need to serve.
        .with_cert_resolver(Arc::new(Fixed(Arc::new(CertifiedKey::new(certs, signing_key)))));
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut up) = TcpStream::connect(("127.0.0.1", backend)).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
            });
        }
    });
    port
}

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

async fn setup() -> (TestApp, Client, String, MockMoonraker, u16) {
    setup_with(SERVER_CERT, SERVER_KEY).await
}

async fn setup_with(cert: &str, key: &str) -> (TestApp, Client, String, MockMoonraker, u16) {
    let (app, mut c) = TestApp::with_admin_env(LOCAL).await;
    let csrf = c.session_csrf().await;
    let mock = MockMoonraker::start().await;
    let port = tls_front(mock.port, cert, key).await;
    (app, c, csrf, mock, port)
}

async fn add(c: &mut Client, csrf: &str, url: &str, ca: &str) -> common::Resp {
    c.post_hx(
        "/printers",
        csrf,
        &[
            ("name", "Voron"),
            ("base_url", url),
            ("enabled", "on"),
            ("ca_cert", ca),
        ],
    )
    .await
}

async fn only_printer(app: &TestApp) -> printers::Printer {
    printers::list(&app.state.db).await.unwrap().pop().unwrap()
}

async fn edit(c: &mut Client, csrf: &str, id: i64, url: &str, ca: &str) -> common::Resp {
    c.post_hx(
        &format!("/printers/{id}"),
        csrf,
        &[
            ("name", "Voron"),
            ("base_url", url),
            ("enabled", "on"),
            ("ca_cert", ca),
        ],
    )
    .await
}

async fn test_connection(c: &mut Client, csrf: &str, id: i64) -> String {
    c.post_hx(&format!("/printers/{id}/test"), csrf, &[]).await.body
}

#[tokio::test]
async fn https_printer_is_trusted_only_through_its_own_ca() {
    let (app, mut c, csrf, mock, port) = setup().await;
    for host in ["127.0.0.1", "localhost"] {
        let url = format!("https://{host}:{port}");

        // No CA: the system roots don't know the test CA.
        let res = add(&mut c, &csrf, &url, "").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let id = only_printer(&app).await.id;
        let body = test_connection(&mut c, &csrf, id).await;
        assert!(
            body.contains("OFFLINE") && body.contains("certificate"),
            "{host}: {body}"
        );
        assert!(
            !body.contains(&port.to_string()),
            "no address in the error: {body}"
        );

        // The wrong CA is refused too.
        edit(&mut c, &csrf, id, &url, OTHER_CA).await;
        let body = test_connection(&mut c, &csrf, id).await;
        assert!(
            body.contains("OFFLINE") && body.contains("certificate"),
            "{host}: {body}"
        );

        // The right one works, for HTTP and for the dashboard's WebSocket.
        edit(&mut c, &csrf, id, &url, CA).await;
        let body = test_connection(&mut c, &csrf, id).await;
        assert!(body.contains("READY"), "{host}: {body}");
        let p = only_printer(&app).await;
        let client = app.state.printer_client(&p).unwrap();
        client
            .websocket()
            .await
            .expect("websocket over TLS with the custom CA");

        printers::delete(&app.state.db, id).await.unwrap();
        app.state.printer_changed(id);
    }
    assert!(mock.paths().iter().any(|p| p == "/server/info"));
}

#[tokio::test]
async fn self_signed_printer_certificate_is_pinned() {
    let (app, mut c, csrf, _mock, port) = setup_with(SELF_SIGNED, SELF_SIGNED_KEY).await;
    for host in ["127.0.0.1", "localhost"] {
        let url = format!("https://{host}:{port}");
        add(&mut c, &csrf, &url, "").await;
        let id = only_printer(&app).await.id;
        let body = test_connection(&mut c, &csrf, id).await;
        assert!(
            body.contains("OFFLINE") && body.contains("certificate"),
            "{host}: {body}"
        );

        // Another certificate (here a CA that didn't sign it) doesn't help, and the error says
        // which certificate the printer sent.
        edit(&mut c, &csrf, id, &url, CA).await;
        let body = test_connection(&mut c, &csrf, id).await;
        let sent = CertificateDer::from_pem_slice(SELF_SIGNED.as_bytes()).unwrap();
        assert!(
            body.contains("OFFLINE")
                && body.contains("isn&#39;t one of the saved ones")
                && body.contains(&printpit::moonraker::tls::fingerprint(&sent))
                && !body.contains("unexpected error"),
            "{host}: {body}"
        );

        // The printer's own certificate does, although it is CA:TRUE and names neither host.
        edit(&mut c, &csrf, id, &url, SELF_SIGNED).await;
        let body = test_connection(&mut c, &csrf, id).await;
        assert!(body.contains("READY"), "{host}: {body}");
        let client = app.state.printer_client(&only_printer(&app).await).unwrap();
        client
            .websocket()
            .await
            .expect("websocket with a pinned certificate");

        // Next to unrelated certificates in the same bundle, too.
        edit(&mut c, &csrf, id, &url, &format!("{OTHER_CA}{SELF_SIGNED}")).await;
        assert!(test_connection(&mut c, &csrf, id).await.contains("READY"));

        printers::delete(&app.state.db, id).await.unwrap();
        app.state.printer_changed(id);
    }
}

#[tokio::test]
async fn version_1_certificate_is_pinned_over_tls12_and_tls13() {
    let (app, mut c) = TestApp::with_admin_env(LOCAL).await;
    let csrf = c.session_csrf().await;
    let mock = MockMoonraker::start().await;
    for versions in [&[&rustls::version::TLS12][..], &[&rustls::version::TLS13][..]] {
        let port = tls_front_versions(mock.port, V1, V1_KEY, versions).await;
        let res = add(&mut c, &csrf, &format!("https://127.0.0.1:{port}"), V1).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let p = only_printer(&app).await;
        let body = test_connection(&mut c, &csrf, p.id).await;
        assert!(body.contains("READY"), "{versions:?}: {body}");
        app.state
            .printer_client(&p)
            .unwrap()
            .websocket()
            .await
            .expect("websocket with a pinned v1 certificate");
        printers::delete(&app.state.db, p.id).await.unwrap();
        app.state.printer_changed(p.id);
    }
}

#[tokio::test]
async fn pin_still_needs_the_matching_key() {
    // The front presents the self-signed certificate but signs with another key: the handshake
    // must fail even though the certificate bytes match the pin.
    let (app, mut c, csrf, _mock, port) = setup_with(SELF_SIGNED, SERVER_KEY).await;
    add(&mut c, &csrf, &format!("https://127.0.0.1:{port}"), SELF_SIGNED).await;
    let body = test_connection(&mut c, &csrf, only_printer(&app).await.id).await;
    assert!(body.contains("OFFLINE"), "{body}");
}

#[tokio::test]
async fn ca_signed_leaf_can_be_pinned_too() {
    let (app, mut c, csrf, _mock, port) = setup().await;
    add(&mut c, &csrf, &format!("https://127.0.0.1:{port}"), SERVER_CERT).await;
    let id = only_printer(&app).await.id;
    assert!(test_connection(&mut c, &csrf, id).await.contains("READY"));
}

#[tokio::test]
async fn ca_is_stored_shown_and_cleared() {
    let (app, mut c, csrf, _mock, port) = setup().await;
    let url = format!("https://127.0.0.1:{port}");
    // Windows line endings and surrounding blank lines are tidied up.
    let pasted = format!("\r\n{}\r\n", CA.replace('\n', "\r\n"));
    add(&mut c, &csrf, &url, &pasted).await;
    let p = only_printer(&app).await;
    assert_eq!(p.ca_cert.as_deref(), Some(CA));

    let list = c.get("/ui/printers").await.body;
    assert!(list.contains(">CERT</span>"), "{list}");
    let form = c.get(&format!("/ui/printers/{}", p.id)).await.body;
    assert!(form.contains("-----BEGIN CERTIFICATE-----"), "{form}");

    edit(&mut c, &csrf, p.id, &url, "   ").await;
    assert_eq!(only_printer(&app).await.ca_cert, None);
    assert!(!c.get("/ui/printers").await.body.contains(">CERT</span>"));

    let details: Vec<String> =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action LIKE 'printer_%' ORDER BY id")
            .fetch_all(&app.state.db)
            .await
            .unwrap();
    assert_eq!(details[0], "name=Voron ca=set");
    assert!(details[1].contains(" ca=cleared "), "{}", details[1]);
    assert!(details.iter().all(|d| !d.contains("BEGIN")));
}

#[tokio::test]
async fn bad_ca_input_is_rejected() {
    let (app, mut c, csrf, _mock, port) = setup().await;
    let url = format!("https://127.0.0.1:{port}");
    let key = format!("{CA}{SERVER_KEY}");
    let cases = [
        (key.as_str(), "private key"),
        (SERVER_KEY, "private key"),
        ("not a certificate", "PEM certificates"),
        (
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            "can&#39;t be read",
        ),
        (
            "-----BEGIN CERTIFICATE REQUEST-----\nAAAA\n-----END CERTIFICATE REQUEST-----\n",
            "only PEM certificates",
        ),
        (include_str!("fixtures/tls/rsa1024.pem"), "RSA key is 1024 bits"),
        (include_str!("fixtures/tls/p521.pem"), "ECDSA P-521 key"),
    ];
    for (input, msg) in cases {
        let res = add(&mut c, &csrf, &url, input).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{input}");
        assert!(res.body.contains(msg), "{input}: {}", res.body);
        assert!(!res.body.contains("PRIVATE KEY-----"), "key not echoed back");
    }
    assert!(printers::list(&app.state.db).await.unwrap().is_empty());
}
