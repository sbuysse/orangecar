//! Phase 3 — **USB device→host transition + iAP2 host transport + CDC-NCM tunnel
//! + Bonjour discovery.**
//!
//! After Phase 1 detects the car's role-switch request (`0x51`) and Phase 2 has
//! a transport-generic iAP2 Device stack, Phase 3 does the hardware dance:
//!
//! ```text
//!   role_switch::switch_to_host          (flip fc000000.usb: peripheral → host)
//!        │
//!        ├── usb_host::wait_for_car_device   (car enumerates as a USB device)
//!        │        └── usb_host::UsbHostTransport::open   (claim iAP2 bulk iface)
//!        │                 └── returned to main → Iap2Device::run_until_ready()
//!        │
//!        └── ncm::bringup      (cdc_ncm netif up + IPv6 link-local, in parallel)
//!                 └── ncm::discover_airplay   (mDNS _airplay._tcp/_carplay._tcp)
//!                          └── AirPlayEndpoint → Phase 4 AirPlay/FairPlay
//! ```
//!
//! Everything here is **hardware-gated** — it cannot be verified without the car.
//! It is therefore written best-effort with heavy logging and every hardware
//! assumption flagged `// TODO(hw): …` (see the per-module docs).

pub mod ctrl_advertise;
pub mod mdns_ctrl;
pub mod ncm;
pub mod role_switch;
pub mod usb_host;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

pub use ncm::{AirPlayEndpoint, NcmLink};
// `CarDevice` is part of the public host API (returned by
// `usb_host::wait_for_car_device`) even though our own main path only consumes
// the transport — re-export it for callers without tripping unused-import.
#[allow(unused_imports)]
pub use usb_host::CarDevice;
pub use usb_host::UsbHostTransport;

/// How long to wait for the car to enumerate as a USB device after the switch.
const CAR_ENUM_TIMEOUT: Duration = Duration::from_secs(20);
/// Short: the car drops the iAP2 link ~4 s after enumerating if it gets no
/// SYN|ACK, so every poll interval is latency taken out of that budget. Kept
/// tight because measured car-enum time varies ~2.1–4.4 s and a slow detect
/// makes us miss the car's SYN window (device dropped before the handshake).
const CAR_ENUM_POLL: Duration = Duration::from_millis(25);
/// mDNS browse window.
const MDNS_TIMEOUT: Duration = Duration::from_secs(8);

/// Everything Phase 3 produces for the later phases.
///
/// Returned as soon as the iAP2 interface is claimed: the car sends its iAP2
/// DETECT + SYN right after the claim and drops the link ~4 s after enumerating
/// without a SYN|ACK, so iAP2 bring-up must start at once.  NCM bring-up
/// ([`bring_up_ncm`]) runs concurrently with it and discovery
/// ([`discover_airplay`]) after both.
pub struct Phase3Outcome {
    /// Host-side USB bulk transport over the car's iAP2 interface — hand this to
    /// [`crate::iap2::Iap2Device::new`].
    pub transport: UsbHostTransport,
    /// The resolved car device metadata (VID/PID, interface numbers).
    pub car: CarDeviceSummary,
    /// Network interfaces present before the role switch (for NCM bring-up).
    pub pre_ifaces: HashSet<String>,
    /// sysfs path of the car USB device (for NCM bring-up).
    pub car_sysfs: PathBuf,
}

/// Copyable summary of the resolved car device (the live `Device`/`Interface`
/// handles are consumed by the transport, so we keep the identifying fields).
#[derive(Debug, Clone)]
pub struct CarDeviceSummary {
    pub vendor_id: u16,
    pub product_id: u16,
    pub iap2_interface: u8,
    pub ncm_interface: Option<u8>,
}

/// Run the full Phase 3 sequence.
///
/// `teardown_gadget` is invoked BEFORE the role switch and must fully remove the
/// peripheral gadget (UDC unbind, configurations, FunctionFS umount), with every
/// ep0 fd already closed by the caller — the hardware-proven order.  We keep it
/// as a callback so `host` stays free of a dependency on the gadget module.
/// Re-acquire the car's iAP2 transport after it dropped off the bus mid-session.
///
/// The head unit frequently detaches and immediately re-enumerates (device 2 ->
/// disconnect -> device 3) during the first seconds of iAP2 bring-up. Our handle
/// then points at the dead device and every transfer returns EPROTO, so the whole
/// attempt used to die at ~t+3 s — and each wasted attempt costs the car's
/// willingness to offer CarPlay again. The port is already in host mode here, so
/// this repeats ONLY step 2 of [`run`]: wait for the device, claim iAP2. It does
/// not touch the gadget or the role switch.
pub async fn reacquire_car(timeout: std::time::Duration) -> Result<(UsbHostTransport, CarDeviceSummary, PathBuf)> {
    info!("phase3: re-acquiring the car USB device after a disconnect (port stays in host mode)");
    let car = usb_host::wait_for_car_device(timeout, CAR_ENUM_POLL)
        .await
        .context("waiting for the car USB device to re-enumerate")?;
    let summary = CarDeviceSummary {
        vendor_id: car.info.vendor_id(),
        product_id: car.info.product_id(),
        iap2_interface: car.iap2_interface,
        ncm_interface: car.ncm_interface,
    };
    let car_sysfs: PathBuf = car.info.sysfs_path().to_path_buf();
    let transport = UsbHostTransport::open(&car)
        .await
        .context("re-claiming iAP2 host transport")?;
    info!("phase3: iAP2 transport re-claimed at t+{} ms", crate::debug::elapsed_ms());
    Ok((transport, summary, car_sysfs))
}

pub async fn run<F>(teardown_gadget: F) -> Result<Phase3Outcome>
where
    F: FnOnce() -> Result<()>,
{
    info!("=== Phase 3: USB host transition + iAP2 transport + CDC-NCM + Bonjour ===");

    // Snapshot netifs BEFORE anything changes so ncm::bringup can spot the new one.
    let pre_ifaces: HashSet<String> = ncm::snapshot_interfaces();

    // Step 0: remove the peripheral gadget entirely.
    info!("phase3: tearing down the peripheral gadget (UDC unbind, configs, FunctionFS umount) before the role switch");
    if let Err(e) = teardown_gadget() {
        warn!("phase3: gadget teardown incomplete (continuing to attempt role switch): {e:#}");
    }

    // Root hubs bound BEFORE the switch, so we can tell the new bus apart.
    let root_hubs_before = bound_root_hubs();

    // Keep cdc_ncm off the car's gadget until iAP2 authentication is done (see
    // ncm_hold_wanted). Must be in place BEFORE the car can enumerate.
    let _held = engage_ncm_hold();

    // Give the car a moment between our ACK of `0x51` and the actual flip — see
    // role_switch_delay_ms(). CarLinKit's default is 10 ms.
    let delay = role_switch_delay_ms();
    if delay > 0 {
        debug!("role-switch: waiting {delay} ms after the 0x51 ack before flipping to host");
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }

    // Step 1: usb quirk + fusb302 unbind + flip the controller to USB host.
    // Everything below this point must put the port back if it fails — see
    // `RoleRollback`.  Blocking (sysfs writes + settle delay) → blocking pool.
    let switched = tokio::task::spawn_blocking(role_switch::switch_to_host)
        .await
        .context("joining the role-switch task")?
        .context("USB role switch to host")?;
    let mut rollback = RoleRollback(Some(switched));
    log_typec_vbus_state("after the switch to host");

    // Only NOW may automatic driver binding be suppressed.
    //
    // `drivers_autoprobe` is GLOBAL. Turning it off before the role switch also
    // stops the `hub` driver binding to the root hub that the freshly created
    // xHCI controller registers — and with no hub driver nothing downstream is
    // ever enumerated, so the car never appears at all. That is exactly what
    // happened on 2026-09-23 (boot-20260923-080958): the car offered the role
    // switch in all 6 attempts, we became host, the root hubs came up, and no
    // device followed. Suppression must therefore wait until the root hub is
    // bound, and it must FAIL OPEN: the collision costs ~750 ms, but suppressing
    // too early costs the whole session.
    let mut autobind_guard = if defer_ncm_bind() && wait_for_root_hub_bound(&root_hubs_before).await {
        Some(suppress_kernel_autobind())
    } else {
        if defer_ncm_bind() {
            warn!("usb: root hub not bound in time — leaving automatic binding ON (the \
                   cdc_ncm/iAP2 collision may cost ~750 ms, but the car will enumerate)");
        }
        None
    };

    // Step 2: wait for the car to enumerate as a device, then claim iAP2.
    let mut waited = usb_host::wait_for_car_device(CAR_ENUM_TIMEOUT, CAR_ENUM_POLL).await;

    // SAFETY NET. Suppressing automatic binding is a latency optimisation; the
    // car enumerating is the session. If it did not appear while we had binding
    // suppressed, put the board back exactly as it was before this optimisation
    // existed and give it one more chance, rather than losing the attempt.
    if waited.is_err() && autobind_guard.is_some() {
        warn!("usb: the car did not enumerate while automatic binding was suppressed — \
               restoring it and retrying once (the cdc_ncm collision is the lesser evil)");
        drop(autobind_guard.take());
        waited = usb_host::wait_for_car_device(CAR_ENUM_TIMEOUT, CAR_ENUM_POLL).await;
    }
    if waited.is_err() {
        role_switch::log_role_snapshot("car never enumerated after the role switch");
    }
    let car = waited.context("waiting for car USB device")?;
    let summary = CarDeviceSummary {
        vendor_id: car.info.vendor_id(),
        product_id: car.info.product_id(),
        iap2_interface: car.iap2_interface,
        ncm_interface: car.ncm_interface,
    };
    let car_sysfs: PathBuf = car.info.sysfs_path().to_path_buf();

    // The car's interfaces are registered now and no kernel driver has claimed
    // them. Re-enable autoprobe immediately: it only affects devices that appear
    // from here on, and leaving it off would stop the kernel binding anything on
    // every other USB port.
    drop(autobind_guard); // restores automatic binding (no-op if we never suppressed)
    pin_device_awake(&car_sysfs);

    // CONTROL EXPERIMENT (RASCAR_ENUM_THEN_IDLE_MS): enumerate the car and then do
    // NOTHING — no interface claim, no iAP2, no AirPlay, not a single byte.
    //
    // Why: device lifetime measured across 8 runs on 2026-09-21 was 6293-6630 ms
    // (mean ~6.4 s, sd ~120 ms) and was THE SAME whether we died in iAP2
    // identification or reached SessionReady and streamed video. A teardown that
    // ignores how far we get is a fixed timer from enumeration, not a reaction to
    // anything we send. This isolates that claim: if the car still pulls its
    // gadget at ~6.4 s while we are completely silent, then nothing we send
    // matters and the cause is something the car wants that we never provide at
    // all. If it instead stays up, the teardown IS ours to fix.
    if let Some(ms) = std::env::var("RASCAR_ENUM_THEN_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        idle_and_watch_car(&car_sysfs, Duration::from_millis(ms)).await;
        anyhow::bail!("enum-then-idle control experiment finished — deliberately not continuing");
    }

    let transport = UsbHostTransport::open(&car)
        .await
        .context("claiming iAP2 host transport")?;

    // Let the kernel FINISH binding cdc_ncm before iAP2 starts. Measured at the
    // car (usbmon, boot-20260921-080831 run 013):
    //
    //   0.986  SET_MAX_DATAGRAM_SIZE      cdc_ncm's NTB setup
    //   1.211  SET_INTERFACE              cdc_ncm switches the data alt-setting
    //   1.226  iAP2 bulk begins           15 ms later
    //   1.237  our IdentificationAccepted SUBMITTED -- and never completes
    //   5.308  hub: C_PORT_CONNECTION     the car drops off the bus
    //
    // The car's composite gadget stops servicing its iAP2 bulk OUT endpoint when
    // the NCM data interface alt-setting switch lands mid-handshake. It used to
    // be harmless because cdc_ncm finished ~235 ms before iAP2 started; the
    // msleep(220) we added to cdc_ncm to fix the enumeration EPROTO ate exactly
    // that gap. Tuning the delay only moves the collision, so gate on the event
    // instead: the netdev is registered at the very END of cdc_ncm's bind, after
    // the alt-setting switch, so its presence proves the switch is done.
    //
    // Phase 4 needs NCM up regardless, so this is not time thrown away.
    if defer_ncm_bind() {
        info!(
            "phase3: NOT waiting for cdc_ncm — the driver is kept off the car until \
             authentication completes, so its SET_INTERFACE cannot collide with the iAP2 \
             handshake (measured: that collision cost 378-1359 ms of bulk-OUT stall in 10 of \
             11 runs on 2026-09-22). RASCAR_DEFER_NCM_BIND=0 to A/B it."
        );
    } else {
        wait_for_ncm_bind(&car_sysfs).await;
    }

    info!(
        "phase3: iAP2 transport claimed at t+{} ms — NCM bind settled, starting iAP2 bring-up",
        crate::debug::elapsed_ms()
    );

    // Phase 3 succeeded: the port must STAY in host mode.
    rollback.disarm();

    Ok(Phase3Outcome {
        transport,
        car: summary,
        pre_ifaces,
        car_sysfs,
    })
}

/// Whether to keep `cdc_ncm` away from the car until iAP2 authentication is done.
///
/// **DEFAULT OFF since 2026-09-23, after this cost two car trips.**
///
/// The problem it targets is real and measured: `cdc_ncm`'s `SET_INTERFACE`
/// lands 10-57 ms before the car stops servicing its bulk endpoints for
/// 378-1359 ms (mean 752), in 10 of 11 streaming runs — ~13% of the car's
/// ~5.6 s budget. But every mechanism available for suppressing it is worse
/// than the disease:
///
/// * `modprobe -r cdc_ncm` returns **EBUSY even on a clean boot** with refcnt 0
///   and no holders, and `rmmod -f` removes the module but leaves the USB driver
///   registered so it can never be re-inserted without a reboot;
/// * `/sys/bus/usb/drivers_autoprobe=0` is global. Before the role switch it
///   stops the `hub` driver binding to the new root hub, so nothing enumerates
///   at all. After the root hub is up it still stops the **`usb` generic driver**
///   binding — and that driver is what SETS THE DEVICE CONFIGURATION, so the car
///   enumerates with no configuration and therefore no interfaces to match
///   (measured 2026-09-23: `usb 7-1: new high-speed USB device number 2` in the
///   kernel log, while our scan reported "no matching car device" for 20 s).
///
/// The remaining viable route is a parameter in the cdc_ncm module we already
/// patch and ship, so the alt-setting switch can be held off without touching
/// binding at all. That must be proven on the bench before it reaches a drive.
///
/// `RASCAR_DEFER_NCM_BIND=1` re-enables the (broken) suppression for bench work.
pub fn defer_ncm_bind() -> bool {
    defer_ncm_bind_from(std::env::var("RASCAR_DEFER_NCM_BIND").ok().as_deref())
}

/// Pure form of [`defer_ncm_bind`], so the default is testable without touching
/// the process environment.
pub fn defer_ncm_bind_from(raw: Option<&str>) -> bool {
    match raw.map(str::trim) {
        None | Some("") => false,
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"),
    }
}

/// CDC-NCM bring-up (netif + IPv6 link-local).  Non-fatal for iAP2, so a failure
/// is logged and reported as `None`.  Safe to run concurrently with iAP2 bring-up.
pub async fn bring_up_ncm(pre_ifaces: &HashSet<String>, car_sysfs: &Path) -> Option<NcmLink> {
    let link = bringup_ncm(pre_ifaces, Some(car_sysfs)).await;
    info!(
        "phase3: NCM bring-up done at t+{} ms: {}",
        crate::debug::elapsed_ms(),
        link.as_ref().map(|n| n.ifname.as_str()).unwrap_or("<none>")
    );
    link
}

/// mDNS discovery of the car's AirPlay/CarPlay receiver on the NCM link.
///
/// Must run AFTER [`bring_up_ncm`] has finished, never concurrently with it:
/// `avahi-browse -t` dumps the cache and exits after ~1s, while bring-up can
/// spend 5s scanning for the netif plus 10s waiting for SLAAC, so a parallel
/// browse finishes before the link the car answers on even exists.
pub async fn discover_airplay(ncm: Option<&NcmLink>) -> Vec<AirPlayEndpoint> {
    match ncm {
        Some(link) if link.link_local.is_some() => discover(Some(&link.ifname)).await,
        Some(link) => {
            warn!(
                "phase3: NCM link '{}' has no IPv6 link-local — skipping the mDNS browse, the car cannot answer on a link that is not up yet",
                link.ifname
            );
            Vec::new()
        }
        None => {
            warn!("phase3: no NCM link was brought up — skipping the mDNS browse (nothing to browse on)");
            Vec::new()
        }
    }
}

/// Scope guard that returns the SoC USB port to peripheral mode if Phase 3 exits
/// with an error after the role switch. Without it a single slow enumeration
/// leaves the port in host mode, and the *next* launch fails at "bind to UDC"
/// with an error that names the gadget, not the role switch — needing a reboot.
struct RoleRollback(Option<role_switch::RoleSwitch>);

impl RoleRollback {
    /// Keep the port in host mode (Phase 3 succeeded).
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RoleRollback {
    fn drop(&mut self) {
        if let Some(switched) = self.0.take() {
            warn!("phase3: failed after the USB role switch — restoring peripheral mode");
            switched.revert_to_device();
        }
    }
}

async fn bringup_ncm(pre_ifaces: &HashSet<String>, car_sysfs: Option<&Path>) -> Option<NcmLink> {
    match ncm::bringup(pre_ifaces, car_sysfs).await {
        Ok(link) => Some(link),
        Err(e) => {
            warn!("phase3: CDC-NCM bring-up failed (non-fatal for iAP2): {e:#}");
            None
        }
    }
}

async fn discover(ifname: Option<&str>) -> Vec<AirPlayEndpoint> {
    ncm::discover_airplay(MDNS_TIMEOUT, ifname).await
}

/// How long Phase 3 waits for `cdc_ncm` to finish binding before iAP2 starts.
/// Generous: the bind is ~250 ms and the car's own teardown watchdog is ~5-7 s.
const NCM_BIND_SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
const NCM_BIND_SETTLE_POLL: Duration = Duration::from_millis(10);

/// True once the kernel has registered a netdev under one of the car device's
/// USB interfaces. `cdc_ncm` registers it as the LAST step of its bind, after
/// `usb_set_interface(data_altsetting)`, so this is a precise "the alt-setting
/// switch is over" signal — far better than sleeping a guessed interval.
fn ncm_netdev_present(car_sysfs: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(car_sysfs) else {
        return false;
    };
    entries.flatten().any(|e| {
        // USB interface dirs are "<bus>-<port>:<config>.<interface>".
        e.file_name().to_string_lossy().contains(':') && e.path().join("net").is_dir()
    })
}

/// Block until `cdc_ncm` has finished binding to the car, or the timeout expires.
///
/// Best-effort on purpose: if the netdev never shows up we still run iAP2 rather
/// than abandoning the session — a missing netdev costs us Phase 4, but not
/// starting iAP2 at all costs us everything.
async fn wait_for_ncm_bind(car_sysfs: &Path) {
    let started = std::time::Instant::now();
    if ncm_netdev_present(car_sysfs) {
        info!("phase3: cdc_ncm already bound (netdev present) — no wait before iAP2");
        return;
    }
    while started.elapsed() < NCM_BIND_SETTLE_TIMEOUT {
        tokio::time::sleep(NCM_BIND_SETTLE_POLL).await;
        if ncm_netdev_present(car_sysfs) {
            info!(
                "phase3: cdc_ncm bind finished after {} ms — iAP2 may start without the \
                 NCM alt-setting switch landing mid-handshake",
                started.elapsed().as_millis()
            );
            return;
        }
    }
    warn!(
        "phase3: no NCM netdev after {} ms — starting iAP2 anyway. If iAP2 now hangs on a \
         bulk OUT, suspect the cdc_ncm alt-setting switch colliding with the handshake.",
        NCM_BIND_SETTLE_TIMEOUT.as_millis()
    );
}

#[cfg(test)]
mod ncm_bind_gate_tests {
    use super::*;

    /// Mirrors the real layout under a car device's sysfs node, e.g.
    /// `/sys/.../usb7/7-1/{7-1:1.0, 7-1:1.1, 7-1:1.2}` where cdc_ncm registers
    /// `7-1:1.1/net/usb0` as the LAST step of its bind.
    fn car_dir(with_net: bool) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for iface in ["7-1:1.0", "7-1:1.1", "7-1:1.2"] {
            std::fs::create_dir_all(d.path().join(iface)).unwrap();
        }
        // Non-interface siblings that must never be mistaken for one.
        std::fs::create_dir_all(d.path().join("power")).unwrap();
        std::fs::create_dir_all(d.path().join("ep_00")).unwrap();
        if with_net {
            std::fs::create_dir_all(d.path().join("7-1:1.1").join("net").join("usb0")).unwrap();
        }
        d
    }

    #[test]
    fn netdev_absent_until_cdc_ncm_finishes_binding() {
        let d = car_dir(false);
        assert!(
            !ncm_netdev_present(d.path()),
            "interfaces exist but no netdev yet — cdc_ncm is still binding, iAP2 must wait"
        );
    }

    #[test]
    fn netdev_present_once_bound() {
        let d = car_dir(true);
        assert!(ncm_netdev_present(d.path()), "usb0 registered — bind is over");
    }

    #[test]
    fn missing_sysfs_path_is_not_a_false_positive() {
        assert!(!ncm_netdev_present(Path::new("/definitely/not/here")));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_gives_up_instead_of_blocking_forever() {
        // A car whose NCM never binds must not cost us the whole session.
        let d = car_dir(false);
        let t = std::time::Instant::now();
        wait_for_ncm_bind(d.path()).await;
        assert!(t.elapsed() >= NCM_BIND_SETTLE_TIMEOUT, "must wait the full budget");
    }
}

/// Watch the car's sysfs node without touching the bus, and report exactly when
/// it disappears. Used only by the `RASCAR_ENUM_THEN_IDLE_MS` control experiment.
async fn idle_and_watch_car(car_sysfs: &Path, budget: Duration) {
    let t0 = Instant::now();
    info!(
        "**** CONTROL EXPERIMENT **** car enumerated at t+{} ms. Sending NOTHING for up to \
         {} ms. Watching {}",
        crate::debug::elapsed_ms(),
        budget.as_millis(),
        car_sysfs.display()
    );
    let mut last_log = Instant::now();
    while t0.elapsed() < budget {
        if !car_sysfs.exists() {
            warn!(
                "**** CONTROL EXPERIMENT RESULT **** car DISCONNECTED after {} ms with ZERO \
                 traffic from us — the ~6.4 s teardown is NOT triggered by anything we send",
                t0.elapsed().as_millis()
            );
            return;
        }
        if last_log.elapsed() >= Duration::from_secs(1) {
            info!("control experiment: still connected at {} ms", t0.elapsed().as_millis());
            last_log = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    info!(
        "**** CONTROL EXPERIMENT RESULT **** car STILL CONNECTED after {} ms of silence — the \
         teardown IS driven by the session, so it is ours to fix",
        budget.as_millis()
    );
}

/// Keep `cdc_ncm` OUT of the kernel until authentication completes.
///
/// Apple's CarPlay spec, under Figure 23-2: "Until authentication is completed
/// successfully, the Apple device will access the iAP2 interface." We normally do
/// the opposite — the kernel binds cdc_ncm the instant the car enumerates.
///
/// Doing this with the GLOBAL `/sys/bus/usb/drivers_autoprobe` was a mistake and
/// is why three runs at the car saw the device never enumerate at all: with
/// autoprobe off the device-level driver does not bind either, so the car is
/// never configured. Unloading just this one module is surgical, and registering
/// it again later makes usbcore probe the already-enumerated interfaces, so no
/// manual bind is needed.
/// Poll until a root hub is bound to the `hub` driver, i.e. the bus created by
/// the role switch can actually enumerate devices.
///
/// Returns false if that has not happened within the budget, in which case the
/// caller must NOT suppress automatic binding.
async fn wait_for_root_hub_bound(before: &HashSet<String>) -> bool {
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);
    let deadline = std::time::Instant::now() + BUDGET;
    loop {
        let now = bound_root_hubs();
        if let Some(fresh) = now.difference(before).next() {
            debug!("usb: new root hub {fresh} bound to the hub driver — safe to suppress autoprobe");
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Root-hub interfaces (`<bus>-0:1.0`) currently bound to the `hub` driver.
///
/// It must be a root hub that did NOT exist before the role switch: this board
/// always has buses 1-6, so "any bound root hub" is true before the switch even
/// starts and would let us suppress autoprobe too early — the very mistake this
/// check exists to prevent.
fn bound_root_hubs() -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.contains("-0:1.0") {
            continue;
        }
        let bound = std::fs::read_link(e.path().join("driver"))
            .map(|p| p.file_name().is_some_and(|d| d == "hub"))
            .unwrap_or(false);
        if bound {
            out.insert(name);
        }
    }
    out
}

/// Milliseconds to wait between acknowledging the car's `0x51` role-switch
/// request and actually flipping the controller to host.
///
/// CarLinKit exposes exactly this as a module parameter (`hnp_interval_ms`,
/// default **10**, range 0-1000) and persists a per-model override — for a
/// Kenwood DNX893S it forces 100 and reboots to apply it. So this is a real
/// per-head-unit compatibility axis, not a constant. Sweep 10/50/100/250 with
/// `RASCAR_ROLE_SWITCH_DELAY_MS` if everything else is right and the car still
/// drops us.
fn role_switch_delay_ms() -> u64 {
    role_switch_delay_from(std::env::var("RASCAR_ROLE_SWITCH_DELAY_MS").ok().as_deref())
}

fn role_switch_delay_from(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms <= 2000)
        .unwrap_or(10)
}

/// Pin the car's USB device awake.
///
/// A runtime-suspended device is a perfectly good reason for a head unit to end
/// a session, and this board's kernel is far newer than the 3.14 a CarLinKit
/// runs, where autosuspend is effectively off. Free to do, and it rules itself
/// out in one run. Best-effort: the node may not exist.
fn pin_device_awake(car_sysfs: &Path) {
    let path = car_sysfs.join("power/control");
    match std::fs::write(&path, "on") {
        Ok(()) => info!("usb: autosuspend disabled for the car ({})", path.display()),
        Err(e) => debug!("usb: could not disable autosuspend at {}: {e}", path.display()),
    }
}

const NCM_HOLD_PARAM: &str = "/sys/module/cdc_ncm/parameters/hold_apple_gadget";

/// Set only when the hold actually took, so the release path never acts on a
/// hold that was never there (an old module resident, or RASCAR_NCM_HOLD=0).
static NCM_HOLD_ENGAGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this attempt is holding cdc_ncm off the car right now.
pub fn ncm_hold_engaged() -> bool {
    NCM_HOLD_ENGAGED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether to keep `cdc_ncm` from touching the car until iAP2 authentication is
/// done, using the `hold_apple_gadget` parameter of our `rascar-3` module.
///
/// Default OFF (see the pure form below for why).
///
/// Original motivation (2026-09-23, 42 enumerated attempts): in 14 the
/// driver's first class requests hit the head unit's gadget while it was still
/// initialising, and either the bind failed (`bind() failure`) or the hub disabled
/// the port 1-12 ms later (`disabled by hub (EMI?)`), losing the attempt. CarLinKit
/// avoids exactly this by not loading cdc_ncm until its iAP2 session is up.
///
/// This replaces two mechanisms that could not work on this board: the module was
/// `[permanent]` (a struct-module layout mismatch in the build headers, fixed in
/// `rascar-3`) so it could never be unloaded, and `drivers_autoprobe=0` is global
/// and stops the car's device ever being configured. `RASCAR_NCM_HOLD=0` for an A/B.
pub fn ncm_hold_wanted() -> bool {
    ncm_hold_from(std::env::var("RASCAR_NCM_HOLD").ok().as_deref())
}

pub fn ncm_hold_from(raw: Option<&str>) -> bool {
    // DEFAULT OFF since the 2026-09-23 drive. The hold works mechanically (dmesg:
    // "RasCar: bind held ... device untouched") but it REFUTED its own premise:
    // the hub still disabled the port ("disabled by hub (EMI?)") with cdc_ncm
    // completely untouched, so cdc_ncm was never the cause — the earlier
    // "bind() failure" was the car's gadget already vanishing under the bind. And
    // with the hold engaged none of 3 attempts authenticated (one with a 4 s
    // bulk-OUT stall), against 2 of 3 without it. Opt in with RASCAR_NCM_HOLD=1.
    matches!(
        raw.map(str::trim),
        Some("1") | Some("on") | Some("true") | Some("yes")
    )
}

/// Set the module's hold parameter and read it back. Returns whether it took.
fn set_ncm_hold(on: bool) -> bool {
    let want = if on { "Y" } else { "N" };
    if let Err(e) = std::fs::write(NCM_HOLD_PARAM, if on { "1" } else { "0" }) {
        warn!("usb: cannot write {NCM_HOLD_PARAM}: {e} (is cdc_ncm rascar-3 loaded?)");
        return false;
    }
    match std::fs::read_to_string(NCM_HOLD_PARAM) {
        Ok(v) if v.trim() == want => true,
        Ok(v) => {
            warn!("usb: {NCM_HOLD_PARAM} reads back {:?}, wanted {want}", v.trim());
            false
        }
        Err(e) => {
            warn!("usb: cannot read back {NCM_HOLD_PARAM}: {e}");
            false
        }
    }
}

/// Engage the hold before the car can enumerate. FAILS OPEN: if the parameter is
/// missing (an older cdc_ncm) we carry on exactly as before and say so loudly —
/// the collision costs a third of attempts, but a wedged attempt costs all of it.
pub fn engage_ncm_hold() -> bool {
    if !ncm_hold_wanted() {
        info!("usb: cdc_ncm hold DISABLED (RASCAR_NCM_HOLD=0) — it binds the moment the car appears");
        return false;
    }
    if set_ncm_hold(true) {
        NCM_HOLD_ENGAGED.store(true, std::sync::atomic::Ordering::SeqCst);
        info!("usb: cdc_ncm hold ENGAGED — the driver will not touch the car until after iAP2 auth");
        true
    } else {
        warn!("usb: could NOT engage the cdc_ncm hold — continuing without it (collision risk ~1 in 3)");
        false
    }
}

/// Release the hold and bind the car's NCM interface explicitly.
pub fn release_ncm_hold_and_bind(car_sysfs: &Path) {
    NCM_HOLD_ENGAGED.store(false, std::sync::atomic::Ordering::SeqCst);
    if set_ncm_hold(false) {
        info!("usb: cdc_ncm hold released");
    } else {
        warn!("usb: could not release the cdc_ncm hold — binding anyway");
    }
    bind_ncm_now(car_sysfs);
}

/// Log whether the board drives VBUS on the car's USB-C port.
///
/// The car's port already supplies 5 V; if our `vbus5v0_typec` regulator were
/// enabled too, two supplies would fight on one wire (brown-outs, back-feed).
/// On the bench it is disabled with no users; this records the state in the car.
fn log_typec_vbus_state(when: &str) {
    let Ok(dir) = std::fs::read_dir("/sys/class/regulator") else { return };
    for entry in dir.flatten() {
        let path = entry.path();
        let name = std::fs::read_to_string(path.join("name")).unwrap_or_default();
        if name.trim() == "vbus5v0_typec" {
            let state = std::fs::read_to_string(path.join("state")).unwrap_or_default();
            let users = std::fs::read_to_string(path.join("num_users")).unwrap_or_default();
            info!("power: vbus5v0_typec {} (users {}) {when}", state.trim(), users.trim());
            return;
        }
    }
}

/// Whether to reset the car's USB device from the host side right after iAP2
/// authentication (opt in with `RASCAR_POST_AUTH_RESET=1`).
///
/// Test of 2026-09-25 (three-plane corpus review): the car decides to end the
/// session at AuthenticationSucceeded + 2.14 s (sd ~0.07 s over 17/24 runs) and
/// detaches 2.09 s later; nothing in iAP2 or AirPlay moves it. Apple's CarPlay
/// Figure 23-2 has a SECOND enumeration after authentication, after which the
/// Apple device evaluates all interfaces. We never trigger one; the hypothesis is
/// that the Apple device, being the USB host, does it and the car waits for it.
pub fn post_auth_reset_wanted() -> bool {
    post_auth_reset_from(std::env::var("RASCAR_POST_AUTH_RESET").ok().as_deref())
}

pub fn post_auth_reset_from(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1") | Some("on") | Some("true") | Some("yes"))
}

/// Port-reset the car's USB device (`USBDEVFS_RESET` on its usbfs node): the
/// kernel re-enumerates it in place, which the car's gadget sees as a bus reset.
pub fn reset_car_device(car_sysfs: &Path) -> Result<()> {
    let read = |attr: &str| -> Result<u32> {
        let raw = std::fs::read_to_string(car_sysfs.join(attr))
            .with_context(|| format!("reading {}/{attr}", car_sysfs.display()))?;
        raw.trim().parse::<u32>().with_context(|| format!("parsing {attr}={raw:?}"))
    };
    let (bus, dev) = (read("busnum")?, read("devnum")?);
    let node = format!("/dev/bus/usb/{bus:03}/{dev:03}");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .with_context(|| format!("opening {node}"))?;
    // USBDEVFS_RESET = _IO('U', 20).
    const USBDEVFS_RESET: libc::c_ulong = 0x5514;
    let started = std::time::Instant::now();
    let rc = unsafe { libc::ioctl(std::os::fd::AsRawFd::as_raw_fd(&file), USBDEVFS_RESET as _, 0) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("USBDEVFS_RESET on {node} failed: {err}");
    }
    info!("usb: car device {node} port-reset in {} ms", started.elapsed().as_millis());
    Ok(())
}

/// Best-effort: never leave the hold set when this process exits, or the next
/// NCM device on the board would silently refuse to bind.
pub fn clear_ncm_hold_at_exit() {
    if std::path::Path::new(NCM_HOLD_PARAM).exists() && !set_ncm_hold(false) {
        warn!("usb: could not clear the cdc_ncm hold at exit");
    }
}

const USB_AUTOPROBE: &str = "/sys/bus/usb/drivers_autoprobe";

/// Set usbcore's automatic driver binding and **verify it took**.
///
/// Never assume a sysfs write landed: the previous mechanism here was
/// `modprobe -r cdc_ncm`, which returns `EBUSY` on this board (the driver stays
/// registered after a forced unload) while our code only logged a warning and
/// carried on — so the deferral silently did nothing while the log claimed it
/// was active.
fn set_usb_autoprobe(on: bool) -> bool {
    let want = if on { "1" } else { "0" };
    if let Err(e) = std::fs::write(USB_AUTOPROBE, want) {
        warn!("usb: cannot write {USB_AUTOPROBE}={want}: {e}");
        return false;
    }
    match std::fs::read_to_string(USB_AUTOPROBE) {
        Ok(v) if v.trim() == want => true,
        Ok(v) => {
            warn!("usb: {USB_AUTOPROBE} reads back {:?}, wanted {want}", v.trim());
            false
        }
        Err(e) => {
            warn!("usb: cannot read back {USB_AUTOPROBE}: {e}");
            false
        }
    }
}

/// Restores automatic binding whatever happens, including on an error path.
/// Leaving it off would stop the kernel binding ANY new USB device on the board.
struct AutoprobeGuard(bool);
impl Drop for AutoprobeGuard {
    fn drop(&mut self) {
        if self.0 && set_usb_autoprobe(true) {
            info!("usb: automatic driver binding restored");
        }
    }
}

/// Keep every kernel driver off the car's interfaces while it enumerates, so
/// `cdc_ncm` cannot issue its `SET_INTERFACE` in the middle of the iAP2
/// handshake. See [`defer_ncm_bind`] for the measurement that motivates it.
fn suppress_kernel_autobind() -> AutoprobeGuard {
    if set_usb_autoprobe(false) {
        info!("usb: automatic driver binding OFF until the car has enumerated \
               (keeps cdc_ncm's SET_INTERFACE out of the iAP2 handshake)");
        AutoprobeGuard(true)
    } else {
        warn!("usb: could NOT disable automatic driver binding — cdc_ncm will bind during the \
               handshake and the ~750 ms bulk-OUT stall is expected to return");
        AutoprobeGuard(false)
    }
}

/// Load `cdc_ncm` now that authentication is done. Registering the driver makes
/// usbcore probe every already-enumerated interface, which binds the car's NCM
/// interfaces without any manual bind write.
pub fn bind_ncm_now(car_sysfs: &Path) {
    // The module may or may not be resident; either is fine, we only need the
    // driver registered before the explicit bind below.
    match std::process::Command::new("modprobe").arg("cdc_ncm").status() {
        Ok(st) if st.success() => debug!("usb: cdc_ncm present"),
        Ok(st) => warn!("usb: modprobe cdc_ncm exited {st} (continuing — it may be resident)"),
        Err(e) => warn!("usb: could not run modprobe cdc_ncm: {e}"),
    }

    // Autoprobe was off while the car enumerated, so nothing bound its NCM
    // interface. Bind it by hand now that authentication is done.
    let Some(iface) = ncm_interface_dir(car_sysfs) else {
        warn!("usb: no CDC-NCM interface found under {} — cannot bind cdc_ncm", car_sysfs.display());
        return;
    };
    let bind = Path::new("/sys/bus/usb/drivers/cdc_ncm/bind");
    match std::fs::write(bind, &iface) {
        Ok(()) => {
            let bound = Path::new("/sys/bus/usb/drivers/cdc_ncm").join(&iface).exists();
            if bound {
                info!("usb: cdc_ncm bound to {iface} post-authentication — its SET_INTERFACE \
                       now lands outside the iAP2 handshake");
            } else {
                warn!("usb: wrote {iface} to cdc_ncm/bind but the driver link did not appear");
            }
        }
        // EBUSY here means something already claimed it, which is not fatal.
        Err(e) => warn!("usb: binding cdc_ncm to {iface} failed: {e}"),
    }
}

/// Name of the car's CDC-NCM interface directory (e.g. `7-1:1.1`), found by its
/// class rather than by guessing the interface number.
fn ncm_interface_dir(car_sysfs: &Path) -> Option<String> {
    let entries = std::fs::read_dir(car_sysfs).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.contains(':') {
            continue;
        }
        let class = std::fs::read_to_string(p.join("bInterfaceClass")).unwrap_or_default();
        let sub = std::fs::read_to_string(p.join("bInterfaceSubClass")).unwrap_or_default();
        // CDC communications class 0x02, subclass 0x0d = Network Control Model.
        if class.trim().eq_ignore_ascii_case("02") && sub.trim().eq_ignore_ascii_case("0d") {
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod post_auth_reset_tests {
    use super::*;

    #[test]
    fn post_auth_reset_is_opt_in() {
        assert!(!post_auth_reset_from(None));
        assert!(!post_auth_reset_from(Some("0")));
        assert!(post_auth_reset_from(Some("1")));
        assert!(post_auth_reset_from(Some(" on ")));
    }
}

#[cfg(test)]
mod defer_ncm_tests {
    use super::{defer_ncm_bind_from, ncm_hold_from, role_switch_delay_from};

    #[test]
    fn the_ncm_hold_is_opt_in_because_the_drive_refuted_it() {
        // 2026-09-23: EMI port-disable happened with cdc_ncm held and untouched,
        // and 0 of 3 held attempts authenticated.
        assert!(!ncm_hold_from(None));
        assert!(!ncm_hold_from(Some("")));
        assert!(!ncm_hold_from(Some("0")));
        for on in ["1", "on", "true", "yes", " 1 "] {
            assert!(ncm_hold_from(Some(on)), "{on:?} should opt in");
        }
    }

    #[test]
    fn role_switch_delay_matches_carlinkits_default_and_sweeps() {
        // CarLinKit's g_iphone.ko: hnp_interval_ms default 10, range 0-1000,
        // overridden per head unit (100 for a Kenwood DNX893S).
        assert_eq!(role_switch_delay_from(None), 10);
        for v in ["0", "50", "100", "250"] {
            assert_eq!(role_switch_delay_from(Some(v)), v.parse::<u64>().unwrap());
        }
        assert_eq!(role_switch_delay_from(Some("99999")), 10, "absurd values fall back");
        assert_eq!(role_switch_delay_from(Some("nope")), 10);
    }

    #[test]
    fn deferring_is_off_by_default_because_every_mechanism_breaks_enumeration() {
        // Suppressing autoprobe stops the `usb` generic driver setting the device
        // configuration, so the car enumerates with no interfaces at all. Two
        // car trips were lost to this. Off until a mechanism is proven on a bench.
        assert!(!defer_ncm_bind_from(None));
        assert!(!defer_ncm_bind_from(Some("")));
    }

    #[test]
    fn it_can_be_turned_on_explicitly_for_bench_work() {
        for on in ["1", "on", "true", "yes", "ON", " 1 "] {
            assert!(defer_ncm_bind_from(Some(on)), "{on:?} should enable deferring");
        }
    }

    #[test]
    fn anything_unrecognised_stays_off() {
        for junk in ["0", "off", "false", "no", "maybe", "2"] {
            assert!(!defer_ncm_bind_from(Some(junk)), "{junk:?} must not defer");
        }
    }
}
