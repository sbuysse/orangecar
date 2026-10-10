//! rascar-release — the dev-machine half of OTA (driven by tools/release.sh).
//!
//! ```text
//! rascar-release keygen  --key ~/.config/rascar/ota-signing.key     # once; prints the public key
//! rascar-release pubkey  --key …                                     # print the public key again
//! rascar-release bundle  --dir <staging> --out bundle.tar.gz         # deterministic tar.gz of tree/ + system/
//! rascar-release manifest --dir <staging> --bundle bundle.tar.gz --version V --serial N … --out manifest.json
//! rascar-release sign    --key … --manifest manifest.json --out manifest.sig
//! rascar-release verify  --pubkey <hex> --manifest manifest.json --sig manifest.sig [--bundle bundle.tar.gz]
//! ```
//! The signing key is a 32-byte ed25519 seed in hex. It never leaves the dev
//! machine; the car only holds the public key.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rascar_agent::manifest::{self, BundleRef, FileEntry, Health, HealthService, Manifest};
use rascar_agent::util;

#[derive(Parser)]
#[command(about = "Build, sign and verify OrangeCar OTA releases")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Keygen {
        #[arg(long)]
        key: PathBuf,
    },
    Pubkey {
        #[arg(long)]
        key: PathBuf,
    },
    Bundle {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Manifest {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        serial: u64,
        #[arg(long, default_value = "")]
        git_sha: String,
        #[arg(long, default_value = "orangecar")]
        target: String,
        /// Tree dirs fully owned by the bundle (comma-separated).
        #[arg(long, value_delimiter = ',')]
        mirror: Vec<String>,
        /// Tree paths never touched (comma-separated).
        #[arg(long, value_delimiter = ',')]
        preserve: Vec<String>,
        /// Services restarted by a live apply (comma-separated).
        #[arg(long, value_delimiter = ',')]
        restart: Vec<String>,
        /// Units that must stay active without restart (comma-separated).
        #[arg(long, value_delimiter = ',')]
        health_active: Vec<String>,
        /// Units that must never enter `failed` (comma-separated).
        #[arg(long, value_delimiter = ',')]
        health_not_failed: Vec<String>,
        /// URLs that must answer 2xx (comma-separated).
        #[arg(long, value_delimiter = ',')]
        health_http: Vec<String>,
        #[arg(long, default_value_t = 60)]
        settle_secs: u64,
        #[arg(long)]
        out: PathBuf,
    },
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Verify {
        /// Hex public key, or a file holding it.
        #[arg(long)]
        pubkey: String,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        sig: PathBuf,
        #[arg(long)]
        bundle: Option<PathBuf>,
        #[arg(long, default_value = "orangecar")]
        target: String,
    },
}

fn read_key(path: &Path) -> Result<String> {
    let k = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(k.trim().to_string())
}

/// Every regular file under `dir`, as sorted relative paths.
fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    fn rec(base: &Path, d: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for e in std::fs::read_dir(d)? {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_dir() {
                rec(base, &e.path(), out)?;
            } else if ft.is_file() {
                out.push(e.path().strip_prefix(base)?.to_path_buf());
            } else {
                bail!("{} is not a regular file (symlinks are not shipped)", e.path().display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    rec(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen { key } => {
            if key.exists() {
                bail!("{} exists; refusing to overwrite a signing key", key.display());
            }
            let mut seed = [0u8; 32];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).map_err(|_| anyhow::anyhow!("no randomness"))?;
            if let Some(p) = key.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key)?;
            writeln!(f, "{}", util::hex(&seed))?;
            println!("{}", manifest::public_key_hex(&util::hex(&seed))?);
        }
        Cmd::Pubkey { key } => println!("{}", manifest::public_key_hex(&read_key(&key)?)?),
        Cmd::Bundle { dir, out } => {
            // Deterministic: sorted entries, mtime 0, root:root, modes kept.
            let mut tb = tar::Builder::new(flate2::write::GzEncoder::new(std::fs::File::create(&out)?, flate2::Compression::best()));
            for rel in walk(&dir)? {
                let rs = rel.to_string_lossy();
                manifest::classify(&rs)?;
                let p = dir.join(&rel);
                let meta = std::fs::metadata(&p)?;
                let mut h = tar::Header::new_gnu();
                h.set_size(meta.len());
                h.set_mode(meta.permissions().mode() & 0o7777);
                h.set_mtime(0);
                h.set_uid(0);
                h.set_gid(0);
                h.set_cksum();
                tb.append_data(&mut h, &rel, std::fs::File::open(&p)?)?;
            }
            tb.into_inner()?.finish()?;
        }
        Cmd::Manifest { dir, bundle, version, serial, git_sha, target, mirror, preserve, restart, health_active, health_not_failed, health_http, settle_secs, out } => {
            let mut files = Vec::new();
            for rel in walk(&dir)? {
                let p = dir.join(&rel);
                files.push(FileEntry {
                    path: rel.to_string_lossy().into_owned(),
                    sha256: util::sha256_file(&p)?,
                    size: std::fs::metadata(&p)?.len(),
                    mode: std::fs::metadata(&p)?.permissions().mode() & 0o7777,
                });
            }
            fn svc(v: Vec<String>, check: &'static str) -> impl Iterator<Item = HealthService> {
                v.into_iter().filter(|u| !u.is_empty()).map(move |unit| HealthService { unit, check: check.into() })
            }
            let m = Manifest {
                format: manifest::FORMAT,
                version,
                serial,
                git_sha,
                created: chrono::Utc::now().to_rfc3339(),
                target: target.clone(),
                bundle: BundleRef { size: std::fs::metadata(&bundle)?.len(), sha256: util::sha256_file(&bundle)? },
                files,
                mirror: mirror.into_iter().filter(|s| !s.is_empty()).collect(),
                preserve: preserve.into_iter().filter(|s| !s.is_empty()).collect(),
                restart: restart.into_iter().filter(|s| !s.is_empty()).collect(),
                health: Health {
                    services: svc(health_active, "active").chain(svc(health_not_failed, "not-failed")).collect(),
                    http: health_http.into_iter().filter(|s| !s.is_empty()).collect(),
                    settle_secs,
                },
            };
            m.validate(&target)?;
            std::fs::write(&out, serde_json::to_vec_pretty(&m)?)?;
        }
        Cmd::Sign { key, manifest: mp, out } => {
            let bytes = std::fs::read(&mp)?;
            // Refuse to sign something the car would refuse anyway.
            let m: Manifest = serde_json::from_slice(&bytes).context("not a manifest")?;
            m.validate(&m.target)?;
            std::fs::write(&out, manifest::sign(&read_key(&key)?, &bytes)? + "\n")?;
        }
        Cmd::Verify { pubkey, manifest: mp, sig, bundle, target } => {
            let pk = if Path::new(&pubkey).is_file() { read_key(Path::new(&pubkey))? } else { pubkey };
            let bytes = std::fs::read(&mp)?;
            let m = manifest::open(&pk, &bytes, &std::fs::read_to_string(&sig)?, &target)?;
            if let Some(b) = bundle {
                let sha = util::sha256_file(&b)?;
                if sha != m.bundle.sha256 || std::fs::metadata(&b)?.len() != m.bundle.size {
                    bail!("bundle does not match the manifest");
                }
            }
            println!("OK {} serial {} ({} files)", m.version, m.serial, m.files.len());
        }
    }
    Ok(())
}
