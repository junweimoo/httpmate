//! Certificate authority: a locally generated root CA plus on-demand leaf
//! certificates for TLS interception.
//!
//! The CA *private key* never sits next to the cert on disk: it goes through
//! a [`SecretStore`] (macOS keychain in the app; a 0600 file elsewhere and in
//! tests). Only the public certificate is written to the data dir for export
//! and trust installation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SerialNumber,
};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;

const CA_CERT_FILE: &str = "httpmate-ca.pem";
const CA_KEY_SECRET: &str = "ca-key.pem";
const CA_COMMON_NAME: &str = "httpmate Root CA";
/// Apple rejects TLS leaf certs valid for more than 825 days; stay under.
const LEAF_VALIDITY_DAYS: i64 = 820;
const ROOT_VALIDITY_DAYS: i64 = 3650;
const LEAF_CACHE_CAP: usize = 1024;

/// Storage for the CA private key.
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, name: &str, data: &[u8]) -> Result<()>;
}

/// File-based secret store (0600 on unix). Used on non-macOS platforms and in
/// tests; the app swaps in the keychain store on macOS.
pub struct FileSecretStore {
    dir: PathBuf,
}

impl FileSecretStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.dir.join(name)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, name: &str, data: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(name);
        std::fs::write(&path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

struct LoadedCa {
    cert: Certificate,
    key: KeyPair,
}

pub struct CertAuthority {
    data_dir: PathBuf,
    secrets: Box<dyn SecretStore>,
    inner: Mutex<Option<LoadedCa>>,
    leaf_cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl CertAuthority {
    pub fn new(data_dir: impl Into<PathBuf>, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            data_dir: data_dir.into(),
            secrets,
            inner: Mutex::new(None),
            leaf_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn cert_path(&self) -> PathBuf {
        self.data_dir.join(CA_CERT_FILE)
    }

    pub fn is_generated(&self) -> bool {
        self.cert_path().exists()
    }

    /// Load the persisted CA, generating a fresh one on first run.
    pub fn ensure_ca(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_some() {
            return Ok(());
        }
        *inner = Some(if self.cert_path().exists() {
            self.load_ca().context("loading persisted CA")?
        } else {
            self.generate_ca().context("generating root CA")?
        });
        Ok(())
    }

    pub fn cert_pem(&self) -> Result<String> {
        self.ensure_ca()?;
        std::fs::read_to_string(self.cert_path()).context("reading CA cert")
    }

    /// Mint (or fetch from cache) a leaf certificate for `host`, ready to be
    /// served by rustls. `host` is a DNS name or IP literal from SNI or the
    /// CONNECT authority.
    pub fn mint(&self, host: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(ck) = self.leaf_cache.lock().unwrap().get(host) {
            return Ok(ck.clone());
        }
        self.ensure_ca()?;
        let inner = self.inner.lock().unwrap();
        let ca = inner.as_ref().expect("ensure_ca filled inner");

        let leaf_key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .with_context(|| format!("invalid host for certificate: {host}"))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        dn.push(DnType::OrganizationName, "httpmate");
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(LEAF_VALIDITY_DAYS);
        // Unique-enough serial: duplicate serials from one CA upset clients.
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        params.serial_number = Some(SerialNumber::from(nanos));

        let cert = params.signed_by(&leaf_key, &ca.cert, &ca.key)?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let signing_key = any_supported_type(&key_der)
            .map_err(|e| anyhow::anyhow!("unsupported leaf key: {e}"))?;
        let ck = Arc::new(CertifiedKey::new(vec![cert.der().clone()], signing_key));

        let mut cache = self.leaf_cache.lock().unwrap();
        if cache.len() >= LEAF_CACHE_CAP {
            cache.clear();
        }
        cache.insert(host.to_string(), ck.clone());
        Ok(ck)
    }

    fn generate_ca(&self) -> Result<LoadedCa> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, CA_COMMON_NAME);
        dn.push(DnType::OrganizationName, "httpmate");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(ROOT_VALIDITY_DAYS);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        params.serial_number = Some(SerialNumber::from(nanos));

        let cert = params.self_signed(&key)?;

        self.secrets.set(CA_KEY_SECRET, key.serialize_pem().as_bytes())?;
        std::fs::create_dir_all(&self.data_dir)?;
        write_public(&self.cert_path(), cert.pem().as_bytes())?;
        tracing::info!("generated new root CA at {}", self.cert_path().display());
        Ok(LoadedCa { cert, key })
    }

    fn load_ca(&self) -> Result<LoadedCa> {
        let cert_pem = std::fs::read_to_string(self.cert_path())?;
        let Some(key_pem) = self.secrets.get(CA_KEY_SECRET)? else {
            bail!(
                "CA certificate exists but its private key is missing from the secret store; \
                 delete {} to regenerate",
                self.cert_path().display()
            );
        };
        let key = KeyPair::from_pem(std::str::from_utf8(&key_pem)?)?;
        // Re-signing parsed params with the same key yields a cert whose
        // subject and key match the trusted original, which is all leaf
        // chain validation needs (the root itself is never served).
        let params = CertificateParams::from_ca_cert_pem(&cert_pem)?;
        let cert = params.self_signed(&key)?;
        Ok(LoadedCa { cert, key })
    }
}

fn write_public(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_in(dir: &Path) -> CertAuthority {
        CertAuthority::new(dir, Box::new(FileSecretStore::new(dir.join("secrets"))))
    }

    #[test]
    fn generates_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(dir.path());
        assert!(!ca.is_generated());
        ca.ensure_ca().unwrap();
        assert!(ca.is_generated());
        let pem = ca.cert_pem().unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"));

        // Fresh instance loads the same CA from disk + secret store.
        let ca2 = ca_in(dir.path());
        ca2.ensure_ca().unwrap();
        assert_eq!(ca2.cert_pem().unwrap(), pem);
        ca2.mint("example.com").unwrap();
    }

    #[test]
    fn mints_and_caches_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let ca = ca_in(dir.path());
        let a = ca.mint("example.com").unwrap();
        let b = ca.mint("example.com").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second mint should hit the cache");
        let c = ca.mint("other.test").unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        // IP literals work too (SAN type IP).
        ca.mint("127.0.0.1").unwrap();
    }
}
