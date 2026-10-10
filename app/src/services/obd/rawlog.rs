//! Durable raw log of the ELM327 conversation: every command sent and every
//! reply received, verbatim, including "NO DATA", errors and timeouts.
//!
//! It lives on the NVMe under `/var/lib/rascar-debug/obd/` (the convention for
//! debug evidence on the Pi): `/var/log` is a 50 MB zram that is lost at every
//! power cut, which is how the car turns the Pi off.
//!
//! Format, one line per event, local time with offset:
//!
//! ```text
//! 2026-10-06T10:12:13.456+02:00 TX 010C
//! 2026-10-06T10:12:13.512+02:00 RX 56ms 41 0C 1A F8
//! 2026-10-06T10:12:13.600+02:00 RX 61ms NO DATA
//! 2026-10-06T10:12:16.700+02:00 ERR 3001ms OBD read timed out after 3s
//! 2026-10-06T10:12:10.000+02:00 EV connect /dev/rfcomm0 38400
//! ```
//!
//! Line breaks inside a reply are written as `\r` / `\n` so one exchange stays
//! on one line. Files rotate at `MAX_FILE_BYTES`; `KEEP_FILES` are kept
//! (`obd-raw.log`, `obd-raw.log.1`, ...), so the log never exceeds ~200 MB.
//! Writes happen on a dedicated thread and are fdatasync'ed every
//! `SYNC_EVERY`, so the poll loop never waits on the disk and a power cut
//! loses at most a couple of seconds.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const DEFAULT_DIR: &str = "/var/lib/rascar-debug/obd";
pub const FILE_NAME: &str = "obd-raw.log";
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const KEEP_FILES: usize = 12;
const SYNC_EVERY: Duration = Duration::from_secs(2);
const QUEUE: usize = 4096;

#[derive(Clone)]
pub struct RawLog {
    tx: Option<SyncSender<String>>,
    dropped: Arc<AtomicU64>,
}

impl RawLog {
    /// A log that writes nothing (tests, or `RASCAR_OBD_RAWLOG_DIR=off`).
    pub fn disabled() -> Self {
        Self { tx: None, dropped: Arc::new(AtomicU64::new(0)) }
    }

    /// Directory from `RASCAR_OBD_RAWLOG_DIR` (`off` disables), else
    /// `DEFAULT_DIR`.
    pub fn from_env() -> Self {
        match std::env::var("RASCAR_OBD_RAWLOG_DIR") {
            Ok(v) if v.is_empty() || v.eq_ignore_ascii_case("off") => {
                info!("OBD raw log disabled (RASCAR_OBD_RAWLOG_DIR={v:?})");
                Self::disabled()
            }
            Ok(v) => Self::open(PathBuf::from(v)),
            Err(_) => Self::open(PathBuf::from(DEFAULT_DIR)),
        }
    }

    pub fn open(dir: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(QUEUE);
        let spawned = std::thread::Builder::new()
            .name("obd-rawlog".into())
            .spawn(move || writer(dir, rx));
        match spawned {
            Ok(_) => Self { tx: Some(tx), dropped: Arc::new(AtomicU64::new(0)) },
            Err(e) => {
                warn!("OBD raw log thread not started: {e}");
                Self::disabled()
            }
        }
    }

    fn push(&self, line: String) {
        let Some(tx) = &self.tx else { return };
        match tx.try_send(line) {
            Ok(()) => {}
            // Disk stalled: drop rather than slow the poll loop; the count is
            // written into the log once the queue drains.
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
        let lost = self.dropped.swap(0, Ordering::Relaxed);
        if lost > 0 && tx.try_send(stamp(&format!("EV dropped {lost} lines (queue full)"))).is_err() {
            self.dropped.fetch_add(lost, Ordering::Relaxed);
        }
    }

    pub fn tx(&self, cmd: &str) {
        self.push(stamp(&format!("TX {}", escape(cmd))));
    }

    pub fn rx(&self, elapsed: Duration, raw: &str) {
        self.push(stamp(&format!("RX {}ms {}", elapsed.as_millis(), escape(raw))));
    }

    pub fn err(&self, elapsed: Duration, error: &str) {
        self.push(stamp(&format!("ERR {}ms {}", elapsed.as_millis(), escape(error))));
    }

    pub fn event(&self, text: &str) {
        self.push(stamp(&format!("EV {}", escape(text))));
    }
}

fn stamp(body: &str) -> String {
    format!("{} {body}\n", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z"))
}

/// Keep one event per line: control characters become visible escapes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn rotated(dir: &Path, n: usize) -> PathBuf {
    if n == 0 { dir.join(FILE_NAME) } else { dir.join(format!("{FILE_NAME}.{n}")) }
}

/// obd-raw.log → .1 → .2 ...; the oldest beyond KEEP_FILES is deleted.
fn rotate(dir: &Path) {
    let _ = std::fs::remove_file(rotated(dir, KEEP_FILES - 1));
    for n in (0..KEEP_FILES - 1).rev() {
        let from = rotated(dir, n);
        if from.exists() {
            let _ = std::fs::rename(&from, rotated(dir, n + 1));
        }
    }
}

fn open_current(dir: &Path) -> std::io::Result<(std::fs::File, u64)> {
    std::fs::create_dir_all(dir)?;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(rotated(dir, 0))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    Ok((f, len))
}

fn writer(dir: PathBuf, rx: Receiver<String>) {
    let (mut file, mut size) = match open_current(&dir) {
        Ok(v) => v,
        Err(e) => {
            warn!("OBD raw log disabled: cannot write {}: {e}", dir.display());
            // Drain so senders never see a full queue for nothing.
            for _ in rx {}
            return;
        }
    };
    info!("OBD raw log: {} (rotates at {} MiB, keeps {KEEP_FILES})", rotated(&dir, 0).display(), MAX_FILE_BYTES >> 20);
    let mut last_sync = Instant::now();
    let mut dirty = false;
    loop {
        let line = match rx.recv_timeout(SYNC_EVERY) {
            Ok(l) => Some(l),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(line) = line {
            if size + line.len() as u64 > MAX_FILE_BYTES {
                let _ = file.sync_data();
                rotate(&dir);
                match open_current(&dir) {
                    Ok((f, s)) => { file = f; size = s; }
                    Err(e) => {
                        warn!("OBD raw log stopped: cannot reopen after rotation: {e}");
                        for _ in rx {}
                        return;
                    }
                }
            }
            if file.write_all(line.as_bytes()).is_ok() {
                size += line.len() as u64;
                dirty = true;
            }
        }
        if dirty && last_sync.elapsed() >= SYNC_EVERY {
            let _ = file.sync_data();
            dirty = false;
            last_sync = Instant::now();
        }
    }
    let _ = file.sync_data();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_keeps_one_line() {
        assert_eq!(escape("43 00\r43 00\r\r"), "43 00\\r43 00\\r\\r");
        assert_eq!(escape("a\nb\x07"), "a\\nb\\x07");
    }

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("obd-rawlog-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let log = RawLog::open(dir.clone());
            log.tx("010C");
            log.rx(Duration::from_millis(56), "41 0C 1A F8\r");
            log.err(Duration::from_millis(3001), "timeout");
            // Dropping the last sender ends the writer thread, which syncs.
        }
        let path = dir.join(FILE_NAME);
        let mut text = String::new();
        for _ in 0..50 {
            text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.lines().count() == 3 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with(" TX 010C"));
        assert!(lines[1].ends_with(" RX 56ms 41 0C 1A F8\\r"));
        assert!(lines[2].ends_with(" ERR 3001ms timeout"));

        // Rotation shifts files and caps their number.
        for _ in 0..KEEP_FILES + 2 {
            rotate(&dir);
            std::fs::write(dir.join(FILE_NAME), "x").unwrap();
        }
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, KEEP_FILES);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
