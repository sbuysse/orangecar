//! SSL/TLS layer for Android Auto protocol.
//!
//! After the initial version exchange (unencrypted), the HU and phone perform
//! a TLS 1.2 handshake. The phone acts as TLS SERVER (receives ClientHello
//! from the HU). All subsequent messages are encrypted.
//!
//! We use rustls with a self-signed certificate. Some head units accept
//! self-signed; others may require Google-signed certs.

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Runtime location for the persisted self-signed TLS material.
///
/// Resolved at runtime (NOT `CARGO_MANIFEST_DIR`, which bakes in the build
/// machine's path): prefer `$XDG_CONFIG_HOME/rascar/certs`, then
/// `$HOME/.config/rascar/certs`, falling back to `/etc/rascar/certs` when
/// neither env var is set (e.g. running as a system service).
fn certs_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("rascar").join("certs");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".config").join("rascar").join("certs");
    }
    PathBuf::from("/etc/rascar/certs")
}

/// Write the TLS private key with owner-only permissions.
///
/// `fs::write` honours the process umask (typically 0o022 → 0o644), which would
/// leave the device key world-readable in `$HOME/.config/rascar/certs` or
/// `/etc/rascar/certs`. Create it 0o600 from the start so it is never briefly
/// readable, rather than chmod-ing after the fact.
fn write_private_key(path: &Path, key_der: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(key_der)?;
    Ok(())
}

/// Tighten permissions on a key written by an earlier (world-readable) version.
fn harden_existing_key(path: &Path) {
    match fs::metadata(path) {
        Ok(meta) if meta.permissions().mode() & 0o077 != 0 => {
            match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
                Ok(()) => warn!("tightened permissions on {} to 0600", path.display()),
                Err(e) => warn!("could not tighten permissions on {}: {e}", path.display()),
            }
        }
        _ => {}
    }
}

/// Generate or load self-signed RSA 2048 TLS certificate.
fn ensure_certs() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let dir = certs_dir();
    // 0o700: the directory holds the device private key, so it must not be
    // traversable by other users.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let key_path = dir.join("device.key.der");
    let cert_path = dir.join("device.cert.der");

    if key_path.exists() && cert_path.exists() {
        harden_existing_key(&key_path);
        let key_der = fs::read(&key_path)?;
        let cert_der = fs::read(&cert_path)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
        let cert = CertificateDer::from(cert_der);
        return Ok((vec![cert], key));
    }

    info!("generating self-signed TLS certificate ...");
    let mut params = CertificateParams::new(vec!["RasCar Android Auto".into()])?;
    params.distinguished_name.push(
        rcgen::DnType::OrganizationName,
        rcgen::DnValue::Utf8String("RasCar".into()),
    );

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    fs::write(&cert_path, &cert_der)?;
    write_private_key(&key_path, &key_der)?;
    info!("certificate saved to {}", dir.display());

    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    let cert = CertificateDer::from(cert_der);
    Ok((vec![cert], key))
}

/// TLS context for the AA protocol.
/// We act as TLS SERVER — the car's head unit initiates the handshake.
pub struct AATlsContext {
    pub config: Arc<ServerConfig>,
}

impl AATlsContext {
    pub fn new() -> Result<Self> {
        let (certs, key) = ensure_certs().context("loading TLS certs")?;

        let config = ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS12])
        .context("setting TLS 1.2")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building TLS config")?;

        Ok(Self {
            config: Arc::new(config),
        })
    }
}

/// Wraps raw bytes through TLS encryption/decryption.
///
/// Since the AA protocol sends TLS records inside USB frames (not over a TCP socket),
/// we manually drive the TLS state machine using rustls's Connection API
/// and feed bytes in/out explicitly.
pub struct AATlsSession {
    conn: rustls::ServerConnection,
    handshake_complete: bool,
}

impl AATlsSession {
    pub fn new(config: Arc<ServerConfig>) -> Result<Self> {
        let conn = rustls::ServerConnection::new(config)?;
        Ok(Self {
            conn,
            handshake_complete: false,
        })
    }

    /// Feed incoming TLS handshake/data bytes from the HU.
    /// Returns outgoing bytes to send back.
    pub fn feed(&mut self, incoming: &[u8]) -> Result<Vec<u8>> {
        // Write incoming ciphertext into rustls
        let mut rd = &incoming[..];
        while !rd.is_empty() {
            match self.conn.read_tls(&mut rd) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.conn.process_new_packets()?;

        if !self.handshake_complete && !self.conn.is_handshaking() {
            self.handshake_complete = true;
            info!("TLS handshake complete");
        }

        // Read outgoing TLS data
        let mut outgoing = Vec::new();
        self.conn.write_tls(&mut outgoing)?;
        Ok(outgoing)
    }

    /// Encrypt plaintext for transmission.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        use std::io::Write;
        self.conn.writer().write_all(plaintext)?;
        let mut out = Vec::new();
        self.conn.write_tls(&mut out)?;
        Ok(out)
    }

    /// Decrypt ciphertext received from USB.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let mut rd = &ciphertext[..];
        while !rd.is_empty() {
            match self.conn.read_tls(&mut rd) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.conn.process_new_packets()?;
        let mut plaintext = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            match self.conn.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => plaintext.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(plaintext)
    }

    pub fn is_handshake_complete(&self) -> bool {
        self.handshake_complete
    }
}
