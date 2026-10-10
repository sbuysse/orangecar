//! OBD-II service — talks to an ELM327 over the Bluetooth RFCOMM serial port
//! (`/dev/rfcomm0`, bound by `rascar-obd-link`).
//!
//! At connect it reads the ECU's mode 01 support bitmaps (01 00 / 20 / 40 /
//! ...), the protocol (`ATDPN`) and the VIN (09 02), then polls every
//! supported PID that has a decoder in [`pids::PIDS`], fast-moving values on
//! every cycle and slow ones less often. Stored, pending and permanent DTCs
//! (modes 03 / 07 / 0A) are read every 30 s. Every exchange is written verbatim
//! to the durable raw log ([`rawlog`]).
//!
//! On 11-bit CAN, mode 01 requests go physically to the engine ECU
//! (`ATSH7E0`, reply from 7E8 only): a functional request (7DF) is also
//! answered by the gearbox ECU, and with headers off the two replies arrive in
//! random order, so rpm, throttle, load and speed used to flip between ECUs.
//! DTC reads and the clear stay functional so every ECU's codes are seen.
//!
//! Once per drive, after the engine has run for two minutes, the ECM's
//! on-board monitor results (mode 06: misfire counts per cylinder, catalyst,
//! O2 sensors, each against its limits) and in-use performance counters
//! (09 08) are read, one request per poll cycle. With `RASCAR_OBD_TPMS=1` the
//! tyre pressures are read from the TPMS module (751, mode 22) every minute
//! while the engine runs; see [`health`].

mod dtc;
mod dtc_table;
mod elm;
pub mod health;
pub mod pids;
mod rawlog;

use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

pub use dtc::DtcKind;
use rawlog::RawLog;

/// Samples kept per value for the history chart.
const HISTORY_LEN: usize = 120;
/// Hard cap on a single ELM327 response so a chatty/garbage port can't grow
/// the buffer without bound (4 KiB is far larger than any real reply).
const MAX_RESPONSE_BYTES: usize = 4096;
/// Overall deadline for a single command/response exchange.
const CMD_DEADLINE: Duration = Duration::from_secs(3);
/// After this long with no successfully decoded PID the displayed values are
/// marked stale. The adapter can answer happily ("NO DATA") while the bus is
/// dead — ignition off, ECU asleep — and every field would otherwise stay
/// frozen at its last value behind a green "Connected" badge.
const STALE_AFTER: Duration = Duration::from_secs(10);
/// After this long with no decode the link is reported as disconnected: it is
/// electrically up but carrying no usable OBD data.
const DISCONNECT_AFTER: Duration = Duration::from_secs(30);
/// How long `clear_dtcs` waits for the serial loop to transmit mode 04.
const CLEAR_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// Stored / pending / permanent DTCs are re-read this often.
const DTC_EVERY: Duration = Duration::from_secs(30);
/// Retry the support-bitmap discovery this often while it has not succeeded
/// (ignition off at connect).
const DISCOVERY_RETRY: Duration = Duration::from_secs(30);
/// Non-fast PIDs polled per cycle, most overdue first. Bounds a cycle to
/// roughly (fast PIDs + 4) requests.
const MAX_SLOWER_PER_CYCLE: usize = 4;
/// Same undecoded reply to the same command is reported to the journal at
/// most this often (the raw log has every one).
const UNDECODED_REPORT_EVERY: Duration = Duration::from_secs(60);
/// Consecutive failed exchanges before the link is declared lost.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
/// Wait before reconnecting after a failed attempt. The first attempts are
/// quick: an in-drive Bluetooth drop is usually followed by one failed
/// connect (the RFCOMM page times out after 5.12 s with an empty reply), and
/// the very next one succeeds, so a fixed 5 s back-off only added dead time
/// (~11.5 s between attempts in the raw log). After `QUICK_RETRIES` failures
/// in a row (car off, adapter unpowered) it settles at the slow rate.
const QUICK_RETRY: Duration = Duration::from_secs(1);
const SLOW_RETRY: Duration = Duration::from_secs(5);
const QUICK_RETRIES: u32 = 10;
/// Pause after a cycle in which nothing decoded (bus asleep, ignition off):
/// keeps the adapter from being hammered with requests that all answer
/// "NO DATA". A live cycle goes straight into the next one.
const IDLE_CYCLE_PAUSE: Duration = Duration::from_millis(200);
/// Polled until the support bitmaps have been read: the PIDs this dashboard
/// always polled, so a car that never answers 01 00 still shows them.
const FALLBACK_PIDS: &[u8] = &[0x0C, 0x0D, 0x05, 0x04, 0x11, 0x2F, 0x0F, 0x5C, 0x0B];
/// Shown as the four gauges; everything else goes to the metric list.
const GAUGE_KEYS: &[&str] = &["rpm", "speed", "coolant", "battery"];

#[derive(Debug, Clone, Default, Serialize)]
pub struct ObdData {
    pub rpm: f64,
    pub speed: f64,
    pub coolant_temp: f64,
    pub engine_load: f64,
    pub throttle: f64,
    pub fuel_level: f64,
    pub battery_voltage: f64,
    pub intake_temp: f64,
    /// Intake manifold absolute pressure (PID 0B), kPa.
    pub boost_pressure: f64,
    pub oil_temp: f64,
    pub is_connected: bool,
    /// True when no PID has decoded recently: the values below are the last
    /// known ones, not live readings. Consumers must not present them as
    /// current.
    pub stale: bool,
    /// Distinct codes across the stored, pending and permanent lists.
    pub dtc_count: u32,
    /// Stored (mode 03) codes.
    pub dtc_codes: Vec<String>,
    /// Pending (mode 07) codes.
    pub dtc_pending: Vec<String>,
    /// Permanent (mode 0A) codes.
    pub dtc_permanent: Vec<String>,
    /// Latest value of every decoded output, by key (see `pids`).
    pub values: BTreeMap<String, f64>,
    /// Mode 01 PIDs the car reports as supported (hex, e.g. "0C"); empty until
    /// the bitmaps have been read.
    pub supported_pids: Vec<String>,
    /// Supported PIDs this dashboard has no decoder for.
    pub undecoded_pids: Vec<String>,
    pub protocol: Option<String>,
    pub adapter: Option<String>,
    pub vin: Option<String>,
    /// Litres burned since rascar-app started, integrated on every fuel-flow
    /// sample (see [`fuel_rate_lph`]). Consumers take differences.
    pub fuel_used_l: f64,
    /// When the last fuel-flow sample was metered.
    #[serde(skip)]
    fuel_metered_at: Option<Instant>,
    /// Readiness monitors, mode 06 results, in-use performance counters.
    pub health: health::Health,
}

/// Air/fuel mass ratio and density (g/L) of Euro 95 E10 petrol. Pure
/// petrol is 14.7, E5 14.4; E10 at 14.7 under-reads the fuel by 2-3.5 %
/// (measured on the reference car).
const PETROL_AFR: f64 = 14.1;
const PETROL_G_PER_L: f64 = 750.0;
/// A gap between two fuel-flow samples longer than this (link drop, slow
/// cycle) is not integrated: the flow in between is unknown.
const MAX_METER_GAP: Duration = Duration::from_secs(3);

/// Fuel flow in L/h from live data: PID 5E when the car has it, else the air
/// flow at the E10 stoichiometric ratio. Overrun fuel cut (air flow under
/// 1 g/s while the engine turns above 900 rpm at speed) is 0: the injectors are
/// off. On this car the commanded-λ fuel-cut flag (PID 44) is read too rarely
/// to zero fast samples (measured: it cost 5.6 % of the fuel).
pub fn fuel_rate_lph(o: &ObdData) -> Option<f64> {
    if !o.is_connected || o.stale {
        return None;
    }
    if let Some(r) = o.values.get("fuel_rate").copied().filter(|r| r.is_finite() && *r >= 0.0) {
        return Some(r);
    }
    let maf = o.values.get("maf").copied().filter(|m| m.is_finite() && *m >= 0.0)?;
    let rpm = o.values.get("rpm").copied().unwrap_or(0.0);
    let speed = o.values.get("speed").copied().unwrap_or(0.0);
    if maf < 1.0 && rpm > 900.0 && speed > 10.0 {
        return Some(0.0);
    }
    Some(maf * 3600.0 / (PETROL_AFR * PETROL_G_PER_L))
}

impl ObdData {
    fn set_value(&mut self, key: &str, v: f64) {
        self.values.insert(key.to_string(), v);
        match key {
            "rpm" => self.rpm = v,
            "speed" => self.speed = v,
            "coolant" => self.coolant_temp = v,
            "load" => self.engine_load = v,
            "throttle" => self.throttle = v,
            "fuel" => self.fuel_level = v,
            "intake" => self.intake_temp = v,
            "boost" => self.boost_pressure = v,
            "oil" => self.oil_temp = v,
            "battery" => self.battery_voltage = v,
            _ => {}
        }
    }

    /// Forget a value the ECU reported as unavailable. The legacy fields keep
    /// their last value (consumers read them as plain numbers); the metric
    /// list and `values` consumers see it as missing.
    /// Add the fuel burned since the previous fuel-flow sample at the current
    /// rate. Called after each MAF / fuel-rate decode, so trips see every fast
    /// sample instead of one held value per 5 s cycle.
    fn meter_fuel(&mut self, now: Instant) {
        let rate = fuel_rate_lph(self);
        if let (Some(rate), Some(prev)) = (rate, self.fuel_metered_at) {
            let dt = now.saturating_duration_since(prev);
            if dt <= MAX_METER_GAP {
                self.fuel_used_l += rate * dt.as_secs_f64() / 3600.0;
            }
        }
        self.fuel_metered_at = rate.map(|_| now);
    }

    fn clear_value(&mut self, key: &str) {
        self.values.remove(key);
    }

    fn recount_dtcs(&mut self) {
        let mut all: Vec<&String> = self
            .dtc_codes
            .iter()
            .chain(self.dtc_pending.iter())
            .chain(self.dtc_permanent.iter())
            .collect();
        all.sort();
        all.dedup();
        self.dtc_count = all.len() as u32;
    }
}

/// One row of the OBD page's metric list.
#[derive(Debug, Clone, Serialize)]
pub struct MetricView {
    pub k: &'static str,
    pub l: &'static str,
    pub u: &'static str,
    pub d: u8,
    pub v: f64,
    /// Text of an enumerated value (fuel system status...), shown instead of
    /// the number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<&'static str>,
}

/// Every decoded value that is not one of the four gauges, in catalogue order.
pub fn metrics_view(data: &ObdData) -> Vec<MetricView> {
    pids::all_outputs()
        .filter(|o| !GAUGE_KEYS.contains(&o.key))
        .filter_map(|o| {
            data.values.get(o.key).map(|&v| MetricView {
                k: o.key,
                l: o.label,
                u: o.unit,
                d: o.decimals,
                v,
                t: o.text.and_then(|f| f(v)),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct DtcEntry {
    pub code: String,
    pub description: String,
    /// False when the code is not in the table and `description` only says
    /// what the J2012 numbering tells (scope and subsystem).
    pub known: bool,
    pub kind: DtcKind,
}

/// Outcome of a DTC clear request, reported back to the caller instead of an
/// unconditional success.
#[derive(Debug, Clone, Serialize)]
pub struct ClearOutcome {
    /// True only when the ECU actually received the mode-04 command.
    pub ok: bool,
    /// True when the request reached the serial loop (even if it then failed).
    pub queued: bool,
    pub connected: bool,
    pub error: Option<String>,
}

/// A pending "clear DTCs" request: the serial loop takes it, transmits mode 04
/// and reports the real outcome back over the oneshot channel.
type ClearRequest = Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<(), String>>>>>;
type Histories = Arc<RwLock<HashMap<String, VecDeque<f64>>>>;

pub struct ObdService {
    pub data: Arc<RwLock<ObdData>>,
    /// Recent samples of every decoded value, by key, created on first decode.
    pub histories: Histories,
    /// Set by `clear_dtcs()`; the serial poll loop consumes it to transmit a
    /// real ELM327 mode-04 ("clear DTCs") command and acknowledges the result.
    clear_requested: ClearRequest,
    port: String,
    baud: u32,
}

impl ObdService {
    pub fn new(port: &str, baud: u32) -> Self {
        Self {
            data: Arc::new(RwLock::new(ObdData::default())),
            histories: Arc::new(RwLock::new(HashMap::new())),
            clear_requested: Arc::new(tokio::sync::Mutex::new(None)),
            port: port.to_string(),
            baud,
        }
    }

    pub async fn start(&self) {
        let data = self.data.clone();
        let histories = self.histories.clone();
        let clear_requested = self.clear_requested.clone();
        let port = self.port.clone();
        let baud = self.baud;
        let raw = RawLog::from_env();

        tokio::spawn(async move {
            let mut failures: u32 = 0;
            // Outlives a connection: a Bluetooth drop mid-drive must not
            // start the once-per-drive reads over.
            let mut drive = DriveState::from_env();
            loop {
                let mut adapter_answered = false;
                if let Err(e) = connect_and_poll(
                    &data,
                    &histories,
                    &clear_requested,
                    &raw,
                    &port,
                    baud,
                    &mut adapter_answered,
                    &mut drive,
                )
                .await
                {
                    warn!("OBD connection lost: {e}");
                    raw.event(&format!("disconnect {e}"));
                    data.write().await.is_connected = false;
                }
                // A link that came up and later dropped starts a fresh series
                // of quick retries.
                failures = if adapter_answered { 1 } else { failures.saturating_add(1) };
                tokio::time::sleep(reconnect_delay(failures)).await;
            }
        });

        info!("OBD service started (port={})", self.port);
    }

    pub async fn get_dtc_list(&self) -> Vec<DtcEntry> {
        let data = self.data.read().await;
        let lists = [
            (DtcKind::Stored, &data.dtc_codes),
            (DtcKind::Pending, &data.dtc_pending),
            (DtcKind::Permanent, &data.dtc_permanent),
        ];
        lists
            .into_iter()
            .flat_map(|(kind, codes)| {
                codes.iter().map(move |code| {
                    let (description, known) = dtc::describe(code);
                    DtcEntry { code: code.clone(), description, known, kind }
                })
            })
            .collect()
    }

    /// Request an ELM327 mode-04 ("clear DTCs") and wait for the serial loop to
    /// report what actually happened.
    ///
    /// The cached DTC list is cleared only once the ECU has acknowledged.
    /// Reporting success before transmission would tell the driver the fault
    /// codes are gone while the MIL is still lit and the codes are still in the
    /// ECU — and would fire a surprise mode-04 later, on reconnect.
    pub async fn clear_dtcs(&self) -> ClearOutcome {
        let connected = self.data.read().await.is_connected;
        if !connected {
            return ClearOutcome {
                ok: false,
                queued: false,
                connected: false,
                error: Some("OBD adapter not connected".to_string()),
            };
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        // Replacing a still-pending request is fine: its sender is dropped and
        // the earlier caller resolves as "cancelled".
        *self.clear_requested.lock().await = Some(tx);

        let outcome = match tokio::time::timeout(CLEAR_ACK_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => ClearOutcome { ok: true, queued: true, connected: true, error: None },
            Ok(Ok(Err(e))) => ClearOutcome {
                ok: false, queued: true, connected: true, error: Some(e),
            },
            // Sender dropped: the poll loop died (link lost) or a newer request
            // superseded this one.
            Ok(Err(_)) => ClearOutcome {
                ok: false, queued: true, connected: false,
                error: Some("OBD link lost before the clear was transmitted".to_string()),
            },
            Err(_) => ClearOutcome {
                ok: false, queued: true, connected: true,
                error: Some("timed out waiting for the ECU to acknowledge".to_string()),
            },
        };

        if outcome.ok {
            // Mode 04 erases stored and pending codes; permanent ones stay
            // until the ECU itself sees the fault gone. The next DTC read
            // repopulates whatever remains.
            let mut data = self.data.write().await;
            data.dtc_codes.clear();
            data.dtc_pending.clear();
            data.recount_dtcs();
        }
        outcome
    }
}

/// Back-off before reconnect attempt number `failures` (1 = first retry).
fn reconnect_delay(failures: u32) -> Duration {
    if failures <= QUICK_RETRIES { QUICK_RETRY } else { SLOW_RETRY }
}

fn push_history(histories: &mut HashMap<String, VecDeque<f64>>, key: &str, value: f64) {
    let buf = histories
        .entry(key.to_string())
        .or_insert_with(|| VecDeque::with_capacity(HISTORY_LEN));
    if buf.len() >= HISTORY_LEN {
        buf.pop_front();
    }
    buf.push_back(value);
}

fn hex_list(pids: &[u8]) -> Vec<String> {
    pids.iter().map(|p| format!("{p:02X}")).collect()
}

/// What the link talks to: the RFCOMM serial port, or a scripted adapter in
/// the tests.
trait Port: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Port for T {}

/// CAN request identifier the adapter sends to (`ATSH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    /// 7DF, every emission-related ECU answers (the ELM327 default).
    Functional,
    /// 7E0, the engine ECU alone (11-bit CAN).
    Ecm,
}

impl Header {
    fn command(self) -> &'static str {
        match self {
            Header::Functional => "ATSH7DF",
            Header::Ecm => "ATSH7E0",
        }
    }
}

/// The serial link plus the bookkeeping every exchange shares.
struct Link<'a, S = tokio_serial::SerialStream> {
    serial: S,
    raw: &'a RawLog,
    consecutive_errors: u32,
    /// Last journal report per command of a reply we could not decode.
    undecoded: HashMap<String, (Instant, u32)>,
    /// Header the adapter currently uses; `None` after an `ATSH` that was not
    /// acknowledged, so the next `set_header` sends it again.
    header: Option<Header>,
    /// Mode 01 goes to the ECM alone (set by discovery once 7E0 answered).
    ecm_only: bool,
    /// The adapter answered `ATSH` with something other than OK.
    atsh_rejected: bool,
}

impl<S: Port> Link<'_, S> {
    /// Send one command, log both directions verbatim, and return the cleaned
    /// reply. Too many consecutive failures end the connection (Err), so the
    /// supervisor reconnects with backoff.
    async fn exchange(&mut self, cmd: &str) -> Result<String> {
        self.raw.tx(cmd);
        let t0 = Instant::now();
        let (raw, result) = send_cmd(&mut self.serial, cmd).await;
        match result {
            Ok(()) => {
                self.raw.rx(t0.elapsed(), &raw);
                self.consecutive_errors = 0;
                Ok(raw.replace('>', "").trim().to_string())
            }
            Err(e) => {
                let partial = if raw.is_empty() { String::new() } else { format!(" (partial: {raw})") };
                self.raw.err(t0.elapsed(), &format!("{e}{partial}"));
                self.consecutive_errors += 1;
                warn!("OBD command {cmd} failed ({}): {e}", self.consecutive_errors);
                if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    anyhow::bail!("OBD link lost after {} consecutive errors: {e}", self.consecutive_errors);
                }
                Err(e)
            }
        }
    }

    /// Like `exchange`, but a single failed exchange is not an error for the
    /// caller (only a lost link is).
    async fn ask(&mut self, cmd: &str) -> Result<Option<String>> {
        match self.exchange(cmd).await {
            Ok(r) => Ok(Some(r)),
            Err(e) if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// Point requests at `want`, sending `ATSH` only when it changes. Returns
    /// whether the adapter acknowledged.
    async fn set_header(&mut self, want: Header) -> Result<bool> {
        if self.header == Some(want) {
            return Ok(true);
        }
        if self.atsh_rejected {
            return Ok(false);
        }
        match self.ask(want.command()).await? {
            Some(r) if elm::is_ok(&r) => {
                self.header = Some(want);
                Ok(true)
            }
            // Answered but refused ("?"): the adapter kept its header. Stop
            // asking, or a clone without ATSH would get one per cycle.
            Some(r) => {
                warn!("OBD {} rejected ({r:?}); request header left at the adapter default", want.command());
                self.atsh_rejected = true;
                Ok(false)
            }
            // No answer: whether it was applied is unknown, so send it again
            // next time.
            None => {
                self.header = None;
                Ok(false)
            }
        }
    }

    /// Header for mode 01 / 09 requests: the ECM alone when discovery
    /// established that it answers physically, else functional.
    fn mode01_header(&self) -> Header {
        if self.ecm_only { Header::Ecm } else { Header::Functional }
    }

    /// Report a reply we could not decode to the journal, rate-limited per
    /// command. Everything is in the raw log regardless.
    fn note_undecoded(&mut self, cmd: &str, resp: &str) {
        let now = Instant::now();
        let entry = self.undecoded.entry(cmd.to_string()).or_insert((now - UNDECODED_REPORT_EVERY, 0));
        entry.1 += 1;
        if now.duration_since(entry.0) >= UNDECODED_REPORT_EVERY {
            info!(
                "OBD {cmd}: reply not decoded: {:?} ({} undecoded since last report)",
                rawlog::escape(resp),
                entry.1
            );
            *entry = (now, 0);
        }
    }
}

/// What the support bitmaps said.
struct Discovery {
    supported: Vec<u8>,
    /// Mode 09 info types the car supports (09 00 bitmap).
    info_types: Vec<u8>,
}

/// Read the mode 01 support bitmaps (01 00, 01 20, ... while the last bit
/// says the next one exists) with the adapter's current header.
async fn read_support<S: Port>(link: &mut Link<'_, S>) -> Result<Vec<u8>> {
    let mut supported: Vec<u8> = Vec::new();
    let mut base: u8 = 0x00;
    loop {
        let cmd = format!("01{base:02X}");
        let Some(resp) = link.ask(&cmd).await? else { break };
        let Some(pids) = pids::parse_support_bitmap(&resp, base) else {
            if base == 0 {
                link.note_undecoded(&cmd, &resp);
            }
            break;
        };
        info!("OBD support bitmap {cmd}: {:?}", rawlog::escape(&resp));
        let next = base.checked_add(0x20);
        let more = next.is_some_and(|n| pids.contains(&n));
        supported.extend(pids.into_iter().filter(|p| p % 0x20 != 0));
        match next {
            Some(n) if more && n <= 0xE0 => base = n,
            _ => break,
        }
    }
    Ok(supported)
}

async fn discover<S: Port>(link: &mut Link<'_, S>, data: &Arc<RwLock<ObdData>>) -> Result<Option<Discovery>> {
    // Functional first: with ATSP0 the adapter searches the protocol on this
    // request, and the ECM's physical id is only known once it is CAN 11-bit.
    link.ecm_only = false;
    link.set_header(Header::Functional).await?;
    let mut supported = read_support(link).await?;
    if supported.is_empty() {
        return Ok(None);
    }

    // Protocol: decides how DTC replies are framed (CAN has a count byte) and
    // whether the ECM can be addressed alone.
    let mut protocol = None;
    if let Some(resp) = link.ask("ATDPN").await? {
        if let Some(p) = elm::protocol_number(&resp) {
            info!("OBD protocol {p:X}: {}", elm::protocol_name(p));
            protocol = Some(p);
        }
    }

    // ECM-only addressing on 11-bit CAN. If the clone rejects ATSH or the ECM
    // does not answer 7E0, stay functional: replies from two ECUs then remain
    // ambiguous (headers are off, so the sender is unknown and
    // `pids::mode01_data` takes the first message). Turning ATH1 on instead
    // would change every reply's format; not worth it for a fallback.
    if protocol.is_some_and(elm::protocol_is_can_11bit) {
        let ecm = if link.set_header(Header::Ecm).await? { read_support(link).await? } else { Vec::new() };
        if ecm.is_empty() {
            warn!("OBD: ECM did not answer physical requests (7E0); staying on functional 7DF, two-ECU replies stay ambiguous");
            link.set_header(Header::Functional).await?;
        } else {
            info!("OBD: mode 01 addressed to the ECM alone (7E0), {} PIDs", ecm.len());
            link.ecm_only = true;
            supported = ecm;
        }
    }

    let (with, without): (Vec<u8>, Vec<u8>) = supported.iter().partition(|p| pids::find(**p).is_some());
    info!("OBD supported mode 01 PIDs ({}): {}", supported.len(), hex_list(&supported).join(" "));
    info!("OBD polled (decoder available, {}): {}", with.len(), hex_list(&with).join(" "));
    if !without.is_empty() {
        info!("OBD supported but not decoded ({}): {}", without.len(), hex_list(&without).join(" "));
    }

    // Mode 09: vehicle information. Only the VIN is decoded; the bitmap is
    // logged so the other items (calibration ids, ECU name) can be added.
    let mut vin = None;
    let mut info_types = Vec::new();
    if let Some(resp) = link.ask("0900").await? {
        let m09: Vec<u8> = elm::messages(&resp)
            .into_iter()
            .filter(|m| m.len() >= 6 && m[0] == 0x49 && m[1] == 0x00)
            .fold(0u32, |acc, m| acc | u32::from_be_bytes([m[2], m[3], m[4], m[5]]))
            .to_be_bytes()
            .iter()
            .enumerate()
            .flat_map(|(i, byte)| (0..8).filter(move |b| byte & (0x80 >> b) != 0).map(move |b| (i * 8 + b + 1) as u8))
            .collect();
        info!("OBD mode 09 supported: {}", hex_list(&m09).join(" "));
        info_types = m09.clone();
        if m09.contains(&0x02) {
            if let Some(resp) = link.ask("0902").await? {
                vin = pids::parse_vin(&resp);
                match &vin {
                    Some(v) => info!("OBD VIN {v}"),
                    None => link.note_undecoded("0902", &resp),
                }
            }
        }
    }

    {
        let mut d = data.write().await;
        d.supported_pids = hex_list(&supported);
        d.undecoded_pids = hex_list(&without);
        d.protocol = protocol.map(|p| format!("{p:X} {}", elm::protocol_name(p)));
        if vin.is_some() {
            d.vin = vin;
        }
    }
    Ok(Some(Discovery { supported: with, info_types }))
}

/// Poll one mode 01 PID; true when it decoded.
async fn poll_pid<S: Port>(
    link: &mut Link<'_, S>,
    pid: u8,
    data: &Arc<RwLock<ObdData>>,
    histories: &Histories,
) -> Result<bool> {
    let Some(def) = pids::find(pid) else { return Ok(false) };
    let cmd = format!("01{pid:02X}");
    let Some(resp) = link.ask(&cmd).await? else { return Ok(false) };
    let Some(bytes) = pids::mode01_data(&resp, pid) else {
        link.note_undecoded(&cmd, &resp);
        return Ok(false);
    };
    let (decoded, invalid) = decode_outputs(def, &bytes);
    let mut d = data.write().await;
    // Readiness (01 01) and this-drive (01 41) monitor bits, per monitor.
    if pid == 0x01 || pid == 0x41 {
        d.health.set_monitor_bytes(pid, &bytes);
    }
    // The ECU answered, but with a sentinel ("not available", fuel cut): the
    // previous reading is no longer current, so it is removed rather than
    // left on screen.
    for key in invalid {
        d.clear_value(key);
    }
    if decoded.is_empty() {
        drop(d);
        link.note_undecoded(&cmd, &resp);
        return Ok(false);
    }
    let mut h = histories.write().await;
    for (key, v) in decoded {
        d.set_value(key, v);
        push_history(&mut h, key, v);
    }
    if pid == 0x10 || pid == 0x5E {
        d.meter_fuel(Instant::now());
    }
    // True boost = MAP − barometric pressure, when the car reports both.
    if pid == 0x0B {
        if let (Some(map), Some(baro)) = (d.values.get("boost").copied(), d.values.get("baro").copied()) {
            d.set_value("boost_gauge", map - baro);
            push_history(&mut h, "boost_gauge", map - baro);
        }
    }
    Ok(true)
}

/// Split a PID's outputs into decoded values and keys whose value is missing
/// or invalid in this reply.
fn decode_outputs(def: &pids::PidDef, bytes: &[u8]) -> (Vec<(&'static str, f64)>, Vec<&'static str>) {
    let mut decoded = Vec::new();
    let mut invalid = Vec::new();
    for o in def.outputs {
        match (o.decode)(bytes).filter(|v| v.is_finite()) {
            Some(v) => decoded.push((o.key, v)),
            None => invalid.push(o.key),
        }
    }
    (decoded, invalid)
}

/// Stored, pending and permanent DTCs, functionally addressed so every ECU's
/// codes are read (the gearbox's too); the next poll cycle switches mode 01
/// back to the ECM.
async fn dtc_round<S: Port>(link: &mut Link<'_, S>, data: &Arc<RwLock<ObdData>>) -> Result<()> {
    let can = protocol_of(data).await.map(elm::protocol_is_can);
    link.set_header(Header::Functional).await?;
    for kind in [DtcKind::Stored, DtcKind::Pending, DtcKind::Permanent] {
        if let Some(codes) = read_dtcs(link, kind, can).await? {
            let mut d = data.write().await;
            let list = match kind {
                DtcKind::Stored => &mut d.dtc_codes,
                DtcKind::Pending => &mut d.dtc_pending,
                DtcKind::Permanent => &mut d.dtc_permanent,
            };
            if *list != codes {
                info!("OBD {kind:?} DTCs ({}): {:?}", kind.request(), codes);
                *list = codes;
            }
            d.recount_dtcs();
        }
    }
    Ok(())
}

/// Read one DTC list. Returns `None` when the ECU did not answer positively
/// (mode not supported, bus asleep), so the previous list is kept.
async fn read_dtcs<S: Port>(link: &mut Link<'_, S>, kind: DtcKind, can: Option<bool>) -> Result<Option<Vec<String>>> {
    let Some(resp) = link.ask(kind.request()).await? else { return Ok(None) };
    if !dtc::is_positive(&resp, kind) {
        link.note_undecoded(kind.request(), &resp);
        return Ok(None);
    }
    Ok(Some(dtc::parse_dtcs(&resp, kind, can)))
}

#[allow(clippy::too_many_arguments)]
async fn connect_and_poll(
    data: &Arc<RwLock<ObdData>>,
    histories: &Histories,
    clear_requested: &ClearRequest,
    raw: &RawLog,
    port: &str,
    baud: u32,
    adapter_answered: &mut bool,
    drive: &mut DriveState,
) -> Result<()> {
    use tokio_serial::SerialPortBuilderExt;

    let serial = tokio_serial::new(port, baud)
        .timeout(Duration::from_secs(2))
        .open_native_async()?;
    raw.event(&format!("connect {port} {baud}"));
    let mut link = Link {
        serial,
        raw,
        consecutive_errors: 0,
        undecoded: HashMap::new(),
        header: None,
        ecm_only: false,
        atsh_rejected: false,
    };

    // ELM327 init: reset, echo off, linefeeds off, spaces on, headers off,
    // automatic protocol.
    //
    // The first exchange after opening the port also carries the Bluetooth
    // connection set-up: ATZ answers in 0.3-5.4 s in the raw log, so it cannot
    // be given a shorter deadline. When the page fails, the port returns EOF
    // after 5.12 s (the Bluetooth page timeout) and every later write fails
    // with EIO; give up at once and let the supervisor retry quickly, rather
    // than spending another 5 exchanges and the fixed back-off on a dead link.
    if link.ask("ATZ").await?.is_none() {
        anyhow::bail!("adapter did not answer ATZ");
    }
    *adapter_answered = true;
    // The ELM prints its prompt once the reset is done; a short margin for
    // clones that still settle after it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    link.header = Some(Header::Functional);
    for cmd in ["ATE0", "ATL0", "ATS1", "ATH0", "ATSP0"] {
        link.ask(cmd).await?;
    }
    if let Some(v) = link.ask("ATI").await? {
        info!("OBD adapter: {v}");
        data.write().await.adapter = Some(v);
    }

    {
        let mut d = data.write().await;
        d.is_connected = true;
        d.stale = true; // nothing decoded yet on this link
    }
    info!("OBD connected via {port}");

    // Last time a PID actually decoded. `is_connected` alone only says the
    // adapter is answering; it says nothing about the bus behind it.
    let mut last_decode = Instant::now();
    let mut discovery: Option<Discovery> = None;
    let mut last_discovery: Option<Instant> = None;
    let mut last_polled: HashMap<u8, Instant> = HashMap::new();
    let mut last_battery: Option<Instant> = None;
    let mut last_dtc: Option<Instant> = None;
    // `Tier::Once` PIDs that decoded on this link.
    let mut once_done: std::collections::HashSet<u8> = std::collections::HashSet::new();

    loop {
        if discovery.is_none() && last_discovery.is_none_or(|t| t.elapsed() >= DISCOVERY_RETRY) {
            last_discovery = Some(Instant::now());
            discovery = discover(&mut link, data).await?;
            if discovery.is_none() {
                info!("OBD support bitmaps unavailable (ignition off?); polling the default PIDs, retry in {DISCOVERY_RETRY:?}");
            }
        }
        let polled: &[u8] = discovery.as_ref().map(|d| d.supported.as_slice()).unwrap_or(FALLBACK_PIDS);

        // Fast PIDs every cycle, then the most overdue slower ones.
        let now = Instant::now();
        let mut due: Vec<u8> = polled.iter().copied().filter(|p| pids::find(*p).is_some_and(|d| d.tier == pids::Tier::Fast)).collect();
        let mut slower: Vec<(Duration, u8)> = polled
            .iter()
            .filter_map(|&p| {
                let def = pids::find(p)?;
                if def.tier == pids::Tier::Fast || once_done.contains(&p) {
                    return None;
                }
                let overdue = match last_polled.get(&p) {
                    None => Duration::MAX,
                    Some(t) => now.duration_since(*t).checked_sub(def.tier.interval())?,
                };
                Some((overdue, p))
            })
            .collect();
        slower.sort_by(|a, b| b.0.cmp(&a.0));
        due.extend(slower.into_iter().take(MAX_SLOWER_PER_CYCLE).map(|(_, p)| p));

        // Did anything decode on this cycle? Drives the staleness flag.
        let mut decoded_any = false;
        // Back to the mode 01 header after a DTC round or a clear.
        link.set_header(link.mode01_header()).await?;
        for pid in due {
            last_polled.insert(pid, Instant::now());
            if poll_pid(&mut link, pid, data, histories).await? {
                decoded_any = true;
                if pids::find(pid).is_some_and(|d| d.tier == pids::Tier::Once) {
                    once_done.insert(pid);
                }
            }
        }

        // Once-per-drive reads (mode 06, 09 08) and the tyre pressures, while
        // the engine runs. Rpm is a Fast PID: it decoded on this cycle when
        // the bus is live.
        let running = decoded_any && data.read().await.values.get("rpm").is_some_and(|r| *r >= RUNNING_RPM);
        drive_step(&mut link, drive, running, discovery.as_ref(), data, histories).await?;

        // Battery voltage, measured by the adapter itself (not the ECU), so it
        // is deliberately NOT counted as a live-bus decode.
        if last_battery.is_none_or(|t| t.elapsed() >= pids::Tier::Medium.interval()) {
            last_battery = Some(Instant::now());
            if let Some(resp) = link.ask("ATRV").await? {
                match resp.trim().trim_end_matches(['V', 'v']).trim().parse::<f64>() {
                    Ok(v) => {
                        data.write().await.set_value("battery", v);
                        push_history(&mut *histories.write().await, "battery", v);
                    }
                    Err(_) => link.note_undecoded("ATRV", &resp),
                }
            }
        }

        // Honour a pending clear request: transmit ELM327 mode 04 and report
        // the actual outcome back to whoever asked.
        let pending = clear_requested.lock().await.take();
        if let Some(ack) = pending {
            // Functional, so every ECU clears its codes, not only the ECM.
            link.set_header(Header::Functional).await?;
            let result = match link.exchange("04").await {
                // Mode 04 answers "44"; anything else (NO DATA, ERROR, ?) means
                // the ECU refused or never saw it.
                Ok(resp) if elm::messages(&resp).iter().any(|m| m.first() == Some(&0x44)) => {
                    info!("OBD DTCs cleared (mode 04)");
                    last_dtc = None; // re-read at once
                    Ok(())
                }
                Ok(resp) => {
                    warn!("OBD clear (mode 04) rejected: {resp}");
                    Err(format!("ECU did not accept the clear command: {resp}"))
                }
                Err(e) => {
                    warn!("OBD clear (mode 04) failed: {e}");
                    let _ = ack.send(Err(format!("clear command failed: {e}")));
                    if link.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return Err(e);
                    }
                    continue;
                }
            };
            let _ = ack.send(result);
        }

        // Stored / pending / permanent DTCs, on the first cycle and then every
        // DTC_EVERY.
        if last_dtc.is_none_or(|t| t.elapsed() >= DTC_EVERY) {
            last_dtc = Some(Instant::now());
            dtc_round(&mut link, data).await?;
        }

        // Track staleness: an adapter answering "NO DATA" keeps every field
        // frozen. Report that instead of a green badge over fabricated values.
        if decoded_any {
            last_decode = Instant::now();
        }
        let idle = last_decode.elapsed();
        {
            let mut d = data.write().await;
            d.stale = idle >= STALE_AFTER;
            if idle >= DISCONNECT_AFTER && d.is_connected {
                warn!("OBD adapter responding but no PID decoded for {idle:?} — reporting disconnected");
                d.is_connected = false;
            } else if decoded_any {
                d.is_connected = true;
            }
        }
        // The bus came back after the bitmaps failed: read them now.
        if decoded_any && discovery.is_none() {
            last_discovery = None;
        }

        // The fixed 200 ms pause per cycle came from the first scaffold (which
        // also slept between PIDs) and was 14 % of every cycle doing nothing.
        // Every exchange already waits for the adapter's prompt, so a live bus
        // needs no pacing; only an idle one is slowed down.
        if !decoded_any {
            tokio::time::sleep(IDLE_CYCLE_PAUSE).await;
        }
    }
}

/// Engine speed from which the engine counts as running.
const RUNNING_RPM: f64 = 400.0;
/// Engine run time before the once-per-drive reads (mode 06, 09 08): the
/// monitors of this drive have had a chance to run, and the gauges are not
/// slowed while the car pulls away.
const DRIVE_READS_AFTER: Duration = Duration::from_secs(120);
/// No running engine for longer than this ends a drive. A shorter gap (a
/// stall, a Bluetooth drop) is the same drive.
const DRIVE_GAP: Duration = Duration::from_secs(120);
/// Upper bound on the mode 06 requests of one drive (bitmaps + MIDs): ~15 on
/// a 4-cylinder petrol, 64 leaves room for a V8.
const MODE06_MAX_REQUESTS: usize = 64;
/// Tyre pressures are read this often while the engine runs.
const TPMS_EVERY: Duration = Duration::from_secs(60);
/// TPMS rounds in a row without a single answer before the reads stop for
/// the session (the module is absent or behind a gateway that drops 751).
const TPMS_MAX_SILENT_ROUNDS: u32 = 3;

/// Progress of the mode 06 read of the current drive.
#[derive(Debug, Default)]
enum Mode06Job {
    #[default]
    Idle,
    Running { queue: VecDeque<u8>, tests: Vec<health::MonitorTest>, sent: usize },
    Done,
}

/// Manufacturer tyre-pressure reads (`RASCAR_OBD_TPMS`).
#[derive(Debug, Default)]
struct TpmsState {
    enabled: bool,
    /// Why the reads stopped for this session (rejected AT command, module
    /// silent, every DID refused).
    stopped: Option<String>,
    /// DIDs answered with a negative response: not asked again.
    unsupported: HashSet<u16>,
    silent_rounds: u32,
    last: Option<Instant>,
    reported: bool,
}

/// Per-drive bookkeeping. Lives across reconnects.
#[derive(Debug, Default)]
struct DriveState {
    running_since: Option<Instant>,
    last_running: Option<Instant>,
    mode06: Mode06Job,
    ipt_done: bool,
    tpms: TpmsState,
}

impl DriveState {
    fn from_env() -> Self {
        let enabled = std::env::var("RASCAR_OBD_TPMS")
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        if enabled {
            info!("OBD TPMS reads enabled (RASCAR_OBD_TPMS): 751 22 2076-2079 every {TPMS_EVERY:?} while the engine runs");
        }
        Self { tpms: TpmsState { enabled, ..Default::default() }, ..Default::default() }
    }

    /// Track the engine; a run after a gap longer than `DRIVE_GAP` starts a
    /// new drive and re-arms the once-per-drive reads.
    fn observe(&mut self, running: bool, now: Instant) {
        if !running {
            return;
        }
        if self.last_running.is_none_or(|t| now.saturating_duration_since(t) > DRIVE_GAP) {
            self.running_since = Some(now);
            self.mode06 = Mode06Job::Idle;
            self.ipt_done = false;
            info!("OBD: engine running, new drive (mode 06 / 09 08 in {DRIVE_READS_AFTER:?})");
        }
        self.last_running = Some(now);
    }

    fn running_for(&self, now: Instant) -> Duration {
        self.running_since.map(|t| now.saturating_duration_since(t)).unwrap_or_default()
    }
}

/// Protocol number from the discovered protocol text ("6 ISO 15765-4 ...").
async fn protocol_of(data: &Arc<RwLock<ObdData>>) -> Option<u8> {
    let d = data.read().await;
    d.protocol.as_deref().and_then(|p| p.split(' ').next()).and_then(|n| u8::from_str_radix(n, 16).ok())
}

fn local_hhmm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// One step of the per-drive work: at most one mode 06 / 09 08 request per
/// cycle, and a TPMS round when one is due.
async fn drive_step<S: Port>(
    link: &mut Link<'_, S>,
    drive: &mut DriveState,
    running: bool,
    discovery: Option<&Discovery>,
    data: &Arc<RwLock<ObdData>>,
    histories: &Histories,
) -> Result<()> {
    let now = Instant::now();
    drive.observe(running, now);
    if !running {
        return Ok(());
    }
    let proto = protocol_of(data).await;

    if drive.running_for(now) >= DRIVE_READS_AFTER {
        if !matches!(drive.mode06, Mode06Job::Done) {
            // Mode 06's record format is the CAN one; K-line differs.
            if proto.is_some_and(elm::protocol_is_can) {
                mode06_step(link, &mut drive.mode06, data).await?;
            } else {
                drive.mode06 = Mode06Job::Done;
            }
        } else if !drive.ipt_done {
            drive.ipt_done = true;
            if discovery.is_some_and(|d| d.info_types.contains(&0x08)) {
                read_ipt(link, data).await?;
            }
        }
    }

    let t = &drive.tpms;
    let tpms_due = t.enabled && t.stopped.is_none() && t.last.is_none_or(|l| now.saturating_duration_since(l) >= TPMS_EVERY);
    if tpms_due && proto.is_some_and(elm::protocol_is_can_11bit) && !link.atsh_rejected {
        tpms_round(link, &mut drive.tpms, data, histories).await?;
    }
    Ok(())
}

/// Send the next mode 06 request: the support bitmaps (06 00, 06 20, ...)
/// queue the supported MIDs, each MID reply adds its test records, and the
/// last one publishes the results.
async fn mode06_step<S: Port>(link: &mut Link<'_, S>, job: &mut Mode06Job, data: &Arc<RwLock<ObdData>>) -> Result<()> {
    if matches!(job, Mode06Job::Idle) {
        info!("OBD mode 06: reading on-board monitor results");
        *job = Mode06Job::Running { queue: VecDeque::from([0x00]), tests: Vec::new(), sent: 0 };
    }
    let Mode06Job::Running { queue, tests, sent } = job else { return Ok(()) };
    let Some(mid) = queue.pop_front() else { return Ok(()) };
    *sent += 1;
    let cmd = format!("06{mid:02X}");
    if let Some(resp) = link.ask(&cmd).await? {
        if mid % 0x20 == 0 {
            match health::parse_bitmap(&resp, 0x46, mid) {
                Some(ids) => {
                    queue.extend(ids.iter().copied().filter(|m| m % 0x20 != 0));
                    let next = mid.checked_add(0x20).filter(|n| ids.contains(n) && *n <= 0xE0);
                    if let Some(n) = next {
                        queue.push_front(n);
                    }
                }
                None if mid == 0 => {
                    info!("OBD mode 06 not supported: {:?}", rawlog::escape(&resp));
                    *job = Mode06Job::Done;
                    return Ok(());
                }
                None => link.note_undecoded(&cmd, &resp),
            }
        } else {
            let found = health::parse_mode06(&resp);
            if found.is_empty() {
                link.note_undecoded(&cmd, &resp);
            }
            tests.extend(found);
        }
    }
    if queue.is_empty() || *sent >= MODE06_MAX_REQUESTS {
        let mut d = data.write().await;
        d.health.set_mode06(tests, local_hhmm());
        let misfire: Vec<String> = d
            .health
            .misfire
            .iter()
            .map(|c| {
                let n = |v: Option<f64>| v.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into());
                let name = if c.cyl == 0 { "all".to_string() } else { format!("cyl{}", c.cyl) };
                format!("{name} {}/{}", n(c.this_drive), n(c.avg10))
            })
            .collect();
        let failed: Vec<String> = d.health.out_of_limits.iter().map(|t| format!("{} {}/{} {}", t.monitor, t.mid, t.tid, t.raw)).collect();
        info!(
            "OBD mode 06: {} tests in {} requests; misfire this drive/avg10: [{}]; out of limits: [{}]",
            tests.len(),
            sent,
            misfire.join(", "),
            failed.join("; ")
        );
        *job = Mode06Job::Done;
    }
    Ok(())
}

/// In-use performance tracking (09 08), once per drive.
async fn read_ipt<S: Port>(link: &mut Link<'_, S>, data: &Arc<RwLock<ObdData>>) -> Result<()> {
    let Some(resp) = link.ask("0908").await? else { return Ok(()) };
    let Some(ipt) = health::parse_ipt(&resp) else {
        link.note_undecoded("0908", &resp);
        return Ok(());
    };
    let summary: Vec<String> = ipt
        .monitors
        .iter()
        .filter(|m| m.cond > 0 || m.comp > 0)
        .map(|m| format!("{} {}/{}", m.name, m.comp, m.cond))
        .collect();
    info!(
        "OBD in-use performance (09 08): ignition cycles {}, OBD conditions {}, monitors comp/cond: {}",
        ipt.ignition_cycles,
        ipt.obd_conditions,
        summary.join(", ")
    );
    let mut d = data.write().await;
    d.health.ipt = Some(ipt);
    d.health.ipt_at = Some(local_hhmm());
    Ok(())
}

/// Read the tyre pressures from the TPMS module, then put the adapter back on
/// the mode 01 addressing whatever happened.
async fn tpms_round<S: Port>(
    link: &mut Link<'_, S>,
    tpms: &mut TpmsState,
    data: &Arc<RwLock<ObdData>>,
    histories: &Histories,
) -> Result<()> {
    tpms.last = Some(Instant::now());
    let result = tpms_reads(link, tpms, data, histories).await;
    // A lost link is reported first; the reconnect's ATZ resets the adapter.
    let restored = restore_addressing(link).await;
    result.and(restored)
}

async fn tpms_reads<S: Port>(
    link: &mut Link<'_, S>,
    tpms: &mut TpmsState,
    data: &Arc<RwLock<ObdData>>,
    histories: &Histories,
) -> Result<()> {
    let (atsh, atcra) = health::manufacturer_addressing(health::TPMS_REQUEST_ID).map_err(anyhow::Error::msg)?;
    // From here the adapter's header is none of `Header`'s: the next
    // `set_header` must send ATSH whatever it was.
    link.header = None;
    for at in [&atsh, &atcra] {
        match link.ask(at).await? {
            Some(r) if elm::is_ok(&r) => {}
            // No answer at all: try again next round.
            None => return Ok(()),
            Some(r) => {
                let why = format!("{at} not accepted ({:?})", rawlog::escape(&r));
                warn!("OBD TPMS stopped for this session: {why}");
                tpms.stopped = Some(why);
                return Ok(());
            }
        }
    }
    let mut answered = false;
    let mut read: Vec<String> = Vec::new();
    for &(did, key) in health::TPMS_DIDS {
        if tpms.unsupported.contains(&did) {
            continue;
        }
        let cmd = health::manufacturer_request(0x22, did).map_err(anyhow::Error::msg)?;
        let Some(resp) = link.ask(&cmd).await? else { continue };
        match health::parse_did_reply(&resp, did) {
            health::DidReply::Data(bytes) => match health::tyre_bar(&bytes) {
                Some(bar) => {
                    answered = true;
                    read.push(format!("{key} {bar:.2}"));
                    data.write().await.set_value(key, bar);
                    push_history(&mut *histories.write().await, key, bar);
                }
                None => link.note_undecoded(&cmd, &resp),
            },
            health::DidReply::Negative(nrc) if health::nrc_is_transient(nrc) => answered = true,
            health::DidReply::Negative(nrc) => {
                answered = true;
                warn!("OBD TPMS {cmd}: negative response 7F 22 {nrc:02X}, not asked again this session");
                tpms.unsupported.insert(did);
            }
            health::DidReply::None => link.note_undecoded(&cmd, &resp),
        }
    }
    if answered {
        tpms.silent_rounds = 0;
        if !tpms.reported && !read.is_empty() {
            info!("OBD TPMS: {} bar", read.join(", "));
            tpms.reported = true;
        }
    } else {
        tpms.silent_rounds += 1;
        if tpms.silent_rounds >= TPMS_MAX_SILENT_ROUNDS {
            let why = format!("no answer from {:03X} in {} rounds", health::TPMS_REQUEST_ID, tpms.silent_rounds);
            warn!("OBD TPMS stopped for this session: {why}");
            tpms.stopped = Some(why);
        }
    }
    if health::TPMS_DIDS.iter().all(|(did, _)| tpms.unsupported.contains(did)) {
        tpms.stopped = Some("every tyre DID refused".into());
    }
    Ok(())
}

/// Reset the CAN receive filter and point requests back at the mode 01
/// header.
async fn restore_addressing<S: Port>(link: &mut Link<'_, S>) -> Result<()> {
    // `ATCRA` without an address resets the receive filter to automatic.
    // A clone that refuses it gets `ATAR` (automatic receive address).
    let reset = link.ask("ATCRA").await?;
    if !reset.as_deref().is_some_and(elm::is_ok) {
        link.ask("ATAR").await?;
    }
    link.header = None;
    link.set_header(link.mode01_header()).await?;
    Ok(())
}

/// Write `cmd\r` and read until the ELM327 prompt `>`. Returns whatever was
/// received (verbatim, also on error, for the raw log) and the outcome.
async fn send_cmd<S: Port>(serial: &mut S, cmd: &str) -> (String, Result<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut response = Vec::new();
    let result = async {
        serial.write_all(format!("{cmd}\r").as_bytes()).await?;
        serial.flush().await?;
        let mut buf = [0u8; 128];
        // Read until the prompt appears, but bail on an overall deadline and a
        // hard byte cap so a port that never sends '>' cannot hang or grow the
        // buffer without bound.
        let read_loop = async {
            loop {
                let n = serial.read(&mut buf).await?;
                response.extend_from_slice(&buf[..n]);
                if response.contains(&b'>') {
                    break;
                }
                // End of file without a prompt: the RFCOMM link is gone (a
                // failed Bluetooth page ends this way after 5.12 s). It used to
                // count as an empty success and the init carried on into EIOs.
                if n == 0 {
                    anyhow::bail!("OBD port closed (EOF) before the prompt");
                }
                if response.len() >= MAX_RESPONSE_BYTES {
                    anyhow::bail!("OBD response exceeded {MAX_RESPONSE_BYTES} bytes");
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::time::timeout(CMD_DEADLINE, read_loop)
            .await
            .map_err(|_| anyhow::anyhow!("OBD read timed out after {CMD_DEADLINE:?}"))?
    }
    .await;
    (String::from_utf8_lossy(&response).into_owned(), result)
}

#[cfg(test)]
mod fuel_meter_tests {
    use super::*;

    fn obd(vals: &[(&str, f64)]) -> ObdData {
        let mut o = ObdData { is_connected: true, ..Default::default() };
        for (k, v) in vals {
            o.values.insert(k.to_string(), *v);
        }
        o
    }

    #[test]
    fn rate_prefers_5e_then_air_flow_and_zeroes_overrun_cut() {
        assert_eq!(fuel_rate_lph(&obd(&[("fuel_rate", 4.2), ("maf", 10.0)])), Some(4.2));
        // 10 g/s of air at 14.1:1 = 0.709 g/s of E10 = 3.40 L/h.
        let r = fuel_rate_lph(&obd(&[("maf", 10.0), ("rpm", 2000.0), ("speed", 80.0)])).unwrap();
        assert!((r - 3.404).abs() < 0.01, "{r}");
        assert_eq!(fuel_rate_lph(&obd(&[("maf", 0.2), ("rpm", 1500.0), ("speed", 60.0)])), Some(0.0));
        // Idle at a light: low flow but not a cut.
        assert!(fuel_rate_lph(&obd(&[("maf", 0.8), ("rpm", 700.0), ("speed", 0.0)])).unwrap() > 0.0);
        assert_eq!(fuel_rate_lph(&obd(&[("rpm", 800.0)])), None);
        let mut stale = obd(&[("maf", 10.0)]);
        stale.stale = true;
        assert_eq!(fuel_rate_lph(&stale), None);
    }

    #[test]
    fn meter_integrates_samples_and_skips_gaps() {
        let mut o = obd(&[("fuel_rate", 3.6)]); // 1 mL/s
        let t0 = Instant::now();
        o.meter_fuel(t0);
        o.meter_fuel(t0 + Duration::from_millis(1500));
        assert!((o.fuel_used_l - 0.0015).abs() < 1e-9, "{}", o.fuel_used_l);
        // A 10 s gap (link drop) adds nothing; metering resumes after it.
        o.meter_fuel(t0 + Duration::from_millis(11_500));
        assert!((o.fuel_used_l - 0.0015).abs() < 1e-9);
        o.meter_fuel(t0 + Duration::from_millis(12_500));
        assert!((o.fuel_used_l - 0.0025).abs() < 1e-9);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_view_skips_gauges_and_keeps_catalogue_order() {
        let mut d = ObdData::default();
        d.set_value("rpm", 800.0);
        d.set_value("fuel", 40.0);
        d.set_value("load", 20.0);
        d.set_value("baro", 101.0);
        d.set_value("battery", 12.6);
        let keys: Vec<&str> = metrics_view(&d).iter().map(|m| m.k).collect();
        assert_eq!(keys, vec!["load", "fuel", "baro"]);
        assert_eq!(d.rpm, 800.0);
        assert_eq!(d.fuel_level, 40.0);
        assert_eq!(d.battery_voltage, 12.6);
    }

    #[test]
    fn dtc_count_is_distinct_across_lists() {
        let mut d = ObdData::default();
        d.dtc_codes = vec!["P0420".into()];
        d.dtc_pending = vec!["P0420".into(), "P0171".into()];
        d.dtc_permanent = vec!["P0420".into()];
        d.recount_dtcs();
        assert_eq!(d.dtc_count, 2);
    }

    #[test]
    fn sentinel_clears_previous_value() {
        let mut d = ObdData::default();
        let def = pids::find(0x44).unwrap();
        let (ok, bad) = decode_outputs(def, &pids::mode01_data("41 44 80 00 \r\r", 0x44).unwrap());
        for (k, v) in ok {
            d.set_value(k, v);
        }
        assert!(bad.is_empty());
        assert_eq!(d.values.get("lambda_cmd"), Some(&1.0));
        // Overrun: λ FFFF. λ disappears, fuel_cut turns on.
        let (ok, bad) = decode_outputs(def, &pids::mode01_data("41 44 FF FF \r\r", 0x44).unwrap());
        assert_eq!(bad, vec!["lambda_cmd"]);
        for k in bad {
            d.clear_value(k);
        }
        for (k, v) in ok {
            d.set_value(k, v);
        }
        assert_eq!(d.values.get("lambda_cmd"), None);
        assert_eq!(d.values.get("fuel_cut"), Some(&1.0));
        assert!(metrics_view(&d).iter().all(|m| m.k != "lambda_cmd"));
    }

    #[test]
    fn reconnect_backoff_quick_then_slow() {
        assert_eq!(reconnect_delay(1), QUICK_RETRY);
        assert_eq!(reconnect_delay(QUICK_RETRIES), QUICK_RETRY);
        assert_eq!(reconnect_delay(QUICK_RETRIES + 1), SLOW_RETRY);
        // Raw log: a failed page costs 5.12 s, the next attempt usually works.
        // Old schedule: 5.12 + 1.0 (post-ATZ sleep) + 0.4 + 5.0 = 11.5 s between
        // attempts; now 5.12 + 1.0.
        assert!(reconnect_delay(1) + Duration::from_millis(5120) < Duration::from_millis(6500));
    }

    #[test]
    fn header_commands() {
        assert_eq!(Header::Ecm.command(), "ATSH7E0");
        assert_eq!(Header::Functional.command(), "ATSH7DF");
    }

    #[test]
    fn history_created_on_demand_and_capped() {
        let mut h = HashMap::new();
        for i in 0..(HISTORY_LEN + 5) {
            push_history(&mut h, "cat_b1s1", i as f64);
        }
        let buf = &h["cat_b1s1"];
        assert_eq!(buf.len(), HISTORY_LEN);
        assert_eq!(*buf.front().unwrap(), 5.0);
    }
    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    /// A scripted ELM327 at the other end of an in-memory pipe. It records
    /// every command and answers `reply(ecm_header, cmd)` plus the prompt;
    /// an `ATSH` answered "OK" switches its header.
    fn fake_elm(reply: fn(bool, &str) -> String) -> (tokio::io::DuplexStream, Log) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let log: Log = Arc::default();
        let seen = log.clone();
        tokio::spawn(async move {
            let mut ecm = false;
            let mut cmd = Vec::new();
            let mut b = [0u8; 1];
            while theirs.read(&mut b).await.unwrap_or(0) == 1 {
                if b[0] != b'\r' {
                    cmd.push(b[0]);
                    continue;
                }
                let c = String::from_utf8_lossy(&cmd).into_owned();
                cmd.clear();
                let r = reply(ecm, &c);
                if r == "OK" && c.starts_with("ATSH") {
                    ecm = c == "ATSH7E0";
                }
                seen.lock().unwrap().push(c);
                if theirs.write_all(format!("{r}\r\r>").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        (ours, log)
    }

    fn link(serial: tokio::io::DuplexStream, raw: &RawLog) -> Link<'_, tokio::io::DuplexStream> {
        Link {
            serial,
            raw,
            consecutive_errors: 0,
            undecoded: HashMap::new(),
            header: Some(Header::Functional),
            ecm_only: false,
            atsh_rejected: false,
        }
    }

    /// The Jaguar as seen in the raw log: ECM and TCM both answer 7DF, only
    /// the ECM answers 7E0.
    fn jaguar(ecm: bool, cmd: &str) -> String {
        match cmd {
            "ATSH7E0" | "ATSH7DF" => "OK".into(),
            "ATDPN" => "A6".into(),
            "0100" if ecm => "41 00 BE 3F A8 13 ".into(),
            "0100" => "SEARCHING...\r41 00 BE 3F A8 13 \r41 00 98 18 80 13 ".into(),
            "0120" if ecm => "41 20 80 00 00 00 ".into(),
            "0120" => "41 20 80 00 00 00 \r41 20 80 00 00 00 ".into(),
            "0111" if ecm => "41 11 24 ".into(),
            "0111" => "41 11 24 \r41 11 0D ".into(),
            "03" => "43 00 \r43 00 ".into(),
            "07" => "47 00 \r47 00 ".into(),
            "0A" => "4A 00 \r4A 00 ".into(),
            _ => "NO DATA".into(),
        }
    }

    #[tokio::test]
    async fn discovery_addresses_the_ecm_on_can_11bit() {
        let raw = RawLog::disabled();
        let (port, log) = fake_elm(jaguar);
        let mut l = link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let d = discover(&mut l, &data).await.unwrap().unwrap();
        assert!(l.ecm_only);
        assert_eq!(l.header, Some(Header::Ecm));
        assert!(d.supported.contains(&0x0C) && d.supported.contains(&0x11));
        let cmds = log.lock().unwrap().clone();
        // Protocol search on a functional request first, ECM after ATDPN.
        assert_eq!(&cmds[..6], &["0100", "0120", "ATDPN", "ATSH7E0", "0100", "0120"]);

        // Mode 01 now gets the ECM's single reply: no more TCM throttle.
        let h: Histories = Arc::default();
        assert!(poll_pid(&mut l, 0x11, &data, &h).await.unwrap());
        assert!((data.read().await.throttle - 0x24 as f64 * 100.0 / 255.0).abs() < 1e-9);

        // DTCs go functional (both ECUs), and the next mode 01 header switch
        // goes back to the ECM.
        dtc_round(&mut l, &data).await.unwrap();
        l.set_header(l.mode01_header()).await.unwrap();
        let cmds = log.lock().unwrap().clone();
        let tail: Vec<&str> = cmds[cmds.len() - 6..].iter().map(String::as_str).collect();
        assert_eq!(tail, ["0111", "ATSH7DF", "03", "07", "0A", "ATSH7E0"]);
        assert_eq!(data.read().await.dtc_count, 0);
    }

    #[tokio::test]
    async fn clone_without_atsh_stays_functional_and_stops_asking() {
        fn no_atsh(_: bool, cmd: &str) -> String {
            if cmd.starts_with("ATSH") { "?".into() } else { jaguar(false, cmd) }
        }
        let raw = RawLog::disabled();
        let (port, log) = fake_elm(no_atsh);
        let mut l = link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        discover(&mut l, &data).await.unwrap().unwrap();
        assert!(!l.ecm_only);
        for _ in 0..3 {
            l.set_header(l.mode01_header()).await.unwrap();
            dtc_round(&mut l, &data).await.unwrap();
        }
        let atsh = log.lock().unwrap().iter().filter(|c| c.starts_with("ATSH")).count();
        assert_eq!(atsh, 1, "a rejected ATSH is not retried every cycle");
    }

    #[tokio::test]
    async fn ecm_silent_on_7e0_falls_back_to_functional() {
        fn deaf_ecm(ecm: bool, cmd: &str) -> String {
            if ecm && cmd.starts_with("01") { "NO DATA".into() } else { jaguar(ecm, cmd) }
        }
        let raw = RawLog::disabled();
        let (port, log) = fake_elm(deaf_ecm);
        let mut l = link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let d = discover(&mut l, &data).await.unwrap().unwrap();
        assert!(!l.ecm_only);
        assert_eq!(l.header, Some(Header::Functional));
        assert!(d.supported.contains(&0x0C), "the functional bitmaps are kept");
        assert!(log.lock().unwrap().iter().any(|c| c == "ATSH7DF"));
    }

    /// Header and receive filter the scripted adapter is set to.
    #[derive(Clone, Default)]
    struct Addr {
        header: String,
        cra: String,
    }

    type AddrLog = Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// A scripted ELM327 that tracks `ATSH` / `ATCRA` (applied when it
    /// answers OK; `ATCRA` alone clears the filter) and logs every command
    /// with the header it was sent under.
    fn fake_elm_addr(reply: fn(&Addr, &str) -> String) -> (tokio::io::DuplexStream, AddrLog) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let log: AddrLog = Arc::default();
        let seen = log.clone();
        tokio::spawn(async move {
            let mut addr = Addr { header: "7E0".into(), cra: String::new() };
            let mut cmd = Vec::new();
            let mut b = [0u8; 1];
            while theirs.read(&mut b).await.unwrap_or(0) == 1 {
                if b[0] != b'\r' {
                    cmd.push(b[0]);
                    continue;
                }
                let c = String::from_utf8_lossy(&cmd).into_owned();
                cmd.clear();
                let r = reply(&addr, &c);
                seen.lock().unwrap().push((addr.header.clone(), c.clone()));
                if r == "OK" {
                    if let Some(h) = c.strip_prefix("ATSH") {
                        addr.header = h.into();
                    } else if let Some(f) = c.strip_prefix("ATCRA") {
                        addr.cra = f.into();
                    }
                }
                if theirs.write_all(format!("{r}\r\r>").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        (ours, log)
    }

    /// The TPMS module per OBDb: answers 751 requests on 759, which the
    /// adapter only passes with the receive filter set. 2078 (RR) is refused.
    fn tpms_car(a: &Addr, cmd: &str) -> String {
        if cmd.starts_with("AT") {
            return "OK".into();
        }
        if a.header != "751" {
            return "NO DATA".into();
        }
        if a.cra != "759" {
            return "NO DATA".into(); // reply on 759 filtered out
        }
        match cmd {
            "222076" => "62 20 76 00 92 ".into(),
            "222077" => "62 20 77 00 98 ".into(),
            "222078" => "7F 22 31 ".into(),
            "222079" => "62 20 79 00 A0 ".into(),
            _ => "NO DATA".into(),
        }
    }

    fn ecm_link(serial: tokio::io::DuplexStream, raw: &RawLog) -> Link<'_, tokio::io::DuplexStream> {
        let mut l = link(serial, raw);
        l.header = Some(Header::Ecm);
        l.ecm_only = true;
        l
    }

    #[tokio::test]
    async fn tpms_round_sets_header_and_filter_then_restores_the_ecm() {
        let raw = RawLog::disabled();
        let (port, log) = fake_elm_addr(tpms_car);
        let mut l = ecm_link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let h: Histories = Arc::default();
        let mut t = TpmsState { enabled: true, ..Default::default() };
        tpms_round(&mut l, &mut t, &data, &h).await.unwrap();

        let cmds: Vec<String> = log.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(cmds, ["ATSH751", "ATCRA759", "222076", "222077", "222078", "222079", "ATCRA", "ATSH7E0"]);
        // Only reads were sent while addressed to the TPMS module.
        for (hdr, c) in log.lock().unwrap().iter() {
            if hdr == "751" {
                assert!(c.starts_with("AT") || c.starts_with("22"), "{c} sent to 751");
            }
        }
        assert_eq!(l.header, Some(Header::Ecm), "mode 01 addressing restored");
        let d = data.read().await;
        assert!((d.values["tyre_fl"] - 2.00458).abs() < 1e-9);
        assert!((d.values["tyre_fr"] - 2.08696).abs() < 1e-9);
        assert!((d.values["tyre_rl"] - 0xA0 as f64 * 0.01373).abs() < 1e-9);
        assert!(!d.values.contains_key("tyre_rr"));
        drop(d);
        assert!(t.unsupported.contains(&0x2078));
        assert!(t.stopped.is_none());

        // The refused DID is not asked again.
        tpms_round(&mut l, &mut t, &data, &h).await.unwrap();
        let n2078 = log.lock().unwrap().iter().filter(|(_, c)| c == "222078").count();
        assert_eq!(n2078, 1);
    }

    #[tokio::test]
    async fn tpms_without_atcra_stops_and_still_restores() {
        fn no_cra(a: &Addr, cmd: &str) -> String {
            if cmd.starts_with("ATCRA") { "?".into() } else { tpms_car(a, cmd) }
        }
        let raw = RawLog::disabled();
        let (port, log) = fake_elm_addr(no_cra);
        let mut l = ecm_link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let h: Histories = Arc::default();
        let mut t = TpmsState { enabled: true, ..Default::default() };
        tpms_round(&mut l, &mut t, &data, &h).await.unwrap();
        let cmds: Vec<String> = log.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(cmds, ["ATSH751", "ATCRA759", "ATCRA", "ATAR", "ATSH7E0"]);
        assert!(t.stopped.is_some());
        assert_eq!(l.header, Some(Header::Ecm));
    }

    #[tokio::test]
    async fn tpms_silent_module_gives_up_after_three_rounds() {
        fn absent(a: &Addr, cmd: &str) -> String {
            if cmd.starts_with("22") { "NO DATA".into() } else { tpms_car(a, cmd) }
        }
        let raw = RawLog::disabled();
        let (port, _log) = fake_elm_addr(absent);
        let mut l = ecm_link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let h: Histories = Arc::default();
        let mut t = TpmsState { enabled: true, ..Default::default() };
        for _ in 0..TPMS_MAX_SILENT_ROUNDS {
            assert!(t.stopped.is_none());
            tpms_round(&mut l, &mut t, &data, &h).await.unwrap();
        }
        assert!(t.stopped.is_some());
        assert_eq!(l.header, Some(Header::Ecm));
    }

    /// The ECM's mode 06 and 09 08 as a 4-cylinder petrol would answer.
    fn monitor_car(_: &Addr, cmd: &str) -> String {
        let rec = |mid: u8, tid: u8, uas: u8, v: u16, lo: u16, hi: u16| {
            let mut r = vec![mid, tid, uas];
            for x in [v, lo, hi] {
                r.extend_from_slice(&x.to_be_bytes());
            }
            r
        };
        let reply = |records: Vec<Vec<u8>>| {
            let mut b = vec![0x46];
            for r in records {
                b.extend(r);
            }
            elm::iso_tp(&b)
        };
        match cmd {
            "0600" => "46 00 C0 00 00 01 ".into(), // 01 02, next
            "0620" => "46 20 80 00 00 01 ".into(), // 21, next
            "0640" | "0660" | "0680" => format!("46 {} 00 00 00 01 ", &cmd[2..]),
            "06A0" => "46 A0 F8 00 00 00 ".into(), // A1-A5
            "0601" => reply(vec![rec(0x01, 0x01, 0x0A, 3600, 0, 0xFFFF), rec(0x01, 0x05, 0x10, 70, 0, 120)]),
            "0602" => reply(vec![rec(0x02, 0x07, 0x0A, 400, 0, 1000)]),
            // Catalyst B1 above its limit.
            "0621" => reply(vec![rec(0x21, 0x80, 0x01, 0x0123, 0, 0x0100)]),
            "06A1" => reply(vec![rec(0xA1, 0x0B, 0x24, 4, 0, 0xFFFF), rec(0xA1, 0x0C, 0x24, 14, 0, 0xFFFF)]),
            "06A2" | "06A3" | "06A5" => {
                let mid = u8::from_str_radix(&cmd[2..], 16).unwrap();
                reply(vec![rec(mid, 0x0B, 0x24, 0, 0, 0xFFFF), rec(mid, 0x0C, 0x24, 0, 0, 0xFFFF)])
            }
            "06A4" => reply(vec![rec(0xA4, 0x0B, 0x24, 4, 0, 0xFFFF), rec(0xA4, 0x0C, 0x24, 14, 0, 0xFFFF)]),
            "0908" => {
                let mut b = vec![0x49, 0x08, 0x14];
                for c in [12u16, 300, 9, 12, 0, 0, 10, 12, 0, 0, 12, 12, 0, 0, 2, 12, 9, 12, 0, 0] {
                    b.extend_from_slice(&c.to_be_bytes());
                }
                elm::iso_tp(&b)
            }
            _ => "NO DATA".into(),
        }
    }

    #[tokio::test]
    async fn drive_reads_mode06_then_0908_one_request_per_cycle() {
        let raw = RawLog::disabled();
        let (port, log) = fake_elm_addr(monitor_car);
        let mut l = ecm_link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData { protocol: Some("6 ISO 15765-4 CAN".into()), ..Default::default() }));
        let h: Histories = Arc::default();
        let disc = Discovery { supported: vec![], info_types: vec![0x02, 0x08] };
        let mut drive = DriveState::default();

        // Engine just started: nothing is asked yet.
        drive_step(&mut l, &mut drive, true, Some(&disc), &data, &h).await.unwrap();
        assert!(log.lock().unwrap().is_empty());

        // Two minutes later: one request per cycle until both are done.
        drive.running_since = Some(Instant::now() - DRIVE_READS_AFTER);
        for cycle in 0..40 {
            let before = log.lock().unwrap().len();
            drive_step(&mut l, &mut drive, true, Some(&disc), &data, &h).await.unwrap();
            assert!(log.lock().unwrap().len() - before <= 1, "cycle {cycle}");
        }
        let cmds: Vec<String> = log.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
        assert_eq!(
            cmds,
            ["0600", "0620", "0640", "0660", "0680", "06A0", "0601", "0602", "0621", "06A1", "06A2", "06A3", "06A4", "06A5", "0908"]
        );
        assert!(matches!(drive.mode06, Mode06Job::Done) && drive.ipt_done);

        let d = data.read().await;
        let m = &d.health.misfire;
        assert_eq!(m.len(), 5, "A1 (all) + 4 cylinders");
        assert_eq!((m[0].cyl, m[0].this_drive, m[0].avg10), (0, Some(14.0), Some(4.0)));
        assert_eq!((m[3].cyl, m[3].this_drive), (3, Some(14.0)));
        assert_eq!(m[1].this_drive, Some(0.0));
        assert_eq!(d.health.mode06_count, 14, "2 + 1 + 1 + 5 misfire MIDs × 2");
        assert_eq!(d.health.out_of_limits.len(), 1);
        assert_eq!(d.health.out_of_limits[0].monitor, "Catalyst B1");
        let ipt = d.health.ipt.as_ref().unwrap();
        assert_eq!(ipt.ignition_cycles, 300);
        assert_eq!(ipt.monitors[6].name, "EVAP");
        assert_eq!((ipt.monitors[6].comp, ipt.monitors[6].cond), (2, 12));
        // The broadcast view leaves the full test list out.
        let v = d.health.broadcast_view();
        assert!(v.get("tests").is_none() && v["misfire"].is_array());
        drop(d);

        // Same drive: nothing more. After a long engine-off gap, a new drive
        // re-arms both reads.
        drive_step(&mut l, &mut drive, true, Some(&disc), &data, &h).await.unwrap();
        assert_eq!(log.lock().unwrap().len(), 15);
        drive.last_running = Some(Instant::now() - DRIVE_GAP - Duration::from_secs(1));
        drive_step(&mut l, &mut drive, true, Some(&disc), &data, &h).await.unwrap();
        assert!(matches!(drive.mode06, Mode06Job::Idle) && !drive.ipt_done);
    }

    #[tokio::test]
    async fn mode06_unsupported_is_done_after_one_request() {
        let raw = RawLog::disabled();
        let (port, log) = fake_elm_addr(|_, _| "NO DATA".into());
        let mut l = ecm_link(port, &raw);
        let data = Arc::new(RwLock::new(ObdData::default()));
        let mut job = Mode06Job::Idle;
        mode06_step(&mut l, &mut job, &data).await.unwrap();
        assert!(matches!(job, Mode06Job::Done));
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(data.read().await.health.mode06_at.is_none());
    }

    #[test]
    fn metrics_view_carries_enumerated_text() {
        let mut d = ObdData::default();
        d.set_value("fuel_sys1", 2.0);
        d.set_value("boost_cmd", 125.0);
        let v = metrics_view(&d);
        let fs = v.iter().find(|m| m.k == "fuel_sys1").unwrap();
        assert_eq!(fs.t, Some("Closed loop"));
        let json = serde_json::to_value(&v).unwrap();
        assert!(json.as_array().unwrap().iter().any(|m| m["k"] == "boost_cmd" && m.get("t").is_none()));
    }

    #[tokio::test]
    async fn eof_without_prompt_is_an_error() {
        // A failed Bluetooth page: the port reads EOF after 5.12 s with no
        // bytes. That used to be an empty "success".
        use tokio::io::AsyncReadExt;
        let (mut ours, mut theirs) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let mut b = [0u8; 8];
            let _ = theirs.read(&mut b).await;
            drop(theirs);
        });
        let (raw, res) = send_cmd(&mut ours, "ATZ").await;
        assert!(raw.is_empty());
        assert!(res.is_err());
    }
}
