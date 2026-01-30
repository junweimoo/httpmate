//! The proxy engine: TCP listener, CONNECT handling (MITM or opaque tunnel),
//! the request pipeline through the interceptor chain, upstream forwarding,
//! and transaction finalization.

use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http::header::{CONNECTION, CONTENT_TYPE, HOST, UPGRADE};
use http::{HeaderMap, Method, StatusCode, Uri, Version};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

use crate::ca::CertAuthority;
use crate::config::ProxySettings;
use crate::events::{ProxyEvent, TxState};
use crate::intercept::{serialize_headers, Interceptor, RequestAction, TransactionCtx, TxData};
use crate::store::{CompletedTx, StoreHandle};
use crate::tee::{CapturedBody, TeeBody};
use crate::{now_ms, BoxError, ProxyBody};

pub type UpstreamClient = Client<HttpsConnector<HttpConnector>, ProxyBody>;

#[derive(Clone, Debug, Default)]
pub struct TlsMeta {
    pub version: Option<String>,
    pub alpn: Option<String>,
}

pub struct Engine {
    pub bus: broadcast::Sender<ProxyEvent>,
    pub store: StoreHandle,
    pub ca: Arc<CertAuthority>,
    pub settings: ProxySettings,
    pub client: UpstreamClient,
    pub next_id: Arc<AtomicU64>,
    pub interceptors: Vec<Arc<dyn Interceptor>>,
    /// Hosts that failed the client-side TLS handshake (certificate pinning,
    /// most likely) and are tunneled opaquely from then on.
    pub dynamic_passthrough: Mutex<HashSet<String>>,
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Bind the listener and run the accept loop until the watch flips.
pub async fn start(
    engine: Arc<Engine>,
    bind: SocketAddr,
    stop_tx: &watch::Sender<bool>,
) -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding proxy listener on {bind}"))?;
    let local_addr = listener.local_addr()?;
    let mut stop_rx = stop_tx.subscribe();

    let handle = tokio::spawn(async move {
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let engine = engine.clone();
                        conns.spawn(async move {
                            let _ = stream.set_nodelay(true);
                            serve_conn(engine, stream, peer).await;
                        });
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                },
            }
        }
        conns.abort_all();
    });
    Ok((local_addr, handle))
}

async fn serve_conn(engine: Arc<Engine>, stream: TcpStream, peer: SocketAddr) {
    let service = service_fn(move |req: Request<Incoming>| {
        let engine = engine.clone();
        async move {
            let resp = if req.method() == Method::CONNECT {
                engine.handle_connect(req, peer).await
            } else if req.uri().scheme().is_some() {
                engine.handle(req, "http", None, peer, None).await
            } else {
                text_response(
                    StatusCode::BAD_REQUEST,
                    "httpmate is an HTTP proxy. Point your client's HTTP(S) proxy at this \
                     address instead of browsing to it directly.\n",
                )
            };
            Ok::<_, Infallible>(resp)
        }
    });
    if let Err(e) = auto::Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!("client connection ended: {e}");
    }
}

impl Engine {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn is_passthrough(&self, host: &str) -> bool {
        self.settings
            .passthrough_hosts
            .iter()
            .any(|p| pattern_matches(p, host))
            || self.dynamic_passthrough.lock().unwrap().contains(host)
    }

    /// CONNECT: either an opaque tunnel (passthrough) or TLS interception.
    async fn handle_connect(
        self: &Arc<Self>,
        req: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<ProxyBody> {
        let Some(authority) = req.uri().authority().cloned() else {
            return text_response(StatusCode::BAD_REQUEST, "CONNECT requires an authority\n");
        };
        let host = authority.host().to_string();
        let port = authority.port_u16().unwrap_or(443);

        if self.is_passthrough(&host) {
            return self.start_tunnel(req, host, port, peer).await;
        }

        let engine = self.clone();
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => engine.mitm_serve(upgraded, host, port, peer).await,
                Err(e) => tracing::debug!("CONNECT upgrade failed: {e}"),
            }
        });
        Response::new(empty_body())
    }

    /// Opaque CONNECT tunnel; recorded with byte counts only.
    async fn start_tunnel(
        self: &Arc<Self>,
        req: Request<Incoming>,
        host: String,
        port: u16,
        peer: SocketAddr,
    ) -> Response<ProxyBody> {
        let upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(s) => s,
            Err(e) => {
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("could not reach {host}:{port}: {e}\n"),
                )
            }
        };

        let ctx = TransactionCtx::new(TxData {
            id: self.next_id(),
            started_at_ms: now_ms(),
            kind: "tunnel".into(),
            scheme: "https".into(),
            method: "CONNECT".into(),
            host: display_host(&host, Some(port), "https"),
            path: String::new(),
            client_addr: peer.to_string(),
            http_version: "HTTP/1.1".into(),
            ..Default::default()
        });
        let _ = self.bus.send(ProxyEvent::TransactionStarted(ctx.summary(TxState::Active)));

        let engine = self.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let mut stop = engine.shutdown_rx.clone();
            let result = match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let mut client_io = TokioIo::new(upgraded);
                    let mut upstream = upstream;
                    tokio::select! {
                        r = tokio::io::copy_bidirectional(&mut client_io, &mut upstream) => r.map_err(|e| e.to_string()),
                        _ = stop.changed() => Ok((0, 0)),
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            let (sent, received) = match &result {
                Ok(pair) => *pair,
                Err(e) => {
                    ctx.with(|d| d.error = Some(format!("tunnel error: {e}")));
                    (0, 0)
                }
            };
            let duration = started.elapsed().as_millis() as u64;
            engine
                .finish_transaction(&ctx, Some(duration), sent, received, CapturedBody::default(), CapturedBody::default())
                .await;
        });
        Response::new(empty_body())
    }

    /// TLS-terminate the CONNECT stream with a minted cert and serve the
    /// decrypted requests through the normal pipeline.
    async fn mitm_serve(
        self: &Arc<Self>,
        upgraded: hyper::upgrade::Upgraded,
        host: String,
        port: u16,
        peer: SocketAddr,
    ) {
        let server_config = match self.mitm_server_config(&host) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("could not build MITM TLS config for {host}: {e:#}");
                return;
            }
        };
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let tls = match acceptor.accept(TokioIo::new(upgraded)).await {
            Ok(tls) => tls,
            Err(e) => {
                // Client refused our certificate — likely pinning. Tunnel
                // this host from now on and surface what happened.
                self.dynamic_passthrough.lock().unwrap().insert(host.clone());
                self.record_handshake_failure(&host, port, peer, &e.to_string()).await;
                return;
            }
        };

        let tls_meta = {
            let (_, conn) = tls.get_ref();
            TlsMeta {
                version: conn.protocol_version().map(|v| format!("{v:?}").replace('_', ".")),
                alpn: conn
                    .alpn_protocol()
                    .map(|p| String::from_utf8_lossy(p).into_owned()),
            }
        };

        let authority = if port == 443 { host.clone() } else { format!("{host}:{port}") };
        let engine = self.clone();
        let service = service_fn(move |req: Request<Incoming>| {
            let engine = engine.clone();
            let authority = authority.clone();
            let tls_meta = tls_meta.clone();
            async move {
                Ok::<_, Infallible>(
                    engine.handle(req, "https", Some(authority), peer, Some(tls_meta)).await,
                )
            }
        });

        let mut stop = self.shutdown_rx.clone();
        let builder = auto::Builder::new(TokioExecutor::new());
        let serve = builder.serve_connection_with_upgrades(TokioIo::new(tls), service);
        tokio::select! {
            r = serve => {
                if let Err(e) = r {
                    tracing::debug!("MITM connection for {host} ended: {e}");
                }
            }
            _ = stop.changed() => {}
        }
    }

    fn mitm_server_config(&self, host: &str) -> Result<ServerConfig> {
        let resolver = MintResolver { ca: self.ca.clone(), fallback: host.to_string() };
        let mut config = ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(config)
    }

    async fn record_handshake_failure(&self, host: &str, port: u16, peer: SocketAddr, err: &str) {
        let ctx = TransactionCtx::new(TxData {
            id: self.next_id(),
            started_at_ms: now_ms(),
            kind: "tunnel".into(),
            scheme: "https".into(),
            method: "CONNECT".into(),
            host: display_host(host, Some(port), "https"),
            path: String::new(),
            client_addr: peer.to_string(),
            http_version: "HTTP/1.1".into(),
            error: Some(format!(
                "client rejected the intercept certificate for {host}:{port} ({err}); \
                 the host may use certificate pinning — it will be tunneled opaquely from now on"
            )),
            ..Default::default()
        });
        let _ = self.bus.send(ProxyEvent::TransactionStarted(ctx.summary(TxState::Active)));
        self.finish_transaction(&ctx, Some(0), 0, 0, CapturedBody::default(), CapturedBody::default())
            .await;
    }

    /// The main pipeline: record → (future: rewrite/mock) → forward.
    async fn handle(
        self: &Arc<Self>,
        mut req: Request<Incoming>,
        scheme: &'static str,
        connect_authority: Option<String>,
        peer: SocketAddr,
        tls: Option<TlsMeta>,
    ) -> Response<ProxyBody> {
        let uri = match absolute_uri(&req, scheme, connect_authority.as_deref()) {
            Ok(u) => u,
            Err(e) => return text_response(StatusCode::BAD_REQUEST, &format!("{e:#}\n")),
        };

        let ctx = TransactionCtx::new(TxData {
            id: self.next_id(),
            started_at_ms: now_ms(),
            kind: "http".into(),
            scheme: scheme.into(),
            method: req.method().to_string(),
            host: display_host(uri.host().unwrap_or_default(), uri.port_u16(), scheme),
            path: uri.path().to_string(),
            query: uri.query().map(str::to_string),
            http_version: format!("{:?}", req.version()),
            client_addr: peer.to_string(),
            tls_version: tls.as_ref().and_then(|t| t.version.clone()),
            alpn: tls.as_ref().and_then(|t| t.alpn.clone()),
            req_header_blob: serialize_headers(req.headers()),
            ..Default::default()
        });
        let started = Instant::now();

        // Take the server-side upgrade handle before the request is consumed,
        // so 101 responses (websockets) can be bridged.
        let wants_upgrade = req.headers().contains_key(UPGRADE);
        let server_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut req));

        // Tee the request body: stream through, capture up to the cap.
        let limit = self.settings.body_capture_limit;
        let (parts, body) = req.into_parts();
        let (tee, rx_req) = TeeBody::new(body, limit);
        let mut action =
            RequestAction::Continue(Request::from_parts(parts, boxed(tee)));

        for interceptor in &self.interceptors {
            match action {
                RequestAction::Continue(r) => action = interceptor.on_request(&ctx, r).await,
                RequestAction::Respond(_) => break,
            }
        }

        let mut resp: Response<ProxyBody> = match action {
            RequestAction::Respond(r) => r,
            RequestAction::Continue(req) => {
                self.forward(&ctx, req, uri, wants_upgrade, server_upgrade).await
            }
        };

        for interceptor in self.interceptors.iter().rev() {
            resp = interceptor.on_response(&ctx, resp).await;
        }

        // Tee the response body and finalize once both sides finish.
        let (parts, body) = resp.into_parts();
        let (tee, rx_resp) = TeeBody::new(body, limit);
        let resp = Response::from_parts(parts, boxed(tee));

        let engine = self.clone();
        tokio::spawn(async move {
            let req_cap = rx_req.await.unwrap_or_default();
            let resp_cap = rx_resp.await.unwrap_or_default();
            let duration = started.elapsed().as_millis() as u64;
            let req_size = ctx.with(|d| d.req_header_blob.len()) as u64 + req_cap.total;
            let resp_size = ctx.with(|d| d.resp_header_blob.len()) as u64 + resp_cap.total;
            engine
                .finish_transaction(&ctx, Some(duration), req_size, resp_size, req_cap, resp_cap)
                .await;
        });

        resp
    }

    /// Terminal step of the chain: send upstream, bridge upgrades.
    async fn forward(
        self: &Arc<Self>,
        ctx: &TransactionCtx,
        req: Request<ProxyBody>,
        uri: Uri,
        wants_upgrade: bool,
        server_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Response<ProxyBody> {
        let (mut parts, body) = req.into_parts();
        parts.uri = uri;
        // Let hyper pick the wire version per connection (ALPN upstream).
        parts.version = Version::HTTP_11;
        strip_hop_headers(&mut parts.headers, wants_upgrade);
        parts.headers.remove(HOST);
        let upstream_req = Request::from_parts(parts, body);

        let mut upstream_resp = match self.client.request(upstream_req).await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("upstream request failed: {e}");
                ctx.with(|d| d.error = Some(msg.clone()));
                return text_response(StatusCode::BAD_GATEWAY, &format!("{msg}\n"));
            }
        };

        if upstream_resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            ctx.with(|d| d.kind = "ws-upgrade".into());
            let client_upgrade = hyper::upgrade::on(&mut upstream_resp);
            if let Some(server_upgrade) = server_upgrade {
                let host = ctx.with(|d| d.host.clone());
                tokio::spawn(async move {
                    match tokio::try_join!(server_upgrade, client_upgrade) {
                        Ok((client_side, upstream_side)) => {
                            let mut a = TokioIo::new(client_side);
                            let mut b = TokioIo::new(upstream_side);
                            if let Err(e) = tokio::io::copy_bidirectional(&mut a, &mut b).await {
                                tracing::debug!("upgraded stream for {host} ended: {e}");
                            }
                        }
                        Err(e) => tracing::debug!("upgrade bridge for {host} failed: {e}"),
                    }
                });
            }
            // Keep Connection/Upgrade headers intact on a 101.
            return upstream_resp.map(boxed_incoming);
        }

        let (mut parts, body) = upstream_resp.into_parts();
        strip_hop_headers(&mut parts.headers, false);
        Response::from_parts(parts, boxed_incoming(body))
    }

    /// Write the completed transaction and emit the final event. Shared by
    /// the HTTP pipeline, tunnels, and handshake-failure records.
    async fn finish_transaction(
        &self,
        ctx: &TransactionCtx,
        duration_ms: Option<u64>,
        req_size: u64,
        resp_size: u64,
        req_cap: CapturedBody,
        resp_cap: CapturedBody,
    ) {
        let state =
            if ctx.with(|d| d.error.is_some()) { TxState::Failed } else { TxState::Completed };
        let summary = ctx.summary_sized(state, duration_ms, req_size, resp_size);
        let completed = ctx.with(|d| CompletedTx {
            summary: summary.clone(),
            http_version: d.http_version.clone(),
            client_addr: d.client_addr.clone(),
            tls_version: d.tls_version.clone(),
            alpn: d.alpn.clone(),
            req_header_blob: std::mem::take(&mut d.req_header_blob),
            resp_header_blob: std::mem::take(&mut d.resp_header_blob),
            req_body_total: req_cap.total,
            req_body_truncated: req_cap.truncated,
            req_body: req_cap.bytes,
            resp_body_total: resp_cap.total,
            resp_body_truncated: resp_cap.truncated,
            resp_body: resp_cap.bytes,
            tags: serde_json::Value::Object(d.tags.clone()),
        });
        if let Err(e) = self.store.insert(completed).await {
            tracing::warn!("failed to persist transaction {}: {e:#}", summary.id);
        }
        let _ = self.bus.send(ProxyEvent::TransactionCompleted(summary));
    }
}

/// Build the pooled upstream client (rustls, webpki roots + configured
/// extras, ALPN h2/http1).
pub fn build_upstream_client(settings: &ProxySettings) -> Result<UpstreamClient> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for pem in &settings.extra_root_certs_pem {
        for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
            roots
                .add(cert.context("parsing extra root certificate")?)
                .context("adding extra root certificate")?;
        }
    }
    let tls = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_all_versions()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// Mints a leaf for the SNI (or the CONNECT host when SNI is absent).
struct MintResolver {
    ca: Arc<CertAuthority>,
    fallback: String,
}

impl fmt::Debug for MintResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintResolver").field("fallback", &self.fallback).finish()
    }
}

impl ResolvesServerCert for MintResolver {
    fn resolve(&self, client_hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let name = client_hello
            .server_name()
            .map(str::to_string)
            .unwrap_or_else(|| self.fallback.clone());
        match self.ca.mint(&name) {
            Ok(ck) => Some(ck),
            Err(e) => {
                tracing::warn!("failed to mint certificate for {name}: {e:#}");
                None
            }
        }
    }
}

fn absolute_uri(
    req: &Request<Incoming>,
    scheme: &str,
    connect_authority: Option<&str>,
) -> Result<Uri> {
    if req.uri().scheme().is_some() {
        return Ok(req.uri().clone());
    }
    let authority = req
        .uri()
        .authority()
        .map(|a| a.as_str())
        .or(connect_authority)
        .context("request has no authority and no CONNECT context")?;
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    format!("{scheme}://{authority}{path_and_query}")
        .parse::<Uri>()
        .context("assembling absolute request URI")
}

/// Remove hop-by-hop headers (RFC 9110 §7.6.1), including any named by the
/// Connection header. With `preserve_upgrade`, Connection/Upgrade survive so
/// 101 handshakes can pass through.
fn strip_hop_headers(headers: &mut HeaderMap, preserve_upgrade: bool) {
    let connection_named: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    for name in connection_named {
        if !(preserve_upgrade && name == "upgrade") {
            headers.remove(name.as_str());
        }
    }
    for name in
        ["proxy-connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding"]
    {
        headers.remove(name);
    }
    if !preserve_upgrade {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    }
}

/// Host as users expect to see it: port included unless it's the scheme
/// default. Keeps displayed URLs (and copy-as-cURL) correct.
fn display_host(host: &str, port: Option<u16>, scheme: &str) -> String {
    match (scheme, port) {
        (_, None) | ("http", Some(80)) | ("https", Some(443)) => host.to_string(),
        (_, Some(p)) => format!("{host}:{p}"),
    }
}

fn pattern_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.len() > suffix.len() + 1 && host.to_ascii_lowercase().ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
    } else {
        pattern.eq_ignore_ascii_case(host)
    }
}

fn boxed<B>(body: B) -> ProxyBody
where
    B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxError>,
{
    body.map_err(Into::into).boxed()
}

fn boxed_incoming(body: Incoming) -> ProxyBody {
    body.map_err(|e| Box::new(e) as BoxError).boxed()
}

pub fn empty_body() -> ProxyBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

pub fn full_body(data: impl Into<Bytes>) -> ProxyBody {
    Full::new(data.into()).map_err(|never| match never {}).boxed()
}

fn text_response(status: StatusCode, body: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full_body(body.to_string()))
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_patterns() {
        assert!(pattern_matches("example.com", "EXAMPLE.com"));
        assert!(!pattern_matches("example.com", "sub.example.com"));
        assert!(pattern_matches("*.example.com", "sub.example.com"));
        assert!(pattern_matches("*.example.com", "a.b.example.com"));
        assert!(!pattern_matches("*.example.com", "example.com"));
        assert!(!pattern_matches("*.example.com", "notexample.com"));
    }

    #[test]
    fn hop_headers_stripped() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, "keep-alive, x-custom-hop".parse().unwrap());
        h.insert("x-custom-hop", "1".parse().unwrap());
        h.insert("proxy-connection", "keep-alive".parse().unwrap());
        h.insert("te", "trailers".parse().unwrap());
        h.insert("accept", "*/*".parse().unwrap());
        strip_hop_headers(&mut h, false);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("accept"));
    }

    #[test]
    fn upgrade_headers_survive_when_preserved() {
        let mut h = HeaderMap::new();
        h.insert(CONNECTION, "Upgrade".parse().unwrap());
        h.insert(UPGRADE, "websocket".parse().unwrap());
        h.insert("sec-websocket-key", "abc".parse().unwrap());
        strip_hop_headers(&mut h, true);
        assert!(h.contains_key(CONNECTION));
        assert!(h.contains_key(UPGRADE));
        assert!(h.contains_key("sec-websocket-key"));
    }
}
