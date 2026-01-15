use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where the app keeps its state (database, CA cert, settings, blobs).
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
}

/// User-tunable proxy settings, persisted as settings.json in the data dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxySettings {
    /// Port the proxy listens on. 0 picks an ephemeral port (used in tests).
    pub port: u16,
    /// Bind address. Localhost by default; LAN exposure is an explicit opt-in.
    pub bind_addr: String,
    /// Max bytes of each request/response body captured for inspection.
    /// Bodies stream through unmodified regardless; beyond this cap the
    /// recording is marked truncated.
    pub body_capture_limit: usize,
    /// Hosts that are tunneled opaquely instead of MITM'd. Patterns are an
    /// exact host or a `*.suffix` wildcard.
    pub passthrough_hosts: Vec<String>,
    /// Extra PEM root certificates trusted for upstream TLS (e.g. a local
    /// dev server's self-signed cert, or a corporate root).
    pub extra_root_certs_pem: Vec<String>,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            port: 8888,
            bind_addr: "127.0.0.1".into(),
            body_capture_limit: 10 * 1024 * 1024,
            passthrough_hosts: Vec::new(),
            extra_root_certs_pem: Vec::new(),
        }
    }
}

impl ProxySettings {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("settings.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("invalid settings.json ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join("settings.json");
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
