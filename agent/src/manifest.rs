//! Release manifest: what a bundle contains, how to verify it, how to apply it.
//!
//! Trust model: the manifest is signed (ed25519) on the release machine and
//! verified on the car with a public key the car holds (`/etc/rascar/ota.pub`).
//! The manifest pins the SHA-256 of the bundle and of every file in it, so
//! once the signature verifies, nothing the update server (or the network) serves can
//! change what gets installed. A `serial` that must grow blocks replaying an
//! old signed release (downgrade to a known-bad build).
//!
//! Bundle layout (gzip tar):
//! ```text
//! tree/<path>     goes into the release tree (the /opt/orangecar symlink target)
//! system/<path>   installed at /<path>, allowlisted (scripts, units, udev rules)
//! ```

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: u32,
    /// Human version, also the release directory name (`20261006.1412-a056fb1`).
    pub version: String,
    /// Strictly increasing across releases (unix time of the build).
    pub serial: u64,
    pub git_sha: String,
    pub created: String,
    /// Device family the release is for (`orangecar`).
    pub target: String,
    pub bundle: BundleRef,
    pub files: Vec<FileEntry>,
    /// Tree directories the bundle fully owns: files there that are not in
    /// the bundle are removed from the new tree (like `rsync --delete`).
    #[serde(default)]
    pub mirror: Vec<String>,
    /// Tree paths never removed nor replaced (board-only files: certificates,
    /// Chromium's `_metadata`).
    #[serde(default)]
    pub preserve: Vec<String>,
    /// Services restarted by a live apply (`apply-now`) or a live rollback.
    /// A boot-time apply restarts nothing: the services start after it.
    #[serde(default)]
    pub restart: Vec<String>,
    #[serde(default)]
    pub health: Health,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleRef {
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// `tree/<rel>` or `system/<abs without the leading slash>`.
    pub path: String,
    pub sha256: String,
    pub size: u64,
    /// Unix permission bits (e.g. 0o755 = 493).
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Health {
    pub services: Vec<HealthService>,
    /// URLs that must answer 2xx (e.g. the dashboard on :8080).
    pub http: Vec<String>,
    /// Seconds the services are watched before the release is accepted.
    pub settle_secs: u64,
}

impl Default for Health {
    fn default() -> Self {
        Self { services: vec![], http: vec![], settle_secs: 60 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthService {
    pub unit: String,
    /// `active`: running the whole window without a restart.
    /// `not-failed`: may restart (one-attempt-per-run units) but never `failed`.
    #[serde(default = "default_check")]
    pub check: String,
}

fn default_check() -> String {
    "active".into()
}

/// Where a bundle entry goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// Relative path inside the release tree.
    Tree(String),
    /// Absolute system path.
    System(String),
}

/// System paths a release may write. Anything else in `system/` is refused,
/// so even a correctly signed bundle cannot touch board configuration
/// (`/etc/rascar`), credentials, or the boot guard that rolls releases back.
pub fn system_path_allowed(abs: &str) -> bool {
    // The boot guard and its unit: the rollback path must not be replaceable
    // by the releases it judges.
    const GUARD: [&str; 2] = ["/usr/local/sbin/rascar-ota-boot", "/etc/systemd/system/rascar-ota-boot.service"];
    let name = abs.rsplit('/').next().unwrap_or("");
    if GUARD.contains(&abs) || abs.starts_with("/etc/systemd/system/rascar-ota-boot.service.d/") || abs.contains("/../") || abs.ends_with("/..") || !abs.starts_with('/') {
        return false;
    }
    (abs.starts_with("/usr/local/sbin/rascar-") && !abs[16..].contains('/'))
        || (abs.starts_with("/etc/systemd/system/rascar-") && (abs.ends_with(".service") || abs.ends_with(".target") || abs.ends_with(".timer") || abs.ends_with(".conf")))
        || (abs.starts_with("/etc/udev/rules.d/") && name.contains("rascar") && name.ends_with(".rules") && abs.matches('/').count() == 4)
        || (abs.starts_with("/etc/modules-load.d/rascar") && name.ends_with(".conf"))
}

/// A relative path with only normal components (no `..`, no absolute, no `.`).
pub fn clean_relative(p: &str) -> bool {
    !p.is_empty()
        && !p.contains('\\')
        && Path::new(p).components().all(|c| matches!(c, Component::Normal(_)))
        && !p.ends_with('/')
}

/// Split a bundle entry path into its destination, refusing anything unsafe.
pub fn classify(entry: &str) -> Result<Dest> {
    if let Some(rel) = entry.strip_prefix("tree/") {
        if clean_relative(rel) {
            return Ok(Dest::Tree(rel.to_string()));
        }
    } else if let Some(rel) = entry.strip_prefix("system/") {
        let abs = format!("/{rel}");
        if clean_relative(rel) && system_path_allowed(&abs) {
            return Ok(Dest::System(abs));
        }
    }
    bail!("bundle entry {entry:?} is not allowed")
}

/// Is `rel` equal to, or inside, one of `prefixes`?
pub fn under_any(rel: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        let p = p.trim_end_matches('/');
        rel == p || rel.starts_with(&format!("{p}/"))
    })
}

impl Manifest {
    /// Structural checks every manifest must pass before anything is downloaded.
    pub fn validate(&self, target: &str) -> Result<()> {
        if self.format != FORMAT {
            bail!("manifest format {} is not supported", self.format);
        }
        if self.target != target {
            bail!("manifest targets {:?}, this device is {target:?}", self.target);
        }
        if self.version.is_empty()
            || self.version.len() > 64
            || !self.version.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.version.starts_with('.')
        {
            bail!("invalid version {:?}", self.version);
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.files {
            classify(&f.path)?;
            if !seen.insert(f.path.as_str()) {
                bail!("{} is listed twice", f.path);
            }
            if f.sha256.len() != 64 {
                bail!("{} has no valid sha256", f.path);
            }
            if f.mode & !0o7777 != 0 || f.mode & 0o6000 != 0 {
                bail!("{} has a forbidden mode {:o}", f.path, f.mode);
            }
        }
        for m in self.mirror.iter().chain(&self.preserve) {
            if !clean_relative(m.trim_end_matches('/')) {
                bail!("invalid mirror/preserve path {m:?}");
            }
        }
        for s in self.restart.iter().chain(self.health.services.iter().map(|h| &h.unit)) {
            if !s.starts_with("rascar-") || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b)) {
                bail!("service {s:?} may not be managed by a release");
            }
        }
        for h in &self.health.services {
            if h.check != "active" && h.check != "not-failed" {
                bail!("unknown health check {:?}", h.check);
            }
        }
        Ok(())
    }
}

/// Verify `sig_b64` (base64, 64 bytes) over `msg` with a hex ed25519 key.
pub fn verify(pubkey_hex: &str, msg: &[u8], sig_b64: &str) -> Result<()> {
    let pk = crate::util::unhex(pubkey_hex).context("public key is not hex")?;
    if pk.len() != 32 {
        bail!("public key must be 32 bytes");
    }
    let sig = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim()).context("signature is not base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pk)
        .verify(msg, &sig)
        .map_err(|_| anyhow::anyhow!("signature does not verify with the release key"))
}

/// Verify the signature, then parse and validate. Only bytes that verified
/// are ever parsed.
pub fn open(pubkey_hex: &str, manifest_bytes: &[u8], sig_b64: &str, target: &str) -> Result<Manifest> {
    verify(pubkey_hex, manifest_bytes, sig_b64)?;
    let m: Manifest = serde_json::from_slice(manifest_bytes).context("manifest is not valid JSON")?;
    m.validate(target)?;
    Ok(m)
}

/// Sign with a 32-byte seed (hex). Returns base64.
pub fn sign(seed_hex: &str, msg: &[u8]) -> Result<String> {
    let kp = keypair(seed_hex)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(kp.sign(msg).as_ref()))
}

pub fn keypair(seed_hex: &str) -> Result<ring::signature::Ed25519KeyPair> {
    let seed = crate::util::unhex(seed_hex).context("signing key is not hex")?;
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| anyhow::anyhow!("invalid ed25519 seed"))
}

pub fn public_key_hex(seed_hex: &str) -> Result<String> {
    use ring::signature::KeyPair;
    Ok(crate::util::hex(keypair(seed_hex)?.public_key().as_ref()))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

    pub fn sample() -> Manifest {
        Manifest {
            format: 1,
            version: "20261006.1412-a056fb1".into(),
            serial: 1_791_000_000,
            git_sha: "a056fb1".into(),
            created: "2026-10-06T14:12:00Z".into(),
            target: "orangecar".into(),
            bundle: BundleRef { size: 10, sha256: "0".repeat(64) },
            files: vec![FileEntry { path: "tree/target/release/rascar-app".into(), sha256: "a".repeat(64), size: 1, mode: 0o755 }],
            mirror: vec!["app/frontend".into()],
            preserve: vec!["bridge/certs".into()],
            restart: vec!["rascar-app".into()],
            health: Health::default(),
        }
    }

    #[test]
    fn signed_manifest_opens_and_tampering_is_caught() {
        let bytes = serde_json::to_vec(&sample()).unwrap();
        let sig = sign(SEED, &bytes).unwrap();
        let pk = public_key_hex(SEED).unwrap();
        assert_eq!(open(&pk, &bytes, &sig, "orangecar").unwrap(), sample());

        // One byte changed after signing.
        let mut evil = bytes.clone();
        let i = evil.iter().position(|&b| b == b'a').unwrap();
        evil[i] = b'b';
        assert!(open(&pk, &evil, &sig, "orangecar").is_err());
        // Another key (a compromised update server signing its own release).
        let other = public_key_hex(&"01".repeat(32)).unwrap();
        assert!(open(&other, &bytes, &sig, "orangecar").is_err());
        // Garbage signature.
        assert!(open(&pk, &bytes, "AAAA", "orangecar").is_err());
        // Right signature, wrong device.
        assert!(open(&pk, &bytes, &sig, "othercar").is_err());
    }

    #[test]
    fn rfc8032_test_vector_1_verifies() {
        // RFC 8032 §7.1 TEST 1 (empty message).
        let pk = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        let sig_hex = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";
        let sig = base64::engine::general_purpose::STANDARD.encode(crate::util::unhex(sig_hex).unwrap());
        verify(pk, b"", &sig).unwrap();
        assert_eq!(
            public_key_hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60").unwrap(),
            pk
        );
    }

    #[test]
    fn bundle_paths_are_confined() {
        assert_eq!(classify("tree/app/frontend/index.html").unwrap(), Dest::Tree("app/frontend/index.html".into()));
        assert_eq!(
            classify("system/usr/local/sbin/rascar-carplay-run").unwrap(),
            Dest::System("/usr/local/sbin/rascar-carplay-run".into())
        );
        assert!(classify("system/etc/systemd/system/rascar-app.service").is_ok());
        assert!(classify("system/etc/systemd/system/rascar-carplay.service.d/20-chromium-clean-stop.conf").is_ok());
        assert!(classify("system/etc/udev/rules.d/70-rascar-aa.rules").is_ok());
        for bad in [
            "tree/../etc/passwd",
            "tree//abs",
            "tree/",
            "tree/./x",
            "system/etc/rascar/agent.json",
            "system/etc/shadow",
            "system/usr/local/sbin/rascar-ota-boot",
            "system/etc/systemd/system/rascar-ota-boot.service",
            "system/etc/systemd/system/rascar-ota-boot.service.d/x.conf",
            "system/usr/local/sbin/other",
            "system/usr/local/sbin/rascar-x/../../bin/sh",
            "system/etc/udev/rules.d/99-evil.rules",
            "system/etc/systemd/system/sshd.service",
            "other/x",
        ] {
            assert!(classify(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validate_refuses_unmanaged_services_and_setuid() {
        let mut m = sample();
        m.restart = vec!["sshd".into()];
        assert!(m.validate("orangecar").is_err());
        let mut m = sample();
        m.files[0].mode = 0o4755;
        assert!(m.validate("orangecar").is_err());
        let mut m = sample();
        m.files.push(m.files[0].clone());
        assert!(m.validate("orangecar").is_err());
        let mut m = sample();
        m.version = "../x".into();
        assert!(m.validate("orangecar").is_err());
    }

    #[test]
    fn under_any_matches_whole_components() {
        let p = vec!["app/frontend".to_string()];
        assert!(under_any("app/frontend", &p));
        assert!(under_any("app/frontend/js/app.js", &p));
        assert!(!under_any("app/frontend2/x", &p));
    }
}
