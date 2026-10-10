//! Android persona probe (`RASCAR_PERSONA=android`): learn what the head unit
//! asks of an Android phone.
//!
//! The mySPIN launcher (`com.bosch.myspin.launcherapp_jaguar` 2.10.0) reaches
//! the car over Android Open Accessory: the car, as USB host, switches the phone
//! into accessory mode and identifies itself with strings such as `Jaguar` or
//! `Denso` / `JLR-PCV-Gen2.1-Jaguar` (the app's accessory_filter.xml). With an
//! iPhone identity the car never offers an app protocol (2026-09-25), so this is
//! the remaining route to InControl Apps.
//!
//! Flow, all logged:
//!   1. Enumerate as a generic Android phone (Google VID) with one vendor
//!      interface (two bulk endpoints), full + high speed.
//!   2. Answer the AOA requests on ep0: GET_PROTOCOL (0x33) -> 2; log every
//!      SEND_STRING (0x34) with its index, AOA2 audio (0x3A) and HID (0x36-0x39)
//!      requests; START (0x35) ends the phase.
//!   3. Re-enumerate as the accessory (18D1:2D00), wait for ENABLE, then dump
//!      everything the car sends on bulk OUT, decoding mySPIN packet headers
//!      (`u8 type, u32, u32 len`, little endian). Optionally send the mySPIN
//!      Ping the real phone opens with.
//!
//! Self-contained: its own gadget name and FunctionFS mount, never touching the
//! CarPlay gadget, which is removed first.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

const GADGET: &str = "/sys/kernel/config/usb_gadget/rascar-aoa";
const FFS_INSTANCE: &str = "aoa";
const FFS_MOUNT: &str = "/dev/ffs-rascar-aoa";

const GOOGLE_VID: &str = "0x18d1";
/// Pixel-style MTP product id: what an Android phone shows before AOA.
const PHONE_PID: &str = "0x4ee1";
/// AOA accessory product id (accessory only, no ADB, no audio).
const ACCESSORY_PID: &str = "0x2d00";

const AOA_GET_PROTOCOL: u8 = 0x33;
const AOA_SEND_STRING: u8 = 0x34;
const AOA_START: u8 = 0x35;
const AOA_REGISTER_HID: u8 = 0x36;
const AOA_UNREGISTER_HID: u8 = 0x37;
const AOA_SET_HID_REPORT_DESC: u8 = 0x38;
const AOA_SEND_HID_EVENT: u8 = 0x39;
const AOA_SET_AUDIO_MODE: u8 = 0x3a;
/// AOA protocol version we claim (2 = AOA2: audio + HID).
const AOA_VERSION: u16 = 2;

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
/// HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP | CONFIG0_SETUP, as in `ffs.rs`:
/// AOA requests target the DEVICE recipient and may come before SET_CONFIGURATION.
const DESCRIPTOR_FLAGS: u32 = 1 | 2 | 64 | 128;

const FUNCTIONFS_BIND: u8 = 0;
const FUNCTIONFS_UNBIND: u8 = 1;
const FUNCTIONFS_ENABLE: u8 = 2;
const FUNCTIONFS_DISABLE: u8 = 3;
const FUNCTIONFS_SETUP: u8 = 4;
const FUNCTIONFS_SUSPEND: u8 = 5;
const FUNCTIONFS_RESUME: u8 = 6;
const EVENT_SIZE: usize = 12;

/// mySPIN Ping the real phone sends first and every 500 ms (type 0x80, empty
/// u32, length 4, payload 0) (from a capture of a real phone).
const MYSPIN_PING: [u8; 13] = [0x80, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0];

/// Answer the car's opening handshake (default on; `RASCAR_AOA_HANDSHAKE=0` to
/// only log). Byte layouts were taken from captures of a real phone's handshake.
fn handshake_enabled() -> bool {
    !matches!(std::env::var("RASCAR_AOA_HANDSHAKE").ok().as_deref().map(str::trim), Some("0") | Some("off"))
}

/// One mySPIN packet: 9-byte header `u8 type | u32 0 | u32 len` + payload.
pub fn myspin_packet(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(9 + payload.len());
    p.push(ty);
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    p.extend_from_slice(payload);
    p
}

/// mySPIN String: `01 <u16 len> bytes` (1 = UTF-8).
fn myspin_string(s: &str) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&(s.len() as u16).to_le_bytes());
    v.extend_from_slice(s.as_bytes());
    v
}

/// ServerInit (type 14) for protocol 1.0.x / 1.1.x / 1.2.x, as the Jaguar
/// launcher 2.10.0 builds it (it rejects anything above 1.2.x).
pub fn server_init(minor: u16) -> Vec<u8> {
    let mut p = myspin_string("mySPIN Android Server");
    p.extend_from_slice(&[1, 1, 1]);
    match minor {
        0 => p.extend_from_slice(&1u32.to_le_bytes()),
        1 => {
            p.extend_from_slice(&2u32.to_le_bytes());
            p.extend_from_slice(&myspin_string("Android"));
        }
        _ => {
            p.extend_from_slice(&6u32.to_le_bytes());
            p.extend_from_slice(&myspin_string("Android"));
            p.extend_from_slice(&[1, 0, 0]);
        }
    }
    myspin_packet(14, &p)
}

/// Replies to one car packet during the opening handshake; `minor` remembers
/// the version the car announced.
pub fn handshake_replies(ty: u8, payload: &[u8], minor: &mut Option<u16>) -> Vec<Vec<u8>> {
    match ty {
        // ClientProtocolVersion: u16 major, u16 minor, u16 build.
        1 if payload.len() >= 6 => {
            let major = u16::from_le_bytes([payload[0], payload[1]]);
            let min = u16::from_le_bytes([payload[2], payload[3]]);
            let build = u16::from_le_bytes([payload[4], payload[5]]);
            info!("**** mySPIN ClientProtocolVersion **** {major}.{min}.{build}");
            if major != 1 || min > 2 {
                warn!("mySPIN: version {major}.{min} is above what the Jaguar launcher accepts (1.2.x) — acking anyway");
            }
            *minor = Some(min.min(2));
            vec![myspin_packet(2, &[1])]
        }
        // ClientInit: ack, ServerInit, and (1.2.x) ServerInitDone.
        11 => {
            info!("**** mySPIN ClientInit **** {} B: {}", payload.len(), crate::debug::hex_prefix(payload, 256));
            let min = minor.unwrap_or(2);
            let mut out = vec![myspin_packet(12, &[1]), server_init(min)];
            if min >= 2 {
                out.push(myspin_packet(18, &[1]));
            }
            out
        }
        17 => {
            info!("**** mySPIN ClientInitDone **** handshake complete — runtime starts (frame requests next)");
            Vec::new()
        }
        25 => {
            info!("mySPIN FramebufferUpdateRequest: {} (not answered by the probe)", crate::debug::hex_prefix(payload, 16));
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Whether this process runs the Android probe instead of CarPlay.
pub fn enabled() -> bool {
    persona_is_android(std::env::var("RASCAR_PERSONA").ok().as_deref())
}

fn persona_is_android(raw: Option<&str>) -> bool {
    matches!(raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("android"))
}

/// Whether to send the mySPIN Ping once the accessory pipe is open (default on;
/// `RASCAR_AOA_PING=0` to only listen).
fn ping_enabled() -> bool {
    !matches!(std::env::var("RASCAR_AOA_PING").ok().as_deref().map(str::trim), Some("0") | Some("off"))
}

/// AOA string index -> name (Android `UsbAccessory` fields).
fn string_name(index: u16) -> &'static str {
    match index {
        0 => "manufacturer",
        1 => "model",
        2 => "description",
        3 => "version",
        4 => "uri",
        5 => "serial",
        _ => "?",
    }
}

fn aoa_request_name(request: u8) -> &'static str {
    match request {
        AOA_GET_PROTOCOL => "GET_PROTOCOL",
        AOA_SEND_STRING => "SEND_STRING",
        AOA_START => "START",
        AOA_REGISTER_HID => "REGISTER_HID",
        AOA_UNREGISTER_HID => "UNREGISTER_HID",
        AOA_SET_HID_REPORT_DESC => "SET_HID_REPORT_DESC",
        AOA_SEND_HID_EVENT => "SEND_HID_EVENT",
        AOA_SET_AUDIO_MODE => "SET_AUDIO_MODE",
        _ => "-",
    }
}

fn endpoint(addr: u8, hs: bool) -> [u8; 7] {
    let mp = if hs { 512u16 } else { 64 }.to_le_bytes();
    [7, 0x05, addr, 0x02, mp[0], mp[1], 0]
}

/// One vendor interface (0xFF/0xFF/0x00, iInterface 1) with bulk OUT 0x01 then
/// bulk IN 0x81 — so FunctionFS exposes ep1 = OUT (car -> us), ep2 = IN.
pub fn descriptors() -> Vec<u8> {
    let iface: [u8; 9] = [9, 0x04, 0, 0, 2, 0xFF, 0xFF, 0x00, 1];
    let set = |hs: bool| [iface.to_vec(), endpoint(0x01, hs).to_vec(), endpoint(0x81, hs).to_vec()].concat();
    let (fs_set, hs_set) = (set(false), set(true));
    let total = 20 + fs_set.len() + hs_set.len();
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    b.extend_from_slice(&(total as u32).to_le_bytes());
    b.extend_from_slice(&DESCRIPTOR_FLAGS.to_le_bytes());
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&fs_set);
    b.extend_from_slice(&hs_set);
    b
}

/// FunctionFS strings: en-US, string 1 = "MTP" (the interface name Android uses).
pub fn strings() -> Vec<u8> {
    let s1: &[u8] = b"MTP\0";
    let total = 16 + 2 + s1.len();
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    b.extend_from_slice(&(total as u32).to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&0x0409u16.to_le_bytes());
    b.extend_from_slice(s1);
    b
}

/// Best-effort decode of mySPIN packet headers in one bulk chunk.
pub fn describe_myspin(chunk: &[u8]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 9 <= chunk.len() && out.len() < 8 {
        let ty = chunk[i];
        let field = u32::from_le_bytes(chunk[i + 1..i + 5].try_into().unwrap());
        let len = u32::from_le_bytes(chunk[i + 5..i + 9].try_into().unwrap()) as usize;
        out.push(format!("type={ty}(0x{ty:02x}) u32={field} len={len}"));
        if len > chunk.len() {
            break;
        }
        i += 9 + len;
    }
    if out.is_empty() { "no complete header".into() } else { out.join(" | ") }
}

fn write_attr(path: impl AsRef<Path>, value: &str) -> Result<()> {
    let path = path.as_ref();
    fs::write(path, value).with_context(|| format!("writing {value:?} to {}", path.display()))?;
    debug!("wrote {} → {value}", path.display());
    Ok(())
}

fn udc_name() -> Result<String> {
    fs::read_dir("/sys/class/udc")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .context("no UDC under /sys/class/udc")
}

fn serial() -> String {
    std::env::var("RASCAR_AOA_SERIAL").unwrap_or_else(|_| "RASCAR0AOA0PROBE1".to_string())
}

fn create_gadget(pid: &str) -> Result<()> {
    for module in ["libcomposite", "usb_f_fs"] {
        let _ = Command::new("modprobe").arg(module).status();
    }
    let g = PathBuf::from(GADGET);
    if g.exists() {
        warn!("aoa: stale gadget found, removing it first");
        remove_gadget();
    }
    fs::create_dir_all(&g).context("creating the AOA gadget in configfs")?;
    write_attr(g.join("idVendor"), GOOGLE_VID)?;
    write_attr(g.join("idProduct"), pid)?;
    write_attr(g.join("bcdDevice"), "0x0440")?;
    write_attr(g.join("bcdUSB"), "0x0200")?;
    write_attr(g.join("bMaxPacketSize0"), "0x40")?;
    let strings = g.join("strings/0x409");
    fs::create_dir_all(&strings)?;
    write_attr(strings.join("manufacturer"), "Google")?;
    write_attr(strings.join("product"), "Pixel 7")?;
    write_attr(strings.join("serialnumber"), &serial())?;
    let config = g.join("configs/c.1");
    fs::create_dir_all(config.join("strings/0x409"))?;
    write_attr(config.join("strings/0x409/configuration"), "mtp")?;
    write_attr(config.join("MaxPower"), "500")?;
    let func = g.join(format!("functions/ffs.{FFS_INSTANCE}"));
    fs::create_dir_all(&func)?;
    symlink(&func, config.join(format!("ffs.{FFS_INSTANCE}")))?;
    fs::create_dir_all(FFS_MOUNT)?;
    let status = Command::new("mount").args(["-t", "functionfs", FFS_INSTANCE, FFS_MOUNT]).status()?;
    if !status.success() {
        bail!("mounting FunctionFS {FFS_INSTANCE} at {FFS_MOUNT} failed ({status})");
    }
    info!("aoa: Android gadget created (Google {GOOGLE_VID}:{pid} 'Pixel 7', serial {})", serial());
    Ok(())
}

/// Tear the probe gadget down (best effort, reverse order of creation).
pub fn remove_gadget() {
    let g = PathBuf::from(GADGET);
    let _ = fs::write(g.join("UDC"), "");
    let _ = Command::new("umount").arg(FFS_MOUNT).status();
    let _ = fs::remove_file(g.join(format!("configs/c.1/ffs.{FFS_INSTANCE}")));
    let _ = fs::remove_dir(g.join("configs/c.1/strings/0x409"));
    let _ = fs::remove_dir(g.join("configs/c.1"));
    let _ = fs::remove_dir(g.join(format!("functions/ffs.{FFS_INSTANCE}")));
    let _ = fs::remove_dir(g.join("strings/0x409"));
    let _ = fs::remove_dir(&g);
}

fn ep0_read(fd: i32, len: usize) -> nix::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let n = nix::unistd::read(fd, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn ep0_write(file: &File, data: &[u8]) -> Result<()> {
    let n = nix::unistd::write(file, data).map_err(std::io::Error::from)?;
    if n != data.len() {
        bail!("short ep0 write: {n} of {}", data.len());
    }
    Ok(())
}

/// What happened on ep0.
#[derive(Debug, PartialEq, Eq)]
enum Ep0Outcome {
    Start,
    Enabled,
    Other,
}

/// Read and answer one ep0 event; `phase` only labels the log lines.
fn handle_ep0(ep0: &File, phase: &str, started: Instant) -> Result<Ep0Outcome> {
    let fd = ep0.as_raw_fd();
    let mut ev = [0u8; EVENT_SIZE];
    let n = match nix::unistd::read(fd, &mut ev) {
        Ok(n) => n,
        Err(nix::errno::Errno::EINTR) => return Ok(Ep0Outcome::Other),
        Err(e) => {
            warn!("aoa[{phase}]: ep0 read error {e} — continuing");
            std::thread::sleep(Duration::from_millis(50));
            return Ok(Ep0Outcome::Other);
        }
    };
    if n != EVENT_SIZE {
        return Ok(Ep0Outcome::Other);
    }
    let t = started.elapsed().as_millis();
    match ev[8] {
        FUNCTIONFS_BIND => info!("aoa[{phase}]: BIND (t+{t} ms)"),
        FUNCTIONFS_UNBIND => info!("aoa[{phase}]: UNBIND (t+{t} ms)"),
        FUNCTIONFS_ENABLE => {
            info!("aoa[{phase}]: ENABLE — car selected our configuration (t+{t} ms)");
            return Ok(Ep0Outcome::Enabled);
        }
        FUNCTIONFS_DISABLE => info!("aoa[{phase}]: DISABLE (t+{t} ms)"),
        FUNCTIONFS_SUSPEND => info!("aoa[{phase}]: SUSPEND (t+{t} ms)"),
        FUNCTIONFS_RESUME => info!("aoa[{phase}]: RESUME (t+{t} ms)"),
        FUNCTIONFS_SETUP => {
            let (rt, req) = (ev[0], ev[1]);
            let value = u16::from_le_bytes([ev[2], ev[3]]);
            let index = u16::from_le_bytes([ev[4], ev[5]]);
            let length = u16::from_le_bytes([ev[6], ev[7]]) as usize;
            let is_in = rt & 0x80 != 0;
            let is_vendor = rt & 0x60 == 0x40;
            info!(
                "aoa[{phase}]: SETUP (t+{t} ms) bmRequestType=0x{rt:02x} bRequest=0x{req:02x} ({}) wValue=0x{value:04x} wIndex=0x{index:04x} wLength={length}",
                if is_vendor { aoa_request_name(req) } else { "non-vendor" }
            );
            if is_vendor && is_in && req == AOA_GET_PROTOCOL {
                ep0_write(ep0, &AOA_VERSION.to_le_bytes()[..length.min(2)])?;
                info!("**** AOA GET_PROTOCOL **** answered version {AOA_VERSION} — the car speaks Android Open Accessory");
            } else if is_in {
                // Unknown IN: stall (a read on an IN setup halts ep0).
                match ep0_read(fd, 0) {
                    Err(nix::errno::Errno::EL2HLT) => info!("aoa[{phase}]: stalled unknown IN request"),
                    other => warn!("aoa[{phase}]: stalling IN request gave {other:?}"),
                }
            } else {
                let data = ep0_read(fd, length).map_err(std::io::Error::from)?;
                if is_vendor && req == AOA_SEND_STRING {
                    let text = String::from_utf8_lossy(&data).trim_end_matches('\0').to_string();
                    info!("**** AOA STRING **** {} (index {index}) = {text:?}", string_name(index));
                } else if is_vendor && req == AOA_SET_AUDIO_MODE {
                    info!("**** AOA SET_AUDIO_MODE **** wValue={value} (0 = none, 1 = 44.1 kHz 16-bit stereo PCM)");
                } else if !data.is_empty() {
                    info!("aoa[{phase}]: OUT data {}", crate::debug::hex_prefix(&data, 64));
                }
                if is_vendor && req == AOA_START {
                    info!("**** AOA START **** car asks us to re-enumerate as its accessory");
                    return Ok(Ep0Outcome::Start);
                }
            }
        }
        other => debug!("aoa[{phase}]: event type {other}"),
    }
    Ok(Ep0Outcome::Other)
}

/// Queue into the single writer thread: nothing ever blocks behind a write.
type SharedIn = std::sync::mpsc::Sender<Vec<u8>>;

fn send(ep_in: &SharedIn, packet: &[u8], _started: Instant) {
    if ep_in.send(packet.to_vec()).is_err() {
        warn!("aoa[bulk]: writer thread gone — packet dropped");
    }
}

/// Owns bulk IN; writes queued packets in order and logs them (pings sparsely).
fn writer_loop(mut ep_in: File, rx: std::sync::mpsc::Receiver<Vec<u8>>, started: Instant) {
    let mut pings = 0u64;
    while let Ok(packet) = rx.recv() {
        let is_ping = packet == MYSPIN_PING;
        if let Err(e) = ep_in.write_all(&packet) {
            warn!("aoa[bulk]: write failed ({}): {e} — writer stopping", describe_myspin(&packet));
            return;
        }
        if is_ping {
            pings += 1;
            if pings <= 3 || pings % 20 == 0 {
                info!("aoa[bulk] -> car mySPIN Ping #{pings} (t+{} ms)", started.elapsed().as_millis());
            }
        } else {
            info!(
                "aoa[bulk] -> car {} B (t+{} ms): {} || {}",
                packet.len(),
                started.elapsed().as_millis(),
                describe_myspin(&packet),
                crate::debug::hex_prefix(&packet, 64)
            );
        }
    }
}

/// Dump the accessory bulk OUT pipe (car -> us) until it fails, answering the
/// opening handshake when enabled.
fn dump_bulk(mut ep_out: File, ep_in: SharedIn, started: Instant) {
    let mut buf = vec![0u8; 16384];
    let mut total = 0usize;
    let mut pending: Vec<u8> = Vec::new();
    let mut minor: Option<u16> = None;
    let answer = handshake_enabled();
    loop {
        match ep_out.read(&mut buf) {
            Ok(0) => {
                info!("aoa[bulk]: end of stream after {total} bytes");
                return;
            }
            Ok(n) => {
                total += n;
                info!(
                    "aoa[bulk] <- car {n} B (t+{} ms, total {total}): {} || {}",
                    started.elapsed().as_millis(),
                    describe_myspin(&buf[..n]),
                    crate::debug::hex_prefix(&buf[..n], 160)
                );
                pending.extend_from_slice(&buf[..n]);
                while pending.len() >= 9 {
                    let len = u32::from_le_bytes(pending[5..9].try_into().unwrap()) as usize;
                    if pending.len() < 9 + len {
                        break;
                    }
                    let ty = pending[0];
                    let payload: Vec<u8> = pending[9..9 + len].to_vec();
                    pending.drain(..9 + len);
                    if answer {
                        for reply in handshake_replies(ty, &payload, &mut minor) {
                            send(&ep_in, &reply, started);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("aoa[bulk]: read failed after {total} bytes: {e}");
                return;
            }
        }
    }
}

/// Queue the mySPIN Ping every 500 ms, as the phone does.
fn ping_loop(ep_in: SharedIn) {
    while ep_in.send(MYSPIN_PING.to_vec()).is_ok() {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Open ep0, write descriptors + strings, bind to the UDC (retrying a busy UDC).
fn open_and_bind() -> Result<File> {
    let ep0 = OpenOptions::new().read(true).write(true).open(format!("{FFS_MOUNT}/ep0"))?;
    ep0_write(&ep0, &descriptors()).context("writing FunctionFS descriptors")?;
    ep0_write(&ep0, &strings()).context("writing FunctionFS strings")?;
    let udc = udc_name()?;
    let mut last = None;
    for attempt in 1..=10 {
        match fs::write(Path::new(GADGET).join("UDC"), &udc) {
            Ok(()) => {
                info!("aoa: bound to UDC {udc} (attempt {attempt})");
                return Ok(ep0);
            }
            Err(e) => {
                warn!("aoa: binding UDC {udc} failed (attempt {attempt}): {e}");
                last = Some(e);
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("bind failed")))
        .context("binding the AOA gadget to the UDC")
}

fn run_blocking() -> Result<()> {
    let started = Instant::now();
    // Leave nothing of the CarPlay gadget behind: one UDC, one gadget.
    if let Err(e) = super::gadget::remove() {
        warn!("aoa: removing the CarPlay gadget failed: {e:#}");
    }
    create_gadget(PHONE_PID)?;
    let ep0 = open_and_bind()?;
    info!("aoa: Android phone visible to the car; waiting for AOA requests");

    // Phase A: AOA handshake.
    loop {
        if handle_ep0(&ep0, "phone", started)? == Ep0Outcome::Start {
            break;
        }
    }

    // Phase B: re-enumerate as the accessory. Rebinding the same gadget after an
    // unbind returned EBUSY in the car, so the gadget is torn
    // down and rebuilt with the accessory product id — the path the first bind
    // already proves.
    drop(ep0);
    remove_gadget();
    std::thread::sleep(Duration::from_millis(100));
    create_gadget(ACCESSORY_PID)?;
    let ep0 = open_and_bind()?;
    info!("aoa: re-enumerating as accessory {GOOGLE_VID}:{ACCESSORY_PID}");
    loop {
        if handle_ep0(&ep0, "accessory", started)? == Ep0Outcome::Enabled {
            break;
        }
    }

    // Phase C: open the pipes, dump the car's first packets, keep ep0 serviced.
    let ep_out = OpenOptions::new().read(true).open(format!("{FFS_MOUNT}/ep1")).context("opening bulk OUT ep1")?;
    let ep_in = OpenOptions::new().write(true).open(format!("{FFS_MOUNT}/ep2")).context("opening bulk IN ep2")?;
    info!("aoa: accessory pipe open (ep1 OUT car->us, ep2 IN us->car)");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new().name("aoa-bulk-tx".into()).spawn(move || writer_loop(ep_in, rx, started))?;
    let rx_tx = tx.clone();
    std::thread::Builder::new().name("aoa-bulk-rx".into()).spawn(move || dump_bulk(ep_out, rx_tx, started))?;
    if ping_enabled() {
        std::thread::Builder::new().name("aoa-ping".into()).spawn(move || ping_loop(tx))?;
    } else {
        info!("aoa: no Ping (RASCAR_AOA_PING=0)");
    }
    loop {
        handle_ep0(&ep0, "accessory", started)?;
    }
}

/// Run the probe for the rest of the process (until power-off), then clean up.
pub async fn run() -> Result<()> {
    info!("=== RasCar Android persona probe (RASCAR_PERSONA=android) — CarPlay disabled for this run ===");
    let result = tokio::task::spawn_blocking(run_blocking).await.context("joining the AOA probe")?;
    if let Err(e) = &result {
        warn!("aoa probe ended: {e:#}");
    }
    remove_gadget();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_is_opt_in() {
        assert!(!persona_is_android(None));
        assert!(!persona_is_android(Some("iphone")));
        assert!(persona_is_android(Some("android")));
        assert!(persona_is_android(Some(" Android ")));
    }

    #[test]
    fn descriptor_bundle_is_well_formed() {
        let d = descriptors();
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize, d.len());
        // FS and HS sets: interface + 2 endpoints each, 23 bytes each.
        assert_eq!(d.len(), 20 + 23 * 2);
        // HS bulk wMaxPacketSize 512 on the last endpoint.
        assert_eq!(&d[d.len() - 3..d.len() - 1], &512u16.to_le_bytes());
        // Endpoint order: OUT 0x01 first so ep1 = OUT.
        assert_eq!(d[20 + 9 + 2], 0x01);
        assert_eq!(d[20 + 16 + 2], 0x81);
    }

    #[test]
    fn strings_bundle_length_matches() {
        let s = strings();
        assert_eq!(u32::from_le_bytes(s[4..8].try_into().unwrap()) as usize, s.len());
    }

    #[test]
    fn myspin_headers_decode() {
        // ClientProtocolVersion-like packet: type 1, u32 0, len 3, payload.
        let pkt = [1, 0, 0, 0, 0, 3, 0, 0, 0, 1, 2, 0];
        assert_eq!(describe_myspin(&pkt), "type=1(0x01) u32=0 len=3");
        assert_eq!(describe_myspin(&MYSPIN_PING), "type=128(0x80) u32=0 len=4");
        assert_eq!(describe_myspin(&[1, 2]), "no complete header");
    }

    #[test]
    fn server_init_lengths_match_the_launcher() {
        // Header length field: 0x1F / 0x29 / 0x2C for 1.0 / 1.1 / 1.2.
        for (minor, len) in [(0u16, 0x1Fu32), (1, 0x29), (2, 0x2C)] {
            let p = server_init(minor);
            assert_eq!(p[0], 14);
            assert_eq!(u32::from_le_bytes(p[5..9].try_into().unwrap()), len, "1.{minor}");
            assert_eq!(p.len(), 9 + len as usize);
        }
        assert_eq!(&server_init(2)[9..12], &[1, 0x15, 0]);
    }

    #[test]
    fn handshake_follows_the_documented_sequence() {
        let mut minor = None;
        let r = handshake_replies(1, &[1, 0, 2, 0, 7, 0], &mut minor);
        assert_eq!(r, vec![vec![2, 0, 0, 0, 0, 1, 0, 0, 0, 1]]);
        assert_eq!(minor, Some(2));
        let r = handshake_replies(11, &[0u8; 20], &mut minor);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], vec![0x0C, 0, 0, 0, 0, 1, 0, 0, 0, 1]);
        assert_eq!(r[2], vec![0x12, 0, 0, 0, 0, 1, 0, 0, 0, 1]);
        // 1.1.x: no ServerInitDone.
        let mut minor = None;
        handshake_replies(1, &[1, 0, 1, 0, 0, 0], &mut minor);
        assert_eq!(handshake_replies(11, &[], &mut minor).len(), 2);
        assert!(handshake_replies(17, &[1], &mut minor).is_empty());
    }

    #[test]
    fn string_indices_follow_android() {
        assert_eq!(string_name(0), "manufacturer");
        assert_eq!(string_name(1), "model");
        assert_eq!(string_name(5), "serial");
    }
}
