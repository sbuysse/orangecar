//! Host-side USB: discover the car (now a USB **device**) and expose its iAP2
//! vendor bulk interface as a `tokio::io::AsyncRead + AsyncWrite` transport that
//! Phase 2's [`crate::iap2::Iap2Device`] consumes.
//!
//! After [`super::role_switch::switch_to_host`] flips `fc000000.usb` to host
//! mode, the car enumerates on our bus exposing (at least):
//!   * a **vendor-specific** interface (class `0xFF`) carrying **iAP2** over a
//!     bulk IN + bulk OUT endpoint pair, and
//!   * a **CDC-NCM** interface (Communications class `0x02` / subclass `0x0D`)
//!     for the AirPlay network tunnel (handled in [`super::ncm`]).
//!
//! We use **`nusb`** (pure-Rust, async, tokio feature) rather than libusb.
//!
//! # ⚠ Hardware verification required
//! - `// TODO(hw): the car's VID/PID are unknown` — we identify the device
//!   *structurally* (a device that has BOTH a vendor 0xFF interface and a CDC
//!   interface).  Verify with `lsusb -v` once the car enumerates and pin the
//!   real VID/PID if the heuristic is ambiguous.
//! - `// TODO(hw): confirm the iAP2 interface is class 0xFF` (vs a CDC/other
//!   subclass) and that its two bulk endpoints are the iAP2 link (not NCM data).
//! - `// TODO(hw): confirm bulk max-packet.` The alt-setting question is now
//!   handled: [`CarDevice::iap2_alt`] carries the alternate setting the bulk
//!   pair was found on, and [`UsbHostTransport::open`] selects it before opening
//!   the endpoints (the common Apple layout is alt 0 with no endpoints, alt 1
//!   with the bulk pair).

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{debug, info, warn};

use nusb::descriptors::TransferType;
use nusb::io::{EndpointRead, EndpointWrite};
use nusb::transfer::{Bulk, In, Out};
use nusb::{Device, DeviceInfo, Interface};

/// USB Communications Device Class (CDC) — the NCM interface's class.
const CLASS_CDC_COMM: u8 = 0x02;
/// CDC subclass for Network Control Model (NCM).
const CDC_SUBCLASS_NCM: u8 = 0x0D;
/// Vendor-specific interface class (the iAP2 bulk interface).
const CLASS_VENDOR: u8 = 0xFF;

/// Per-transfer buffer size for the buffered endpoint reader/writer.
const TRANSFER_BUFFER: usize = 16 * 1024;

/// Bytes of each bulk transfer shown in a wire-trace dump.
const WIRE_DUMP_MAX: usize = 256;

/// A discovered car USB device with its resolved iAP2 + (optional) NCM interfaces.
pub struct CarDevice {
    pub device: Device,
    pub info: DeviceInfo,
    /// bInterfaceNumber of the vendor (iAP2) interface.
    pub iap2_interface: u8,
    /// bAlternateSetting the iAP2 bulk pair was found on. Endpoints resolve
    /// against the *currently selected* alt setting, so this must be selected
    /// before opening them when it is not 0.
    pub iap2_alt: u8,
    /// Bulk IN endpoint address of the iAP2 interface (car → us).
    pub iap2_ep_in: u8,
    /// Bulk OUT endpoint address of the iAP2 interface (us → car).
    pub iap2_ep_out: u8,
    /// bInterfaceNumber of the CDC-NCM interface, if present.
    pub ncm_interface: Option<u8>,
}

impl std::fmt::Debug for CarDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarDevice")
            .field("vid", &format_args!("0x{:04x}", self.info.vendor_id()))
            .field("pid", &format_args!("0x{:04x}", self.info.product_id()))
            .field("iap2_interface", &self.iap2_interface)
            .field("iap2_alt", &self.iap2_alt)
            .field("iap2_ep_in", &format_args!("0x{:02x}", self.iap2_ep_in))
            .field("iap2_ep_out", &format_args!("0x{:02x}", self.iap2_ep_out))
            .field("ncm_interface", &self.ncm_interface)
            .finish()
    }
}

/// Poll the host USB bus until the car appears (or `timeout` elapses).
///
/// We re-scan every `poll_interval` because the car may take a moment to
/// enumerate after the role switch.
pub async fn wait_for_car_device(timeout: Duration, poll_interval: Duration) -> Result<CarDevice> {
    info!(
        "usb-host: waiting up to {:?} for the car to enumerate as a USB device (polling every {:?}, t+{} ms)",
        timeout,
        poll_interval,
        crate::debug::elapsed_ms()
    );
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let mut attempt: u64 = 0;
    // Devices already described in the log, so each is dumped once, not per poll.
    let mut seen: HashSet<String> = HashSet::new();

    loop {
        attempt += 1;
        match scan_once(&mut seen).await {
            Ok(Some(car)) => {
                info!(
                    "usb-host: car device found on attempt #{attempt} after {} ms: {car:?}",
                    started.elapsed().as_millis()
                );
                return Ok(car);
            }
            Ok(None) => debug!(
                "usb-host: scan #{attempt} ({} ms elapsed) — no matching car device yet",
                started.elapsed().as_millis()
            ),
            Err(e) => warn!(
                "usb-host: scan #{attempt} ({} ms elapsed) error: {e:#}",
                started.elapsed().as_millis()
            ),
        }
        // One role snapshot shortly after the switch: the kernel applies role
        // changes asynchronously, so this shows whether the port really moved.
        if attempt == 2 {
            super::role_switch::log_role_snapshot("second car-wait poll, shortly after the role switch");
        }

        if tokio::time::Instant::now() >= deadline {
            warn!(
                "usb-host: giving up after {attempt} scan(s) / {} ms; {} distinct USB device(s) were seen (see debug inventory)",
                started.elapsed().as_millis(),
                seen.len()
            );
            bail!(
                "usb-host: timed out after {:?} waiting for the car USB device. \
                 // TODO(hw): confirm the role switch actually took effect and run \
                 `lsusb -v` to see what the car enumerated as.",
                timeout
            );
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// One enumeration pass: return the first device that looks like the car.
async fn scan_once(seen: &mut HashSet<String>) -> Result<Option<CarDevice>> {
    let devices: Vec<DeviceInfo> = nusb::list_devices()
        .await
        .context("nusb::list_devices")?
        .collect();
    debug!("usb-host: {} USB device(s) on the bus", devices.len());

    for info in devices {
        let key = format!(
            "{}-{}:{:04x}:{:04x}",
            info.busnum(),
            info.device_address(),
            info.vendor_id(),
            info.product_id()
        );
        if seen.insert(key) {
            debug!(
                "usb-host: NEW device (t+{} ms): {}",
                crate::debug::elapsed_ms(),
                describe_device(&info)
            );
        }

        // Only the port we just switched to host mode can carry the car. Any
        // other bus may hold a device that also has a vendor bulk pair (the
        // ASRMicro 4G stick 1286:4e31 on usb3 matched and was claimed as "the
        // car", 2 ms after the switch, on every run of the 2026-09-29 drive).
        if !on_role_switched_controller(info.sysfs_path()) {
            continue;
        }

        // Cheap pre-filter on the cached interface list: must expose a vendor
        // 0xFF interface (the iAP2 candidate). CDC-NCM is preferred but optional
        // at this stage (some cars expose NCM only after iAP2 negotiation).
        let has_vendor = info.interfaces().any(|i| i.class() == CLASS_VENDOR);
        if !has_vendor {
            continue;
        }

        match inspect_device(&info).await {
            Ok(Some(car)) => return Ok(Some(car)),
            Ok(None) => debug!("usb-host: vendor-bearing device did not resolve iAP2 endpoints, skipping"),
            Err(e) => warn!("usb-host: failed to inspect candidate: {e:#}"),
        }
    }
    Ok(None)
}

/// True when `sysfs` is a device below the role-switched dwc3 controller
/// (`/sys/devices/platform/fc000000.usb/xhci-hcd.N.auto/usbX/...`).
fn on_role_switched_controller(sysfs: &Path) -> bool {
    sysfs
        .components()
        .any(|c| c.as_os_str() == super::role_switch::UDC_NAME)
}

/// Multi-line inventory of one enumerated device: identity, class, and each
/// interface with the endpoints sysfs reports for its active alt setting.
fn describe_device(info: &DeviceInfo) -> String {
    let mut out = format!(
        "bus {} addr {} vid:pid={:04x}:{:04x} class=0x{:02x}/0x{:02x}/0x{:02x} usb=0x{:04x} speed={:?} \
         manufacturer={:?} product={:?} serial={:?} sysfs={}",
        info.busnum(),
        info.device_address(),
        info.vendor_id(),
        info.product_id(),
        info.class(),
        info.subclass(),
        info.protocol(),
        info.usb_version(),
        info.speed(),
        info.manufacturer_string(),
        info.product_string(),
        info.serial_number(),
        info.sysfs_path().display()
    );
    let mut any = false;
    for iface in info.interfaces() {
        any = true;
        out.push_str(&format!(
            "\n    interface #{} class=0x{:02x} subclass=0x{:02x} protocol=0x{:02x} name={:?} endpoints=[{}]",
            iface.interface_number(),
            iface.class(),
            iface.subclass(),
            iface.protocol(),
            iface.interface_string(),
            sysfs_endpoints(info.sysfs_path(), iface.interface_number())
        ));
    }
    if !any {
        out.push_str("\n    <no interfaces reported>");
    }
    out
}

/// Endpoints of interface `ifnum` as sysfs exposes them (active alt setting
/// only): `ep 0x81 Bulk mps=512`. Best-effort; never fails.
fn sysfs_endpoints(dev: &Path, ifnum: u8) -> String {
    use crate::debug::read_attr;
    let Ok(rd) = std::fs::read_dir(dev) else {
        return "<sysfs unreadable>".to_string();
    };
    let mut eps: Vec<String> = Vec::new();
    for e in rd.flatten() {
        // Interface directories are named `<dev>:<config>.<ifnum>`.
        if !e.file_name().to_string_lossy().contains(':') {
            continue;
        }
        let dir = e.path();
        let num = u8::from_str_radix(&read_attr(dir.join("bInterfaceNumber")), 16).ok();
        if num != Some(ifnum) {
            continue;
        }
        let alt = read_attr(dir.join("bAlternateSetting"));
        let Ok(inner) = std::fs::read_dir(&dir) else { continue };
        let mut list: Vec<String> = inner
            .flatten()
            .filter(|c| c.file_name().to_string_lossy().starts_with("ep_"))
            .map(|c| {
                let p = c.path();
                format!(
                    "ep 0x{} {} mps={} (alt {alt})",
                    read_attr(p.join("bEndpointAddress")),
                    read_attr(p.join("type")),
                    read_attr(p.join("wMaxPacketSize"))
                )
            })
            .collect();
        list.sort();
        eps.extend(list);
    }
    if eps.is_empty() {
        "none in sysfs".to_string()
    } else {
        eps.join(", ")
    }
}

/// Open a candidate and resolve its iAP2 bulk endpoints + NCM interface from the
/// active configuration descriptor.
async fn inspect_device(info: &DeviceInfo) -> Result<Option<CarDevice>> {
    let device = info.open().await.context("opening car USB device")?;
    let config = device
        .active_configuration()
        .context("reading active configuration descriptor")?;

    // Full descriptor dump (every alt setting, not just the active one sysfs
    // shows) — the authoritative evidence for which alt carries the bulk pair.
    for iface in config.interface_alt_settings() {
        let eps: Vec<String> = iface
            .endpoints()
            .map(|ep| {
                format!(
                    "0x{:02x} {:?} mps={}",
                    ep.address(),
                    ep.transfer_type(),
                    ep.max_packet_size()
                )
            })
            .collect();
        debug!(
            "usb-host: {:04x}:{:04x} config descriptor: interface #{} alt {} class=0x{:02x}/0x{:02x}/0x{:02x} endpoints=[{}]",
            info.vendor_id(),
            info.product_id(),
            iface.interface_number(),
            iface.alternate_setting(),
            iface.class(),
            iface.subclass(),
            iface.protocol(),
            eps.join(", ")
        );
    }

    let mut iap2: Option<(u8, u8, u8, u8)> = None; // (iface, alt, ep_in, ep_out)
    let mut ncm_interface: Option<u8> = None;

    for iface in config.interface_alt_settings() {
        let (cls, sub, num) = (iface.class(), iface.subclass(), iface.interface_number());
        let alt = iface.alternate_setting();

        // CDC-NCM control interface.
        if cls == CLASS_CDC_COMM && sub == CDC_SUBCLASS_NCM {
            info!("usb-host: found CDC-NCM interface #{num} (class 0x{cls:02x}/0x{sub:02x})");
            ncm_interface.get_or_insert(num);
            continue;
        }
        // Some cars report NCM data as a plain communications/data interface;
        // record any CDC interface as a fallback NCM candidate.
        if cls == CLASS_CDC_COMM && ncm_interface.is_none() {
            debug!("usb-host: CDC interface #{num} (subclass 0x{sub:02x}) — NCM fallback candidate");
            ncm_interface = Some(num);
        }

        // Vendor interface → iAP2 bulk pair.
        if cls == CLASS_VENDOR && iap2.is_none() {
            let mut ep_in: Option<u8> = None;
            let mut ep_out: Option<u8> = None;
            for ep in iface.endpoints() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                let addr = ep.address();
                if addr & 0x80 != 0 {
                    ep_in.get_or_insert(addr);
                } else {
                    ep_out.get_or_insert(addr);
                }
            }
            match (ep_in, ep_out) {
                (Some(i), Some(o)) => {
                    info!(
                        "usb-host: found vendor iAP2 interface #{num} alt {alt} (class 0x{cls:02x}) \
                         bulk IN=0x{i:02x} OUT=0x{o:02x}"
                    );
                    iap2 = Some((num, alt, i, o));
                }
                _ => debug!(
                    "usb-host: vendor interface #{num} alt {alt} lacks a bulk IN+OUT pair (in={ep_in:?} out={ep_out:?})"
                ),
            }
        }
    }

    let Some((iap2_interface, iap2_alt, iap2_ep_in, iap2_ep_out)) = iap2 else {
        return Ok(None);
    };

    if ncm_interface.is_none() {
        warn!("usb-host: no CDC-NCM interface on this device yet — NCM bring-up may need to wait for iAP2 to complete. // TODO(hw): verify NCM appears pre- or post-iAP2.");
    }

    Ok(Some(CarDevice {
        device,
        info: info.clone(),
        iap2_interface,
        iap2_alt,
        iap2_ep_in,
        iap2_ep_out,
        ncm_interface,
    }))
}

/// Host-side USB bulk transport over the car's iAP2 vendor interface.
///
/// Implements `tokio::io::AsyncRead + AsyncWrite + Unpin` by delegating to
/// nusb's buffered [`EndpointRead`] / [`EndpointWrite`] (which, with nusb's
/// `tokio` feature, implement the matching tokio async traits).  This is exactly
/// what [`crate::iap2::Iap2Device::new`] wants for `T`.
pub struct UsbHostTransport {
    /// Kept alive so the interface claim (and thus the endpoints) stays valid.
    _interface: Interface,
    /// `Option` so [`UsbHostTransport::recover`] can take the `Endpoint` out of
    /// the buffered wrapper: nusb's `EndpointRead`/`EndpointWrite` own the
    /// `Endpoint`, and `Endpoint::clear_halt` needs `&mut Endpoint`, reachable
    /// only by consuming the wrapper with `into_inner`. Both are `Some` at all
    /// times except briefly inside `recover`; a poll that finds `None` (a
    /// recovery future dropped mid-flight) reports a broken pipe rather than
    /// panicking.
    reader: Option<EndpointRead<Bulk>>,
    writer: Option<EndpointWrite<Bulk>>,
    /// Endpoint addresses, for wire-trace labels and `clear_halt` logging.
    ep_in: u8,
    ep_out: u8,
}

impl UsbHostTransport {
    /// Claim the car's iAP2 vendor interface and wrap its bulk IN/OUT endpoints.
    pub async fn open(car: &CarDevice) -> Result<Self> {
        info!(
            "usb-host: claiming iAP2 interface #{} alt {} (bulk IN=0x{:02x} OUT=0x{:02x})",
            car.iap2_interface, car.iap2_alt, car.iap2_ep_in, car.iap2_ep_out
        );

        // detach_and_claim so we win over any kernel driver that grabbed the
        // vendor interface (no-op on non-Linux; Linux-only kernel detach).
        let interface = car
            .device
            .detach_and_claim_interface(car.iap2_interface)
            .await
            .with_context(|| format!("claiming iAP2 interface #{}", car.iap2_interface))?;

        // Claiming leaves alt 0 active, and `endpoint()` resolves against the
        // CURRENT alt setting. The Apple layout puts the bulk pair on alt 1 with
        // an empty alt 0, so the endpoints simply would not exist without this.
        if car.iap2_alt != 0 {
            interface
                .set_alt_setting(car.iap2_alt)
                .await
                .with_context(|| {
                    format!(
                        "selecting alt setting {} on iAP2 interface #{}",
                        car.iap2_alt, car.iap2_interface
                    )
                })?;
            info!(
                "usb-host: selected alt setting {} on interface #{}",
                car.iap2_alt, car.iap2_interface
            );
        }

        let ep_in = interface
            .endpoint::<Bulk, In>(car.iap2_ep_in)
            .with_context(|| format!("opening iAP2 bulk IN 0x{:02x}", car.iap2_ep_in))?;
        let ep_out = interface
            .endpoint::<Bulk, Out>(car.iap2_ep_out)
            .with_context(|| format!("opening iAP2 bulk OUT 0x{:02x}", car.iap2_ep_out))?;

        let reader = ep_in.reader(TRANSFER_BUFFER);
        let writer = ep_out.writer(TRANSFER_BUFFER);

        info!(
            "usb-host: iAP2 bulk transport ready (buffered, {TRANSFER_BUFFER} B/transfer, wire_trace={})",
            crate::debug::wire_trace()
        );
        Ok(UsbHostTransport {
            _interface: interface,
            reader: Some(reader),
            writer: Some(writer),
            ep_in: car.iap2_ep_in,
            ep_out: car.iap2_ep_out,
        })
    }
}

/// Broken-pipe error returned when a poll finds an endpoint wrapper `None`,
/// which only happens if a [`UsbHostTransport::recover`] future was dropped
/// between taking the wrapper out and putting it back.
fn recovering_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "iAP2 bulk endpoint unavailable (transport recovery interrupted)",
    )
}

// Delegate the tokio async traits to nusb's buffered endpoint wrappers. All
// fields are `Unpin`, so working through `self.get_mut()` is sound.
impl AsyncRead for UsbHostTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let ep_in = this.ep_in;
        let Some(reader) = this.reader.as_mut() else {
            return Poll::Ready(Err(recovering_error()));
        };
        let before = buf.filled().len();
        let poll = Pin::new(reader).poll_read(cx, buf);
        if crate::debug::wire_trace() {
            match &poll {
                Poll::Ready(Ok(())) => {
                    let got = &buf.filled()[before..];
                    debug!(
                        "usb bulk IN ep 0x{:02x} len={}:{}",
                        ep_in,
                        got.len(),
                        crate::debug::hex_dump(got, WIRE_DUMP_MAX)
                    );
                }
                Poll::Ready(Err(e)) => debug!("usb bulk IN ep 0x{:02x} error: {e}", ep_in),
                Poll::Pending => {}
            }
        }
        poll
    }
}

impl AsyncWrite for UsbHostTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let ep_out = this.ep_out;
        let Some(writer) = this.writer.as_mut() else {
            return Poll::Ready(Err(recovering_error()));
        };
        let poll = Pin::new(writer).poll_write(cx, buf);
        if crate::debug::wire_trace() {
            match &poll {
                Poll::Ready(Ok(n)) => debug!(
                    "usb bulk OUT ep 0x{:02x} len={} (of {} offered):{}",
                    ep_out,
                    n,
                    buf.len(),
                    crate::debug::hex_dump(&buf[..*n], WIRE_DUMP_MAX)
                ),
                Poll::Ready(Err(e)) => debug!("usb bulk OUT ep 0x{:02x} error: {e}", ep_out),
                Poll::Pending => {}
            }
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let Some(writer) = this.writer.as_mut() else {
            return Poll::Ready(Err(recovering_error()));
        };
        Pin::new(writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let Some(writer) = this.writer.as_mut() else {
            return Poll::Ready(Err(recovering_error()));
        };
        Pin::new(writer).poll_shutdown(cx)
    }
}

// After a transient bulk error (the car NAKing our OUT during a cold handshake —
// URB status `-71`/EPROTO, which nusb maps to `TransferError::Fault`, or a real
// stall/EPIPE mapped to `TransferError::Stall`), the endpoint may be halted, so
// a plain resend keeps failing. `recover` clears the halt on BOTH bulk endpoints
// and rebuilds the buffered wrappers with fresh transfer state, mirroring the
// proven kernel `g_iphone` driver, which calls `usb_clear_halt` on the RX and TX
// bulk pipes on `-EPIPE` and resubmits. This is the only path to `clear_halt`
// here: nusb 0.2.5 has `Endpoint::clear_halt` (not `Interface::clear_halt`), and
// the `Endpoint` is owned by the buffered `EndpointRead`/`EndpointWrite`, so it
// is reached by consuming the wrapper with `into_inner` and then re-wrapping.
#[async_trait::async_trait]
impl crate::iap2::Iap2Transport for UsbHostTransport {
    async fn recover(&mut self) {
        let (Some(reader), Some(writer)) = (self.reader.take(), self.writer.take()) else {
            warn!(
                "usb-host: cannot recover iAP2 endpoints — a prior recovery was interrupted and the endpoints are gone"
            );
            return;
        };
        let mut endpoint_in = reader.into_inner();
        let mut endpoint_out = writer.into_inner();

        // `clear_halt` should not run with transfers pending, so cancel any that
        // the buffered wrappers had queued. Leftover cancelled completions are
        // harmless: the rebuilt reader treats a `Cancelled` status as no error.
        endpoint_in.cancel_all();
        endpoint_out.cancel_all();

        info!(
            "usb-host: recovering iAP2 bulk endpoints — clear_halt on IN 0x{:02x} + OUT 0x{:02x} (t+{} ms)",
            self.ep_in,
            self.ep_out,
            crate::debug::elapsed_ms()
        );
        if let Err(e) = endpoint_out.clear_halt().await {
            warn!("usb-host: clear_halt on OUT 0x{:02x} failed: {e:#} (continuing)", self.ep_out);
        }
        if let Err(e) = endpoint_in.clear_halt().await {
            warn!("usb-host: clear_halt on IN 0x{:02x} failed: {e:#} (continuing)", self.ep_in);
        }

        // Rebuild the buffered wrappers around the (now un-halted) endpoints.
        self.reader = Some(endpoint_in.reader(TRANSFER_BUFFER));
        self.writer = Some(endpoint_out.writer(TRANSFER_BUFFER));
        info!("usb-host: iAP2 bulk endpoints recovered (wrappers rebuilt)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_role_switched_port_can_be_the_car() {
        // Where the car enumerated in every archived run.
        let car = Path::new("/sys/devices/platform/fc000000.usb/xhci-hcd.8.auto/usb7/7-1");
        // The 4G stick that was claimed as the car on 2026-09-29.
        let stick = Path::new("/sys/devices/platform/fc880000.usb/usb3/3-1");
        // A look-alike controller name must not match as a substring.
        let lookalike = Path::new("/sys/devices/platform/fc000000.usb2/usb9/9-1");
        assert!(on_role_switched_controller(car));
        assert!(!on_role_switched_controller(stick));
        assert!(!on_role_switched_controller(lookalike));
    }
}
