//! The imperative API surface of the core. The Tauri shell maps its commands
//! onto this 1:1; a future CLI or agent control socket does the same.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::ca::CertAuthority;
use crate::config::{AppConfig, ProxySettings};
use crate::events::{CaState, ProxyEvent, ProxyStatus, QueryFilter, TransactionDetail, TransactionSummary};
use crate::intercept::{Interceptor, Recorder};
use crate::proxy::{self, Engine};
use crate::store::StoreHandle;
use crate::{macos, now_ms};

const EVENT_BUS_CAPACITY: usize = 4096;

struct Running {
    local_addr: SocketAddr,
    stop_tx: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

struct Inner {
    data_dir: PathBuf,
    bus: broadcast::Sender<ProxyEvent>,
    store: StoreHandle,
    ca: Arc<CertAuthority>,
    settings: Mutex<ProxySettings>,
    next_id: Arc<AtomicU64>,
    running: tokio::sync::Mutex<Option<Running>>,
    system_proxy_enabled: AtomicBool,
}

#[derive(Clone)]
pub struct Controller {
    inner: Arc<Inner>,
}

impl Controller {
    pub fn new(config: AppConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("creating data dir {}", config.data_dir.display()))?;

        // A previous run may have crashed with the system proxy still set.
        match macos::restore_if_dangling(&config.data_dir) {
            Ok(true) => tracing::warn!("restored system proxy settings left by a previous run"),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not restore dangling system proxy settings: {e:#}"),
        }

        let store = StoreHandle::open(&config.data_dir)?;
        let next_id = Arc::new(AtomicU64::new(store.max_id_at_open + 1));
        let ca = Arc::new(CertAuthority::new(
            &config.data_dir,
            macos::default_secret_store(&config.data_dir),
        ));
        let settings = ProxySettings::load(&config.data_dir);
        let (bus, _) = broadcast::channel(EVENT_BUS_CAPACITY);

        Ok(Self {
            inner: Arc::new(Inner {
                data_dir: config.data_dir,
                bus,
                store,
                ca,
                settings: Mutex::new(settings),
                next_id,
                running: tokio::sync::Mutex::new(None),
                system_proxy_enabled: AtomicBool::new(false),
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProxyEvent> {
        self.inner.bus.subscribe()
    }

    /// Start the proxy. `port_override` (used by the UI's port field and by
    /// tests with port 0) takes precedence over persisted settings for this
    /// run only.
    pub async fn start(&self, port_override: Option<u16>) -> Result<ProxyStatus> {
        let mut running = self.inner.running.lock().await;
        if running.is_some() {
            return Ok(self.status_locked(&running));
        }

        let mut settings = self.inner.settings.lock().unwrap().clone();
        if let Some(port) = port_override {
            settings.port = port;
        }

        self.inner.ca.ensure_ca()?;
        let client = proxy::build_upstream_client(&settings)?;
        let (stop_tx, stop_rx) = watch::channel(false);
        let interceptors: Vec<Arc<dyn Interceptor>> =
            vec![Arc::new(Recorder { bus: self.inner.bus.clone() })];
        let engine = Arc::new(Engine {
            bus: self.inner.bus.clone(),
            store: self.inner.store.clone(),
            ca: self.inner.ca.clone(),
            settings: settings.clone(),
            client,
            next_id: self.inner.next_id.clone(),
            interceptors,
            dynamic_passthrough: Mutex::new(HashSet::new()),
            shutdown_rx: stop_rx,
        });

        let bind: SocketAddr = format!("{}:{}", settings.bind_addr, settings.port)
            .parse()
            .with_context(|| format!("invalid bind address {}:{}", settings.bind_addr, settings.port))?;
        let (local_addr, handle) = proxy::start(engine, bind, &stop_tx).await?;
        tracing::info!("proxy listening on {local_addr}");
        *running = Some(Running { local_addr, stop_tx, handle });

        let status = self.status_locked(&running);
        let _ = self.inner.bus.send(ProxyEvent::ProxyState(status.clone()));
        Ok(status)
    }

    pub async fn stop(&self) -> Result<ProxyStatus> {
        let mut running = self.inner.running.lock().await;
        if let Some(r) = running.take() {
            let _ = r.stop_tx.send(true);
            let _ = r.handle.await;
            tracing::info!("proxy stopped");
        }
        drop(running);

        if self.inner.system_proxy_enabled.swap(false, Ordering::SeqCst) {
            if let Err(e) = self.disable_system_proxy_blocking().await {
                tracing::warn!("failed to unset system proxy on stop: {e:#}");
            }
        }

        let status = self.status().await;
        let _ = self.inner.bus.send(ProxyEvent::ProxyState(status.clone()));
        Ok(status)
    }

    pub async fn status(&self) -> ProxyStatus {
        let running = self.inner.running.lock().await;
        self.status_locked(&running)
    }

    fn status_locked(&self, running: &Option<Running>) -> ProxyStatus {
        ProxyStatus {
            running: running.is_some(),
            addr: running.as_ref().map(|r| r.local_addr.to_string()),
            port: running.as_ref().map(|r| r.local_addr.port()),
            system_proxy_enabled: self.inner.system_proxy_enabled.load(Ordering::SeqCst),
        }
    }

    pub async fn query(&self, filter: QueryFilter) -> Result<Vec<TransactionSummary>> {
        self.inner.store.query(filter).await
    }

    pub async fn get_transaction(&self, id: u64) -> Result<Option<TransactionDetail>> {
        self.inner.store.get(id).await
    }

    pub async fn clear_session(&self) -> Result<()> {
        self.inner.store.clear().await
    }

    pub fn get_settings(&self) -> ProxySettings {
        self.inner.settings.lock().unwrap().clone()
    }

    /// Persist new settings. Takes effect on the next proxy start.
    pub fn set_settings(&self, settings: ProxySettings) -> Result<()> {
        settings.save(&self.inner.data_dir)?;
        *self.inner.settings.lock().unwrap() = settings;
        Ok(())
    }

    pub fn ca_state(&self) -> CaState {
        let generated = self.inner.ca.is_generated();
        let cert_path = self.inner.ca.cert_path();
        CaState {
            generated,
            trusted: if generated { macos::ca_trusted(&cert_path) } else { None },
            cert_path: generated.then(|| cert_path.display().to_string()),
        }
    }

    /// Generate (if needed) and return the CA cert PEM plus its on-disk path.
    pub fn export_ca(&self) -> Result<(String, String)> {
        let pem = self.inner.ca.cert_pem()?;
        Ok((pem, self.inner.ca.cert_path().display().to_string()))
    }

    pub async fn install_ca_trust(&self) -> Result<CaState> {
        self.inner.ca.ensure_ca()?;
        let cert_path = self.inner.ca.cert_path();
        tokio::task::spawn_blocking(move || macos::install_ca_trust(&cert_path)).await??;
        let state = self.ca_state();
        let _ = self.inner.bus.send(ProxyEvent::CaState(state.clone()));
        Ok(state)
    }

    /// Toggle the macOS system-wide proxy. Requires the proxy to be running
    /// when enabling.
    pub async fn set_system_proxy(&self, enabled: bool) -> Result<ProxyStatus> {
        if enabled {
            let port = {
                let running = self.inner.running.lock().await;
                running
                    .as_ref()
                    .map(|r| r.local_addr.port())
                    .context("start the proxy before enabling the system proxy")?
            };
            let data_dir = self.inner.data_dir.clone();
            tokio::task::spawn_blocking(move || macos::enable_system_proxy(&data_dir, port))
                .await??;
            self.inner.system_proxy_enabled.store(true, Ordering::SeqCst);
        } else {
            self.disable_system_proxy_blocking().await?;
            self.inner.system_proxy_enabled.store(false, Ordering::SeqCst);
        }
        let status = self.status().await;
        let _ = self.inner.bus.send(ProxyEvent::ProxyState(status.clone()));
        Ok(status)
    }

    async fn disable_system_proxy_blocking(&self) -> Result<()> {
        let data_dir = self.inner.data_dir.clone();
        tokio::task::spawn_blocking(move || macos::disable_system_proxy(&data_dir)).await?
    }

    /// Best-effort cleanup before process exit: never leave the user's
    /// network pointed at a dead proxy.
    pub async fn prepare_exit(&self) {
        if self.inner.system_proxy_enabled.swap(false, Ordering::SeqCst) {
            if let Err(e) = self.disable_system_proxy_blocking().await {
                tracing::error!("failed to restore system proxy on exit: {e:#}");
            }
        }
    }

    /// Milliseconds since epoch; exposed for shells that want a consistent
    /// clock with transaction timestamps.
    pub fn now_ms(&self) -> i64 {
        now_ms()
    }
}
