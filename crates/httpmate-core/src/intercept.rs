//! The interceptor chain — the extensibility seam every transaction passes
//! through. v1 ships a single `Recorder` interceptor; the future rewrite
//! engine, mock responder and agent breakpoints plug in as additional
//! implementations without engine changes.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hyper::{Request, Response};
use tokio::sync::broadcast;

use crate::events::{ProxyEvent, TransactionSummary, TxState};
use crate::ProxyBody;

/// Mutable per-transaction state shared between the engine, interceptors and
/// the finalize task.
#[derive(Debug, Default)]
pub struct TxData {
    pub id: u64,
    pub started_at_ms: i64,
    /// "http" | "tunnel" | "ws-upgrade"
    pub kind: String,
    pub scheme: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: Option<String>,
    pub http_version: String,
    pub client_addr: String,
    pub tls_version: Option<String>,
    pub alpn: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub error: Option<String>,
    pub req_header_blob: Vec<u8>,
    pub resp_header_blob: Vec<u8>,
    /// Interceptor annotations (e.g. which rewrite rule fired), surfaced in
    /// the UI and stored alongside the transaction.
    pub tags: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone)]
pub struct TransactionCtx {
    data: Arc<Mutex<TxData>>,
}

impl TransactionCtx {
    pub fn new(data: TxData) -> Self {
        Self { data: Arc::new(Mutex::new(data)) }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut TxData) -> R) -> R {
        f(&mut self.data.lock().unwrap())
    }

    pub fn summary(&self, state: TxState) -> TransactionSummary {
        self.summary_sized(state, None, 0, 0)
    }

    pub fn summary_sized(
        &self,
        state: TxState,
        duration_ms: Option<u64>,
        req_size: u64,
        resp_size: u64,
    ) -> TransactionSummary {
        let d = self.data.lock().unwrap();
        TransactionSummary {
            id: d.id,
            started_at_ms: d.started_at_ms,
            kind: d.kind.clone(),
            scheme: d.scheme.clone(),
            method: d.method.clone(),
            host: d.host.clone(),
            path: d.path.clone(),
            query: d.query.clone(),
            status: d.status,
            duration_ms,
            req_size,
            resp_size,
            content_type: d.content_type.clone(),
            error: d.error.clone(),
            state,
        }
    }
}

/// What an interceptor decides to do with a request.
pub enum RequestAction {
    /// Pass the (possibly modified) request down the chain.
    Continue(Request<ProxyBody>),
    /// Short-circuit: answer without contacting upstream (mocks, blocks).
    Respond(Response<ProxyBody>),
}

#[async_trait]
pub trait Interceptor: Send + Sync {
    async fn on_request(&self, ctx: &TransactionCtx, req: Request<ProxyBody>) -> RequestAction;
    async fn on_response(
        &self,
        ctx: &TransactionCtx,
        resp: Response<ProxyBody>,
    ) -> Response<ProxyBody>;
}

/// v1's only interceptor: observes everything, modifies nothing, and emits
/// live events for the UI. Captured bodies and the final store write are
/// handled by the engine's finalize task, which knows when bodies complete.
pub struct Recorder {
    pub bus: broadcast::Sender<ProxyEvent>,
}

#[async_trait]
impl Interceptor for Recorder {
    async fn on_request(&self, ctx: &TransactionCtx, req: Request<ProxyBody>) -> RequestAction {
        let _ = self.bus.send(ProxyEvent::TransactionStarted(ctx.summary(TxState::Active)));
        RequestAction::Continue(req)
    }

    async fn on_response(
        &self,
        ctx: &TransactionCtx,
        resp: Response<ProxyBody>,
    ) -> Response<ProxyBody> {
        ctx.with(|d| {
            d.status = Some(resp.status().as_u16());
            d.content_type = resp
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            d.resp_header_blob = serialize_headers(resp.headers());
        });
        let _ = self.bus.send(ProxyEvent::TransactionUpdated(ctx.summary(TxState::Active)));
        resp
    }
}

/// Serialize headers as order-preserving `name: value` lines. HeaderValues
/// cannot contain newlines, so the format is unambiguous.
pub fn serialize_headers(headers: &http::HeaderMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    for (name, value) in headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out
}
