//! USB FunctionFS implementation for the CarPlay transmitter (Phase 1).
//!
//! We enumerate to the car as a real-iPhone-shaped Apple device and answer the
//! control (ep0) requests the head unit issues until it sends the **CarPlay USB
//! role-switch vendor request** (`40 51 wValue=1`).
//!
//! # Hardware-proven layout (car sample #2, 2026-09-15)
//!
//! Everything here mirrors an earlier Python FunctionFS probe (four
//! iPhone-shaped configurations, padded string descriptors, full-speed HID
//! report descriptor), which made the car:
//!   1. enumerate us at HIGH speed, read all 4 configurations and select
//!      configuration 2 ("iPod USB Interface");
//!   2. send `21 0a` SET_IDLE and `81 06 wValue=0x2200` (HID report descriptor);
//!   3. send `C0 53` (answered `01 00 00 00`), `40 40 wValue=500 wIndex=1000`,
//!      the report descriptor again, then `40 51 wValue=1`.
//!
//! A single-config, full-speed-only gadget never received 0x53/0x51.
//!
//! # FunctionFS mechanics
//!
//! - One FunctionFS instance per configuration (`ffs.c1`..`ffs.c4`, mounted at
//!   [`mount_point`]); each instance holds exactly one configuration's function.
//! - Every ep0 must be kept open by this process for the whole session: closing
//!   any one of them unbinds the whole gadget.  [`Ep0Set`] owns them together.
//! - FS and HS descriptor sets have the same endpoint count and the same
//!   endpoint address order; HS bulk endpoints use wMaxPacketSize 512.
//! - FunctionFS rejects class-specific audio descriptors (type 0x24), so the
//!   iPod configuration uses plain interface descriptors only.
//!
//! Flow (see main.rs):
//!   1. `gadget::create()` builds configfs and mounts every instance
//!   2. [`Ep0Set::open_all`] opens each ep0 and writes descriptors + strings
//!   3. `gadget::bind()` binds to the UDC → the car enumerates us
//!   4. [`capture_control_requests`] polls every ep0, answers each request and
//!      returns on the role switch, closing all ep0 fds on the way out.

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::fs::File;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Mount point prefix, distinct from the AA bridge's `/dev/ffs-rascar` so both
/// gadgets can coexist.  Instance N is mounted at `<prefix>-cN`.
pub const FFS_MOUNT_PREFIX: &str = "/dev/ffs-carplay";

/// Mount point of configuration `config` (1-based).
pub fn mount_point(config: u8) -> String {
    format!("{FFS_MOUNT_PREFIX}-c{config}")
}

/// FunctionFS instance name of configuration `config` (1-based).  It is both the
/// configfs function suffix (`functions/ffs.<name>`) and the mount source, which
/// is how the kernel pairs a mount with its configfs function.
pub fn instance_name(config: u8) -> String {
    format!("c{config}")
}

// ── FunctionFS binary format constants ───────────────────────────────────────

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
/// Deliver vendor/class requests targeting ANY recipient (not just our
/// interface).  Required so the CarPlay vendor requests, which the car directs
/// at the DEVICE recipient, are forwarded to us on ep0.
const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 64;
/// Also forward setup requests while the device is unconfigured (config 0).
const FUNCTIONFS_CONFIG0_SETUP: u32 = 128;

const DESCRIPTOR_FLAGS: u32 =
    FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC | FUNCTIONFS_ALL_CTRL_RECIP | FUNCTIONFS_CONFIG0_SETUP;

// ── ep0 event types ───────────────────────────────────────────────────────────

const FUNCTIONFS_BIND: u8 = 0;
const FUNCTIONFS_UNBIND: u8 = 1;
const FUNCTIONFS_ENABLE: u8 = 2;
const FUNCTIONFS_DISABLE: u8 = 3;
const FUNCTIONFS_SETUP: u8 = 4;
const FUNCTIONFS_SUSPEND: u8 = 5;
const FUNCTIONFS_RESUME: u8 = 6;

/// Size of one `struct usb_functionfs_event`.
const EVENT_SIZE: usize = 12;

/// Name of an ep0 event type, for logging.
fn event_name(event_type: u8) -> &'static str {
    match event_type {
        FUNCTIONFS_BIND => "BIND",
        FUNCTIONFS_UNBIND => "UNBIND",
        FUNCTIONFS_ENABLE => "ENABLE",
        FUNCTIONFS_DISABLE => "DISABLE",
        FUNCTIONFS_SETUP => "SETUP",
        FUNCTIONFS_SUSPEND => "SUSPEND",
        FUNCTIONFS_RESUME => "RESUME",
        _ => "UNKNOWN",
    }
}

/// Recipient field (bmRequestType bits 4..0) of a control request.
fn recipient_name(bm_request_type: u8) -> &'static str {
    match bm_request_type & 0x1f {
        0 => "device",
        1 => "interface",
        2 => "endpoint",
        3 => "other",
        _ => "reserved",
    }
}

/// Chapter-9 standard request name (only meaningful for type=standard).
fn standard_request_name(b_request: u8) -> &'static str {
    match b_request {
        0x00 => "GET_STATUS",
        0x01 => "CLEAR_FEATURE",
        0x03 => "SET_FEATURE",
        0x05 => "SET_ADDRESS",
        0x06 => "GET_DESCRIPTOR",
        0x07 => "SET_DESCRIPTOR",
        0x08 => "GET_CONFIGURATION",
        0x09 => "SET_CONFIGURATION",
        0x0a => "GET_INTERFACE",
        0x0b => "SET_INTERFACE",
        0x0c => "SYNCH_FRAME",
        _ => "?",
    }
}

// ── CarPlay vendor requests ───────────────────────────────────────────────────
//
// Measured against the car (see module docs):
//   * `C0 53` (vendor IN)  → the car reads role-switch capability; a real iPhone
//     answers `01 00 00 00`.
//   * `40 51 wValue=1` (vendor OUT) → "please become USB host".
const CARPLAY_ROLE_SWITCH_REQ: u8 = 0x51;
const CARPLAY_ROLE_READ_REQ: u8 = 0x53;
const CARPLAY_ROLE_SWITCH_VALUE: u16 = 1;
const REQTYPE_VENDOR_OUT: u8 = 0x40; // vendor, host→device, device recipient

/// Vendor IN requests we answer, with the payload (truncated to wLength).
/// Any other IN request is stalled.
/// `C0 53` answer of an iPhone that cannot role-switch (CarPlay unavailable).
const NO_ROLE_SWITCH_CAPABILITY: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

/// Answer `C0 53` with "no role-switch capability" (opt in with
/// `RASCAR_HIDE_CARPLAY=1`). Test of 2026-09-24: the car only shows its
/// CarPlay/InControl chooser because we claim CarPlay; picking InControl there
/// yields `40 51 wValue=0` and then silence. A phone without CarPlay should leave
/// the car one option, iAP2 over HID, which is the InControl transport.
pub fn hide_carplay_capability() -> bool {
    hide_carplay_from(std::env::var("RASCAR_HIDE_CARPLAY").ok().as_deref())
}

fn hide_carplay_from(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1") | Some("on") | Some("true") | Some("yes"))
}

const VENDOR_IN_REPLIES: &[(u8, &[u8])] = &[
    // CarPlay role-switch capability read.
    (CARPLAY_ROLE_READ_REQ, &[0x01, 0x00, 0x00, 0x00]),
    // usbmuxd GET_MODE (INFERRED, not observed from the car).
    (0x45, &[0x01]),
];

// ── Descriptors (byte-identical to the probe's `descriptors(builders, False)`) ─

/// iAP-over-HID report descriptor, the full-speed variant shipped by
/// Read byte-for-byte off a **CarLinkit CPC200-CCPA in iPhone mode** on
/// 2026-09-21 (`05ac:12a8`, bcdDevice 10.01, 17-character serial) -- the
/// dongle that works in the Jaguar F-PACE. It declares `wDescriptorLength 208`.
///
/// This is the iAP2-over-HID transport used in USB *device* mode, before the
/// role switch: 12 Input reports (ids 0x01..0x0c, 5..767 B) and 9 Output reports
/// (ids 0x0d..0x15, 5..255 B), so a message picks the smallest report that fits.
///
/// We previously served ipod-gadget's 96-byte full-speed probe descriptor, which
/// offers only 4 Input / 5 Output reports capped at 63 B -- a >12x smaller
/// maximum than a real device. We never send HID reports ourselves (iAP2 runs
/// over bulk after the role switch), so this is descriptive only, but the head
/// unit does read it during bring-up.
pub const HID_REPORT: [u8; 208] = [
    0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x75, 0x08, 0x26, 0x80, 0x00, 0x15, 0x00,
    0x09, 0x01, 0x85, 0x01, 0x95, 0x05, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x02, 0x95,
    0x09, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x03, 0x95, 0x0d, 0x82, 0x02, 0x01, 0x09,
    0x01, 0x85, 0x04, 0x95, 0x11, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x05, 0x95, 0x19,
    0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x06, 0x95, 0x31, 0x82, 0x02, 0x01, 0x09, 0x01,
    0x85, 0x07, 0x95, 0x5f, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x08, 0x95, 0xc1, 0x82,
    0x02, 0x01, 0x09, 0x01, 0x85, 0x09, 0x96, 0x01, 0x01, 0x82, 0x02, 0x01, 0x09, 0x01,
    0x85, 0x0a, 0x96, 0x81, 0x01, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x0b, 0x96, 0x01,
    0x02, 0x82, 0x02, 0x01, 0x09, 0x01, 0x85, 0x0c, 0x96, 0xff, 0x02, 0x82, 0x02, 0x01,
    0x09, 0x01, 0x85, 0x0d, 0x95, 0x05, 0x92, 0x02, 0x01, 0x09, 0x01, 0x85, 0x0e, 0x95,
    0x09, 0x92, 0x02, 0x01, 0x09, 0x01, 0x85, 0x0f, 0x95, 0x0d, 0x92, 0x02, 0x01, 0x09,
    0x01, 0x85, 0x10, 0x95, 0x11, 0x92, 0x02, 0x01, 0x09, 0x01, 0x85, 0x11, 0x95, 0x19,
    0x92, 0x02, 0x01, 0x09, 0x01, 0x85, 0x12, 0x95, 0x31, 0x92, 0x02, 0x01, 0x09, 0x01,
    0x85, 0x13, 0x95, 0x5f, 0x92, 0x02, 0x01, 0x09, 0x01, 0x85, 0x14, 0x95, 0xc1, 0x92,
    0x02, 0x01, 0x09, 0x01, 0x85, 0x15, 0x95, 0xff, 0x92, 0x02, 0x01, 0xc0
];

fn iface(num: u8, alt: u8, num_eps: u8, class: u8, subclass: u8, protocol: u8, string: u8) -> Vec<u8> {
    vec![9, 0x04, num, alt, num_eps, class, subclass, protocol, string]
}

fn endpoint(addr: u8, attributes: u8, max_packet: u16, interval: u8) -> Vec<u8> {
    let mp = max_packet.to_le_bytes();
    vec![7, 0x05, addr, attributes, mp[0], mp[1], interval]
}

fn bulk(addr: u8, hs: bool) -> Vec<u8> {
    endpoint(addr, 0x02, if hs { 512 } else { 64 }, 0)
}

fn interrupt(addr: u8, hs: bool) -> Vec<u8> {
    endpoint(addr, 0x03, 64, if hs { 7 } else { 8 })
}

/// HID class descriptor (bcdHID 1.11, one report descriptor of HID_REPORT's size).
fn hid_desc() -> Vec<u8> {
    let len = (HID_REPORT.len() as u16).to_le_bytes();
    vec![9, 0x21, 0x11, 0x01, 0, 1, 0x22, len[0], len[1]]
}

/// An interface group: given the first interface number inside this FunctionFS
/// instance and the speed, returns its descriptors and how many interfaces it uses.
type Group = fn(u8, bool) -> (Vec<Vec<u8>>, u8);

/// PTP (still image) interface.
fn ptp(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    (vec![iface(n, 0, 3, 0x06, 0x01, 0x01, 0), bulk(0x02, hs), bulk(0x81, hs), interrupt(0x83, hs)], 1)
}

/// iPod USB Interface: audio control + audio streaming (alt 0 idle, alt 1
/// isochronous IN) + HID.  Class-specific audio descriptors are omitted because
/// FunctionFS rejects type 0x24.
/// Whether configuration 2's audio function comes from the kernel's `f_uac1`
/// instead of the bare descriptors below.
///
/// The CPC200-CCPA (which works in the Jaguar) presents config 2 as 149 bytes
/// including a full USB Audio Class microphone: AC HEADER, INPUT_TERMINAL
/// (0x0201 Microphone), OUTPUT_TERMINAL (0x0101 USB Streaming), AS_GENERAL,
/// FORMAT_TYPE (2 ch / 16-bit / 9 rates 8000..48000) and a CS_ENDPOINT. Those
/// are class-specific descriptor types 0x24/0x25, and `ffs_do_single_desc()`
/// rejects any type it does not know, so FunctionFS cannot carry them -- our
/// config 2 was ~59 bytes with the audio interfaces present but empty.
///
/// With this enabled, `f_uac1` supplies interfaces 0 and 1 (with conformant
/// class descriptors) and FunctionFS supplies only the HID interface, which the
/// kernel numbers 2 -- the same 0/1/2 layout the car sees on a real device.
///
/// `RASCAR_UAC1=off` falls back to the old descriptors without a rebuild.
pub fn uac1_enabled() -> bool {
    !matches!(std::env::var("RASCAR_UAC1"), Ok(v) if v.eq_ignore_ascii_case("off"))
}

/// iPod USB Interface.
///
/// With `f_uac1` enabled this contributes ONLY the HID interface. FunctionFS
/// interface numbers are rewritten by the kernel at bind
/// (`__ffs_func_bind_do_descs` assigns from `usb_interface_id()`), so declaring
/// it as interface `n` here still lands on 2 on the wire when uac1 was linked
/// into the configuration first.
fn ipod_usb(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    if uac1_enabled() { ipod_usb_uac1(n, hs) } else { ipod_usb_legacy(n, hs) }
}

/// HID only -- `f_uac1` provides interfaces 0 and 1.
fn ipod_usb_uac1(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    (vec![iface(n, 0, 1, 0x03, 0x00, 0x00, 0), hid_desc(), interrupt(0x83, hs)], 1)
}

/// Audio control + audio streaming + HID, all as bare descriptors.
fn ipod_usb_legacy(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    (
        vec![
            iface(n, 0, 0, 0x01, 0x01, 0x00, 0),
            iface(n + 1, 0, 0, 0x01, 0x02, 0x00, 0),
            iface(n + 1, 1, 1, 0x01, 0x02, 0x00, 0),
            endpoint(0x81, 0x05, 192, 1),
            iface(n + 2, 0, 1, 0x03, 0x00, 0x00, 0),
            hid_desc(),
            interrupt(0x83, hs),
        ],
        3,
    )
}

/// Apple USB Multiplexor (usbmuxd); iInterface = string 1 of the instance.
fn apple_mobile_device(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    (vec![iface(n, 0, 2, 0xFF, 0xFE, 0x02, 1), bulk(0x04, hs), bulk(0x85, hs)], 1)
}

/// Apple USB Ethernet (ipheth): alt 0 idle, alt 1 with two bulk endpoints.
fn apple_ethernet(n: u8, hs: bool) -> (Vec<Vec<u8>>, u8) {
    (
        vec![
            iface(n, 0, 0, 0xFF, 0xFD, 0x01, 0),
            iface(n, 1, 2, 0xFF, 0xFD, 0x01, 0),
            bulk(0x86, hs),
            bulk(0x05, hs),
        ],
        1,
    )
}

/// One USB configuration of the gadget, backed by its own FunctionFS instance.
pub struct ConfigSpec {
    /// Configuration string (`configs/c.N/strings/0x409/configuration`).
    pub label: &'static str,
    groups: &'static [Group],
}

/// The four configurations of a real iPhone, in configuration-value order
/// (index 0 is `c.1`).
pub const CONFIGS: &[ConfigSpec] = &[
    ConfigSpec { label: "PTP", groups: &[ptp] },
    ConfigSpec { label: "iPod USB Interface", groups: &[ipod_usb] },
    ConfigSpec { label: "PTP + Apple Mobile Device", groups: &[ptp, apple_mobile_device] },
    ConfigSpec {
        label: "PTP + Apple Mobile Device + Apple USB Ethernet",
        groups: &[ptp, apple_mobile_device, apple_ethernet],
    },
];

impl ConfigSpec {
    /// FunctionFS v2 descriptor bundle with FS + HS sets.
    pub fn descriptors(&self) -> Vec<u8> {
        descriptors(self.groups)
    }
}

fn descriptors(groups: &[Group]) -> Vec<u8> {
    let (mut fs, mut hs): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
    let (mut n_fs, mut n_hs) = (0u8, 0u8);
    for group in groups {
        let (d, k) = group(n_fs, false);
        fs.extend(d);
        n_fs += k;
        let (d, k) = group(n_hs, true);
        hs.extend(d);
        n_hs += k;
    }
    let fs_bytes = fs.concat();
    let hs_bytes = hs.concat();
    // header (magic, length, flags) + fs_count + hs_count + both sets.
    let total = 12 + 4 + 4 + fs_bytes.len() + hs_bytes.len();

    let mut bundle = Vec::with_capacity(total);
    bundle.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    bundle.extend_from_slice(&(total as u32).to_le_bytes());
    bundle.extend_from_slice(&DESCRIPTOR_FLAGS.to_le_bytes());
    bundle.extend_from_slice(&(fs.len() as u32).to_le_bytes());
    bundle.extend_from_slice(&(hs.len() as u32).to_le_bytes());
    bundle.extend_from_slice(&fs_bytes);
    bundle.extend_from_slice(&hs_bytes);
    bundle
}

/// FunctionFS strings bundle: one language (en-US), one string (index 1, used
/// by the Apple Mobile Device interface).  Every instance writes the same one;
/// `str_count >= 1` is required for the FFS state machine to reach ACTIVE.
pub fn strings() -> Vec<u8> {
    let s1: &[u8] = b"Apple USB Multiplexor\0";
    let (str_count, lang_count) = (1u32, 1u32);
    let total = 16 + 2 + s1.len();
    let mut bundle = Vec::with_capacity(total);
    bundle.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    bundle.extend_from_slice(&(total as u32).to_le_bytes());
    bundle.extend_from_slice(&str_count.to_le_bytes());
    bundle.extend_from_slice(&lang_count.to_le_bytes());
    bundle.extend_from_slice(&0x0409u16.to_le_bytes());
    bundle.extend_from_slice(s1);
    bundle
}

// ── ep0 set ───────────────────────────────────────────────────────────────────

/// One opened ep0, tagged with its configuration value.
struct Ep0 {
    config: u8,
    file: File,
}

/// Every FunctionFS ep0 of the gadget, kept open together.  Dropping the set
/// closes all of them, which unbinds the gadget.
pub struct Ep0Set {
    eps: Vec<Ep0>,
}

impl Ep0Set {
    /// Open every configuration's ep0 and write its descriptors + strings.
    /// Must run after `gadget::create()` and before `gadget::bind()`.
    ///
    /// Blocking (plain syscalls); call it from a blocking context.  On error the
    /// already-opened ep0 fds are closed when the partial set drops.
    pub fn open_all() -> Result<Self> {
        let mut set = Ep0Set { eps: Vec::with_capacity(CONFIGS.len()) };
        let strings = strings();
        for (index, spec) in CONFIGS.iter().enumerate() {
            let config = index as u8 + 1;
            let path = format!("{}/ep0", mount_point(config));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("opening {path} — is FunctionFS instance {} mounted?", instance_name(config)))?;
            let descs = spec.descriptors();
            ep0_write(file.as_fd(), &descs)
                .with_context(|| format!("c.{config} '{}': writing {} bytes of descriptors", spec.label, descs.len()))?;
            ep0_write(file.as_fd(), &strings)
                .with_context(|| format!("c.{config} '{}': writing {} bytes of strings", spec.label, strings.len()))?;
            info!(
                "FFS c.{config} '{}': {} bytes of FS+HS descriptors + {} bytes of strings written ({})",
                spec.label,
                descs.len(),
                strings.len(),
                mount_point(config)
            );
            set.eps.push(Ep0 { config, file });
        }
        info!("FFS descriptors written for {} configurations — ready for bind", set.eps.len());
        Ok(set)
    }
}

// ── Raw ep0 data/status-stage helpers ─────────────────────────────────────────
//
// The data/status stage of a control transfer must reach the kernel as exactly
// one real `read(2)` / `write(2)` on the ep0 descriptor.  Buffered or looping
// I/O cannot express that: tokio's `AsyncWriteExt::write_all(&[])` and std's
// `Write::write_all(&[])` both return without a syscall, so a zero-length packet
// is only ever emitted by calling `write(2)` directly.  The capture loop runs on
// a blocking thread, so these are plain syscalls.

/// One `write(2)` on ep0: a descriptor/strings bundle, or an IN data stage
/// (zero-length is a genuine ZLP).  A short write is an error.
fn ep0_write(fd: BorrowedFd<'_>, data: &[u8]) -> Result<()> {
    let n = nix::unistd::write(fd, data).map_err(std::io::Error::from)?;
    if n != data.len() {
        bail!("short ep0 write: {n} of {} bytes", data.len());
    }
    Ok(())
}

/// Exactly ONE `read(2)` on ep0.
///
/// Never loop to fill the buffer: a host may legitimately end an OUT data stage
/// short of `wLength`, and a second read would be served from the FunctionFS
/// *event* queue, splicing a 12-byte `usb_functionfs_event` into the payload and
/// desynchronising every later request.  `len == 0` performs the status stage of
/// a `wLength == 0` OUT setup; on an IN setup it makes the kernel stall ep0.
fn ep0_read(fd: RawFd, len: usize) -> nix::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let n = nix::unistd::read(fd, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

// ── Request classification ────────────────────────────────────────────────────

/// Decoded `usb_ctrlrequest` (little-endian on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Setup {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

impl Setup {
    fn parse(event: &[u8; EVENT_SIZE]) -> Self {
        Setup {
            request_type: event[0],
            request: event[1],
            value: u16::from_le_bytes([event[2], event[3]]),
            index: u16::from_le_bytes([event[4], event[5]]),
            length: u16::from_le_bytes([event[6], event[7]]),
        }
    }

    fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }

    fn is_role_switch(&self) -> bool {
        self.request_type == REQTYPE_VENDOR_OUT
            && self.request == CARPLAY_ROLE_SWITCH_REQ
            && self.value == CARPLAY_ROLE_SWITCH_VALUE
    }
}

/// How a SETUP is answered (same decision table as the probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// HID GET_DESCRIPTOR(report): write `HID_REPORT[..wLength]`.
    HidReport,
    /// HID GET_REPORT: write wLength zero bytes.
    GetReport,
    /// Known vendor IN request: write the payload truncated to wLength.
    VendorReply(&'static [u8]),
    /// Any other IN request: stall (a read on an IN setup).
    Stall,
    /// Any OUT request: read the data stage (or the zero-length status stage).
    AckOut,
}

fn classify(setup: &Setup) -> Answer {
    if setup.request_type == 0x81 && setup.request == 0x06 && (setup.value >> 8) == 0x22 {
        return Answer::HidReport;
    }
    if setup.request_type == 0xA1 && setup.request == 0x01 {
        return Answer::GetReport;
    }
    if setup.is_in() {
        if (setup.request_type & 0x60) == 0x40 {
            if setup.request == CARPLAY_ROLE_READ_REQ && hide_carplay_capability() {
                return Answer::VendorReply(&NO_ROLE_SWITCH_CAPABILITY);
            }
            if let Some((_, reply)) = VENDOR_IN_REPLIES.iter().find(|(req, _)| *req == setup.request) {
                return Answer::VendorReply(reply);
            }
        }
        return Answer::Stall;
    }
    Answer::AckOut
}

/// Perform the data/status stage for `answer`; returns a description for the
/// `→ SETUP #n answered` line.
fn answer_setup(file: &File, setup: &Setup, answer: Answer) -> Result<String> {
    let len = setup.length as usize;
    match answer {
        Answer::HidReport => {
            let data = &HID_REPORT[..len.min(HID_REPORT.len())];
            ep0_write(file.as_fd(), data)?;
            Ok(format!("HID report descriptor {} bytes", data.len()))
        }
        Answer::GetReport => {
            ep0_write(file.as_fd(), &vec![0u8; len])?;
            Ok(format!("GET_REPORT id={} answered {len} zero bytes", setup.value & 0xff))
        }
        Answer::VendorReply(reply) => {
            let data = &reply[..len.min(reply.len())];
            ep0_write(file.as_fd(), data)?;
            Ok(format!("replied {}", hex::encode(data)))
        }
        Answer::Stall => match ep0_read(file.as_raw_fd(), 0) {
            // The kernel halts ep0 and reports EL2HLT: that IS the stall.
            Err(Errno::EL2HLT) => Ok("stalled (no reply defined)".to_string()),
            Err(e) => Err(std::io::Error::from(e)).context("stalling ep0"),
            Ok(_) => bail!("read on an IN setup returned data instead of stalling"),
        },
        Answer::AckOut => {
            let data = ep0_read(file.as_raw_fd(), len).map_err(std::io::Error::from)?;
            let tag = if (setup.request_type, setup.request) == (0x21, 0x09) {
                forward_hid_report(&data);
                format!(" SET_REPORT id={}", setup.value & 0xff)
            } else {
                String::new()
            };
            if len == 0 {
                Ok(format!("ACKed OUT{tag} (zero-length status read)"))
            } else {
                Ok(format!(
                    "ACKed OUT{tag}: read {} of wLength={len} bytes, data {}",
                    data.len(),
                    hex::encode(&data)
                ))
            }
        }
    }
}

// ── iAP2-over-HID sink ────────────────────────────────────────────────────────

/// Where SET_REPORT data stages go when the device-mode iAP2 responder runs
/// (see `hid_iap2`). `None` (the default) keeps them log-only.
static HID_IAP2_SINK: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>> =
    std::sync::Mutex::new(None);

/// Install (or clear) the receiver of the car's HID Output reports.
pub fn set_hid_iap2_sink(sink: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>) {
    if let Ok(mut slot) = HID_IAP2_SINK.lock() {
        *slot = sink;
    }
}

fn forward_hid_report(data: &[u8]) {
    if let Ok(slot) = HID_IAP2_SINK.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(data.to_vec());
        }
    }
}

// ── Capture loop ──────────────────────────────────────────────────────────────

/// Why the ep0 capture loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The car issued the CarPlay role-switch request (`40 51 wValue=1`).
    /// The caller should now tear the gadget down and run Phase 3.
    RoleSwitchRequested,
}

/// Poll interval; also the cadence of the UDC state/speed check.
const POLL_TIMEOUT_MS: u16 = 200;

/// How long after the car configures us we wait for the CarPlay role switch
/// before re-presenting ourselves.
///
/// From a cold start the car configures us within ~2.5 s, reads the role-switch
/// capability (C0 53) and asks for charging current (40 40), then sends 40 51
/// on that SAME enumeration once its CarPlay service is up: t+21.8..26.9 s
/// after power-on over 7 cold starts (2026-10-07), 5-11 s after the last C0 53,
/// with no new enumeration needed. An already-running car sends it ~0.7 s after
/// C0 53. A re-presentation that lands while the car is deciding cuts it off:
/// the old 3/6/12 s backoff re-presented at t+27.8 s, and on the one cold start
/// that had not switched by then the car never sent 40 51
/// again, through 10 more re-presentations over 5 min and two Pi reboots, until
/// its head unit reset the bus itself. So the first re-presentation waits well
/// past the slowest observed cold boot; re-presenting is only a last resort.
/// Override with RASCAR_ROLE_SWITCH_GRACE_MS.
const ROLE_SWITCH_GRACE: Duration = Duration::from_secs(45);
/// Upper bound of the backoff between re-presentations; the Pi stays plugged
/// in, so we keep trying for as long as the car takes.
const ROLE_SWITCH_GRACE_MAX: Duration = Duration::from_secs(60);
/// Time off the bus for a re-presentation (like the stay-device path).
const REPRESENT_OFF: Duration = Duration::from_millis(1000);

fn role_switch_grace() -> Duration {
    std::env::var("RASCAR_ROLE_SWITCH_GRACE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms >= 500)
        .map(Duration::from_millis)
        .unwrap_or(ROLE_SWITCH_GRACE)
}

/// Wait before the `attempt`-th re-presentation (0-based): the grace period,
/// doubling each time up to `ROLE_SWITCH_GRACE_MAX`.
fn represent_wait(grace: Duration, attempt: u32) -> Duration {
    grace.saturating_mul(1u32 << attempt.min(8)).min(ROLE_SWITCH_GRACE_MAX.max(grace))
}
/// Back-off after an ep0 read error (host reset / unbind), like the probe.
const READ_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Phase 1 capture: answer every control request the car sends on ANY of the
/// configurations' ep0s until the role-switch request.
///
/// Consumes the set: all ep0 fds are closed before this returns, whatever the
/// outcome, so the caller can tear the gadget down right away.
///
/// ep0 multiplexing: one blocking thread runs `poll(2)` over every ep0 and
/// answers each SETUP synchronously on the fd it came from, exactly like the
/// probe.  Per-fd reader tasks would be wrong here: after a SETUP event the next
/// read on that fd IS the data/status stage, and an independent reader blocked
/// on the same fd would steal it.
pub async fn capture_control_requests(set: Ep0Set, udc: String) -> Result<CaptureOutcome> {
    tokio::task::spawn_blocking(move || {
        let outcome = capture_blocking(&set, &udc);
        let count = set.eps.len();
        drop(set);
        info!("FFS: closed all {count} ep0 fds (t+{} ms)", crate::debug::elapsed_ms());
        outcome
    })
    .await
    .context("joining the ep0 capture thread")?
}

fn capture_blocking(set: &Ep0Set, udc: &str) -> Result<CaptureOutcome> {
    info!(
        "capturing car control requests on {} ep0s (waiting for CarPlay attach)...",
        set.eps.len()
    );
    info!(
        "looking for role-switch: bmRequestType=0x{REQTYPE_VENDOR_OUT:02x} bRequest=0x{CARPLAY_ROLE_SWITCH_REQ:02x} wValue={CARPLAY_ROLE_SWITCH_VALUE}"
    );

    let mut setup_count: u64 = 0;
    let mut last_udc: Option<(String, String)> = None;
    let grace = role_switch_grace();
    // When the car last configured us without (yet) asking for CarPlay.
    let mut configured_at: Option<std::time::Instant> = None;
    let mut represents: u32 = 0;

    loop {
        // UDC state + negotiated speed, logged on change (HS is required).
        let udc_now = (
            crate::debug::read_attr(format!("/sys/class/udc/{udc}/state")),
            crate::debug::read_attr(format!("/sys/class/udc/{udc}/current_speed")),
        );
        if last_udc.as_ref() != Some(&udc_now) {
            info!("UDC state={} speed={} (t+{} ms)", udc_now.0, udc_now.1, crate::debug::elapsed_ms());
            configured_at = (udc_now.0 == "configured").then(std::time::Instant::now);
            last_udc = Some(udc_now);
        }

        // Configured but no role switch long after any cold boot should have
        // finished: last resort, re-present ourselves so the car looks again.
        if let Some(since) = configured_at {
            let wait = represent_wait(grace, represents);
            if since.elapsed() >= wait {
                represents += 1;
                warn!(
                    "car configured us {} ms ago but sent no CarPlay role switch — car wedged? \
                     re-presenting (soft reconnect #{represents}, next wait {} ms)",
                    since.elapsed().as_millis(),
                    represent_wait(grace, represents).as_millis()
                );
                match crate::usb::gadget::soft_reconnect(udc, REPRESENT_OFF) {
                    Ok(()) => info!("re-presented to the car (t+{} ms), expecting a fresh enumeration", crate::debug::elapsed_ms()),
                    Err(e) => warn!("re-presentation failed: {e:#}"),
                }
                configured_at = None;
                last_udc = None; // log the state after the reconnect
            }
        }

        let mut fds: Vec<PollFd<'_>> = set
            .eps
            .iter()
            .map(|ep| PollFd::new(ep.file.as_fd(), PollFlags::POLLIN))
            .collect();
        match poll(&mut fds, PollTimeout::from(POLL_TIMEOUT_MS)) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(std::io::Error::from(e)).context("poll(2) over the ep0 fds"),
        }
        let ready: Vec<PollFlags> = fds.iter().map(|p| p.revents().unwrap_or(PollFlags::empty())).collect();
        drop(fds);

        for (ep, revents) in set.eps.iter().zip(ready) {
            if revents.contains(PollFlags::POLLNVAL) {
                bail!("c.{} ep0 fd is no longer valid (POLLNVAL)", ep.config);
            }
            if !revents.intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP) {
                continue;
            }
            if let Some(outcome) = handle_event(ep, &mut setup_count)? {
                return Ok(outcome);
            }
        }
    }
}

/// Read and handle one event from `ep`.  Returns `Some` on the role switch.
fn handle_event(ep: &Ep0, setup_count: &mut u64) -> Result<Option<CaptureOutcome>> {
    let c = ep.config;
    let mut event = [0u8; EVENT_SIZE];
    let n = match nix::unistd::read(ep.file.as_raw_fd(), &mut event) {
        Ok(n) => n,
        Err(Errno::EINTR) => return Ok(None),
        // EIDRM / ESHUTDOWN happen when the host resets mid-transfer; the probe
        // logs and keeps going, and so do we.
        Err(e) => {
            warn!("FFS ep0 read error [c.{c}]: {e} (host reset/unbind?) — continuing");
            std::thread::sleep(READ_ERROR_BACKOFF);
            return Ok(None);
        }
    };
    if n != EVENT_SIZE {
        warn!("FFS event [c.{c}]: short read of {n} bytes, ignored");
        return Ok(None);
    }

    let event_type = event[8];
    let t_ms = crate::debug::elapsed_ms();
    debug!(
        "FFS event raw {} [c.{c}] (t+{t_ms} ms): {}",
        event_name(event_type),
        crate::debug::hex_prefix(&event, event.len())
    );

    let label = CONFIGS.get(c as usize - 1).map(|s| s.label).unwrap_or("?");
    match event_type {
        FUNCTIONFS_BIND => info!("FFS event BIND [c.{c}] (t+{t_ms} ms) — function bound to the gadget"),
        FUNCTIONFS_ENABLE => info!("FFS event ENABLE [c.{c}] (t+{t_ms} ms) — car selected configuration {c} '{label}'"),
        FUNCTIONFS_DISABLE => info!("FFS event DISABLE [c.{c}] (t+{t_ms} ms) — configuration {c} deselected"),
        FUNCTIONFS_UNBIND => warn!("FFS event UNBIND [c.{c}] (t+{t_ms} ms) — gadget detached from UDC (cable pulled or host reset)"),
        FUNCTIONFS_SUSPEND => info!("FFS event SUSPEND [c.{c}] (t+{t_ms} ms) — host suspended the bus"),
        FUNCTIONFS_RESUME => info!("FFS event RESUME [c.{c}] (t+{t_ms} ms) — host resumed the bus"),
        FUNCTIONFS_SETUP => {
            *setup_count += 1;
            let n = *setup_count;
            let setup = Setup::parse(&event);
            let class_name = match (setup.request_type >> 5) & 0x03 {
                0 => "standard",
                1 => "class",
                2 => "vendor",
                _ => "reserved",
            };
            let request_name = if class_name == "standard" { standard_request_name(setup.request) } else { "-" };
            info!(
                "FFS event SETUP #{n} [c.{c}] (t+{t_ms} ms): bmRequestType=0x{:02x} ({class_name},{},recipient={}) bRequest=0x{:02x} ({request_name}) wValue=0x{:04x} wIndex=0x{:04x} wLength={}",
                setup.request_type,
                if setup.is_in() { "IN" } else { "OUT" },
                recipient_name(setup.request_type),
                setup.request,
                setup.value,
                setup.index,
                setup.length
            );

            let role_switch = setup.is_role_switch();
            if role_switch {
                info!("**** CARPLAY ROLE-SWITCH REQUEST **** [c.{c}] car asked the iPhone to become USB host (40 51 wValue=1)");
            }
            if setup.request_type == REQTYPE_VENDOR_OUT && setup.request == CARPLAY_ROLE_SWITCH_REQ && setup.value == 0 {
                info!("**** ROLE-SWITCH wValue=0 **** [c.{c}] car asked the iPhone to STAY USB device (non-CarPlay / InControl path?)");
                if crate::usb::hid_iap2::enabled() {
                    crate::usb::hid_iap2::soft_reconnect_after_stay_device();
                }
            }
            if setup.request_type == 0xC0 && setup.request == CARPLAY_ROLE_READ_REQ {
                if hide_carplay_capability() {
                    info!("**** CARPLAY ROLE READ **** [c.{c}] car reads role-switch capability (C0 53), answering 00 00 00 00 (RASCAR_HIDE_CARPLAY: no CarPlay)");
                } else {
                    info!("**** CARPLAY ROLE READ **** [c.{c}] car reads role-switch capability (C0 53), answering 01 00 00 00");
                }
            }

            let answer = classify(&setup);
            match answer_setup(&ep.file, &setup, answer) {
                Ok(description) => info!(
                    "→ SETUP #{n} [c.{c}] (bRequest=0x{:02x}) answered at t+{} ms: {description}",
                    setup.request,
                    crate::debug::elapsed_ms()
                ),
                Err(e) => error!(
                    "→ SETUP #{n} [c.{c}] (bRequest=0x{:02x}) data/status stage FAILED: {e:#} — the car may see a stall",
                    setup.request
                ),
            }

            if role_switch {
                // Handed off even if the ACK failed: the car did ask for the switch.
                info!("ep0 capture: role-switch request handled — returning to tear down the gadget and run Phase 3");
                return Ok(Some(CaptureOutcome::RoleSwitchRequested));
            }
        }
        other => info!("FFS event {} [c.{c}] type={other} (t+{t_ms} ms) — not handled", event_name(other)),
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden bundles generated by the hardware-proven probe:
    //   m = ffs_multiconfig_probe.py; m.REPORT = m.HID_REPORT["fs"]
    // 2026-09-21: GOLDEN_C2_IPOD's two HID class descriptors now carry
    // wDescriptorLength = 0x00d0 (208), not 0x0060 (96), because HID_REPORT was
    // replaced with the CPC200-CCPA's descriptor. Only that field changed; the
    // bundle length is unaffected (wDescriptorLength describes the report
    // descriptor served over ep0, not bytes present in the bundle).
    //   m.descriptors(builders, False).hex() for each LAYOUTS["iphone4"] entry,
    //   m.strings().hex()
    const GOLDEN_C1_PTP: &str = "0300000050000000c30000000400000004000000090400000306010100070502024000000705810240000007058303400008090400000306010100070502020002000705810200020007058303400007";
    const GOLDEN_C2_IPOD: &str = "030000008a000000c3000000070000000700000009040000000101000009040100000102000009040101010102000007058105c0000109040200010300000009211101000122d0000705830340000809040000000101000009040100000102000009040101010102000007058105c0000109040200010300000009211101000122d00007058303400007";
    const GOLDEN_C3_PTP_AMD: &str = "030000007e000000c300000007000000070000000904000003060101000705020240000007058102400000070583034000080904010002fffe020107050402400000070585024000000904000003060101000705020200020007058102000200070583034000070904010002fffe02010705040200020007058502000200";
    const GOLDEN_C4_PTP_AMD_ETH: &str = "03000000be000000c30000000b0000000b0000000904000003060101000705020240000007058102400000070583034000080904010002fffe020107050402400000070585024000000904020000fffd01000904020102fffd010007058602400000070505024000000904000003060101000705020200020007058102000200070583034000070904010002fffe020107050402000200070585020002000904020000fffd01000904020102fffd01000705860200020007050502000200";
    const GOLDEN_STRINGS: &str = "0200000028000000010000000100000009044170706c6520555342204d756c7469706c65786f7200";
    /// c.2 when f_uac1 supplies the audio: HID interface + HID class descriptor
    /// + interrupt endpoint, per speed.
    const GOLDEN_C2_IPOD_UAC1: &str = "0300000046000000c3000000030000000300000009040000010300000009211101000122d0000705830340000809040000010300000009211101000122d00007058303400007";
    const GOLDEN_HID_REPORT: &str = "0600ff0901a1017508268000150009018501950582020109018502950982020109018503950d82020109018504951182020109018505951982020109018506953182020109018507955f8202010901850895c1820201090185099601018202010901850a9681018202010901850b9601028202010901850c96ff028202010901850d95059202010901850e95099202010901850f950d92020109018510951192020109018511951992020109018512953192020109018513955f9202010901851495c19202010901851595ff920201c0";

    #[test]
    fn hide_carplay_is_opt_in_and_answers_zero() {
        assert!(!hide_carplay_from(None));
        assert!(!hide_carplay_from(Some("0")));
        assert!(hide_carplay_from(Some("1")));
        assert_eq!(NO_ROLE_SWITCH_CAPABILITY, [0, 0, 0, 0]);
        // Default path still claims CarPlay.
        let claimed = VENDOR_IN_REPLIES.iter().find(|(r, _)| *r == CARPLAY_ROLE_READ_REQ).unwrap().1;
        assert_eq!(claimed, &[0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn config_labels_match_probe() {
        let labels: Vec<&str> = CONFIGS.iter().map(|c| c.label).collect();
        assert_eq!(
            labels,
            [
                "PTP",
                "iPod USB Interface",
                "PTP + Apple Mobile Device",
                "PTP + Apple Mobile Device + Apple USB Ethernet"
            ]
        );
    }

    #[test]
    fn descriptor_bundles_are_byte_identical_to_probe() {
        // c.2 is asserted separately: it now depends on whether f_uac1 supplies
        // the audio interfaces, so both layouts are pinned below rather than
        // whichever one this process's environment selects.
        let golden = [GOLDEN_C1_PTP, GOLDEN_C3_PTP_AMD, GOLDEN_C4_PTP_AMD_ETH];
        for (spec, want) in [&CONFIGS[0], &CONFIGS[2], &CONFIGS[3]].iter().zip(golden) {
            assert_eq!(hex::encode(spec.descriptors()), want, "config '{}'", spec.label);
        }
    }

    /// Legacy layout: audio interfaces present but with no class-specific
    /// descriptors behind them (FunctionFS rejects type 0x24).
    #[test]
    fn ipod_usb_legacy_bundle_matches_the_probe() {
        assert_eq!(hex::encode(descriptors(&[ipod_usb_legacy])), GOLDEN_C2_IPOD);
    }

    /// With f_uac1 linked first, FunctionFS contributes only the HID interface;
    /// the kernel renumbers it to 2 at bind.
    #[test]
    fn ipod_usb_uac1_bundle_is_hid_only() {
        let hex = hex::encode(descriptors(&[ipod_usb_uac1]));
        assert_eq!(hex, GOLDEN_C2_IPOD_UAC1);
        // one interface per speed, and it is the HID class (0x03)
        let (descs, n) = ipod_usb_uac1(0, true);
        assert_eq!(n, 1, "must claim exactly one interface number");
        assert_eq!(descs[0][5], 0x03, "interface class must be HID");
        assert!(descs.iter().all(|d| d[1] != 0x24), "no CS_INTERFACE via FunctionFS");
    }

    #[test]
    fn strings_bundle_is_byte_identical_to_probe() {
        assert_eq!(hex::encode(strings()), GOLDEN_STRINGS);
    }

    #[test]
    fn hid_report_is_byte_identical_to_the_ccpa() {
        assert_eq!(HID_REPORT.len(), 208);
        assert_eq!(hex::encode(HID_REPORT), GOLDEN_HID_REPORT);
    }

    #[test]
    fn mount_points_and_instances() {
        assert_eq!(mount_point(1), "/dev/ffs-carplay-c1");
        assert_eq!(instance_name(4), "c4");
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Setup {
        Setup { request_type, request, value, index, length }
    }

    #[test]
    fn setup_parses_little_endian() {
        let ev = [0x40, 0x40, 0xf4, 0x01, 0xe8, 0x03, 0x00, 0x00, FUNCTIONFS_SETUP, 0, 0, 0];
        assert_eq!(Setup::parse(&ev), setup(0x40, 0x40, 500, 1000, 0));
    }

    #[test]
    fn classify_matches_probe_decision_table() {
        // Sequence measured from the car.
        assert_eq!(classify(&setup(0x21, 0x0a, 0, 2, 0)), Answer::AckOut); // SET_IDLE
        assert_eq!(classify(&setup(0x81, 0x06, 0x2200, 2, 96)), Answer::HidReport);
        assert_eq!(classify(&setup(0xC0, 0x53, 0, 0, 4)), Answer::VendorReply(&[1, 0, 0, 0]));
        assert_eq!(classify(&setup(0x40, 0x40, 500, 1000, 0)), Answer::AckOut);
        let role = setup(0x40, 0x51, 1, 0, 0);
        assert_eq!(classify(&role), Answer::AckOut);
        assert!(role.is_role_switch());
        assert!(!setup(0x40, 0x51, 0, 0, 0).is_role_switch());
        // Other rows of the table.
        assert_eq!(classify(&setup(0xA1, 0x01, 0x0101, 2, 12)), Answer::GetReport);
        assert_eq!(classify(&setup(0xC0, 0x45, 0, 0, 1)), Answer::VendorReply(&[1]));
        assert_eq!(classify(&setup(0xC0, 0x99, 0, 0, 4)), Answer::Stall);
        assert_eq!(classify(&setup(0xA1, 0x02, 0, 0, 1)), Answer::Stall); // class IN, not GET_REPORT
        assert_eq!(classify(&setup(0x21, 0x09, 0x0201, 2, 8)), Answer::AckOut); // SET_REPORT
    }
}

#[cfg(test)]
mod represent_tests {
    use super::{represent_wait, ROLE_SWITCH_GRACE, ROLE_SWITCH_GRACE_MAX};
    use std::time::Duration;

    #[test]
    fn re_presentation_backs_off_to_the_cap() {
        let g = Duration::from_secs(3);
        assert_eq!(represent_wait(g, 0), Duration::from_secs(3));
        assert_eq!(represent_wait(g, 1), Duration::from_secs(6));
        assert_eq!(represent_wait(g, 2), Duration::from_secs(12));
        assert_eq!(represent_wait(g, 3), Duration::from_secs(24));
        assert_eq!(represent_wait(g, 4), Duration::from_secs(48));
        assert_eq!(represent_wait(g, 5), ROLE_SWITCH_GRACE_MAX);
        assert_eq!(represent_wait(g, 40), ROLE_SWITCH_GRACE_MAX);
    }

    /// The default schedule must never re-present inside the car's cold-boot
    /// decision window: 40 51 arrived up to t+26.9 s after power-on, and the
    /// car configures us ~2.5 s after power-on.
    #[test]
    fn first_re_presentation_clears_the_cold_boot_window() {
        let configured_at = Duration::from_millis(2_500);
        let slowest_cold_switch = Duration::from_millis(26_900);
        let first = configured_at + represent_wait(ROLE_SWITCH_GRACE, 0);
        assert!(first >= slowest_cold_switch + Duration::from_secs(15), "first re-presentation at {first:?}");
        assert_eq!(represent_wait(ROLE_SWITCH_GRACE, 1), ROLE_SWITCH_GRACE_MAX);
    }
}
