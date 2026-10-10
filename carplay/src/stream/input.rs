//! Touch injection: map touch coordinates from the car onto the dashboard
//! surface and inject them locally.
//!
//! # What is used where
//!
//! * [`InputInjector`] (a uinput virtual touchscreen, with an X11 XTest path
//!   when an X display is given) and [`map_coords`] are used by the CarPlay
//!   session: touches arrive as `hidSendReport` events on the AirPlay event
//!   channel, are decoded there (`airplay::carplay_session`) and injected here.
//!   Under Xvfb the injector drives Chromium's pointer through XTest; under the
//!   Wayland kiosk it uses the uinput touchscreen, which cage reads via libinput.
//!   The injector comes from the project's earlier Android Auto bridge code.
//! * [`run`] belongs to the older AirPlay screen-mirroring path (`stream::run`,
//!   Apple TV style), which has no defined reverse-touch channel. There the
//!   placeholder parser [`parse_touch`] is **disabled by default**: the loop
//!   hex-logs whatever arrives on the data connection's read half and injects
//!   nothing. `RASCAR_TOUCH_PARSER=experimental` lets the placeholder parser
//!   drive the injector (records outside the touch resolution are rejected, not
//!   clamped).

use anyhow::{Context, Result};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{close, write};
use std::os::fd::{BorrowedFd, RawFd};
use tokio::io::AsyncReadExt;
use tokio::net::tcp::OwnedReadHalf;
use tracing::{debug, info, warn};

// Linux input event constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2F;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;

const BTN_TOUCH: u16 = 0x14A;
const BTN_TOOL_FINGER: u16 = 0x145;

// uinput ioctl codes
const UI_DEV_CREATE: u64 = 0x5501;
const UI_DEV_DESTROY: u64 = 0x5502;
const UI_SET_EVBIT: u64 = 0x40045564;
const UI_SET_KEYBIT: u64 = 0x40045565;
const UI_SET_ABSBIT: u64 = 0x40045567;
const UI_SET_PROPBIT: u64 = 0x4004556e;
/// A touchSCREEN (absolute, on the display), not a touchpad: without it
/// libinput classifies this multi-touch device as a touchpad and a Wayland
/// compositor moves a pointer instead of delivering touches.
const INPUT_PROP_DIRECT: u64 = 0x01;

// X11 event type constants (XTest fake_input)
const X11_MOTION_NOTIFY: u8 = 6;
const X11_BUTTON_PRESS: u8 = 4;
const X11_BUTTON_RELEASE: u8 = 5;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Press = 0,
    Release = 1,
    Drag = 2,
}

impl TouchAction {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Press),
            1 => Some(Self::Release),
            2 => Some(Self::Drag),
            _ => None,
        }
    }
}

struct X11Injector {
    conn: x11rb::rust_connection::RustConnection,
    root: x11rb::protocol::xproto::Window,
}

impl X11Injector {
    fn open(disp: &str) -> Option<Self> {
        use x11rb::connection::Connection;
        let (conn, screen_num) = x11rb::connect(Some(disp))
            .map_err(|e| tracing::warn!("X11 connect failed ({}): {e}", disp))
            .ok()?;
        let root = conn.setup().roots[screen_num].root;
        info!("input: X11 XTest injector connected (display={}, root={})", disp, root);
        Some(Self { conn, root })
    }

    /// Inject one event. Returns false if the X connection rejected any part of
    /// it — the caller counts consecutive failures instead of looping blind.
    fn inject(&self, action: TouchAction, x: u32, y: u32) -> bool {
        use x11rb::connection::Connection;
        use x11rb::protocol::xtest::ConnectionExt;
        let ix = x as i16;
        let iy = y as i16;
        let mut ok = self
            .conn
            .xtest_fake_input(X11_MOTION_NOTIFY, 0, 0, self.root, ix, iy, 0)
            .is_ok();
        match action {
            TouchAction::Press => {
                ok &= self
                    .conn
                    .xtest_fake_input(X11_BUTTON_PRESS, 1, 0, self.root, ix, iy, 0)
                    .is_ok();
            }
            TouchAction::Release => {
                ok &= self
                    .conn
                    .xtest_fake_input(X11_BUTTON_RELEASE, 1, 0, self.root, ix, iy, 0)
                    .is_ok();
            }
            TouchAction::Drag => {}
        }
        ok &= self.conn.flush().is_ok();
        ok
    }
}

pub struct InputInjector {
    screen_width: u32,
    screen_height: u32,
    max_contacts: u32,
    fd: Option<RawFd>,
    x11: Option<X11Injector>,
}

impl InputInjector {
    /// `display` is the X display to drive (e.g. `":0"`); pass an empty string
    /// to force the uinput path. It is passed explicitly rather than read from
    /// the environment: this runs on the multi-threaded runtime, where mutating
    /// or racing the process environ is unsound.
    pub fn new(display: &str, screen_width: u32, screen_height: u32) -> Self {
        let x11 = if display.is_empty() {
            None
        } else {
            X11Injector::open(display)
        };
        Self {
            screen_width,
            screen_height,
            max_contacts: 5,
            fd: None,
            x11,
        }
    }

    pub fn setup(&mut self) -> Result<()> {
        if self.x11.is_some() {
            info!("input: X11 mode — skipping uinput device (using XTest only)");
            return Ok(());
        }
        let fd = open(
            "/dev/uinput",
            OFlag::O_WRONLY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .context("opening /dev/uinput")?;

        unsafe {
            ioctl(fd, UI_SET_EVBIT, EV_SYN as u64)?;
            ioctl(fd, UI_SET_EVBIT, EV_KEY as u64)?;
            ioctl(fd, UI_SET_EVBIT, EV_ABS as u64)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as u64)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_FINGER as u64)?;
            ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT)?;
            for axis in [
                ABS_X, ABS_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID,
                ABS_MT_POSITION_X, ABS_MT_POSITION_Y,
            ] {
                ioctl(fd, UI_SET_ABSBIT, axis as u64)?;
            }
        }

        let dev = self.build_user_dev();
        write(unsafe { BorrowedFd::borrow_raw(fd) }, &dev).context("writing uinput_user_dev")?;
        unsafe { ioctl(fd, UI_DEV_CREATE, 0)? };
        self.fd = Some(fd);
        info!("input: virtual touchscreen created ({}x{})", self.screen_width, self.screen_height);
        Ok(())
    }

    fn build_user_dev(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 80 + 8 + 4 + 256 * 4];
        let name = b"RasCar CarPlay Touchscreen";
        buf[..name.len()].copy_from_slice(name);
        let off = 80;
        buf[off..off + 2].copy_from_slice(&3u16.to_le_bytes()); // BUS_USB
        buf[off + 2..off + 4].copy_from_slice(&0x05ACu16.to_le_bytes()); // Apple vendor
        buf[off + 4..off + 6].copy_from_slice(&0x0001u16.to_le_bytes());
        buf[off + 6..off + 8].copy_from_slice(&0x0001u16.to_le_bytes());
        let absmax_off = 92;
        let w = self.screen_width.saturating_sub(1) as i32;
        let h = self.screen_height.saturating_sub(1) as i32;
        let set_abs = |buf: &mut Vec<u8>, code: u16, val: i32| {
            let off = absmax_off + (code as usize) * 4;
            buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
        };
        set_abs(&mut buf, ABS_X, w);
        set_abs(&mut buf, ABS_Y, h);
        set_abs(&mut buf, ABS_MT_SLOT, self.max_contacts.saturating_sub(1) as i32);
        set_abs(&mut buf, ABS_MT_TRACKING_ID, 65535);
        set_abs(&mut buf, ABS_MT_POSITION_X, w);
        set_abs(&mut buf, ABS_MT_POSITION_Y, h);
        buf
    }

    /// Inject one touch event. Returns false when the event could not be
    /// delivered (dead X connection, uinput write failure) so the caller can
    /// stop instead of "injecting" into the void forever.
    pub fn inject_touch(&self, action: TouchAction, x: u32, y: u32, pointer_id: u32) -> bool {
        if pointer_id == 0 {
            echo_touch(action, x, y);
        }
        if let Some(ref x11) = self.x11 {
            if pointer_id == 0 {
                return x11.inject(action, x, y);
            }
            return true;
        }
        let Some(fd) = self.fd else { return false };
        let slot = (pointer_id % self.max_contacts) as i32;
        let mut ok = self.emit(fd, EV_ABS, ABS_MT_SLOT, slot);
        match action {
            TouchAction::Release => {
                ok &= self.emit(fd, EV_ABS, ABS_MT_TRACKING_ID, -1);
                if pointer_id == 0 {
                    ok &= self.emit(fd, EV_KEY, BTN_TOUCH, 0);
                }
            }
            TouchAction::Press | TouchAction::Drag => {
                ok &= self.emit(fd, EV_ABS, ABS_MT_TRACKING_ID, pointer_id as i32);
                ok &= self.emit(fd, EV_ABS, ABS_MT_POSITION_X, x as i32);
                ok &= self.emit(fd, EV_ABS, ABS_MT_POSITION_Y, y as i32);
                if action == TouchAction::Press && pointer_id == 0 {
                    ok &= self.emit(fd, EV_KEY, BTN_TOUCH, 1);
                }
                if pointer_id == 0 {
                    ok &= self.emit(fd, EV_ABS, ABS_X, x as i32);
                    ok &= self.emit(fd, EV_ABS, ABS_Y, y as i32);
                }
            }
        }
        // The SYN_REPORT is what commits the packet; losing it after a
        // BTN_TOUCH=1 leaves the virtual touchscreen latched pressed, so it is
        // always attempted and always reported.
        ok &= self.emit(fd, EV_SYN, SYN_REPORT, 0);
        ok
    }

    fn emit(&self, fd: RawFd, ev_type: u16, code: u16, value: i32) -> bool {
        let mut ev = [0u8; 24];
        ev[16..18].copy_from_slice(&ev_type.to_le_bytes());
        ev[18..20].copy_from_slice(&code.to_le_bytes());
        ev[20..24].copy_from_slice(&value.to_le_bytes());
        write_event(fd, &ev)
    }

    pub fn destroy(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe { let _ = ioctl(fd, UI_DEV_DESTROY, 0); }
            let _ = close(fd);
            info!("input: virtual touchscreen destroyed");
        }
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Write a whole `input_event` to the `O_NONBLOCK` uinput fd, retrying short
/// writes and transient `EAGAIN`/`EINTR` a bounded number of times. A partially
/// written event would desync the device, so a stall is reported as a failure
/// rather than silently dropped.
fn write_event(fd: RawFd, buf: &[u8]) -> bool {
    const MAX_STALLED_ATTEMPTS: u32 = 32;
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut off = 0usize;
    let mut stalled = 0u32;
    while off < buf.len() {
        match write(bfd, &buf[off..]) {
            Ok(0) => stalled += 1,
            Ok(n) => {
                off += n;
                stalled = 0;
            }
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => stalled += 1,
            Err(e) => {
                warn!("input: uinput write failed: {e}");
                return false;
            }
        }
        if stalled >= MAX_STALLED_ATTEMPTS {
            warn!("input: uinput write stalled after {off}/{} bytes", buf.len());
            return false;
        }
        if stalled > 0 {
            std::hint::spin_loop();
        }
    }
    true
}

unsafe fn ioctl(fd: RawFd, request: u64, arg: u64) -> Result<()> {
    let ret = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg as libc::c_ulong) };
    if ret < 0 {
        anyhow::bail!("ioctl 0x{:x} failed: {}", request, std::io::Error::last_os_error());
    }
    Ok(())
}

/// Map a touch point from the car's advertised touchscreen resolution onto the
/// dashboard surface. Clamps to the destination bounds.
/// Where rascar-app listens for touch echoes (it draws the touch glow; the
/// car's video carries no cursor, and touches inside embedded sites never
/// reach the dashboard page, so the injector is the one place that sees all).
const TOUCH_ECHO_ADDR: &str = "127.0.0.1:7702";
/// At most one drag echo per this interval (the car reports at ~60 Hz).
const TOUCH_ECHO_DRAG_EVERY: std::time::Duration = std::time::Duration::from_millis(33);

/// Fire-and-forget: one UDP datagram "p|d|r x y" per touch. Never blocks and
/// never fails the injection — nobody listening just means no glow.
fn echo_touch(action: TouchAction, x: u32, y: u32) {
    use std::sync::{Mutex, OnceLock};
    static SOCK: OnceLock<Option<std::net::UdpSocket>> = OnceLock::new();
    static LAST_DRAG: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let tag = match action {
        TouchAction::Press => 'p',
        TouchAction::Drag => {
            let mut last = LAST_DRAG.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|t| t.elapsed() < TOUCH_ECHO_DRAG_EVERY) {
                return;
            }
            *last = Some(std::time::Instant::now());
            'd'
        }
        TouchAction::Release => 'r',
    };
    let sock = SOCK.get_or_init(|| {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").ok()?;
        s.set_nonblocking(true).ok()?;
        Some(s)
    });
    if let Some(s) = sock {
        let _ = s.send_to(format!("{tag} {x} {y}").as_bytes(), TOUCH_ECHO_ADDR);
    }
}

pub fn map_coords(x: u32, y: u32, from: (u32, u32), to: (u32, u32)) -> (u32, u32) {
    let (fw, fh) = from;
    let (tw, th) = to;
    if fw == 0 || fh == 0 {
        // Unknown source resolution — pass through, clamped.
        return (x.min(tw.saturating_sub(1)), y.min(th.saturating_sub(1)));
    }
    let mx = ((x as u64 * tw as u64) / fw as u64) as u32;
    let my = ((y as u64 * th as u64) / fh as u64) as u32;
    (mx.min(tw.saturating_sub(1)), my.min(th.saturating_sub(1)))
}

/// A parsed touch event in the car's coordinate space (pre-mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawTouch {
    pub action: TouchAction,
    pub x: u32,
    pub y: u32,
    pub pointer_id: u32,
}

/// **BEST-EFFORT / PLACEHOLDER** parser for the CarPlay reverse-touch wire
/// format, which is unverified (`// TODO(hw)`). Hypothesised layout, matching
/// nothing authoritative yet: a 16-byte record `[action:u8][pad:3][x:u32 LE]
/// [y:u32 LE][pointer:u32 LE]`. Returns the events it could decode; the runtime
/// logs raw bytes alongside so a real capture can correct this.
pub fn parse_touch(data: &[u8]) -> Vec<RawTouch> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 16 <= data.len() {
        let action = match TouchAction::from_u8(data[pos]) {
            Some(a) => a,
            None => {
                pos += 1;
                continue;
            }
        };
        let x = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        let y = u32::from_le_bytes(data[pos + 8..pos + 12].try_into().unwrap());
        let pid = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap());
        out.push(RawTouch { action, x, y, pointer_id: pid });
        pos += 16;
    }
    out
}

/// Env variable gating the unverified reverse-touch parser. Only the exact
/// value `experimental` enables injection.
pub const TOUCH_PARSER_ENV: &str = "RASCAR_TOUCH_PARSER";

/// Gate decision, split out from the environment read so it is testable without
/// mutating the process environ.
fn touch_parser_enabled_from(value: Option<&str>) -> bool {
    value == Some("experimental")
}

/// True when the operator has explicitly opted into the placeholder parser.
fn touch_parser_enabled() -> bool {
    touch_parser_enabled_from(std::env::var(TOUCH_PARSER_ENV).ok().as_deref())
}

/// Hex dump for capture/analysis of the (unknown) reverse-channel format.
fn hex_dump(data: &[u8], max: usize) -> String {
    use std::fmt::Write;
    let shown = data.len().min(max);
    let mut s = String::with_capacity(shown * 3 + 16);
    for b in &data[..shown] {
        let _ = write!(s, "{b:02x} ");
    }
    if data.len() > shown {
        let _ = write!(s, "... (+{} B)", data.len() - shown);
    }
    s
}

/// Reject records the car could not possibly have produced. `from` is the car's
/// advertised touch resolution; `(0, 0)` means "unknown", in which case nothing
/// can be validated and the record is accepted.
fn touch_in_bounds(t: &RawTouch, from: (u32, u32)) -> bool {
    let (fw, fh) = from;
    if fw == 0 || fh == 0 {
        return true;
    }
    t.x < fw && t.y < fh
}

/// Reverse-channel touch loop: read from the video data-conn read half and, when
/// the experimental parser is enabled, decode / map / inject. Runs until the
/// channel closes.
///
/// **Injection is OFF by default.** The wire format is unverified (`TODO(hw)`),
/// and [`parse_touch`] is a resyncing scanner that would happily synthesize
/// clicks out of ciphertext or keep-alives. With the gate closed the loop only
/// hex-logs what arrives, so a capture can be taken from a real head unit.
///
/// `display` is the X display to inject into (empty string → uinput).
/// `car_touch_res` is the car's advertised touchscreen resolution (falls back to
/// the receiver display size). `dash_res` is our dashboard surface (800x480).
pub async fn run(
    mut rd: OwnedReadHalf,
    display: String,
    car_touch_res: (u32, u32),
    dash_res: (u32, u32),
) -> Result<()> {
    /// Consecutive failed injections before we stop pretending to inject.
    const MAX_CONSECUTIVE_INJECT_FAILURES: u32 = 16;
    /// Chunks logged at info! before dropping to debug! (avoid flooding).
    const LOUD_CHUNKS: u64 = 32;

    let enabled = touch_parser_enabled();
    let mut injector = if enabled {
        warn!(
            "input: {TOUCH_PARSER_ENV}=experimental — the UNVERIFIED placeholder touch parser is \
             ACTIVE. It can synthesize touches from arbitrary bytes; use only with a capture setup."
        );
        let mut inj = InputInjector::new(&display, dash_res.0, dash_res.1);
        if let Err(e) = inj.setup() {
            warn!("input: injector setup failed ({e:#}) — touch will be dropped");
        }
        Some(inj)
    } else {
        info!(
            "input: reverse-touch injection DISABLED (default). The CarPlay reverse-touch wire \
             format is unverified (TODO(hw)); bytes are hex-logged for capture/analysis and NOTHING \
             is injected. Set {TOUCH_PARSER_ENV}=experimental to enable the placeholder parser."
        );
        None
    };

    info!(
        "input: reverse-touch loop started (car touch res {car_touch_res:?} → dashboard {dash_res:?}, \
         parser={})",
        if enabled { "experimental" } else { "off" }
    );

    let mut buf = vec![0u8; 8192];
    let mut chunks: u64 = 0;
    let mut consecutive_failures: u32 = 0;
    loop {
        let n = match rd.read(&mut buf).await {
            Ok(0) => {
                info!("input: reverse channel closed (EOF) — stopping touch loop");
                break;
            }
            Ok(n) => n,
            Err(e) => {
                warn!("input: reverse channel read error: {e:#}");
                break;
            }
        };
        let chunk = &buf[..n];
        chunks += 1;

        let Some(inj) = injector.as_mut() else {
            // Gate closed: capture only.
            if chunks <= LOUD_CHUNKS {
                info!("input: reverse channel chunk #{chunks} {n}B (not injected): {}", hex_dump(chunk, 64));
            } else {
                debug!("input: reverse channel chunk #{chunks} {n}B (not injected): {}", hex_dump(chunk, 64));
            }
            continue;
        };

        debug!("input: reverse channel {n}B: {}", hex_dump(chunk, 32));

        let mut stop = false;
        for t in parse_touch(chunk) {
            if !touch_in_bounds(&t, car_touch_res) {
                // Out-of-range coordinates mean this was not a touch record.
                // Rejecting beats clamping it onto the dashboard's corner.
                debug!(
                    "input: rejecting out-of-range record {:?} ({},{}) — car touch res is {car_touch_res:?}",
                    t.action, t.x, t.y
                );
                continue;
            }
            let (mx, my) = map_coords(t.x, t.y, car_touch_res, dash_res);
            debug!("input: touch {:?} car=({},{}) → dash=({mx},{my}) pid={}", t.action, t.x, t.y, t.pointer_id);
            if inj.inject_touch(t.action, mx, my, t.pointer_id) {
                consecutive_failures = 0;
            } else {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_INJECT_FAILURES {
                    warn!(
                        "input: {consecutive_failures} consecutive injection failures — the sink is \
                         gone; stopping the touch loop instead of looping blind"
                    );
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_coords_scales_down() {
        // Car 1280x720 → dashboard 800x480. Center maps to center.
        let (x, y) = map_coords(640, 360, (1280, 720), (800, 480));
        assert_eq!((x, y), (400, 240));
        // Bottom-right corner clamps in-bounds.
        let (x, y) = map_coords(1280, 720, (1280, 720), (800, 480));
        assert_eq!((x, y), (799, 479));
        // Origin.
        assert_eq!(map_coords(0, 0, (1280, 720), (800, 480)), (0, 0));
    }

    #[test]
    fn map_coords_unknown_source_passes_through_clamped() {
        assert_eq!(map_coords(50, 60, (0, 0), (800, 480)), (50, 60));
        assert_eq!(map_coords(5000, 5000, (0, 0), (800, 480)), (799, 479));
    }

    #[test]
    fn touch_bounds_reject_out_of_range_records() {
        let t = |x, y| RawTouch { action: TouchAction::Press, x, y, pointer_id: 0 };
        assert!(touch_in_bounds(&t(0, 0), (1280, 720)));
        assert!(touch_in_bounds(&t(1279, 719), (1280, 720)));
        assert!(!touch_in_bounds(&t(1280, 719), (1280, 720)));
        assert!(!touch_in_bounds(&t(0, 720), (1280, 720)));
        assert!(!touch_in_bounds(&t(0xDEAD_BEEF, 3), (1280, 720)));
        // Unknown source resolution cannot validate anything.
        assert!(touch_in_bounds(&t(99999, 99999), (0, 0)));
    }

    #[test]
    fn touch_parser_is_off_unless_explicitly_experimental() {
        assert_eq!(TOUCH_PARSER_ENV, "RASCAR_TOUCH_PARSER");
        assert!(touch_parser_enabled_from(Some("experimental")));
        // Everything else — unset, empty, truthy-looking, or differently cased —
        // must leave injection disabled.
        for v in [None, Some(""), Some("1"), Some("true"), Some("yes"), Some("Experimental"), Some("experimental ")] {
            assert!(!touch_parser_enabled_from(v), "{v:?} must not enable the parser");
        }
    }

    #[test]
    fn parse_touch_decodes_record() {
        let mut data = vec![0u8; 16];
        data[0] = 0; // Press
        data[4..8].copy_from_slice(&123u32.to_le_bytes());
        data[8..12].copy_from_slice(&456u32.to_le_bytes());
        data[12..16].copy_from_slice(&1u32.to_le_bytes());
        let evs = parse_touch(&data);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0], RawTouch { action: TouchAction::Press, x: 123, y: 456, pointer_id: 1 });
    }
}
