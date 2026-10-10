//! The car's head unit identity, as rascar-carplay recorded it from iAP2.
//!
//! rascar-carplay writes `/var/lib/rascar/headunit.json` (atomically, 0644)
//! when the car identifies itself (0x1D01) and when it states its USB power
//! budget (0xAE03): name, model, manufacturer, serial, firmware, hardware,
//! Bluetooth name/MAC, engine type, NMEA sentences offered, power, first/last
//! seen. This side only reads it, on demand. The file is tiny and changes once
//! per session, so there is no cache: every `/api/headunit` reads it again.
//! Path overridable with `RASCAR_HEADUNIT_PATH` (same variable as the writer).

use std::io::Read;

const DEFAULT_PATH: &str = "/var/lib/rascar/headunit.json";
/// Anything larger is not ours; refuse rather than parse it.
const MAX_BYTES: u64 = 64 * 1024;

pub fn path() -> String {
    std::env::var("RASCAR_HEADUNIT_PATH").unwrap_or_else(|_| DEFAULT_PATH.to_string())
}

/// The parsed record, or `None` when no CarPlay session has written one yet
/// (or the file is unreadable, oversized or not a JSON object).
pub fn read_from(path: &str) -> Option<serde_json::Value> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(MAX_BYTES + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_BYTES {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&buf).ok().filter(|v| v.is_object())
}

/// Async wrapper: the read runs on a blocking thread.
pub async fn read() -> Option<serde_json::Value> {
    let p = path();
    tokio::task::spawn_blocking(move || read_from(&p)).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_record_and_rejects_junk() {
        let dir = std::env::temp_dir().join(format!("rascar-headunit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("headunit.json");
        let p = f.to_string_lossy().into_owned();

        assert_eq!(read_from(&p), None, "missing file");
        std::fs::write(&f, r#"{"name":"Jaguar F-PACE","power":{"available_current_ma":2100}}"#).unwrap();
        let v = read_from(&p).unwrap();
        assert_eq!(v["name"], "Jaguar F-PACE");
        assert_eq!(v["power"]["available_current_ma"], 2100);

        std::fs::write(&f, "not json").unwrap();
        assert_eq!(read_from(&p), None);
        std::fs::write(&f, "[1,2]").unwrap();
        assert_eq!(read_from(&p), None, "must be an object");
        let big = format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_BYTES as usize));
        std::fs::write(&f, big).unwrap();
        assert_eq!(read_from(&p), None, "oversized file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
