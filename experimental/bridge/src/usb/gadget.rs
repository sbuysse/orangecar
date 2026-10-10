//! USB Gadget setup via configfs + FunctionFS.
//!
//! Configures the OrangePi 5 Plus USB-C OTG port as an Android Open Accessory
//! device.  Uses libcomposite + usb_f_fs (FunctionFS) instead of the
//! Android-BSP-only usb_f_accessory module.
//!
//! Requires:
//!   - rk3588-dwc3-peripheral overlay in /boot/armbianEnv.txt
//!   - Kernel modules: libcomposite, usb_f_fs
//!   - Root privileges

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

const CONFIGFS: &str = "/sys/kernel/config/usb_gadget";
const GADGET_NAME: &str = "rascar";

/// Google AOA IDs — used BEFORE accessory mode switch.
/// The f_accessory module handles the AOA handshake and re-enumerates
/// with VID=0x18D1 PID=0x2D00 automatically.
const DEFAULT_VID: &str = "0x18D1"; // Google
const DEFAULT_PID: &str = "0x4EE1"; // Nexus generic (triggers AOA probe on HU)

const SERIAL: &str = "RASCAR0001";
const MANUFACTURER: &str = "RasCar";
const PRODUCT: &str = "RasCar Android Auto";

fn gadget_path() -> PathBuf {
    Path::new(CONFIGFS).join(GADGET_NAME)
}

fn write_file(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).with_context(|| format!("writing {}", path.display()))?;
    tracing::debug!("wrote {} → {}", path.display(), value.trim());
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

fn find_udc() -> Result<String> {
    let udc_dir = Path::new("/sys/class/udc");
    if !udc_dir.exists() {
        anyhow::bail!("no /sys/class/udc — is dwc2 loaded?");
    }
    // Collect and sort so selection is deterministic across boots. On boards that
    // expose more than one UDC, blindly taking the first can bind the wrong
    // controller — surface that instead of failing silently.
    let mut names: Vec<String> = fs::read_dir(udc_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let name = names.first().cloned().ok_or_else(|| {
        anyhow::anyhow!("no UDC available. Ensure dtoverlay=dwc2 in /boot/config.txt")
    })?;
    if names.len() > 1 {
        warn!("multiple UDCs present {names:?} — using {name} (set the peripheral overlay if wrong)");
    }
    info!("found UDC: {name}");
    Ok(name)
}

/// Create the USB gadget with FunctionFS function and mount FunctionFS.
/// Caller must open FFS_MOUNT/ep0 and call ffs::write_descriptors() before gadget::bind().
pub fn create() -> Result<()> {
    let gpath = gadget_path();
    if gpath.exists() {
        warn!("gadget {GADGET_NAME} already exists, removing first");
        remove()?;
    }

    load_modules()?;

    info!("creating gadget at {}", gpath.display());
    fs::create_dir_all(&gpath)?;

    // Device descriptor
    write_file(&gpath.join("idVendor"), DEFAULT_VID)?;
    write_file(&gpath.join("idProduct"), DEFAULT_PID)?;
    write_file(&gpath.join("bcdDevice"), "0x0100")?;
    write_file(&gpath.join("bcdUSB"), "0x0200")?;
    write_file(&gpath.join("bDeviceClass"), "0x00")?;
    write_file(&gpath.join("bDeviceSubClass"), "0x00")?;
    write_file(&gpath.join("bDeviceProtocol"), "0x00")?;
    // Limit to full-speed (USB 1.1, 12 Mbps) — we only provide FS descriptors.
    // DWC3 defaults to super-speed-plus; without SS/HS descriptors in FFS the
    // kernel won't advertise any configurations to the host (bNumConfigurations=0).
    write_file(&gpath.join("max_speed"), "full-speed")?;

    // String descriptors (English)
    let strings = gpath.join("strings/0x409");
    fs::create_dir_all(&strings)?;
    write_file(&strings.join("serialnumber"), SERIAL)?;
    write_file(&strings.join("manufacturer"), MANUFACTURER)?;
    write_file(&strings.join("product"), PRODUCT)?;

    // Configuration
    let config = gpath.join("configs/c.1");
    fs::create_dir_all(&config)?;
    let config_strings = config.join("strings/0x409");
    fs::create_dir_all(&config_strings)?;
    write_file(&config_strings.join("configuration"), "Android Auto Accessory")?;
    write_file(&config.join("MaxPower"), "500")?;

    // FunctionFS function — "usb0" must match the FFS mount source name
    let func = gpath.join("functions/ffs.usb0");
    fs::create_dir_all(&func)?;

    // Symlink function into configuration
    let link = config.join("ffs.usb0");
    if !link.exists() {
        symlink(&func, &link)?;
    }

    // Create FFS mount point and mount
    let ffs_mount = crate::usb::ffs::FFS_MOUNT;
    fs::create_dir_all(ffs_mount)?;

    let status = Command::new("mount")
        .args(["-t", "functionfs", "usb0", ffs_mount])
        .status()
        .context("running mount for FunctionFS")?;
    if !status.success() {
        anyhow::bail!("failed to mount FunctionFS at {ffs_mount}");
    }

    info!("FunctionFS mounted at {ffs_mount}");
    Ok(())
}

/// Bind the gadget to the UDC — device becomes visible on USB bus.
pub fn bind() -> Result<()> {
    let udc = find_udc()?;
    write_file(&gadget_path().join("UDC"), &udc)?;
    info!("gadget bound to UDC {udc} — device is now visible to car");
    Ok(())
}

/// Unbind the gadget from UDC.
pub fn unbind() -> Result<()> {
    let udc_file = gadget_path().join("UDC");
    if udc_file.exists() {
        write_file(&udc_file, "")?;
        info!("gadget unbound from UDC");
    }
    Ok(())
}

/// Set idProduct without touching UDC binding.
pub fn set_pid(pid: &str) -> Result<()> {
    write_file(&gadget_path().join("idProduct"), pid)
}

/// Is `path` currently a mount point? Reads /proc/self/mounts so we can tell a
/// stale mount from a bare directory — `Path::exists()` cannot, because the
/// mount point directory survives the unmount.
fn is_mounted(path: &str) -> bool {
    fs::read_to_string("/proc/self/mounts")
        .map(|mounts| {
            mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(path))
        })
        .unwrap_or(false)
}

/// Tear down the entire gadget and unmount FunctionFS.
pub fn remove() -> Result<()> {
    let gpath = gadget_path();

    // Unmount FunctionFS first (before removing configfs dirs).
    // Mounts stack: a failed unmount that is ignored means the next create()
    // mounts a SECOND FunctionFS on the same point, and the gadget then talks to
    // an instance nobody holds ep0 on. So keep unmounting until the point is
    // clear and surface any failure instead of discarding it.
    let ffs_mount = crate::usb::ffs::FFS_MOUNT;
    const MAX_UNMOUNT_ATTEMPTS: u32 = 10;
    let mut attempts = 0;
    while is_mounted(ffs_mount) {
        if attempts >= MAX_UNMOUNT_ATTEMPTS {
            anyhow::bail!(
                "{ffs_mount} is still mounted after {MAX_UNMOUNT_ATTEMPTS} attempts — \
                 refusing to stack another FunctionFS on it"
            );
        }
        attempts += 1;
        let status = Command::new("umount")
            .arg(ffs_mount)
            .status()
            .context("running umount for FunctionFS")?;
        if status.success() {
            info!("FunctionFS unmounted");
        } else {
            // umount can report EBUSY for a moment after the last endpoint handle
            // is closed (tokio defers the real close to its blocking pool), so a
            // failure is retried before it is fatal.
            warn!("umount {ffs_mount} failed ({status}) — retrying");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    if !gpath.exists() {
        return Ok(());
    }

    let _ = unbind();

    // Remove symlinks from configs
    let config = gpath.join("configs/c.1");
    if config.exists() {
        for entry in fs::read_dir(&config)? {
            let entry = entry?;
            if entry.path().is_symlink() {
                fs::remove_file(entry.path())?;
            }
        }
        let _ = fs::remove_dir(config.join("strings/0x409"));
        let _ = fs::remove_dir(&config);
    }

    // Remove functions
    let funcs = gpath.join("functions");
    if funcs.exists() {
        for entry in fs::read_dir(&funcs)? {
            let _ = fs::remove_dir(entry?.path());
        }
    }

    let _ = fs::remove_dir(gpath.join("strings/0x409"));
    let _ = fs::remove_dir(&gpath);
    info!("gadget {GADGET_NAME} removed");
    Ok(())
}
