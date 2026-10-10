//! Log push: journal → durable spool → update server, plus files under
//! `/var/lib/rascar-debug` (or any configured glob).
//!
//! Why a spool: the journal lives in a 50 MB zram that a power cut erases, and
//! the car is switched off by cutting power. Every `journal.intervalSecs` the
//! agent copies the new entries of the current boot to the NVMe
//! (`<state>/spool/journal/boot-<date>-<id>.log`, fsynced). Uploading then
//! works from files only, at whatever pace the uplink allows.
//!
//! Exactly-once, across power cuts:
//! * spool: after each append + fsync the state records `(cursor, spool_len)`.
//!   At start a spool longer than `spool_len` is truncated back (those lines
//!   are read again from the journal after the recorded cursor);
//! * upload: every stream is addressed by byte offset. The server stores a batch
//!   once even if it is sent twice and answers `have` (what it holds); the
//!   local cursor only moves to what the server acknowledged.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tracing::{info, warn};

use crate::config::{AgentConfig, FileSource, JournalConfig};
use crate::server::{Req, UpdateServer};
use crate::util;

const STATE_FILE: &str = "logs-state.json";
const JOURNAL_READ_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LogState {
    pub journal: Option<JournalCursor>,
    /// Upload progress per stream (server-acknowledged bytes).
    pub streams: BTreeMap<String, StreamState>,
    /// Identity of every tracked source file, to notice replacement/truncation.
    pub files: BTreeMap<PathBuf, FileTrack>,
    pub metered: MeteredUse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalCursor {
    pub boot_id: String,
    pub cursor: Option<String>,
    /// Spool file name (in `<state>/spool/journal/`).
    pub spool: String,
    /// Bytes of the spool that belong to entries up to `cursor`.
    pub spool_len: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamState {
    pub path: PathBuf,
    pub sent: u64,
    /// Bytes of the file that are shipped at most.
    pub cap: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileTrack {
    pub ino: u64,
    /// Bumped when the file shrinks in place (copytruncate rotation).
    pub generation: u32,
    pub last_len: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MeteredUse {
    pub day: String,
    pub bytes: u64,
}

pub fn load_state(dir: &Path) -> LogState {
    std::fs::read(dir.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(dir: &Path, st: &LogState) -> Result<()> {
    util::atomic_write(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(st)?)
}

// ── Journal → spool ─────────────────────────────────────────────────────────

fn field<'a>(v: &'a serde_json::Value, k: &str) -> Option<std::borrow::Cow<'a, str>> {
    match v.get(k)? {
        serde_json::Value::String(s) => Some(std::borrow::Cow::Borrowed(s)),
        // journald exports non-UTF-8 / binary fields as an array of bytes.
        serde_json::Value::Array(a) => {
            let bytes: Vec<u8> = a.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect();
            Some(std::borrow::Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()))
        }
        _ => None,
    }
}

/// Shell-style glob with `*` only (the unit patterns).
pub fn glob_match(pat: &str, s: &str) -> bool {
    glob::Pattern::new(pat).is_ok_and(|p| p.matches(s))
}

/// One journal entry as a spool line, or None when filtered out.
/// Format: `<local time µs> <unit|kernel|ident> <priority>: <message>`,
/// newlines in the message escaped so one entry is one line (the update server's
/// `unit=` filter reads the second field).
pub fn format_entry(v: &serde_json::Value, cfg: &JournalConfig) -> Option<String> {
    let transport = field(v, "_TRANSPORT");
    let unit = field(v, "_SYSTEMD_UNIT");
    let about = field(v, "UNIT"); // systemd's own messages about a unit
    let prio: u8 = field(v, "PRIORITY").and_then(|p| p.parse().ok()).unwrap_or(6);
    let is_kernel = transport.as_deref() == Some("kernel");
    let unit_hit = |u: &Option<std::borrow::Cow<str>>| u.as_deref().is_some_and(|u| cfg.units.iter().any(|p| glob_match(p, u)));
    let keep = (cfg.kernel && is_kernel)
        || unit_hit(&unit)
        || unit_hit(&about)
        || cfg.any_unit_max_priority.is_some_and(|max| prio <= max);
    if !keep {
        return None;
    }
    let who = if is_kernel {
        "kernel".to_string()
    } else if let (Some(a), true) = (&about, unit.as_deref() == Some("init.scope")) {
        format!("{a}>systemd")
    } else {
        unit.or_else(|| field(v, "SYSLOG_IDENTIFIER")).map(|s| s.into_owned()).unwrap_or_else(|| "?".into())
    };
    let who: String = who.chars().map(|c| if c.is_whitespace() { '_' } else { c }).collect();
    let ts = field(v, "__REALTIME_TIMESTAMP")
        .and_then(|t| t.parse::<i64>().ok())
        .and_then(chrono::DateTime::from_timestamp_micros)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S%.6f%z").to_string())
        .unwrap_or_else(|| "-".into());
    let msg = field(v, "MESSAGE").unwrap_or_default().replace('\r', "").replace('\n', "\\n");
    Some(format!("{ts} {who} {prio}: {msg}\n"))
}

fn spool_dir(cfg: &AgentConfig) -> PathBuf {
    cfg.state_dir.join("spool").join("journal")
}

/// Bring the spool back to what the state says was committed (a power cut
/// between the spool fsync and the state write leaves extra lines).
pub fn repair_spool(cfg: &AgentConfig, st: &LogState) -> Result<()> {
    let Some(j) = &st.journal else { return Ok(()) };
    let p = spool_dir(cfg).join(&j.spool);
    if let Ok(meta) = std::fs::metadata(&p) {
        if meta.len() > j.spool_len {
            warn!(spool = %p.display(), from = meta.len(), to = j.spool_len, "journal spool: dropping uncommitted tail");
            std::fs::OpenOptions::new().write(true).open(&p)?.set_len(j.spool_len)?;
        }
    }
    Ok(())
}

/// Copy new journal entries of this boot into the spool. Returns entries kept.
pub async fn collect_journal(cfg: &AgentConfig, st: &mut LogState) -> Result<usize> {
    let boot = util::boot_id();
    if st.journal.as_ref().is_none_or(|j| j.boot_id != boot) {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let short: String = boot.chars().take(8).collect();
        st.journal = Some(JournalCursor { boot_id: boot.clone(), cursor: None, spool: format!("boot-{stamp}-{short}.log"), spool_len: 0 });
    }
    let j = st.journal.clone().expect("set above");
    let mut args = vec!["-b".to_string(), "0".into(), "-o".into(), "json".into(), "--no-pager".into()];
    if let Some(c) = &j.cursor {
        args.push(format!("--after-cursor={c}"));
    }
    let mut child = tokio::process::Command::new("journalctl")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawn journalctl")?;
    let stdout = child.stdout.take().context("journalctl stdout")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut out = String::new();
    let mut cursor = j.cursor.clone();
    let mut read = 0usize;
    let mut kept = 0usize;
    let deadline = tokio::time::Instant::now() + JOURNAL_READ_TIMEOUT;
    while read < cfg.journal.max_entries_per_cycle {
        let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                warn!("journalctl read timed out; keeping what was read");
                break;
            }
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        read += 1;
        if let Some(c) = field(&v, "__CURSOR") {
            cursor = Some(c.into_owned());
        }
        if let Some(l) = format_entry(&v, &cfg.journal) {
            out.push_str(&l);
            kept += 1;
        }
    }
    drop(lines);
    let _ = child.start_kill();
    if cursor == j.cursor {
        return Ok(0);
    }
    let dir = spool_dir(cfg);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(&j.spool);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(out.as_bytes())?;
    f.sync_data()?;
    let len = f.metadata()?.len();
    st.journal = Some(JournalCursor { cursor, spool_len: len, ..j });
    Ok(kept)
}

// ── Sources ─────────────────────────────────────────────────────────────────

/// One thing to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub stream: String,
    pub path: PathBuf,
    pub cap: u64,
}

/// The update server accepts `[A-Za-z0-9._@~+-]` per component.
pub fn safe_component(s: &str) -> String {
    let c: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || "._@~+-".contains(c) { c } else { '_' }).collect();
    if c.is_empty() || c == "." || c == ".." { "_".into() } else { c }
}

/// Stream name of a source file: `files/<rel>~<inode>[.<gen>]`. The inode
/// keeps a deleted-and-recreated file from appending to its predecessor's
/// stream; the generation does the same for a file truncated in place.
pub fn file_stream(rel: &Path, ino: u64, generation: u32) -> String {
    let mut parts: Vec<String> = rel.components().map(|c| safe_component(&c.as_os_str().to_string_lossy())).collect();
    // The server takes at most 6 components; keep the tail.
    while parts.len() > 5 {
        parts.remove(0);
    }
    let last = parts.pop().unwrap_or_else(|| "_".into());
    let suffix = if generation > 0 { format!("~{ino}.{generation}") } else { format!("~{ino}") };
    parts.push(format!("{last}{suffix}"));
    format!("files/{}", parts.join("/"))
}

/// Journal spools, the current boot first, then the others newest first.
pub fn journal_sources(cfg: &AgentConfig, st: &LogState) -> Vec<Source> {
    let mut names: Vec<String> = std::fs::read_dir(spool_dir(cfg))
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_owned)).filter(|n| n.ends_with(".log")).collect())
        .unwrap_or_default();
    names.sort_unstable_by(|a, b| b.cmp(a));
    if let Some(cur) = st.journal.as_ref().map(|j| j.spool.clone()) {
        if let Some(i) = names.iter().position(|n| *n == cur) {
            let n = names.remove(i);
            names.insert(0, n);
        }
    }
    names
        .into_iter()
        .map(|n| Source {
            stream: format!("{}/journal.log", safe_component(n.trim_end_matches(".log"))),
            path: spool_dir(cfg).join(&n),
            cap: u64::MAX,
        })
        .collect()
}

/// Files matching the configured globs, newest first. Updates `st.files`.
pub fn file_sources(cfg: &AgentConfig, st: &mut LogState, metered: bool, now: std::time::SystemTime) -> Vec<Source> {
    let mut found: Vec<(std::time::SystemTime, Source)> = Vec::new();
    let mut alive = std::collections::HashSet::new();
    for src in &cfg.files {
        if metered && !src.metered {
            continue;
        }
        for (path, meta) in glob_files(src) {
            let mtime = meta.modified().unwrap_or(now);
            if now.duration_since(mtime).unwrap_or_default() > Duration::from_secs(src.max_age_days * 86_400) {
                continue;
            }
            let track = st.files.entry(path.clone()).or_default();
            if track.ino != meta.ino() {
                *track = FileTrack { ino: meta.ino(), generation: 0, last_len: 0 };
            } else if meta.len() < track.last_len {
                track.generation += 1;
            }
            track.last_len = meta.len();
            alive.insert(path.clone());
            let rel = path.strip_prefix(&src.root).unwrap_or(&path).to_path_buf();
            found.push((mtime, Source { stream: file_stream(&rel, track.ino, track.generation), path, cap: src.max_bytes_per_file }));
        }
    }
    // Forget files that are gone (or aged out) so the state does not grow forever.
    st.files.retain(|p, _| alive.contains(p));
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, s)| s).collect()
}

fn glob_files(src: &FileSource) -> Vec<(PathBuf, std::fs::Metadata)> {
    let pattern = src.root.join(&src.glob);
    let Some(pattern) = pattern.to_str() else { return vec![] };
    glob::glob(pattern)
        .map(|it| {
            it.flatten()
                .filter_map(|p| {
                    // symlink_metadata: never follow a link out of the tree (`latest`).
                    let m = std::fs::symlink_metadata(&p).ok()?;
                    m.is_file().then_some((p, m))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Metered uplink ──────────────────────────────────────────────────────────

/// Interface of the lowest-metric default route in `/proc/net/route` text.
pub fn default_route_iface(route: &str) -> Option<String> {
    route
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
            let metric = f.get(6)?.parse::<u32>().ok()?;
            (f.get(1)? == &"00000000" && flags & 1 == 1).then(|| (metric, f[0].to_string()))
        })
        .min()
        .map(|(_, i)| i)
}

/// True when traffic currently leaves through a metered link (4G, tethering).
pub fn uplink_is_metered(drivers: &[String]) -> bool {
    let Some(iface) = std::fs::read_to_string("/proc/net/route").ok().and_then(|r| default_route_iface(&r)) else {
        return false;
    };
    if iface.starts_with("ww") {
        return true;
    }
    std::fs::read_link(format!("/sys/class/net/{iface}/device/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|d| drivers.contains(&d))
}

// ── Upload ──────────────────────────────────────────────────────────────────

/// The bytes of `[from, to)` to send next: whole lines when the chunk does
/// not reach the end of the file (so the server rarely holds half a line).
pub fn next_chunk(path: &Path, from: u64, to: u64, max: u64) -> Result<Vec<u8>> {
    let end = to.min(from + max);
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0u8; (end - from) as usize];
    f.read_exact(&mut buf)?;
    if end < to {
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            buf.truncate(i + 1);
        }
    }
    Ok(buf)
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

/// Why an upload round stopped early.
#[derive(Debug, PartialEq)]
pub enum Stop {
    Done,
    Offline,
    Budget,
    Refused(u16),
}

pub struct UploadReport {
    pub stop: Stop,
    pub bytes_sent: u64,
    pub backlog: u64,
}

/// Upload what is pending, oldest offset first within each stream.
pub async fn upload(cfg: &AgentConfig, st: &mut LogState, g: &UpdateServer, metered: bool) -> UploadReport {
    let today = chrono::Local::now().format("%Y%m%d").to_string();
    if st.metered.day != today {
        st.metered = MeteredUse { day: today, bytes: 0 };
    }
    let mut sources = journal_sources(cfg, st);
    sources.extend(file_sources(cfg, st, metered, std::time::SystemTime::now()));
    let live: std::collections::HashSet<String> = sources.iter().map(|s| s.stream.clone()).collect();
    st.streams.retain(|k, _| live.contains(k));

    let mut sent_total = 0u64;
    let mut stop = Stop::Done;
    'outer: for src in &sources {
        let Ok(len) = std::fs::metadata(&src.path).map(|m| m.len()) else { continue };
        let entry = st.streams.entry(src.stream.clone()).or_insert_with(|| StreamState { path: src.path.clone(), sent: 0, cap: src.cap });
        entry.cap = src.cap;
        let target = len.min(src.cap);
        while entry.sent < target {
            if metered && st.metered.bytes >= cfg.upload.metered_daily_budget_bytes {
                stop = Stop::Budget;
                break 'outer;
            }
            let chunk = match next_chunk(&src.path, entry.sent, target, cfg.upload.batch_bytes) {
                Ok(c) if !c.is_empty() => c,
                _ => break,
            };
            let body = gzip(&chunk);
            let wire = body.len() as u64;
            let req = Req::post("/api/carops/logs", body, UPLOAD_TIMEOUT)
                .header("X-Car-Stream", src.stream.clone())
                .header("X-Car-Offset", entry.sent.to_string())
                .header("Content-Encoding", "gzip")
                .header("Content-Type", "application/octet-stream");
            let resp = match g.send(&req).await {
                Ok(r) => r,
                Err(e) => {
                    info!(error = %e, "log upload: update server unreachable, will retry");
                    stop = Stop::Offline;
                    break 'outer;
                }
            };
            let status = resp.status().as_u16();
            let have = resp.json::<serde_json::Value>().await.ok().and_then(|v| v["have"].as_u64());
            if metered {
                st.metered.bytes += wire;
            }
            sent_total += wire;
            match (status, have) {
                (200, Some(h)) | (409, Some(h)) if h <= len => entry.sent = h,
                (409, Some(h)) => {
                    // The server holds more than this file has: not the same content
                    // (should not happen with inode-named streams). Skip it.
                    warn!(stream = %src.stream, have = h, len, "log upload: the server is ahead of the file; skipping it");
                    entry.sent = target;
                }
                _ => {
                    warn!(stream = %src.stream, status, "log upload refused");
                    stop = Stop::Refused(status);
                    break 'outer;
                }
            }
        }
    }
    let backlog = st.streams.values().map(|s| std::fs::metadata(&s.path).map(|m| m.len().min(s.cap)).unwrap_or(0).saturating_sub(s.sent)).sum();
    UploadReport { stop, bytes_sent: sent_total, backlog }
}

/// Remove journal spools that are fully uploaded (never the current boot's),
/// then the oldest ones while the spool exceeds its cap.
pub fn prune_spool(cfg: &AgentConfig, st: &LogState) -> Vec<PathBuf> {
    let current = st.journal.as_ref().map(|j| spool_dir(cfg).join(&j.spool));
    let mut spools: Vec<(PathBuf, u64, bool)> = journal_sources(cfg, st)
        .into_iter()
        .filter_map(|s| {
            let len = std::fs::metadata(&s.path).ok()?.len();
            let done = st.streams.get(&s.stream).is_some_and(|x| x.sent >= len);
            Some((s.path, len, done))
        })
        .collect();
    let mut removed = Vec::new();
    spools.retain(|(p, _, done)| {
        if *done && Some(p) != current.as_ref() {
            removed.push(p.clone());
            false
        } else {
            true
        }
    });
    let mut total: u64 = spools.iter().map(|s| s.1).sum();
    spools.sort_by(|a, b| a.0.cmp(&b.0)); // names start with the date: oldest first
    for (p, len, _) in spools {
        if total <= cfg.upload.spool_max_bytes || Some(&p) == current.as_ref() {
            continue;
        }
        warn!(spool = %p.display(), "journal spool over its cap: dropping a boot that was never uploaded");
        total -= len;
        removed.push(p);
    }
    for p in &removed {
        let _ = std::fs::remove_file(p);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jcfg() -> JournalConfig {
        JournalConfig::default()
    }

    #[test]
    fn journal_entries_are_filtered_and_formatted() {
        let c = jcfg();
        let app = json!({"_SYSTEMD_UNIT":"rascar-app.service","PRIORITY":"6","MESSAGE":"line1\nline2","__REALTIME_TIMESTAMP":"1791269844281772"});
        let l = format_entry(&app, &c).unwrap();
        assert!(l.contains(" rascar-app.service 6: line1\\nline2\n"), "{l}");
        assert_eq!(l.matches('\n').count(), 1, "one entry, one line");
        let k = json!({"_TRANSPORT":"kernel","PRIORITY":"4","MESSAGE":"usb 7-1: reset"});
        assert!(format_entry(&k, &c).unwrap().contains(" kernel 4: usb 7-1: reset"));
        // PID 1 talking about our unit (a crash/restart) is kept and labelled.
        let pid1 = json!({"_SYSTEMD_UNIT":"init.scope","UNIT":"rascar-carplay.service","PRIORITY":"6","MESSAGE":"Started"});
        assert!(format_entry(&pid1, &c).unwrap().contains(" rascar-carplay.service>systemd 6: Started"));
        // Unrelated chatter is dropped, unrelated errors are kept.
        let other = json!({"_SYSTEMD_UNIT":"cron.service","PRIORITY":"6","MESSAGE":"x"});
        assert!(format_entry(&other, &c).is_none());
        let err = json!({"_SYSTEMD_UNIT":"cron.service","PRIORITY":"3","MESSAGE":"boom"});
        assert!(format_entry(&err, &c).is_some());
        // Binary MESSAGE as a byte array.
        let bin = json!({"_SYSTEMD_UNIT":"rascar-app.service","MESSAGE":[104,105]});
        assert!(format_entry(&bin, &c).unwrap().ends_with(": hi\n"));
    }

    #[test]
    fn file_streams_carry_inode_and_generation() {
        assert_eq!(file_stream(Path::new("earlyboot/20261006-0712-ab.kmsg"), 42, 0), "files/earlyboot/20261006-0712-ab.kmsg~42");
        assert_eq!(file_stream(Path::new("obd/raw log.txt"), 7, 2), "files/obd/raw_log.txt~7.2");
        assert_eq!(file_stream(Path::new("a/b/c/d/e/f/g.log"), 1, 0), "files/c/d/e/f/g.log~1");
    }

    #[test]
    fn default_route_picks_the_lowest_metric() {
        let route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                     wwan0\t00000000\t0100000A\t0003\t0\t0\t2048\t00000000\t0\t0\t0\n\
                     enP3p49s0\t00000000\t0158A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                     enP3p49s0\t0058A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_route_iface(route).as_deref(), Some("enP3p49s0"));
        let only4g = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                      wwan0\t00000000\t0100000A\t0003\t0\t0\t2048\t00000000\n";
        assert_eq!(default_route_iface(only4g).as_deref(), Some("wwan0"));
        assert_eq!(default_route_iface("Iface\n"), None);
    }

    #[test]
    fn chunks_end_on_a_line_unless_they_reach_the_end() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        std::fs::write(&p, b"aaa\nbbb\nccc").unwrap();
        assert_eq!(next_chunk(&p, 0, 11, 6).unwrap(), b"aaa\n");
        assert_eq!(next_chunk(&p, 4, 11, 100).unwrap(), b"bbb\nccc");
        // A single line longer than the batch is sent cut, not stuck.
        assert_eq!(next_chunk(&p, 0, 11, 2).unwrap(), b"aa");
    }

    fn cfg_in(dir: &Path) -> AgentConfig {
        AgentConfig { state_dir: dir.to_path_buf(), files: vec![FileSource::new(dir.join("dbg").to_str().unwrap(), "obd/*")], ..AgentConfig::default() }
    }

    #[test]
    fn spool_repair_drops_the_uncommitted_tail() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path());
        std::fs::create_dir_all(spool_dir(&cfg)).unwrap();
        std::fs::write(spool_dir(&cfg).join("boot-x.log"), b"committed\nuncommitted\n").unwrap();
        let st = LogState {
            journal: Some(JournalCursor { boot_id: "b".into(), cursor: Some("c".into()), spool: "boot-x.log".into(), spool_len: 10 }),
            ..Default::default()
        };
        repair_spool(&cfg, &st).unwrap();
        assert_eq!(std::fs::read(spool_dir(&cfg).join("boot-x.log")).unwrap(), b"committed\n");
    }

    #[test]
    fn file_tracking_follows_replacement_and_truncation() {
        let d = tempfile::tempdir().unwrap();
        let cfg = cfg_in(d.path());
        let obd = d.path().join("dbg/obd");
        std::fs::create_dir_all(&obd).unwrap();
        std::fs::write(obd.join("raw.log"), b"0123456789").unwrap();
        let mut st = LogState::default();
        let now = std::time::SystemTime::now();
        let s1 = file_sources(&cfg, &mut st, false, now);
        assert_eq!(s1.len(), 1);
        assert!(s1[0].stream.starts_with("files/obd/raw.log~"));
        // Truncated in place: a new generation, i.e. a new stream.
        std::fs::write(obd.join("raw.log"), b"01").unwrap();
        let s2 = file_sources(&cfg, &mut st, false, now);
        assert_ne!(s1[0].stream, s2[0].stream);
        assert!(s2[0].stream.ends_with(".1"), "{}", s2[0].stream);
        // Gone: forgotten.
        std::fs::remove_file(obd.join("raw.log")).unwrap();
        assert!(file_sources(&cfg, &mut st, false, now).is_empty());
        assert!(st.files.is_empty());
    }

    #[test]
    fn spool_prune_keeps_the_current_boot_and_pending_data() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = cfg_in(d.path());
        let sd = spool_dir(&cfg);
        std::fs::create_dir_all(&sd).unwrap();
        for n in ["boot-1.log", "boot-2.log", "boot-3.log"] {
            std::fs::write(sd.join(n), vec![b'x'; 100]).unwrap();
        }
        let mut st = LogState {
            journal: Some(JournalCursor { boot_id: "b".into(), cursor: None, spool: "boot-3.log".into(), spool_len: 100 }),
            ..Default::default()
        };
        // boot-1 fully uploaded, boot-2 half, boot-3 current (fully uploaded too).
        for (n, sent) in [("boot-1", 100), ("boot-2", 50), ("boot-3", 100)] {
            st.streams.insert(format!("{n}/journal.log"), StreamState { path: sd.join(format!("{n}.log")), sent, cap: u64::MAX });
        }
        let removed = prune_spool(&cfg, &st);
        assert_eq!(removed, vec![sd.join("boot-1.log")]);
        // Over the cap: the oldest non-current spool goes even if not uploaded.
        cfg.upload.spool_max_bytes = 150;
        let removed = prune_spool(&cfg, &st);
        assert_eq!(removed, vec![sd.join("boot-2.log")]);
        assert!(sd.join("boot-3.log").exists());
    }
}
