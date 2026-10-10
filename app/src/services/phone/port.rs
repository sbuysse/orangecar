//! Find the SIM7600's voice ports by USB interface number, never by ttyUSB
//! index (the numbering shifts when another USB-serial device is plugged in
//! first). Same sysfs walk as services/cell.rs: `/sys/class/tty/ttyUSBn/device`
//! is the usb-serial port, `device/..` the USB interface (bInterfaceNumber),
//! `device/../..` the USB device (idVendor/idProduct).

use std::path::{Path, PathBuf};

pub const SIM7600_VID: &str = "1e0e";
pub const SIM7600_PID: &str = "9001";
/// Second AT port: the phone's alone (cell.rs polls interface 2).
pub const AT_IFACE: u8 = 3;
/// "Audio 9001": raw PCM while AT+CPCMREG=1.
pub const PCM_IFACE: u8 = 4;

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// `/dev/ttyUSBn` of the modem's interface `iface`, if enumerated.
pub fn find_iface_tty(class_tty: &Path, vid: &str, pid: &str, iface: u8) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(class_tty)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("ttyUSB"))
        .filter_map(|e| {
            let usb = std::fs::canonicalize(e.path().join("device/../..")).ok()?;
            if read_trim(&usb.join("idVendor")).as_deref() != Some(vid) || read_trim(&usb.join("idProduct")).as_deref() != Some(pid) {
                return None;
            }
            let n = read_trim(&e.path().join("device/../bInterfaceNumber")).and_then(|n| u8::from_str_radix(&n, 16).ok())?;
            (n == iface).then(|| PathBuf::from("/dev").join(e.file_name()))
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// (AT port, PCM port) of a plugged-in SIM7600.
pub fn sim7600_ports() -> Option<(PathBuf, PathBuf)> {
    let class = Path::new("/sys/class/tty");
    Some((find_iface_tty(class, SIM7600_VID, SIM7600_PID, AT_IFACE)?, find_iface_tty(class, SIM7600_VID, SIM7600_PID, PCM_IFACE)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake sysfs: usb device dir with interfaces 2..4, each holding a
    /// ttyUSB port dir, and class/tty/ttyUSBn/device symlinks into them.
    fn fake_sysfs(vid: &str, pid: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rascar-phone-sysfs-{}-{vid}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("devices/1-1");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(dev.join("idProduct"), format!("{pid}\n")).unwrap();
        let class = root.join("class/tty");
        std::fs::create_dir_all(&class).unwrap();
        for (iface, tty) in [(2u8, "ttyUSB2"), (3, "ttyUSB3"), (4, "ttyUSB4")] {
            let intf = dev.join(format!("1-1:1.{iface}"));
            std::fs::create_dir_all(intf.join(tty)).unwrap();
            std::fs::write(intf.join("bInterfaceNumber"), format!("{iface:02x}\n")).unwrap();
            std::fs::create_dir_all(class.join(tty)).unwrap();
            std::os::unix::fs::symlink(intf.join(tty), class.join(tty).join("device")).unwrap();
        }
        root
    }

    #[test]
    fn finds_ports_by_interface_number() {
        let root = fake_sysfs("1e0e", "9001");
        let class = root.join("class/tty");
        assert_eq!(find_iface_tty(&class, "1e0e", "9001", AT_IFACE), Some(PathBuf::from("/dev/ttyUSB3")));
        assert_eq!(find_iface_tty(&class, "1e0e", "9001", PCM_IFACE), Some(PathBuf::from("/dev/ttyUSB4")));
        assert_eq!(find_iface_tty(&class, "1e0e", "9001", 5), None);
        // Another vendor's ttys are never taken.
        assert_eq!(find_iface_tty(&class, "1286", "4e31", AT_IFACE), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
