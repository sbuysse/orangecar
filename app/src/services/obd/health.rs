//! Engine health data that is not a plain mode 01 value: on-board monitor test
//! results (mode 06), in-use performance counters (09 08), readiness monitor
//! bits (01 01 / 01 41) and the manufacturer tyre-pressure reads (mode 22).
//!
//! Only parsing and the guards live here; the poll loop in `mod.rs` decides
//! when to ask.
//!
//! Sources:
//! - Mode 06 record layout and the unit-and-scaling (UAS) table: SAE J1979
//!   appendix E as implemented by python-OBD (`obd/UnitsAndScaling.py`,
//!   `obd/decoders.py::monitor`, commit a378bdd8): CAN replies are `46` then
//!   9-byte records `MID TID UAS VALh VALl MINh MINl MAXh MAXl`.
//! - 09 08 counter order: SAE J1979 InfoType 08 (Wikipedia "OBD-II PIDs",
//!   Service 09 PID 08): OBDCOND, IGNCNTR, then COMP/COND pairs.
//! - 01 41 / 01 01 monitor bits: OBDb SAEJ1979 signalset (`signalsets/v3/
//!   default.json`, command 01 41: MIS_ENA bix 15 ... CAT_CMPL bix 31).
//! - Tyre pressures: OBDb Jaguar-F-PACE (`signalsets/v3/default.json`, commit
//!   ba0bea8d): request header 751, `22 2076/2077/2078/2079` = FL/FR/RR/RL,
//!   8 bits at bix 8 of the data after the DID, × 0.01373 bar; test cases
//!   MY2017 and MY2020 (`759 05 62 20 76 00 92` = 2.00 bar). MY2018/2019 are
//!   in the file's debug filter: not validated on this car.

use super::elm;
use serde::Serialize;

// --- unit and scaling ---------------------------------------------------------

/// One SAE J1979 unit-and-scaling id: value per bit, offset, unit. Whether
/// the 16-bit fields are signed is bit 7 of the id (00-7F unsigned, 81-FE
/// signed), see `MonitorTest::raw`.
struct Uas {
    scale: f64,
    offset: f64,
    unit: &'static str,
}

const fn uas(scale: f64, unit: &'static str) -> Uas {
    Uas { scale, offset: 0.0, unit }
}

/// The J1979 UAS ids with a defined scaling (python-OBD `UAS_IDS`). An id
/// missing here leaves the test value raw.
fn uas_lookup(id: u8) -> Option<Uas> {
    Some(match id {
        0x01 => uas(1.0, ""),
        0x02 => uas(0.1, ""),
        0x03 => uas(0.01, ""),
        0x04 => uas(0.001, ""),
        0x05 => uas(0.000_030_5, ""),
        0x06 => uas(0.000_305, ""),
        0x07 => uas(0.25, "rpm"),
        0x08 => uas(0.01, "km/h"),
        0x09 => uas(1.0, "km/h"),
        0x0A => uas(0.122, "mV"),
        0x0B => uas(0.001, "V"),
        0x0C => uas(0.01, "V"),
        0x0D => uas(0.003_906_25, "mA"),
        0x0E => uas(0.001, "A"),
        0x0F => uas(0.01, "A"),
        0x10 => uas(1.0, "ms"),
        0x11 => uas(100.0, "ms"),
        0x12 => uas(1.0, "s"),
        0x13 => uas(1.0, "mΩ"),
        0x14 => uas(1.0, "Ω"),
        0x15 => uas(1.0, "kΩ"),
        0x16 => Uas { scale: 0.1, offset: -40.0, unit: "°C" },
        0x17 => uas(0.01, "kPa"),
        0x18 => uas(0.0117, "kPa"),
        0x19 => uas(0.079, "kPa"),
        0x1A => uas(1.0, "kPa"),
        0x1B => uas(10.0, "kPa"),
        0x1C => uas(0.01, "°"),
        0x1D => uas(0.5, "°"),
        0x1E => uas(0.000_030_5, "λ"),
        0x1F => uas(0.05, "AFR"),
        0x20 => uas(0.003_906_25, "ratio"),
        0x21 => uas(1.0, "mHz"),
        0x22 => uas(1.0, "Hz"),
        0x23 => uas(1.0, "kHz"),
        0x24 => uas(1.0, "counts"),
        0x25 => uas(1.0, "km"),
        0x26 => uas(0.1, "mV/ms"),
        0x27 => uas(0.01, "g/s"),
        0x28 => uas(1.0, "g/s"),
        0x29 => uas(0.25, "Pa/s"),
        0x2A => uas(0.001, "kg/h"),
        0x2B => uas(1.0, "switches"),
        0x2C => uas(0.01, "g/cyl"),
        0x2D => uas(0.01, "mg/stroke"),
        0x2F => uas(0.01, "%"),
        0x30 => uas(0.001_526, "%"),
        0x31 => uas(0.001, "L"),
        0x33 => uas(0.000_244_14, "λ"),
        0x34 => uas(1.0, "min"),
        0x35 => uas(10.0, "ms"),
        0x36 => uas(0.01, "g"),
        0x37 => uas(0.1, "g"),
        0x38 => uas(1.0, "g"),
        0x39 => Uas { scale: 0.01, offset: -327.68, unit: "%" },
        0x3A => uas(0.001, "g"),
        0x3B => uas(0.0001, "g"),
        0x3C => uas(0.1, "µs"),
        0x3D => uas(0.01, "mA"),
        0x3F => uas(0.01, "L"),
        0x40 => uas(1.0, "ppm"),
        0x41 => uas(0.01, "µA"),
        0x81 => uas(1.0, ""),
        0x82 => uas(0.1, ""),
        0x83 => uas(0.01, ""),
        0x84 => uas(0.001, ""),
        0x85 => uas(0.000_030_5, ""),
        0x86 => uas(0.000_305, ""),
        0x87 => uas(1.0, "ppm"),
        0x8A => uas(0.122, "mV"),
        0x8B => uas(0.001, "V"),
        0x8C => uas(0.01, "V"),
        0x8D => uas(0.003_906_25, "mA"),
        0x8E => uas(0.001, "A"),
        0x90 => uas(1.0, "ms"),
        0x96 => uas(0.1, "°C"),
        0x99 => uas(0.1, "kPa"),
        0x9C => uas(0.01, "°"),
        0x9D => uas(0.5, "°"),
        0xA8 => uas(1.0, "g/s"),
        0xA9 => uas(0.25, "Pa/s"),
        0xAD => uas(0.01, "mg/stroke"),
        0xAE => uas(0.1, "mg/stroke"),
        0xAF => uas(0.01, "%"),
        0xB0 => uas(0.003_052, "%"),
        0xB1 => uas(2.0, "mV/s"),
        0xFC => uas(0.01, "kPa"),
        0xFD => uas(0.001, "kPa"),
        0xFE => uas(0.25, "Pa"),
        _ => return None,
    })
}

// --- mode 06 ------------------------------------------------------------------

/// One on-board monitor test result (mode 06, CAN format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorTest {
    pub mid: u8,
    pub tid: u8,
    pub uas: u8,
    pub value: u16,
    pub min: u16,
    pub max: u16,
}

impl MonitorTest {
    /// Raw 16-bit field in the UAS's signedness (signed ids are 0x81-0xFE).
    fn raw(&self, v: u16) -> i32 {
        if self.uas & 0x80 != 0 { v as i16 as i32 } else { v as i32 }
    }

    /// Within its limits. Scaling is linear with a positive factor, so the
    /// raw comparison in the right signedness is the scaled one.
    pub fn passed(&self) -> bool {
        let (v, lo, hi) = (self.raw(self.value), self.raw(self.min), self.raw(self.max));
        lo <= v && v <= hi
    }

    fn scaled(&self, v: u16) -> Option<f64> {
        uas_lookup(self.uas).map(|u| self.raw(v) as f64 * u.scale + u.offset)
    }

    pub fn view(&self) -> TestView {
        TestView {
            mid: format!("{:02X}", self.mid),
            tid: format!("{:02X}", self.tid),
            monitor: mid_name(self.mid),
            test: tid_name(self.tid),
            value: self.scaled(self.value),
            min: self.scaled(self.min),
            max: self.scaled(self.max),
            unit: uas_lookup(self.uas).map(|u| u.unit).unwrap_or(""),
            raw: format!("UAS {:02X}: {:04X} [{:04X}..{:04X}]", self.uas, self.value, self.min, self.max),
            pass: self.passed(),
        }
    }
}

/// A test result as served to the UI and `/api/obd`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TestView {
    pub mid: String,
    pub tid: String,
    pub monitor: String,
    pub test: String,
    /// Scaled with the J1979 UAS table; `None` for an id it does not define
    /// (`raw` still has the bytes).
    pub value: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: &'static str,
    pub raw: String,
    pub pass: bool,
}

/// Every test record in a mode 06 reply. On CAN each record repeats its MID,
/// so a reply to one MID with several TIDs is `46` + n × 9 bytes; a trailing
/// partial record (padding) is dropped.
pub fn parse_mode06(resp: &str) -> Vec<MonitorTest> {
    let mut out = Vec::new();
    for m in elm::messages(resp) {
        if m.first() != Some(&0x46) {
            continue;
        }
        for r in m[1..].chunks_exact(9) {
            // MID 00/20/40... are support bitmaps, not tests.
            if r[0] % 0x20 == 0 {
                continue;
            }
            out.push(MonitorTest {
                mid: r[0],
                tid: r[1],
                uas: r[2],
                value: u16::from_be_bytes([r[3], r[4]]),
                min: u16::from_be_bytes([r[5], r[6]]),
                max: u16::from_be_bytes([r[7], r[8]]),
            });
        }
    }
    out
}

/// Monitor name of a J1979 OBDMID.
pub fn mid_name(mid: u8) -> String {
    let bank_sensor = |n: u8| format!("B{}S{}", (n - 1) / 4 + 1, (n - 1) % 4 + 1);
    match mid {
        0x01..=0x10 => format!("O2 sensor {}", bank_sensor(mid)),
        0x21..=0x24 => format!("Catalyst B{}", mid - 0x20),
        0x31..=0x34 => format!("EGR B{}", mid - 0x30),
        0x35..=0x38 => format!("VVT B{}", mid - 0x34),
        0x39 => "EVAP (cap off / 0.150\")".into(),
        0x3A => "EVAP (0.090\")".into(),
        0x3B => "EVAP (0.040\")".into(),
        0x3C => "EVAP (0.020\")".into(),
        0x3D => "Purge flow".into(),
        0x41..=0x50 => format!("O2 heater {}", bank_sensor(mid - 0x40)),
        0x61..=0x64 => format!("Heated catalyst B{}", mid - 0x60),
        0x71..=0x74 => format!("Secondary air {}", mid - 0x70),
        0x81..=0x84 => format!("Fuel system B{}", mid - 0x80),
        0x85..=0x86 => format!("Boost pressure B{}", mid - 0x84),
        0x90..=0x91 => format!("NOx absorber B{}", mid - 0x8F),
        0x98..=0x99 => format!("NOx catalyst B{}", mid - 0x97),
        0xA1 => "Misfire (all cylinders)".into(),
        0xA2..=0xAD => format!("Misfire cylinder {}", mid - 0xA1),
        0xB0..=0xB1 => format!("PM filter B{}", mid - 0xAF),
        _ => format!("MID {mid:02X}"),
    }
}

/// Standard test ids (J1979 TIDs 01-0C); 0x80 and up are manufacturer tests.
pub fn tid_name(tid: u8) -> String {
    match tid {
        0x01 => "Rich to lean threshold voltage".into(),
        0x02 => "Lean to rich threshold voltage".into(),
        0x03 => "Low voltage for switch time".into(),
        0x04 => "High voltage for switch time".into(),
        0x05 => "Rich to lean switch time".into(),
        0x06 => "Lean to rich switch time".into(),
        0x07 => "Minimum voltage".into(),
        0x08 => "Maximum voltage".into(),
        0x09 => "Time between transitions".into(),
        0x0A => "Sensor period".into(),
        TID_MISFIRE_AVG => "Misfires, average of last 10 drives".into(),
        TID_MISFIRE_NOW => "Misfires, current/last drive".into(),
        _ => format!("Test {tid:02X}"),
    }
}

/// Misfire TIDs (J1979): EWMA over the last ten driving cycles, and the count
/// of the current (or, before it has run, the last) driving cycle.
const TID_MISFIRE_AVG: u8 = 0x0B;
const TID_MISFIRE_NOW: u8 = 0x0C;

/// Misfire counts of one cylinder (`cyl` 0 = MID A1, all cylinders).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CylinderMisfire {
    pub cyl: u8,
    /// TID 0C: this (or the last) driving cycle.
    pub this_drive: Option<f64>,
    /// TID 0B: average of the last ten driving cycles.
    pub avg10: Option<f64>,
}

/// Per-cylinder misfire counts out of the misfire MIDs (A1-AD), cylinders in
/// order, the all-cylinder MID A1 first when present.
pub fn misfires(tests: &[MonitorTest]) -> Vec<CylinderMisfire> {
    let mut out: Vec<CylinderMisfire> = Vec::new();
    for t in tests.iter().filter(|t| (0xA1..=0xAD).contains(&t.mid)) {
        let cyl = t.mid - 0xA1;
        let idx = match out.iter().position(|c| c.cyl == cyl) {
            Some(i) => i,
            None => {
                out.push(CylinderMisfire { cyl, this_drive: None, avg10: None });
                out.len() - 1
            }
        };
        // Counts are UAS 24; another id is still a count, scaled.
        let v = t.scaled(t.value).or(Some(t.raw(t.value) as f64));
        match t.tid {
            TID_MISFIRE_NOW => out[idx].this_drive = v,
            TID_MISFIRE_AVG => out[idx].avg10 = v,
            _ => {}
        }
    }
    out.sort_by_key(|c| c.cyl);
    out
}

/// Supported-id bitmap reply (`46 00 b b b b`, `49 00 ...`, `41 00 ...`) for
/// `base`, OR-ed over the answering ECUs. Returns the supported ids in
/// `base+1 ..= base+32`.
pub fn parse_bitmap(resp: &str, sid: u8, base: u8) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut any = false;
    for m in elm::messages(resp) {
        if m.len() >= 6 && m[0] == sid && m[1] == base {
            bits |= u32::from_be_bytes([m[2], m[3], m[4], m[5]]);
            any = true;
        }
    }
    if !any {
        return None;
    }
    Some((0..32u32).filter(|i| bits & (0x8000_0000 >> i) != 0).map(|i| base.wrapping_add(i as u8 + 1)).collect())
}

// --- 09 08 in-use performance tracking ----------------------------------------

/// Monitor completion counter pairs of 09 08 (spark ignition), after OBDCOND
/// and IGNCNTR, in J1979 order.
const IPT_MONITORS: &[&str] = &[
    "Catalyst B1",
    "Catalyst B2",
    "O2 sensor B1",
    "O2 sensor B2",
    "EGR / VVT",
    "Secondary air",
    "EVAP",
    "Secondary O2 B1",
    "Secondary O2 B2",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IptMonitor {
    pub name: &'static str,
    /// Times the monitor completed (COMP).
    pub comp: u16,
    /// Drives in which its conditions were met (COND).
    pub cond: u16,
    /// COMP / COND; `None` when it never had the conditions.
    pub ratio: Option<f64>,
}

/// In-use performance tracking (09 08).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Ipt {
    /// IGNCNTR: ignition cycles counted.
    pub ignition_cycles: u16,
    /// OBDCOND: drives that met the general OBD monitoring conditions.
    pub obd_conditions: u16,
    pub monitors: Vec<IptMonitor>,
}

/// Decode a 09 08 reply. CAN: `49 08 NODI` then NODI 16-bit counters (16 or
/// 20 for spark engines). K-line replies (one message per two counters) are
/// not handled; the car is CAN.
pub fn parse_ipt(resp: &str) -> Option<Ipt> {
    let m = elm::messages(resp).into_iter().find(|m| m.len() >= 3 && m[0] == 0x49 && m[1] == 0x08)?;
    let nodi = m[2] as usize;
    let counters: Vec<u16> = m[3..].chunks_exact(2).take(nodi).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    if counters.len() < 2 || counters.len() < nodi {
        return None;
    }
    let monitors = IPT_MONITORS
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            let comp = *counters.get(2 + 2 * i)?;
            let cond = *counters.get(3 + 2 * i)?;
            Some(IptMonitor { name, comp, cond, ratio: (cond > 0).then(|| comp as f64 / cond as f64) })
        })
        .collect();
    Some(Ipt { obd_conditions: counters[0], ignition_cycles: counters[1], monitors })
}

// --- readiness monitors (01 01 / 01 41) ---------------------------------------

/// Monitor names by bit for bytes B (bits 0-2) and C/D (bits 0-7), spark
/// ignition, then compression ignition (byte B bit 3 set).
const CONT_MONITORS: [&str; 3] = ["Misfire", "Fuel system", "Components"];
const SPARK_MONITORS: [&str; 8] =
    ["Catalyst", "Heated catalyst", "EVAP", "Secondary air", "A/C refrigerant", "O2 sensor", "O2 heater", "EGR / VVT"];
const DIESEL_MONITORS: [&str; 8] =
    ["NMHC catalyst", "NOx / SCR", "", "Boost pressure", "", "Exhaust gas sensor", "PM filter", "EGR / VVT"];

/// One monitor of a 01 01 or 01 41 reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorBit {
    pub name: &'static str,
    /// 01 01: supported by the car; 01 41: enabled this drive cycle.
    pub available: bool,
    /// 01 01: ready since the codes were cleared; 01 41: complete this
    /// drive cycle. The bit is "incomplete", inverted here.
    pub complete: bool,
}

/// Monitor bits of a 01 01 or 01 41 reply, data bytes A-D. Byte B bits 0-2
/// are available, bits 4-6 incomplete; C is available, D incomplete.
pub fn monitor_bits(d: &[u8]) -> Vec<MonitorBit> {
    if d.len() < 4 {
        return Vec::new();
    }
    let (b, c, dd) = (d[1], d[2], d[3]);
    let mut out: Vec<MonitorBit> = CONT_MONITORS
        .iter()
        .enumerate()
        .map(|(i, name)| MonitorBit { name, available: b & (1 << i) != 0, complete: b & (0x10 << i) == 0 })
        .collect();
    let names = if b & 0x08 != 0 { &DIESEL_MONITORS } else { &SPARK_MONITORS };
    for (i, name) in names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        out.push(MonitorBit { name, available: c & (1 << i) != 0, complete: dd & (1 << i) == 0 });
    }
    out
}

/// A readiness monitor as the UI shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MonitorView {
    pub name: &'static str,
    /// Ready since the last clear (01 01); `None` before it was read.
    pub ready: Option<bool>,
    /// Enabled this drive cycle (01 41).
    pub enabled: Option<bool>,
    /// Completed this drive cycle (01 41); meaningful only when enabled.
    pub complete: Option<bool>,
}

/// Merge 01 01 (supported, ready) and 01 41 (this drive). A monitor the car
/// reports as unsupported in 01 01 is left out; before 01 01 has been read,
/// the monitors enabled in 01 41 are shown.
pub fn monitor_views(m0101: Option<&[u8]>, m0141: Option<&[u8]>) -> Vec<MonitorView> {
    let since_clear = m0101.map(monitor_bits).unwrap_or_default();
    let this_drive = m0141.map(monitor_bits).unwrap_or_default();
    let names: Vec<&'static str> = if since_clear.is_empty() {
        this_drive.iter().filter(|m| m.available).map(|m| m.name).collect()
    } else {
        since_clear.iter().filter(|m| m.available).map(|m| m.name).collect()
    };
    names
        .into_iter()
        .map(|name| {
            let sc = since_clear.iter().find(|m| m.name == name);
            let td = this_drive.iter().find(|m| m.name == name);
            MonitorView {
                name,
                ready: sc.map(|m| m.complete),
                enabled: td.map(|m| m.available),
                complete: td.map(|m| m.complete),
            }
        })
        .collect()
}

// --- the health snapshot --------------------------------------------------------

/// Everything the OBD page's Health section shows, apart from plain values
/// (fuel system, boost, tyre pressures are mode 01 / extra values).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    pub monitors: Vec<MonitorView>,
    /// Misfire counts per cylinder from the last mode 06 read.
    pub misfire: Vec<CylinderMisfire>,
    /// Local time ("HH:MM") of the last complete mode 06 read.
    pub mode06_at: Option<String>,
    /// Number of mode 06 test results of that read.
    pub mode06_count: usize,
    /// Tests outside their min/max limits.
    pub out_of_limits: Vec<TestView>,
    /// Every test of the last read (served by `/api/obd`, not broadcast).
    pub tests: Vec<TestView>,
    pub ipt: Option<Ipt>,
    pub ipt_at: Option<String>,
    #[serde(skip)]
    m0101: Option<Vec<u8>>,
    #[serde(skip)]
    m0141: Option<Vec<u8>>,
}

impl Health {
    /// A new 01 01 (`pid` 0x01) or 01 41 reply's data bytes.
    pub fn set_monitor_bytes(&mut self, pid: u8, data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        match pid {
            0x01 => self.m0101 = Some(data.to_vec()),
            0x41 => self.m0141 = Some(data.to_vec()),
            _ => return,
        }
        self.monitors = monitor_views(self.m0101.as_deref(), self.m0141.as_deref());
    }

    pub fn set_mode06(&mut self, tests: &[MonitorTest], at: String) {
        self.misfire = misfires(tests);
        self.tests = tests.iter().map(MonitorTest::view).collect();
        self.out_of_limits = self.tests.iter().filter(|t| !t.pass).cloned().collect();
        self.mode06_count = tests.len();
        self.mode06_at = Some(at);
    }

    /// The broadcast form: without the full test list.
    pub fn broadcast_view(&self) -> serde_json::Value {
        serde_json::json!({
            "monitors": self.monitors,
            "misfire": self.misfire,
            "mode06_at": self.mode06_at,
            "mode06_count": self.mode06_count,
            "out_of_limits": self.out_of_limits,
            "ipt": self.ipt,
            "ipt_at": self.ipt_at,
        })
    }
}

// --- manufacturer requests (mode 22) ------------------------------------------

/// The only service this dashboard sends to a manufacturer address:
/// ReadDataByIdentifier. Everything else on a UDS module can change state
/// (sessions 10, reset 11, clear 14, security 27, communication control 28,
/// writes 2E/2F, routines 31, downloads 34-37, DTC setting 85).
const MANUFACTURER_SERVICES: &[u8] = &[0x22];

/// Request bytes for a manufacturer read, refusing any service that is not a
/// read.
pub fn manufacturer_request(service: u8, did: u16) -> Result<String, String> {
    if !MANUFACTURER_SERVICES.contains(&service) {
        return Err(format!("service {service:02X} refused: only ReadDataByIdentifier (22) is allowed"));
    }
    Ok(format!("{service:02X}{did:04X}"))
}

/// `ATSH` and `ATCRA` for a physical 11-bit diagnostic request id (reply id
/// = request + 8). Only 0x700-0x7F7 qualify; the functional id 7DF is
/// excluded (a 22 read would reach every module).
pub fn manufacturer_addressing(request_id: u16) -> Result<(String, String), String> {
    if !(0x700..=0x7F7).contains(&request_id) || request_id == 0x7DF {
        return Err(format!("request id {request_id:03X} refused: only physical diagnostic ids 700-7F7"));
    }
    Ok((format!("ATSH{request_id:03X}"), format!("ATCRA{:03X}", request_id + 8)))
}

/// Outcome of one `22 <did>` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DidReply {
    /// Data bytes after `62 <did>`.
    Data(Vec<u8>),
    /// `7F 22 <nrc>`.
    Negative(u8),
    /// NO DATA, a timeout or something else.
    None,
}

pub fn parse_did_reply(resp: &str, did: u16) -> DidReply {
    let [hi, lo] = did.to_be_bytes();
    for m in elm::messages(resp) {
        if m.len() >= 3 && m[0] == 0x62 && m[1] == hi && m[2] == lo {
            return DidReply::Data(m[3..].to_vec());
        }
        if m.len() >= 3 && m[0] == 0x7F && m[1] == 0x22 {
            return DidReply::Negative(m[2]);
        }
    }
    DidReply::None
}

/// Negative response codes that mean "ask again later", not "unsupported":
/// busyRepeatRequest (21) and responsePending (78).
pub fn nrc_is_transient(nrc: u8) -> bool {
    matches!(nrc, 0x21 | 0x78)
}

/// TPMS module request id (OBDb F-PACE `hdr`).
pub const TPMS_REQUEST_ID: u16 = 0x751;
/// Tyre pressure DIDs and their value keys (OBDb F-PACE: 2076 FL, 2077 FR,
/// 2078 RR, 2079 RL).
pub const TPMS_DIDS: &[(u16, &str)] = &[(0x2076, "tyre_fl"), (0x2077, "tyre_fr"), (0x2078, "tyre_rr"), (0x2079, "tyre_rl")];
/// OBDb F-PACE `mul` for the tyre pressure byte.
const TPMS_BAR_PER_BIT: f64 = 0.01373;

/// Tyre pressure in bar: data byte 1 after the DID (OBDb `bix 8, len 8`).
pub fn tyre_bar(data: &[u8]) -> Option<f64> {
    data.get(1).map(|&b| b as f64 * TPMS_BAR_PER_BIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::elm::iso_tp;

    #[test]
    fn iso_tp_helper_matches_the_vin_layout() {
        // Same shape as the real 0902 reply the existing tests use.
        let vin = b"\x49\x02\x01SAJAA2AV3HA123456";
        let s = iso_tp(vin);
        assert!(s.starts_with("014\r0: 49 02 01 53 41 4A \r1: 41 41 32 41 56 33 48 \r"), "{s}");
        assert_eq!(elm::messages(&s), vec![vin.to_vec()]);
    }

    #[test]
    fn uas_scaling_pinned_to_python_obd() {
        // python-OBD UAS_IDS: 0x24 counts ×1, 0x0A 0.122 mV, 0x16 0.1 °C −40,
        // 0x84 signed ×0.001, 0x20 ratio /256.
        let t = |uas, v| MonitorTest { mid: 0x21, tid: 0x80, uas, value: v, min: 0, max: 0xFFFF };
        assert_eq!(t(0x24, 12).view().value, Some(12.0));
        assert!((t(0x0A, 1000).view().value.unwrap() - 122.0).abs() < 1e-9);
        assert!((t(0x16, 1400).view().value.unwrap() - 100.0).abs() < 1e-9);
        assert!((t(0x84, 0xFFFE).view().value.unwrap() + 0.002).abs() < 1e-12);
        assert!((t(0x20, 128).view().value.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(t(0x7E, 5).view().value, None, "undefined id stays raw");
        assert_eq!(t(0x7E, 5).view().raw, "UAS 7E: 0005 [0000..FFFF]");
    }

    #[test]
    fn signed_limits_compare_signed() {
        // UAS 84 (signed): value −2, limits −10..+10 passes; unsigned reading
        // of 0xFFFE would have failed it.
        let t = MonitorTest { mid: 0x21, tid: 0x81, uas: 0x84, value: 0xFFFE, min: 0xFFF6, max: 0x000A };
        assert!(t.passed());
        let t = MonitorTest { uas: 0x01, ..t };
        assert!(!t.passed());
    }

    #[test]
    fn mode06_misfire_multi_frame() {
        // 06 A2 on a 4-cylinder: cylinder 1, TID 0B (EWMA) = 3, TID 0C (this
        // drive) = 12, both counts (UAS 24) with limits 0..0xFFFF.
        // 1 + 2 × 9 = 19 bytes: a multi-frame reply.
        let reply = [
            0x46, 0xA2, 0x0B, 0x24, 0x00, 0x03, 0x00, 0x00, 0xFF, 0xFF, //
            0xA2, 0x0C, 0x24, 0x00, 0x0C, 0x00, 0x00, 0xFF, 0xFF,
        ];
        let resp = iso_tp(&reply);
        assert!(resp.starts_with("013\r0: 46 A2 0B 24 00 03 \r1: 00 00 FF FF A2 0C 24 \r2: 00 0C 00 00 FF FF AA \r"), "{resp}");
        let tests = parse_mode06(&resp);
        assert_eq!(tests.len(), 2);
        assert!(tests.iter().all(MonitorTest::passed));
        let m = misfires(&tests);
        assert_eq!(m, vec![CylinderMisfire { cyl: 1, this_drive: Some(12.0), avg10: Some(3.0) }]);
    }

    #[test]
    fn mode06_catalyst_out_of_limits() {
        // 06 21: catalyst B1, manufacturer TID 80, UAS 01, value 0x0123 above
        // its max 0x0100.
        let reply = [0x46, 0x21, 0x80, 0x01, 0x01, 0x23, 0x00, 0x00, 0x01, 0x00];
        let tests = parse_mode06(&iso_tp(&reply));
        assert_eq!(tests.len(), 1);
        let v = tests[0].view();
        assert!(!v.pass);
        assert_eq!(v.monitor, "Catalyst B1");
        assert_eq!(v.value, Some(291.0));
        let mut h = Health::default();
        h.set_mode06(&tests, "08:15".into());
        assert_eq!(h.out_of_limits.len(), 1);
        assert_eq!(h.mode06_count, 1);
    }

    #[test]
    fn mode06_single_frame_and_garbage() {
        // A one-record reply fits a single frame (10 bytes would not: 7 max),
        // so real replies are always multi-frame; a truncated record is dropped.
        assert!(parse_mode06("46 21 80 01 01").is_empty());
        assert!(parse_mode06("NO DATA").is_empty());
        assert!(parse_mode06("7F 06 12").is_empty());
    }

    #[test]
    fn mode06_support_bitmap() {
        // 0600 → MIDs 01 02 (O2 B1S1/S2) and 20 (next bitmap).
        assert_eq!(parse_bitmap("46 00 C0 00 00 01 \r\r", 0x46, 0x00), Some(vec![0x01, 0x02, 0x20]));
        // 06A0 → A1-A5 (misfire general + cylinders 1-4).
        assert_eq!(parse_bitmap("46 A0 F8 00 00 00", 0x46, 0xA0), Some(vec![0xA1, 0xA2, 0xA3, 0xA4, 0xA5]));
        assert_eq!(parse_bitmap("NO DATA", 0x46, 0x00), None);
    }

    #[test]
    fn mid_names() {
        assert_eq!(mid_name(0x01), "O2 sensor B1S1");
        assert_eq!(mid_name(0x02), "O2 sensor B1S2");
        assert_eq!(mid_name(0x05), "O2 sensor B2S1");
        assert_eq!(mid_name(0x21), "Catalyst B1");
        assert_eq!(mid_name(0xA5), "Misfire cylinder 4");
        assert_eq!(mid_name(0x85), "Boost pressure B1");
    }

    #[test]
    fn ipt_spark_20_counters() {
        // 49 08 14 + 20 counters: OBDCOND 0x0010, IGNCNTR 0x0123, CAT B1 8/16,
        // CAT B2 0/0, O2 B1 10/16, O2 B2 0/0, EGR/VVT 16/16, AIR 0/0, EVAP 1/16,
        // SO2 B1 10/16, SO2 B2 0/0. 43 bytes → 7 frames.
        let counters: [u16; 20] = [16, 0x123, 8, 16, 0, 0, 10, 16, 0, 0, 16, 16, 0, 0, 1, 16, 10, 16, 0, 0];
        let mut reply = vec![0x49, 0x08, 0x14];
        for c in counters {
            reply.extend_from_slice(&c.to_be_bytes());
        }
        let resp = iso_tp(&reply);
        assert!(resp.starts_with("02B\r0: 49 08 14 00 10 01 \r1: 23 00 08 00 10 00 00 \r"), "{resp}");
        let ipt = parse_ipt(&resp).unwrap();
        assert_eq!(ipt.obd_conditions, 16);
        assert_eq!(ipt.ignition_cycles, 0x123);
        assert_eq!(ipt.monitors.len(), 9);
        assert_eq!(ipt.monitors[0], IptMonitor { name: "Catalyst B1", comp: 8, cond: 16, ratio: Some(0.5) });
        assert_eq!(ipt.monitors[1].ratio, None);
        assert_eq!(ipt.monitors[6].name, "EVAP");
        assert!((ipt.monitors[6].ratio.unwrap() - 0.0625).abs() < 1e-12);
        assert_eq!(ipt.monitors[8].name, "Secondary O2 B2");
    }

    #[test]
    fn ipt_spark_16_counters_and_short() {
        let mut reply = vec![0x49, 0x08, 0x10];
        for c in 0..16u16 {
            reply.extend_from_slice(&c.to_be_bytes());
        }
        let ipt = parse_ipt(&iso_tp(&reply)).unwrap();
        assert_eq!(ipt.monitors.len(), 7, "no secondary O2 pairs in a 16-counter record");
        // Announces 16 counters, carries 2: rejected.
        assert_eq!(parse_ipt("49 08 10 00 01 00 02"), None);
        assert_eq!(parse_ipt("NO DATA"), None);
    }

    #[test]
    fn monitor_bits_0141_and_0101() {
        // 01 41 `00 07 65 21`: misfire/fuel/components enabled, all complete;
        // C 65 = catalyst, EVAP, O2 sensor, O2 heater enabled; D 21 =
        // catalyst and O2 sensor incomplete.
        let bits = monitor_bits(&[0x00, 0x07, 0x65, 0x21]);
        let get = |n: &str| *bits.iter().find(|m| m.name == n).unwrap();
        assert_eq!(get("Misfire"), MonitorBit { name: "Misfire", available: true, complete: true });
        assert_eq!(get("Catalyst"), MonitorBit { name: "Catalyst", available: true, complete: false });
        assert_eq!(get("EVAP"), MonitorBit { name: "EVAP", available: true, complete: true });
        assert!(!get("EGR / VVT").available);
        // 01 01 supports EGR/VVT too and everything is ready since the clear.
        let mut h = Health::default();
        h.set_monitor_bytes(0x01, &[0x00, 0x07, 0xE5, 0x00]);
        h.set_monitor_bytes(0x41, &[0x00, 0x07, 0x65, 0x21]);
        let names: Vec<&str> = h.monitors.iter().map(|m| m.name).collect();
        assert_eq!(names, ["Misfire", "Fuel system", "Components", "Catalyst", "EVAP", "O2 sensor", "O2 heater", "EGR / VVT"]);
        let cat = h.monitors.iter().find(|m| m.name == "Catalyst").unwrap();
        assert_eq!((cat.ready, cat.enabled, cat.complete), (Some(true), Some(true), Some(false)));
        let egr = h.monitors.iter().find(|m| m.name == "EGR / VVT").unwrap();
        assert_eq!(egr.enabled, Some(false));
    }

    #[test]
    fn service_guard_refuses_everything_but_reads() {
        assert_eq!(manufacturer_request(0x22, 0x2076).as_deref(), Ok("222076"));
        for svc in [0x10, 0x11, 0x14, 0x19, 0x27, 0x28, 0x2E, 0x2F, 0x31, 0x34, 0x35, 0x36, 0x37, 0x3E, 0x85, 0x01, 0x04] {
            assert!(manufacturer_request(svc, 0x2076).is_err(), "{svc:02X}");
        }
    }

    #[test]
    fn addressing_guard_physical_7xx_only() {
        assert_eq!(manufacturer_addressing(0x751), Ok(("ATSH751".into(), "ATCRA759".into())));
        for id in [0x7DF, 0x6FF, 0x7F8, 0x123, 0x0, 0x7FF] {
            assert!(manufacturer_addressing(id).is_err(), "{id:03X}");
        }
    }

    #[test]
    fn tyre_pressure_obdb_vectors() {
        // OBDb F-PACE test cases (MY2017): `759 05 62 20 76 00 92` = 2.00458
        // bar, `... 00 C0` = 2.63616; MY2020 2077 `00 98` = 2.08696.
        let cases = [("62 20 76 00 92", 0x2076, 2.00458), ("62 20 76 00 C0", 0x2076, 2.63616), ("62 20 77 00 98", 0x2077, 2.08696)];
        for (resp, did, want) in cases {
            let DidReply::Data(d) = parse_did_reply(resp, did) else { panic!("{resp}") };
            assert!((tyre_bar(&d).unwrap() - want).abs() < 1e-9, "{resp}");
        }
        assert_eq!(parse_did_reply("7F 22 31", 0x2076), DidReply::Negative(0x31));
        assert_eq!(parse_did_reply("NO DATA", 0x2076), DidReply::None);
        assert_eq!(parse_did_reply("62 20 77 00 98", 0x2076), DidReply::None, "another DID's answer");
        assert!(nrc_is_transient(0x78) && !nrc_is_transient(0x31));
    }
}
