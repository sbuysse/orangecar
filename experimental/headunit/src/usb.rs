//! USB host transport for Android Auto.
//!
//! Phase 0 — AOA2 init:
//!   Pi starts as VID=0x18D1 PID=0x4EE1 (Nexus generic).
//!   We send USB control requests to trigger the f_accessory kernel module
//!   to re-enumerate as VID=0x18D1 PID=0x2D00 (Android Auto accessory).
//!
//! Phase 1 — AA protocol:
//!   Open bulk IN/OUT endpoints on 0x2D00 and implement the Transport trait.

use anyhow::{bail, Context, Result};
use nusb::transfer::{Control, ControlType, EndpointType, Recipient, RequestBuffer};
use nusb::{Device, DeviceInfo, Interface};
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

const GOOGLE_VID: u16 = 0x18D1;
const PID_NEXUS: u16 = 0x4EE1; // Pre-accessory mode (before AOA2 init)
const PID_AA: u16 = 0x2D00; // Android Auto accessory mode

// AOA2 vendor requests
const AOA_GET_PROTOCOL: u8 = 51;
const AOA_SEND_STRING: u8 = 52;
const AOA_START_ACCESSORY: u8 = 53;

// AOA2 string IDs
const AOA_STR_MANUFACTURER: u16 = 0;
const AOA_STR_MODEL: u16 = 1;
const AOA_STR_DESCRIPTION: u16 = 2;
const AOA_STR_VERSION: u16 = 3;
const AOA_STR_URI: u16 = 4;
const AOA_STR_SERIAL: u16 = 5;

pub struct UsbTransport {
    // Keep the device alive so the interface remains valid.
    _device: Device,
    iface: Interface,
    ep_in: u8,
    ep_out: u8,
}

impl UsbTransport {
    /// Open the Pi's USB gadget. Performs AOA2 init if needed.
    /// Returns (UsbTransport, aoa2_init_performed).
    pub async fn open() -> Result<(Self, bool)> {
        // If device is already in AA mode (0x2D00) from a previous session,
        // perform a USB reset to force re-enumeration. The bridge's FFS
        // endpoints were re-opened when it detected the disconnect, so our
        // old handles are stale. A reset makes the kernel drop and re-probe
        // the device, giving us fresh endpoints.
        if let Some(info) = find_device(PID_AA) {
            info!("device in stale AA mode (0x2D00) — resetting USB to force re-enumerate");
            if let Ok(device) = info.open() {
                let _ = device.reset();
            }
            drop(info);
            // Wait for the device to re-appear — bridge needs time to
            // detect the disconnect and recreate the gadget.
            info!("waiting for device to re-appear after reset...");
            for attempt in 0..50 {
                sleep(Duration::from_millis(200)).await;
                if let Some(info) = find_device(PID_NEXUS) {
                    info!("device re-appeared as 0x4EE1 after {}ms", (attempt + 1) * 200);
                    // Found in pre-accessory mode — do AOA2 init
                    aoa2_init_async(info)
                        .await
                        .context("AOA2 init failed after reset")?;
                    info!("waiting for device to re-enumerate as 0x2D00 ...");
                    for _ in 0..30 {
                        sleep(Duration::from_millis(100)).await;
                        if let Some(info) = find_device(PID_AA) {
                            info!("device re-enumerated as 0x2D00");
                            let transport = open_aa_device(info)?;
                            return Ok((transport, true));
                        }
                    }
                    bail!("device did not re-enumerate as 0x2D00 after reset");
                }
                if let Some(info) = find_device(PID_AA) {
                    info!("device re-appeared as 0x2D00 after {}ms", (attempt + 1) * 200);
                    let transport = open_aa_device(info)?;
                    return Ok((transport, false));
                }
            }
            bail!("device did not re-appear after USB reset — is the bridge running?");
        }

        // Device is in pre-accessory mode — perform AOA2 init
        if let Some(info) = find_device(PID_NEXUS) {
            info!("found device in Nexus mode (0x4EE1), starting AOA2 init");
            aoa2_init_async(info).await.context("AOA2 init failed")?;

            // Wait for re-enumeration (up to 3s)
            info!("waiting for device to re-enumerate as 0x2D00 ...");
            for _ in 0..30 {
                sleep(Duration::from_millis(100)).await;
                if let Some(info) = find_device(PID_AA) {
                    info!("device re-enumerated as 0x2D00");
                    let transport = open_aa_device(info)?;
                    return Ok((transport, true));
                }
            }
            bail!("device did not re-enumerate as 0x2D00 within 3s");
        }

        bail!(
            "no RasCar device found (expected VID=0x{GOOGLE_VID:04x} PID=0x{PID_NEXUS:04x} or \
             0x{PID_AA:04x}). Is the bridge running on the Pi? Is the USB cable connected?"
        );
    }

    pub fn ep_in(&self) -> u8 {
        self.ep_in
    }

    pub fn ep_out(&self) -> u8 {
        self.ep_out
    }
}

impl crate::handshake::Transport for UsbTransport {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.iface
            .bulk_out(self.ep_out, data.to_vec())
            .await
            .into_result()
            .context("USB bulk write failed")?;
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let result = self
            .iface
            .bulk_in(self.ep_in, RequestBuffer::new(buf.len()))
            .await
            .into_result()
            .context("USB bulk read failed")?;
        // Truncation here silently drops bytes off the end of a transfer,
        // which corrupts frame alignment for everything that follows. The
        // caller sizes its buffer to MAX_FRAME_PAYLOAD + frame header, so this
        // should never trigger — but make it loud rather than silent if it does.
        if result.len() > buf.len() {
            bail!(
                "USB transfer {} bytes exceeds read buffer {} — bytes would be truncated",
                result.len(),
                buf.len()
            );
        }
        debug_assert!(result.len() <= buf.len(), "USB read truncation");
        let n = result.len();
        buf[..n].copy_from_slice(&result[..n]);
        Ok(n)
    }
}

fn find_device(pid: u16) -> Option<DeviceInfo> {
    nusb::list_devices()
        .ok()?
        .find(|d| d.vendor_id() == GOOGLE_VID && d.product_id() == pid)
}

fn open_aa_device(info: DeviceInfo) -> Result<UsbTransport> {
    let device = info
        .open()
        .context("failed to open USB device — are udev rules set up?")?;

    // Find bulk IN and OUT endpoints on interface 0 before claiming.
    let (ep_in, ep_out) = find_bulk_endpoints(&device)?;

    let iface = device
        .detach_and_claim_interface(0)
        .context("failed to claim interface 0")?;

    info!("USB opened: ep_in=0x{ep_in:02x} ep_out=0x{ep_out:02x}");

    Ok(UsbTransport {
        _device: device,
        iface,
        ep_in,
        ep_out,
    })
}

fn find_bulk_endpoints(device: &Device) -> Result<(u8, u8)> {
    let config = device
        .active_configuration()
        .context("no active USB configuration")?;

    let iface_alt = config
        .interface_alt_settings()
        .find(|i| i.interface_number() == 0)
        .context("interface 0 not found in active configuration")?;

    let mut ep_in: Option<u8> = None;
    let mut ep_out: Option<u8> = None;

    for ep in iface_alt.endpoints() {
        if ep.transfer_type() == EndpointType::Bulk {
            if ep.address() & 0x80 != 0 {
                ep_in = Some(ep.address());
            } else {
                ep_out = Some(ep.address());
            }
        }
    }

    match (ep_in, ep_out) {
        (Some(i), Some(o)) => Ok((i, o)),
        _ => bail!("could not find bulk IN and OUT endpoints on interface 0"),
    }
}

/// Run `aoa2_init` off the async runtime.
///
/// It issues 8 *blocking* control transfers of up to 2s each. Called directly
/// from an `async fn`, a wedged gadget holds a tokio worker thread for up to
/// 16s and the surrounding `tokio::time::timeout` can never fire, because the
/// future never yields.
async fn aoa2_init_async(info: DeviceInfo) -> Result<()> {
    tokio::task::spawn_blocking(move || aoa2_init(&info))
        .await
        .context("AOA2 init task panicked")?
}

/// Send AOA2 USB control requests to trigger f_accessory re-enumeration.
fn aoa2_init(info: &DeviceInfo) -> Result<()> {
    let device = info
        .open()
        .context("failed to open device for AOA2 init")?;

    // GET_PROTOCOL — must return ≥ 1
    let protocol = {
        let mut buf = [0u8; 2];
        device
            .control_in_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: AOA_GET_PROTOCOL,
                    value: 0,
                    index: 0,
                },
                &mut buf,
                Duration::from_secs(2),
            )
            .context("GET_PROTOCOL failed")?;
        u16::from_le_bytes(buf)
    };

    if protocol < 1 {
        bail!("device reports AOA protocol version {protocol} — expected ≥ 1");
    }
    info!("AOA protocol version: {protocol}");

    // SEND_STRING for each required field
    let strings: &[(u16, &str)] = &[
        (AOA_STR_MANUFACTURER, "RasCar"),
        (AOA_STR_MODEL, "RasCar Head Unit Prober"),
        (AOA_STR_DESCRIPTION, "AA protocol conformance prober"),
        (AOA_STR_VERSION, "1.0"),
        (AOA_STR_URI, "https://github.com/rascar"),
        (AOA_STR_SERIAL, "RASCAR-PROBER-1"),
    ];

    for &(id, s) in strings {
        let mut payload = s.as_bytes().to_vec();
        // Include null terminator as required by AOA2 spec
        payload.push(0);
        device
            .control_out_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: AOA_SEND_STRING,
                    value: 0,
                    index: id,
                },
                &payload,
                Duration::from_secs(2),
            )
            .with_context(|| format!("SEND_STRING({id}) failed"))?;
    }

    // START_ACCESSORY — triggers re-enumeration
    device
        .control_out_blocking(
            Control {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOA_START_ACCESSORY,
                value: 0,
                index: 0,
            },
            &[],
            Duration::from_secs(2),
        )
        .context("START_ACCESSORY failed")?;

    info!("AOA2 init complete — device will re-enumerate");
    Ok(())
}
