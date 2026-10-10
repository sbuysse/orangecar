//! 4G status of the USB modem: the SIMCom SIM7600E-H (Qualcomm, USB 1e0e:9001,
//! the one fitted now) or the older D-Link DWM-222W (ASR PXA1802, 1286:4e31).
//!
//! * SIM7600: USB interfaces 0 diag, 1 NMEA, 2 and 3 AT, 4 audio, 5 QMI. Only
//!   interface 2 is ever opened here: interface 3 (AT) and 4 (PCM) belong to
//!   the phone service (services/phone), so the two never interleave on one
//!   port. Data goes over the qmi_wwan netdev, dialed
//!   by `rascar-4g-qmi` (it owns /dev/cdc-wdm0; we never touch QMI), which
//!   writes one status line to `/run/rascar/4g.status`
//!   (`connected <dev> <ip>` / `dialing` / `dial failed` / `no modem`).
//!   Signal comes from `AT+CPSI?` (RSRP/RSRQ/RSSI printed in tenths of dB).
//! * DWM-222W: an ECM netdev (`enx…`) and `option` ttys, one of which answers
//!   AT (setup's `rascar-4g-radio` finds it the same way: probe every tty of
//!   the stick and keep the one that answers). Its ttyUSB0 is a debug-log port
//!   that prints bare "OK" lines, so a port only counts when it returns BOTH a
//!   whole `OK` line AND the expected `+XXX:` answer. Signal from `AT+CESQ`.
//!
//! SAFETY RULE: only read-only queries are ever sent (`AT+CEREG?`, `AT+CESQ`,
//! `AT+CSQ`, `AT+COPS?`, `AT+CPSI?`). Never AT+CFUN, AT+CSIM or any write command — a raw
//! AT+CSIM once dropped the SIM. The port is opened, queried and closed each
//! cycle, and a cycle is skipped while `rascar-4g-radio` (or ModemManager) is
//! running so we never interleave with its conversation.

use serde::Serialize;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// A supported modem.
#[derive(Debug, PartialEq)]
pub struct Model {
    vid: &'static str,
    pid: &'static str,
    /// USB interface numbers that may be opened as AT ports (`None` = probe all).
    at_ifaces: Option<&'static [u8]>,
    /// Signal from `AT+CPSI?` (SIMCom) rather than `AT+CESQ`.
    cpsi: bool,
    /// Link state written by the dialing service, if the model has one.
    link_status: Option<&'static str>,
}

const MODELS: &[Model] = &[
    Model { vid: "1e0e", pid: "9001", at_ifaces: Some(&[2]), cpsi: true, link_status: Some("/run/rascar/4g.status") },
    Model { vid: "1286", pid: "4e31", at_ifaces: None, cpsi: false, link_status: None },
];
/// AT poll period (the brief asks for 10-15 s).
const AT_PERIOD: Duration = Duration::from_secs(12);
/// Internet reachability check period.
const NET_PERIOD: Duration = Duration::from_secs(30);
/// Cheap reachability probe: a TCP handshake, nothing sent.
const PROBE_ADDR: &str = "1.1.1.1:443";

/// What the dashboard shows next to the Wi-Fi bars.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct CellStatus {
    /// The stick is plugged in (USB 1286:4e31 enumerated).
    pub present: bool,
    /// Registered on a network (`+CEREG` stat 1 = home or 5 = roaming).
    pub registered: bool,
    pub operator: Option<String>,
    /// "4G", "3G", "2G", "5G" from the `+COPS` access technology.
    pub tech: Option<String>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<f32>,
    pub rssi_dbm: Option<i32>,
    /// 0-5; 0 when not registered.
    pub bars: u8,
    /// A TCP handshake through the stick's interface succeeded.
    pub internet: bool,
    /// The stick's network interface (enx…), if found.
    pub iface: Option<String>,
    /// An AT port answered this cycle (false = stick present but no answer yet).
    pub at_ok: bool,
    /// The dialing service's state (`connected`, `dialing`, `dial failed`, `no modem`).
    pub link: Option<String>,
}

pub struct CellService {
    pub status: Arc<RwLock<CellStatus>>,
}

impl CellService {
    pub fn new() -> Self {
        Self { status: Arc::new(RwLock::new(CellStatus::default())) }
    }

    pub async fn start(&self) {
        let status = self.status.clone();
        tokio::spawn(async move {
            let mut at_port: Option<PathBuf> = None;
            let mut last_net = std::time::Instant::now() - NET_PERIOD;
            // A new/moved default route re-runs the probe at once instead of
            // leaving "NO INTERNET" up for up to NET_PERIOD.
            let mut net = super::net_changes();
            let mut logged_present: Option<bool> = None;
            loop {
                let model = find_model(Path::new("/sys/bus/usb/devices"));
                let present = model.is_some();
                if logged_present != Some(present) {
                    info!("4G modem {}", model.map(|m| format!("present ({}:{})", m.vid, m.pid)).unwrap_or_else(|| "not plugged in".into()));
                    logged_present = Some(present);
                }
                let Some(model) = model else {
                    at_port = None;
                    *status.write().await = CellStatus::default();
                    tokio::time::sleep(AT_PERIOD).await;
                    continue;
                };

                let mut next = status.read().await.clone();
                next.present = true;
                next.iface = stick_iface(model, Path::new("/sys/class/net"));
                let link = model.link_status.and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_link(&t));
                next.link = link.as_ref().map(|(state, _)| state.clone());

                if at_port_busy() {
                    debug!("4G: AT port in use by rascar-4g-radio/ModemManager, skipping this cycle");
                } else {
                    let cached = at_port.clone();
                    let polled = tokio::task::spawn_blocking(move || poll_at(model, cached)).await.ok().flatten();
                    match polled {
                        Some((port, radio)) => {
                            at_port = Some(port);
                            next.at_ok = true;
                            next.registered = radio.registered;
                            next.operator = radio.operator;
                            next.tech = radio.tech;
                            next.rsrp_dbm = radio.rsrp_dbm;
                            next.rsrq_db = radio.rsrq_db;
                            next.rssi_dbm = radio.rssi_dbm;
                            next.bars = bars(radio.registered, radio.rsrp_dbm, radio.rssi_dbm);
                        }
                        None => {
                            at_port = None;
                            next.at_ok = false;
                            next.registered = false;
                            next.bars = 0;
                        }
                    }
                }

                if last_net.elapsed() >= NET_PERIOD || net.has_changed().unwrap_or(false) {
                    net.borrow_and_update();
                    last_net = std::time::Instant::now();
                    // With a dialing service, only a "connected" link can carry
                    // traffic, and its own device name is authoritative.
                    let iface = match &link {
                        Some((state, dev)) if state == "connected" => dev.clone().or(next.iface.clone()),
                        Some(_) => None,
                        None => next.iface.clone(),
                    };
                    next.internet = match iface.as_deref() {
                        Some(iface) => internet_via(iface).await,
                        None => false,
                    };
                }

                *status.write().await = next;
                tokio::time::sleep(AT_PERIOD).await;
            }
        });
    }
}

// ── Signal → bars ─────────────────────────────────────────────

/// 0-5 bars. RSRP (LTE) is the reference: ≥-85 → 5, ≥-95 → 4, ≥-105 → 3,
/// ≥-115 → 2, else 1. Without an RSRP (3G/2G cell) the CSQ RSSI stands in with
/// the usual RSSI steps. Not registered → 0 whatever the radio hears.
pub fn bars(registered: bool, rsrp_dbm: Option<i32>, rssi_dbm: Option<i32>) -> u8 {
    if !registered {
        return 0;
    }
    if let Some(r) = rsrp_dbm {
        return match r {
            r if r >= -85 => 5,
            r if r >= -95 => 4,
            r if r >= -105 => 3,
            r if r >= -115 => 2,
            _ => 1,
        };
    }
    match rssi_dbm {
        Some(r) if r >= -65 => 5,
        Some(r) if r >= -75 => 4,
        Some(r) if r >= -85 => 3,
        Some(r) if r >= -95 => 2,
        _ => 1,
    }
}

// ── AT reply parsing (pure, unit-tested) ──────────────────────

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Radio {
    pub registered: bool,
    pub operator: Option<String>,
    pub tech: Option<String>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<f32>,
    pub rssi_dbm: Option<i32>,
}

/// A reply is complete and successful only with a whole `OK` line (the debug
/// port's log text contains "...IsOK" mid-line, and it also prints bare OKs,
/// hence the answer line check in [`answer_fields`]).
pub fn reply_ok(reply: &str) -> bool {
    reply.lines().any(|l| l.trim() == "OK")
}

fn reply_done(reply: &str) -> bool {
    reply.lines().any(|l| {
        let l = l.trim();
        l == "OK" || l == "ERROR" || l.starts_with("+CME ERROR") || l.starts_with("+CMS ERROR")
    })
}

/// The comma-separated fields of the `+<cmd>:` answer line, when the reply is
/// a successful one. `cmd` is e.g. "CEREG".
pub fn answer_fields(reply: &str, cmd: &str) -> Option<Vec<String>> {
    if !reply_ok(reply) {
        return None;
    }
    let prefix = format!("+{cmd}:");
    let line = reply.lines().map(str::trim).find(|l| l.starts_with(&prefix))?;
    Some(split_fields(line[prefix.len()..].trim()))
}

/// Split on commas outside double quotes; quotes are removed.
pub(crate) fn split_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// `+CEREG: <n>,<stat>[,…]` → registered when stat is 1 (home) or 5 (roaming).
/// The unsolicited form `+CEREG: <stat>[,<tac>,…]` (n omitted) is not what
/// `AT+CEREG?` answers, so the second field is always the stat.
pub fn parse_cereg(reply: &str) -> Option<bool> {
    let f = answer_fields(reply, "CEREG")?;
    let stat: u8 = f.get(1)?.parse().ok()?;
    Some(stat == 1 || stat == 5)
}

/// `+CESQ: <rxlev>,<ber>,<rscp>,<ecno>,<rsrq>,<rsrp>` (3GPP 27.007 §8.69).
/// rsrq 0..34 → -19.5 dB + 0.5 per step (0 = below -19.5); rsrp 0..97 →
/// -140 dBm + 1 per step (0 = below -140). 255 = unknown.
pub fn parse_cesq(reply: &str) -> Option<(Option<i32>, Option<f32>)> {
    let f = answer_fields(reply, "CESQ")?;
    if f.len() < 6 {
        return None;
    }
    let rsrq = f[4].parse::<u16>().ok().filter(|v| *v <= 34).map(|v| -20.0 + v as f32 * 0.5);
    let rsrp = f[5].parse::<i32>().ok().filter(|v| (0..=97).contains(v)).map(|v| v - 141);
    Some((rsrp, rsrq))
}

/// SIMCom `+CPSI: <mode>,<op>,<MCC-MNC>,<TAC>,<cell>,<PCI>,<band>,<earfcn>,
/// <dlbw>,<ulbw>,<RSRQ>,<RSRP>,<RSSI>,<RSSNR>` (LTE; SIM7600 AT manual). RSRQ,
/// RSRP and RSSI are in tenths of dB: -1098 = -109.8 dBm. Other modes (WCDMA,
/// GSM, NO SERVICE) carry no RSRP → (None, None).
pub fn parse_cpsi(reply: &str) -> Option<(Option<i32>, Option<f32>)> {
    let f = answer_fields(reply, "CPSI")?;
    if !f.first()?.eq_ignore_ascii_case("LTE") || f.len() < 12 {
        return Some((None, None));
    }
    let tenths = |s: &str| s.parse::<i32>().ok().filter(|v| *v != 0);
    let rsrq = tenths(&f[10]).map(|v| v as f32 / 10.0);
    let rsrp = tenths(&f[11]).map(|v| (v as f32 / 10.0).round() as i32).filter(|v| (-160..=-30).contains(v));
    Some((rsrp, rsrq))
}

/// `+CSQ: <rssi>,<ber>`: rssi 0..31 → -113 dBm + 2 per step; 99 = unknown.
pub fn parse_csq(reply: &str) -> Option<Option<i32>> {
    let f = answer_fields(reply, "CSQ")?;
    let v: i32 = f.first()?.parse().ok()?;
    Some(if (0..=31).contains(&v) { Some(-113 + 2 * v) } else { None })
}

/// The SIM7600 reports some names twice ("Example Mobile Example Mobile").
fn undouble(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let half = words.len() / 2;
    if words.len() % 2 == 0 && half > 0 && words[..half] == words[half..] {
        words[..half].join(" ")
    } else {
        name.to_string()
    }
}

/// `+COPS: <mode>[,<format>,"<oper>"[,<AcT>]]` → (operator, technology).
pub fn parse_cops(reply: &str) -> Option<(Option<String>, Option<String>)> {
    let f = answer_fields(reply, "COPS")?;
    let oper = f.get(2).filter(|o| !o.is_empty()).map(|o| undouble(o));
    let tech = f.get(3).and_then(|a| a.parse::<u8>().ok()).map(|act| {
        match act {
            0 | 1 | 3 => "2G",
            2 | 4 | 5 | 6 => "3G",
            7 | 8 | 9 | 10 => "4G",
            11..=13 => "5G",
            _ => "?",
        }
        .to_string()
    });
    Some((oper, tech))
}

// ── Stick discovery (sysfs) ───────────────────────────────────

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn is_stick(model: &Model, usb_dev: &Path) -> bool {
    read_trim(&usb_dev.join("idVendor")).as_deref() == Some(model.vid)
        && read_trim(&usb_dev.join("idProduct")).as_deref() == Some(model.pid)
}

/// The first supported modem that is plugged in.
fn find_model(usb_devices: &Path) -> Option<&'static Model> {
    let devs: Vec<PathBuf> = std::fs::read_dir(usb_devices).ok()?.flatten().map(|e| e.path()).collect();
    MODELS.iter().find(|m| devs.iter().any(|d| is_stick(m, d)))
}

/// `/run/rascar/4g.status` → (state, device): "connected wwu1u4i5 100.64.0.2"
/// → ("connected", Some("wwu1u4i5")); "dial failed" → ("dial failed", None).
pub fn parse_link(text: &str) -> (String, Option<String>) {
    let line = text.lines().next().unwrap_or("").trim();
    match line.strip_prefix("connected") {
        Some(rest) => ("connected".into(), rest.split_whitespace().next().map(str::to_string)),
        None => (line.to_string(), None),
    }
}

/// The stick's netdev: `/sys/class/net/<if>/device` is the USB interface,
/// whose parent is the USB device carrying idVendor/idProduct.
fn stick_iface(model: &Model, class_net: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(class_net)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let dev = std::fs::canonicalize(e.path().join("device")).ok()?;
            is_stick(model, dev.parent()?).then(|| e.file_name().to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

/// ttyUSB nodes of the stick that may be opened, sorted (same walk as
/// rascar-4g-radio: /sys/class/tty/ttyUSBn/device/.. is the USB interface,
/// ../.. the USB device). Ports outside `at_ifaces` (diag, NMEA, audio on the
/// SIM7600) are never opened.
fn stick_ttys(model: &Model) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir("/sys/class/tty")
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("ttyUSB"))
                .filter_map(|e| {
                    let usb = std::fs::canonicalize(e.path().join("device/../..")).ok()?;
                    if !is_stick(model, &usb) {
                        return None;
                    }
                    if let Some(allowed) = model.at_ifaces {
                        let n = read_trim(&e.path().join("device/../bInterfaceNumber"))
                            .and_then(|n| u8::from_str_radix(&n, 16).ok())?;
                        if !allowed.contains(&n) {
                            return None;
                        }
                    }
                    Some(PathBuf::from("/dev").join(e.file_name()))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Another process is talking to the modem: rascar-4g-radio (a oneshot run by
/// udev when the stick appears) or ModemManager. Their cmdlines are readable
/// by every user, unlike their fd tables.
pub(crate) fn at_port_busy() -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .any(|cmd| {
            let cmd = String::from_utf8_lossy(&cmd);
            cmd.contains("rascar-4g-radio") || cmd.contains("ModemManager")
        })
}

// ── AT conversation (blocking; run in spawn_blocking) ─────────

struct Tty(std::fs::File);

impl Tty {
    fn open(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)?;
        // Raw mode: no local echo (it would feed the modem's own answers back
        // to it), no CR/LF translation, no line buffering.
        let fd = f.as_raw_fd();
        // SAFETY: fd is a valid open descriptor for the lifetime of `f`;
        // `t` is fully initialised by tcgetattr before use.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut t);
            t.c_cflag |= libc::CLOCAL | libc::CREAD;
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self(f))
    }

    /// Send one query and collect the reply until a final result line or the
    /// deadline. Only the four read-only queries of this module go through here.
    fn query(&mut self, cmd: &str, wait: Duration) -> String {
        use std::io::{Read, Write};
        // Hard allow-list, also in release builds: nothing else may reach the modem.
        if !matches!(cmd, "AT+CEREG?" | "AT+CESQ" | "AT+CSQ" | "AT+COPS?" | "AT+CPSI?") {
            return String::new();
        }
        // SAFETY: valid fd; discards unread input so a stale line can't be taken as this answer.
        unsafe { libc::tcflush(self.0.as_raw_fd(), libc::TCIFLUSH) };
        if self.0.write_all(format!("{cmd}\r").as_bytes()).is_err() {
            return String::new();
        }
        let end = std::time::Instant::now() + wait;
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        while std::time::Instant::now() < end {
            match self.0.read(&mut buf) {
                Ok(n) if n > 0 => {
                    out.extend_from_slice(&buf[..n]);
                    if reply_done(&String::from_utf8_lossy(&out)) {
                        break;
                    }
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

/// One poll: find (or reuse) the AT port and read the radio state. The port
/// is closed when this returns.
fn poll_at(model: &Model, cached: Option<PathBuf>) -> Option<(PathBuf, Radio)> {
    let mut candidates = stick_ttys(model);
    if let Some(c) = cached {
        candidates.retain(|p| p != &c);
        candidates.insert(0, c);
    }
    for path in candidates {
        let Ok(mut tty) = Tty::open(&path) else { continue };
        // CEREG doubles as the probe: the debug port never answers "+CEREG:".
        let Some(registered) = parse_cereg(&tty.query("AT+CEREG?", Duration::from_secs(2))) else {
            continue;
        };
        let mut radio = Radio { registered, ..Radio::default() };
        let signal = if model.cpsi {
            parse_cpsi(&tty.query("AT+CPSI?", Duration::from_secs(2)))
        } else {
            parse_cesq(&tty.query("AT+CESQ", Duration::from_secs(2)))
        };
        if let Some((rsrp, rsrq)) = signal {
            radio.rsrp_dbm = rsrp;
            radio.rsrq_db = rsrq;
        }
        if let Some(rssi) = parse_csq(&tty.query("AT+CSQ", Duration::from_secs(2))) {
            radio.rssi_dbm = rssi;
        }
        if let Some((oper, tech)) = parse_cops(&tty.query("AT+COPS?", Duration::from_secs(3))) {
            radio.operator = oper;
            radio.tech = tech;
        }
        return Some((path, radio));
    }
    None
}

// ── Internet through the stick ────────────────────────────────

/// TCP handshake to 1.1.1.1:443 bound to the stick's interface. Linux ≥ 5.7
/// lets an unprivileged socket SO_BINDTODEVICE; if the kernel refuses anyway,
/// fall back to "the default route goes through the stick + a plain connect".
async fn internet_via(iface: &str) -> bool {
    let addr: std::net::SocketAddr = PROBE_ADDR.parse().expect("static address");
    let Ok(sock) = tokio::net::TcpSocket::new_v4() else { return false };
    let bound = sock.bind_device(Some(iface.as_bytes())).is_ok();
    if !bound {
        if default_route_iface().as_deref() != Some(iface) {
            return false;
        }
        debug!("4G: SO_BINDTODEVICE refused, checking through the default route");
    }
    matches!(tokio::time::timeout(Duration::from_secs(5), sock.connect(addr)).await, Ok(Ok(_)))
}

/// Interface of the lowest-metric IPv4 default route (/proc/net/route).
fn default_route_iface() -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/route").ok()?;
    default_route_from_table(&table)
}

fn default_route_from_table(table: &str) -> Option<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask …
            if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" {
                return None;
            }
            let flags = u32::from_str_radix(f[3], 16).ok()?;
            if flags & 0x1 == 0 {
                return None; // RTF_UP
            }
            Some((f[6].parse::<u32>().ok()?, f[0].to_string()))
        })
        .min()
        .map(|(_, iface)| iface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cereg_registered_home_and_roaming_only() {
        assert_eq!(parse_cereg("\r\n+CEREG: 0,1\r\n\r\nOK\r\n"), Some(true));
        assert_eq!(parse_cereg("AT+CEREG?\r\r\n+CEREG: 2,5,\"1A2B\",\"01A2B3C4\",7\r\n\r\nOK\r\n"), Some(true));
        assert_eq!(parse_cereg("\r\n+CEREG: 0,2\r\n\r\nOK\r\n"), Some(false)); // searching
        assert_eq!(parse_cereg("\r\n+CEREG: 0,3\r\n\r\nOK\r\n"), Some(false)); // denied
    }

    #[test]
    fn a_bare_ok_from_the_debug_port_is_not_an_answer() {
        assert_eq!(parse_cereg("\r\nOK\r\n"), None);
        assert_eq!(parse_cereg("[DBG] radio IsOK 12\r\nOK\r\n"), None);
        // The answer without the final OK is incomplete.
        assert_eq!(parse_cereg("\r\n+CEREG: 0,1\r\n"), None);
        assert_eq!(parse_cereg("\r\nERROR\r\n"), None);
        // "IsOK" mid-line does not count as the OK line.
        assert!(!reply_ok("state IsOK\r\n"));
    }

    #[test]
    fn cesq_converts_rsrp_and_rsrq() {
        let (rsrp, rsrq) = parse_cesq("\r\n+CESQ: 99,99,255,255,20,45\r\n\r\nOK\r\n").unwrap();
        assert_eq!(rsrp, Some(-96));
        assert_eq!(rsrq, Some(-10.0));
        let (rsrp, rsrq) = parse_cesq("\r\n+CESQ: 99,99,255,255,255,255\r\n\r\nOK\r\n").unwrap();
        assert_eq!((rsrp, rsrq), (None, None));
        let (rsrp, _) = parse_cesq("+CESQ: 99,99,255,255,34,97\r\nOK\r\n").unwrap();
        assert_eq!(rsrp, Some(-44));
        assert_eq!(parse_cesq("+CESQ: 99,99\r\nOK\r\n"), None);
    }

    #[test]
    fn cpsi_reads_sim7600_tenths() {
        let r = "\r\n+CPSI: LTE,Online,001-01,0x0001,1,1,EUTRAN-BAND3,1300,2,2,-119,-1098,-839,9\r\n\r\nOK\r\n";
        let (rsrp, rsrq) = parse_cpsi(r).unwrap();
        assert_eq!(rsrp, Some(-110));
        assert_eq!(rsrq, Some(-11.9));
        assert_eq!(bars(true, rsrp, None), 2);
        assert_eq!(parse_cpsi("+CPSI: NO SERVICE,Online\r\nOK\r\n"), Some((None, None)));
        assert_eq!(parse_cpsi("+CPSI: WCDMA,Online,001-01,0x1,1,UMTS 2100,10788,0,0,-8,-90,-60\r\nOK\r\n"), Some((None, None)));
        assert_eq!(parse_cpsi("\r\nOK\r\n"), None);
    }

    #[test]
    fn link_status_lines() {
        assert_eq!(parse_link("connected wwu1u4i5 100.64.0.2\n"), ("connected".into(), Some("wwu1u4i5".into())));
        assert_eq!(parse_link("dial failed\n"), ("dial failed".into(), None));
        assert_eq!(parse_link("dialing"), ("dialing".into(), None));
        assert_eq!(parse_link(""), ("".into(), None));
    }

    #[test]
    fn sim7600_only_opens_its_at_interfaces() {
        let sim = MODELS.iter().find(|m| m.vid == "1e0e").unwrap();
        // Interface 3 is the phone's voice AT port (services/phone/port.rs).
        assert_eq!(sim.at_ifaces, Some(&[2u8][..]));
        assert!(!sim.at_ifaces.unwrap().contains(&crate::services::phone::port::AT_IFACE));
        assert!(sim.cpsi);
    }

    #[test]
    fn csq_converts_rssi_and_unknown() {
        assert_eq!(parse_csq("\r\n+CSQ: 18,99\r\n\r\nOK\r\n"), Some(Some(-77)));
        assert_eq!(parse_csq("\r\n+CSQ: 99,99\r\n\r\nOK\r\n"), Some(None));
        assert_eq!(parse_csq("\r\n+CSQ: 0,0\r\n\r\nOK\r\n"), Some(Some(-113)));
    }

    #[test]
    fn cops_operator_and_technology() {
        assert_eq!(
            parse_cops("\r\n+COPS: 0,0,\"Operator A\",7\r\n\r\nOK\r\n"),
            Some((Some("Operator A".into()), Some("4G".into())))
        );
        // Commas inside the quoted operator name stay in the name.
        assert_eq!(
            parse_cops("+COPS: 0,0,\"Operator B, Partner\",2\r\nOK\r\n"),
            Some((Some("Operator B, Partner".into()), Some("3G".into())))
        );
        assert_eq!(
            parse_cops("+COPS: 0,0,\"Example Mobile Example Mobile\",7\r\nOK\r\n"),
            Some((Some("Example Mobile".into()), Some("4G".into())))
        );
        assert_eq!(undouble("Orange Orange B"), "Orange Orange B");
        // Not registered: only the mode.
        assert_eq!(parse_cops("\r\n+COPS: 0\r\n\r\nOK\r\n"), Some((None, None)));
    }

    #[test]
    fn bars_follow_rsrp_steps() {
        assert_eq!(bars(true, Some(-70), None), 5);
        assert_eq!(bars(true, Some(-85), None), 5);
        assert_eq!(bars(true, Some(-86), None), 4);
        assert_eq!(bars(true, Some(-95), None), 4);
        assert_eq!(bars(true, Some(-105), None), 3);
        assert_eq!(bars(true, Some(-115), None), 2);
        assert_eq!(bars(true, Some(-116), None), 1);
        assert_eq!(bars(true, Some(-140), None), 1);
    }

    #[test]
    fn bars_zero_when_not_registered_and_rssi_fallback() {
        assert_eq!(bars(false, Some(-70), Some(-60)), 0);
        assert_eq!(bars(true, None, Some(-77)), 3);
        assert_eq!(bars(true, None, None), 1);
    }

    #[test]
    fn default_route_picks_lowest_metric_up_route() {
        let t = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                 enP3p49s0\t00000000\t0158A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
                 enx001e101f0000\t00000000\t0100A8C0\t0003\t0\t0\t50\t00000000\t0\t0\t0\n\
                 enP3p49s0\t0058A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_route_from_table(t).as_deref(), Some("enx001e101f0000"));
        assert_eq!(default_route_from_table("Iface\tDestination\n"), None);
    }
}
