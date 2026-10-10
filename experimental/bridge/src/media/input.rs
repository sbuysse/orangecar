//! Touch input injection for Android Auto.
//!
//! Receives touch events from the car's head unit and injects them into the
//! Pi's input subsystem. Two backends:
//!   - X11 (DISPLAY set): X11 XTest extension via x11rb — persistent connection,
//!     no subprocess, sub-millisecond per event. Used with Xvfb.
//!   - Framebuffer (no DISPLAY): Linux uinput virtual touchscreen.
//!
//! NOTE: do NOT inject via both simultaneously. Xvfb registers the uinput device
//! as mouse0 and will fight xdotool/XTest, causing cursor jumps.

use anyhow::{Context, Result};
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use nix::unistd::{close, write};
use std::os::fd::{BorrowedFd, RawFd};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long to wait before re-opening an X11 connection that failed. Without a
/// back-off a dead X server would be re-dialled on every touch event.
const X11_REOPEN_BACKOFF: Duration = Duration::from_secs(2);

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

/// Persistent X11 XTest connection for direct mouse event injection.
/// Much faster than spawning xdotool subprocesses and avoids cursor races.
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
        info!("X11 XTest injector connected (display={}, root={})", disp, root);
        Some(Self { conn, root })
    }

    /// Inject one event. Returns false when the X connection has failed (server
    /// restarted, socket closed) so the caller can drop and re-open it — silently
    /// discarding the error would kill touch for the rest of the process lifetime
    /// while the bridge kept logging every tap as if it had landed.
    fn inject(&self, action: TouchAction, x: u32, y: u32) -> bool {
        use x11rb::connection::Connection;
        use x11rb::protocol::xtest::ConnectionExt;
        let ix = x as i16;
        let iy = y as i16;
        // Always move cursor to the event position first.
        if self
            .conn
            .xtest_fake_input(X11_MOTION_NOTIFY, 0, 0, self.root, ix, iy, 0)
            .is_err()
        {
            return false;
        }
        let sent = match action {
            TouchAction::Press => self
                .conn
                .xtest_fake_input(X11_BUTTON_PRESS, 1, 0, self.root, ix, iy, 0)
                .is_ok(),
            TouchAction::Release => self
                .conn
                .xtest_fake_input(X11_BUTTON_RELEASE, 1, 0, self.root, ix, iy, 0)
                .is_ok(),
            TouchAction::Drag => true,
        };
        // Flush sends queued requests to the X server over the local socket and is
        // where a broken connection actually surfaces.
        sent && self.conn.flush().is_ok()
    }
}

pub struct InputInjector {
    screen_width: u32,
    screen_height: u32,
    max_contacts: u32,
    fd: Option<RawFd>,
    /// X11 XTest injector. Present when an X11 display is in use and reachable.
    x11: Option<X11Injector>,
    /// The X11 display to inject into, when X11 is the chosen backend. Kept so a
    /// connection lost mid-session can be re-opened on the same display.
    x11_display: Option<String>,
    /// Earliest instant at which a failed X11 connection may be re-opened.
    x11_retry_at: Option<Instant>,
}

impl InputInjector {
    /// `display` is the `--display` flag: an X11 display (":0"), or "fb"/"test"
    /// for the framebuffer/synthetic backends. It MUST be the same value the
    /// video encoder captures from, otherwise video comes from one screen and
    /// touches go to another.
    pub fn new(screen_width: u32, screen_height: u32, display: &str) -> Self {
        let x11_display = match display {
            // Not X11 displays — use the uinput backend.
            "fb" | "test" | "" => None,
            disp => Some(disp.to_string()),
        };
        let x11 = x11_display.as_deref().and_then(X11Injector::open);

        Self {
            screen_width,
            screen_height,
            max_contacts: 5,
            fd: None,
            // Only keep the display for reconnects if the first open succeeded;
            // otherwise the uinput backend is chosen for the process lifetime and
            // must not be pre-empted later by a late-arriving X server.
            x11_display: x11_display.filter(|_| x11.is_some()),
            x11,
            x11_retry_at: None,
        }
    }

    /// Re-open the X11 connection after a failure, respecting the back-off.
    fn ensure_x11(&mut self) {
        if self.x11.is_some() {
            return;
        }
        let Some(disp) = self.x11_display.as_deref() else {
            return;
        };
        match self.x11_retry_at {
            Some(at) if Instant::now() < at => return,
            _ => {}
        }
        match X11Injector::open(disp) {
            Some(inj) => {
                info!("X11 injector re-opened on {disp}");
                self.x11 = Some(inj);
                self.x11_retry_at = None;
            }
            None => self.x11_retry_at = Some(Instant::now() + X11_REOPEN_BACKOFF),
        }
    }

    /// Create the virtual touchscreen device via /dev/uinput.
    /// No-op when X11 is available — uinput conflicts with Xvfb (registered as
    /// mouse0, Xvfb centres cursor at axis midpoint and fights XTest).
    pub fn setup(&mut self) -> Result<()> {
        if self.x11_display.is_some() {
            info!("X11 mode: skipping uinput device (using XTest only)");
            return Ok(());
        }
        let fd = open(
            "/dev/uinput",
            OFlag::O_WRONLY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .context("opening /dev/uinput")?;

        // Enable event types
        unsafe {
            ioctl(fd, UI_SET_EVBIT, EV_SYN as u64)?;
            ioctl(fd, UI_SET_EVBIT, EV_KEY as u64)?;
            ioctl(fd, UI_SET_EVBIT, EV_ABS as u64)?;

            ioctl(fd, UI_SET_KEYBIT, BTN_TOUCH as u64)?;
            ioctl(fd, UI_SET_KEYBIT, BTN_TOOL_FINGER as u64)?;

            for axis in [
                ABS_X, ABS_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID,
                ABS_MT_POSITION_X, ABS_MT_POSITION_Y,
            ] {
                ioctl(fd, UI_SET_ABSBIT, axis as u64)?;
            }
        }

        // Write uinput_user_dev struct
        let dev = self.build_user_dev();
        write(unsafe { BorrowedFd::borrow_raw(fd) }, &dev).context("writing uinput_user_dev")?;

        // Create device
        unsafe { ioctl(fd, UI_DEV_CREATE, 0)? };
        self.fd = Some(fd);

        info!(
            "virtual touchscreen created: {}x{}",
            self.screen_width, self.screen_height
        );
        Ok(())
    }

    fn build_user_dev(&self) -> Vec<u8> {
        // struct uinput_user_dev:
        //   name[80] + id{bus,vendor,product,version} (8 bytes)
        //   + ff_effects_max(4) + absmax[64](256) + absmin[64](256)
        //   + absfuzz[64](256) + absflat[64](256)
        let mut buf = vec![0u8; 80 + 8 + 4 + 256 * 4];

        // Name
        let name = b"RasCar Touchscreen";
        buf[..name.len()].copy_from_slice(name);

        // ID: BUS_USB=0x03
        let off = 80;
        buf[off..off + 2].copy_from_slice(&3u16.to_le_bytes()); // bustype
        buf[off + 2..off + 4].copy_from_slice(&0x18D1u16.to_le_bytes()); // vendor
        buf[off + 4..off + 6].copy_from_slice(&0x0001u16.to_le_bytes()); // product
        buf[off + 6..off + 8].copy_from_slice(&0x0001u16.to_le_bytes()); // version

        // absmax starts at offset 92 (80+8+4), each entry is i32 (4 bytes)
        let absmax_off = 92;
        // saturating_sub guards against a 0 dimension (e.g. `--width 0`), which
        // would otherwise underflow-panic in debug / wrap to a degenerate max.
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

    /// Inject a single touch event.
    pub fn inject_touch(&mut self, action: TouchAction, x: u32, y: u32, pointer_id: u32) {
        // X11 path: use XTest directly. Skip uinput — Xvfb registers the uinput device
        // as mouse0 and would fight XTest, causing cursor jumps.
        if self.x11_display.is_some() {
            self.ensure_x11();
            // Only inject primary finger for cursor/click; ignore other pointers.
            if pointer_id == 0 {
                let alive = match self.x11 {
                    Some(ref x11) => x11.inject(action, x, y),
                    None => return, // disconnected, waiting on the re-open back-off
                };
                if !alive {
                    warn!("X11 injection failed — dropping connection, will re-open");
                    self.x11 = None;
                    self.x11_retry_at = Some(Instant::now() + X11_REOPEN_BACKOFF);
                }
            }
            return;
        }

        // Framebuffer/headless path: uinput only.
        let Some(fd) = self.fd else { return };
        let slot = (pointer_id % self.max_contacts) as i32;

        self.emit(fd, EV_ABS, ABS_MT_SLOT, slot);

        match action {
            TouchAction::Release => {
                self.emit(fd, EV_ABS, ABS_MT_TRACKING_ID, -1);
                if pointer_id == 0 {
                    self.emit(fd, EV_KEY, BTN_TOUCH, 0);
                }
            }
            TouchAction::Press | TouchAction::Drag => {
                self.emit(fd, EV_ABS, ABS_MT_TRACKING_ID, pointer_id as i32);
                self.emit(fd, EV_ABS, ABS_MT_POSITION_X, x as i32);
                self.emit(fd, EV_ABS, ABS_MT_POSITION_Y, y as i32);
                if action == TouchAction::Press && pointer_id == 0 {
                    self.emit(fd, EV_KEY, BTN_TOUCH, 1);
                }
                // Legacy single-touch
                if pointer_id == 0 {
                    self.emit(fd, EV_ABS, ABS_X, x as i32);
                    self.emit(fd, EV_ABS, ABS_Y, y as i32);
                }
            }
        }

        self.emit(fd, EV_SYN, SYN_REPORT, 0);
    }

    fn emit(&self, fd: RawFd, ev_type: u16, code: u16, value: i32) {
        // struct input_event on 64-bit: tv_sec(8) + tv_usec(8) + type(2) + code(2) + value(4) = 24 bytes
        let mut ev = [0u8; 24];
        ev[16..18].copy_from_slice(&ev_type.to_le_bytes());
        ev[18..20].copy_from_slice(&code.to_le_bytes());
        ev[20..24].copy_from_slice(&value.to_le_bytes());
        let _ = write(unsafe { BorrowedFd::borrow_raw(fd) }, &ev);
    }

    pub fn destroy(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe { let _ = ioctl(fd, UI_DEV_DESTROY, 0); }
            let _ = close(fd);
            info!("virtual touchscreen destroyed");
        }
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        self.destroy();
    }
}

unsafe fn ioctl(fd: RawFd, request: u64, arg: u64) -> Result<()> {
    let ret = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg as libc::c_ulong) };
    if ret < 0 {
        anyhow::bail!("ioctl 0x{:x} failed: {}", request, std::io::Error::last_os_error());
    }
    Ok(())
}

/// Maximum nesting depth for the field-3 recursion in `parse_touch_event`.
/// Bounds adversarial head-unit input that could otherwise blow the stack.
const MAX_TOUCH_DEPTH: u32 = 8;

/// Parse AA touch event protobuf (simplified).
/// Returns Vec of (action, x, y, pointer_id).
pub fn parse_touch_event(data: &[u8]) -> Vec<(TouchAction, u32, u32, u32)> {
    parse_touch_event_inner(data, 0)
}

fn parse_touch_event_inner(data: &[u8], depth: u32) -> Vec<(TouchAction, u32, u32, u32)> {
    let mut results = Vec::new();

    // Bail on excessive nesting — malformed/hostile input must not recurse forever.
    if depth >= MAX_TOUCH_DEPTH {
        return results;
    }

    // Simplified protobuf parsing — scan for known field patterns.
    // A full implementation should use prost-generated types.
    let mut action = TouchAction::Press;
    let mut locations: Vec<(u32, u32, u32)> = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let tag = data[pos];
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        pos += 1;

        match wire_type {
            0 => {
                // varint
                let (value, new_pos) = match decode_varint(data, pos) {
                    Some(v) => v,
                    None => break,
                };
                pos = new_pos;
                if field_num == 3 {
                    action = TouchAction::from_u8(value as u8).unwrap_or(TouchAction::Press);
                }
            }
            2 => {
                // length-delimited
                let (length, new_pos) = match decode_varint(data, pos) {
                    Some(v) => v,
                    None => break,
                };
                pos = new_pos;
                let length = length as usize;
                // Guard against length exceeding the buffer and against pos+length
                // overflowing usize on adversarial input.
                if length > data.len() {
                    break;
                }
                let end = match pos.checked_add(length) {
                    Some(e) if e <= data.len() => e,
                    _ => break,
                };
                if field_num == 1 {
                    // touch_location
                    if let Some(loc) = parse_touch_location(&data[pos..end]) {
                        locations.push(loc);
                    }
                } else if field_num == 3 {
                    // Nested touch_event — merge its results rather than returning
                    // early, which would discard the locations/action already parsed
                    // at this level and any fields after it.
                    results.extend(parse_touch_event_inner(&data[pos..end], depth + 1));
                }
                pos = end;
            }
            _ => break,
        }
    }

    for (x, y, pid) in locations {
        results.push((action, x, y, pid));
    }
    results
}

fn parse_touch_location(data: &[u8]) -> Option<(u32, u32, u32)> {
    let mut x = 0u32;
    let mut y = 0u32;
    let mut pid = 0u32;
    let mut pos = 0;

    while pos < data.len() {
        let tag = data[pos];
        let field_num = tag >> 3;
        let wire_type = tag & 0x07;
        pos += 1;

        // The wire type MUST be honoured. Decoding every field as a varint makes a
        // length-delimited or fixed-width field consume its length/first word as
        // the value, after which the remaining bytes are re-read as tags and the
        // coordinates become arbitrary.
        match wire_type {
            // 0 = varint — the only wire type touch coordinates use.
            0 => {
                let (value, new_pos) = decode_varint(data, pos)?;
                pos = new_pos;
                match field_num {
                    1 => x = value as u32,
                    2 => y = value as u32,
                    3 => pid = value as u32,
                    _ => {}
                }
            }
            // 1 = 64-bit fixed, 5 = 32-bit fixed — skip the fixed width.
            1 => pos = pos.checked_add(8).filter(|end| *end <= data.len())?,
            5 => pos = pos.checked_add(4).filter(|end| *end <= data.len())?,
            // 2 = length-delimited — skip the declared payload.
            2 => {
                let (len, new_pos) = decode_varint(data, pos)?;
                let len = usize::try_from(len).ok()?;
                pos = new_pos.checked_add(len).filter(|end| *end <= data.len())?;
            }
            // Groups (3/4) and the reserved 6/7 cannot be sized here; a tag we
            // cannot skip means alignment is lost, so bail instead of guessing.
            _ => return None,
        }
    }
    Some((x, y, pid))
}

pub(crate) fn decode_varint(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    while pos < data.len() {
        // protobuf varints are at most 10 bytes (64 bits / 7 bits per byte).
        // Reject anything longer instead of shifting past 63 bits (UB / overflow).
        if shift >= 64 {
            return None;
        }
        let byte = data[pos];
        pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, pos));
        }
        shift += 7;
    }
    None
}
