//! System proxy toggling via `networksetup`, with a snapshot of prior state
//! and a guard file so a crashed run can be repaired on next launch.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const SNAPSHOT_FILE: &str = "system-proxy-snapshot.json";
const GUARD_FILE: &str = "system-proxy.guard";

#[derive(Debug, Serialize, Deserialize)]
struct ProxyState {
    enabled: bool,
    server: String,
    port: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ServiceSnapshot {
    service: String,
    web: ProxyState,
    secure: ProxyState,
}

pub fn guard_exists(data_dir: &Path) -> bool {
    data_dir.join(GUARD_FILE).exists()
}

pub fn enable(data_dir: &Path, port: u16) -> Result<()> {
    let services = list_services()?;
    if services.is_empty() {
        bail!("no active network services found");
    }

    // Snapshot only once: re-enabling (e.g. after a port change) must not
    // overwrite the user's original settings with ours.
    if !guard_exists(data_dir) {
        let snapshot: Vec<ServiceSnapshot> = services
            .iter()
            .map(|s| {
                Ok(ServiceSnapshot {
                    service: s.clone(),
                    web: get_state(s, false)?,
                    secure: get_state(s, true)?,
                })
            })
            .collect::<Result<_>>()?;
        std::fs::write(data_dir.join(SNAPSHOT_FILE), serde_json::to_vec_pretty(&snapshot)?)?;
        std::fs::write(data_dir.join(GUARD_FILE), b"httpmate system proxy is active\n")?;
    }

    let port = port.to_string();
    for service in &services {
        run(&["-setwebproxy", service, "127.0.0.1", &port])?;
        run(&["-setsecurewebproxy", service, "127.0.0.1", &port])?;
    }
    Ok(())
}

pub fn disable(data_dir: &Path) -> Result<()> {
    let snapshot_path = data_dir.join(SNAPSHOT_FILE);
    let snapshot: Vec<ServiceSnapshot> = match std::fs::read(&snapshot_path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    if snapshot.is_empty() {
        // No snapshot — just switch proxying off everywhere.
        for service in list_services()? {
            run(&["-setwebproxystate", &service, "off"])?;
            run(&["-setsecurewebproxystate", &service, "off"])?;
        }
    } else {
        for entry in &snapshot {
            restore_one(&entry.service, &entry.web, false)?;
            restore_one(&entry.service, &entry.secure, true)?;
        }
    }

    let _ = std::fs::remove_file(&snapshot_path);
    let _ = std::fs::remove_file(data_dir.join(GUARD_FILE));
    Ok(())
}

fn restore_one(service: &str, state: &ProxyState, secure: bool) -> Result<()> {
    let (set_proxy, set_state) = if secure {
        ("-setsecurewebproxy", "-setsecurewebproxystate")
    } else {
        ("-setwebproxy", "-setwebproxystate")
    };
    if state.enabled && !state.server.is_empty() {
        run(&[set_proxy, service, &state.server, &state.port])?;
        run(&[set_state, service, "on"])?;
    } else {
        run(&[set_state, service, "off"])?;
    }
    Ok(())
}

fn list_services() -> Result<Vec<String>> {
    let out = run(&["-listallnetworkservices"])?;
    Ok(out
        .lines()
        .skip(1) // first line is an explanatory banner
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('*')) // '*' marks disabled services
        .map(str::to_string)
        .collect())
}

fn get_state(service: &str, secure: bool) -> Result<ProxyState> {
    let flag = if secure { "-getsecurewebproxy" } else { "-getwebproxy" };
    let out = run(&[flag, service])?;
    let mut state = ProxyState { enabled: false, server: String::new(), port: String::new() };
    for line in out.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "Enabled" => state.enabled = value.eq_ignore_ascii_case("yes"),
                "Server" => state.server = value.to_string(),
                "Port" => state.port = value.to_string(),
                _ => {}
            }
        }
    }
    Ok(state)
}

fn run(args: &[&str]) -> Result<String> {
    let out = Command::new("networksetup")
        .args(args)
        .output()
        .context("running networksetup (is this macOS?)")?;
    if !out.status.success() {
        bail!(
            "networksetup {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
