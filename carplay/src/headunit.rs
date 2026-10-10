//! The head unit's identity, as it describes itself over iAP2, published for
//! the rest of the system.
//!
//! The car tells us who it is in `IdentificationInformation` (0x1D01): name,
//! model, manufacturer, serial, firmware, hardware, its Bluetooth transport,
//! its vehicle information and the NMEA sentences it can stream. It then tells
//! us how much USB current it offers in `PowerSourceUpdate` (0xAE03). Until now
//! all of that was logged and forgotten; this module keeps the latest of both
//! in a small JSON file that `rascar-app` (dashboard) and `rascar-agent`
//! (status reporting) read:
//!
//! ```json
//! { "name": "Jaguar F-PACE", "model": "Jaguar F-PACE", "manufacturer": "Jaguar",
//!   "serial": "XXXX-000000-XX_0000000", "firmware": "S22A_22.32.4-928505",
//!   "hardware": "IxC.1.0", "bluetooth": { "name": "Jaguar", "mac": "00:24:D6:00:00:01" },
//!   "engine_type": 0, "location_sentences": ["GPGGA", "GPRMC", "GPGSV", "PASCD"],
//!   "power": { "available_current_ma": 2100, "should_charge": true },
//!   "first_seen": "2026-01-01T12:00:00Z", "last_seen": "2026-01-01T12:00:00Z" }
//! ```
//!
//! Path: `/var/lib/rascar/headunit.json`, overridable with
//! `RASCAR_HEADUNIT_PATH`. Written atomically and durably (temp file, fsync,
//! rename, fsync of the directory: the car cuts power without warning) and
//! world-readable, because the readers do not run as root.
//!
//! Nothing here touches the wire: it only observes messages the session has
//! already received, and every write runs on a blocking thread so a slow disk
//! can never delay an iAP2 reply.

use crate::iap2::control::{
    nul_string, AccessoryIdentification, ControlMessage, PARAM_BLUETOOTH_TRANSPORT_COMPONENT,
    PARAM_VEHICLE_INFORMATION_COMPONENT,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_PATH: &str = "/var/lib/rascar/headunit.json";

/// What the car offers us on USB power (`PowerSourceUpdate`, 0xAE03).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Power {
    /// `AvailableCurrentForDevice` (param 0, u16, mA).
    pub available_current_ma: Option<u16>,
    /// `DeviceBatteryShouldChargeIfPowerIsPresent` (param 1, bool).
    pub should_charge: Option<bool>,
}

impl Power {
    /// Decode a `PowerSourceUpdate`. Layout pinned to catplay_csm
    /// (`msg/power.rs`: 0 = u16, 1 = bool) and to the Jaguar's own message
    /// (`0x0000(2B: 08 34), 0x0001(1B: 01)` = 2100 mA, charge).
    pub fn from_message(msg: &ControlMessage) -> Power {
        Power {
            available_current_ma: msg
                .param(0x0000)
                .filter(|d| d.len() == 2)
                .map(|d| u16::from_be_bytes([d[0], d[1]])),
            should_charge: msg.param(0x0001).filter(|d| d.len() == 1).map(|d| d[0] != 0),
        }
    }
}

/// The record behind `headunit.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadUnit {
    pub name: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub hardware: Option<String>,
    pub bluetooth_name: Option<String>,
    pub bluetooth_mac: Option<String>,
    /// `VehicleInformationComponent` EngineType: 0 gasoline, 1 diesel,
    /// 2 electric, 3 CNG (catplay_csm `EngineTypes`).
    pub engine_type: Option<u8>,
    pub location_sentences: Vec<String>,
    pub power: Power,
    /// Unix seconds.
    pub first_seen: u64,
    pub last_seen: u64,
}

impl HeadUnit {
    pub fn from_identification(id: &AccessoryIdentification, now: u64) -> HeadUnit {
        let bt = id.component(PARAM_BLUETOOTH_TRANSPORT_COMPONENT).unwrap_or_default();
        let vehicle = id.component(PARAM_VEHICLE_INFORMATION_COMPONENT).unwrap_or_default();
        let sub = |group: &[(u16, &[u8])], sid: u16| group.iter().find(|(i, _)| *i == sid).map(|(_, d)| d.to_vec());
        HeadUnit {
            name: id.string_param(0x0000),
            model: id.string_param(0x0001),
            manufacturer: id.string_param(0x0002),
            serial: id.string_param(0x0003),
            firmware: id.string_param(0x0004),
            hardware: id.string_param(0x0005),
            bluetooth_name: sub(&bt, 1).map(|d| nul_string(&d)),
            bluetooth_mac: sub(&bt, 3).filter(|d| d.len() == 6).map(|d| {
                d.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
            }),
            engine_type: sub(&vehicle, 2).and_then(|d| d.first().copied()),
            location_sentences: id.location_sentences().into_iter().map(String::from).collect(),
            power: Power::default(),
            first_seen: now,
            last_seen: now,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "model": self.model,
            "manufacturer": self.manufacturer,
            "serial": self.serial,
            "firmware": self.firmware,
            "hardware": self.hardware,
            "bluetooth": { "name": self.bluetooth_name, "mac": self.bluetooth_mac },
            "engine_type": self.engine_type,
            "location_sentences": self.location_sentences,
            "power": {
                "available_current_ma": self.power.available_current_ma,
                "should_charge": self.power.should_charge,
            },
            "first_seen": rfc3339(self.first_seen),
            "last_seen": rfc3339(self.last_seen),
        })
    }

    /// Read back what `to_json` wrote (only what the merge needs: identity
    /// fields for the firmware check, first_seen and power).
    pub fn from_json(v: &Value) -> HeadUnit {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let p = v.get("power");
        HeadUnit {
            name: s("name"),
            model: s("model"),
            manufacturer: s("manufacturer"),
            serial: s("serial"),
            firmware: s("firmware"),
            hardware: s("hardware"),
            bluetooth_name: v.pointer("/bluetooth/name").and_then(Value::as_str).map(String::from),
            bluetooth_mac: v.pointer("/bluetooth/mac").and_then(Value::as_str).map(String::from),
            engine_type: v.get("engine_type").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok()),
            location_sentences: v
                .get("location_sentences")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            power: Power {
                available_current_ma: p
                    .and_then(|p| p.get("available_current_ma"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok()),
                should_charge: p.and_then(|p| p.get("should_charge")).and_then(Value::as_bool),
            },
            first_seen: v.get("first_seen").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(0),
            last_seen: v.get("last_seen").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(0),
        }
    }

    /// One-line identity for the session log.
    pub fn summary(&self) -> String {
        let o = |v: &Option<String>| v.clone().unwrap_or_else(|| "?".into());
        format!(
            "{} ({}), serial {}, firmware {}, hardware {}, Bluetooth {} {}, location [{}]",
            o(&self.name),
            o(&self.manufacturer),
            o(&self.serial),
            o(&self.firmware),
            o(&self.hardware),
            o(&self.bluetooth_name),
            o(&self.bluetooth_mac),
            self.location_sentences.join(" ")
        )
    }
}

/// The file plus the record last written to it.
pub struct Store {
    path: PathBuf,
    current: Option<HeadUnit>,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Store {
        Store { path: path.into(), current: None }
    }

    /// The previous record: what this process wrote last, else the file.
    fn previous(&self) -> Option<HeadUnit> {
        self.current.clone().or_else(|| read_bounded(&self.path).map(|v| HeadUnit::from_json(&v)))
    }

    /// A fresh identification: log it, warn on a firmware change, persist.
    /// Returns the previous firmware when it differed (for tests).
    pub fn on_identification(&mut self, id: &AccessoryIdentification, now: u64) -> Option<String> {
        let mut rec = HeadUnit::from_identification(id, now);
        let prev = self.previous();
        let mut firmware_changed = None;
        if let Some(prev) = &prev {
            let same_unit = prev.serial == rec.serial;
            if same_unit && prev.first_seen > 0 {
                rec.first_seen = prev.first_seen;
            }
            // The car's USB budget is a property of the unit: keep it until
            // this session's 0xAE03 refreshes it.
            if same_unit {
                rec.power = prev.power;
            }
            if prev.firmware.is_some() && rec.firmware.is_some() && prev.firmware != rec.firmware {
                warn!(
                    "head unit firmware changed: {} -> {} (the head unit was updated; compare CarPlay behaviour before/after)",
                    prev.firmware.as_deref().unwrap_or("?"),
                    rec.firmware.as_deref().unwrap_or("?")
                );
                firmware_changed = prev.firmware.clone();
            }
        }
        info!("head unit: {}", rec.summary());
        self.commit(rec);
        firmware_changed
    }

    /// A `PowerSourceUpdate` (0xAE03): record what the car offers us.
    pub fn on_power(&mut self, power: Power, now: u64) {
        let mut rec = self.previous().unwrap_or_else(|| HeadUnit { first_seen: now, ..Default::default() });
        if rec.power != power {
            info!(
                "head unit power: {} mA available, should charge {:?}",
                power.available_current_ma.map(|m| m.to_string()).unwrap_or_else(|| "?".into()),
                power.should_charge
            );
        }
        rec.power = power;
        rec.last_seen = now;
        self.commit(rec);
    }

    fn commit(&mut self, rec: HeadUnit) {
        let mut body = serde_json::to_vec_pretty(&rec.to_json()).unwrap_or_default();
        body.push(b'\n');
        match write_durable(&self.path, &body) {
            Ok(()) => debug!("head unit record written to {}", self.path.display()),
            Err(e) => warn!("head unit: could not write {}: {e}", self.path.display()),
        }
        self.current = Some(rec);
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: std::sync::OnceLock<Mutex<Store>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| {
        let path = std::env::var("RASCAR_HEADUNIT_PATH").unwrap_or_else(|_| DEFAULT_PATH.to_string());
        Mutex::new(Store::new(path))
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record an identification, off the async thread. Unit tests of the iAP2
/// driver must not write the real file, so this is a no-op under `cfg(test)`;
/// `Store` is tested directly.
pub fn record_identification(id: &AccessoryIdentification) {
    if cfg!(test) {
        return;
    }
    let id = id.clone();
    tokio::task::spawn_blocking(move || {
        let mut s = store().lock().unwrap_or_else(|p| p.into_inner());
        s.on_identification(&id, now_secs());
    });
}

/// Record a `PowerSourceUpdate` (0xAE03), off the async thread.
pub fn record_power_source(msg: &ControlMessage) {
    if cfg!(test) {
        return;
    }
    let power = Power::from_message(msg);
    tokio::task::spawn_blocking(move || {
        let mut s = store().lock().unwrap_or_else(|p| p.into_inner());
        s.on_power(power, now_secs());
    });
}

/// Read a small JSON file, refusing anything implausibly large.
fn read_bounded(path: &Path) -> Option<Value> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(64 * 1024).read_to_end(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Replace `path` atomically and durably, world-readable (0644 file, 0755 dir).
fn write_durable(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&tmp)?;
        // The process umask could narrow the mode given to open().
        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Unix seconds -> `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `YYYY-MM-DDTHH:MM:SSZ` -> Unix seconds (only the form `rfc3339` writes).
fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?);
    let secs = days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?;
    u64::try_from(secs).ok()
}

// Howard Hinnant's civil-date algorithms (same as gps_clock.rs).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iap2::control::jaguar_fixture;

    // 2026-01-01T12:00:00Z, when the fixture was captured.
    const T0: u64 = 1_767_268_800;

    fn power_msg(ma: &[u8], charge: &[u8]) -> ControlMessage {
        ControlMessage {
            msg_id: 0xAE03,
            params: vec![(0x0000, bytes::Bytes::copy_from_slice(ma)), (0x0001, bytes::Bytes::copy_from_slice(charge))],
        }
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(T0), "2026-01-01T12:00:00Z");
        assert_eq!(parse_rfc3339("2026-01-01T12:00:00Z"), Some(T0));
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn the_jaguar_identification_becomes_the_expected_json() {
        let rec = HeadUnit::from_identification(&jaguar_fixture::identification(), T0);
        let v = rec.to_json();
        assert_eq!(v["name"], "Jaguar F-PACE");
        assert_eq!(v["model"], "Jaguar F-PACE");
        assert_eq!(v["manufacturer"], "Jaguar");
        assert_eq!(v["serial"], "XXXX-000000-XX_0000000");
        assert_eq!(v["firmware"], "S22A_22.32.4-928505");
        assert_eq!(v["hardware"], "IxC.1.0");
        assert_eq!(v["bluetooth"]["name"], "Jaguar");
        assert_eq!(v["bluetooth"]["mac"], "00:24:D6:00:00:01");
        assert_eq!(v["engine_type"], 0);
        assert_eq!(v["location_sentences"], json!(["GPGGA", "GPRMC", "GPGSV", "PASCD"]));
        assert_eq!(v["power"]["available_current_ma"], Value::Null);
        assert_eq!(v["first_seen"], "2026-01-01T12:00:00Z");
        assert_eq!(HeadUnit::from_json(&v), rec, "the file must read back to the same record");
    }

    #[test]
    fn the_jaguar_power_source_update_is_2100_ma_and_charge() {
        let p = Power::from_message(&power_msg(&[0x08, 0x34], &[0x01]));
        assert_eq!(p, Power { available_current_ma: Some(2100), should_charge: Some(true) });
        // Wrong widths are ignored, not misread.
        assert_eq!(Power::from_message(&power_msg(&[0x08], &[])), Power::default());
    }

    #[test]
    fn store_writes_a_world_readable_file_and_keeps_first_seen() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/headunit.json");
        let ident = jaguar_fixture::identification();

        let mut s = Store::new(&path);
        assert_eq!(s.on_identification(&ident, T0), None);
        s.on_power(Power::from_message(&power_msg(&[0x08, 0x34], &[0x01])), T0 + 1);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["power"], json!({ "available_current_ma": 2100, "should_charge": true }));
        assert_eq!(v["last_seen"], "2026-01-01T12:00:01Z");

        // A new process (fresh Store) sees the next session of the same car:
        // first_seen and the known power budget survive.
        let mut s2 = Store::new(&path);
        assert_eq!(s2.on_identification(&ident, T0 + 3600), None);
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["first_seen"], "2026-01-01T12:00:00Z");
        assert_eq!(v["last_seen"], "2026-01-01T13:00:00Z");
        assert_eq!(v["power"]["available_current_ma"], 2100);
    }

    #[test]
    fn a_firmware_update_is_detected_against_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headunit.json");
        let mut old = HeadUnit::from_identification(&jaguar_fixture::identification(), T0);
        old.firmware = Some("S22A_22.20.1-900000".into());
        std::fs::write(&path, serde_json::to_vec(&old.to_json()).unwrap()).unwrap();

        let mut s = Store::new(&path);
        assert_eq!(
            s.on_identification(&jaguar_fixture::identification(), T0 + 60),
            Some("S22A_22.20.1-900000".into())
        );
        // Written now, so the next session does not warn again.
        let mut s2 = Store::new(&path);
        assert_eq!(s2.on_identification(&jaguar_fixture::identification(), T0 + 120), None);
    }

    #[test]
    fn a_corrupt_or_huge_previous_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headunit.json");
        std::fs::write(&path, vec![b'{'; 200 * 1024]).unwrap();
        let mut s = Store::new(&path);
        assert_eq!(s.on_identification(&jaguar_fixture::identification(), T0), None);
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["name"], "Jaguar F-PACE");
    }
}
