//! Debug instrumentation helpers shared by every phase.
//!
//! The first real-car runs happen with no debugger attached: the log is the only
//! evidence. This module holds the knobs that make that log conclusive without
//! changing behaviour:
//!
//! - [`wire_trace`] — `RASCAR_WIRE_TRACE=1` enables byte-level dumps (USB bulk,
//!   iAP2 link headers, RTSP bodies). Read once; off by default.
//! - [`hex_dump`] — offset + hex + ASCII dump, truncated with a byte-count note.
//! - [`elapsed_ms`] — monotonic milliseconds since process start, so events from
//!   different layers (FFS, role switch, iAP2) can be ordered and timed.
//! - [`PhaseTimer`] — `Phase X START` / `Phase X END ok|err in N ms` markers.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use tracing::{error, info};

/// Env var enabling byte-level wire tracing.
pub const WIRE_TRACE_ENV: &str = "RASCAR_WIRE_TRACE";

static WIRE_TRACE: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// True when `RASCAR_WIRE_TRACE=1` (read once, cached for the process lifetime).
pub fn wire_trace() -> bool {
    *WIRE_TRACE.get_or_init(|| std::env::var(WIRE_TRACE_ENV).ok().as_deref() == Some("1"))
}

/// Read a boolean tuning knob from the environment.
///
/// These gate the individual candidate fixes for the car's session teardown, so
/// a single deployed binary can A/B them at the car without a rebuild — car
/// attempts are scarce and each one degrades the head unit's willingness to
/// offer CarPlay again. Accepts `1`/`on`/`true`/`yes` and `0`/`off`/`false`/`no`
/// (case-insensitive); anything else, or an unset variable, yields `default`.
pub fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "on" | "true" | "yes" => true,
            "0" | "off" | "false" | "no" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

/// Pin the process start instant. Call first thing in `main`; later callers of
/// [`elapsed_ms`] fall back to "first use" if it was never called.
pub fn init_clock() {
    START.get_or_init(Instant::now);
}

/// Monotonic milliseconds since [`init_clock`].
pub fn elapsed_ms() -> u128 {
    START.get_or_init(Instant::now).elapsed().as_millis()
}

/// Offset + hex + ASCII dump of at most `max_len` bytes, 16 per line.
///
/// Truncated input ends with a `(N more bytes)` note so a short dump is never
/// mistaken for a short message.
pub fn hex_dump(bytes: &[u8], max_len: usize) -> String {
    let shown = bytes.len().min(max_len);
    let mut out = String::with_capacity(shown * 4 + 32);
    for (line_no, chunk) in bytes[..shown].chunks(16).enumerate() {
        let _ = write!(out, "\n  {:04x}  ", line_no * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
        }
        out.push_str(" |");
        for &b in chunk {
            out.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' });
        }
        out.push('|');
    }
    if bytes.is_empty() {
        out.push_str(" <empty>");
    }
    if bytes.len() > shown {
        let _ = write!(out, "\n  ({} more bytes)", bytes.len() - shown);
    }
    out
}

/// Short single-line hex prefix (`ff 5a 00 06 …`) for status lines.
pub fn hex_prefix(bytes: &[u8], max_len: usize) -> String {
    let shown = bytes.len().min(max_len);
    let mut s = String::with_capacity(shown * 3 + 8);
    for (i, b) in bytes[..shown].iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    if bytes.len() > shown {
        s.push_str(" …");
    }
    if bytes.is_empty() {
        s.push_str("<empty>");
    }
    s
}

/// Truncate text to `max_len` bytes (on a char boundary) with a note.
pub fn truncate_text(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut end = max_len;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} more bytes)", &text[..end], text.len() - end)
}

/// Best-effort read of a sysfs/debugfs/procfs attribute. Never fails: an error is
/// rendered inline as `<error text>` so a snapshot can always be logged.
pub fn read_attr(path: impl AsRef<Path>) -> String {
    match std::fs::read(path.as_ref()) {
        // Device-tree strings are NUL-terminated; sysfs values end in '\n'.
        Ok(raw) => String::from_utf8_lossy(&raw)
            .trim_end_matches(['\0', '\n'])
            .replace('\0', ",")
            .trim()
            .to_string(),
        Err(e) => format!("<{e}>"),
    }
}

/// Rate limiter for hot-path traces: true for the first `first` events, then
/// for every `every`-th event.
pub fn sampled(count: u64, first: u64, every: u64) -> bool {
    count < first || (every > 0 && count % every == 0)
}

/// `Phase X START` / `Phase X END ok|err in N ms` markers around a phase.
pub struct PhaseTimer {
    name: &'static str,
    start: Instant,
}

impl PhaseTimer {
    pub fn start(name: &'static str) -> Self {
        info!("Phase {name} START (t+{} ms)", elapsed_ms());
        Self {
            name,
            start: Instant::now(),
        }
    }

    /// Log the end marker for `result` (only borrowed — the caller keeps it).
    pub fn end<T, E: std::fmt::Display>(self, result: &Result<T, E>) {
        let ms = self.start.elapsed().as_millis();
        match result {
            Ok(_) => info!("Phase {} END ok in {ms} ms (t+{} ms)", self.name, elapsed_ms()),
            Err(e) => error!("Phase {} END err in {ms} ms (t+{} ms): {e:#}", self.name, elapsed_ms()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_formats_and_truncates() {
        let data: Vec<u8> = (0u8..40).collect();
        let dump = hex_dump(&data, 20);
        assert!(dump.contains("0000  00 01 02"), "{dump}");
        assert!(dump.contains("0010  10 11 12 13"), "{dump}");
        assert!(dump.contains("(20 more bytes)"), "{dump}");
        assert!(hex_dump(b"AB", 16).contains("|AB|"));
        assert!(hex_dump(&[], 16).contains("<empty>"));
    }

    #[test]
    fn hex_prefix_and_truncate_text() {
        assert_eq!(hex_prefix(&[0xff, 0x5a, 0x00], 2), "ff 5a …");
        assert_eq!(truncate_text("héllo", 2), "h… (5 more bytes)");
        assert_eq!(truncate_text("ok", 10), "ok");
    }

    #[test]
    fn sampled_rate_limits() {
        let hits: Vec<u64> = (0..700).filter(|&c| sampled(c, 5, 300)).collect();
        assert_eq!(hits, vec![0, 1, 2, 3, 4, 300, 600]);
    }
}
