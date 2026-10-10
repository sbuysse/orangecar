//! USB host side: find the phone, switch it to Android Open Accessory mode,
//! and move bytes on its bulk endpoints.
//!
//! Unlike the prober's `usb.rs` (which only knows our bridge's Google IDs),
//! a real phone shows up with its OWN vendor id (Samsung 04e8, Google 18d1,
//! ...). As aasdk does (`USBHub` + `AccessoryModeQueryChain`), every candidate
//! device is asked AOA GET_PROTOCOL (vendor request 51); a phone answers a
//! version of at least 1, gets the six identification strings (exactly aasdk's, which Android
//! Auto recognises) and START (53), then re-enumerates as 18d1:2d00/2d01.
//!
//! Devices that never answer AOA are remembered by bus/address so the Pi's
//! own peripherals are not re-probed every second. The known on-board
//! devices (4G modem, Bluetooth, hubs) are never probed at all.

use anyhow::{anyhow, bail, Context, Result};
use nusb::transfer::{Control, ControlType, EndpointType, Queue, Recipient, RequestBuffer};
use nusb::{DeviceInfo, Interface};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info};

const GOOGLE_VID: u16 = 0x18D1;
const PID_ACCESSORY: u16 = 0x2D00;
const PID_ACCESSORY_ADB: u16 = 0x2D01;

const AOA_GET_PROTOCOL: u8 = 51;
const AOA_SEND_STRING: u8 = 52;
const AOA_START: u8 = 53;

/// aasdk `AccessoryModeQueryFactory.cpp`, index = AOA string id.
const AOA_STRINGS: [&str; 6] = ["Android", "Android Auto", "Android Auto", "2.0.1", "https://f1xstudio.com", "HU-AAAAAA001"];

/// Vendors never probed: root hubs, the 4G modem (SIM7600, 1e0e), the
/// Bluetooth dongles (Foxconn 0489, TP-Link 2357), Genesys hubs (05e3).
pub const DEFAULT_SKIP_VIDS: &[u16] = &[0x1d6b, 0x1e0e, 0x0489, 0x2357, 0x05e3];

const CTRL_TIMEOUT: Duration = Duration::from_secs(1);

pub fn is_accessory(d: &DeviceInfo) -> bool {
    d.vendor_id() == GOOGLE_VID && matches!(d.product_id(), PID_ACCESSORY | PID_ACCESSORY_ADB)
}

fn describe(d: &DeviceInfo) -> String {
    format!(
        "{:04x}:{:04x} bus {} addr {} ({} {})",
        d.vendor_id(),
        d.product_id(),
        d.bus_number(),
        d.device_address(),
        d.manufacturer_string().unwrap_or("?"),
        d.product_string().unwrap_or("?")
    )
}

type DevKey = (u8, u8, u16, u16);

/// A probe that failed with an ERROR (not a clean "no AOA") is retried: the
/// first scan can hit a freshly plugged phone whose device node is still
/// root:root (udev sets plugdev a few ms later), or a phone not ready to answer
/// control transfers yet. 2026-10-05 such a phone was rejected for good and
/// Android Auto never started until it was re-plugged.
const PROBE_RETRY_EVERY: Duration = Duration::from_secs(3);
const PROBE_MAX_ERRORS: u32 = 5;

/// Remembers devices that did not speak AOA.
#[derive(Default)]
pub struct Prober {
    rejected: HashSet<DevKey>,
    errors: HashMap<DevKey, (u32, Instant)>,
    pub skip_vids: Vec<u16>,
}

pub enum Found {
    /// A phone already in accessory mode: open it.
    Accessory(DeviceInfo),
    /// A phone was just told to switch; it will re-enumerate shortly.
    Switching,
    Nothing,
}

impl Prober {
    pub fn new(skip_vids: Vec<u16>) -> Self {
        Self { rejected: HashSet::new(), errors: HashMap::new(), skip_vids }
    }

    /// One scan of the bus. Blocking (control transfers): call from
    /// `spawn_blocking`.
    pub fn scan(&mut self) -> Result<Found> {
        let devices: Vec<DeviceInfo> = nusb::list_devices().context("listing USB devices")?.collect();
        if let Some(d) = devices.iter().find(|d| is_accessory(d)) {
            return Ok(Found::Accessory(d.clone()));
        }
        // Forget rejections of devices that are gone (a phone re-plugged at
        // the same address gets a fresh try).
        let present: HashSet<_> =
            devices.iter().map(|d| (d.bus_number(), d.device_address(), d.vendor_id(), d.product_id())).collect();
        self.rejected.retain(|k| present.contains(k));
        self.errors.retain(|k, _| present.contains(k));

        for d in &devices {
            let key = (d.bus_number(), d.device_address(), d.vendor_id(), d.product_id());
            if d.class() == 0x09 || self.skip_vids.contains(&d.vendor_id()) || self.rejected.contains(&key) {
                continue;
            }
            if let Some((_, last)) = self.errors.get(&key) {
                if last.elapsed() < PROBE_RETRY_EVERY {
                    continue;
                }
            }
            match try_switch(d) {
                Ok(true) => return Ok(Found::Switching),
                Ok(false) => {
                    self.rejected.insert(key);
                }
                Err(e) => {
                    let n = self.errors.get(&key).map_or(0, |(n, _)| *n) + 1;
                    if n >= PROBE_MAX_ERRORS {
                        info!("aa-usb: {} gave up after {n} failed probes: {e:#}", describe(d));
                        self.errors.remove(&key);
                        self.rejected.insert(key);
                    } else {
                        debug!("aa-usb: {} probe failed ({n}/{PROBE_MAX_ERRORS}), retrying: {e:#}", describe(d));
                        self.errors.insert(key, (n, Instant::now()));
                    }
                }
            }
        }
        Ok(Found::Nothing)
    }
}

/// GET_PROTOCOL → strings → START. `Ok(false)` = answers but AOA < 1.
fn try_switch(d: &DeviceInfo) -> Result<bool> {
    let dev = d.open().with_context(|| format!("opening {} (udev rule / permissions?)", describe(d)))?;
    let vendor = |request: u8, index: u16| Control {
        control_type: ControlType::Vendor,
        recipient: Recipient::Device,
        request,
        value: 0,
        index,
    };
    let mut buf = [0u8; 2];
    let n = dev.control_in_blocking(vendor(AOA_GET_PROTOCOL, 0), &mut buf, CTRL_TIMEOUT)?;
    if n < 2 {
        return Ok(false);
    }
    let proto = u16::from_le_bytes(buf);
    if proto < 1 {
        return Ok(false);
    }
    info!("aa-usb: {} speaks AOA {proto} — switching to accessory mode", describe(d));
    for (id, s) in AOA_STRINGS.iter().enumerate() {
        let mut z = s.as_bytes().to_vec();
        z.push(0);
        dev.control_out_blocking(vendor(AOA_SEND_STRING, id as u16), &z, CTRL_TIMEOUT)
            .with_context(|| format!("AOA SEND_STRING {id}"))?;
    }
    dev.control_out_blocking(vendor(AOA_START, 0), &[], CTRL_TIMEOUT).context("AOA START")?;
    Ok(true)
}

/// Bulk link to a phone in accessory mode.
pub struct AccessoryLink {
    pub iface: Interface,
    pub ep_in: u8,
    pub ep_out: u8,
    pub name: String,
}

impl AccessoryLink {
    pub fn open(d: &DeviceInfo) -> Result<Self> {
        let dev = d.open().with_context(|| format!("opening accessory {}", describe(d)))?;
        let cfg = dev.active_configuration().context("no active configuration")?;
        // Interface 0 is the accessory interface (2d01 adds ADB as interface 1).
        let alt = cfg
            .interface_alt_settings()
            .find(|i| i.interface_number() == 0)
            .ok_or_else(|| anyhow!("accessory has no interface 0"))?;
        let mut ep_in = None;
        let mut ep_out = None;
        for ep in alt.endpoints() {
            if ep.transfer_type() == EndpointType::Bulk {
                if ep.address() & 0x80 != 0 {
                    ep_in = Some(ep.address());
                } else {
                    ep_out = Some(ep.address());
                }
            }
        }
        let (Some(ep_in), Some(ep_out)) = (ep_in, ep_out) else {
            bail!("accessory interface 0 lacks bulk IN/OUT endpoints");
        };
        let iface = dev.detach_and_claim_interface(0).context("claiming accessory interface 0")?;
        info!("aa-usb: accessory open, {} ep_in 0x{ep_in:02x} ep_out 0x{ep_out:02x}", describe(d));
        Ok(Self { iface, ep_in, ep_out, name: describe(d) })
    }

    /// A read queue with several transfers in flight, so the phone never
    /// waits for us to re-submit (and no read is ever cancelled mid-transfer,
    /// the bug the prober works around in `stream_video_chan`).
    pub fn reader(&self) -> Queue<RequestBuffer> {
        let mut q = self.iface.bulk_in_queue(self.ep_in);
        for _ in 0..4 {
            q.submit(RequestBuffer::new(64 * 1024));
        }
        q
    }

    pub async fn write(&self, data: Vec<u8>) -> Result<()> {
        self.iface.bulk_out(self.ep_out, data).await.into_result().context("USB bulk write")?;
        Ok(())
    }
}

/// Next completed read from `q` (re-submits a buffer each time).
pub async fn read(q: &mut Queue<RequestBuffer>) -> Result<Vec<u8>> {
    let c = q.next_complete().await;
    let data = c.data;
    c.status.map_err(|e| anyhow!("USB bulk read: {e}"))?;
    q.submit(RequestBuffer::new(64 * 1024));
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aoa_strings_match_aasdk() {
        // Order = AOA string ids: manufacturer, model, description, version, uri, serial.
        assert_eq!(AOA_STRINGS[0], "Android");
        assert_eq!(AOA_STRINGS[1], "Android Auto");
        assert_eq!(AOA_STRINGS[5], "HU-AAAAAA001");
    }

    #[test]
    fn onboard_devices_are_skipped() {
        // lsusb on the Pi (2026-10-05): modem, BT dongles, hubs.
        for vid in [0x1e0e, 0x0489, 0x2357, 0x05e3, 0x1d6b] {
            assert!(DEFAULT_SKIP_VIDS.contains(&vid));
        }
        assert!(!DEFAULT_SKIP_VIDS.contains(&0x04e8)); // Samsung phones must be probed
        assert!(!DEFAULT_SKIP_VIDS.contains(&0x18d1)); // Pixel phones too
    }
}
