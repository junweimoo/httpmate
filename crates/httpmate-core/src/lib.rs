//! httpmate-core: proxy engine, interception, storage and CA for httpmate.
//!
//! Everything that captures, stores, or transforms traffic lives here and is
//! callable without a GUI. The Tauri shell (and future CLI / agent control
//! surfaces) are thin adapters over [`Controller`].

pub mod ca;
pub mod config;
pub mod events;
pub mod store;

pub use config::{AppConfig, ProxySettings};
pub use events::*;

/// Boxed error type used across body plumbing.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The unified body type flowing through the interceptor chain.
pub type ProxyBody = http_body_util::combinators::BoxBody<bytes::Bytes, BoxError>;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as i64
}
