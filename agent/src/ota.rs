//! OTA: ask the update server for a newer release, download it, verify it, stage it,
//! and switch to it at the next boot with an automatic rollback.
//!
//! Life of a release on the car:
//!
//! 1. **Stage** (agent daemon, background, any time): offer → signature +
//!    manifest checks → resumable download → bundle SHA-256 → a complete new
//!    release tree in `/opt/orangecar-releases/<version>`: hard links of the
//!    current tree (board-only files such as `bridge/certs` come along) with
//!    the bundle's files written over them, each checked against its SHA-256.
//!    Nothing running is touched.
//! 2. **Apply** (`rascar-agent apply-staged`, called by the boot guard
//!    `rascar-ota-boot` from `rascar-ota-boot.service`, ordered before every
//!    rascar service): install the release's system files (backing up what
//!    they replace), then atomically point the `/opt/orangecar` symlink at the
//!    new tree and write the `trial` file. Milliseconds of local work, no
//!    network, no restart (the services start afterwards).
//! 3. **Judge** (agent daemon, once the services ran `settle_secs`): health
//!    checks from the manifest pass → the trial file is removed (committed);
//!    they fail → live rollback (symlink back, system files restored,
//!    services restarted) and the version is marked bad.
//! 4. **Boot guard**: a trial still pending after `max_trial_boots` boots
//!    (the new agent crashed, or never got to judge) is rolled back by the
//!    guard script, which is not part of any release.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::AgentConfig;
use crate::server::{Req, UpdateServer};
use crate::manifest::{self, Dest, Manifest};
use crate::util;

const STATE_FILE: &str = "ota-state.json";
pub const TRIAL_FILE: &str = "trial";
pub const BAD_FILE: &str = "bad-versions";
/// The guard leaves this behind when it rolled back, for the agent to report.
pub const GUARD_ROLLBACK_FILE: &str = "guard-rollback";
const OTA_DIR: &str = ".ota";
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(20);
const RESTART_TIMEOUT: Duration = Duration::from_secs(60);
/// No byte for this long during a download: drop the connection, resume later.
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const FSYNC_EVERY: u64 = 8 * 1024 * 1024;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OtaState {
    /// Version staged and waiting for the next boot.
    pub staged: Option<String>,
    pub last_update: Option<UpdateResult>,
    /// Download/stage failures per version (a version failing 5 times is skipped).
    #[serde(default)]
    pub failures: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateResult {
    pub version: String,
    /// `staged`, `applied`, `ok`, `rolled_back`, `failed`.
    pub result: String,
    pub at: String,
    pub detail: String,
}

impl UpdateResult {
    pub fn new(version: &str, result: &str, detail: impl Into<String>) -> Self {
        Self { version: version.into(), result: result.into(), at: chrono::Local::now().to_rfc3339(), detail: detail.into() }
    }
}

pub fn load_state(dir: &Path) -> OtaState {
    std::fs::read(dir.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(dir: &Path, st: &OtaState) -> Result<()> {
    util::atomic_write(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(st)?)
}

pub fn bad_versions(state_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(state_dir.join(BAD_FILE))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn mark_bad(state_dir: &Path, version: &str) -> Result<()> {
    let mut v = bad_versions(state_dir);
    if !v.iter().any(|x| x == version) {
        v.push(version.to_string());
    }
    util::atomic_write(&state_dir.join(BAD_FILE), (v.join("\n") + "\n").as_bytes())
}

// ── Trial file (shared with the boot guard shell script) ────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    pub version: String,
    pub release: PathBuf,
    pub previous: PathBuf,
    pub boots: u32,
}

impl Trial {
    pub fn render(&self) -> String {
        format!(
            "version={}\nrelease={}\nprevious={}\nboots={}\n",
            self.version,
            self.release.display(),
            self.previous.display(),
            self.boots
        )
    }

    pub fn parse(s: &str) -> Option<Self> {
        let kv: BTreeMap<&str, &str> = s.lines().filter_map(|l| l.split_once('=')).collect();
        Some(Self {
            version: kv.get("version")?.to_string(),
            release: kv.get("release")?.into(),
            previous: kv.get("previous")?.into(),
            boots: kv.get("boots")?.parse().ok()?,
        })
    }
}

pub fn read_trial(state_dir: &Path) -> Option<Trial> {
    Trial::parse(&std::fs::read_to_string(state_dir.join(TRIAL_FILE)).ok()?)
}

// ── Current release ─────────────────────────────────────────────────────────

/// Version + serial of the tree `install_link` points at. A tree that was not
/// installed by OTA (the one migrated from the manual deploys) is `base`/0.
pub fn installed(cfg: &AgentConfig) -> (String, u64) {
    let m = std::fs::read(cfg.ota.install_link.join(OTA_DIR).join("manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok());
    m.map(|m| (m.version, m.serial)).unwrap_or_else(|| ("base".into(), 0))
}

fn pubkey(cfg: &AgentConfig) -> Result<String> {
    let k = std::fs::read_to_string(&cfg.ota.pubkey_file)
        .with_context(|| format!("release public key {} missing", cfg.ota.pubkey_file.display()))?;
    Ok(k.trim().to_string())
}

/// The real directory behind `install_link`; it must BE a symlink (the
/// one-time migration in setup/ota/install-ota.sh makes it one).
pub fn current_tree(cfg: &AgentConfig) -> Result<PathBuf> {
    let meta = std::fs::symlink_metadata(&cfg.ota.install_link)
        .with_context(|| format!("{} missing", cfg.ota.install_link.display()))?;
    if !meta.file_type().is_symlink() {
        bail!("{} is not a symlink yet: run setup/ota/install-ota.sh --migrate first", cfg.ota.install_link.display());
    }
    Ok(std::fs::canonicalize(&cfg.ota.install_link)?)
}

// ── Offer → download → stage ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Offer {
    pub version: String,
    pub serial: u64,
    pub manifest: String,
    pub signature: String,
    pub bundle_size: u64,
    pub bundle_url: String,
}

/// What a check found.
#[derive(Debug, PartialEq)]
pub enum Check {
    UpToDate,
    AlreadyStaged(String),
    Deferred(&'static str),
    Staged(String),
}

/// Why an offered release is not taken, if it is not.
pub fn refuse_offer(m: &Manifest, offer: &Offer, installed_serial: u64, bad: &[String]) -> Option<String> {
    if m.version != offer.version || m.serial != offer.serial {
        return Some("offer does not match its signed manifest".into());
    }
    if m.serial <= installed_serial {
        return Some(format!("serial {} is not newer than the installed {installed_serial} (downgrade refused)", m.serial));
    }
    if bad.contains(&m.version) {
        return Some("version was rolled back before".into());
    }
    if m.bundle.size != offer.bundle_size {
        return Some("bundle size differs from the signed manifest".into());
    }
    None
}

pub async fn check_and_stage(cfg: &AgentConfig, g: &UpdateServer, st: &mut OtaState, metered: bool) -> Result<Check> {
    if read_trial(&cfg.state_dir).is_some() {
        return Ok(Check::Deferred("a release is on trial"));
    }
    let (cur_version, cur_serial) = installed(cfg);
    let path = format!(
        "/api/carops/update?version={}&serial={cur_serial}",
        cur_version.replace(|c: char| !c.is_ascii_alphanumeric() && !"._-".contains(c), "_")
    );
    let resp = g.send(&Req::get(path, Duration::from_secs(30))).await?;
    match resp.status().as_u16() {
        204 => return Ok(Check::UpToDate),
        200 => {}
        s => bail!("update check answered {s}"),
    }
    let offer: Offer = resp.json().await.context("update offer is not JSON")?;
    let key = pubkey(cfg)?;
    let manifest_bytes = base64::engine::general_purpose::STANDARD.decode(&offer.manifest).context("manifest not base64")?;
    let m = manifest::open(&key, &manifest_bytes, &offer.signature, &cfg.ota.target)?;
    if let Some(why) = refuse_offer(&m, &offer, cur_serial, &bad_versions(&cfg.state_dir)) {
        bail!("release {} refused: {why}", offer.version);
    }
    if st.staged.as_deref() == Some(&m.version) && cfg.ota.releases_dir.join(&m.version).is_dir() {
        return Ok(Check::AlreadyStaged(m.version));
    }
    if st.failures.get(&m.version).is_some_and(|n| *n >= 5) {
        return Ok(Check::Deferred("this release failed to stage 5 times"));
    }
    if metered && !cfg.ota.download_on_metered {
        return Ok(Check::Deferred("metered uplink"));
    }
    let dl = cfg.state_dir.join("ota");
    std::fs::create_dir_all(&dl)?;
    let bundle = dl.join(format!("{}.tar.gz", m.version));
    let result = async {
        if !bundle.exists() {
            download(g, &offer.bundle_url, &dl.join(format!("{}.tar.gz.part", m.version)), m.bundle.size).await?;
            let part = dl.join(format!("{}.tar.gz.part", m.version));
            let sha = tokio::task::spawn_blocking({
                let p = part.clone();
                move || util::sha256_file(&p)
            })
            .await??;
            if sha != m.bundle.sha256 {
                let _ = std::fs::remove_file(&part);
                bail!("bundle sha256 mismatch (got {sha})");
            }
            std::fs::rename(&part, &bundle)?;
        }
        let (cfg2, m2, mb, sig, b2) = (cfg.clone(), m.clone(), manifest_bytes.clone(), offer.signature.clone(), bundle.clone());
        tokio::task::spawn_blocking(move || stage(&cfg2, &m2, &mb, &sig, &b2)).await?
    }
    .await;
    match result {
        Ok(dir) => {
            info!(version = %m.version, dir = %dir.display(), "release staged; applies at next boot");
            let _ = std::fs::remove_file(&bundle);
            st.staged = Some(m.version.clone());
            st.last_update = Some(UpdateResult::new(&m.version, "staged", "applies at next boot"));
            st.failures.remove(&m.version);
            Ok(Check::Staged(m.version))
        }
        Err(e) => {
            *st.failures.entry(m.version.clone()).or_default() += 1;
            st.last_update = Some(UpdateResult::new(&m.version, "failed", format!("{e:#}")));
            Err(e)
        }
    }
}

/// Download `url` into `part`, resuming from its current length with Range.
pub async fn download(g: &UpdateServer, url: &str, part: &Path, size: u64) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    for _round in 0..3 {
        let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have == size {
            return Ok(());
        }
        let (from, truncate) = if have > size { (0, true) } else { (have, false) };
        let req = Req::get(url, Duration::from_secs(6 * 3600)).header("Range", format!("bytes={from}-"));
        let mut resp = g.send(&req).await?;
        let status = resp.status().as_u16();
        let mut file = tokio::fs::OpenOptions::new().create(true).write(true).append(!truncate).truncate(truncate).open(part).await?;
        match status {
            206 => {}
            200 => {
                // The server ignored the range: start over.
                file.set_len(0).await?;
            }
            416 => {
                file.set_len(0).await?;
                continue;
            }
            s => bail!("bundle download answered {s}"),
        }
        let mut since_sync = 0u64;
        loop {
            let chunk = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| anyhow::anyhow!("download stalled"))??;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).await?;
            since_sync += chunk.len() as u64;
            if since_sync >= FSYNC_EVERY {
                file.sync_data().await?;
                since_sync = 0;
            }
        }
        file.sync_data().await?;
    }
    let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have != size {
        bail!("bundle incomplete: {have}/{size} bytes");
    }
    Ok(())
}

/// Build `releases_dir/<version>` from the current tree + the bundle.
/// Blocking; runs in the background long before it is used.
pub fn stage(cfg: &AgentConfig, m: &Manifest, manifest_bytes: &[u8], sig: &str, bundle: &Path) -> Result<PathBuf> {
    let current = current_tree(cfg)?;
    let releases = &cfg.ota.releases_dir;
    std::fs::create_dir_all(releases)?;
    let staging = releases.join(format!(".staging-{}", m.version));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    // 1. Carry the current tree over as hard links (no data copied), except
    //    build leftovers and what the bundle fully owns (`mirror`, minus `preserve`).
    carry_tree(&current, &staging, Path::new(""), &cfg.ota.carry_exclude, &m.mirror, &m.preserve)?;
    // 2. Write the bundle over it, every file checked against the manifest.
    let wanted: BTreeMap<&str, &manifest::FileEntry> = m.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(std::fs::File::open(bundle)?));
    for entry in ar.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().trim_start_matches("./").to_string();
        match entry.header().entry_type() {
            tar::EntryType::Directory => continue,
            tar::EntryType::Regular => {}
            t => bail!("bundle entry {name} has a forbidden type {t:?}"),
        }
        let fe = *wanted.get(name.as_str()).with_context(|| format!("bundle entry {name} is not in the manifest"))?;
        let dest = match manifest::classify(&name)? {
            Dest::Tree(rel) => {
                if manifest::under_any(&rel, &m.preserve) {
                    bail!("bundle overwrites preserved path {rel}");
                }
                staging.join(&rel)
            }
            Dest::System(abs) => staging.join(OTA_DIR).join("system").join(abs.trim_start_matches('/')),
        };
        let owner_ref = match manifest::classify(&name)? {
            Dest::Tree(rel) => current.join(rel),
            Dest::System(abs) => PathBuf::from(abs),
        };
        write_checked(&mut entry, &dest, fe, &owner_ref)?;
        seen.insert(fe.path.clone());
    }
    if let Some(missing) = m.files.iter().find(|f| !seen.contains(&f.path)) {
        bail!("bundle is missing {}", missing.path);
    }
    // 3. Record what this tree is, then make it visible under its name.
    let ota = staging.join(OTA_DIR);
    std::fs::create_dir_all(&ota)?;
    std::fs::write(ota.join("manifest.json"), manifest_bytes)?;
    std::fs::write(ota.join("manifest.sig"), sig)?;
    // SAFETY: sync(2) has no preconditions; it flushes every filesystem.
    unsafe { libc::sync() };
    let dir = releases.join(&m.version);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::rename(&staging, &dir)?;
    util::fsync_dir(releases);
    Ok(dir)
}

/// Hard-link copy of `src` into `dst` (`rel` = path below the tree root).
fn carry_tree(src: &Path, dst: &Path, rel: &Path, exclude: &[String], mirror: &[String], preserve: &[String]) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    std::fs::create_dir_all(dst)?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    let _ = std::os::unix::fs::chown(dst, Some(meta.uid()), Some(meta.gid()));
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let r = rel.join(e.file_name());
        let rs = r.to_string_lossy();
        if manifest::under_any(&rs, exclude) {
            continue;
        }
        let ft = e.file_type()?;
        let to = dst.join(e.file_name());
        if ft.is_dir() {
            carry_tree(&e.path(), &to, &r, exclude, mirror, preserve)?;
        } else if manifest::under_any(&rs, mirror) && !manifest::under_any(&rs, preserve) {
            // Owned by the bundle: not carried (this is how files dropped from
            // a release disappear).
            continue;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &to)?;
        } else if ft.is_file() {
            std::fs::hard_link(e.path(), &to).with_context(|| format!("link {}", e.path().display()))?;
        }
    }
    Ok(())
}

/// Write one bundle file to `dest` through a temp file, checking size and
/// SHA-256, with the manifest mode and the owner of the file it replaces.
fn write_checked(r: &mut impl Read, dest: &Path, fe: &manifest::FileEntry, owner_ref: &Path) -> Result<()> {
    let parent = dest.parent().context("no parent")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.ota-tmp", dest.file_name().and_then(|n| n.to_str()).unwrap_or("f")));
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut n = 0u64;
    {
        let mut out = std::fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let k = r.read(&mut buf)?;
            if k == 0 {
                break;
            }
            ctx.update(&buf[..k]);
            out.write_all(&buf[..k])?;
            n += k as u64;
        }
    }
    let sha = util::hex(ctx.finish().as_ref());
    if sha != fe.sha256 || n != fe.size {
        let _ = std::fs::remove_file(&tmp);
        bail!("{} does not match the manifest (sha256/size)", fe.path);
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(fe.mode))?;
    let owner = std::fs::metadata(owner_ref).or_else(|_| std::fs::metadata(parent)).ok();
    if let Some(o) = owner {
        let _ = std::os::unix::fs::chown(&tmp, Some(o.uid()), Some(o.gid()));
    }
    // Replaces the hard link: the previous release keeps its own inode.
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

// ── Apply (boot) ────────────────────────────────────────────────────────────

/// Read and re-verify a staged/installed release's recorded manifest.
pub fn release_manifest(cfg: &AgentConfig, dir: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(dir.join(OTA_DIR).join("manifest.json"))?;
    let sig = std::fs::read_to_string(dir.join(OTA_DIR).join("manifest.sig"))?;
    manifest::open(&pubkey(cfg)?, &bytes, &sig, &cfg.ota.target)
}

/// Point `link` at `target` atomically (new symlink renamed over the old one).
pub fn swap_link(link: &Path, target: &Path) -> Result<()> {
    let tmp = link.with_extension("ota-new");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link)?;
    if let Some(p) = link.parent() {
        util::fsync_dir(p);
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Applied {
    pub version: String,
    pub units_changed: bool,
    pub rules_changed: bool,
}

/// Boot-time switch to the staged release. Local work only; returns quickly.
pub async fn apply_staged(cfg: &AgentConfig) -> Result<Option<Applied>> {
    let mut st = load_state(&cfg.state_dir);
    let Some(version) = st.staged.clone() else { return Ok(None) };
    if read_trial(&cfg.state_dir).is_some() {
        return Ok(None);
    }
    let dir = cfg.ota.releases_dir.join(&version);
    let prepared = (|| -> Result<(Manifest, PathBuf)> {
        let m = release_manifest(cfg, &dir)?;
        if m.version != version {
            bail!("staged directory holds {}, expected {version}", m.version);
        }
        if bad_versions(&cfg.state_dir).contains(&version) {
            bail!("{version} is marked bad");
        }
        // Cheap integrity check (sizes); the hashes were checked when staged.
        for f in &m.files {
            let p = match manifest::classify(&f.path)? {
                Dest::Tree(rel) => dir.join(rel),
                Dest::System(abs) => dir.join(OTA_DIR).join("system").join(abs.trim_start_matches('/')),
            };
            if std::fs::metadata(&p).map(|x| x.len()).ok() != Some(f.size) {
                bail!("staged file {} is missing or damaged", f.path);
            }
        }
        Ok((m, current_tree(cfg)?))
    })();
    let (m, previous) = match prepared {
        Ok(x) => x,
        Err(e) => {
            warn!(error = %e, "staged release unusable; dropping it");
            st.staged = None;
            st.last_update = Some(UpdateResult::new(&version, "failed", format!("{e:#}")));
            save_state(&cfg.state_dir, &st)?;
            return Ok(None);
        }
    };
    if previous == dir {
        st.staged = None;
        save_state(&cfg.state_dir, &st)?;
        return Ok(None);
    }
    let mut applied = Applied { version: version.clone(), ..Default::default() };
    install_system_files(&m, &dir, &mut applied)?;
    // The trial file goes first: if power is cut right after the swap, the
    // guard still knows a release is on trial.
    let trial = Trial { version: version.clone(), release: dir.clone(), previous, boots: 1 };
    util::atomic_write(&cfg.state_dir.join(TRIAL_FILE), trial.render().as_bytes())?;
    swap_link(&cfg.ota.install_link, &dir)?;
    reload_managers(&applied).await;
    st.staged = None;
    st.last_update = Some(UpdateResult::new(&version, "applied", "on trial"));
    save_state(&cfg.state_dir, &st)?;
    info!(version = %version, "release applied (on trial)");
    Ok(Some(applied))
}

fn install_system_files(m: &Manifest, dir: &Path, applied: &mut Applied) -> Result<()> {
    let ota = dir.join(OTA_DIR);
    let mut installed = String::new();
    for f in &m.files {
        let Dest::System(abs) = manifest::classify(&f.path)? else { continue };
        let src = ota.join("system").join(abs.trim_start_matches('/'));
        let dst = PathBuf::from(&abs);
        if std::fs::read(&src).ok() == std::fs::read(&dst).ok() {
            continue; // unchanged
        }
        if dst.exists() {
            let bak = ota.join("backup").join(abs.trim_start_matches('/'));
            std::fs::create_dir_all(bak.parent().context("no parent")?)?;
            std::fs::copy(&dst, &bak)?;
        }
        let parent = dst.parent().context("no parent")?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".{}.ota-tmp", dst.file_name().and_then(|n| n.to_str()).unwrap_or("f")));
        std::fs::copy(&src, &tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(f.mode))?;
        let _ = std::os::unix::fs::chown(&tmp, Some(0), Some(0));
        std::fs::rename(&tmp, &dst)?;
        installed.push_str(&abs);
        installed.push('\n');
        applied.units_changed |= abs.starts_with("/etc/systemd/");
        applied.rules_changed |= abs.starts_with("/etc/udev/");
    }
    util::atomic_write(&ota.join("installed.list"), installed.as_bytes())
}

async fn reload_managers(a: &Applied) {
    if a.units_changed {
        if let Err(e) = util::run("systemctl", &["daemon-reload"], SYSTEMCTL_TIMEOUT).await {
            warn!(error = %e, "systemctl daemon-reload failed");
        }
    }
    if a.rules_changed {
        if let Err(e) = util::run("udevadm", &["control", "--reload"], SYSTEMCTL_TIMEOUT).await {
            warn!(error = %e, "udevadm reload failed");
        }
    }
}

// ── Rollback ────────────────────────────────────────────────────────────────

/// Undo `trial`: system files restored, symlink back, version marked bad.
/// Services are NOT restarted here (the caller decides: at boot they have not
/// started yet; live, they must be).
pub async fn rollback(cfg: &AgentConfig, trial: &Trial, reason: &str) -> Result<()> {
    warn!(version = %trial.version, reason, "rolling back");
    let ota = trial.release.join(OTA_DIR);
    let mut a = Applied::default();
    for abs in std::fs::read_to_string(ota.join("installed.list")).unwrap_or_default().lines().filter(|l| !l.is_empty()) {
        if !manifest::system_path_allowed(abs) {
            continue;
        }
        let bak = ota.join("backup").join(abs.trim_start_matches('/'));
        if bak.exists() {
            let tmp = PathBuf::from(format!("{abs}.ota-tmp"));
            std::fs::copy(&bak, &tmp)?;
            std::fs::rename(&tmp, abs)?;
        } else {
            let _ = std::fs::remove_file(abs);
        }
        a.units_changed |= abs.starts_with("/etc/systemd/");
        a.rules_changed |= abs.starts_with("/etc/udev/");
    }
    swap_link(&cfg.ota.install_link, &trial.previous)?;
    reload_managers(&a).await;
    mark_bad(&cfg.state_dir, &trial.version)?;
    let _ = std::fs::remove_file(cfg.state_dir.join(TRIAL_FILE));
    let mut st = load_state(&cfg.state_dir);
    st.last_update = Some(UpdateResult::new(&trial.version, "rolled_back", reason));
    save_state(&cfg.state_dir, &st)
}

// ── Health ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub active_state: String,
    pub n_restarts: u64,
}

pub fn parse_show(out: &str) -> Sample {
    let mut s = Sample { active_state: String::new(), n_restarts: 0 };
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("ActiveState=") {
            s.active_state = v.trim().into();
        } else if let Some(v) = l.strip_prefix("NRestarts=") {
            s.n_restarts = v.trim().parse().unwrap_or(0);
        }
    }
    s
}

/// The verdict on one unit from samples taken across the settle window.
/// `active`: active in every sample and never restarted. `not-failed`: never
/// `failed` (restarts are its normal life). An empty series fails.
pub fn unit_verdict(check: &str, samples: &[Sample]) -> Result<(), String> {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else { return Err("no sample".into()) };
    if samples.iter().any(|s| s.active_state == "failed") {
        return Err("entered the failed state".into());
    }
    if check == "active" {
        if let Some(s) = samples.iter().find(|s| s.active_state != "active") {
            return Err(format!("was {} during the check", s.active_state));
        }
        if last.n_restarts > first.n_restarts {
            return Err(format!("restarted {} times during the check", last.n_restarts - first.n_restarts));
        }
    }
    Ok(())
}

/// What to do with a pending trial when the agent starts.
#[derive(Debug, PartialEq)]
pub enum TrialAction {
    None,
    /// Judge it (health checks).
    Judge,
    /// The link points elsewhere (manual change): forget the trial.
    Forget,
    /// Over the boot budget (the guard should have done it already).
    Rollback,
}

pub fn trial_action(trial: Option<&Trial>, current: Option<&Path>, max_boots: u32) -> TrialAction {
    let Some(t) = trial else { return TrialAction::None };
    if current != Some(t.release.as_path()) {
        return TrialAction::Forget;
    }
    if t.boots > max_boots { TrialAction::Rollback } else { TrialAction::Judge }
}

async fn sample(unit: &str) -> Sample {
    match util::run("systemctl", &["show", "-p", "ActiveState", "-p", "NRestarts", unit], Duration::from_secs(10)).await {
        Ok(o) => parse_show(&o.stdout),
        Err(_) => Sample { active_state: "unknown".into(), n_restarts: 0 },
    }
}

/// Watch the manifest's services for `settle_secs` and probe its URLs.
pub async fn health_check(m: &Manifest) -> Result<(), String> {
    let settle = Duration::from_secs(m.health.settle_secs.clamp(5, 600));
    let step = Duration::from_secs(5);
    let mut series: BTreeMap<&str, Vec<Sample>> = BTreeMap::new();
    let start = tokio::time::Instant::now();
    loop {
        for h in &m.health.services {
            series.entry(&h.unit).or_default().push(sample(&h.unit).await);
        }
        if start.elapsed() >= settle {
            break;
        }
        tokio::time::sleep(step).await;
    }
    for h in &m.health.services {
        unit_verdict(&h.check, series.get(h.unit.as_str()).map(Vec::as_slice).unwrap_or(&[])).map_err(|e| format!("{}: {e}", h.unit))?;
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
    for url in &m.health.http {
        let mut ok = false;
        for _ in 0..3 {
            if client.get(url).send().await.is_ok_and(|r| r.status().is_success()) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        if !ok {
            return Err(format!("{url} does not answer"));
        }
    }
    Ok(())
}

pub async fn restart_units(units: &[String]) {
    for u in units {
        match util::run("systemctl", &["restart", u], RESTART_TIMEOUT).await {
            Ok(o) if o.status == 0 => {}
            Ok(o) => warn!(unit = %u, err = %o.stderr.trim(), "restart failed"),
            Err(e) => warn!(unit = %u, error = %e, "restart failed"),
        }
    }
}

/// Units a live apply or rollback restarts: the manifest's list plus its
/// health-checked units, without duplicates and never the agent itself (it
/// is the one doing the restarting).
pub fn units_to_restart(m: &Manifest) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for u in m.restart.iter().chain(m.health.services.iter().map(|h| &h.unit)) {
        if !v.contains(u) && !u.starts_with("rascar-agent") {
            v.push(u.clone());
        }
    }
    v
}

/// Judge a pending trial: commit on success, roll back (and restart the
/// services on the previous release) on failure.
pub async fn judge(cfg: &AgentConfig, trial: &Trial) -> Result<UpdateResult> {
    let m = match release_manifest(cfg, &trial.release) {
        Ok(m) => m,
        Err(e) => {
            rollback(cfg, trial, &format!("manifest unreadable: {e:#}")).await?;
            return Ok(UpdateResult::new(&trial.version, "rolled_back", format!("{e:#}")));
        }
    };
    match health_check(&m).await {
        Ok(()) => {
            let _ = std::fs::remove_file(cfg.state_dir.join(TRIAL_FILE));
            let r = UpdateResult::new(&trial.version, "ok", "health checks passed");
            let mut st = load_state(&cfg.state_dir);
            st.last_update = Some(r.clone());
            save_state(&cfg.state_dir, &st)?;
            prune_releases(cfg);
            info!(version = %trial.version, "release committed");
            Ok(r)
        }
        Err(why) => {
            rollback(cfg, trial, &why).await?;
            // The services run the bad release: bring them back on the old one.
            let old = release_manifest(cfg, &trial.previous).ok();
            let units = old.as_ref().map(units_to_restart).unwrap_or_else(|| units_to_restart(&m));
            restart_units(&units).await;
            Ok(UpdateResult::new(&trial.version, "rolled_back", why))
        }
    }
}

/// Remove release trees beyond `keep_releases` (newest by mtime kept), never
/// the current one, the staged one, or a trial's previous one.
pub fn prune_releases(cfg: &AgentConfig) -> Vec<PathBuf> {
    let current = std::fs::canonicalize(&cfg.ota.install_link).ok();
    let staged = load_state(&cfg.state_dir).staged.map(|v| cfg.ota.releases_dir.join(v));
    let trial_prev = read_trial(&cfg.state_dir).map(|t| t.previous);
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&cfg.ota.releases_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    let is_protected = |d: &PathBuf| Some(d) == current.as_ref() || Some(d) == staged.as_ref() || Some(d) == trial_prev.as_ref();
    // Protected trees count toward the budget first; the newest others fill the rest.
    let mut kept = dirs.iter().filter(|(_, d)| is_protected(d)).count();
    let mut removed = Vec::new();
    for (_, d) in dirs {
        let stale_staging = d.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".staging-"));
        if is_protected(&d) {
            continue;
        }
        if !stale_staging && kept < cfg.ota.keep_releases {
            kept += 1;
            continue;
        }
        if std::fs::remove_dir_all(&d).is_ok() {
            removed.push(d);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BundleRef, FileEntry, Health, HealthService};

    fn s(state: &str, n: u64) -> Sample {
        Sample { active_state: state.into(), n_restarts: n }
    }

    #[test]
    fn health_verdicts() {
        assert!(unit_verdict("active", &[s("active", 0), s("active", 0)]).is_ok());
        assert!(unit_verdict("active", &[s("active", 0), s("active", 1)]).unwrap_err().contains("restarted"));
        assert!(unit_verdict("active", &[s("activating", 0), s("active", 0)]).is_err());
        assert!(unit_verdict("active", &[]).is_err());
        // A one-attempt-per-run unit restarts all the time; only `failed` counts.
        assert!(unit_verdict("not-failed", &[s("activating", 3), s("active", 9)]).is_ok());
        assert!(unit_verdict("not-failed", &[s("active", 3), s("failed", 3)]).is_err());
        assert_eq!(parse_show("NRestarts=4\nActiveState=active\n"), s("active", 4));
    }

    #[test]
    fn trial_decisions() {
        let t = Trial { version: "v2".into(), release: "/r/v2".into(), previous: "/r/v1".into(), boots: 1 };
        assert_eq!(trial_action(None, Some(Path::new("/r/v2")), 3), TrialAction::None);
        assert_eq!(trial_action(Some(&t), Some(Path::new("/r/v2")), 3), TrialAction::Judge);
        assert_eq!(trial_action(Some(&t), Some(Path::new("/r/v1")), 3), TrialAction::Forget);
        let late = Trial { boots: 4, ..t.clone() };
        assert_eq!(trial_action(Some(&late), Some(Path::new("/r/v2")), 3), TrialAction::Rollback);
        assert_eq!(Trial::parse(&t.render()), Some(t));
        assert_eq!(Trial::parse("version=x\n"), None);
    }

    fn offer_for(m: &Manifest) -> Offer {
        Offer {
            version: m.version.clone(),
            serial: m.serial,
            manifest: String::new(),
            signature: String::new(),
            bundle_size: m.bundle.size,
            bundle_url: String::new(),
        }
    }

    #[test]
    fn offers_are_refused_on_downgrade_bad_version_or_mismatch() {
        let m = crate::manifest::tests::sample();
        let o = offer_for(&m);
        assert_eq!(refuse_offer(&m, &o, 0, &[]), None);
        assert!(refuse_offer(&m, &o, m.serial, &[]).unwrap().contains("downgrade"));
        assert!(refuse_offer(&m, &o, 0, &[m.version.clone()]).is_some());
        let lying = Offer { serial: m.serial + 1, ..offer_for(&m) };
        assert!(refuse_offer(&m, &lying, 0, &[]).is_some());
    }

    #[test]
    fn restart_list_merges_and_skips_the_agent() {
        let mut m = crate::manifest::tests::sample();
        m.restart = vec!["rascar-app".into(), "rascar-agent".into()];
        m.health.services = vec![HealthService { unit: "rascar-app".into(), check: "active".into() }, HealthService { unit: "rascar-carplay".into(), check: "not-failed".into() }];
        assert_eq!(units_to_restart(&m), vec!["rascar-app".to_string(), "rascar-carplay".to_string()]);
    }

    // ── Staging + apply + rollback on a fake filesystem ─────────────────────

    struct Fx {
        _d: tempfile::TempDir,
        cfg: AgentConfig,
        root: PathBuf,
    }

    fn fixture() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let cfg = AgentConfig {
            state_dir: root.join("state"),
            ota: crate::config::OtaConfig {
                install_link: root.join("opt/orangecar"),
                releases_dir: root.join("opt/releases"),
                pubkey_file: root.join("ota.pub"),
                ..Default::default()
            },
            ..Default::default()
        };
        std::fs::create_dir_all(&cfg.state_dir).unwrap();
        std::fs::write(&cfg.ota.pubkey_file, manifest::public_key_hex(crate::manifest::tests::SEED).unwrap()).unwrap();
        // The migrated base tree.
        let base = cfg.ota.releases_dir.join("base");
        for (p, c) in [
            ("target/release/rascar-app", "old-app"),
            ("app/frontend/index.html", "old-index"),
            ("app/frontend/gone.js", "dropped in the new release"),
            ("app/chromium-ext/frame-unlock/_metadata/rules.json", "chromium-made"),
            ("bridge/certs/headunit.key", "board-only"),
            ("target/release/deps/junk.rlib", "build leftover"),
        ] {
            std::fs::create_dir_all(base.join(p).parent().unwrap()).unwrap();
            std::fs::write(base.join(p), c).unwrap();
        }
        std::fs::create_dir_all(root.join("opt")).unwrap();
        std::os::unix::fs::symlink(&base, &cfg.ota.install_link).unwrap();
        Fx { _d: d, cfg, root }
    }

    /// Build bundle + signed manifest the way tools/release.sh does.
    fn build_release(fx: &Fx, version: &str, serial: u64, files: &[(&str, &[u8], u32)]) -> (Manifest, Vec<u8>, String, PathBuf) {
        let bundle = fx.root.join(format!("{version}.tar.gz"));
        let mut tb = tar::Builder::new(flate2::write::GzEncoder::new(std::fs::File::create(&bundle).unwrap(), flate2::Compression::fast()));
        let mut entries = Vec::new();
        for (path, data, mode) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            tb.append_data(&mut h, path, *data).unwrap();
            entries.push(FileEntry { path: path.to_string(), sha256: util::sha256_bytes(data), size: data.len() as u64, mode: *mode });
        }
        tb.into_inner().unwrap().finish().unwrap();
        let m = Manifest {
            format: 1,
            version: version.into(),
            serial,
            git_sha: "abc".into(),
            created: "now".into(),
            target: "orangecar".into(),
            bundle: BundleRef { size: std::fs::metadata(&bundle).unwrap().len(), sha256: util::sha256_file(&bundle).unwrap() },
            files: entries,
            mirror: vec!["app/frontend".into(), "app/chromium-ext".into()],
            preserve: vec!["bridge/certs".into(), "app/chromium-ext/frame-unlock/_metadata".into()],
            restart: vec!["rascar-app".into()],
            health: Health { services: vec![], http: vec![], settle_secs: 5 },
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = manifest::sign(crate::manifest::tests::SEED, &bytes).unwrap();
        (m, bytes, sig, bundle)
    }

    #[test]
    fn staging_builds_a_complete_tree_and_keeps_board_files() {
        let fx = fixture();
        let (m, bytes, sig, bundle) = build_release(
            &fx,
            "v2",
            2,
            &[
                ("tree/target/release/rascar-app", b"new-app", 0o755),
                ("tree/app/frontend/index.html", b"new-index", 0o644),
                ("tree/app/chromium-ext/frame-unlock/manifest.json", b"{}", 0o644),
            ],
        );
        let dir = stage(&fx.cfg, &m, &bytes, &sig, &bundle).unwrap();
        let rd = |p: &str| std::fs::read_to_string(dir.join(p)).ok();
        assert_eq!(rd("target/release/rascar-app").as_deref(), Some("new-app"));
        assert_eq!(rd("app/frontend/index.html").as_deref(), Some("new-index"));
        assert_eq!(rd("app/frontend/gone.js"), None, "mirrored dir: files dropped from the release disappear");
        assert_eq!(rd("bridge/certs/headunit.key").as_deref(), Some("board-only"));
        assert_eq!(rd("app/chromium-ext/frame-unlock/_metadata/rules.json").as_deref(), Some("chromium-made"));
        assert_eq!(rd("target/release/deps/junk.rlib"), None, "build leftovers are not carried");
        assert_eq!(std::fs::metadata(dir.join("target/release/rascar-app")).unwrap().mode() & 0o777, 0o755);
        // The old release is untouched (hard links were replaced, not written through).
        let base = fx.cfg.ota.releases_dir.join("base");
        assert_eq!(std::fs::read_to_string(base.join("target/release/rascar-app")).unwrap(), "old-app");
        assert_eq!(std::fs::read_to_string(base.join("app/frontend/gone.js")).unwrap(), "dropped in the new release");
        // Carried files are hard links, not copies.
        assert_eq!(
            std::fs::metadata(dir.join("bridge/certs/headunit.key")).unwrap().ino(),
            std::fs::metadata(base.join("bridge/certs/headunit.key")).unwrap().ino()
        );
    }

    #[test]
    fn staging_refuses_a_bundle_that_disagrees_with_its_manifest() {
        let fx = fixture();
        let (mut m, _, _, bundle) = build_release(&fx, "v2", 2, &[("tree/target/release/rascar-app", b"new-app", 0o755)]);
        m.files[0].sha256 = "0".repeat(64);
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = manifest::sign(crate::manifest::tests::SEED, &bytes).unwrap();
        assert!(stage(&fx.cfg, &m, &bytes, &sig, &bundle).unwrap_err().to_string().contains("does not match"));
        assert!(!fx.cfg.ota.releases_dir.join("v2").exists(), "nothing half-built is left under the version name");
        // An extra, unlisted file in the bundle.
        let (mut m, _, _, bundle) = build_release(&fx, "v3", 3, &[("tree/a", b"1", 0o644), ("tree/b", b"2", 0o644)]);
        m.files.pop();
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = manifest::sign(crate::manifest::tests::SEED, &bytes).unwrap();
        assert!(stage(&fx.cfg, &m, &bytes, &sig, &bundle).is_err());
    }

    #[tokio::test]
    async fn apply_then_rollback_restores_the_previous_tree() {
        let fx = fixture();
        let (m, bytes, sig, bundle) = build_release(&fx, "v2", 2, &[("tree/target/release/rascar-app", b"new-app", 0o755)]);
        let dir = stage(&fx.cfg, &m, &bytes, &sig, &bundle).unwrap();
        let mut st = OtaState { staged: Some("v2".into()), ..Default::default() };
        save_state(&fx.cfg.state_dir, &st).unwrap();

        let applied = apply_staged(&fx.cfg).await.unwrap().expect("applied");
        assert_eq!(applied.version, "v2");
        assert_eq!(std::fs::canonicalize(&fx.cfg.ota.install_link).unwrap(), dir);
        assert_eq!(installed(&fx.cfg), ("v2".into(), 2));
        let trial = read_trial(&fx.cfg.state_dir).unwrap();
        assert_eq!(trial.boots, 1);
        assert_eq!(trial.previous, fx.cfg.ota.releases_dir.join("base"));
        // A second apply while on trial does nothing.
        st.staged = Some("v2".into());
        save_state(&fx.cfg.state_dir, &st).unwrap();
        assert!(apply_staged(&fx.cfg).await.unwrap().is_none());

        rollback(&fx.cfg, &trial, "test").await.unwrap();
        assert_eq!(std::fs::canonicalize(&fx.cfg.ota.install_link).unwrap(), fx.cfg.ota.releases_dir.join("base"));
        assert!(read_trial(&fx.cfg.state_dir).is_none());
        assert_eq!(bad_versions(&fx.cfg.state_dir), vec!["v2".to_string()]);
        assert_eq!(load_state(&fx.cfg.state_dir).last_update.unwrap().result, "rolled_back");
        assert_eq!(installed(&fx.cfg), ("base".into(), 0));
    }

    #[tokio::test]
    async fn a_damaged_staged_release_is_dropped_not_applied() {
        let fx = fixture();
        let (m, bytes, sig, bundle) = build_release(&fx, "v2", 2, &[("tree/target/release/rascar-app", b"new-app", 0o755)]);
        let dir = stage(&fx.cfg, &m, &bytes, &sig, &bundle).unwrap();
        std::fs::write(dir.join("target/release/rascar-app"), b"x").unwrap(); // truncated by a power cut
        save_state(&fx.cfg.state_dir, &OtaState { staged: Some("v2".into()), ..Default::default() }).unwrap();
        assert!(apply_staged(&fx.cfg).await.unwrap().is_none());
        assert_eq!(std::fs::canonicalize(&fx.cfg.ota.install_link).unwrap(), fx.cfg.ota.releases_dir.join("base"));
        assert!(load_state(&fx.cfg.state_dir).staged.is_none());
        // A staged tree whose manifest was re-signed by another key is refused too.
        let other = "01".repeat(32);
        std::fs::write(dir.join(".ota/manifest.sig"), manifest::sign(&other, &bytes).unwrap()).unwrap();
        assert!(release_manifest(&fx.cfg, &dir).is_err());
    }

    #[test]
    fn prune_keeps_current_staged_and_newest() {
        let fx = fixture();
        let mut cfg = fx.cfg.clone();
        cfg.ota.keep_releases = 1;
        for v in ["a", "b", ".staging-c"] {
            std::fs::create_dir_all(cfg.ota.releases_dir.join(v)).unwrap();
        }
        save_state(&cfg.state_dir, &OtaState { staged: Some("a".into()), ..Default::default() }).unwrap();
        let removed = prune_releases(&cfg);
        assert!(cfg.ota.releases_dir.join("base").exists(), "current");
        assert!(cfg.ota.releases_dir.join("a").exists(), "staged");
        assert!(removed.contains(&cfg.ota.releases_dir.join(".staging-c")));
        assert!(removed.contains(&cfg.ota.releases_dir.join("b")));
    }

    #[test]
    fn current_tree_requires_the_migration() {
        let fx = fixture();
        let mut cfg = fx.cfg.clone();
        cfg.ota.install_link = fx.root.join("plain-dir");
        std::fs::create_dir_all(&cfg.ota.install_link).unwrap();
        assert!(current_tree(&cfg).unwrap_err().to_string().contains("not a symlink"));
    }
}
