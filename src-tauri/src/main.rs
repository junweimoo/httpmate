//! Tauri shell: a thin adapter over httpmate-core's Controller.
//! Commands map 1:1 onto the controller; bus events are coalesced and
//! forwarded to the WebView.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use httpmate_core::events::{
    CaState, ProxyEvent, ProxyStatus, QueryFilter, TransactionDetail, TransactionSummary,
};
use httpmate_core::{AppConfig, Controller, ProxySettings};
use tauri::{Emitter, Manager, State};
use tokio::sync::broadcast;

struct AppState {
    controller: Controller,
}

fn err_str(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
async fn start_proxy(state: State<'_, AppState>, port: Option<u16>) -> Result<ProxyStatus, String> {
    state.controller.start(port).await.map_err(err_str)
}

#[tauri::command]
async fn stop_proxy(state: State<'_, AppState>) -> Result<ProxyStatus, String> {
    state.controller.stop().await.map_err(err_str)
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<ProxyStatus, String> {
    Ok(state.controller.status().await)
}

#[tauri::command]
async fn set_system_proxy(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<ProxyStatus, String> {
    state.controller.set_system_proxy(enabled).await.map_err(err_str)
}

#[tauri::command]
async fn query_transactions(
    state: State<'_, AppState>,
    filter: QueryFilter,
) -> Result<Vec<TransactionSummary>, String> {
    state.controller.query(filter).await.map_err(err_str)
}

#[tauri::command]
async fn get_transaction(
    state: State<'_, AppState>,
    id: u64,
) -> Result<Option<TransactionDetail>, String> {
    state.controller.get_transaction(id).await.map_err(err_str)
}

#[tauri::command]
async fn clear_session(state: State<'_, AppState>) -> Result<(), String> {
    state.controller.clear_session().await.map_err(err_str)
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<ProxySettings, String> {
    Ok(state.controller.get_settings())
}

#[tauri::command]
async fn set_settings(state: State<'_, AppState>, settings: ProxySettings) -> Result<(), String> {
    state.controller.set_settings(settings).map_err(err_str)
}

#[tauri::command]
async fn ca_state(state: State<'_, AppState>) -> Result<CaState, String> {
    Ok(state.controller.ca_state())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportedCa {
    pem: String,
    path: String,
}

#[tauri::command]
async fn export_ca(state: State<'_, AppState>) -> Result<ExportedCa, String> {
    let (pem, path) = state.controller.export_ca().map_err(err_str)?;
    Ok(ExportedCa { pem, path })
}

#[tauri::command]
async fn install_ca_trust(state: State<'_, AppState>) -> Result<CaState, String> {
    state.controller.install_ca_trust().await.map_err(err_str)
}

/// Forward bus events to the WebView. Transaction events are batched on a
/// 100ms tick so a busy proxy doesn't overwhelm IPC or the render loop;
/// state changes go out immediately.
async fn event_pump(mut rx: broadcast::Receiver<ProxyEvent>, app: tauri::AppHandle) {
    let mut buffer: Vec<ProxyEvent> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ProxyEvent::ProxyState(status)) => {
                    let _ = app.emit("proxy:state", &status);
                }
                Ok(ProxyEvent::CaState(ca)) => {
                    let _ = app.emit("ca:state", &ca);
                }
                Ok(ev) => buffer.push(ev),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("event pump lagged, dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                if !buffer.is_empty() {
                    let _ = app.emit("traffic:batch", &buffer);
                    buffer.clear();
                }
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,httpmate_core=debug".into()),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let controller = Controller::new(AppConfig { data_dir })?;
            let rx = controller.subscribe();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(event_pump(rx, handle));
            app.manage(AppState { controller });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_proxy,
            stop_proxy,
            get_status,
            set_system_proxy,
            query_transactions,
            get_transaction,
            clear_session,
            get_settings,
            set_settings,
            ca_state,
            export_ca,
            install_ca_trust,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Never leave the user's network pointed at a dead proxy.
                let state: State<AppState> = app.state();
                tauri::async_runtime::block_on(state.controller.prepare_exit());
            }
        });
}
