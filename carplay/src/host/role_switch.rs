//! USB **device → host** role transition for the RK3588 dwc3 controller.
//!
//! After the car sends the CarPlay role-switch vendor request (`bRequest=0x51`,
//! detected in Phase 1), the "iPhone" is expected to flip its USB controller
//! from **peripheral** to **host** so it can enumerate the car (which becomes a
//! USB device exposing the iAP2 bulk + CDC-NCM interfaces).
//!
//! # Mechanisms (RK3588 / dwc3, hardware-specific — see sources)
//!
//! There is no single portable way to flip a dwc3 OTG port at runtime; different
//! kernels expose different knobs.  We try them in priority order and succeed if
//! **any** write lands, logging every attempt:
//!
//! 1. **`usb_role_switch` framework** — `/sys/class/usb_role/<switch>/role`,
//!    write `host` / `device` / `none`.  This is the modern, generic interface
//!    (Type-C `tcpm` ports register here too).
//!    Source: <https://www.kernel.org/doc/Documentation/ABI/testing/sysfs-class-usb_role>
//!
//! 2. **dwc3 debugfs `mode`** — `/sys/kernel/debug/usb/fc000000.usb/mode`,
//!    write `host` / `device` / `peripheral`.  Rockchip's own docs call this the
//!    "recommended" method ("Method 2, no DTS limitation").
//!    Source: <https://docs.bit-brick.com/docs/ssom_3588/software/peripheral_driver/USB>
//!
//! 3. **dwc3 platform `role`** — `/sys/bus/platform/devices/fc000000.usb/dwc3.*/role`
//!    (the dwc3 core also exports a `role` sysfs attr on some trees).
//!
//! 4. **Rockchip usb2-phy `otg_mode`** (legacy Rockchip vendor kernels) —
//!    `/sys/devices/platform/*.syscon/*:usb2-phy@*/otg_mode`, write
//!    `host` / `peripheral`.  Needs the DTS `dr_mode = "otg"`.
//!    Source: same bit-brick doc, "Method 1".
//!
//! # Diagnostics
//!
//! Every attempt is bracketed by a [`role_snapshot`] (usb_role attributes, dwc3
//! debugfs `mode`/`link_state`, Type-C port roles, UDC state and the device-tree
//! `dr_mode`), and the function always ends with one grep-able
//! `ROLE-SWITCH RESULT: method=… readback=…` line.
//!
//! Measured on the OrangePi 5 Plus (Armbian trixie, 6.18.20): the usb_role switch
//! `fc000000.usb-role-switch` exists but has **no `role` attribute**, and the
//! boot overlays force `rk3588-dwc3-peripheral`, so a host re-role may well be
//! refused — the snapshots exist to make that visible.
//!
//! # ⚠ Hardware verification required
//!
//! - `// TODO(hw): verify which of these paths actually exists on the OrangePi 5
//!   Plus Armbian 6.12 kernel, and which one *actually* re-roles the port` — run
//!   `find /sys -name role -o -name mode -o -name otg_mode 2>/dev/null` and
//!   `ls /sys/class/usb_role` on the board with the car attached, and confirm the
//!   `role` value read back as `host` after the write.
//! - `// TODO(hw): confirm whether the peripheral gadget must be UNBOUND from the
//!   UDC first` (we do that in the orchestrator) and whether flipping the role
//!   requires the Type-C `tcpm` port to also agree (data-role swap over CC).

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing::{debug, info, warn};

use crate::debug::read_attr;

/// The RK3588 OTG dwc3 controller platform device we impersonate the phone on.
// TODO(hw): verify this is the port physically wired to the car on the OrangePi
// 5 Plus (the board has multiple dwc3 instances; fc000000.usb is the USB3 OTG /
// Type-C DRD one per the mainline rk3588 DTS).
pub const UDC_NAME: &str = "fc000000.usb";

/// Value written to request USB **host** mode.
const ROLE_HOST: &str = "host";

/// Delay before re-reading dwc3 `mode` after the host write.
const MODE_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// Values that put the port back into peripheral mode, tried in order. The
/// `usb_role` framework and the dwc3 knobs spell it `device`; the Rockchip
/// legacy usb2-phy `otg_mode` spells it `peripheral`. A sysfs write of an
/// unsupported value fails with `EINVAL`, so trying both is safe.
const ROLE_DEVICE_VALUES: &[&str] = &["device", "peripheral"];

/// Record of which sysfs knob(s) actually accepted the host-mode write, so that
/// exactly those — and nothing else — can be put back on the way out.
///
/// Without this, any failure after [`switch_to_host`] (car enumeration timeout,
/// interface claim failure) leaves the SoC port stuck in host mode: the next
/// launch then fails at "bind to UDC" with an error that says nothing about the
/// role switch, and the board needs a reboot.
#[derive(Debug, Default)]
pub struct RoleSwitch {
    accepted: Vec<PathBuf>,
    /// fusb302 i2c devices (e.g. `6-0022`) unbound before the switch, so they
    /// can be re-bound on revert.
    unbound_typec: Vec<String>,
}

impl RoleSwitch {
    /// Put the port back into peripheral/device mode on every knob that took the
    /// host write, then re-bind the fusb302 Type-C controller(s) that were
    /// unbound. Best-effort and never fails: it runs on error paths, where the
    /// interesting error is the one already being propagated.
    pub fn revert_to_device(&self) {
        if self.accepted.is_empty() && self.unbound_typec.is_empty() {
            return;
        }
        info!(
            "role-switch: reverting {} knob(s) back to peripheral mode so the gadget can bind again",
            self.accepted.len()
        );
        info!("role-switch: BEFORE revert snapshot: {}", role_snapshot());
        for path in &self.accepted {
            let mut reverted = false;
            for value in ROLE_DEVICE_VALUES {
                match write_role(path, value) {
                    AttemptResult::Wrote => {
                        info!(
                            "role-switch: wrote '{value}' → {} (reads back: {})",
                            path.display(),
                            read_back(path).unwrap_or_else(|| "<unreadable>".into())
                        );
                        reverted = true;
                        break;
                    }
                    other => debug!(
                        "role-switch: revert write '{value}' → {}: {}",
                        path.display(),
                        other.describe()
                    ),
                }
            }
            if !reverted {
                warn!(
                    "role-switch: could NOT revert {} to peripheral mode (tried {ROLE_DEVICE_VALUES:?}) \
                     — the port may stay in host mode until a reboot",
                    path.display()
                );
            }
        }
        // Re-bind tcpm only after the controller is back in device mode (probe order).
        rebind_typec_controllers(&self.unbound_typec);
        info!("role-switch: AFTER revert snapshot: {}", role_snapshot());
    }
}

// ── fusb302 Type-C controller (tcpm) ──────────────────────────────────────────
//
// Measured on the car: its ~220 ms VBUS cycle after the role-switch request made
// tcpm push `usb-role 0` then `usb-role 2` through the usb_role switch, and dwc3
// reverted to device mode (xhci removed). With the fusb302 i2c driver unbound,
// nothing drives the role and dwc3 stays host.

/// Driver directory of the fusb302 Type-C port controller.
const FUSB302_DRIVER: &str = "/sys/bus/i2c/drivers/typec_fusb302";

/// True for an i2c device name (`<bus>-<4 hex digit address>`, e.g. `6-0022`),
/// as opposed to the driver directory's other entries (`bind`, `module`, ...).
fn is_i2c_device_name(name: &str) -> bool {
    match name.split_once('-') {
        Some((bus, addr)) => {
            !bus.is_empty()
                && bus.bytes().all(|b| b.is_ascii_digit())
                && addr.len() == 4
                && addr.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Unbind every i2c device currently bound to the fusb302 driver.  Returns the
/// names that were actually unbound.
fn unbind_typec_controllers() -> Vec<String> {
    let driver = Path::new(FUSB302_DRIVER);
    let Ok(rd) = fs::read_dir(driver) else {
        info!("role-switch[typec]: {FUSB302_DRIVER} not present — no fusb302 to unbind");
        return Vec::new();
    };
    let mut bound: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_i2c_device_name(name))
        .collect();
    bound.sort();
    if bound.is_empty() {
        info!("role-switch[typec]: no i2c device bound to {FUSB302_DRIVER}");
    }

    let mut unbound = Vec::new();
    for name in bound {
        match fs::write(driver.join("unbind"), &name) {
            Ok(()) => {
                info!("role-switch[typec]: unbound fusb302 i2c device {name} (tcpm no longer drives the USB role)");
                unbound.push(name);
            }
            Err(e) => warn!(
                "role-switch[typec]: unbinding fusb302 i2c device {name} FAILED: {e} — tcpm may force dwc3 back to device mode"
            ),
        }
    }
    unbound
}

/// Re-bind fusb302 i2c devices previously returned by [`unbind_typec_controllers`].
fn rebind_typec_controllers(names: &[String]) {
    let bind = Path::new(FUSB302_DRIVER).join("bind");
    for name in names {
        match fs::write(&bind, name) {
            Ok(()) => info!("role-switch[typec]: re-bound fusb302 i2c device {name}"),
            Err(e) => warn!("role-switch[typec]: re-binding fusb302 i2c device {name} FAILED: {e}"),
        }
    }
}

// ── usbcore quirk for the car's USB device ────────────────────────────────────
//
// Measured with usbmon: once in host mode, the car (8086:9999 "Gadget MFi v0.1")
// has a 64-byte string descriptor #3 and sends no terminating zero-length packet,
// so the usual 255-byte string read (cdc_ncm reading iMACAddress) hangs 4 s, ends
// in EPROTO and wedges the car. USB_QUIRK_STRING_FETCH_255 (flag `a`) makes
// usbcore avoid the 255-byte read for that VID:PID.
//
// USB_QUIRK_NO_LPM (flag `k`): the car advertises USB2 LPM + BESL and usbcore
// enables hardware LPM by default. On 2026-09-25, in the car, writing
// `8086:9999:ak` at runtime was the last change before the FIRST CarPlay session
// that outlived the car's ~2.1 s-after-auth deadline (video + touch working);
// every earlier attempt of the day, with all other changes already in place, died
// at 4.3 s. Kept on for every attempt.

const USBCORE_QUIRKS: &str = "/sys/module/usbcore/parameters/quirks";
const CAR_QUIRK_KEY: &str = "8086:9999:";
const CAR_QUIRK_FLAGS: &str = "ak";
const CAR_QUIRK_ENTRY: &str = "8086:9999:ak";

/// New quirks parameter value with the car's entry appended, or `None` when an
/// entry for that VID:PID already exists.
fn quirks_with_car_entry(current: &str) -> Option<String> {
    let current = current.trim();
    if current.contains(CAR_QUIRK_KEY) {
        // Upgrade an existing car entry (e.g. the older `8086:9999:a`) so it
        // carries every flag we need; leave other devices' entries untouched.
        let merged: Vec<String> = current
            .split(',')
            .map(|entry| match entry.strip_prefix(CAR_QUIRK_KEY) {
                Some(flags) => {
                    let mut f = flags.to_string();
                    for c in CAR_QUIRK_FLAGS.chars() {
                        if !f.contains(c) {
                            f.push(c);
                        }
                    }
                    format!("{CAR_QUIRK_KEY}{f}")
                }
                None => entry.to_string(),
            })
            .collect();
        let merged = merged.join(",");
        if merged == current { None } else { Some(merged) }
    } else if current.is_empty() {
        Some(CAR_QUIRK_ENTRY.to_string())
    } else {
        Some(format!("{current},{CAR_QUIRK_ENTRY}"))
    }
}

/// Best-effort: add USB_QUIRK_STRING_FETCH_255 for the car before it enumerates.
fn apply_car_usb_quirk() {
    let before = match fs::read_to_string(USBCORE_QUIRKS) {
        Ok(v) => v.trim().to_string(),
        Err(e) => {
            warn!("usb quirk: cannot read {USBCORE_QUIRKS}: {e} — car string reads may hang");
            return;
        }
    };
    match quirks_with_car_entry(&before) {
        None => info!("usb quirk: {CAR_QUIRK_ENTRY} already present, before=[{before}] (unchanged)"),
        Some(new_value) => match fs::write(USBCORE_QUIRKS, &new_value) {
            Ok(()) => info!(
                "usb quirk: before=[{before}] after=[{}]",
                read_attr(USBCORE_QUIRKS)
            ),
            Err(e) => warn!("usb quirk: writing [{new_value}] to {USBCORE_QUIRKS} FAILED: {e} (before=[{before}])"),
        },
    }
}

/// Outcome of a single mechanism attempt (for structured logging).
#[derive(Debug)]
enum AttemptResult {
    /// Path did not exist — mechanism not available on this kernel.
    NotPresent,
    /// Write succeeded.
    Wrote,
    /// Path existed but the write failed.
    Failed(String),
}

impl AttemptResult {
    fn describe(&self) -> String {
        match self {
            AttemptResult::NotPresent => "path not present".to_string(),
            AttemptResult::Wrote => "write OK".to_string(),
            AttemptResult::Failed(e) => format!("write FAILED: {e}"),
        }
    }
}

/// Try to switch `fc000000.usb` to USB **host** mode.
///
/// Best-effort: attempts every known mechanism, logs each, and returns `Ok(())`
/// if at least one write succeeded.  Returns `Err` only if no mechanism was even
/// present (so the caller knows the board exposes none of the expected knobs).
///
/// Before any write it adds the car's usbcore quirk and unbinds the fusb302
/// Type-C controller (see the constants above); on the error paths the fusb302
/// is re-bound before returning.  On this board the debugfs `mode` write is the
/// mechanism that actually lands (the usb_role switch has no `role` attribute).
///
/// Blocking (sysfs writes plus a 300 ms settle): call from a blocking context.
pub fn switch_to_host() -> Result<RoleSwitch> {
    info!("role-switch: requesting USB device→host transition for {UDC_NAME}");
    info!("role-switch: BEFORE snapshot: {}", role_snapshot());

    let mut any_present = false;
    let mut switched = RoleSwitch::default();
    apply_car_usb_quirk();
    switched.unbound_typec = unbind_typec_controllers();
    // Mechanism tags that accepted the write, for the final RESULT line.
    let mut methods: Vec<&'static str> = Vec::new();

    // 1) usb_role_switch framework — discover the switch(es) under /sys/class/usb_role.
    for switch_path in usb_role_switches() {
        any_present = true;
        let attr = switch_path.join("role");
        let result = attempt("usb_role", &attr);
        match result {
            AttemptResult::Wrote => {
                switched.accepted.push(attr.clone());
                methods.push("usb_role");
                if let Some(v) = read_back(&attr) {
                    info!("role-switch[usb_role]: wrote host → {} (reads back: {v})", attr.display());
                }
            }
            AttemptResult::Failed(e) => warn!("role-switch[usb_role]: write {} failed: {e}", attr.display()),
            AttemptResult::NotPresent => info!(
                "role-switch[usb_role]: switch {} exists but {} does not — no role attribute on this board, \
                 this mechanism cannot re-role the port",
                switch_path.display(),
                attr.display()
            ),
        }
    }

    // 2) dwc3 debugfs mode.
    let dbg_mode = PathBuf::from(format!("/sys/kernel/debug/usb/{UDC_NAME}/mode"));
    match attempt("debugfs", &dbg_mode) {
        AttemptResult::Wrote => {
            any_present = true;
            switched.accepted.push(dbg_mode.clone());
            methods.push("debugfs");
            if let Some(v) = read_back(&dbg_mode) {
                info!("role-switch[debugfs]: wrote host → {} (reads back: {v})", dbg_mode.display());
            }
        }
        AttemptResult::Failed(e) => {
            any_present = true;
            warn!("role-switch[debugfs]: write {} failed: {e} (is debugfs mounted / running as root?)", dbg_mode.display());
        }
        AttemptResult::NotPresent => debug!("role-switch[debugfs]: {} not present", dbg_mode.display()),
    }

    // 3) dwc3 platform `role` attribute: /sys/bus/platform/devices/fc000000.usb/dwc3.*/role
    let dwc3_attrs = dwc3_role_attrs();
    if dwc3_attrs.is_empty() {
        info!("role-switch[dwc3.role]: no /sys/bus/platform/devices/{UDC_NAME}/[dwc3.*/]role attribute present");
    }
    for role_attr in dwc3_attrs {
        match attempt("dwc3.role", &role_attr) {
            AttemptResult::Wrote => {
                any_present = true;
                switched.accepted.push(role_attr.clone());
                methods.push("dwc3.role");
                if let Some(v) = read_back(&role_attr) {
                    info!("role-switch[dwc3.role]: wrote host → {} (reads back: {v})", role_attr.display());
                }
            }
            AttemptResult::Failed(e) => {
                any_present = true;
                warn!("role-switch[dwc3.role]: write {} failed: {e}", role_attr.display());
            }
            AttemptResult::NotPresent => {}
        }
    }

    // 4) Rockchip legacy usb2-phy otg_mode.
    let otg_attrs = rockchip_otg_mode_attrs();
    if otg_attrs.is_empty() {
        info!("role-switch[rk-usb2phy]: no /sys/devices/platform/*syscon*/*usb2-phy@*/otg_mode attribute present");
    }
    for otg in otg_attrs {
        any_present = true;
        match attempt("rk-usb2phy", &otg) {
            AttemptResult::Wrote => {
                switched.accepted.push(otg.clone());
                methods.push("rk-usb2phy");
                info!("role-switch[rk-usb2phy]: wrote host → {}", otg.display());
            }
            AttemptResult::Failed(e) => warn!("role-switch[rk-usb2phy]: write {} failed: {e}", otg.display()),
            AttemptResult::NotPresent => {}
        }
    }

    info!("role-switch: AFTER snapshot: {}", role_snapshot());

    if !any_present {
        warn!(
            "ROLE-SWITCH RESULT: method=none typec_unbound={:?} readback={} (no mechanism present)",
            switched.unbound_typec,
            result_readback(&switched)
        );
        switched.revert_to_device();
        bail!(
            "role-switch: none of the expected sysfs knobs exist \
             (/sys/class/usb_role/*, /sys/kernel/debug/usb/{UDC_NAME}/mode, \
             .../dwc3.*/role, .../usb2-phy@*/otg_mode). \
             // TODO(hw): discover the real re-role interface on the board."
        );
    }
    if switched.accepted.is_empty() {
        warn!(
            "ROLE-SWITCH RESULT: method=none typec_unbound={:?} readback={} (mechanisms present but every write failed)",
            switched.unbound_typec,
            result_readback(&switched)
        );
        switched.revert_to_device();
        bail!("role-switch: mechanisms were present but every write failed — see warnings above");
    }

    // The kernel applies the role from a work queue, and a tcpm that is still
    // bound flips it back within ~200 ms: read the mode again after it settles.
    let readback = result_readback(&switched);
    std::thread::sleep(MODE_SETTLE);
    let settled_mode = read_attr(format!("/sys/kernel/debug/usb/{UDC_NAME}/mode"));
    info!(
        "ROLE-SWITCH RESULT: method={} typec_unbound={:?} readback={readback} mode_after_{}ms={settled_mode}",
        methods.join(","),
        switched.unbound_typec,
        MODE_SETTLE.as_millis()
    );
    if settled_mode != ROLE_HOST {
        warn!(
            "role-switch: dwc3 mode is '{settled_mode}' {} ms after the host write — the port did not stay in host mode",
            MODE_SETTLE.as_millis()
        );
    }
    info!(
        "role-switch: host-mode request issued ({} mechanism(s) accepted the write, recorded for revert)",
        switched.accepted.len()
    );
    // NOTE: a successful write does NOT prove the port re-rolled — the kernel may
    // silently ignore it if dr_mode!=otg or the tcpm port disagrees.
    // TODO(hw): after this returns, wait_for_car_device() is the real proof; if it
    // times out, dump the readback values above to see which knob (if any) stuck.
    Ok(switched)
}

/// Run one host-mode write with a snapshot before and after it and the exact
/// path / value / result logged.
fn attempt(tag: &'static str, path: &Path) -> AttemptResult {
    let exists = path.exists();
    if exists {
        debug!("role-switch[{tag}]: pre-attempt snapshot: {}", role_snapshot());
    }
    let result = write_role(path, ROLE_HOST);
    info!(
        "role-switch[{tag}]: write '{ROLE_HOST}' → {}: {} (t+{} ms)",
        path.display(),
        result.describe(),
        crate::debug::elapsed_ms()
    );
    if exists {
        debug!("role-switch[{tag}]: post-attempt snapshot (immediate): {}", role_snapshot());
        delayed_snapshot(tag);
    }
    result
}

/// Readback string for the RESULT line: each accepted knob plus the dwc3 debugfs
/// `mode` (which reports the controller's actual current role).
fn result_readback(switched: &RoleSwitch) -> String {
    let mut parts: Vec<String> = switched
        .accepted
        .iter()
        .map(|p| format!("{}={}", p.display(), read_attr(p)))
        .collect();
    parts.push(format!(
        "debugfs.mode={}",
        read_attr(format!("/sys/kernel/debug/usb/{UDC_NAME}/mode"))
    ));
    parts.join(",")
}

/// Log a [`role_snapshot`] at info with a caller-supplied label.
pub fn log_role_snapshot(label: &str) {
    info!("role-switch: snapshot ({label}): {}", role_snapshot());
}

/// One-line, best-effort dump of every knob that decides or reveals the port
/// role. Never fails: unreadable values are rendered as `<error>`.
pub fn role_snapshot() -> String {
    let mut parts: Vec<String> = Vec::new();

    // usb_role class switches — every readable regular file in each switch dir.
    match fs::read_dir("/sys/class/usb_role") {
        Ok(rd) => {
            let mut switches: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            switches.sort();
            if switches.is_empty() {
                parts.push("usb_role=<no switches>".to_string());
            }
            for sw in switches {
                let name = sw
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut attrs: Vec<String> = Vec::new();
                let mut has_role = false;
                if let Ok(entries) = fs::read_dir(&sw) {
                    let mut entries: Vec<_> = entries.flatten().collect();
                    entries.sort_by_key(|e| e.file_name());
                    for e in entries {
                        let path = e.path();
                        let fname = e.file_name().to_string_lossy().into_owned();
                        // `metadata` follows symlinks, so `device`/`subsystem`
                        // (links to directories) and `power/` are skipped.
                        if fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false) {
                            if fname == "role" {
                                has_role = true;
                            }
                            attrs.push(format!("{fname}={}", read_attr(&path).replace('\n', " ")));
                        }
                    }
                }
                if !has_role {
                    attrs.push("role=<no role attribute on this board>".to_string());
                }
                parts.push(format!("usb_role/{name}{{{}}}", attrs.join(" ")));
            }
        }
        Err(e) => parts.push(format!("usb_role=<{e}>")),
    }

    // dwc3 debugfs: the controller's current mode and link state.
    let dbg = format!("/sys/kernel/debug/usb/{UDC_NAME}");
    parts.push(format!("debugfs.mode={}", read_attr(format!("{dbg}/mode"))));
    parts.push(format!("debugfs.link_state={}", read_attr(format!("{dbg}/link_state"))));

    // Type-C port (fusb302 tcpm) roles.
    let port = "/sys/class/typec/port0";
    parts.push(format!(
        "typec.port0{{data_role={} power_role={} port_type={} partner={}}}",
        read_attr(format!("{port}/data_role")),
        read_attr(format!("{port}/power_role")),
        read_attr(format!("{port}/port_type")),
        Path::new("/sys/class/typec/port0-partner").exists()
    ));

    // UDC state(s).
    match fs::read_dir("/sys/class/udc") {
        Ok(rd) => {
            let mut udcs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            udcs.sort();
            if udcs.is_empty() {
                parts.push("udc=<none registered>".to_string());
            }
            for u in udcs {
                parts.push(format!(
                    "udc/{}.state={}",
                    u.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    read_attr(u.join("state"))
                ));
            }
        }
        Err(e) => parts.push(format!("udc=<{e}>")),
    }

    // Device-tree dr_mode of the OTG controller node(s). Measured on the board:
    // dr_mode="otg" plus a `usb-role-switch` property, i.e. the role is owned by
    // the usb_role framework (tcpm/fusb302 on typec port0), which may override a
    // direct dwc3 debugfs `mode` write.
    let nodes = dt_usb_nodes();
    if nodes.is_empty() {
        parts.push(format!("dt=<no node named usb@fc000000 under {DT_BASE}>"));
    }
    for node in nodes {
        parts.push(format!(
            "dt:{}{{dr_mode={} status={} usb-role-switch={}}}",
            node.display(),
            read_attr(node.join("dr_mode")),
            read_attr(node.join("status")),
            if node.join("usb-role-switch").exists() { "present" } else { "absent" }
        ));
    }

    parts.join(" | ")
}

/// Live device tree. `/proc/device-tree` is only a symlink to this; reading the
/// real directory avoids any tool or walker that does not follow the link.
const DT_BASE: &str = "/sys/firmware/devicetree/base";

/// Device-tree node directories for the OTG controller (cached — the live tree
/// does not change at runtime). The top-level `usb@fc000000` node is checked
/// directly; the bounded walk only runs if an overlay moved it elsewhere.
fn dt_usb_nodes() -> &'static [PathBuf] {
    static NODES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    NODES.get_or_init(|| {
        let direct = Path::new(DT_BASE).join("usb@fc000000");
        if direct.is_dir() {
            return vec![direct];
        }
        let mut out = Vec::new();
        walk_dt(Path::new(DT_BASE), 0, &mut out);
        out.sort();
        out
    })
}

/// Log a snapshot ~500 ms after a write attempt, from a detached thread so the
/// role-switch flow itself is not delayed (the kernel applies role changes from
/// a work queue, so an immediate readback can still show the old role).
fn delayed_snapshot(tag: &'static str) {
    const DELAY: std::time::Duration = std::time::Duration::from_millis(500);
    let spawned = std::thread::Builder::new()
        .name("role-snapshot".into())
        .spawn(move || {
            std::thread::sleep(DELAY);
            info!(
                "role-switch[{tag}]: snapshot +{} ms after write (t+{} ms; later attempts may already have run): {}",
                DELAY.as_millis(),
                crate::debug::elapsed_ms(),
                role_snapshot()
            );
        });
    if let Err(e) = spawned {
        debug!("role-switch[{tag}]: could not spawn the delayed snapshot thread: {e}");
    }
}

fn walk_dt(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 6;
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        // `file_type` does not follow symlinks, so the walk cannot loop.
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = e.path();
        if e.file_name().to_string_lossy().contains("usb@fc000000") {
            out.push(path.clone());
        }
        if depth < MAX_DEPTH {
            walk_dt(&path, depth + 1, out);
        }
    }
}

/// Enumerate `/sys/class/usb_role/*` switch directories.
fn usb_role_switches() -> Vec<PathBuf> {
    let base = Path::new("/sys/class/usb_role");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(base) {
        for e in rd.flatten() {
            out.push(e.path());
        }
    }
    out.sort();
    if out.is_empty() {
        debug!("role-switch: no /sys/class/usb_role/* switches found");
    } else {
        info!("role-switch: found usb_role switches: {:?}", out.iter().map(|p| p.file_name()).collect::<Vec<_>>());
    }
    out
}

/// Glob-ish scan for `/sys/bus/platform/devices/fc000000.usb/dwc3.*/role`.
fn dwc3_role_attrs() -> Vec<PathBuf> {
    let base = PathBuf::from(format!("/sys/bus/platform/devices/{UDC_NAME}"));
    let mut out = Vec::new();
    // Direct role attr on the platform device itself.
    let direct = base.join("role");
    if direct.exists() {
        out.push(direct);
    }
    // dwc3.N child glue node.
    if let Ok(rd) = fs::read_dir(&base) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("dwc3.") {
                let attr = e.path().join("role");
                if attr.exists() {
                    out.push(attr);
                }
            }
        }
    }
    out.sort();
    out
}

/// Scan `/sys/devices/platform/*syscon*/*:usb2-phy@*/otg_mode` (Rockchip legacy).
fn rockchip_otg_mode_attrs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let plat = Path::new("/sys/devices/platform");
    let Ok(rd) = fs::read_dir(plat) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if !name.to_string_lossy().contains("syscon") {
            continue;
        }
        if let Ok(inner) = fs::read_dir(&p) {
            for c in inner.flatten() {
                let cn = c.file_name();
                if cn.to_string_lossy().contains("usb2-phy@") {
                    let attr = c.path().join("otg_mode");
                    if attr.exists() {
                        out.push(attr);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn write_role(path: &Path, value: &str) -> AttemptResult {
    if !path.exists() {
        return AttemptResult::NotPresent;
    }
    debug!("role-switch: writing '{value}' → {}", path.display());
    match fs::write(path, value) {
        Ok(()) => AttemptResult::Wrote,
        Err(e) => AttemptResult::Failed(e.to_string()),
    }
}

fn read_back(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot runs on error paths and must never fail: on a host without
    /// any of the RK3588 knobs it still renders every section.
    #[test]
    fn role_snapshot_is_best_effort() {
        let snap = role_snapshot();
        for section in ["debugfs.mode=", "debugfs.link_state=", "typec.port0{", "dt"] {
            assert!(snap.contains(section), "missing {section} in {snap}");
        }
    }

    #[test]
    fn recognises_i2c_device_names() {
        assert!(is_i2c_device_name("6-0022"));
        assert!(is_i2c_device_name("12-004a"));
        for other in ["bind", "unbind", "uevent", "module", "6-22", "-0022", "x-0022", "6-00zz"] {
            assert!(!is_i2c_device_name(other), "{other}");
        }
    }

    #[test]
    fn car_quirk_is_appended_once() {
        assert_eq!(quirks_with_car_entry("").as_deref(), Some("8086:9999:ak"));
        assert_eq!(quirks_with_car_entry("\n").as_deref(), Some("8086:9999:ak"));
        assert_eq!(
            quirks_with_car_entry("0bda:8153:k\n").as_deref(),
            Some("0bda:8153:k,8086:9999:ak")
        );
        assert_eq!(quirks_with_car_entry("8086:9999:ak"), None);
        assert_eq!(quirks_with_car_entry("0bda:8153:k,8086:9999:ak"), None);
    }

    #[test]
    fn older_car_entry_is_upgraded_with_no_lpm() {
        assert_eq!(quirks_with_car_entry("8086:9999:a").as_deref(), Some("8086:9999:ak"));
        assert_eq!(
            quirks_with_car_entry("0bda:8153:k,8086:9999:a").as_deref(),
            Some("0bda:8153:k,8086:9999:ak")
        );
        assert_eq!(quirks_with_car_entry("8086:9999:ka"), None);
    }
}
