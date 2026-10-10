//! USB FunctionFS implementation — replaces usb_f_accessory.
//!
//! The usb_f_accessory kernel module is Android-BSP-only and unavailable in
//! mainline/Armbian kernels.  We replace it with FunctionFS (usb_f_fs), which
//! lets userspace handle AOA2 vendor control requests on ep0 and exposes bulk
//! endpoints ep1/ep2 for AA data transfer.
//!
//! Flow:
//!   1. gadget::create() mounts FunctionFS at FFS_MOUNT
//!   2. Caller opens ep0 and calls write_descriptors()
//!   3. gadget::bind() binds to UDC
//!   4. handle_aoa_setup() loops on ep0: responds to GET_PROTOCOL, ACKs
//!      SEND_STRING, signals on START_ACCESSORY
//!   5. gadget::switch_to_accessory_mode() unbinds, sets PID=0x2D00, rebinds
//!   6. wait_for_enable() waits for the car to reconnect and select the config
//!   7. open_endpoints() opens ep1 (bulk OUT) and ep2 (bulk IN)

use anyhow::{Context, Result};
use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

pub const FFS_MOUNT: &str = "/dev/ffs-rascar";

// ── FunctionFS binary format constants ───────────────────────────────────────

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
/// Deliver vendor/class requests targeting any recipient (not just our interface).
/// Required to receive AOA2 vendor requests which target the DEVICE recipient.
const FUNCTIONFS_ALL_CTRL_RECIP: u32 = 64;

// ── ep0 event types ───────────────────────────────────────────────────────────

const FUNCTIONFS_BIND: u8 = 0;
const FUNCTIONFS_UNBIND: u8 = 1;
const FUNCTIONFS_ENABLE: u8 = 2;
const FUNCTIONFS_DISABLE: u8 = 3;
const FUNCTIONFS_SETUP: u8 = 4;

// ── AOA2 vendor request codes (ep0, bRequestType & 0x60 == 0x40) ─────────────

const AOA_GET_PROTOCOL: u8 = 51; // IN  — device returns supported AOA version
const AOA_SEND_STRING: u8 = 52;  // OUT — host sends accessory identification strings
const AOA_START_ACCESSORY: u8 = 53; // OUT — trigger re-enumeration as accessory
const AOA_VERSION: u16 = 2;

// ── Descriptor helpers ────────────────────────────────────────────────────────

fn interface_desc() -> [u8; 9] {
    [
        0x09, // bLength
        0x04, // bDescriptorType = INTERFACE
        0x00, // bInterfaceNumber
        0x00, // bAlternateSetting
        0x02, // bNumEndpoints
        0xFF, // bInterfaceClass = VENDOR
        0xFF, // bInterfaceSubClass
        0x00, // bInterfaceProtocol
        0x01, // iInterface → string index 1 ("RasCar AA")
    ]
}

fn endpoint_desc(addr: u8, max_packet: u16) -> [u8; 7] {
    let mp = max_packet.to_le_bytes();
    [
        0x07, // bLength
        0x05, // bDescriptorType = ENDPOINT
        addr, // bEndpointAddress
        0x02, // bmAttributes = bulk
        mp[0],
        mp[1], // wMaxPacketSize
        0x00,  // bInterval
    ]
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Write USB descriptors and strings to ep0.
/// Must be called after opening ep0 and before gadget::bind().
///
/// NOTE: Only FS descriptors are provided (HAS_HS_DESC omitted).
/// Adding HAS_HS_DESC causes EINVAL on the 6.12 Armbian kernel for RK3588 —
/// possibly a kernel quirk with DWC3 in peripheral mode.  Full Speed (12 Mbps)
/// is sufficient for Android Auto; HS can be added once the HU is working.
pub async fn write_descriptors(ep0: &mut File) -> Result<()> {
    let iface = interface_desc();
    let ep_out_fs = endpoint_desc(0x01, 64);
    let ep_in_fs = endpoint_desc(0x81, 64);

    let fs_descs: Vec<u8> = [iface.as_ref(), ep_out_fs.as_ref(), ep_in_fs.as_ref()].concat();

    let flags = FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_ALL_CTRL_RECIP;
    let fs_count: u32 = 3;
    let total = 12u32 + 4 + fs_descs.len() as u32;

    let mut bundle: Vec<u8> = Vec::with_capacity(total as usize);
    bundle.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    bundle.extend_from_slice(&total.to_le_bytes());
    bundle.extend_from_slice(&flags.to_le_bytes());
    bundle.extend_from_slice(&fs_count.to_le_bytes());
    bundle.extend_from_slice(&fs_descs);

    ep0.write_all(&bundle).await.context("writing FFS descriptor bundle")?;
    debug!("FFS: wrote {} bytes of descriptors", bundle.len());

    // Strings bundle — 1 string (iInterface=1), English only.
    // The kernel requires str_count >= 1 and lang_count >= 1 for the FFS
    // state machine to reach FFS_ACTIVE.  str_count=0 is rejected on 6.12.
    let s1: &[u8] = b"RasCar AA\0"; // index 1: interface string
    let str_count: u32 = 1;
    let lang_count: u32 = 1;
    // header (16) + lang code (2) + s1
    let data_len: u32 = 16 + 2 + s1.len() as u32;
    // round up to 4-byte alignment
    let str_len: u32 = (data_len + 3) & !3;
    let mut strings: Vec<u8> = Vec::with_capacity(str_len as usize);
    strings.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    strings.extend_from_slice(&str_len.to_le_bytes());
    strings.extend_from_slice(&str_count.to_le_bytes());
    strings.extend_from_slice(&lang_count.to_le_bytes());
    strings.extend_from_slice(&0x0409u16.to_le_bytes()); // English
    strings.extend_from_slice(s1);
    // pad to str_len
    strings.resize(str_len as usize, 0);

    ep0.write_all(&strings).await.context("writing FFS strings bundle")?;
    debug!("FFS strings written ({} bytes)", str_len);
    info!("FFS descriptors written — ready for bind");

    Ok(())
}

/// Handle AOA2 vendor setup requests on ep0.
/// Loops reading ep0 events until START_ACCESSORY is received.
/// The caller then performs the accessory-mode switch inline (drop ep0 →
/// gadget::remove → recreate with PID 0x2D00), see main.rs Phase 4.
// ── ep0 control-transfer completion ──────────────────────────────────────────
//
// `ffs_func_setup()` in drivers/usb/gadget/function/f_fs.c ends with:
//
//     return ffs->ev.setup.wLength == 0 ? USB_GADGET_DELAYED_STATUS : 0;
//
// so a `wLength == 0` SETUP delivered to userspace is NOT auto-ACKed by the
// kernel: the UDC waits for userspace to queue a response before running the
// status stage.  dwc3 (the RK3588 controller) honours that, so an unanswered
// zero-length request leaves the host's control transfer hanging until the
// gadget is torn down.  Userspace must perform an explicit zero-length access:
// `read(2)` for an OUT-direction setup, `write(2)` for IN.
//
// Tokio cannot express one — `AsyncWriteExt::write_all(&[])` is
// `while !buf.is_empty() { .. }` and returns `Ok(())` without ever polling
// `poll_write`, so no syscall reaches the descriptor.  Both helpers therefore
// go to the raw fd on the blocking pool.

/// Complete an IN-direction control transfer with a genuine zero-length packet.
async fn ep0_write_zlp(fd: RawFd) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        // SAFETY: `fd` is owned by the ep0 `File` the caller holds by mutable
        // reference across this await, so it stays open for the whole call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        nix::unistd::write(borrowed, &[])
            .map(|_| ())
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
    })
    .await
    .context("joining ep0 zero-length write task")?
    .context("write(2) of the ep0 zero-length packet")
}

/// Perform exactly ONE `read(2)` on ep0 and return what the host actually sent.
///
/// Never `read_exact` here: a host may legitimately end an OUT data stage short
/// of `wLength`, and the second read would then be served from the FunctionFS
/// *event* queue, splicing a 12-byte `usb_functionfs_event` into the control
/// payload and desynchronising every later request.  `len == 0` performs the
/// status-stage access for a `wLength == 0` OUT setup.
async fn ep0_read(fd: RawFd, len: usize) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; len];
        let n = nix::unistd::read(fd, &mut buf)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        buf.truncate(n);
        Ok::<Vec<u8>, std::io::Error>(buf)
    })
    .await
    .context("joining ep0 read task")?
    .context("read(2) of the ep0 data/status stage")
}

/// Complete a SETUP we do not otherwise answer, so ep0 is never left pending.
async fn ep0_complete_unhandled(fd: RawFd, is_in: bool, w_length: u16) -> Result<()> {
    if is_in {
        ep0_write_zlp(fd).await
    } else {
        // Drain any data stage, then (or directly, when wLength == 0) run the
        // status stage.
        ep0_read(fd, w_length as usize).await.map(|_| ())
    }
}

pub async fn handle_aoa_setup(ep0: &mut File) -> Result<()> {
    // Raw fd for the zero-length ep0 accesses tokio cannot express (see above).
    let fd: RawFd = ep0.as_raw_fd();
    info!("waiting for car AOA2 init (GET_PROTOCOL / SEND_STRING / START_ACCESSORY)...");

    // Each event: 8 bytes usb_ctrlrequest + 1 byte type + 3 bytes pad = 12 bytes
    let mut event = [0u8; 12];

    loop {
        ep0.read_exact(&mut event).await.context("reading FFS ep0 event")?;

        let event_type = event[8];
        debug!("FFS ep0 event type={event_type}");

        match event_type {
            FUNCTIONFS_BIND => info!("FFS: BIND"),
            FUNCTIONFS_UNBIND => {
                warn!("FFS: UNBIND during AOA setup — connection lost");
                anyhow::bail!("gadget unbound during AOA2 handshake");
            }
            FUNCTIONFS_ENABLE => info!("FFS: ENABLE — car selected configuration"),
            FUNCTIONFS_DISABLE => info!("FFS: DISABLE"),

            FUNCTIONFS_SETUP => {
                let b_request_type = event[0];
                let b_request = event[1];
                let w_length = u16::from_le_bytes([event[6], event[7]]);
                let is_vendor = (b_request_type & 0x60) == 0x40;
                let is_in = b_request_type & 0x80 != 0;

                debug!(
                    "FFS SETUP: type=0x{b_request_type:02x} req={b_request} len={w_length}"
                );

                if !is_vendor {
                    // Delivered because FUNCTIONFS_ALL_CTRL_RECIP is set. We do
                    // not implement it, but it still has to be completed: a
                    // wLength==0 SETUP is left in DELAYED_STATUS otherwise and
                    // ep0 wedges for the rest of the session.
                    debug!("non-vendor SETUP req={b_request} — completing without handling");
                    if let Err(e) = ep0_complete_unhandled(fd, is_in, w_length).await {
                        warn!("failed to complete unhandled SETUP req={b_request}: {e:#}");
                    }
                    continue;
                }

                match b_request {
                    AOA_GET_PROTOCOL => {
                        // IN request — respond with supported AOA version
                        info!("← AOA GET_PROTOCOL → version {AOA_VERSION}");
                        ep0.write_all(&AOA_VERSION.to_le_bytes())
                            .await
                            .context("responding to GET_PROTOCOL")?;
                    }

                    AOA_SEND_STRING => {
                        // OUT request — host sends an identification string; consume it
                        // One read(2), sized from wLength. With wLength > 0 the
                        // kernel returns 0 (not DELAYED_STATUS) and this data
                        // stage completes the transfer.
                        let buf = ep0_read(fd, w_length as usize)
                            .await
                            .context("reading SEND_STRING data")?;
                        debug!("← AOA SEND_STRING: {:?}", String::from_utf8_lossy(&buf));
                    }

                    AOA_START_ACCESSORY => {
                        // OUT, wLength == 0 — the kernel returned
                        // USB_GADGET_DELAYED_STATUS, so the status stage only
                        // runs once we issue the zero-length read. Skipping it
                        // leaves the host's control transfer hanging until the
                        // gadget is torn down.
                        info!("← AOA START_ACCESSORY — switching to PID 0x2D00");
                        ep0_read(fd, 0)
                            .await
                            .context("completing START_ACCESSORY status stage")?;
                        return Ok(());
                    }

                    _ => {
                        warn!("unknown AOA vendor request {b_request} (type=0x{b_request_type:02x})");
                        // Must still be completed in both directions — see
                        // ep0_complete_unhandled. `write_all(&[])` would NOT do
                        // it: tokio short-circuits on an empty slice.
                        if let Err(e) = ep0_complete_unhandled(fd, is_in, w_length).await {
                            warn!("failed to complete unknown request {b_request}: {e:#}");
                        }
                    }
                }
            }

            _ => debug!("FFS: unknown event type {event_type}"),
        }
    }
}

/// Wait for the ENABLE event after gadget::switch_to_accessory_mode().
/// The car reconnects and selects the configuration before bulk I/O is possible.
pub async fn wait_for_enable(ep0: &mut File) -> Result<()> {
    info!("waiting for car to reconnect (PID 0x2D00 enumeration)...");
    let mut event = [0u8; 12];
    loop {
        ep0.read_exact(&mut event).await.context("reading FFS ep0 event")?;
        match event[8] {
            FUNCTIONFS_BIND => info!("FFS: BIND (PID 0x2D00)"),
            FUNCTIONFS_ENABLE => {
                info!("FFS: ENABLE — car ready for AA protocol");
                return Ok(());
            }
            FUNCTIONFS_DISABLE => info!("FFS: DISABLE"),
            FUNCTIONFS_UNBIND => warn!("FFS: UNBIND while waiting for enable"),
            t => debug!("FFS: event type={t} (waiting for enable)"),
        }
    }
}

/// Open FunctionFS bulk endpoints after the function is enabled.
/// Returns `(ep1_out, ep2_in)`: ep1 for reading (car→device), ep2 for writing (device→car).
pub async fn open_endpoints() -> Result<(File, File)> {
    let ep1 = format!("{FFS_MOUNT}/ep1");
    let ep2 = format!("{FFS_MOUNT}/ep2");

    // ep files exist from mount time but may not be openable until ENABLE
    for attempt in 0..20 {
        if std::path::Path::new(&ep1).exists() && std::path::Path::new(&ep2).exists() {
            break;
        }
        if attempt == 19 {
            anyhow::bail!("FFS endpoint files did not appear at {FFS_MOUNT}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let ep1_file = OpenOptions::new()
        .read(true)
        .open(&ep1)
        .await
        .context("opening FFS ep1 (bulk OUT)")?;

    let ep2_file = OpenOptions::new()
        .write(true)
        .open(&ep2)
        .await
        .context("opening FFS ep2 (bulk IN)")?;

    info!("FFS bulk endpoints opened — ready for AA protocol");
    Ok((ep1_file, ep2_file))
}
