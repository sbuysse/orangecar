//! 4G data usage: per-day byte totals of the modem's netdev, the allowance per
//! billing cycle, and the user's controls over the 4G link (on/off, reconnect,
//! cut at the allowance, OTA / log uploads over 4G).
//!
//! Accounting. The Pi reboots on every drive (switched 12 V socket) and the
//! kernel counters restart at 0 with it, so nothing but this module keeps a
//! history. Every [`TICK`] the netdev's `statistics/{rx,tx}_bytes` are read
//! and the increase since the previous reading is added to the local day.
//! The previous reading (the "baseline": boot id + ifindex + counters) is
//! stored in the same transaction as the totals, so after an app restart in
//! the same boot (an OTA apply restarts us) counting resumes from exactly
//! what is already in the database: no double count, no gap. A different
//! boot id or ifindex means fresh kernel counters, which started at 0 and are
//! therefore counted whole. Writes happen every [`FLUSH_EVERY`] and on park
//! sync, so a power cut loses at most that much traffic.
//!
//! Controls act on `rascar-4g-qmi.service` (root, through `sudo -n`). The unit
//! is started by udev whenever the modem's QMI port appears, i.e. at every
//! boot, so "4G off" is not a one-shot stop: the tick re-applies it whenever
//! the dial service is found running while it should not be.

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{info, warn};

use super::settings::SettingsService;

/// Counter sampling period.
const TICK: Duration = Duration::from_secs(10);
/// Longest stretch of counted traffic kept only in memory (lost on a power cut).
const FLUSH_EVERY: Duration = Duration::from_secs(60);
/// Minimum gap between two automatic "keep 4G off" enforcements: a failing
/// sudo must not turn into a process spawn every tick.
const ENFORCE_GAP: Duration = Duration::from_secs(30);
/// Any blocking database job; the API answers an error rather than hang.
const DB_TIMEOUT: Duration = Duration::from_secs(5);
/// One root helper call (systemctl stop waits for the dial script to exit).
const CMD_TIMEOUT: Duration = Duration::from_secs(20);

/// The SIM7600 dial service (setup/4g/rascar-4g-qmi.service).
pub const DIAL_UNIT: &str = "rascar-4g-qmi.service";
const STATUS_FILE: &str = "/run/rascar/4g.status";
/// Touched by systemd-timesyncd once the clock is set from the network. The
/// Pi has no RTC: until then the clock is the last saved one, possibly a day
/// or more behind, and traffic must not be filed under that stale date.
const TIME_SYNCED_FLAG: &str = "/run/systemd/timesync/synchronized";
/// Operators sell decimal gigabytes.
const GB: f64 = 1e9;
/// Status-bar warning threshold, share of the allowance.
const WARN_SHARE: f64 = 0.8;
/// Key of our settings in settings.json's free-form `extra` map: keeps the
/// typed `AppSettings` (and the `/api/settings` handler that rebuilds it field
/// by field) untouched, and `/api/settings` preserves `extra` as is.
const SETTINGS_KEY: &str = "cell_data";

// ── Settings ──────────────────────────────────────────────────

/// `settings.json` → `cell_data`. rascar-agent is meant to read
/// `ota_over_4g` / `logs_over_4g` from here (see the agent note in the summary
/// of this feature); the app only stores them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CellDataSettings {
    /// The user wants mobile data (4G off = dial service stopped, link down).
    pub enabled: bool,
    /// Allowance per billing cycle in GB (decimal), 0 = none.
    pub allowance_gb: f64,
    /// Day of the month the operator's billing cycle starts (1-28: every
    /// month has it). The allowance, warning, cut and the per-cycle table
    /// follow this cycle, not calendar months.
    pub cycle_start_day: u32,
    /// Stop the dial service once the cycle's usage reaches the allowance.
    pub cut_at_limit: bool,
    pub ota_over_4g: bool,
    pub logs_over_4g: bool,
    /// Cycle (its start, "YYYY-MM-DD") in which the user turned 4G back on
    /// after the allowance cut: the cut does not fire again in that cycle.
    pub cap_override_cycle: Option<String>,
}

impl Default for CellDataSettings {
    fn default() -> Self {
        // OTA defaults on, like rascar-agent's downloadOnMetered: the car is
        // rarely on Wi-Fi and 4G is how releases normally reach it.
        // No allowance until the user sets their plan's (Settings > 4G);
        // cycles start on the 1st by default.
        Self {
            enabled: true,
            allowance_gb: 0.0,
            cycle_start_day: 1,
            cut_at_limit: false,
            ota_over_4g: true,
            logs_over_4g: true,
            cap_override_cycle: None,
        }
    }
}

impl CellDataSettings {
    fn allowance_bytes(&self) -> Option<u64> {
        (self.allowance_gb > 0.0).then_some((self.allowance_gb * GB) as u64)
    }

    fn start_day(&self) -> u32 {
        self.cycle_start_day.clamp(1, 28)
    }

    /// The allowance cut is in force in `cycle` (its start key).
    pub fn cap_blocked(&self, cycle: &str, cycle_bytes: u64) -> bool {
        self.cut_at_limit
            && self.allowance_bytes().is_some_and(|a| cycle_bytes >= a)
            && self.cap_override_cycle.as_deref() != Some(cycle)
    }

    pub fn want_on(&self, cycle: &str, cycle_bytes: u64) -> bool {
        self.enabled && !self.cap_blocked(cycle, cycle_bytes)
    }
}

// ── Counter deltas (pure, unit-tested) ────────────────────────

/// The last counter reading already accounted for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub boot_id: String,
    pub ifindex: u32,
    pub rx: u64,
    pub tx: u64,
}

/// One reading of the 4G netdev.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub ifindex: u32,
    pub rx: u64,
    pub tx: u64,
}

/// Bytes moved since `prev`. Kernel counters start at 0 when an interface is
/// created, so whenever `prev` does not describe the same interface in the same
/// boot (first run, reboot, USB reset re-creating the netdev) the reading is
/// counted whole. A rename keeps the ifindex and the counters, hence the
/// ifindex (not the name) as identity. A decrease on the same interface cannot
/// come from the kernel's 64-bit counters except through a reset: counted whole.
pub fn delta(prev: Option<&Baseline>, boot_id: &str, s: &Sample) -> (u64, u64) {
    match prev {
        Some(p) if p.boot_id == boot_id && p.ifindex == s.ifindex && s.rx >= p.rx && s.tx >= p.tx => (s.rx - p.rx, s.tx - p.tx),
        _ => (s.rx, s.tx),
    }
}

/// The dial service's status line → (state, device, ip).
fn parse_status(text: &str) -> (String, Option<String>, Option<String>) {
    let mut words = text.lines().next().unwrap_or("").split_whitespace();
    let first = words.next().unwrap_or("").to_string();
    if first == "connected" {
        (first, words.next().map(str::to_string), words.next().map(str::to_string))
    } else {
        (text.lines().next().unwrap_or("").trim().to_string(), None, None)
    }
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn read_sample(class_net: &Path, name: &str) -> Option<Sample> {
    let dir = class_net.join(name);
    Some(Sample {
        name: name.to_string(),
        ifindex: read_u64(&dir.join("ifindex"))? as u32,
        rx: read_u64(&dir.join("statistics/rx_bytes"))?,
        tx: read_u64(&dir.join("statistics/tx_bytes"))?,
    })
}

/// The 4G netdev: the dial service's device when it names one that exists,
/// else the first `ww*` interface (qmi_wwan's `wwan0` before udev renames it
/// to `wwu1i5`-style names, which also start with "ww").
pub fn find_sample(class_net: &Path, status_dev: Option<&str>) -> Option<Sample> {
    if let Some(s) = status_dev.and_then(|d| read_sample(class_net, d)) {
        return Some(s);
    }
    let mut names: Vec<String> = std::fs::read_dir(class_net)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ww"))
        .collect();
    names.sort();
    names.iter().find_map(|n| read_sample(class_net, n))
}

fn iface_up(class_net: &Path, name: &str) -> bool {
    std::fs::read_to_string(class_net.join(name).join("flags"))
        .ok()
        .and_then(|f| u32::from_str_radix(f.trim().trim_start_matches("0x"), 16).ok())
        .is_some_and(|f| f & 1 != 0) // IFF_UP
}

fn ww_ifaces(class_net: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(class_net)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("ww")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

// ── Day keys ──────────────────────────────────────────────────

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn ymd(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Start of the billing cycle containing `d`, for cycles starting on `start_day` (1-28).
pub fn cycle_start(d: chrono::NaiveDate, start_day: u32) -> chrono::NaiveDate {
    use chrono::Datelike;
    let this = d.with_day(start_day).unwrap_or(d);
    if d.day() >= start_day { this } else { this.checked_sub_months(chrono::Months::new(1)).unwrap_or(this) }
}

/// Last day of the cycle starting on `start`.
pub fn cycle_end(start: chrono::NaiveDate) -> chrono::NaiveDate {
    start.checked_add_months(chrono::Months::new(1)).unwrap_or(start) - chrono::Duration::days(1)
}

/// Starts of the `n` cycles ending with the one containing `last`, oldest first.
fn last_cycles(last: chrono::NaiveDate, start_day: u32, n: u32) -> Vec<chrono::NaiveDate> {
    let cur = cycle_start(last, start_day);
    (0..n).rev().map(|i| cur.checked_sub_months(chrono::Months::new(i)).unwrap_or(cur)).collect()
}

/// The `n` days ending with `last`, oldest first.
fn last_days(last: chrono::NaiveDate, n: i64) -> Vec<String> {
    (0..n).rev().map(|i| (last - chrono::Duration::days(i)).format("%Y-%m-%d").to_string()).collect()
}


// ── Storage ───────────────────────────────────────────────────

fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let c = Connection::open(path)?;
    // synchronous=FULL: a commit happens once a minute at most, and the whole
    // point of it is to survive the power cut that follows parking.
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS usage_day(day TEXT PRIMARY KEY, rx INTEGER NOT NULL DEFAULT 0, tx INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE IF NOT EXISTS state(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(c)
}

fn load_baseline(c: &Connection) -> Option<Baseline> {
    let v: String = c.query_row("SELECT value FROM state WHERE key='baseline'", [], |r| r.get(0)).ok()?;
    serde_json::from_str(&v).ok()
}

fn max_day(c: &Connection) -> Option<String> {
    c.query_row("SELECT max(day) FROM usage_day", [], |r| r.get::<_, Option<String>>(0)).ok().flatten()
}

/// Add `pending` to the totals and record `baseline`, atomically.
fn commit(c: &mut Connection, pending: &BTreeMap<String, (u64, u64)>, baseline: Option<&Baseline>) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    for (day, (rx, tx_b)) in pending {
        tx.execute(
            "INSERT INTO usage_day(day, rx, tx) VALUES (?1, ?2, ?3)
             ON CONFLICT(day) DO UPDATE SET rx = rx + excluded.rx, tx = tx + excluded.tx",
            params![day, *rx as i64, *tx_b as i64],
        )?;
    }
    if let Some(b) = baseline {
        tx.execute(
            "INSERT OR REPLACE INTO state(key, value) VALUES ('baseline', ?1)",
            params![serde_json::to_string(b).unwrap_or_default()],
        )?;
    }
    tx.commit()
}

// ── Service ───────────────────────────────────────────────────

#[derive(Default)]
struct Inner {
    baseline: Option<Baseline>,
    /// Counted traffic not yet in the database, per day.
    pending: BTreeMap<String, (u64, u64)>,
    /// Counted while the clock was not network-synced: filed at flush time.
    unsynced: (u64, u64),
    /// The baseline changed since the last commit.
    dirty: bool,
    last_flush: Option<Instant>,
    /// (link identity "dev ip", unix time it was first seen connected).
    link_since: Option<(String, i64)>,
    last_enforce: Option<Instant>,
    /// We stopped the dial service for the allowance cut (restart it when the
    /// cut lifts: new cycle, allowance raised, cut disabled).
    stopped_by_cap: bool,
    /// Last control action, shown in the panel.
    last_action: Option<String>,
}

pub struct DataUsage {
    inner: Mutex<Inner>,
    db: Arc<Mutex<Option<Connection>>>,
    db_path: PathBuf,
    class_net: PathBuf,
    status_file: PathBuf,
    boot_id: String,
    settings: Arc<SettingsService>,
    /// Serialises control actions (a stop racing a start would leave anything).
    ctl: tokio::sync::Mutex<()>,
}

static GLOBAL: OnceLock<Arc<DataUsage>> = OnceLock::new();

/// Park-sync hook: commit what is counted before the car cuts our power.
pub async fn flush_for_power_loss() {
    if let Some(du) = GLOBAL.get() {
        du.sample_and_flush(true).await;
    }
}

impl DataUsage {
    pub fn new(settings: Arc<SettingsService>) -> Arc<Self> {
        let db_path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root")).join(".config/rascar/datausage.db");
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default();
        Self::with_paths(settings, db_path, PathBuf::from("/sys/class/net"), PathBuf::from(STATUS_FILE), boot_id)
    }

    fn with_paths(settings: Arc<SettingsService>, db_path: PathBuf, class_net: PathBuf, status_file: PathBuf, boot_id: String) -> Arc<Self> {
        let conn = match open_db(&db_path) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("data usage: cannot open {}: {e} — counting in memory only", db_path.display());
                None
            }
        };
        let baseline = conn.as_ref().and_then(load_baseline);
        Arc::new(Self {
            inner: Mutex::new(Inner { baseline, ..Default::default() }),
            db: Arc::new(Mutex::new(conn)),
            db_path,
            class_net,
            status_file,
            boot_id,
            settings,
            ctl: tokio::sync::Mutex::new(()),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let _ = GLOBAL.set(self.clone());
        let me = self.clone();
        tokio::spawn(async move {
            info!("data usage: counting 4G traffic into {}", me.db_path.display());
            loop {
                me.sample_and_flush(false).await;
                me.enforce().await;
                tokio::time::sleep(TICK).await;
            }
        });
    }

    fn status(&self) -> (String, Option<String>, Option<String>) {
        std::fs::read_to_string(&self.status_file).map(|t| parse_status(&t)).unwrap_or_else(|_| (String::new(), None, None))
    }

    /// Read the counters, account the increase, and commit when due (or `force`).
    async fn sample_and_flush(&self, force: bool) {
        let (state, dev, ip) = self.status();
        let sample = find_sample(&self.class_net, dev.as_deref());
        let synced = Path::new(TIME_SYNCED_FLAG).exists();
        let job = {
            let mut g = self.inner.lock().unwrap();
            if let Some(s) = &sample {
                let (drx, dtx) = delta(g.baseline.as_ref(), &self.boot_id, s);
                if drx > 0 || dtx > 0 {
                    if synced {
                        let e = g.pending.entry(today()).or_default();
                        e.0 += drx;
                        e.1 += dtx;
                    } else {
                        g.unsynced.0 += drx;
                        g.unsynced.1 += dtx;
                    }
                }
                let b = Baseline { boot_id: self.boot_id.clone(), ifindex: s.ifindex, rx: s.rx, tx: s.tx };
                if g.baseline.as_ref() != Some(&b) {
                    g.baseline = Some(b);
                    g.dirty = true;
                }
            }
            // "Connected since": first time this dev+ip pair was seen connected.
            if state == "connected" {
                let key = format!("{} {}", dev.as_deref().unwrap_or(""), ip.as_deref().unwrap_or(""));
                if g.link_since.as_ref().map(|(k, _)| k) != Some(&key) {
                    g.link_since = Some((key, chrono::Utc::now().timestamp()));
                }
            } else {
                g.link_since = None;
            }
            let due = force || g.last_flush.is_none_or(|t| t.elapsed() >= FLUSH_EVERY);
            if due && (g.dirty || !g.pending.is_empty() || g.unsynced != (0, 0)) {
                g.last_flush = Some(Instant::now());
                g.dirty = false;
                Some((std::mem::take(&mut g.pending), std::mem::take(&mut g.unsynced), g.baseline.clone()))
            } else {
                None
            }
        };
        let Some((mut pending, unsynced, baseline)) = job else { return };
        let db = self.db.clone();
        // Not bounded by a timeout on purpose: a dropped wait would leave the
        // commit running while its bytes were handed back to memory (counted
        // twice). The caller that must not wait (park sync) bounds itself.
        let res = tokio::task::spawn_blocking(move || {
            let mut guard = db.lock().unwrap();
            let Some(c) = guard.as_mut() else { return (Err("no database".to_string()), pending) };
            if unsynced != (0, 0) {
                // Clock not set from the network yet: the restored clock lags,
                // so never file before the newest day already recorded.
                let day = if Path::new(TIME_SYNCED_FLAG).exists() { today() } else { max_day(c).map_or_else(today, |d| d.max(today())) };
                let e = pending.entry(day).or_default();
                e.0 += unsynced.0;
                e.1 += unsynced.1;
            }
            let r = commit(c, &pending, baseline.as_ref()).map_err(|e| e.to_string());
            (r, pending)
        })
        .await;
        match res {
            Ok((Ok(()), _)) => {}
            Ok((Err(e), back)) => {
                // Keep the bytes in memory: they go out with the next flush.
                warn!("data usage: commit failed: {e}");
                let mut g = self.inner.lock().unwrap();
                g.dirty = true;
                for (day, (rx, tx)) in back {
                    let e = g.pending.entry(day).or_default();
                    e.0 += rx;
                    e.1 += tx;
                }
            }
            Err(e) => warn!("data usage: commit task: {e}"),
        }
    }

    fn cell_settings(s: &super::settings::AppSettings) -> CellDataSettings {
        s.extra.get(SETTINGS_KEY).and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default()
    }

    async fn load_settings(&self) -> CellDataSettings {
        let s = self.settings.settings.read().await.clone();
        Self::cell_settings(&s)
    }

    async fn save_settings(&self, cs: &CellDataSettings) -> anyhow::Result<()> {
        let mut s = self.settings.settings.read().await.clone();
        s.extra.insert(SETTINGS_KEY.to_string(), serde_json::to_value(cs)?);
        self.settings.save(s).await
    }

    /// The current cycle's (key, total), database + memory.
    async fn cycle_bytes(&self, cs: &CellDataSettings) -> (String, u64) {
        match self.summary(cs.start_day()).await {
            Ok(s) => (s.cycle.key.clone(), s.cycle.rx + s.cycle.tx),
            Err(_) => (ymd(cycle_start(chrono::Local::now().date_naive(), cs.start_day())), 0),
        }
    }

    /// Keep the link in the state the settings ask for. The dial service is
    /// started by udev at every boot (and on any re-enumeration of the
    /// modem), so "off" has to be re-applied, not set once.
    async fn enforce(&self) {
        let cs = self.load_settings().await;
        let (cycle, used) = self.cycle_bytes(&cs).await;
        let want_on = cs.want_on(&cycle, used);
        let (state, _, _) = self.status();
        // ExecStopPost writes "no modem": that line (or no file) = not running.
        let running = !(state.is_empty() || state == "no modem");
        let up = ww_ifaces(&self.class_net).iter().any(|n| iface_up(&self.class_net, n));
        let (recently, stopped_by_cap) = {
            let g = self.inner.lock().unwrap();
            (g.last_enforce.is_some_and(|t| t.elapsed() < ENFORCE_GAP), g.stopped_by_cap)
        };
        if recently {
            return;
        }
        if !want_on && (running || up) {
            let why = if cs.enabled { "allowance reached" } else { "4G turned off" };
            info!("data usage: {why} — stopping {DIAL_UNIT}");
            self.inner.lock().unwrap().last_enforce = Some(Instant::now());
            let r = self.link_off().await;
            let mut g = self.inner.lock().unwrap();
            g.stopped_by_cap = cs.enabled;
            g.last_action = Some(format!("{why}: {}", r.detail));
        } else if want_on && stopped_by_cap && !running {
            info!("data usage: allowance cut lifted — starting {DIAL_UNIT}");
            self.inner.lock().unwrap().last_enforce = Some(Instant::now());
            let r = self.link_on().await;
            let mut g = self.inner.lock().unwrap();
            g.stopped_by_cap = !r.ok;
            g.last_action = Some(format!("allowance cut lifted: {}", r.detail));
        }
    }

    // ── Root actions ──

    /// Stop the dial service, then take every ww* link down: stopping the
    /// script leaves the modem's data session up with the address and default
    /// route still on the netdev (the script has no stop handler), so traffic
    /// would keep flowing. Link down removes the route; on the next start the
    /// script finds the session up but the link unusable and re-applies it.
    async fn link_off(&self) -> ActionResult {
        let _g = self.ctl.lock().await;
        let mut notes = Vec::new();
        let stop = sudo(&["systemctl", "stop", DIAL_UNIT]).await;
        if let Err(e) = &stop {
            notes.push(format!("stop failed: {e}"));
        }
        for dev in ww_ifaces(&self.class_net) {
            if !iface_up(&self.class_net, &dev) {
                continue;
            }
            if let Err(e) = sudo(&["ip", "link", "set", dev.as_str(), "down"]).await {
                notes.push(format!("{dev} down failed: {e}"));
            }
        }
        // Read back: a stop that did not take must not be reported as done.
        let active = unit_state().await;
        let still_up: Vec<String> = ww_ifaces(&self.class_net).into_iter().filter(|d| iface_up(&self.class_net, d)).collect();
        let ok = active.as_deref() != Some("active") && still_up.is_empty();
        let detail = if ok && notes.is_empty() {
            "4G off (dial service stopped, link down)".to_string()
        } else if ok {
            // Off as read back, but say what did not go to plan.
            format!("4G off ({})", notes.join("; "))
        } else {
            format!("4G NOT fully off: unit {}, up: [{}]{}", active.as_deref().unwrap_or("?"), still_up.join(","),
                if notes.is_empty() { String::new() } else { format!(" — {}", notes.join("; ")) })
        };
        if ok {
            info!("data usage: {detail}");
        } else {
            warn!("data usage: {detail}");
        }
        ActionResult { ok, detail }
    }

    async fn link_on(&self) -> ActionResult {
        let _g = self.ctl.lock().await;
        let r = sudo(&["systemctl", "start", DIAL_UNIT]).await;
        let active = unit_state().await;
        let ok = r.is_ok() && active.as_deref() == Some("active");
        let detail = match (&r, ok) {
            (_, true) => "4G on (dialing)".to_string(),
            (Err(e), _) => format!("start failed: {e}"),
            (Ok(_), false) => format!("dial service is {}", active.as_deref().unwrap_or("?")),
        };
        if ok {
            info!("data usage: {detail}");
        } else {
            warn!("data usage: {detail}");
        }
        ActionResult { ok, detail }
    }

    // ── Reports ──

    async fn summary(&self, start_day: u32) -> Result<Summary, String> {
        let (pending, unsynced) = {
            let g = self.inner.lock().unwrap();
            (g.pending.clone(), g.unsynced)
        };
        let db = self.db.clone();
        run_blocking(move || {
            let today_s = today();
            let today_d = chrono::Local::now().date_naive();
            let days = last_days(today_d, 30);
            let cycles = last_cycles(today_d, start_day, 12);
            let mut per_day: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            let mut first_day: Option<String> = None;
            let guard = db.lock().unwrap();
            if let Some(c) = guard.as_ref() {
                first_day = c.query_row("SELECT min(day) FROM usage_day", [], |r| r.get::<_, Option<String>>(0)).ok().flatten();
                let mut st = c.prepare("SELECT day, rx, tx FROM usage_day WHERE day >= ?1").map_err(|e| e.to_string())?;
                let rows = st
                    .query_map(params![ymd(cycles[0])], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
                    .map_err(|e| e.to_string())?;
                for (d, rx, tx) in rows.flatten() {
                    per_day.insert(d, (rx.max(0) as u64, tx.max(0) as u64));
                }
            }
            drop(guard);
            for (d, (rx, tx)) in pending.iter().chain(std::iter::once((&today_s, &unsynced))) {
                if *rx == 0 && *tx == 0 {
                    continue;
                }
                if first_day.as_ref().is_none_or(|f| d < f) {
                    first_day = Some(d.clone());
                }
                let e = per_day.entry(d.clone()).or_default();
                e.0 += rx;
                e.1 += tx;
            }
            let get = |d: &str| per_day.get(d).copied().unwrap_or_default();
            let mut by_cycle: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for (d, (rx, tx)) in &per_day {
                let Ok(date) = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") else { continue };
                let e = by_cycle.entry(ymd(cycle_start(date, start_day))).or_default();
                e.0 += rx;
                e.1 += tx;
            }
            let cyc = |start: chrono::NaiveDate| {
                let key = ymd(start);
                let (rx, tx) = by_cycle.get(&key).copied().unwrap_or_default();
                Cycle { key, end: ymd(cycle_end(start)), rx, tx }
            };
            let cur_start = *cycles.last().unwrap();
            let cur_end = cycle_end(cur_start);
            // Counting starts with this feature: a cycle that began before the
            // first recorded day is only partly known.
            let tracked_from = first_day
                .as_deref()
                .and_then(|f| chrono::NaiveDate::parse_from_str(f, "%Y-%m-%d").ok())
                .unwrap_or(today_d)
                .max(cur_start);
            let (trx, ttx) = get(&today_s);
            Ok(Summary {
                today: Total { key: today_s.clone(), rx: trx, tx: ttx },
                cycle: cyc(cur_start),
                cycle_days: (cur_end - cur_start).num_days() + 1,
                // Today included: it is still usable.
                days_left: (cur_end - today_d).num_days() + 1,
                tracked_days: (today_d - tracked_from).num_days() + 1,
                tracked_from: ymd(tracked_from),
                partial: tracked_from > cur_start,
                days: days.iter().map(|d| { let (rx, tx) = get(d); Total { key: d.clone(), rx, tx } }).collect(),
                cycles: cycles.iter().map(|c| cyc(*c)).collect(),
            })
        })
        .await
    }

    async fn report(&self) -> Result<serde_json::Value, String> {
        let cs = self.load_settings().await;
        let sum = self.summary(cs.start_day()).await?;
        let (state, dev, ip) = self.status();
        let used = sum.cycle.rx + sum.cycle.tx;
        let allowance = cs.allowance_bytes();
        let share = allowance.map(|a| used as f64 / a as f64);
        // Straight-line projection at the daily average of the tracked days.
        // Not offered for a partial cycle: the untracked days would make it low.
        let projected = (!sum.partial && sum.tracked_days >= 1)
            .then(|| used + (used as f64 / sum.tracked_days as f64 * (sum.days_left - 1).max(0) as f64) as u64);
        let (since, last_action) = {
            let g = self.inner.lock().unwrap();
            (g.link_since.as_ref().map(|(_, t)| *t), g.last_action.clone())
        };
        let iface = find_sample(&self.class_net, dev.as_deref()).map(|s| s.name);
        Ok(serde_json::json!({
            "today": sum.today,
            "cycle": {
                "key": sum.cycle.key,
                "end": sum.cycle.end,
                "rx": sum.cycle.rx,
                "tx": sum.cycle.tx,
                "start_day": cs.start_day(),
                "days": sum.cycle_days,
                "days_left": sum.days_left,
                "partial": sum.partial,
                "tracked_from": sum.tracked_from,
                "projected": projected,
            },
            "days": sum.days,
            "cycles": sum.cycles,
            "allowance": {
                "gb": cs.allowance_gb,
                "bytes": allowance,
                "share": share,
                "remaining": allowance.map(|a| a.saturating_sub(used)),
                "warn": share.is_some_and(|s| s >= WARN_SHARE),
                "exceeded": share.is_some_and(|s| s >= 1.0),
                "cut_at_limit": cs.cut_at_limit,
                "blocked": cs.cap_blocked(&sum.cycle.key, used),
            },
            "controls": {
                "enabled": cs.enabled,
                "want_on": cs.want_on(&sum.cycle.key, used),
                "ota_over_4g": cs.ota_over_4g,
                "logs_over_4g": cs.logs_over_4g,
                "last_action": last_action,
            },
            "link": { "state": state, "dev": dev, "ip": ip, "since": since, "iface": iface },
        }))
    }
}

#[derive(Debug, Clone, Serialize)]
struct Total {
    /// Day ("YYYY-MM-DD").
    key: String,
    rx: u64,
    tx: u64,
}

/// One billing cycle, `key` (first day) to `end` (last day) inclusive.
#[derive(Debug, Clone, Serialize)]
struct Cycle {
    key: String,
    end: String,
    rx: u64,
    tx: u64,
}

struct Summary {
    today: Total,
    cycle: Cycle,
    cycle_days: i64,
    days_left: i64,
    tracked_days: i64,
    tracked_from: String,
    partial: bool,
    days: Vec<Total>,
    cycles: Vec<Cycle>,
}

#[derive(Debug, Serialize)]
struct ActionResult {
    ok: bool,
    detail: String,
}

async fn run_blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    match tokio::time::timeout(DB_TIMEOUT, tokio::task::spawn_blocking(job)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("task: {e}")),
        Err(_) => Err("database busy".to_string()),
    }
}

/// `sudo -n <args>`, bounded; the child dies with the future (kill_on_drop).
async fn sudo(args: &[&str]) -> Result<(), String> {
    let out = tokio::time::timeout(CMD_TIMEOUT, Command::new("sudo").arg("-n").args(args).kill_on_drop(true).output())
        .await
        .map_err(|_| format!("{} timed out", args.join(" ")))?
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed").to_string())
    }
}

/// `systemctl is-active` of the dial unit (no root needed).
async fn unit_state() -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(5), Command::new("systemctl").args(["is-active", DIAL_UNIT]).kill_on_drop(true).output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── HTTP ──────────────────────────────────────────────────────

pub fn router<S: Clone + Send + Sync + 'static>(du: Arc<DataUsage>) -> Router<S> {
    Router::new().route("/api/cell/usage", get(api_usage)).route("/api/cell/control", post(api_control)).with_state(du)
}

/// `GET /api/cell/usage` → totals (today, billing cycle, 30 days, 12 cycles), the
/// allowance status, the controls' state and the link.
async fn api_usage(State(du): State<Arc<DataUsage>>) -> Json<serde_json::Value> {
    match du.report().await {
        Ok(v) => Json(v),
        Err(e) => Json(serde_json::json!({ "error": e })),
    }
}

#[derive(Debug, Deserialize)]
pub struct ControlBody {
    /// "on" | "off" | "reconnect"
    action: Option<String>,
    allowance_gb: Option<f64>,
    cycle_start_day: Option<u32>,
    cut_at_limit: Option<bool>,
    ota_over_4g: Option<bool>,
    logs_over_4g: Option<bool>,
}

/// `POST /api/cell/control` — settings fields and/or one action. Settings are
/// saved first (durably), then the action runs; the answer carries the
/// action's read-back result and the fresh report.
async fn api_control(State(du): State<Arc<DataUsage>>, Json(body): Json<ControlBody>) -> Json<serde_json::Value> {
    let mut cs = du.load_settings().await;
    let mut warnings = Vec::new();
    if let Some(d) = body.cycle_start_day {
        let c = d.clamp(1, 28);
        if c != d {
            warnings.push(format!("cycle_start_day {d} out of range — set to {c}"));
        }
        cs.cycle_start_day = c;
    }
    if let Some(gb) = body.allowance_gb {
        // 0 = none; 1 TB is beyond any mobile plan this car will have.
        let c = if gb.is_finite() { gb.clamp(0.0, 1000.0) } else { 0.0 };
        if c != gb {
            warnings.push(format!("allowance_gb {gb} out of range — set to {c}"));
        }
        cs.allowance_gb = (c * 2.0).round() / 2.0;
    }
    if let Some(v) = body.cut_at_limit {
        cs.cut_at_limit = v;
    }
    if let Some(v) = body.ota_over_4g {
        cs.ota_over_4g = v;
    }
    if let Some(v) = body.logs_over_4g {
        cs.logs_over_4g = v;
    }
    let action = body.action.as_deref();
    match action {
        Some("on") | Some("reconnect") => {
            cs.enabled = true;
            // Turning 4G back on while the allowance cut holds = "I know, keep
            // it on for this cycle".
            let (cycle, used) = du.cycle_bytes(&cs).await;
            if cs.cap_blocked(&cycle, used) {
                cs.cap_override_cycle = Some(cycle);
            }
        }
        Some("off") => cs.enabled = false,
        Some(other) => return Json(serde_json::json!({ "ok": false, "error": format!("unknown action {other:?}") })),
        None => {}
    }
    if let Err(e) = du.save_settings(&cs).await {
        return Json(serde_json::json!({ "ok": false, "error": format!("settings not saved: {e}") }));
    }
    let result = match action {
        Some("off") => Some(du.link_off().await),
        Some("on") => Some(du.link_on().await),
        Some("reconnect") => {
            let off = du.link_off().await;
            let on = du.link_on().await;
            Some(ActionResult { ok: off.ok && on.ok, detail: if off.ok { on.detail } else { off.detail } })
        }
        _ => None,
    };
    if let Some(r) = &result {
        let mut g = du.inner.lock().unwrap();
        g.last_action = Some(r.detail.clone());
        // A user action supersedes the automatic cut bookkeeping and the
        // enforcement gap (the next tick must not undo a fresh "on").
        g.stopped_by_cap = false;
        g.last_enforce = Some(Instant::now());
    }
    let report = du.report().await.unwrap_or_else(|e| serde_json::json!({ "error": e }));
    Json(serde_json::json!({
        "ok": result.as_ref().is_none_or(|r| r.ok),
        "result": result,
        "warnings": warnings,
        "usage": report,
    }))
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn b(boot: &str, ifindex: u32, rx: u64, tx: u64) -> Baseline {
        Baseline { boot_id: boot.into(), ifindex, rx, tx }
    }
    fn s(ifindex: u32, rx: u64, tx: u64) -> Sample {
        Sample { name: "wwu1i5".into(), ifindex, rx, tx }
    }

    #[test]
    fn delta_counts_increase_on_the_same_interface() {
        assert_eq!(delta(Some(&b("A", 5, 100, 10)), "A", &s(5, 150, 30)), (50, 20));
        assert_eq!(delta(Some(&b("A", 5, 100, 10)), "A", &s(5, 100, 10)), (0, 0));
    }

    #[test]
    fn delta_counts_whole_after_reboot_new_iface_or_reset() {
        // First run ever: counters since the interface appeared.
        assert_eq!(delta(None, "A", &s(5, 70, 7)), (70, 7));
        // Reboot: new boot id, counters restarted at 0 (same ifindex is common).
        assert_eq!(delta(Some(&b("A", 5, 1000, 100)), "B", &s(5, 70, 7)), (70, 7));
        // USB reset re-created the netdev: new ifindex, fresh counters.
        assert_eq!(delta(Some(&b("A", 5, 1000, 100)), "A", &s(6, 70, 7)), (70, 7));
        // Decrease on the same interface: a reset, never a negative delta.
        assert_eq!(delta(Some(&b("A", 5, 1000, 100)), "A", &s(5, 70, 700)), (70, 700));
    }

    /// A fake /sys/class/net with a renameable interface.
    struct FakeNet(PathBuf);
    impl FakeNet {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("rascar-du-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            FakeNet(d)
        }
        fn set(&self, name: &str, ifindex: u32, rx: u64, tx: u64) {
            let d = self.0.join(name).join("statistics");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(self.0.join(name).join("ifindex"), format!("{ifindex}\n")).unwrap();
            std::fs::write(self.0.join(name).join("flags"), "0x1003\n").unwrap();
            std::fs::write(d.join("rx_bytes"), format!("{rx}\n")).unwrap();
            std::fs::write(d.join("tx_bytes"), format!("{tx}\n")).unwrap();
        }
        fn rename(&self, from: &str, to: &str) {
            std::fs::rename(self.0.join(from), self.0.join(to)).unwrap();
        }
        fn remove(&self, name: &str) {
            std::fs::remove_dir_all(self.0.join(name)).unwrap();
        }
    }
    impl Drop for FakeNet {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Feed readings through find_sample + delta like the tick does.
    fn run(net: &FakeNet, base: &mut Option<Baseline>, boot: &str, status_dev: Option<&str>) -> (u64, u64) {
        let Some(smp) = find_sample(&net.0, status_dev) else { return (0, 0) };
        let d = delta(base.as_ref(), boot, &smp);
        *base = Some(Baseline { boot_id: boot.into(), ifindex: smp.ifindex, rx: smp.rx, tx: smp.tx });
        d
    }

    #[test]
    fn fake_sysfs_rename_disappear_reappear_and_reboot() {
        let net = FakeNet::new("seq");
        std::fs::create_dir_all(net.0.join("eth0")).unwrap(); // not a ww* device: ignored
        let mut base = None;
        let mut total = (0u64, 0u64);
        fn add(t: &mut (u64, u64), d: (u64, u64)) {
            t.0 += d.0;
            t.1 += d.1;
        }

        net.set("wwan0", 7, 1_000, 100); // appears before udev renames it
        add(&mut total, run(&net, &mut base, "boot1", None));
        net.set("wwan0", 7, 3_000, 300);
        add(&mut total, run(&net, &mut base, "boot1", None));
        net.rename("wwan0", "wwu1i5"); // rename keeps ifindex + counters
        net.set("wwu1i5", 7, 4_000, 400);
        add(&mut total, run(&net, &mut base, "boot1", Some("wwu1i5")));
        assert_eq!(total, (4_000, 400), "a rename must neither reset nor double count");

        net.remove("wwu1i5"); // USB reset: gone for a while
        add(&mut total, run(&net, &mut base, "boot1", Some("wwu1i5")));
        net.set("wwu1i5", 9, 500, 50); // back with a new ifindex, counters from 0
        add(&mut total, run(&net, &mut base, "boot1", Some("wwu1i5")));
        assert_eq!(total, (4_500, 450));

        // Status naming a device that does not exist falls back to ww*.
        add(&mut total, run(&net, &mut base, "boot1", Some("wwu1u3i5")));
        assert_eq!(total, (4_500, 450));

        // Reboot: the same ifindex comes back with small counters.
        net.set("wwu1i5", 9, 200, 20);
        add(&mut total, run(&net, &mut base, "boot2", Some("wwu1i5")));
        assert_eq!(total, (4_700, 470));
    }

    #[test]
    fn baseline_and_totals_commit_together_and_resume_after_restart() {
        let dir = FakeNet::new("db");
        let path = dir.0.join("datausage.db");
        let mut c = open_db(&path).unwrap();
        let mut pending = BTreeMap::new();
        pending.insert("2026-10-08".to_string(), (100u64, 10u64));
        commit(&mut c, &pending, Some(&b("A", 5, 100, 10))).unwrap();
        pending.insert("2026-10-08".to_string(), (50, 5));
        commit(&mut c, &pending, Some(&b("A", 5, 150, 15))).unwrap();
        drop(c);
        // An app restart in the same boot resumes from the stored baseline.
        let c = open_db(&path).unwrap();
        let base = load_baseline(&c).unwrap();
        assert_eq!(base, b("A", 5, 150, 15));
        assert_eq!(delta(Some(&base), "A", &s(5, 160, 16)), (10, 1));
        let (rx, tx): (i64, i64) = c.query_row("SELECT rx, tx FROM usage_day WHERE day='2026-10-08'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((rx, tx), (150, 15));
        assert_eq!(max_day(&c).as_deref(), Some("2026-10-08"));
    }

    #[test]
    fn allowance_cut_and_override() {
        let mut cs = CellDataSettings { allowance_gb: 10.0, cut_at_limit: true, ..Default::default() };
        assert!(!cs.cap_blocked("2026-03-01", 9_999_999_999));
        assert!(cs.cap_blocked("2026-03-01", 10_000_000_000));
        assert!(!cs.want_on("2026-03-01", 10_000_000_000));
        cs.cap_override_cycle = Some("2026-03-01".into());
        assert!(cs.want_on("2026-03-01", 12_000_000_000), "turned back on by the user this cycle");
        assert!(cs.cap_blocked("2026-04-01", 12_000_000_000), "the override ends with the cycle");
        cs.allowance_gb = 0.0;
        assert!(!cs.cap_blocked("2026-04-01", u64::MAX), "no allowance, no cut");
        cs.enabled = false;
        assert!(!cs.want_on("2026-04-01", 0));
    }

    #[test]
    fn day_and_month_windows() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap();
        let days = last_days(d, 30);
        assert_eq!(days.len(), 30);
        assert_eq!(days.first().unwrap(), "2026-02-01");
        assert_eq!(days.last().unwrap(), "2026-03-02");
        let ymd_ = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap();
        // 17th -> 16th cycles.
        assert_eq!(cycle_start(ymd_(2026, 10, 16), 17), ymd_(2026, 9, 17));
        assert_eq!(cycle_start(ymd_(2026, 10, 17), 17), ymd_(2026, 10, 17));
        assert_eq!(cycle_start(ymd_(2026, 1, 5), 17), ymd_(2025, 12, 17), "crosses the year");
        assert_eq!(cycle_end(ymd_(2026, 10, 17)), ymd_(2026, 11, 16));
        assert_eq!(cycle_end(ymd_(2027, 1, 28)), ymd_(2027, 2, 27));
        assert_eq!(cycle_start(ymd_(2026, 3, 1), 1), ymd_(2026, 3, 1));
        let cycles = last_cycles(ymd_(2026, 10, 9), 17, 12);
        assert_eq!(cycles.len(), 12);
        assert_eq!(*cycles.last().unwrap(), ymd_(2026, 9, 17));
        assert_eq!(*cycles.first().unwrap(), ymd_(2025, 10, 17));
    }

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status("connected wwu1i5 100.64.0.2\n"), ("connected".into(), Some("wwu1i5".into()), Some("100.64.0.2".into())));
        assert_eq!(parse_status("dial failed\n"), ("dial failed".into(), None, None));
        assert_eq!(parse_status(""), ("".into(), None, None));
    }
}
