//! mTLS delivery against a local TLS server that requires a client certificate.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use txwatch_config::WebhookDestination;
use txwatch_notifier::{build_client, send_to_destination_simple, test_payload};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mtls");

fn fixture(name: &str) -> String {
    format!("{}/{}", FIXTURES, name)
}

/// Starts a TLS server that requires a client certificate signed by the test
/// CA, answers every request with 200 and reports each request it received.
async fn start_mtls_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(fixture("ca.pem")).unwrap())
        .unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .unwrap();
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from_pem_file(fixture("server.pem")).unwrap()],
            PrivateKeyDer::from_pem_file(fixture("server.key")).unwrap(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = tls.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, rx)
}

fn destination(port: u16) -> WebhookDestination {
    WebhookDestination {
        url: format!("https://localhost:{}/hook", port),
        ca_cert: Some(fixture("ca.pem")),
        ..Default::default()
    }
}

#[tokio::test]
async fn delivers_with_client_certificate() {
    let (port, mut requests) = start_mtls_server().await;
    let mut dest = destination(port);
    dest.client_cert = Some(fixture("client.pem"));
    dest.client_key = Some(fixture("client.key"));

    let result = send_to_destination_simple(&build_client().unwrap(), &dest, &test_payload("mtls"))
        .await
        .unwrap();
    assert_eq!(result.final_status, 200);
    assert!(requests.recv().await.unwrap().starts_with("POST /hook "));
}

#[tokio::test]
async fn server_rejects_delivery_without_client_certificate() {
    let (port, mut requests) = start_mtls_server().await;

    let result = send_to_destination_simple(
        &build_client().unwrap(),
        &destination(port),
        &test_payload("mtls"),
    )
    .await;
    assert!(result.is_err());
    assert!(
        requests.try_recv().is_err(),
        "no request should be accepted"
    );
}
