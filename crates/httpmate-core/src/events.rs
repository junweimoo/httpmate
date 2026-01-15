//! Public DTOs: event payloads, transaction summaries/details, status types.
//! These cross the IPC boundary to the WebView, so everything is serde
//! camelCase and JSON-friendly (ids stay well under 2^53).

use serde::{Deserialize, Serialize};

/// Lifecycle state of a recorded transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TxState {
    Active,
    Completed,
    Failed,
}

/// Lightweight row for the traffic table. Carried by events and list queries;
/// bodies and full headers are fetched separately via transaction detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSummary {
    pub id: u64,
    pub started_at_ms: i64,
    /// "http" | "tunnel" | "ws-upgrade"
    pub kind: String,
    pub scheme: String,
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: Option<String>,
    pub status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub req_size: u64,
    pub resp_size: u64,
    pub content_type: Option<String>,
    pub error: Option<String>,
    pub state: TxState,
}

/// Full transaction detail for the inspector pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionDetail {
    pub summary: TransactionSummary,
    pub http_version: String,
    pub client_addr: String,
    pub tls_version: Option<String>,
    pub alpn: Option<String>,
    pub req_headers: Vec<(String, String)>,
    pub resp_headers: Vec<(String, String)>,
    pub req_body_base64: String,
    pub req_body_truncated: bool,
    pub req_body_total: u64,
    pub resp_body_base64: String,
    pub resp_body_truncated: bool,
    pub resp_body_total: u64,
    pub tags: serde_json::Value,
}

/// Current proxy service status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    pub running: bool,
    pub addr: Option<String>,
    pub port: Option<u16>,
    pub system_proxy_enabled: bool,
}

/// CA generation/trust state for the onboarding flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaState {
    pub generated: bool,
    pub cert_path: Option<String>,
    /// None = unknown (non-macOS or check failed), Some(bool) = trust status.
    pub trusted: Option<bool>,
}

/// Events pushed over the broadcast bus to all subscribers (GUI, future CLI
/// attach, future agent control surface). Summaries only — no bodies.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum ProxyEvent {
    TransactionStarted(TransactionSummary),
    TransactionUpdated(TransactionSummary),
    TransactionCompleted(TransactionSummary),
    ProxyState(ProxyStatus),
    CaState(CaState),
}

/// Filter for querying transaction history from the store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueryFilter {
    /// Substring match against host and path.
    pub search: Option<String>,
    pub host: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
    /// Return rows with id < before_id (for paging backwards).
    pub before_id: Option<u64>,
    pub limit: Option<u32>,
}
