//! End-to-end tests: a real origin server, a real Controller-managed proxy,
//! and a raw client speaking proxy protocol (absolute-form HTTP and
//! CONNECT + TLS for MITM).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use httpmate_core::events::{ProxyEvent, TransactionSummary, TxState};
use httpmate_core::{AppConfig, Controller, ProxySettings};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

const TIMEOUT: Duration = Duration::from_secs(15);

async fn wait_for_completed(
    rx: &mut broadcast::Receiver<ProxyEvent>,
    pred: impl Fn(&TransactionSummary) -> bool,
) -> TransactionSummary {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let ProxyEvent::TransactionCompleted(s) = rx.recv().await.expect("bus closed") {
                if pred(&s) {
                    return s;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for TransactionCompleted")
}

async fn origin_service(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path().to_string();
    let req_body = req.into_body().collect().await?.to_bytes();
    let body = format!("origin says hello from {path}; you sent {}", req_body.len());
    Ok(Response::builder()
        .header("content-type", "text/plain")
        .header("x-origin", "httpmate-test")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// Plain-HTTP origin; serves connections until dropped.
async fn spawn_http_origin() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(origin_service))
                    .await;
            });
        }
    });
    addr
}

/// TLS origin with a self-signed cert for "localhost". Returns (addr, cert PEM).
async fn spawn_tls_origin() -> (SocketAddr, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_pem = cert.pem();

    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service_fn(origin_service))
                        .await;
                }
            });
        }
    });
    (addr, cert_pem)
}

async fn start_proxy(settings: ProxySettings) -> (Controller, u16, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let controller = Controller::new(AppConfig { data_dir: dir.path().to_path_buf() }).unwrap();
    controller.set_settings(settings).unwrap();
    let status = controller.start(Some(0)).await.unwrap();
    (controller, status.port.unwrap(), dir)
}

#[tokio::test]
async fn plain_http_request_is_proxied_and_recorded() {
    let origin = spawn_http_origin().await;
    let (controller, proxy_port, _dir) = start_proxy(ProxySettings::default()).await;
    let mut events = controller.subscribe();

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let request = format!(
        "POST http://{origin}/things HTTP/1.1\r\nHost: {origin}\r\ncontent-length: 4\r\nx-test-marker: abc\r\nConnection: close\r\n\r\nping"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(TIMEOUT, stream.read_to_end(&mut raw)).await.unwrap().unwrap();
    let raw = String::from_utf8_lossy(&raw);

    assert!(raw.starts_with("HTTP/1.1 200"), "unexpected response: {raw}");
    assert!(raw.contains("origin says hello from /things; you sent 4"), "{raw}");
    assert!(raw.contains("x-origin: httpmate-test"), "origin headers should pass through: {raw}");

    let summary = wait_for_completed(&mut events, |s| s.path == "/things").await;
    assert_eq!(summary.method, "POST");
    assert_eq!(summary.scheme, "http");
    assert_eq!(summary.host, origin.to_string(), "host should include the non-default port");
    assert_eq!(summary.status, Some(200));
    assert_eq!(summary.state, TxState::Completed);
    assert!(summary.duration_ms.is_some());

    // Detail comes from the store with full headers and both bodies.
    let detail = controller.get_transaction(summary.id).await.unwrap().unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    assert_eq!(b64.decode(&detail.req_body_base64).unwrap(), b"ping");
    let resp_body = b64.decode(&detail.resp_body_base64).unwrap();
    assert!(String::from_utf8_lossy(&resp_body).contains("you sent 4"));
    assert!(detail.req_headers.iter().any(|(n, v)| n == "x-test-marker" && v == "abc"));
    assert!(detail.resp_headers.iter().any(|(n, _)| n == "x-origin"));

    controller.stop().await.unwrap();
}

#[tokio::test]
async fn https_is_intercepted_via_mitm() {
    let (origin, origin_cert_pem) = spawn_tls_origin().await;
    let settings = ProxySettings {
        extra_root_certs_pem: vec![origin_cert_pem],
        ..Default::default()
    };
    let (controller, proxy_port, _dir) = start_proxy(settings).await;
    let mut events = controller.subscribe();

    // The client trusts the httpmate CA, exactly like a real app would after
    // installing it.
    let (ca_pem, _path) = controller.export_ca().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();

    // CONNECT through the proxy, then TLS-handshake against the MITM.
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let connect = format!(
        "CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n",
        port = origin.port()
    );
    stream.write_all(connect.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(TIMEOUT, stream.read(&mut buf)).await.unwrap().unwrap();
    let connect_resp = String::from_utf8_lossy(&buf[..n]);
    assert!(connect_resp.starts_with("HTTP/1.1 200"), "CONNECT failed: {connect_resp}");

    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = tokio::time::timeout(TIMEOUT, connector.connect(server_name, stream))
        .await
        .unwrap()
        .expect("TLS handshake with the intercepting proxy failed");

    let (mut sender, conn) =
        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(tls)).await.unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri("/secret")
        .header("host", format!("localhost:{}", origin.port()))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = tokio::time::timeout(TIMEOUT, sender.send_request(req)).await.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("origin says hello from /secret"),
        "unexpected body through MITM"
    );

    // The decrypted exchange is recorded like any plain transaction.
    let summary = wait_for_completed(&mut events, |s| s.path == "/secret").await;
    assert_eq!(summary.scheme, "https");
    assert_eq!(summary.host, format!("localhost:{}", origin.port()));
    assert_eq!(summary.status, Some(200));
    assert_eq!(summary.state, TxState::Completed);

    let detail = controller.get_transaction(summary.id).await.unwrap().unwrap();
    assert_eq!(detail.tls_version.as_deref(), Some("TLSv1.3"));
    let b64 = base64::engine::general_purpose::STANDARD;
    let resp_body = b64.decode(&detail.resp_body_base64).unwrap();
    assert!(String::from_utf8_lossy(&resp_body).contains("/secret"));

    controller.stop().await.unwrap();
}

#[tokio::test]
async fn passthrough_hosts_are_tunneled_not_intercepted() {
    let (origin, origin_cert_pem) = spawn_tls_origin().await;
    let settings = ProxySettings {
        passthrough_hosts: vec!["localhost".to_string()],
        extra_root_certs_pem: vec![origin_cert_pem.clone()],
        ..Default::default()
    };
    let (controller, proxy_port, _dir) = start_proxy(settings).await;
    let mut events = controller.subscribe();

    // Client trusts only the ORIGIN's own cert — if the proxy tried to MITM,
    // this handshake would fail.
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut origin_cert_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();

    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let connect = format!(
        "CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n",
        port = origin.port()
    );
    stream.write_all(connect.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));

    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = connector
        .connect(server_name, stream)
        .await
        .expect("end-to-end TLS through the tunnel should succeed");

    let (mut sender, conn) =
        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(tls)).await.unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .uri("/tunneled")
        .header("host", format!("localhost:{}", origin.port()))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    drop(sender);

    // Recorded as an opaque tunnel: host and byte counts, no decrypted paths.
    let summary = wait_for_completed(&mut events, |s| s.kind == "tunnel").await;
    assert_eq!(summary.method, "CONNECT");
    assert_eq!(summary.host, format!("localhost:{}", origin.port()));

    controller.stop().await.unwrap();
}
