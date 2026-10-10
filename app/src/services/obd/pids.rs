//! Mode 01 PID catalogue: SAE J1979 decoding formulas, polling tiers and the
//! support-bitmap parser.
//!
//! Every PID the car reports as supported AND that has an entry here is polled;
//! a supported PID without an entry is logged (and its raw replies are in the
//! raw log) so a decoder can be added later.

use super::elm;
use std::time::Duration;

/// How often a PID is worth asking for. An ELM327 over Bluetooth manages
/// roughly 10-20 requests per second, so only fast-moving values are polled on
/// every cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Every cycle (~0.5-1 s).
    Fast,
    /// Every ~2 s.
    Medium,
    /// Every ~15 s.
    Slow,
    /// Every ~60 s: counters and values that barely change during a drive.
    Rare,
    /// Once per connection: properties of the car that never change (fuel
    /// type, OBD standard, scaling maxima). Retried every `interval()` until
    /// a reply decodes, then never again on that link.
    Once,
}

impl Tier {
    pub fn interval(self) -> Duration {
        match self {
            Tier::Fast => Duration::ZERO,
            Tier::Medium => Duration::from_secs(2),
            Tier::Slow => Duration::from_secs(15),
            Tier::Rare => Duration::from_secs(60),
            Tier::Once => Duration::from_secs(60),
        }
    }
}

/// One displayed value decoded out of a PID reply.
pub struct Output {
    /// Stable key: WebSocket payload, history buffers, `/api/obd/history/{key}`.
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    /// Decimals shown in the UI.
    pub decimals: u8,
    /// Data bytes (after `41 <pid>`) → value; `None` when the bytes are too
    /// short or flag the value as "not available".
    pub decode: fn(&[u8]) -> Option<f64>,
    /// For enumerated values (status codes): the text the UI shows instead of
    /// the number; `None` for a code it does not know (the number is shown).
    pub text: Option<fn(f64) -> Option<&'static str>>,
}

pub struct PidDef {
    pub pid: u8,
    pub tier: Tier,
    pub outputs: &'static [Output],
}

// --- formula helpers (A, B, C, D = data bytes 0..3) ---
fn a(d: &[u8]) -> Option<f64> { d.first().map(|&x| x as f64) }
fn ab(d: &[u8]) -> Option<f64> { (d.len() >= 2).then(|| d[0] as f64 * 256.0 + d[1] as f64) }
fn cd(d: &[u8]) -> Option<f64> { (d.len() >= 4).then(|| d[2] as f64 * 256.0 + d[3] as f64) }
fn pct_a(d: &[u8]) -> Option<f64> { a(d).map(|v| v * 100.0 / 255.0) }
fn temp_a(d: &[u8]) -> Option<f64> { a(d).map(|v| v - 40.0) }
fn trim_a(d: &[u8]) -> Option<f64> { a(d).map(|v| (v - 128.0) * 100.0 / 128.0) }
/// Fuel trims (PIDs 06-09). 0xFF would be +99.2 %, but no ECU runs a trim
/// near its rail: they clamp around ±25-35 % and set a lean/rich DTC. The
/// Jaguar's LTFT is 0xFF in 12 % of replies, between 0.0 % and +0.8 %
/// readings in steady cruise (largest genuine LTFT in 3.5 days: +7.8 %), so
/// 0xFF is an "invalid" marker, not a trim.
fn fuel_trim(d: &[u8]) -> Option<f64> {
    match d.first() {
        Some(0xFF) | None => None,
        Some(&v) => Some((v as f64 - 128.0) * 100.0 / 128.0),
    }
}
/// Vehicle speed (PID 0D). 0xFF (255 km/h) is what the ECM sends at key-on,
/// engine off, before it has a wheel speed; no car this dashboard runs in
/// reaches 255 km/h, so it is treated as "not available".
fn speed(d: &[u8]) -> Option<f64> {
    match d.first() {
        Some(0xFF) | None => None,
        Some(&v) => Some(v as f64),
    }
}
/// Physical ceiling for the air flow. The car is a 2.0 l turbo petrol
/// (~300 PS): at full power it needs ~200 g/s, and the largest genuine MAF
/// in the raw log is 189 g/s. The one reading above it, 333.33 g/s (0x8235)
/// at a refuel stop, is a sentinel. 300 g/s leaves margin; an engine above
/// ~450 PS would need this raised.
const MAF_MAX_GS: f64 = 300.0;
fn maf(d: &[u8]) -> Option<f64> { ab(d).map(|v| v / 100.0).filter(|v| *v <= MAF_MAX_GS) }
/// Commanded equivalence ratio (PID 44). 0xFFFF (λ 2.0) is the ECU saying
/// "no fuel commanded": deceleration fuel cut (MAF ~0.2 g/s, pedal released,
/// 12.8 % of running time on this car). There is no λ then, so it is missing
/// and `fuel_cut` carries the state instead.
fn lambda_cmd(d: &[u8]) -> Option<f64> {
    match ab(d) {
        Some(v) if v >= 65535.0 => None,
        v => v.map(|v| v * 2.0 / 65536.0),
    }
}
fn fuel_cut(d: &[u8]) -> Option<f64> { ab(d).map(|v| if v >= 65535.0 { 1.0 } else { 0.0 }) }
fn trim_b(d: &[u8]) -> Option<f64> { d.get(1).map(|&v| (v as f64 - 128.0) * 100.0 / 128.0) }
/// Narrow-band O2 sensor voltage (PIDs 14-1B), byte A / 200.
fn o2_volt(d: &[u8]) -> Option<f64> { a(d).map(|v| v / 200.0) }
/// Narrow-band O2 sensor short-term trim, byte B; 0xFF = sensor not used for trim.
fn o2_trim(d: &[u8]) -> Option<f64> {
    match d.get(1) {
        Some(0xFF) | None => None,
        Some(&b) => Some((b as f64 - 128.0) * 100.0 / 128.0),
    }
}
/// Wide-band O2 (PIDs 24-2B, 34-3B): equivalence ratio λ = 2/65536 · (256A+B).
fn lambda(d: &[u8]) -> Option<f64> { ab(d).map(|v| v * 2.0 / 65536.0) }
/// Wide-band O2 voltage (PIDs 24-2B): 8/65536 · (256C+D).
fn wb_volt(d: &[u8]) -> Option<f64> { cd(d).map(|v| v * 8.0 / 65536.0) }
/// Wide-band O2 current (PIDs 34-3B): (256C+D)/256 − 128 mA.
fn wb_ma(d: &[u8]) -> Option<f64> { cd(d).map(|v| v / 256.0 - 128.0) }
/// Catalyst temperature (PIDs 3C-3F): (256A+B)/10 − 40 °C.
fn cat_temp(d: &[u8]) -> Option<f64> { ab(d).map(|v| v / 10.0 - 40.0) }
fn torque_pct(d: &[u8]) -> Option<f64> { a(d).map(|v| v - 125.0) }
/// Multi-sensor temperature PIDs (67, 68): byte A is the support mask, bit
/// `n` set = sensor `n` present, its value in data byte `n + 1` (−40 °C).
/// The byte positions are fixed: absent sensors still occupy their byte
/// (the Jaguar's 0168 is `07 s1 s2 s3 FF FF FF`).
fn sensor_temp(d: &[u8], n: usize) -> Option<f64> {
    let present = d.first().is_some_and(|mask| mask & (1 << n) != 0);
    if present { d.get(n + 1).map(|&v| v as f64 - 40.0) } else { None }
}

/// Fuel system status (PID 03), one byte per system (SAE J1979 values, OBDb
/// SAEJ1979 FUELSYS1/2). Byte B is 0 when there is no second fuel system:
/// only system 1 is "off" at 0.
fn fuel_sys_a(d: &[u8]) -> Option<f64> { a(d) }
fn fuel_sys_b(d: &[u8]) -> Option<f64> { d.get(1).filter(|&&b| b != 0).map(|&b| b as f64) }
fn fuel_sys_text(v: f64) -> Option<&'static str> {
    Some(match v as u32 {
        0 => "Off",
        1 => "Open loop (warming up)",
        2 => "Closed loop",
        4 => "Open loop (load / decel)",
        8 => "Open loop (fault)",
        16 => "Closed loop (O2 fault)",
        32 => "Open loop (warming up, B2)",
        64 => "Open loop (load / decel, B2)",
        128 => "Open loop (fault, B2)",
        _ => return None,
    })
}
/// Monitor status this drive cycle (PID 41): monitors enabled, and how many
/// of them completed. Same bit layout as PID 01 bytes B-D; the per-monitor
/// detail is in `health::monitor_bits`.
fn monitors_enabled(d: &[u8]) -> Option<f64> {
    (d.len() >= 4).then(|| super::health::monitor_bits(d).iter().filter(|m| m.available).count() as f64)
}
fn monitors_complete(d: &[u8]) -> Option<f64> {
    (d.len() >= 4).then(|| super::health::monitor_bits(d).iter().filter(|m| m.available && m.complete).count() as f64)
}
/// Boost pressure control (PID 70, 10 bytes), turbo A (OBDb SAEJ1979 01 70):
/// byte A bit 0 = commanded A supported, bit 1 = actual A, bit 2 = control
/// status A; B-C commanded, D-E actual, kPa absolute at 1/32 kPa per bit;
/// byte J bits 0-1 = status A (1 open loop, 2 closed loop, 3 fault).
fn boost_cmd(d: &[u8]) -> Option<f64> {
    (d.len() >= 3 && d[0] & 0x01 != 0).then(|| u16::from_be_bytes([d[1], d[2]]) as f64 / 32.0)
}
fn boost_act(d: &[u8]) -> Option<f64> {
    (d.len() >= 5 && d[0] & 0x02 != 0).then(|| u16::from_be_bytes([d[3], d[4]]) as f64 / 32.0)
}
fn boost_ctl(d: &[u8]) -> Option<f64> {
    (d.len() >= 10 && d[0] & 0x04 != 0).then(|| (d[9] & 0x03) as f64).filter(|v| *v != 0.0)
}
fn boost_ctl_text(v: f64) -> Option<&'static str> {
    match v as u32 {
        1 => Some("Open loop"),
        2 => Some("Closed loop"),
        3 => Some("Fault"),
        _ => None,
    }
}

macro_rules! out {
    ($key:literal, $label:literal, $unit:literal, $dp:literal, $f:expr) => {
        Output { key: $key, label: $label, unit: $unit, decimals: $dp, decode: $f, text: None }
    };
    ($key:literal, $label:literal, $unit:literal, $dp:literal, $f:expr, text: $t:expr) => {
        Output { key: $key, label: $label, unit: $unit, decimals: $dp, decode: $f, text: Some($t) }
    };
}

macro_rules! pid {
    ($pid:literal, $tier:ident, [$($o:expr),+ $(,)?]) => {
        PidDef { pid: $pid, tier: Tier::$tier, outputs: &[$($o),+] }
    };
}

/// Mode 01 PIDs with a known decoding, in display order.
///
/// Keys `rpm speed coolant load throttle fuel intake boost oil` are the ones
/// the dashboard has always used (history URLs, trip recorder); keep them.
pub static PIDS: &[PidDef] = &[
    pid!(0x0C, Fast, [out!("rpm", "RPM", "rpm", 0, |d| ab(d).map(|v| v / 4.0))]),
    pid!(0x0D, Fast, [out!("speed", "Speed", "km/h", 0, speed)]),
    pid!(0x04, Fast, [out!("load", "Load", "%", 0, pct_a)]),
    pid!(0x11, Fast, [out!("throttle", "Throttle", "%", 0, pct_a)]),
    pid!(0x0B, Fast, [out!("boost", "Manifold pressure", "kPa", 0, a)]),
    pid!(0x10, Fast, [out!("maf", "Air flow (MAF)", "g/s", 1, maf)]),
    pid!(0x5E, Fast, [out!("fuel_rate", "Fuel rate", "L/h", 1, |d| ab(d).map(|v| v / 20.0))]),
    pid!(0x0E, Medium, [out!("timing", "Timing advance", "°", 1, |d| a(d).map(|v| v / 2.0 - 64.0))]),
    // Moved from Medium to Slow (2026-10-09 data analysis), each redundant
    // with a value still polled at least as often:
    // - 49 / 4A pedal D / E: two redundant pedal position sensors; 5A (relative
    //   pedal) stays in Medium.
    // - 47 throttle B: second track of the same throttle body as 11 (Fast).
    // - 45 relative throttle: 11 rescaled to its learnt closed position.
    // - 43 absolute load: the same air charge as 04 load (Fast) and 10 MAF.
    // - 42 ECU voltage: the supply rail, also measured every 2 s by ATRV.
    // Medium polled 18 PIDs at 4 slower requests per cycle, so each came round
    // every 8.3 s instead of 2 s.
    pid!(0x49, Slow, [out!("pedal_d", "Pedal D", "%", 0, pct_a)]),
    pid!(0x4A, Slow, [out!("pedal_e", "Pedal E", "%", 0, pct_a)]),
    pid!(0x5A, Medium, [out!("pedal_rel", "Pedal (relative)", "%", 0, pct_a)]),
    pid!(0x45, Slow, [out!("throttle_rel", "Throttle (relative)", "%", 0, pct_a)]),
    pid!(0x47, Slow, [out!("throttle_b", "Throttle B", "%", 0, pct_a)]),
    pid!(0x4C, Medium, [out!("throttle_cmd", "Throttle actuator", "%", 0, pct_a)]),
    pid!(0x43, Slow, [out!("load_abs", "Load (absolute)", "%", 0, |d| ab(d).map(|v| v * 100.0 / 255.0))]),
    pid!(0x44, Medium, [out!("lambda_cmd", "Commanded λ", "λ", 3, lambda_cmd),
                        out!("fuel_cut", "Fuel cut (overrun)", "on/off", 0, fuel_cut)]),
    pid!(0x61, Medium, [out!("torque_demand", "Demanded torque", "%", 0, torque_pct)]),
    pid!(0x62, Medium, [out!("torque_actual", "Actual torque", "%", 0, torque_pct)]),
    pid!(0x8E, Slow, [out!("torque_friction", "Friction torque", "%", 0, torque_pct)]),
    pid!(0x05, Medium, [out!("coolant", "Coolant", "°C", 0, temp_a)]),
    pid!(0x0F, Medium, [out!("intake", "Intake air", "°C", 0, temp_a)]),
    pid!(0x5C, Medium, [out!("oil", "Oil", "°C", 0, temp_a)]),
    // 0167 on the Jaguar is `01 B C`: the mask claims sensor 1 only, B is a
    // cold loop (median 39 °C with the engine warm, ~18 K above ambient; the
    // radiator outlet of the split cooling circuit, hypothesis) and C repeats
    // PID 05. So B is not the engine coolant: label it for what it reads and
    // leave the engine temperature to `coolant` (05).
    pid!(0x67, Slow, [out!("coolant_2", "Coolant sensor 1 (radiator)", "°C", 0, |d| sensor_temp(d, 0)),
                      out!("coolant_s2", "Coolant sensor 2", "°C", 0, |d| sensor_temp(d, 1))]),
    // 0168 on the Jaguar is `07 s1 s2 s3 FF FF FF`: bank 1 sensors 1-3 (s1
    // airbox ~24 °C, s2 ~30 °C, s3 charge air after the intercooler = PID 0F).
    // Bits 3-5 are bank 2.
    pid!(0x68, Slow, [out!("intake_2", "Intake air B1 sensor 1", "°C", 0, |d| sensor_temp(d, 0)),
                      out!("intake_b1s2", "Intake air B1 sensor 2", "°C", 0, |d| sensor_temp(d, 1)),
                      out!("intake_b1s3", "Intake air B1 sensor 3", "°C", 0, |d| sensor_temp(d, 2)),
                      out!("intake_b2s1", "Intake air B2 sensor 1", "°C", 0, |d| sensor_temp(d, 3)),
                      out!("intake_b2s2", "Intake air B2 sensor 2", "°C", 0, |d| sensor_temp(d, 4)),
                      out!("intake_b2s3", "Intake air B2 sensor 3", "°C", 0, |d| sensor_temp(d, 5))]),
    pid!(0x84, Slow, [out!("manifold_temp", "Manifold surface", "°C", 0, temp_a)]),
    pid!(0x46, Slow, [out!("ambient", "Ambient air", "°C", 0, temp_a)]),
    pid!(0x42, Slow, [out!("ecu_voltage", "ECU voltage", "V", 2, |d| ab(d).map(|v| v / 1000.0))]),
    pid!(0x06, Medium, [out!("stft1", "Short trim B1", "%", 1, fuel_trim)]),
    pid!(0x07, Medium, [out!("ltft1", "Long trim B1", "%", 1, fuel_trim)]),
    pid!(0x08, Medium, [out!("stft2", "Short trim B2", "%", 1, fuel_trim)]),
    pid!(0x09, Medium, [out!("ltft2", "Long trim B2", "%", 1, fuel_trim)]),
    pid!(0x55, Slow, [out!("o2_stft_b1", "O2 2nd STFT B1", "%", 1, trim_a),
                      out!("o2_stft_b3", "O2 2nd STFT B3", "%", 1, trim_b)]),
    pid!(0x56, Slow, [out!("o2_ltft_b1", "O2 2nd LTFT B1", "%", 1, trim_a),
                      out!("o2_ltft_b3", "O2 2nd LTFT B3", "%", 1, trim_b)]),
    pid!(0x57, Slow, [out!("o2_stft_b2", "O2 2nd STFT B2", "%", 1, trim_a),
                      out!("o2_stft_b4", "O2 2nd STFT B4", "%", 1, trim_b)]),
    pid!(0x58, Slow, [out!("o2_ltft_b2", "O2 2nd LTFT B2", "%", 1, trim_a),
                      out!("o2_ltft_b4", "O2 2nd LTFT B4", "%", 1, trim_b)]),
    pid!(0x14, Medium, [out!("o2_s1_v", "O2 sensor 1", "V", 2, o2_volt), out!("o2_s1_trim", "O2 sensor 1 trim", "%", 1, o2_trim)]),
    pid!(0x15, Medium, [out!("o2_s2_v", "O2 sensor 2", "V", 2, o2_volt), out!("o2_s2_trim", "O2 sensor 2 trim", "%", 1, o2_trim)]),
    pid!(0x16, Medium, [out!("o2_s3_v", "O2 sensor 3", "V", 2, o2_volt), out!("o2_s3_trim", "O2 sensor 3 trim", "%", 1, o2_trim)]),
    pid!(0x17, Medium, [out!("o2_s4_v", "O2 sensor 4", "V", 2, o2_volt), out!("o2_s4_trim", "O2 sensor 4 trim", "%", 1, o2_trim)]),
    pid!(0x18, Medium, [out!("o2_s5_v", "O2 sensor 5", "V", 2, o2_volt), out!("o2_s5_trim", "O2 sensor 5 trim", "%", 1, o2_trim)]),
    pid!(0x19, Medium, [out!("o2_s6_v", "O2 sensor 6", "V", 2, o2_volt), out!("o2_s6_trim", "O2 sensor 6 trim", "%", 1, o2_trim)]),
    pid!(0x1A, Medium, [out!("o2_s7_v", "O2 sensor 7", "V", 2, o2_volt), out!("o2_s7_trim", "O2 sensor 7 trim", "%", 1, o2_trim)]),
    pid!(0x1B, Medium, [out!("o2_s8_v", "O2 sensor 8", "V", 2, o2_volt), out!("o2_s8_trim", "O2 sensor 8 trim", "%", 1, o2_trim)]),
    pid!(0x24, Medium, [out!("wb1_lambda", "Wide-band O2 1 λ", "λ", 3, lambda), out!("wb1_v", "Wide-band O2 1", "V", 2, wb_volt)]),
    pid!(0x25, Medium, [out!("wb2_lambda", "Wide-band O2 2 λ", "λ", 3, lambda), out!("wb2_v", "Wide-band O2 2", "V", 2, wb_volt)]),
    pid!(0x26, Medium, [out!("wb3_lambda", "Wide-band O2 3 λ", "λ", 3, lambda), out!("wb3_v", "Wide-band O2 3", "V", 2, wb_volt)]),
    pid!(0x27, Medium, [out!("wb4_lambda", "Wide-band O2 4 λ", "λ", 3, lambda), out!("wb4_v", "Wide-band O2 4", "V", 2, wb_volt)]),
    pid!(0x28, Medium, [out!("wb5_lambda", "Wide-band O2 5 λ", "λ", 3, lambda), out!("wb5_v", "Wide-band O2 5", "V", 2, wb_volt)]),
    pid!(0x29, Medium, [out!("wb6_lambda", "Wide-band O2 6 λ", "λ", 3, lambda), out!("wb6_v", "Wide-band O2 6", "V", 2, wb_volt)]),
    pid!(0x2A, Medium, [out!("wb7_lambda", "Wide-band O2 7 λ", "λ", 3, lambda), out!("wb7_v", "Wide-band O2 7", "V", 2, wb_volt)]),
    pid!(0x2B, Medium, [out!("wb8_lambda", "Wide-band O2 8 λ", "λ", 3, lambda), out!("wb8_v", "Wide-band O2 8", "V", 2, wb_volt)]),
    pid!(0x34, Medium, [out!("wbc1_lambda", "Wide-band O2 1 λ", "λ", 3, lambda), out!("wbc1_ma", "Wide-band O2 1 mA", "mA", 2, wb_ma)]),
    pid!(0x35, Medium, [out!("wbc2_lambda", "Wide-band O2 2 λ", "λ", 3, lambda), out!("wbc2_ma", "Wide-band O2 2 mA", "mA", 2, wb_ma)]),
    pid!(0x36, Medium, [out!("wbc3_lambda", "Wide-band O2 3 λ", "λ", 3, lambda), out!("wbc3_ma", "Wide-band O2 3 mA", "mA", 2, wb_ma)]),
    pid!(0x37, Medium, [out!("wbc4_lambda", "Wide-band O2 4 λ", "λ", 3, lambda), out!("wbc4_ma", "Wide-band O2 4 mA", "mA", 2, wb_ma)]),
    pid!(0x38, Medium, [out!("wbc5_lambda", "Wide-band O2 5 λ", "λ", 3, lambda), out!("wbc5_ma", "Wide-band O2 5 mA", "mA", 2, wb_ma)]),
    pid!(0x39, Medium, [out!("wbc6_lambda", "Wide-band O2 6 λ", "λ", 3, lambda), out!("wbc6_ma", "Wide-band O2 6 mA", "mA", 2, wb_ma)]),
    pid!(0x3A, Medium, [out!("wbc7_lambda", "Wide-band O2 7 λ", "λ", 3, lambda), out!("wbc7_ma", "Wide-band O2 7 mA", "mA", 2, wb_ma)]),
    pid!(0x3B, Medium, [out!("wbc8_lambda", "Wide-band O2 8 λ", "λ", 3, lambda), out!("wbc8_ma", "Wide-band O2 8 mA", "mA", 2, wb_ma)]),
    pid!(0x3C, Slow, [out!("cat_b1s1", "Catalyst B1 S1", "°C", 0, cat_temp)]),
    pid!(0x3D, Slow, [out!("cat_b2s1", "Catalyst B2 S1", "°C", 0, cat_temp)]),
    pid!(0x3E, Slow, [out!("cat_b1s2", "Catalyst B1 S2", "°C", 0, cat_temp)]),
    pid!(0x3F, Slow, [out!("cat_b2s2", "Catalyst B2 S2", "°C", 0, cat_temp)]),
    pid!(0x0A, Medium, [out!("fuel_pressure", "Fuel pressure", "kPa", 0, |d| a(d).map(|v| v * 3.0))]),
    pid!(0x22, Medium, [out!("rail_rel", "Fuel rail rel.", "kPa", 0, |d| ab(d).map(|v| v * 0.079))]),
    pid!(0x23, Medium, [out!("rail_gauge", "Fuel rail pressure", "kPa", 0, |d| ab(d).map(|v| v * 10.0))]),
    pid!(0x59, Medium, [out!("rail_abs", "Fuel rail abs", "kPa", 0, |d| ab(d).map(|v| v * 10.0))]),
    pid!(0x5D, Medium, [out!("injection_timing", "Injection timing", "°", 1, |d| ab(d).map(|v| v / 128.0 - 210.0))]),
    pid!(0x2C, Medium, [out!("egr_cmd", "EGR (commanded)", "%", 0, pct_a)]),
    pid!(0x2D, Medium, [out!("egr_error", "EGR error", "%", 1, trim_a)]),
    pid!(0x2E, Slow, [out!("evap_purge", "Evap purge", "%", 0, pct_a)]),
    pid!(0x32, Slow, [out!("evap_vp", "Evap vapour", "Pa", 0,
        |d| (d.len() >= 2).then(|| i16::from_be_bytes([d[0], d[1]]) as f64 / 4.0))]),
    pid!(0x53, Slow, [out!("evap_abs", "Evap pressure abs", "kPa", 2, |d| ab(d).map(|v| v / 200.0))]),
    pid!(0x54, Slow, [out!("evap_p", "Evap pressure", "Pa", 0, |d| ab(d).map(|v| v - 32767.0))]),
    pid!(0x2F, Medium, [out!("fuel", "Fuel level", "%", 0, pct_a)]),
    pid!(0x52, Rare, [out!("ethanol", "Ethanol", "%", 0, pct_a)]),
    pid!(0x5B, Slow, [out!("hybrid_soc", "Hybrid battery", "%", 0, pct_a)]),
    pid!(0x33, Slow, [out!("baro", "Barometric pressure", "kPa", 0, a)]),
    // Boost control health: the ECU's commanded boost against what the
    // sensor reads (a wastegate or leak shows as actual lagging commanded).
    pid!(0x70, Medium, [out!("boost_cmd", "Boost commanded", "kPa", 0, boost_cmd),
                        out!("boost_act", "Boost actual", "kPa", 0, boost_act),
                        out!("boost_ctl", "Boost control", "", 0, boost_ctl, text: boost_ctl_text)]),
    pid!(0x03, Slow, [out!("fuel_sys1", "Fuel system", "", 0, fuel_sys_a, text: fuel_sys_text),
                      out!("fuel_sys2", "Fuel system 2", "", 0, fuel_sys_b, text: fuel_sys_text)]),
    pid!(0x41, Slow, [out!("mon_drive_enabled", "Monitors enabled (this drive)", "", 0, monitors_enabled),
                      out!("mon_drive_complete", "Monitors complete (this drive)", "", 0, monitors_complete)]),
    pid!(0x01, Slow, [out!("mil", "Check engine light", "on/off", 0, |d| a(d).map(|v| if v as u8 & 0x80 != 0 { 1.0 } else { 0.0 })),
                      out!("ecu_dtc_count", "ECU fault count", "", 0, |d| a(d).map(|v| (v as u8 & 0x7F) as f64))]),
    pid!(0x1F, Slow, [out!("runtime", "Engine run time", "s", 0, ab)]),
    pid!(0x21, Rare, [out!("dist_mil", "Km with MIL on", "km", 0, ab)]),
    pid!(0x4D, Rare, [out!("time_mil", "Time with MIL on", "min", 0, ab)]),
    pid!(0x30, Rare, [out!("warmups", "Warm-ups since clear", "", 0, a)]),
    pid!(0x31, Rare, [out!("dist_clear", "Km since clear", "km", 0, ab)]),
    pid!(0x4E, Rare, [out!("time_clear", "Time since clear", "min", 0, ab)]),
    pid!(0x63, Once, [out!("torque_ref", "Reference torque", "Nm", 0, ab)]),
    // Static properties, read once per connection. The codes are the SAE
    // J1979 enumerations (51: 1 = petrol, 4 = diesel, 8 = electric...; 1C:
    // 6 = EOBD, 9 = EOBD + OBD-II...).
    pid!(0x51, Once, [out!("fuel_type", "Fuel type (J1979 code)", "", 0, a)]),
    pid!(0x1C, Once, [out!("obd_standard", "OBD standard (J1979 code)", "", 0, a)]),
    // Maximum values the ECU scales its replies to. A byte of 0 means "use the
    // J1979 default scaling", which is what the decoders above assume.
    pid!(0x4F, Once, [out!("max_lambda", "Max λ (scaling)", "λ", 0, a),
                      out!("max_o2_v", "Max O2 voltage (scaling)", "V", 0, |d| d.get(1).map(|&v| v as f64)),
                      out!("max_o2_ma", "Max O2 current (scaling)", "mA", 0, |d| d.get(2).map(|&v| v as f64)),
                      out!("max_map", "Max manifold pressure (scaling)", "kPa", 0, |d| d.get(3).map(|&v| v as f64 * 10.0))]),
    pid!(0x50, Once, [out!("max_maf", "Max air flow (scaling)", "g/s", 0, |d| a(d).map(|v| v * 10.0))]),
    pid!(0xA6, Rare, [out!("odometer", "Odometer", "km", 0,
        |d| (d.len() >= 4).then(|| u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as f64 / 10.0))]),
];

/// Values that do not come from a mode 01 PID.
pub static EXTRA_OUTPUTS: &[Output] = &[
    // `ATRV`: voltage measured by the adapter on OBD pin 16.
    out!("battery", "Battery", "V", 1, |_| None),
    // MAP − barometric pressure, when the car reports both (PIDs 0B and 33).
    out!("boost_gauge", "Boost", "kPa", 0, |_| None),
    // Tyre pressures, manufacturer reads from the TPMS module (751, mode 22)
    // when RASCAR_OBD_TPMS is set; see `health::TPMS_DIDS`.
    out!("tyre_fl", "Tyre front left", "bar", 2, |_| None),
    out!("tyre_fr", "Tyre front right", "bar", 2, |_| None),
    out!("tyre_rl", "Tyre rear left", "bar", 2, |_| None),
    out!("tyre_rr", "Tyre rear right", "bar", 2, |_| None),
];

pub fn find(pid: u8) -> Option<&'static PidDef> {
    PIDS.iter().find(|p| p.pid == pid)
}

/// Metadata for any key (PID output or extra), for labels and history.
pub fn output(key: &str) -> Option<&'static Output> {
    PIDS.iter()
        .flat_map(|p| p.outputs.iter())
        .chain(EXTRA_OUTPUTS.iter())
        .find(|o| o.key == key)
}

/// All keys in display order.
pub fn all_outputs() -> impl Iterator<Item = &'static Output> {
    PIDS.iter().flat_map(|p| p.outputs.iter()).chain(EXTRA_OUTPUTS.iter())
}

/// Data bytes of a mode 01 reply for `pid` (first answering ECU), or `None`
/// for "NO DATA", errors or a reply to another PID.
///
/// With headers off a reply does not say which ECU sent it, and when two
/// answer (functional 7DF request: the Jaguar's ECM and TCM) their order on
/// the line is random, so "first" is not deterministic. The poll loop avoids
/// that by addressing the ECM alone (`ATSH7E0`) on 11-bit CAN; on other
/// protocols, or a clone that rejects `ATSH`, the first message is still taken.
pub fn mode01_data(resp: &str, pid: u8) -> Option<Vec<u8>> {
    elm::messages(resp)
        .into_iter()
        .find(|m| m.len() >= 3 && m[0] == 0x41 && m[1] == pid)
        .map(|m| m[2..].to_vec())
}

/// Decode a support-bitmap reply (`01 00`, `01 20`, ...). Every answering ECU
/// contributes (bitwise OR). Returns the supported PIDs in `base+1 ..= base+32`
/// — the last one (base+32) means "the next bitmap is supported too".
pub fn parse_support_bitmap(resp: &str, base: u8) -> Option<Vec<u8>> {
    super::health::parse_bitmap(resp, 0x41, base)
}

/// VIN from a `09 02` reply: CAN gives one multi-frame message
/// (`49 02 01` + 17 characters), the K-line protocols five `49 02 <n> a b c d`
/// lines. The VIN is the last 17 printable characters.
pub fn parse_vin(resp: &str) -> Option<String> {
    let msgs = elm::messages(resp);
    let mut chars: Vec<u8> = Vec::new();
    for m in msgs.iter().filter(|m| m.len() >= 3 && m[0] == 0x49 && m[1] == 0x02) {
        chars.extend_from_slice(&m[3..]);
    }
    let vin: String = chars
        .into_iter()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(|b| b as char)
        .collect();
    (vin.len() >= 17).then(|| vin[vin.len() - 17..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(pid: u8, resp: &str) -> Vec<(&'static str, Option<f64>)> {
        let def = find(pid).expect("pid in catalogue");
        let data = mode01_data(resp, pid).expect("decodable reply");
        def.outputs.iter().map(|o| (o.key, (o.decode)(&data))).collect()
    }

    fn close(v: Option<f64>, want: f64) -> bool {
        v.is_some_and(|v| (v - want).abs() < 1e-6)
    }

    #[test]
    fn classic_formulas() {
        assert!(close(decode(0x0C, "41 0C 1A F8")[0].1, 1726.0)); // (0x1AF8)/4
        assert!(close(decode(0x0D, "41 0D 32")[0].1, 50.0));
        assert!(close(decode(0x05, "41 05 7B")[0].1, 83.0));
        assert!(close(decode(0x04, "41 04 FF")[0].1, 100.0));
        assert!(close(decode(0x0E, "41 0E 90")[0].1, 8.0)); // 144/2-64
        assert!(close(decode(0x10, "41 10 01 F4")[0].1, 5.0));
        assert!(close(decode(0x06, "41 06 80")[0].1, 0.0));
        assert!(close(decode(0x06, "41 06 00")[0].1, -100.0));
        assert!(close(decode(0x42, "41 42 36 B0")[0].1, 14.0));
        assert!(close(decode(0x3C, "41 3C 13 88")[0].1, 460.0)); // 5000/10-40
        assert!(close(decode(0x5E, "41 5E 00 64")[0].1, 5.0));
        assert!(close(decode(0x1F, "41 1F 01 00")[0].1, 256.0));
        assert!(close(decode(0x61, "41 61 7D")[0].1, 0.0));
        assert!(close(decode(0xA6, "41 A6 00 01 E2 40")[0].1, 12345.6));
        assert!(close(decode(0x32, "41 32 FF FC")[0].1, -1.0)); // signed /4
    }

    #[test]
    fn two_value_pids() {
        let o2 = decode(0x15, "41 15 5A FF");
        assert!(close(o2[0].1, 0.45));
        assert_eq!(o2[1].1, None, "0xFF = not used for trim");
        let wb = decode(0x24, "41 24 80 00 80 00");
        assert!(close(wb[0].1, 1.0));
        assert!(close(wb[1].1, 4.0));
        let wbc = decode(0x34, "41 34 80 00 80 00");
        assert!(close(wbc[0].1, 1.0));
        assert!(close(wbc[1].1, 0.0));
        let mil = decode(0x01, "41 01 83 07 E5 00");
        assert!(close(mil[0].1, 1.0));
        assert!(close(mil[1].1, 3.0));
    }

    #[test]
    fn short_or_foreign_replies_rejected() {
        assert_eq!(mode01_data("NO DATA", 0x0C), None);
        assert_eq!(mode01_data("41 0D 32", 0x0C), None);
        assert_eq!(mode01_data("SEARCHING...\r41 0C 1A F8", 0x0C), Some(vec![0x1A, 0xF8]));
        let def = find(0x0C).unwrap();
        assert_eq!((def.outputs[0].decode)(&[0x1A]), None, "one byte for a two-byte PID");
    }

    #[test]
    fn bitmap_bits_map_to_pids() {
        // BE 1F A8 13: the textbook example (01 03 04 05 06 07 0C 0D 0E 0F 10 11 13 15 1C 1F 20).
        let p = parse_support_bitmap("41 00 BE 1F A8 13", 0x00).unwrap();
        assert_eq!(p, vec![0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x13, 0x15, 0x1C, 0x1F, 0x20]);
        // Two ECUs are OR-ed; base 0x20 offsets the PIDs.
        let p = parse_support_bitmap("41 20 80 00 00 00\r41 20 00 00 00 01", 0x20).unwrap();
        assert_eq!(p, vec![0x21, 0x40]);
        assert_eq!(parse_support_bitmap("NO DATA", 0x00), None);
        assert_eq!(parse_support_bitmap("41 00 BE", 0x00), None);
    }

    #[test]
    fn vin_can_and_k_line() {
        let can = "014\r0: 49 02 01 53 41 4A\r1: 41 41 32 41 56 33 48\r2: 41 31 32 33 34 35 36";
        assert_eq!(parse_vin(can).as_deref(), Some("SAJAA2AV3HA123456"));
        let kline = "49 02 01 00 00 00 53\r49 02 02 41 4A 41 41\r49 02 03 32 41 56 33\r49 02 04 48 41 31 32\r49 02 05 33 34 35 36";
        assert_eq!(parse_vin(kline).as_deref(), Some("SAJAA2AV3HA123456"));
        assert_eq!(parse_vin("NO DATA"), None);
    }

    #[test]
    fn catalogue_keys_unique_and_tiers_sane() {
        let mut keys: Vec<&str> = all_outputs().map(|o| o.key).collect();
        let n = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate output key");
        let mut pids: Vec<u8> = PIDS.iter().map(|p| p.pid).collect();
        pids.sort();
        pids.dedup();
        assert_eq!(pids.len(), PIDS.len(), "duplicate PID");
        // The legacy keys the dashboard and trip recorder use must still exist.
        for k in ["rpm", "speed", "coolant", "load", "throttle", "fuel", "intake", "boost", "oil", "battery", "maf", "fuel_rate"] {
            assert!(output(k).is_some(), "{k}");
        }
        // Support-bitmap PIDs are not values.
        for b in [0x00, 0x20, 0x40, 0x60, 0x80, 0xA0, 0xC0] {
            assert!(find(b).is_none());
        }
    }
    /// All outputs of a PID for a raw reply as the poll loop sees it (prompt
    /// stripped), `None` where the decoder rejects the value.
    fn outputs(pid: u8, resp: &str) -> Vec<(&'static str, Option<f64>)> {
        decode(pid, resp.replace('>', "").trim())
    }

    #[test]
    fn sentinels_from_the_raw_log_are_missing() {
        // LTFT 0xFF: 12 % of the Jaguar's 0107 replies.
        assert_eq!(outputs(0x07, "41 07 FF \r\r>")[0].1, None);
        assert!(close(outputs(0x07, "41 07 81 \r\r>")[0].1, 0.78125));
        assert!(close(outputs(0x06, "41 06 7B \r\r>")[0].1, -3.90625));
        // Speed 0xFF at key-on, engine off (ECM alone).
        assert_eq!(outputs(0x0D, "41 0D FF \r\r>")[0].1, None);
        assert!(close(outputs(0x0D, "41 0D 52 \r\r>")[0].1, 82.0));
        // MAF 333.33 g/s at the refuel stop; 189.38 is the largest real value.
        assert_eq!(outputs(0x10, "41 10 82 35 \r\r>")[0].1, None);
        assert!(close(outputs(0x10, "41 10 49 FA \r\r>")[0].1, 189.38));
    }

    #[test]
    fn commanded_lambda_ffff_is_fuel_cut() {
        let cut = outputs(0x44, "41 44 FF FF \r\r>");
        assert_eq!(cut[0], ("lambda_cmd", None));
        assert_eq!(cut[1], ("fuel_cut", Some(1.0)));
        let run = outputs(0x44, "41 44 80 00 \r\r>");
        assert!(close(run[0].1, 1.0));
        assert_eq!(run[1], ("fuel_cut", Some(0.0)));
    }

    #[test]
    fn coolant_0167_follows_the_mask() {
        // Jaguar: mask 01, B = 0x37 (15 °C, radiator loop), C = 0x7A (= PID 05).
        let o = outputs(0x67, "41 67 01 37 7A \r\r>");
        assert_eq!(o[0].0, "coolant_2");
        assert!(close(o[0].1, 15.0));
        assert_eq!(o[1], ("coolant_s2", None), "bit 1 clear: byte C is not a second sensor");
        let both = outputs(0x67, "41 67 03 37 7A");
        assert!(close(both[1].1, 82.0));
    }

    #[test]
    fn intake_0168_decodes_every_sensor_in_the_mask() {
        // Real multi-frame reply: mask 07 = bank 1 sensors 1-3, bank 2 absent.
        let o = outputs(0x68, "009\n0: 41 68 07 47 4A 5B \r1: FF FF FF \r\r>");
        let v: Vec<Option<f64>> = o.iter().map(|(_, v)| *v).collect();
        assert_eq!(v, vec![Some(31.0), Some(34.0), Some(51.0), None, None, None]);
        assert_eq!(o[0].0, "intake_2", "sensor 1 keeps its historical key");
    }

    #[test]
    fn double_reply_takes_the_first_message() {
        // Functional request answered by ECM and TCM (0111, ~9 950 times in the
        // raw log). Without headers this is the documented limitation that
        // ECM-only addressing removes: the first message wins.
        assert_eq!(mode01_data("41 11 24 \r41 11 0D \r\r>", 0x11), Some(vec![0x24]));
        assert_eq!(mode01_data("41 11 0D \r41 11 24 \r\r>", 0x11), Some(vec![0x0D]));
        // Physical request to the ECM: one message.
        assert_eq!(mode01_data("41 11 24 \r\r>", 0x11), Some(vec![0x24]));
    }

    #[test]
    fn ecm_and_tcm_bitmaps() {
        // 0100 with both ECUs (functional) is the OR of the two; the ECM alone
        // (physical 7E0) gives its own list.
        let both = parse_support_bitmap("SEARCHING...\r41 00 BE 3F A8 13 \r41 00 98 18 80 13 \r\r>", 0x00).unwrap();
        let ecm = parse_support_bitmap("41 00 BE 3F A8 13 \r\r>", 0x00).unwrap();
        assert_eq!(both, ecm, "the TCM's 0100 PIDs are a subset of the ECM's");
        assert!(ecm.contains(&0x11) && ecm.contains(&0x0C) && ecm.contains(&0x20));
    }

    #[test]
    fn fuel_system_status_0103() {
        let o = outputs(0x03, "41 03 02 00 \r\r>");
        assert_eq!(o[0], ("fuel_sys1", Some(2.0)));
        assert_eq!(o[1], ("fuel_sys2", None), "no second fuel system");
        let text = find(0x03).unwrap().outputs[0].text.unwrap();
        assert_eq!(text(2.0), Some("Closed loop"));
        assert_eq!(text(1.0), Some("Open loop (warming up)"));
        assert_eq!(text(4.0), Some("Open loop (load / decel)"));
        assert_eq!(text(3.0), None, "two bits at once is not a J1979 value");
        assert_eq!(find(0x03).unwrap().tier, Tier::Slow);
    }

    #[test]
    fn monitor_status_this_drive_0141() {
        // Misfire/fuel/components enabled and complete; catalyst, EVAP, O2,
        // O2 heater enabled, catalyst and O2 still incomplete.
        let o = outputs(0x41, "41 41 00 07 65 21 \r\r>");
        assert_eq!(o[0], ("mon_drive_enabled", Some(7.0)));
        assert_eq!(o[1], ("mon_drive_complete", Some(5.0)));
        assert_eq!(find(0x41).unwrap().tier, Tier::Slow);
    }

    #[test]
    fn boost_control_0170_multi_frame() {
        // 12 bytes: commanded 0x0FA0/32 = 125 kPa, actual 0x0F80/32 = 124 kPa,
        // turbo B absent, status A = closed loop. The ELM splits it in two
        // frames (6 + 7, padded).
        let o = outputs(0x70, "00C\r0: 41 70 07 0F A0 0F \r1: 80 00 00 00 00 02 AA \r\r>");
        assert_eq!(o[0], ("boost_cmd", Some(125.0)));
        assert_eq!(o[1], ("boost_act", Some(124.0)));
        assert_eq!(o[2], ("boost_ctl", Some(2.0)));
        assert_eq!(find(0x70).unwrap().outputs[2].text.unwrap()(2.0), Some("Closed loop"));
        // Actual not supported (bit 1 clear): only commanded.
        let o = outputs(0x70, "00C\r0: 41 70 01 0F A0 0F \r1: 80 00 00 00 00 00 AA ");
        assert_eq!(o[1], ("boost_act", None));
        assert_eq!(o[2], ("boost_ctl", None));
        assert_eq!(find(0x70).unwrap().tier, Tier::Medium);
    }

    #[test]
    fn static_pids_read_once() {
        for pid in [0x51, 0x1C, 0x4F, 0x50, 0x63] {
            assert_eq!(find(pid).unwrap().tier, Tier::Once, "{pid:02X}");
        }
        assert!(close(outputs(0x51, "41 51 01")[0].1, 1.0));
        let scale = outputs(0x4F, "41 4F 00 00 00 00");
        assert!(scale.iter().all(|(_, v)| *v == Some(0.0)));
        assert!(close(outputs(0x50, "41 50 19 00 00 00")[0].1, 250.0));
        // Medium trimmed to values not redundant with another polled PID.
        for pid in [0x49, 0x4A, 0x47, 0x45, 0x43, 0x42] {
            assert_eq!(find(pid).unwrap().tier, Tier::Slow, "{pid:02X}");
        }
    }
}
