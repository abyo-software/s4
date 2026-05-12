//! End-to-end TLS termination test for issue #2.
//!
//! Validates the full path: rcgen self-signed cert -> tokio-rustls TlsAcceptor
//! -> hyper-util auto Builder -> minimal hyper service -> reqwest rustls
//! client (danger_accept_invalid_certs) -> HTTPS handshake + HTTP/2
//! negotiation + body roundtrip.
//!
//! This test exercises the same wiring main.rs uses for the binary, but
//! against an in-process minimal service rather than the full S4 stack —
//! enough to prove the TLS termination path works end-to-end without
//! needing the docker-compose stack.

use std::convert::Infallible;
use std::io::Write;

use http_body_util::Full;
use hyper::body::Bytes as HyperBytes;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use s4_server::tls::{install_default_crypto_provider, load_tls_config};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;

fn write_self_signed_pair() -> (NamedTempFile, NamedTempFile) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file.write_all(cert.cert.pem().as_bytes()).unwrap();
    cert_file.flush().unwrap();
    let mut key_file = NamedTempFile::new().unwrap();
    key_file
        .write_all(cert.key_pair.serialize_pem().as_bytes())
        .unwrap();
    key_file.flush().unwrap();
    (cert_file, key_file)
}

async fn echo_service(
    req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<HyperBytes>>, Infallible> {
    let body = format!(
        "echo: {} {} version={:?}",
        req.method(),
        req.uri().path(),
        req.version()
    );
    Ok(Response::new(Full::new(HyperBytes::from(body))))
}

#[tokio::test]
async fn tls_handshake_and_https_roundtrip() {
    install_default_crypto_provider();
    let (cert, key) = write_self_signed_pair();
    let tls_cfg = load_tls_config(cert.path(), key.path()).expect("tls config");
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_cfg);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let url = format!("https://{}/probe", bound);

    let server = ConnBuilder::new(TokioExecutor::new());

    let server_handle = tokio::spawn(async move {
        // Accept exactly one connection for the test.
        let (sock, _) = listener.accept().await.unwrap();
        let tls_stream = acceptor.accept(sock).await.expect("tls handshake");
        let svc = hyper::service::service_fn(echo_service);
        let _ = server.serve_connection(TokioIo::new(tls_stream), svc).await;
    });

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http2_prior_knowledge() // ALPN should already pick h2; this asserts h2 worked
        .build()
        .unwrap();
    let resp = client.get(&url).send().await.expect("https GET");
    assert!(resp.status().is_success(), "status={}", resp.status());
    assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    let body = resp.text().await.unwrap();
    assert!(body.starts_with("echo: GET /probe"), "body={body:?}");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;
}

#[tokio::test]
async fn tls_negotiates_http_1_when_client_requests_it() {
    install_default_crypto_provider();
    let (cert, key) = write_self_signed_pair();
    let tls_cfg = load_tls_config(cert.path(), key.path()).expect("tls config");
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_cfg);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    let url = format!("https://{}/probe", bound);

    let server = ConnBuilder::new(TokioExecutor::new());

    let server_handle = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let tls_stream = acceptor.accept(sock).await.expect("tls handshake");
        let svc = hyper::service::service_fn(echo_service);
        let _ = server.serve_connection(TokioIo::new(tls_stream), svc).await;
    });

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();
    let resp = client.get(&url).send().await.expect("https GET");
    assert!(resp.status().is_success());
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    let body = resp.text().await.unwrap();
    assert!(body.starts_with("echo: GET /probe"), "body={body:?}");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle).await;
}
