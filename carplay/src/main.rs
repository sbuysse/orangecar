//! RasCar CarPlay Transmitter — Main Orchestrator (Path A, Phase 1).
//!
//! # What this is
//!
//! Path A of the CarPlay transmitter: the OrangePi 5 Plus impersonates an Apple
//! iPhone to a car head unit over USB, so the car starts its CarPlay attach
//! sequence towards us instead of Android Auto.  The car performs **no MFi/cert
//! check on the device** at attach time (primary-source verified), which is why
//! impersonating the USB identity is viable.
//!
//! # Phase 1 scope (this binary)
//!
//! - Enumerate to the car as an **Apple device** (VID 0x05AC "Apple Inc.",
//!   PID 0x12A8 "iPhone", USB 2.00) via configfs + FunctionFS.
//! - Capture and LOG every ep0 control request the car issues, and specifically
//!   recognise the **CarPlay USB role-switch vendor request**
//!   (`bRequest=0x51`, `bmRequestType=0x40`, `wValue=1`, plus the `0x53`/`0xC0`
//!   read variant).
//!
//! Phase 1 is a *capture / proof-of-reachability* milestone.  We do **not**
//! implement iAP2, CDC-NCM, AirPlay, or the actual device→host controller role
//! transition here — those are later phases (see below).
//!
//! # Roadmap (not in this binary)
//!   - Phase 2: iAP2 link layer over USB (Device-side identify / accept-all auth)
//!   - Phase 3: USB host role transition + CDC-NCM tunnel + IPv6 LL + Bonjour
//!   - Phase 4: AirPlay pair + FairPlay SAP (GO/NO-GO gate)
//!   - Phase 5: HEVC/H.264 dashboard encode + AirPlay sender + HID touch
//!
//! Protocol notes: `docs/protocol.md` at the repository root.
//!
//! # Running
//!
//! ```bash
//! sudo cargo run --bin rascar-carplay -- --debug
//! ```
//! Requires root (configfs / FunctionFS) and the rk3588-dwc3-peripheral overlay.
//!
//! # Debug instrumentation
//!
//! - `--debug` → debug level (decoded ep0 answers, role snapshots per attempt,
//!   USB inventory, avahi raw output, RTSP heads).
//! - `RASCAR_WIRE_TRACE=1` → additionally hex-dumps USB bulk transfers, iAP2 link
//!   headers + control messages and RTSP bodies (see `debug.rs`).
//! - Grep-able markers: `Phase <X> START` / `Phase <X> END`, `FFS event`,
//!   `ROLE-SWITCH RESULT`, `iAP2 LINK STATE`, `iAP2 DEVICE STATE`, `PANIC`.

mod airplay;
mod debug;
mod gps_clock;
mod headunit;
mod host;
mod iap2;
mod ntp_time;
mod stream;
mod usb;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::Read;
use tracing::{error, info, warn};

use debug::PhaseTimer;
use usb::{ffs, gadget};

/// The single device MAC we present EVERYWHERE we claim an identity — the
/// `_carplay-ctrl` TXT `id`, the AirPlay SETUP `deviceID`/`macAddress`, and the
/// mirroring stream SETUP. A real iPhone uses one MAC across all of these; we had
/// four different placeholders, which a car that correlates the advertised id with
/// the AirPlay identity could reject. Apple OUI `d0:81:7a` for plausibility; the
/// per-session `sessionUUID` stays random (as on a real device).
pub const DEVICE_MAC: &str = "d0:81:7a:c0:ff:ee";

#[derive(Parser)]
#[command(name = "rascar-carplay", about = "RasCar CarPlay Transmitter (Phase 1: Apple enumeration + role-switch capture)")]
struct Cli {
    /// Enable debug logging (per-request hex, descriptor byte counts).
    #[arg(long)]
    debug: bool,

    /// Skip Phases 1-3 (USB gadget, role switch, iAP2, mDNS) and run the
    /// AirPlay handshake + streaming directly against `HOST:PORT`.
    ///
    /// This is the bench entry point: Phases 1-3 need the board, a USB role
    /// switch and a car, none of which exist on a laptop. Phases 4-5 need only
    /// a TCP peer, so pointing this at a local AirPlay receiver runs the whole
    /// real sender path — render, x11grab, H.264, mirror packetisation,
    /// encryption, RTSP — with no hardware.
    ///
    ///   rascar-carplay --airplay-endpoint 127.0.0.1:7000
    ///
    /// Against UxPlay you get a picture (it renders H.264) but NO proof of the
    /// FairPlay transform — it accepts any m3. Against doubletake's
    /// `cmd/doubletake-test-receiver` you get no picture but a real m3 check.
    #[arg(long, value_name = "HOST:PORT")]
    airplay_endpoint: Option<String>,

    /// Bench: skip Phases 1-3 and run the **CarPlay** session (`run_carplay` —
    /// SETUP /setup → 466?→pair(3939) → GET /info → RECORD → modesChanged →
    /// screen SETUP → H.264 stream) directly against `HOST:PORT`. Point it at a
    /// CarPlay receiver (catplay RX) on the LAN to test the whole sender path with
    /// no board/car. Differs from `--airplay-endpoint`, which runs the AppleTV
    /// FairPlay path instead.
    #[arg(long, value_name = "HOST:PORT")]
    carplay_endpoint: Option<String>,

    /// Bench: run ONLY the `_carplay-ctrl` raw mDNS responder on the given
    /// `fe80::…%iface` link-local for ~30 s, then exit. Lets us capture the
    /// responder's real wire output and fire a live query at it — validating the
    /// deployed binary's packet + socket + query-answer path with no car.
    #[arg(long, value_name = "LINKLOCAL%IFACE")]
    mdns_selftest: Option<String>,

    /// Bench: run the car-audio capture for 3 s exactly as a CarPlay session
    /// would (same PulseAudio discovery, same default-output monitor), print
    /// what it found and how loud the captured audio was, then exit.
    #[arg(long)]
    audio_probe: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything else: pin the log clock and make sure a panic reaches the
    // log file (the default hook only writes to stderr, which may not be captured).
    debug::init_clock();
    install_panic_hook();

    let cli = Cli::parse();

    let filter = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    log_startup_banner(cli.debug);

    if cli.audio_probe {
        return audio_probe().await;
    }

    // Bench path: skip straight to the AirPlay phases against a TCP peer.
    // Deliberately reuses `airplay::run` + `stream::run` unchanged, so what runs
    // here is the same code that runs at a car — only Phases 1-3 are absent.
    if let Some(target) = cli.airplay_endpoint.clone() {
        return run_airplay_only(&target).await;
    }
    if let Some(target) = cli.carplay_endpoint.clone() {
        return run_carplay_only(&target).await;
    }
    if let Some(ll) = cli.mdns_selftest.clone() {
        info!("mdns-selftest: running _carplay-ctrl responder on {ll} for 30s (offline wire validation)");
        host::ctrl_advertise::publish_ncm_address(&ll);
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        info!("mdns-selftest: done");
        return Ok(());
    }

    // Pre-warm the video render pipeline (Xvfb + Chromium) NOW, before the ~6 s USB
    // handshake, so it is painting by the time Phase 4/5 reaches screen SETUP. The
    // car tears the CarPlay session down at a FIXED ~7.8 s from enumeration; cold-
    // starting Chromium at screen SETUP (the end of the RTSP handshake) never yields
    // a keyframe in time. Held for the process lifetime; run_carplay's own
    // ensure_dashboard then finds this X server up and reuses it (instant ffmpeg
    // keyframe). 1200×512 matches the Jaguar F-PACE display negotiated at /info.
    //
    // Bounded: it runs BEFORE the USB gadget exists, so a render start that
    // hangs keeps the car from ever seeing us (2026-09-30 drive 3: a wedged
    // compositor + unbounded wlr-randr, no connection all drive). Past the cap
    // the half-started kiosk is dropped (killed) and screen SETUP cold-starts.
    let _render_prewarm = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        stream::render::ensure_dashboard(1200, 512),
    )
    .await
    {
        Ok(Ok(guard)) => Some(guard),
        Ok(Err(e)) => {
            warn!("render pre-warm failed (will cold-start at screen SETUP): {e:#}");
            None
        }
        Err(_) => {
            warn!("render pre-warm did not finish within 20 s — dropped, carrying on with the USB start (will cold-start at screen SETUP)");
            None
        }
    };

    // Advertise `_carplay-ctrl._tcp` NOW, before the USB handshake, so avahi has
    // probed + committed the record long before the car browses it (~t+3 s) and can
    // answer authoritatively on the NCM link the instant it appears. Held for the
    // process. Without it the car tears the session down when it cannot resolve
    // `_carplay-ctrl` in time.
    let _ctrl_adv = host::ctrl_advertise::spawn();

    // Android persona probe (RASCAR_PERSONA=android): no Apple gadget at all.
    if usb::aoa_probe::enabled() {
        return usb::aoa_probe::run().await;
    }

    info!("=== RasCar CarPlay Transmitter — Phase 1 (Apple enumeration + role-switch capture) ===");

    // Phase 1a: USB gadget + FunctionFS setup, presenting an Apple iPhone identity.
    info!("=== Phase 1a: USB Gadget Setup (Apple iPhone identity) ===");
    let phase = PhaseTimer::start("1a");
    let created = gadget::create()
        .context("gadget setup failed — ensure the rk3588-dwc3-peripheral overlay is enabled and run as root");
    phase.end(&created);
    created?;

    // From here on the configfs gadget and the FunctionFS mount exist, so NOTHING
    // may return early: every path has to reach the teardown below.  An early `?`
    // used to leak both — most easily on a busy UDC, which fails in `bind()` and
    // leaves the port half-configured for the next run.
    let run_result = run_gadget_session().await;

    if let Err(e) = &run_result {
        error!("carplay run ended with error: {e:#}");
        // Come back as a different iPhone. See gadget::rotate_serial_for_next_attempt.
        gadget::rotate_serial_for_next_attempt();
    }

    // Tear down so we leave configfs clean for the next run (or the AA gadget).
    host::clear_ncm_hold_at_exit();
    info!("=== Cleanup: removing gadget ===");
    if let Err(e) = gadget::remove() {
        error!("gadget cleanup failed: {e:#}");
    }
    info!("carplay transmitter shutdown complete (t+{} ms)", debug::elapsed_ms());

    run_result
}

/// Route panics through `tracing::error!` (with location) before the default
/// hook runs, so a crash is recorded in the same log as everything else.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let thread = std::thread::current();
        error!(
            "PANIC in thread '{}' at {location} (t+{} ms): {payload}",
            thread.name().unwrap_or("<unnamed>"),
            debug::elapsed_ms()
        );
        default_hook(info);
    }));
}

/// One-time context line so every log identifies the exact binary and host.
fn log_startup_banner(debug_level: bool) {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    info!(
        "rascar-carplay v{} build={} kernel={} euid={euid} pid={} log_level={} wire_trace={} ({}={:?})",
        env!("CARGO_PKG_VERSION"),
        option_env!("RASCAR_BUILD_ID").unwrap_or("dev"),
        debug::read_attr("/proc/sys/kernel/osrelease"),
        std::process::id(),
        if debug_level { "debug" } else { "info" },
        debug::wire_trace(),
        debug::WIRE_TRACE_ENV,
        std::env::var(debug::WIRE_TRACE_ENV).ok(),
    );
    info!("argv: {:?}", std::env::args().collect::<Vec<_>>());
}

/// Everything that runs while the gadget exists: FunctionFS init, UDC bind, ep0
/// capture, and — once the car asks for it — the Phase 3/2/4/5 hand-off.
///
/// Split out of `main` so a failure anywhere inside still returns through the
/// single unconditional `gadget::remove()` in `main`.
async fn run_gadget_session() -> Result<()> {
    // Phase 1b: open every configuration's ep0 and write its descriptors (must
    // happen before bind).  A partial failure drops the set, closing what opened.
    info!("=== Phase 1b: FunctionFS Init ({} configurations) ===", ffs::CONFIGS.len());
    let phase = PhaseTimer::start("1b");
    let opened: Result<ffs::Ep0Set> = tokio::task::spawn_blocking(ffs::Ep0Set::open_all)
        .await
        .context("joining the FunctionFS init task")
        .and_then(|r| r);
    phase.end(&opened);
    let ep0s = opened?;

    // Phase 1c: Bind to the UDC — the car now sees an Apple device and enumerates.
    info!("=== Phase 1c: Bind to UDC (device visible to car) ===");
    let phase = PhaseTimer::start("1c");
    let bound = gadget::bind();
    phase.end(&bound);
    let udc = bound?;

    // Phase 1d: answer control requests on every ep0 until the role switch.
    info!("=== Phase 1d: Capture Car Control Requests ===");
    info!("Plug the Pi into the car's USB port. Expected log milestones: UDC speed=high-speed, FFS event ENABLE [c.2], C0 53 answered, then '**** CARPLAY ROLE-SWITCH REQUEST ****'.");

    let phase = PhaseTimer::start("1d");
    // Opt-in (RASCAR_IAP2_HID=1): answer the car's device-mode iAP2 over HID, the
    // path a never-seen serial gets before any CarPlay role switch (InControl).
    let hid_iap2 = if usb::hid_iap2::enabled() {
        info!("RASCAR_IAP2_HID on: device-mode iAP2-over-HID responder started on {}", usb::hid_iap2::HID_IN_EP);
        let challenge = random_challenge().context("generating the HID-mode auth challenge")?;
        Some(tokio::spawn(usb::hid_iap2::run_device_mode(challenge)))
    } else {
        None
    };
    // Consumes the set: every ep0 fd is closed when this returns.
    let capture = ffs::capture_control_requests(ep0s, udc).await;
    phase.end(&capture);
    if let Some(task) = hid_iap2 {
        task.abort();
        ffs::set_hid_iap2_sink(None);
        info!("device-mode iAP2-over-HID responder stopped (Phase 1d ended)");
    }

    match capture {
        // All ep0 fds are closed; Phase 3 fully removes the gadget before the switch.
        Ok(ffs::CaptureOutcome::RoleSwitchRequested) => run_phase3_and_iap2().await,
        Err(e) => {
            error!("ep0 capture ended: {e:#}");
            Err(e)
        }
    }
}

/// How many times to re-acquire the car and restart iAP2 bring-up after it drops
/// off the bus mid-handshake, before giving the attempt up.
const IAP2_BRINGUP_RETRIES: u32 = 2;

/// How many times we follow the accessory through a POST-AUTHENTICATION
/// re-enumeration before giving up.
///
/// Apple's CarPlay spec, Figure 23-2 "Process for establishing a CarPlay
/// session", has TWO enumerations:
///
///   Enumerate (Apple device is USB Host)
///     "Apple device only evaluates the iAP2 interface."
///   Establish iAP2 Session -> Negotiate Link Parameters -> Authenticate
///   Enumerate (Apple device is USB Host)              <-- SECOND ENUMERATION
///     "Apple device evaluates all interfaces in the configuration."
///   Establish CarPlay Session
///
/// So the accessory detaching shortly after authentication is NORMAL, not a
/// failure. Measured at the Jaguar F-PACE: device life was 6293-6630 ms from
/// enumeration across 8 runs, IDENTICALLY whether we streamed video or never
/// started AirPlay at all (RASCAR_SKIP_PHASE4), and the car re-presents the same
/// 117-byte configuration ~0.5 s later as a new address. We used to treat that as
/// a fatal "device disconnected" and throw the attempt away, which is why the
/// dashboard only ever appeared for about a second.
const POST_AUTH_REENUM_CYCLES: u32 = 3;
/// The accessory came back ~0.5 s later in every capture; allow plenty of slack.
/// The accessory sometimes re-presents its gadget within ~0.3 s (measured:
/// device 2 gone at 12:21:09.739, device 3 up at 12:21:10.013). It never comes
/// back LATER than that on its own: the kernel logs across the whole
/// 2026-09-21 session show no new `Gadget MFi` until WE revert the port to
/// peripheral mode and re-present the iPhone gadget. So a long wait here is pure
/// dead time — 10 s of it, on 9 runs of that session — and dead time is the
/// scarcest thing we have when someone is sitting in a car.
fn post_auth_reenum_timeout() -> std::time::Duration {
    reenum_timeout_from(std::env::var("RASCAR_REENUM_TIMEOUT_MS").ok().as_deref())
}

fn reenum_timeout_from(raw: Option<&str>) -> std::time::Duration {
    std::time::Duration::from_millis(
        raw.and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms <= 30_000)
            .unwrap_or(1500),
    )
}
/// How long to wait for the car to re-enumerate on such a retry. It normally
/// comes back in well under a second.
const CAR_REACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Whether to bring the CDC-NCM link up concurrently with the iAP2 handshake
/// instead of after authentication.
///
/// Default OFF (see the pure form below for why).
///
/// Original motivation: with the post-auth ordering the
/// `_carplay-ctrl._tcp` service only went on air at role-switch **+2.87 s**
/// (range 2.43-3.90, n=14) inside a budget that ends at ~6.0-6.9 s, and in every
/// one of 37+ archived runs the car browsed, cached our PTR, and then never
/// resolved or connected. `RASCAR_EARLY_NCM=0` restores the old ordering for an A/B.
fn early_ncm_bringup() -> bool {
    early_ncm_from(std::env::var("RASCAR_EARLY_NCM").ok().as_deref())
}

fn early_ncm_from(raw: Option<&str>) -> bool {
    // DEFAULT OFF since the 2026-09-23 drive: started during the iAP2 handshake,
    // the bring-up selected the netdev as `usb0`, then udev renamed it to
    // `enu1i1` 0.8 s later ("renamed from usb0 (while UP)") and the link-local
    // wait hung on the vanished name — so AirPlay never started and no dashboard
    // appeared, in every attempt. This is exactly the failure the comment on the
    // post-auth bring-up already documented; it was overridden on the grounds that
    // the interface is SELECTED by sysfs, but the rename bites after selection.
    // Opt in with RASCAR_EARLY_NCM=1 only once the bring-up re-resolves the name.
    matches!(
        raw.map(str::trim),
        Some("1") | Some("on") | Some("true") | Some("yes")
    )
}

/// True when an error chain reports the USB device going away, as opposed to a
/// protocol or timeout failure. Matched on the rendered chain because the cause
/// arrives wrapped in several layers of context.
fn is_device_disconnect(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("device disconnected") || text.contains("No such device")
}

/// How many empty `_airplay._tcp` discovery rounds to accept before dialling the
/// car directly. Each round costs ~1 s of a session budget measured at ~5 s.
/// Discovery attempts (1 s apart) before we stop waiting for the car's
/// `_airplay._tcp` advertisement and dial it directly.
///
/// Was 3, which made sense when the car tore the USB gadget down ~6.4 s after
/// enumeration and every second counted. Binding cdc_ncm only AFTER iAP2
/// authentication removed that teardown (measured: 0 disconnects), and the cost
/// is that the car's AirPlay service now comes up LATER than our first browse —
/// avahi legitimately returned 0 endpoints three times in a row and we dialled a
/// receiver that was not listening yet. We have the time now; use it.
const FALLBACK_AFTER_DISCOVERY_ATTEMPTS: u32 = 12;
/// How long Phase 4 keeps re-discovering and re-dialling before giving up.
const PHASE4_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
/// The AirPlay receiver's RTSP port.
const AIRPLAY_PORT: u16 = 7000;

/// Phase 3 (USB host transition + iAP2 transport + CDC-NCM + Bonjour) followed by
/// Phase 2 (iAP2 Device bring-up) over the resulting host transport.
async fn run_phase3_and_iap2() -> Result<()> {
    // Phase 3 hands us a `UsbHostTransport` (the car's iAP2 bulk interface) plus
    // the NCM link + discovered AirPlay endpoint(s). It first removes the whole
    // gadget (callback: UDC unbind + configs + FunctionFS umount), then unbinds
    // fusb302 and writes dwc3 mode=host — the hardware-proven order.
    let phase = PhaseTimer::start("3");
    let phase3 = host::run(gadget::remove).await.context("Phase 3");
    phase.end(&phase3);
    let outcome = phase3?;

    info!(
        "phase3 outcome: car vid=0x{:04x} pid=0x{:04x}; iAP2 iface #{}; ncm iface {:?}",
        outcome.car.vendor_id,
        outcome.car.product_id,
        outcome.car.iap2_interface,
        outcome.car.ncm_interface,
    );

    // Phase 2 over the Phase 3 transport: drive the Device-side iAP2 stack.
    // It starts the moment the interface is claimed — the car sends DETECT +
    // SYN right away and drops the link ~4 s after enumerating without a
    // SYN|ACK.
    info!("=== Phase 2 (over Phase 3 USB host transport): iAP2 Device bring-up ===");
    let challenge = random_challenge().context("generating 32-byte auth challenge")?;
    let mut transport = outcome.transport;
    let mut car_sysfs = outcome.car_sysfs;

    // ---- EARLY NCM BRING-UP (2026-09-23) ----------------------------------
    //
    // The car browses `_carplay-ctrl._tcp` within its first couple of seconds and,
    // per AIS 23.3.1.1, IT is the initiator: "Accessories must use this service to
    // discover CarPlay enabled devices and to initiate the CarPlay session".
    // Apple's own reference accessory gives that chain a 5 s Bonjour resolve plus a
    // 2 s connect, retried 5x — inside a budget a shipping CarLinKit dongle
    // hard-codes at 6000 ms, and which we measure at 5.99 s +/- 0.03 on cold runs.
    //
    // We used to bring NCM up only AFTER authentication, which put the service on
    // air at role-switch **+2.87 s** (n=14, measured) — leaving the car ~4 s for a
    // chain that needs more. The captures show the consequence: the car browses,
    // caches our PTR, then never resolves and never opens TCP to our port, in all
    // 37+ archived runs.
    //
    // So start it the moment the car's device exists, CONCURRENTLY with the iAP2
    // handshake. It is a spawned task, never joined ahead of the iAP2 service loop:
    // joining it in front starved that loop once before and the car dropped us
    // 1.2 s after SessionReady. If it has not finished by the time Phase 4 needs a
    // link, Phase 4 falls back to doing the bring-up itself, exactly as before.
    // With the cdc_ncm hold engaged there is no NCM netdev until after
    // authentication, so an early bring-up could only wait for it. Skip it.
    if host::ncm_hold_engaged() && early_ncm_bringup() {
        info!("ncm: early bring-up skipped — the cdc_ncm hold keeps the link down until iAP2 auth");
    }
    let mut early_ncm = if early_ncm_bringup() && !host::ncm_hold_engaged() {
        let pre = outcome.pre_ifaces.clone();
        let sysfs = car_sysfs.clone();
        info!("ncm: starting EARLY bring-up concurrently with the iAP2 handshake \
               (RASCAR_EARLY_NCM=0 to restore the post-auth ordering)");
        Some(tokio::spawn(async move {
            let ncm = host::bring_up_ncm(&pre, &sysfs).await;
            if let Some(ll) = ncm.as_ref().and_then(|n| n.link_local.as_deref()) {
                host::ctrl_advertise::publish_ncm_address(ll);
                info!(
                    "ncm: EARLY bring-up done at t+{} ms — _carplay-ctrl can answer now",
                    crate::debug::elapsed_ms()
                );
            }
            ncm
        }))
    } else {
        None
    };


    // The car regularly detaches and re-enumerates during the first seconds of
    // bring-up (device 2 -> disconnect -> device 3). Our handle then points at the
    // dead device, every transfer returns EPROTO, and the attempt used to die at
    // ~t+3 s — which was killing most attempts and, worse, spending the head
    // unit's limited willingness to offer CarPlay at all. The port is still in
    // host mode, so re-acquire the re-enumerated device and start bring-up again
    // rather than throwing the whole attempt away.
    // Follow the accessory through the spec's second enumeration (see
    // POST_AUTH_REENUM_CYCLES). Everything from iAP2 bring-up to the AirPlay
    // session runs inside this loop so a post-auth detach re-attaches and
    // continues instead of ending the attempt.
    let mut reenum_cycle = 0u32;
    let mut post_auth_reset_done = false;
    let session_result = loop {
    let (device, ready) = {
        let mut try_no = 0u32;
        loop {
            try_no += 1;
            let phase = PhaseTimer::start("2");
            let mut device = iap2::Iap2Device::with_default_config(transport);
            let mut observer = ();
            let brought_up = device
                .run_until_ready(&challenge, &mut observer)
                .await
                .context("iAP2 Device bring-up over USB host transport");
            phase.end(&brought_up);
            match brought_up {
                Ok(ready) => break (device, ready),
                Err(e) if try_no <= IAP2_BRINGUP_RETRIES && is_device_disconnect(&e) => {
                    warn!(
                        "Phase 2 attempt #{try_no} lost the device ({e:#}) — re-acquiring and \
                         retrying (up to {IAP2_BRINGUP_RETRIES} retries)"
                    );
                    drop(device); // release the stale handle before re-scanning
                    let (t, summary, sysfs) = host::reacquire_car(CAR_REACQUIRE_TIMEOUT)
                        .await
                        .context("re-acquiring the car after a mid-bring-up disconnect")?;
                    info!(
                        "phase3: re-acquired car vid=0x{:04x} pid=0x{:04x}; iAP2 iface #{}; ncm iface {:?}",
                        summary.vendor_id, summary.product_id, summary.iap2_interface, summary.ncm_interface
                    );
                    transport = t;
                    car_sysfs = sysfs;
                }
                Err(e) => return Err(e),
            }
        }
    };

    info!(
        "**** SessionReady **** control session 0x{:02x}; accessory '{}' by '{}'",
        ready.control_session_id,
        ready.identification.name.as_deref().unwrap_or("<unknown>"),
        ready.identification.manufacturer.as_deref().unwrap_or("<unknown>"),
    );
    info!("Phase 3 complete: iAP2 session is READY.");

    // Opt-in test (RASCAR_POST_AUTH_RESET=1): trigger Apple's post-auth second
    // enumeration ourselves, once, before any NCM traffic. The next pass redoes
    // iAP2 on the fresh enumeration and then continues exactly as today.
    if host::post_auth_reset_wanted() && !post_auth_reset_done {
        post_auth_reset_done = true;
        info!(
            "**** POST-AUTH RESET **** resetting the car's USB device at t+{} ms (Figure 23-2 second \
             enumeration, host side) before binding/using NCM",
            debug::elapsed_ms()
        );
        drop(device); // release our claim on the iAP2 interface first
        if let Err(e) = host::reset_car_device(&car_sysfs) {
            warn!("post-auth reset failed ({e:#}) — continuing without it");
        }
        match host::reacquire_car(post_auth_reenum_timeout()).await {
            Ok((t, summary, sysfs)) => {
                info!(
                    "re-attached after the post-auth reset: vid=0x{:04x} pid=0x{:04x}; iAP2 iface #{}; ncm iface {:?}",
                    summary.vendor_id, summary.product_id, summary.iap2_interface, summary.ncm_interface
                );
                transport = t;
                car_sysfs = sysfs;
                continue;
            }
            Err(e) => break Err(e).context("the car did not come back after the post-auth reset"),
        }
    }

    // Authentication is done, so the CarPlay spec now allows us to evaluate every
    // interface in the configuration. Bind cdc_ncm at THIS point rather than
    // letting the kernel grab it the moment the car enumerates.
    if host::defer_ncm_bind() {
        host::bind_ncm_now(&car_sysfs);
    } else if host::ncm_hold_engaged() {
        // The driver has been held off the car since before the role switch; now
        // that authentication is done it may bind — the same moment a CarLinKit
        // loads cdc_ncm (on its iAP2 CarPlayStartSession event).
        host::release_ncm_hold_and_bind(&car_sysfs);
    }

    // Nothing else drives the iAP2 link once bring-up is done, yet the car keeps
    // sending control-session traffic for the whole mirroring session. Merely
    // holding the `Iap2Device` (what this used to do) keeps the USB host
    // transport — and thus the CDC-NCM tunnel the car's _airplay._tcp service
    // rides on — open, but reads nothing: no ACK goes back, the car's link layer
    // retransmits until its budget is exhausted and drops CarPlay mid-stream.
    //
    // So run a real service loop concurrently with Phases 4/5. It is a local
    // future rather than a spawned task so a link failure cannot be lost: the
    // `select!` below surfaces it as the return value of this function.
    //
    // It must start the moment bring-up returns. Measured at the car (sample #2):
    // when CDC-NCM bring-up was joined in front of it, the service loop only
    // began 4.4 s after SessionReady, nothing read or ACKed the car's post-auth
    // traffic, and the car dropped USB 1.2 s after SessionReady. So NCM bring-up
    // and discovery now run inside the AirPlay future, alongside the service loop.
    let iap2_service = device.service_link();
    tokio::pin!(iap2_service);

    let pre_ifaces = outcome.pre_ifaces.clone();
    let car_sysfs_for_airplay = car_sysfs.clone();
    let mut early_ncm = early_ncm.take();
    let airplay_phases = async move {
        // Split experiment (RASCAR_SKIP_PHASE4=1): keep the iAP2 link fully
        // serviced but never start AirPlay. The car tears its USB gadget down
        // ~6.4 s after enumeration whether we fail in identification or reach
        // SessionReady and stream video, so the trigger is not in the AirPlay
        // session itself -- unless STARTING AirPlay is what arms it. This is the
        // clean split: if the device still dies at ~6.4 s with iAP2 alone, the
        // cause is at the iAP2/CarPlay-session level and every AirPlay hypothesis
        // is dead. If it survives, the teardown is armed by something in Phase 4.
        if crate::debug::env_flag("RASCAR_SKIP_PHASE4", false) {
            warn!(
                "**** SPLIT EXPERIMENT **** RASCAR_SKIP_PHASE4=1 — iAP2 stays serviced, \
                 AirPlay will NOT be started. Watching how long the car keeps its gadget."
            );
            std::future::pending::<()>().await;
        }

        // Re-discover and re-dial until the budget runs out: the car's AirPlay
        // receiver can start well after our NCM link does.
        let phase4_deadline = std::time::Instant::now() + PHASE4_RETRY_BUDGET;

        loop {
        let last_cycle = std::time::Instant::now() >= phase4_deadline;

        // NCM bring-up must run AFTER auth: running it during auth catches the CDC
        // interface at its raw `usb0` name before udev renames it (e.g. to enu1i1),
        // and the link-local wait then hangs on the renamed-away name.
        // `_carplay-ctrl._tcp` SRV is advertised from process start (see main). Its
        // host ADDRESS must be the NCM IPv6 link-local (what the car dials) — known
        // only now, so publish it the moment the NCM link is up.
        let mut addr_published = false;
        let mut attempt = 0u32;
        let airplay_targets = loop {
            attempt += 1;
            // Prefer the link the early task already brought up; fall back to
            // doing it here if it was disabled, panicked, or has not finished.
            let ncm = match early_ncm.take() {
                Some(handle) => match handle.await {
                    Ok(link) => link,
                    Err(e) => {
                        warn!("ncm: early bring-up task failed ({e}) — doing it inline");
                        host::bring_up_ncm(&pre_ifaces, &car_sysfs_for_airplay).await
                    }
                },
                None => host::bring_up_ncm(&pre_ifaces, &car_sysfs_for_airplay).await,
            };
            if !addr_published {
                if let Some(ll) = ncm.as_ref().and_then(|n| n.link_local.as_deref()) {
                    host::ctrl_advertise::publish_ncm_address(ll);
                    addr_published = true;
                }
            }
            // Dial the car's link-local IMMEDIATELY, before asking avahi anything.
            //
            // Budget arithmetic from the best run of 2026-09-21 (031-122151): the
            // car's device lived 6.5 s and the first video frame did not leave
            // until t+3.23 s into that budget. 0.78 s of those were an
            // `avahi-browse` that returned the very address we can read out of
            // the neighbour table in microseconds — and `ncm.rs` rewrites
            // avahi's answer to that same link-local anyway, because the car
            // refuses :7000 on its advertised IPv4. The browse buys nothing and
            // costs a quarter of the time we have to get a picture up.
            if let Some(n) = ncm.as_ref() {
                if let Some(ll) = host::ncm::car_link_local(&n.ifname).await {
                    info!(
                        "Phase 4: dialling the car's link-local {ll} straight from the neighbour \
                         table — skipping avahi-browse (same address, ~0.8 s cheaper)"
                    );
                    break vec![host::ncm::AirPlayEndpoint {
                        service_type: "_airplay._tcp".into(),
                        name: "car (neighbour table)".into(),
                        host: String::new(),
                        address: ll,
                        port: AIRPLAY_PORT,
                        interface: n.ifname.clone(),
                    }];
                }
            }

            let targets = host::discover_airplay(ncm.as_ref()).await;
            info!(
                "ncm link={:?}; {} airplay endpoint(s) (discovery attempt #{attempt})",
                ncm.as_ref().map(|n| (n.ifname.clone(), n.link_local.clone())),
                targets.len()
            );
            if !targets.is_empty() {
                break targets;
            }
            // The head unit sometimes stops advertising `_airplay._tcp` entirely
            // (measured on the wire: our PTR queries go out, no answer ever comes
            // back) while its AirPlay server is still listening. Discovery is only
            // ever used to obtain an address, and `car_candidates` already derives
            // the car's IPv4 (.1 of the NCM /24) and every IPv6 neighbour on the
            // link — so once the NCM link is up we can dial it without any
            // advertisement at all. Waiting for a record that never comes just
            // burns the session budget.
            if attempt >= FALLBACK_AFTER_DISCOVERY_ATTEMPTS
                && crate::debug::env_flag("RASCAR_AIRPLAY_FALLBACK", true)
            {
                if let Some(n) = ncm.as_ref() {
                    warn!(
                        "Phase 4: no AirPlay advertisement after {attempt} attempts — falling back to                          dialling the car directly on {} (candidates are derived from the link)",
                        n.ifname
                    );
                    // Learn the car's IPv6 link-local from the neighbour table and
                    // dial THAT. Leaving the address empty made `car_candidates`
                    // fall back to the derived IPv4 (192.168.0.1), which the car
                    // does not accept on :7000 — measured: every such attempt
                    // burned ~6 s on a connect timeout and then failed the whole
                    // session. This path matters much more now that cdc_ncm is
                    // bound only after authentication: NCM comes up ~3 ms before we
                    // browse, so avahi legitimately has nothing yet and we land
                    // here on a session that is otherwise healthy.
                    let addr = host::ncm::car_link_local(&n.ifname).await.unwrap_or_default();
                    if addr.is_empty() {
                        warn!(
                            "Phase 4: no car IPv6 link-local in the neighbour table on {} either \
                             — dialling will fall back to the v4 address the car refuses",
                            n.ifname
                        );
                    } else {
                        info!("Phase 4: dialling the car's link-local {addr} learned from the neighbour table");
                    }
                    break vec![host::ncm::AirPlayEndpoint {
                        service_type: "_airplay._tcp".into(),
                        name: "car (undiscovered)".into(),
                        host: String::new(),
                        address: addr,
                        port: AIRPLAY_PORT,
                        interface: n.ifname.clone(),
                    }];
                }
            }
            warn!("Phase 4: no AirPlay endpoint yet (attempt #{attempt}) — retrying in 1 s");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        };
        for ep in &airplay_targets {
            info!("AirPlay target (for Phase 4): {} [{}]:{}", ep.name, ep.address, ep.port);
        }

        // === Phase 4/5: wired CarPlay screen-streaming session (legacy path) ===
        // The AppleTV/FairPlay path (`airplay::run` + `stream::run`) is kept for
        // the bench (`run_airplay_only`); the car path uses the plaintext-RTSP /
        // AES-CTR CarPlay session instead. It negotiates the display resolution
        // from `GET /info` itself, so no width/height is passed here.
        info!("=== Phase 4/5: CarPlay connect + screen stream (legacy, no pairing/FairPlay) ===");

        let mut last_err = None;
        for ep in &airplay_targets {
            info!("Phase 4/5: attempting CarPlay session against {} [{}]:{}", ep.name, ep.address, ep.port);
            let phase = PhaseTimer::start("4");
            let session = airplay::carplay_session::run_carplay(ep).await;
            phase.end(&session);
            match session {
                Ok(()) => {
                    info!("**** CarPlay session with {} ended (encoder stopped) ****", ep.name);
                    return Ok(());
                }
                Err(e) => {
                    error!("CarPlay session against {} FAILED: {e:#}", ep.name);
                    last_err = Some(e);
                }
            }
        }
        if !last_cycle {
            if let Some(e) = &last_err {
                warn!(
                    "Phase 4/5: no endpoint accepted yet ({e:#}) — re-discovering and retrying \
                     ({}s of budget left)",
                    phase4_deadline.saturating_duration_since(std::time::Instant::now()).as_secs()
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        break match last_err {
            Some(e) => Err(e).context("Phase 4/5: all discovered AirPlay endpoints failed"),
            None => Ok(()),
        };
        }
    };
    tokio::pin!(airplay_phases);

    let cycle_outcome = tokio::select! {
        result = &mut airplay_phases => result,
        // The link died before the AirPlay work finished. That is EXPECTED once,
        // right after authentication — it is the spec's second enumeration.
        service = &mut iap2_service => match service {
            // `service_link` returns `Result<Infallible>`, so success is
            // unrepresentable: this arm exists only to satisfy the match.
            Ok(never) => match never {},
            Err(e) => Err(anyhow::Error::new(e))
                .context("iAP2 link service loop failed — the car tore down the iAP2 link"),
        },
    };

    // Drop everything holding the (now stale) USB handle before re-scanning.
    // `device` was consumed by `service_link()`; dropping the pinned futures
    // releases the stale USB handle before we re-scan the bus.
    drop(airplay_phases);
    drop(iap2_service);

    match cycle_outcome {
        Err(e) if is_device_disconnect(&e) && reenum_cycle < POST_AUTH_REENUM_CYCLES => {
            reenum_cycle += 1;
            info!(
                "**** SECOND ENUMERATION **** the accessory detached after authentication at \
                 t+{} ms — this is Apple's CarPlay Figure 23-2 flow, not a failure. Re-attaching \
                 (cycle {reenum_cycle}/{POST_AUTH_REENUM_CYCLES}); on this pass the device \
                 evaluates ALL interfaces, so NCM + the CarPlay session follow.",
                debug::elapsed_ms()
            );
            match host::reacquire_car(post_auth_reenum_timeout()).await {
                Ok((t, summary, sysfs)) => {
                    info!(
                        "re-attached after the second enumeration: vid=0x{:04x} pid=0x{:04x}; \
                         iAP2 iface #{}; ncm iface {:?}",
                        summary.vendor_id, summary.product_id,
                        summary.iap2_interface, summary.ncm_interface
                    );
                    transport = t;
                    car_sysfs = sysfs;
                    continue;
                }
                Err(e) => {
                    break Err(e).context(
                        "the accessory detached after authentication (expected second \
                         enumeration) but never came back",
                    )
                }
            }
        }
        other => break other,
    }
    };

    session_result
}

/// 32 bytes of OS randomness for the iAP2 authentication challenge (no rng dep —
/// read straight from `/dev/urandom`).
fn random_challenge() -> Result<[u8; 32]> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut buf)
        .context("read /dev/urandom")?;
    Ok(buf)
}

/// Bench entry point: run Phases 4-5 against an already-reachable AirPlay
/// receiver, with no USB, no role switch, no iAP2 and no mDNS.
///
/// `target` is `HOST:PORT` — e.g. `127.0.0.1:7000` for UxPlay, or
/// `[fe80::1%25enx0]:7000` for a link-local peer (the `%` must be percent-encoded
/// as `%25` if your shell would otherwise eat it; a bare `%scope` also works).
/// Bench: run the **CarPlay** session path (`run_carplay`) against a TCP peer,
/// with no USB gadget / role switch / iAP2 / mDNS. Lets the full CarPlay sender
/// (pairing, encrypted RTSP, screen stream) be tested against a CarPlay receiver
/// on the LAN. `target` is `HOST:PORT` (bracketed/`%scope` IPv6 accepted).
async fn run_carplay_only(target: &str) -> Result<()> {
    let ep = parse_bench_endpoint(target, "--carplay-endpoint")?;
    warn!(
        "BENCH MODE (CarPlay) — Phases 1-3 skipped (no USB/role-switch/iAP2/mDNS). \
         Runs the real run_carplay path against {}:{}; proves the AirPlay/pairing/screen \
         layer but NOTHING about USB or iAP2.",
        ep.address, ep.port
    );
    let phase = PhaseTimer::start("carplay-bench");
    let r = airplay::carplay_session::run_carplay(&ep).await;
    phase.end(&r);
    r
}

/// Parse a `HOST:PORT` bench target into an `AirPlayEndpoint` (shared by the two
/// bench entry points).
fn parse_bench_endpoint(target: &str, flag: &str) -> Result<host::ncm::AirPlayEndpoint> {
    let (address, port) = target
        .rsplit_once(':')
        .with_context(|| format!("{flag} must be HOST:PORT"))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("{flag}: bad port {port:?}"))?;
    let address = address.trim_start_matches('[').trim_end_matches(']');
    let (address, interface) = match address.split_once('%') {
        Some((addr, scope)) => (addr.to_string(), scope.to_string()),
        None => (address.to_string(), String::new()),
    };
    Ok(host::ncm::AirPlayEndpoint {
        service_type: "_airplay._tcp".to_string(),
        name: format!("manual:{address}"),
        host: address.clone(),
        address,
        port,
        interface,
    })
}

async fn run_airplay_only(target: &str) -> Result<()> {
    let (address, port) = target
        .rsplit_once(':')
        .context("--airplay-endpoint must be HOST:PORT")?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("--airplay-endpoint: bad port {port:?}"))?;
    // Strip the brackets of a literal IPv6 form; `resolve_endpoint` wants the
    // bare address plus a separate scope.
    let address = address.trim_start_matches('[').trim_end_matches(']');

    // A bare link-local needs a scope. Accept `fe80::1%enx0` and split it out so
    // the endpoint carries the interface the same way a discovered one does.
    let (address, interface) = match address.split_once('%') {
        Some((addr, scope)) => (addr.to_string(), scope.to_string()),
        None => (address.to_string(), String::new()),
    };

    let ep = host::ncm::AirPlayEndpoint {
        service_type: "_airplay._tcp".to_string(),
        name: format!("manual:{address}"),
        host: address.clone(),
        address,
        port,
        interface,
    };

    warn!(
        "BENCH MODE — Phases 1-3 skipped (no USB gadget, no role switch, no iAP2, no mDNS). \
         This exercises the real Phase 4/5 code against {}:{}, but proves NOTHING about the \
         USB or iAP2 layers, and a receiver that rubber-stamps fp-setup proves nothing about \
         FairPlay either.",
        ep.address, ep.port
    );

    let (width, height) = (800u32, 480u32);
    info!("=== Phase 4: AirPlay pair + FairPlay SAP (bench) ===");
    let phase = PhaseTimer::start("4");
    let handshake = airplay::run(&ep, width, height)
        .await
        .context("Phase 4 (bench): AirPlay handshake");
    phase.end(&handshake);
    let (sender, session) = handshake?;
    info!(
        "**** Phase 4 SUCCESS **** dataPort={} display={:?}",
        session.data_port, session.display
    );

    info!("=== Phase 5: dashboard capture + AirPlay video sender (bench) ===");
    let phase = PhaseTimer::start("5");
    let streamed = stream::run(sender, session)
        .await
        .context("Phase 5 (bench): streaming session");
    phase.end(&streamed);
    streamed
}

#[cfg(test)]
mod tuning_knob_tests {
    use super::*;

    #[test]
    fn early_ncm_is_off_by_default_because_the_rename_race_hangs_it() {
        // 2026-09-23 drive: the early task grabbed `usb0`, udev renamed it to
        // `enu1i1`, the link-local wait hung, AirPlay never started.
        assert!(!early_ncm_from(None));
        assert!(!early_ncm_from(Some("")));
        assert!(!early_ncm_from(Some("0")));
    }

    #[test]
    fn early_ncm_is_opt_in() {
        for on in ["1", "on", "true", "yes", " 1 "] {
            assert!(early_ncm_from(Some(on)), "{on:?} should opt in");
        }
    }

    #[test]
    fn reenum_timeout_defaults_to_the_old_value_and_is_sweepable() {
        assert_eq!(reenum_timeout_from(None).as_millis(), 1500);
        assert_eq!(reenum_timeout_from(Some("4000")).as_millis(), 4000);
        // Nonsense and absurd values fall back rather than wedging an attempt.
        assert_eq!(reenum_timeout_from(Some("banana")).as_millis(), 1500);
        assert_eq!(reenum_timeout_from(Some("999999")).as_millis(), 1500);
    }
}

/// `--audio-probe`: capture 3 s of what the car would receive and report it.
async fn audio_probe() -> Result<()> {
    let pulse = stream::audio::PulseEnv::discover();
    println!("pulse server: {} (cookie {})", pulse.server(), pulse.cookie);
    println!("default output monitor: {:?}", pulse.default_monitor().await);
    let device = std::env::var("RASCAR_AUDIO_SOURCE").ok().filter(|s| !s.is_empty());
    let mut cap = stream::audio::AudioCapture::new(48_000, 2);
    cap.start(device.as_deref()).await?;
    let mut buf = vec![0u8; 16384];
    let (mut bytes, mut sq) = (0usize, 0f64);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < until {
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), cap.read_chunk(&mut buf)).await??;
        if n == 0 {
            anyhow::bail!("capture ended after {bytes} bytes");
        }
        for ch in buf[..n].chunks_exact(2) {
            let s = i16::from_be_bytes([ch[0], ch[1]]) as f64;
            sq += s * s;
        }
        bytes += n;
    }
    cap.stop().await;
    let rms = (sq / (bytes / 2).max(1) as f64).sqrt();
    println!("captured {bytes} bytes in 3 s (expected ~576000), RMS {rms:.0} ({})", if rms > 30.0 { "sound" } else { "silence" });
    Ok(())
}
