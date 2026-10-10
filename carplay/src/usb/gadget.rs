//! USB Gadget setup via configfs + FunctionFS — Apple-device identity.
//!
//! Configures the OrangePi 5 Plus USB-C OTG port to enumerate to the car head
//! unit as a real-iPhone-shaped **Apple iPhone**: four configurations, each backed
//! by its own FunctionFS instance, high speed.  The layout mirrors a
//! hardware-proven FunctionFS probe (four iPhone-shaped configurations, padded
//! string descriptors), which made the car send the CarPlay role-switch request.
//!
//! Uses its OWN gadget name (`carplay`) and FFS mounts (`/dev/ffs-carplay-cN`) so
//! it never collides with the `rascar` Android Auto gadget.
//!
//! Requires:
//!   - rk3588-dwc3-peripheral overlay (UDC `fc000000.usb`)
//!   - Kernel modules: libcomposite, usb_f_fs
//!   - Root privileges

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, warn};

use crate::usb::ffs;

const CONFIGFS: &str = "/sys/kernel/config/usb_gadget";
/// Distinct from the AA bridge's `rascar` gadget so both can coexist in configfs.
const GADGET_NAME: &str = "carplay";

// ── Apple device identity (identical to the probe) ────────────────────────────

/// Device descriptor attributes, written in this order.  No `max_speed` write:
/// the gadget must negotiate HIGH speed (FunctionFS carries FS + HS sets), and a
/// full-speed link never got the car past configuration selection.
const DEVICE_ATTRS: &[(&str, &str)] = &[
    ("idVendor", "0x05ac"),  // Apple, Inc.
    ("idProduct", "0x12a8"), // iPhone 5/5C/5S/6 (CarPlay era)
    // 0x1001, not 0x1106. Two independent dumps of a device this car accepts
    // agree: the CarlinKit CPC200 we captured on 2026-09-19 reports bcdDevice
    // 10.01, and afa008's published real-iPhone descriptor dump reports 0x1001.
    // We had 0x1106, which matches neither.
    ("bcdDevice", "0x1001"),
    ("bcdUSB", "0x0200"),
    // Class lives on the interfaces, like a real iPhone.
    ("bDeviceClass", "0x00"),
    ("bDeviceSubClass", "0x00"),
    ("bDeviceProtocol", "0x00"),
    ("bMaxPacketSize0", "0x40"),
];

/// Fallback iSerialNumber if /dev/urandom is somehow unreadable. Real value is
/// resolved at runtime by [`device_serial`].
const DEFAULT_SERIAL: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

const DEVICE_STRINGS: &[(&str, &str)] = &[
    ("manufacturer", "Apple Inc."),
    ("product", "iPhone"),
    // Placeholder — the real serial is substituted at write time from
    // `device_serial()` (persisted, per-identity). See that fn.
    ("serialnumber", DEFAULT_SERIAL),
];

/// True for a valid iPhone-style USB serial: exactly 40 hex characters.
fn is_valid_serial(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 40 hex chars from 20 bytes of OS randomness.
///
/// NOTE: an earlier comment here claimed this "matches the CarlinKit dongle".
/// It does not -- the captured CarlinKit serial is 17
/// characters. Left at 40 hex for now (the rotation below is what matters for
/// escaping a head-unit blacklist), but the length is NOT iPhone-verified.
fn random_serial() -> String {
    use std::io::Read;
    let mut buf = [0u8; 20];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf.iter().map(|b| format!("{b:02x}")).collect(),
        Err(e) => {
            warn!("/dev/urandom unreadable ({e}) — using the built-in fallback serial");
            DEFAULT_SERIAL.to_string()
        }
    }
}

/// The USB iSerialNumber presented to the car.
///
/// VID/PID are the fixed Apple iPhone values, so the *serial* is the only field
/// that distinguishes one iPhone from another. It used to be a single hardcoded
/// constant, which meant a head unit that latches a device into "don't offer
/// CarPlay" (after repeated failed/looping sessions — observed on the Jaguar:
/// it reads our role capability then never sends the 0x51 role switch) would
/// refuse us *forever*, always seeing the same blacklisted serial.
///
/// We now present a **persisted, per-identity** serial: a random 40-hex value
/// generated once and stored, so across boots the car pairs with us as ONE new
/// device (no accretion), but never as the burned constant. To escape a fresh
/// latch, delete the store file (a new serial is generated next run); pin one
/// explicitly with `RASCAR_USB_SERIAL`. Store path overridable via
/// `RASCAR_USB_SERIAL_STORE` (default `/var/lib/rascar/usb_serial`).
fn device_serial() -> String {
    if let Ok(s) = std::env::var("RASCAR_USB_SERIAL") {
        let s = s.trim().to_string();
        if is_valid_serial(&s) {
            info!("USB serial pinned via RASCAR_USB_SERIAL");
            return s;
        }
        warn!("RASCAR_USB_SERIAL={s:?} is not 40 hex chars — ignoring");
    }
    let store = std::env::var("RASCAR_USB_SERIAL_STORE")
        .unwrap_or_else(|_| "/var/lib/rascar/usb_serial".to_string());
    let store = Path::new(&store);
    if let Ok(s) = fs::read_to_string(store) {
        let s = s.trim().to_string();
        if is_valid_serial(&s) {
            info!("USB serial {s} (persisted, {})", store.display());
            return s;
        }
        warn!("{} held an invalid serial — rotating", store.display());
    }
    let serial = random_serial();
    if let Some(dir) = store.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match fs::write(store, &serial) {
        Ok(()) => info!("generated fresh USB serial {serial} → {}", store.display()),
        Err(e) => warn!(
            "could not persist USB serial to {} ({e}) — using it for this boot only",
            store.display()
        ),
    }
    serial
}

/// Extra device string so the configuration strings land on the same indices
/// as a real iPhone (config 1's name at index 5).  Needs a kernel with custom
/// configfs gadget strings; best effort.
const PAD_STRING_DIR: &str = "pad";
const PAD_STRING: &str = "Apple Mobile Device";

const CONFIG_MAX_POWER: &str = "500";

/// Configuration whose audio comes from the kernel's `f_uac1` ("iPod USB
/// Interface"). See `ffs::uac1_enabled` for why FunctionFS cannot carry it.
const UAC1_CONFIG: u8 = 2;
/// configfs instance suffix: `functions/uac1.<name>`.
const UAC1_INSTANCE: &str = "aud";
/// Mirrors the CPC200-CCPA's config-2 audio function: a stereo 16-bit capture
/// (gadget -> host, so an IN endpoint, as on the real device) offering the same
/// nine discrete sample rates. `p_chmask = 0` disables the playback direction so
/// no OUT endpoint appears -- the CCPA's config 2 has only the IN endpoint.
const UAC1_ATTRS: &[(&str, &str)] = &[
    ("c_chmask", "3"),
    ("c_ssize", "2"),
    ("c_srate", "8000,11025,12000,16000,22050,24000,32000,44100,48000"),
    ("p_chmask", "0"),
];
const CONFIG_ATTRIBUTES: &str = "0xc0";

fn gadget_path() -> PathBuf {
    Path::new(CONFIGFS).join(GADGET_NAME)
}

fn write_file(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).with_context(|| format!("writing {}", path.display()))?;
    debug!("wrote {} → {}", path.display(), value.trim());
    Ok(())
}

fn load_modules() -> Result<()> {
    for module in &["libcomposite", "usb_f_fs"] {
        let output = Command::new("modprobe")
            .arg(module)
            .output()
            .with_context(|| format!("running modprobe {module}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to load {module}: {stderr}");
        }
        info!("loaded module {module}");
    }
    Ok(())
}

/// Locate the USB Device Controller to bind to.
///
/// Collect and sort so selection is deterministic across boots.  On boards that
/// expose more than one UDC, blindly taking the first can bind the wrong
/// controller — warn instead of failing silently (kept from the AA bridge's
/// improved find_udc).
fn find_udc() -> Result<String> {
    let udc_dir = Path::new("/sys/class/udc");
    if !udc_dir.exists() {
        anyhow::bail!("no /sys/class/udc — is the dwc3 peripheral overlay loaded?");
    }
    let mut names: Vec<String> = fs::read_dir(udc_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let name = names.first().cloned().ok_or_else(|| {
        anyhow::anyhow!("no UDC available. Ensure the rk3588-dwc3-peripheral overlay is enabled")
    })?;
    if names.len() > 1 {
        warn!("multiple UDCs present {names:?} — using {name} (set the peripheral overlay if wrong)");
    }
    info!("found UDC: {name}");
    Ok(name)
}

/// Create the USB gadget (Apple device descriptor, four configurations, one
/// FunctionFS function per configuration) and mount every FunctionFS instance.
///
/// Present a DIFFERENT iPhone on the next attempt.
///
/// CarLinKit does exactly this: `_CheckConnectionErrorThenReboot` regenerates
/// `/etc/device_serial` from `/dev/urandom` before rebooting, so the head unit
/// sees a new phone on the retry. It is a strong hint that these units blacklist
/// a serial that failed — which fits our own observation that after a failed
/// attempt the car does not re-offer CarPlay for 30+ s.
///
/// Rotating the persisted serial costs us nothing (we re-create the gadget on
/// every attempt anyway) where CarLinKit needs a full reboot to apply it.
/// Skipped when the serial is pinned via `RASCAR_USB_SERIAL`, and disabled with
/// `RASCAR_ROTATE_SERIAL=0`.
pub fn rotate_serial_for_next_attempt() {
    if std::env::var("RASCAR_USB_SERIAL").is_ok() {
        debug!("serial rotation skipped: pinned via RASCAR_USB_SERIAL");
        return;
    }
    // DEFAULT OFF since 2026-09-23: rotating makes every attempt a brand-new
    // iPhone to the head unit, and the last configuration that visibly put a
    // dashboard on the car's screen (2026-09-22) kept one persisted serial. Its
    // effect is unmeasured, so it goes back to opt-in until it can be A/B'd alone.
    if !matches!(
        std::env::var("RASCAR_ROTATE_SERIAL").ok().as_deref().map(str::trim),
        Some("1") | Some("on") | Some("true") | Some("yes")
    ) {
        debug!("serial rotation off (opt in with RASCAR_ROTATE_SERIAL=1)");
        return;
    }
    let store = std::env::var("RASCAR_USB_SERIAL_STORE")
        .unwrap_or_else(|_| "/var/lib/rascar/usb_serial".to_string());
    let store = Path::new(&store);
    let serial = random_serial();
    if let Some(dir) = store.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match fs::write(store, &serial) {
        Ok(()) => info!("USB serial rotated to {serial} — the next attempt presents a new iPhone"),
        Err(e) => warn!("could not rotate the USB serial in {}: {e}", store.display()),
    }
}

/// Leftovers of a previous (possibly crashed) run are removed first.  Caller
/// must then run [`ffs::Ep0Set::open_all`] before [`bind`].
pub fn create() -> Result<()> {
    remove().context("removing leftovers of a previous gadget")?;
    load_modules()?;

    let gpath = gadget_path();
    info!("creating Apple gadget at {}", gpath.display());
    let strings = gpath.join("strings/0x409");
    fs::create_dir_all(&strings).with_context(|| format!("creating {}", strings.display()))?;

    for (attr, value) in DEVICE_ATTRS {
        write_file(&gpath.join(attr), value)?;
    }
    let serial = device_serial();
    for (attr, value) in DEVICE_STRINGS {
        let value = if *attr == "serialnumber" { serial.as_str() } else { *value };
        write_file(&strings.join(attr), value)?;
    }
    create_pad_string(&strings);

    for (index, spec) in ffs::CONFIGS.iter().enumerate() {
        let config_value = index as u8 + 1;
        let instance = ffs::instance_name(config_value);

        let func = gpath.join(format!("functions/ffs.{instance}"));
        fs::create_dir_all(&func).with_context(|| format!("creating {}", func.display()))?;

        let config = gpath.join(format!("configs/c.{config_value}"));
        let config_strings = config.join("strings/0x409");
        fs::create_dir_all(&config_strings).with_context(|| format!("creating {}", config_strings.display()))?;
        write_file(&config_strings.join("configuration"), spec.label)?;
        write_file(&config.join("MaxPower"), CONFIG_MAX_POWER)?;
        write_file(&config.join("bmAttributes"), CONFIG_ATTRIBUTES)?;

        // f_uac1 must be linked BEFORE the FunctionFS function: configfs assigns
        // interface numbers in link order, so the audio function takes 0 and 1 and
        // the FunctionFS HID interface lands on 2 -- the layout a real device shows.
        if config_value == UAC1_CONFIG && ffs::uac1_enabled() {
            let uac = gpath.join(format!("functions/uac1.{UAC1_INSTANCE}"));
            fs::create_dir_all(&uac).with_context(|| format!("creating {}", uac.display()))?;
            for (attr, value) in UAC1_ATTRS {
                write_file(&uac.join(attr), value)?;
            }
            let ulink = config.join(format!("uac1.{UAC1_INSTANCE}"));
            symlink(&uac, &ulink)
                .with_context(|| format!("linking {} \u{2192} {}", ulink.display(), uac.display()))?;
            info!("config c.{config_value}: f_uac1 '{UAC1_INSTANCE}' linked (interfaces 0-1)");
        }

        let link = config.join(format!("ffs.{instance}"));
        symlink(&func, &link).with_context(|| format!("linking {} → {}", link.display(), func.display()))?;

        let mount = ffs::mount_point(config_value);
        fs::create_dir_all(&mount).with_context(|| format!("creating mount point {mount}"))?;
        let status = Command::new("mount")
            .args(["-t", "functionfs", &instance, &mount])
            .status()
            .context("running mount for FunctionFS")?;
        if !status.success() {
            anyhow::bail!("failed to mount FunctionFS instance {instance} at {mount} ({status})");
        }
        info!("config c.{config_value} '{}': FunctionFS {instance} mounted at {mount}", spec.label);
    }
    Ok(())
}

/// Best-effort custom device string (older kernels lack configfs string groups).
fn create_pad_string(strings: &Path) {
    let dir = strings.join(PAD_STRING_DIR);
    let created = fs::create_dir(&dir)
        .with_context(|| format!("creating {}", dir.display()))
        .and_then(|()| write_file(&dir.join("s"), PAD_STRING));
    match created {
        Ok(()) => info!("pad string '{PAD_STRING}' created at {}", dir.display()),
        Err(e) => warn!("pad string not supported by this kernel (continuing without it): {e:#}"),
    }
}

/// Bind the gadget to the UDC — the device becomes visible on the USB bus and
/// the car begins enumeration.  Returns the UDC name.
/// Drop off the bus for `off` and come back, so the host enumerates the gadget
/// afresh — the software equivalent of re-plugging the cable. Uses the UDC's
/// `soft_connect` attribute, so the gadget and every FunctionFS fd stay intact.
pub fn soft_reconnect(udc: &str, off: std::time::Duration) -> Result<()> {
    let attr = PathBuf::from(format!("/sys/class/udc/{udc}/soft_connect"));
    fs::write(&attr, "disconnect").with_context(|| format!("writing 'disconnect' to {}", attr.display()))?;
    std::thread::sleep(off);
    fs::write(&attr, "connect").with_context(|| format!("writing 'connect' to {}", attr.display()))?;
    Ok(())
}

pub fn bind() -> Result<String> {
    let udc = find_udc()?;
    write_file(&gadget_path().join("UDC"), &udc)?;
    info!("gadget bound to UDC {udc} — Apple device now visible to car");
    Ok(udc)
}

/// Unbind the gadget from the UDC.
pub fn unbind() -> Result<()> {
    let udc_file = gadget_path().join("UDC");
    if udc_file.exists() {
        write_file(&udc_file, "")?;
        info!("gadget unbound from UDC");
    }
    Ok(())
}

/// FunctionFS mounts owned by this gadget, read from `/proc/mounts`: the current
/// `/dev/ffs-carplay-cN` instances and the legacy single `/dev/ffs-carplay`.
fn carplay_ffs_mounts() -> Vec<String> {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    parse_carplay_ffs_mounts(&mounts)
}

fn parse_carplay_ffs_mounts(mounts: &str) -> Vec<String> {
    let instance_prefix = format!("{}-c", ffs::FFS_MOUNT_PREFIX);
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (_source, target, fstype) = (fields.next()?, fields.next()?, fields.next()?);
            let ours = target == ffs::FFS_MOUNT_PREFIX || target.starts_with(&instance_prefix);
            (fstype == "functionfs" && ours).then(|| target.to_string())
        })
        .collect()
}

/// Unmount a FunctionFS instance, retrying lazily if it is still busy.
///
/// A plain `umount` returns EBUSY while anything still holds `ep0`; a lazy
/// unmount detaches it once the last user closes.
fn umount_ffs(mount: &str) -> Result<()> {
    let status = Command::new("umount")
        .arg(mount)
        .status()
        .with_context(|| format!("running umount {mount}"))?;
    if status.success() {
        info!("FunctionFS unmounted from {mount}");
        return Ok(());
    }

    warn!("umount {mount} failed ({status}) — something still holds the mount; retrying with umount -l");
    let lazy = Command::new("umount")
        .args(["-l", mount])
        .status()
        .with_context(|| format!("running umount -l {mount}"))?;
    if !lazy.success() {
        anyhow::bail!("failed to unmount FunctionFS at {mount} (umount -l exited {lazy})");
    }
    warn!("FunctionFS at {mount} lazily unmounted — the mount detaches once its last user closes");
    Ok(())
}

/// Sorted child paths of `dir` matching `keep` (empty when `dir` is absent).
fn children(dir: &Path, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| keep(p)).collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Tear down the entire gadget and every FunctionFS mount.
///
/// Tolerates any partial state (a crashed previous run, the legacy
/// single-config layout, mounts whose configfs tree is already gone): each step
/// only touches what exists, and a failing step does not stop the others.
/// Order, as in the probe: UDC unbind → function links → configurations →
/// umount FunctionFS → functions → strings → gadget directory.
///
/// Failures are still propagated (collected, then returned together): a
/// `remove()` that reports success while leaving a half-configured gadget
/// behind makes the next `create()` fail with an unrelated error.
pub fn remove() -> Result<()> {
    let gpath = gadget_path();
    let mut failures: Vec<String> = Vec::new();
    let mut note = |what: Result<()>| {
        if let Err(e) = what {
            warn!("gadget teardown: {e:#}");
            failures.push(format!("{e:#}"));
        }
    };

    // 1. Detach from the UDC.  Not fatal on its own: after the role switch the
    //    controller may already be in host mode.  A still-bound gadget makes the
    //    rmdir chain fail, and that IS reported.
    if gpath.exists() {
        if let Err(e) = unbind() {
            warn!("could not unbind gadget from the UDC before teardown: {e:#} — continuing");
        }
    }

    // 2. Function links, then configuration directories.
    let configs = children(&gpath.join("configs"), |p| p.is_dir());
    for config in &configs {
        for link in children(config, |p| p.is_symlink()) {
            note(fs::remove_file(&link).with_context(|| format!("removing function link {}", link.display())));
        }
    }
    for config in &configs {
        note(remove_configfs_dir(&config.join("strings/0x409")));
        note(remove_configfs_dir(config));
    }

    // 3. FunctionFS mounts (after unbind, before removing the functions), then
    //    their now-empty mount points (a leftover empty directory is harmless).
    for mount in carplay_ffs_mounts() {
        note(umount_ffs(&mount));
    }
    let dev_prefix = format!("{}-c", ffs::FFS_MOUNT_PREFIX);
    let mount_dirs = children(Path::new("/dev"), |p| {
        p.to_str().is_some_and(|s| s.starts_with(&dev_prefix) || s == ffs::FFS_MOUNT_PREFIX)
    });
    for dir in mount_dirs {
        if let Err(e) = fs::remove_dir(&dir) {
            debug!("could not remove mount point {}: {e}", dir.display());
        }
    }

    // 4. Function instances.
    for func in children(&gpath.join("functions"), |p| p.is_dir()) {
        note(remove_configfs_dir(&func));
    }

    // 5. Custom string groups (the pad string), language dirs, the gadget.
    for lang in children(&gpath.join("strings"), |p| p.is_dir()) {
        for group in children(&lang, |p| p.is_dir()) {
            note(remove_configfs_dir(&group));
        }
        note(remove_configfs_dir(&lang));
    }
    note(remove_configfs_dir(&gpath));

    if !failures.is_empty() {
        anyhow::bail!("gadget {GADGET_NAME} teardown incomplete: {}", failures.join("; "));
    }
    info!("gadget {GADGET_NAME} removed (FunctionFS unmounted, configfs clean)");
    Ok(())
}

/// `rmdir` a configfs directory, propagating the error with the path attached.
/// A missing directory is not an error (teardown is idempotent).
fn remove_configfs_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir(path).with_context(|| format!("removing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_matches_probe() {
        let attrs: Vec<(&str, &str)> = DEVICE_ATTRS.to_vec();
        assert!(attrs.contains(&("idVendor", "0x05ac")));
        assert!(attrs.contains(&("idProduct", "0x12a8")));
        assert!(attrs.contains(&("bcdDevice", "0x1001")));
        assert!(attrs.contains(&("bMaxPacketSize0", "0x40")));
        assert!(!attrs.iter().any(|(k, _)| *k == "max_speed"), "HS is required: never cap max_speed");
        let serial = DEVICE_STRINGS.iter().find(|(k, _)| *k == "serialnumber").unwrap().1;
        assert_eq!(serial.len(), 40);
        assert!(serial.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn finds_only_our_functionfs_mounts() {
        let mounts = "\
sysfs /sys sysfs rw 0 0
c1 /dev/ffs-carplay-c1 functionfs rw,relatime 0 0
c4 /dev/ffs-carplay-c4 functionfs rw,relatime 0 0
usb0 /dev/ffs-carplay functionfs rw,relatime 0 0
usb0 /dev/ffs-rascar functionfs rw,relatime 0 0
tmpfs /dev/ffs-carplay-c2 tmpfs rw 0 0
";
        assert_eq!(
            parse_carplay_ffs_mounts(mounts),
            ["/dev/ffs-carplay-c1", "/dev/ffs-carplay-c4", "/dev/ffs-carplay"]
        );
    }
}
