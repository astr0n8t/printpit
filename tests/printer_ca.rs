//! Per-printer custom CA: form validation, storage, and that https:// printers are trusted only
//! through their own CA. The mock Moonraker is put behind a TLS front whose certificate is signed
//! by `tests/fixtures/tls/ca.pem` (SANs 127.0.0.1 and localhost, valid for 100 years).

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use common::moonraker::MockMoonraker;
use common::{Client, TestApp};
use printpit::db::printers;

const CA: &str = include_str!("fixtures/tls/ca.pem");
const OTHER_CA: &str = include_str!("fixtures/tls/other_ca.pem");
const SERVER_CERT: &str = include_str!("fixtures/tls/server.pem");
const SERVER_KEY: &str = include_str!("fixtures/tls/server.key");

const LOCAL: &[(&str, &str)] = &[
    ("PRINTPIT_SECURE_COOKIES", "false"),
    ("PRINTPIT_PRINTER_CIDRS", "127.0.0.0/8"),
];

/// Terminates TLS on a free port and forwards the plaintext to `backend`. Returns the port.
async fn tls_front(backend: u16) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = CertificateDer::pem_slice_iter(SERVER_CERT.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(SERVER_KEY.as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
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

async fn setup() -> (TestApp, Client, String, MockMoonraker, u16) {
    let (app, mut c) = TestApp::with_admin_env(LOCAL).await;
    let csrf = c.session_csrf().await;
    let mock = MockMoonraker::start().await;
    let port = tls_front(mock.port).await;
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
async fn ca_is_stored_shown_and_cleared() {
    let (app, mut c, csrf, _mock, port) = setup().await;
    let url = format!("https://127.0.0.1:{port}");
    // Windows line endings and surrounding blank lines are tidied up.
    let pasted = format!("\r\n{}\r\n", CA.replace('\n', "\r\n"));
    add(&mut c, &csrf, &url, &pasted).await;
    let p = only_printer(&app).await;
    assert_eq!(p.ca_cert.as_deref(), Some(CA));

    let list = c.get("/ui/printers").await.body;
    assert!(list.contains(">CA</span>"), "{list}");
    let form = c.get(&format!("/ui/printers/{}", p.id)).await.body;
    assert!(form.contains("-----BEGIN CERTIFICATE-----"), "{form}");

    edit(&mut c, &csrf, p.id, &url, "   ").await;
    assert_eq!(only_printer(&app).await.ca_cert, None);
    assert!(!c.get("/ui/printers").await.body.contains(">CA</span>"));

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
    ];
    for (input, msg) in cases {
        let res = add(&mut c, &csrf, &url, input).await;
        assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY, "{input}");
        assert!(res.body.contains(msg), "{input}: {}", res.body);
        assert!(!res.body.contains("PRIVATE KEY-----"), "key not echoed back");
    }
    assert!(printers::list(&app.state.db).await.unwrap().is_empty());
}
