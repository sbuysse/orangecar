//! Render prerequisite: get the RasCar dashboard onto a display the video
//! capture (`stream::video`) can grab, sized to the car's screen.
//!
//! The size comes from the car's `/info` display description (falling back to
//! 800x480 when it gives none). Two back-ends, selected by `RASCAR_COMPOSITOR`:
//!
//!   * `wayland` (what `setup/carplay.env.example` ships): a headless **cage**
//!     (wlroots kiosk compositor, GLES on the render node, software fallback)
//!     running Chromium on Wayland; frames are captured by `stream::wlcapture`
//!     and touch goes through a uinput touchscreen that cage reads via libinput.
//!   * anything else (the binary's default): X display `:0`. If an X server is
//!     already running there (a kiosk desktop) it is used as-is; otherwise we
//!     start **Xvfb :0** at the negotiated size and a **Chromium --kiosk** on
//!     the `app/` dashboard. Capture is ffmpeg `x11grab`, touch is XTest.
//!
//! On X the display name is handed to the consumers explicitly ([`DISPLAY`]
//! goes to ffmpeg's `x11grab` and to the XTest touch injector). It is
//! deliberately NOT written into the process environment: `ensure_dashboard`
//! runs on the multi-threaded runtime with tasks already spawned, and mutating
//! the environ concurrently with any `getenv` is UB.
//!
//! Missing Xvfb/cage/Chromium, or the app server (`rascar-app`, :8080) not being
//! up yet, is logged loudly but not fatal.

use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// Display we render on and capture from. CarPlay minimum surface is 800x480.
/// X display the dashboard is rendered on and captured from.
///
/// Defaults to `:0` (the board's kiosk desktop). Override with `RASCAR_DISPLAY`
/// to point at a dedicated Xvfb — on a dev box `:0` is the developer's own
/// desktop, so capturing it would grab whatever happens to be in the top-left
/// 800x480 and a kiosk window would take over their screen.
///
///   Xvfb :99 -screen 0 800x480x24 -nolisten tcp &
///   RASCAR_DISPLAY=:99 cargo run --bin rascar-carplay -- --airplay-endpoint ...
pub fn x_display() -> String {
    std::env::var("RASCAR_DISPLAY").unwrap_or_else(|_| ":0".to_string())
}
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 480;
/// The `app/` dashboard (Axum) kiosk URL.
pub const DASHBOARD_URL: &str = "http://localhost:8080";

/// Holds any child processes we spawned so they are killed on drop (i.e. when
/// the streaming session ends). If `:0` pre-existed, both stay `None` and we
/// leave the existing desktop untouched.
pub struct RenderGuard {
    xvfb: Option<Child>,
    chromium: Option<Child>,
    /// Wayland mode: cage, which owns Chromium (it exits with its client).
    compositor: Option<Child>,
}

impl RenderGuard {
    fn none() -> Self {
        Self {
            xvfb: None,
            chromium: None,
            compositor: None,
        }
    }
}

impl Drop for RenderGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.chromium.take() {
            reap(&mut c, "chromium kiosk");
        }
        if let Some(mut x) = self.xvfb.take() {
            reap(&mut x, "Xvfb :0");
        }
        if let Some(mut c) = self.compositor.take() {
            reap(&mut c, "cage compositor");
        }
    }
}

/// Kill a child and actually reap it, so it does not linger as a zombie (or,
/// worse, keep holding the X socket while the next session probes for it).
/// Bounded and non-blocking-ish: `kill_on_drop` covers whatever outlives this.
fn reap(child: &mut Child, what: &str) {
    let _ = child.start_kill();
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(status)) => {
                info!("render: {what} stopped ({status})");
                return;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                warn!("render: could not reap {what}: {e}");
                return;
            }
        }
    }
    warn!("render: {what} did not exit within 500ms of being killed");
}

/// True if an X server is really reachable on `disp`.
///
/// This is a connection attempt, not a `Path::exists` on `/tmp/.X11-unix/X0`:
/// a crashed or killed X server leaves that socket file behind, and trusting it
/// makes the next session skip Xvfb entirely and capture a dead display.
fn display_up() -> bool {
    x11rb::connect(Some(&x_display())).is_ok()
}

/// Root-window geometry of a live X server, or `None` if it can't be queried.
/// Used when a production kiosk `:0` already exists: we capture at ITS actual
/// pixel size, not the car's negotiated dims, so `x11grab` never asks for a
/// region larger than the root (which fails with 0 frames).
fn display_geometry() -> Option<(u32, u32)> {
    use x11rb::connection::Connection;
    let (conn, screen_num) = x11rb::connect(Some(&x_display())).ok()?;
    let screen = conn.setup().roots.get(screen_num)?;
    Some((screen.width_in_pixels as u32, screen.height_in_pixels as u32))
}

/// Ensure the dashboard is rendered on the capture display, sizing the surface
/// to the car's negotiated `(width, height)`, and return the guard plus the
/// EFFECTIVE capture geometry the encoder must use for `-video_size`.
///
/// * If an X server already exists on the display (production kiosk), we use it
///   as-is and report its actual root geometry — the encoder captures that, not
///   the negotiated dims, so x11grab always fits the root.
/// * Otherwise we spawn Xvfb + Chromium sized to `(width, height)` and report
///   those.
///
/// `(width, height)` of `(0, 0)` (unknown) falls back to 800x480.
pub async fn ensure_dashboard(width: u32, height: u32) -> Result<(RenderGuard, u32, u32)> {
    let (w, h) = if width == 0 || height == 0 { (WIDTH, HEIGHT) } else { (width, height) };

    if let Some(target) = wayland_target() {
        return ensure_wayland_dashboard(&target, w, h).await;
    }

    if display_up() {
        let (gw, gh) = display_geometry().unwrap_or((w, h));
        info!(
            "render: X server already present on {} — using it as-is (assuming rascar-kiosk desktop). \
             Capturing at its actual geometry {gw}x{gh} (car negotiated {w}x{h}).",
            x_display()
        );
        return Ok((RenderGuard::none(), gw, gh));
    }

    info!("render: no X server on {} — starting headless Xvfb + Chromium kiosk at {w}x{h}", x_display());

    // --- Xvfb sized to the negotiated dims ---
    let xvfb = Command::new("Xvfb")
        .args([
            &x_display(),
            "-screen",
            "0",
            &format!("{w}x{h}x24"),
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning Xvfb — is the `xvfb` package installed?")?;
    info!("render: Xvfb started on {} ({w}x{h}x24), pid={}", x_display(), xvfb.id().unwrap_or(0));

    // Take ownership of Xvfb RIGHT NOW. Every path below can fail with `?`, and
    // an un-owned Xvfb survives the error, keeps /tmp/.X11-unix/X0, and makes
    // the next run believe a usable display exists.
    let mut guard = RenderGuard { xvfb: Some(xvfb), chromium: None, compositor: None };

    // Give Xvfb a moment to create its socket before Chromium connects.
    //
    // Two consecutive successful connects, and the warning keyed on the loop's
    // own result: x11rb can connect to a socket Xvfb has created but not yet
    // finished initialising, and the very next connect then fails. Re-querying
    // after the break made a perfectly healthy start log "socket did not appear
    // within 5s" 100 ms in, every single run.
    let mut display_ready = false;
    for _ in 0..50 {
        if display_up() && display_up() {
            display_ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !display_ready {
        warn!("render: Xvfb socket did not appear within 5s — Chromium may fail to connect");
    }

    // --- Chromium --kiosk (flags mirror setup/install.sh rascar-kiosk.service) ---
    let chromium_bin = which_chromium();
    let as_root = running_as_root();
    let args = chromium_args(w, h, DASHBOARD_URL, as_root, false);
    let pulse = pulse_for_chromium().await;
    let mut chromium = Command::new(&chromium_bin);
    pulse.apply(&mut chromium);
    let mut chromium = chromium
        .args(&args)
        .env("DISPLAY", x_display())
        .stdout(Stdio::null())
        // NOT null: Chromium reports why it refuses to start on stderr, and
        // swallowing it hid "Running as root without --no-sandbox is not
        // supported" for as long as this code has existed — every session
        // streamed a black screen to the car while the logs looked healthy.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {chromium_bin} — is Chromium installed?"))?;
    info!(
        "render: Chromium kiosk started on {} → {DASHBOARD_URL}, pid={}{}",
        x_display(),
        chromium.id().unwrap_or(0),
        if as_root { " (root: --no-sandbox)" } else { "" }
    );

    // A Chromium that refuses to start is gone within a few hundred ms, long
    // before the first frame. Catch it here rather than discovering a black
    // screen in the car.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    match chromium.try_wait() {
        Ok(Some(status)) => warn!(
            "render: Chromium EXITED IMMEDIATELY ({status}) — the capture display is empty and the \
             car will see a black screen; its stderr is above in this log"
        ),
        Ok(None) => debug!("render: Chromium still alive 700 ms after spawn"),
        Err(e) => warn!("render: cannot poll the Chromium child: {e}"),
    }

    guard.chromium = Some(chromium);
    Ok((guard, w, h))
}

/// Chromium's sound must reach the user's PulseAudio: that is the output
/// whose monitor we capture for the car. Run as root with no PULSE_SERVER,
/// it looked for /run/user/0/pulse, found nothing and played into a null
/// output — radio, TV and web pages were silent in the car while mpv (YT
/// Music, run as the user) was heard (drive 2026-09-30: rms=0 for 7 min of
/// radio). It picks its audio backend once at start-up, so the server must
/// be there first; the user's pulseaudio.socket normally is, within ms.
async fn pulse_for_chromium() -> crate::stream::audio::PulseEnv {
    let pulse = crate::stream::audio::PulseEnv::discover();
    let socket = format!("{}/pulse/native", pulse.runtime_dir);
    let mut pulse_ready = std::path::Path::new(&socket).exists();
    for _ in 0..100 {
        if pulse_ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        pulse_ready = std::path::Path::new(&socket).exists();
    }
    if pulse_ready {
        info!("render: Chromium audio → {}", pulse.server());
    } else {
        warn!("render: no PulseAudio socket at {socket} after 10 s — Chromium's audio will not reach the car");
    }
    pulse
}

// ---------------------------------------------------------------------------
// Wayland: the kiosk on a headless compositor rendering on the GPU
// ---------------------------------------------------------------------------
//
// Xvfb has no DRI3, so everything drawn into it is rendered by the CPU and
// Chromium has no WebGL at all (measured 2026-09-30: `GL:none`, or llvmpipe
// with --ignore-gpu-blocklist). cage (a wlroots kiosk compositor) on the
// headless backend renders with GLES on the Mali's render node, and Chromium
// on Wayland gets WebGL2 on the GPU: `ANGLE (Mesa, Mali-G610 (Panfrost),
// OpenGL ES 3.1)`. Capture: `stream::wlcapture`. Touch: the uinput
// touchscreen of `stream::input`, read by cage through libinput, so Chromium
// receives real touch events.

/// Private runtime dir of the compositor (root-only).
const WAYLAND_RUNTIME_DIR: &str = "/run/rascar-wl";
const WAYLAND_SOCKET: &str = "wayland-0";
/// The Mali's render node (panthor). `RASCAR_RENDER_NODE` overrides it (the
/// bench points it at a missing path to exercise the software fallback).
fn render_node() -> String {
    std::env::var("RASCAR_RENDER_NODE").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// `RASCAR_COMPOSITOR=wayland` selects the Wayland kiosk; anything else keeps
/// Xvfb, the configuration known to work in the car.
pub fn wayland_target() -> Option<crate::stream::wlcapture::WaylandTarget> {
    std::env::var("RASCAR_COMPOSITOR")
        .ok()
        .filter(|v| v.eq_ignore_ascii_case("wayland"))
        .map(|_| crate::stream::wlcapture::WaylandTarget { runtime_dir: WAYLAND_RUNTIME_DIR.into(), socket: WAYLAND_SOCKET.into() })
}

async fn ensure_wayland_dashboard(
    target: &crate::stream::wlcapture::WaylandTarget,
    w: u32,
    h: u32,
) -> Result<(RenderGuard, u32, u32)> {
    use std::os::unix::fs::PermissionsExt;
    // The pre-warm (main.rs) started the kiosk at boot and holds it for the
    // whole process: reuse it, exactly like the Xvfb path reuses a live `:0`.
    // Starting a second compositor here deleted the first one's socket and ran
    // a second Chromium on the same profile, which handed over to the first and
    // exited: "cage EXITED right after start" on every attempt of the
    // one drive, and the car dropped the session at ~8 s.
    if std::os::unix::net::UnixStream::connect(target.socket_path()).is_ok() {
        info!("render: compositor already running on {} — reusing it", target.socket_path());
        set_wayland_mode(target, w, h).await?;
        return Ok((RenderGuard::none(), w, h));
    }
    std::fs::create_dir_all(&target.runtime_dir).with_context(|| format!("create {}", target.runtime_dir))?;
    std::fs::set_permissions(&target.runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    // A killed compositor leaves its socket and lock behind; cage would then
    // pick wayland-1 while we connect to wayland-0.
    for f in [target.socket.clone(), format!("{}.lock", target.socket)] {
        let _ = std::fs::remove_file(format!("{}/{f}", target.runtime_dir));
    }
    let pulse = pulse_for_chromium().await;
    let chromium_bin = which_chromium();
    let args = chromium_args(w, h, DASHBOARD_URL, running_as_root(), true);
    // The GPU driver (panthor) can come up after this service: 34-44 s after
    // boot on 2026-09-30, while the pre-warm runs at ~11 s and the car connects
    // at ~30 s. Without the render node, cage refused to start ("no DRM FD") and
    // there was no dashboard at all. Then render in software instead (what Xvfb
    // always did): the session survives, the next attempt gets the GPU.
    let node = render_node();
    let mut gpu = std::path::Path::new(&node).exists();
    for _ in 0..30 {
        if gpu {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        gpu = std::path::Path::new(&node).exists();
    }
    let mut cage = Command::new("cage");
    if gpu {
        info!("render: starting cage (headless, GLES on {node}) + Chromium on Wayland at {w}x{h}");
        cage.env("WLR_RENDERER", "gles2").env("WLR_RENDER_DRM_DEVICE", &node);
    } else {
        warn!("render: no GPU render node ({node}) yet — starting cage with SOFTWARE rendering (pixman) at {w}x{h}");
        cage.env("WLR_RENDERER", "pixman");
    }
    let compositor = cage
        .arg("-d")
        .arg("--")
        .arg(&chromium_bin)
        .args(&args)
        .env_remove("DISPLAY")
        .env("XDG_RUNTIME_DIR", &target.runtime_dir)
        .env("WLR_BACKENDS", "headless,libinput")
        .env("LIBSEAT_BACKEND", "builtin")
        // Our touchscreen (uinput) only appears when a session starts, and at
        // boot the board's own input devices may not be there yet: without this
        // cage refused to start ("no input devices", 2026-09-30 drive 3) — yet
        // kept its socket, see set_wayland_mode.
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("WLR_HEADLESS_OUTPUTS", "1")
        // Only the audio variables: XDG_RUNTIME_DIR must stay the compositor's,
        // it is where Chromium finds the Wayland socket.
        .env("PULSE_SERVER", pulse.server())
        .env("PULSE_COOKIE", &pulse.cookie)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawning cage — is the `cage` package installed?")?;
    info!("render: cage started, pid={}", compositor.id().unwrap_or(0));
    let mut guard = RenderGuard { xvfb: None, chromium: None, compositor: Some(compositor) };

    let socket = target.socket_path();
    let mut up = false;
    for _ in 0..50 {
        if let Some(Ok(Some(status))) = guard.compositor.as_mut().map(|c| c.try_wait()) {
            bail!("cage EXITED during start-up ({status}) — its stderr is above in this log");
        }
        if std::path::Path::new(&socket).exists() {
            up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !up {
        bail!("cage did not create {socket} within 5 s");
    }
    set_wayland_mode(target, w, h).await?;

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    if let Some(c) = guard.compositor.as_mut() {
        if let Ok(Some(status)) = c.try_wait() {
            bail!("cage EXITED right after start ({status}) — its stderr is above in this log");
        }
    }
    Ok((guard, w, h))
}

/// Size the headless output to the car's screen, refreshing at the video frame rate.
async fn set_wayland_mode(target: &crate::stream::wlcapture::WaylandTarget, w: u32, h: u32) -> Result<()> {
    // The copy completes on the next refresh: the output runs at the video
    // frame rate and paces the capture (`wlcapture`).
    let hz = crate::stream::video::configured_fps();
    // Every call is bounded: a compositor whose backend failed keeps its socket
    // but never answers, and an unbounded wlr-randr then hung the pre-warm —
    // and with it the whole CarPlay start (2026-09-30 drive 3: no connection).
    let randr = |args: Vec<String>| async move {
        let mut c = Command::new("wlr-randr");
        c.args(args)
            .env("XDG_RUNTIME_DIR", &target.runtime_dir)
            .env("WAYLAND_DISPLAY", &target.socket)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        tokio::time::timeout(std::time::Duration::from_secs(3), c.output())
            .await
            .map_err(|_| anyhow::anyhow!("wlr-randr got no answer from the compositor within 3 s"))?
            .context("running wlr-randr — is it installed?")
    };
    // The output appears a moment after the socket.
    for _ in 0..50 {
        let out = randr(vec![]).await?;
        let listing = String::from_utf8_lossy(&out.stdout);
        if let Some(name) = listing.lines().find(|l| !l.starts_with(' ')).and_then(|l| l.split_whitespace().next()) {
            let st = randr(vec!["--output".into(), name.into(), "--custom-mode".into(), format!("{w}x{h}@{hz}Hz")])
                .await?
                .status;
            if !st.success() {
                bail!("wlr-randr refused {w}x{h}@{hz}Hz on {name}");
            }
            info!("render: output {name} set to {w}x{h} @ {hz} Hz");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("the compositor exposed no output within 5 s")
}

/// Pick a chromium binary name (Debian/RPi uses `chromium`, some distros
/// `chromium-browser`). We do not probe the filesystem; the spawn error will
/// name whichever we picked. `// TODO(hw)`: confirm the binary name on the board.
fn which_chromium() -> String {
    // Distributions disagree on the binary name: Fedora ships `chromium-browser`
    // (with `chromium` only sometimes present), Debian/Ubuntu ship `chromium`,
    // and a Chrome install answers to `google-chrome`/`google-chrome-stable`.
    // Hardcoding one name made the whole render step fail on a box that had a
    // perfectly good browser under a different name.
    const CANDIDATES: [&str; 5] = [
        "chromium",
        "chromium-browser",
        "google-chrome-stable",
        "google-chrome",
        "chrome",
    ];
    for name in CANDIDATES {
        if which(name) {
            return name.to_string();
        }
    }
    // Nothing found: return the conventional name so the spawn fails with a
    // recognisable "No such file or directory" naming the binary we wanted.
    "chromium".to_string()
}

/// `true` if `name` resolves on `PATH`.
fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(name);
                std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// True when this process runs with effective uid 0. `rascar-carplay` needs
/// root for configfs/FunctionFS, so this is the normal case on the board.
fn running_as_root() -> bool {
    // SAFETY: geteuid() takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

/// Chromium profile kept across boots (sign-ins, cookies, Widevine licences).
const CHROMIUM_PROFILE_DIR: &str = "/var/lib/rascar/chromium-profile";
/// Deployed path of the frame-unlock extension (see app/chromium-ext/).
const FRAME_UNLOCK_EXT: &str = "/opt/orangecar/app/chromium-ext/frame-unlock";
/// DevTools port rascar-app uses to open/switch streaming-app tabs.
const DEVTOOLS_PORT: u16 = 9222;

/// Chromium's command line, built as data so the root handling is testable
/// without a browser.
///
/// `--no-sandbox` is REQUIRED when running as root: Chromium's zygote refuses to
/// start otherwise ("Running as root without --no-sandbox is not supported",
/// crbug.com/638180) and exits 1 immediately, leaving Xvfb up and empty.
fn chromium_args(width: u32, height: u32, url: &str, as_root: bool, wayland: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--kiosk".into(),
        "--noerrdialogs".into(),
        "--disable-infobars".into(),
        "--disable-session-crashed-bubble".into(),
        "--disable-translate".into(),
        "--disable-features=Translate,TranslateUI".into(),
        // Only our own extension: it strips the frame-blocking headers so the
        // dashboard can embed Google Maps / Waze (app/chromium-ext/frame-unlock).
        format!("--disable-extensions-except={FRAME_UNLOCK_EXT}"),
        format!("--load-extension={FRAME_UNLOCK_EXT}"),
        "--disable-component-extensions-with-background-pages".into(),
        "--no-first-run".into(),
        "--start-fullscreen".into(),
        format!("--window-size={width},{height}"),
        format!("--width={width}"),
        format!("--height={height}"),
        "--window-position=0,0".into(),
        "--autoplay-policy=no-user-gesture-required".into(),
        // Persistent profile: streaming-app sign-ins (Netflix, Disney+, YouTube,
        // Free TV) must survive a reboot; /tmp was wiped at every boot.
        format!("--user-data-dir={CHROMIUM_PROFILE_DIR}"),
        "--lang=fr".into(),
        // rascar-app drives the tabs (open a streaming app, back to the
        // dashboard) through DevTools. Loopback only: never reachable off-box.
        format!("--remote-debugging-port={DEVTOOLS_PORT}"),
        "--remote-debugging-address=127.0.0.1".into(),
    ];
    if as_root {
        args.push("--no-sandbox".into());
    }
    if wayland {
        // Last on the line, after Debian's CHROMIUM_FLAGS: GL through ANGLE on
        // the compositor's EGL (the Mali), not the X11 default.
        args.extend(["--ozone-platform=wayland".into(), "--use-gl=angle".into(), "--use-angle=gles-egl".into()]);
    }
    args.push(url.into());
    args
}

#[cfg(test)]
mod render_args_tests {
    use super::*;

    #[test]
    fn root_gets_no_sandbox_or_chromium_refuses_to_start() {
        let args = chromium_args(1200, 512, "http://localhost:8080", true, false);
        assert!(
            args.iter().any(|a| a == "--no-sandbox"),
            "running as root without --no-sandbox exits 1 and streams a black screen: {args:?}"
        );
    }

    #[test]
    fn devtools_is_loopback_only_and_the_profile_persists() {
        let args = chromium_args(1200, 512, "http://localhost:8080", true, false);
        assert!(args.iter().any(|a| a == "--remote-debugging-port=9222"), "{args:?}");
        assert!(args.iter().any(|a| a == "--remote-debugging-address=127.0.0.1"), "{args:?}");
        assert!(args.iter().any(|a| a == "--user-data-dir=/var/lib/rascar/chromium-profile"), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("/tmp/")), "{args:?}");
    }

    #[test]
    fn only_the_frame_unlock_extension_is_loaded() {
        let args = chromium_args(1200, 512, "http://localhost:8080", true, false);
        assert!(args.iter().any(|a| a == "--load-extension=/opt/orangecar/app/chromium-ext/frame-unlock"), "{args:?}");
        assert!(args.iter().any(|a| a == "--disable-extensions-except=/opt/orangecar/app/chromium-ext/frame-unlock"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--disable-extensions"), "--disable-extensions would also drop ours: {args:?}");
    }

    #[test]
    fn wayland_runs_chromium_on_wayland_with_gpu_gl() {
        let args = chromium_args(1200, 512, "http://localhost:8080", true, true);
        for flag in ["--ozone-platform=wayland", "--use-gl=angle", "--use-angle=gles-egl"] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
        assert_eq!(args.last().map(String::as_str), Some("http://localhost:8080"));
        let x11 = chromium_args(1200, 512, "http://localhost:8080", true, false);
        assert!(!x11.iter().any(|a| a.starts_with("--ozone-platform")), "{x11:?}");
    }

    #[test]
    fn non_root_keeps_the_sandbox() {
        let args = chromium_args(1200, 512, "http://localhost:8080", false, false);
        assert!(!args.iter().any(|a| a == "--no-sandbox"), "{args:?}");
    }

    #[test]
    fn the_url_is_last_and_the_geometry_is_the_negotiated_one() {
        let args = chromium_args(800, 480, "http://localhost:8080", true, false);
        assert_eq!(args.last().map(String::as_str), Some("http://localhost:8080"));
        assert!(args.iter().any(|a| a == "--window-size=800,480"), "{args:?}");
        assert!(args.iter().any(|a| a == "--width=800"), "{args:?}");
        assert!(args.iter().any(|a| a == "--height=480"), "{args:?}");
    }
}
