//! macOS platform integration: system proxy toggle, keychain-backed secret
//! storage, and CA trust install/check. Compiles to inert stubs elsewhere so
//! the core stays testable on any platform.

use std::path::Path;

use anyhow::Result;

use crate::ca::SecretStore;

/// Pick the platform's secret store for the CA private key.
pub fn default_secret_store(data_dir: &Path) -> Box<dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        let _ = data_dir;
        Box::new(imp::KeychainSecretStore)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(crate::ca::FileSecretStore::new(data_dir.join("secrets")))
    }
}

#[cfg(target_os = "macos")]
pub use imp::*;

#[cfg(not(target_os = "macos"))]
pub use stub::*;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub struct KeychainSecretStore;

    impl SecretStore for KeychainSecretStore {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            match security_framework::passwords::get_generic_password("dev.httpmate", name) {
                Ok(data) => Ok(Some(data)),
                // errSecItemNotFound
                Err(e) if e.code() == -25300 => Ok(None),
                Err(e) => Err(anyhow::anyhow!("keychain read failed: {e}")),
            }
        }

        fn set(&self, name: &str, data: &[u8]) -> Result<()> {
            security_framework::passwords::set_generic_password("dev.httpmate", name, data)
                .map_err(|e| anyhow::anyhow!("keychain write failed: {e}"))
        }
    }

    pub fn enable_system_proxy(data_dir: &Path, port: u16) -> Result<()> {
        super::networksetup::enable(data_dir, port)
    }

    pub fn disable_system_proxy(data_dir: &Path) -> Result<()> {
        super::networksetup::disable(data_dir)
    }

    /// Restore network settings left behind by a crashed previous run.
    /// Returns true if a dangling snapshot was found and restored.
    pub fn restore_if_dangling(data_dir: &Path) -> Result<bool> {
        if super::networksetup::guard_exists(data_dir) {
            super::networksetup::disable(data_dir)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Whether the CA cert is trusted by the system. None = unknown.
    pub fn ca_trusted(cert_path: &Path) -> Option<bool> {
        let out = std::process::Command::new("security")
            .args(["verify-cert", "-c"])
            .arg(cert_path)
            .output()
            .ok()?;
        Some(out.status.success())
    }

    /// Install the CA into the user's login keychain as a trusted root.
    /// Triggers a system authentication prompt.
    pub fn install_ca_trust(cert_path: &Path) -> Result<()> {
        let home = std::env::var("HOME")?;
        let keychain = format!("{home}/Library/Keychains/login.keychain-db");
        let out = std::process::Command::new("security")
            .args(["add-trusted-cert", "-r", "trustRoot", "-k", &keychain])
            .arg(cert_path)
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!(
                "security add-trusted-cert failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    }
}

#[cfg(target_os = "macos")]
mod networksetup;

#[cfg(not(target_os = "macos"))]
mod stub {
    use super::*;

    pub fn enable_system_proxy(_data_dir: &Path, _port: u16) -> Result<()> {
        anyhow::bail!("system proxy configuration is only supported on macOS")
    }

    pub fn disable_system_proxy(_data_dir: &Path) -> Result<()> {
        anyhow::bail!("system proxy configuration is only supported on macOS")
    }

    pub fn restore_if_dangling(_data_dir: &Path) -> Result<bool> {
        Ok(false)
    }

    pub fn ca_trusted(_cert_path: &Path) -> Option<bool> {
        None
    }

    pub fn install_ca_trust(_cert_path: &Path) -> Result<()> {
        anyhow::bail!("CA trust installation is only supported on macOS")
    }
}
