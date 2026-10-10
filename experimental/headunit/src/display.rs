//! Display window + H.264 decode pipeline.
//!
//! Owns a native minifb window (Wayland / X11 / whatever the compositor
//! provides) so pointer events come to us directly — no grabs, no hacks.
//!
//! Pipeline:
//!   bridge H.264 → [h264_rx channel]
//!     → ffmpeg stdin  (h264-feed thread)
//!     → ffmpeg stdout → BGRA frames  (decode thread)
//!     → minifb window buffer  (this thread, also handles mouse)
//!
//! Mouse → touch mapping (mirrors a touchscreen):
//!   ButtonPress   → ACTION_DOWN  (action 0)
//!   Motion+held   → ACTION_MOVE  (action 2, ≤60 Hz)
//!   ButtonRelease → ACTION_UP    (action 1)

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const WIDTH: usize = 800;
const HEIGHT: usize = 480;
const FRAME_BYTES: usize = WIDTH * HEIGHT * 4; // BGRA

/// How long a semantically required touch event (DOWN / UP) may wait for room
/// in the touch channel before it is given up on. Bounded so a stalled bridge
/// can never freeze the render loop — see `send_touch`.
const TOUCH_DELIVER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Handle to the running display thread.
///
/// `exited` resolves as soon as the window is gone — closed, Escape pressed, or
/// the pipeline failed to start (no ffmpeg, no compositor). The caller must act
/// on it: without it a failed display leaves the process consuming USB video
/// forever with nothing on screen.
pub struct DisplayHandle {
    exited: oneshot::Receiver<anyhow::Result<()>>,
    join: std::thread::JoinHandle<()>,
    /// Set by the caller to ask the window to close — used when the protocol
    /// side ends first, so joining the thread cannot block on a live window.
    stop: Arc<AtomicBool>,
}

impl DisplayHandle {
    /// Future that resolves when the display thread returns, carrying its result.
    pub fn exited_mut(&mut self) -> &mut oneshot::Receiver<anyhow::Result<()>> {
        &mut self.exited
    }

    /// Ask the window to close. The render loop checks this once per frame.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Wait for the display thread to finish (it kills and reaps ffmpeg on the
    /// way out). Call `stop()` first unless the thread is known to have ended.
    pub fn join(self) {
        let _ = self.join.join();
    }
}

/// Spawn the display thread. Returns immediately; the window runs in the background.
pub fn spawn(
    first_frame: Vec<u8>,
    h264_rx: mpsc::Receiver<Vec<u8>>,
    touch_tx: mpsc::Sender<(u32, u32, u8)>,
) -> DisplayHandle {
    let (exit_tx, exited) = oneshot::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::spawn(move || {
        let result = run(first_frame, h264_rx, touch_tx, thread_stop);
        if let Err(e) = &result {
            eprintln!("[display] error: {e:#}");
        }
        // The receiver being gone just means the session already ended.
        let _ = exit_tx.send(result);
    });
    DisplayHandle { exited, join, stop }
}

fn run(
    first_frame: Vec<u8>,
    h264_rx: mpsc::Receiver<Vec<u8>>,
    touch_tx: mpsc::Sender<(u32, u32, u8)>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};

    // ── Spawn ffmpeg: H.264 → raw BGRA ───────────────────────────────────
    // If this fails, `h264_rx` is dropped here and the protocol loop's send
    // fails, which is how the session learns the display is dead.
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-loglevel", "warning",
            "-f", "h264",
            "-i", "pipe:0",
            "-f", "rawvideo",
            "-pix_fmt", "bgra",
            "-s", &format!("{WIDTH}x{HEIGHT}"),
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    // Everything past this point must reap ffmpeg on the way out: a
    // std::process::Child is NOT killed on drop, so an early return here would
    // leak an ffmpeg process for every run.
    let result = run_window(&mut ffmpeg, first_frame, h264_rx, touch_tx, stop);
    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    result
}

fn run_window(
    ffmpeg: &mut std::process::Child,
    first_frame: Vec<u8>,
    h264_rx: mpsc::Receiver<Vec<u8>>,
    touch_tx: mpsc::Sender<(u32, u32, u8)>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use std::sync::mpsc as std_mpsc;

    let mut ffmpeg_in = ffmpeg
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin already taken"))?;
    let ffmpeg_out = ffmpeg
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stdout already taken"))?;

    // Thread: feed H.264 bytes (first frame + channel) → ffmpeg stdin.
    // Owns `h264_rx`; once ffmpeg is killed the write fails, the loop breaks
    // and the receiver drops — which is what makes the protocol loop bail.
    std::thread::spawn(move || {
        let _ = ffmpeg_in.write_all(&first_frame);
        let mut rx = h264_rx;
        loop {
            match rx.blocking_recv() {
                Some(data) => {
                    if ffmpeg_in.write_all(&data).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    // Thread: read decoded BGRA frames from ffmpeg stdout → frame channel
    let (frame_tx, frame_rx) = std_mpsc::sync_channel::<Vec<u32>>(2);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut src = ffmpeg_out;
        let mut raw = vec![0u8; FRAME_BYTES];
        loop {
            if src.read_exact(&mut raw).is_err() {
                break;
            }
            // BGRA → minifb u32 (0x00RRGGBB)
            let frame: Vec<u32> = raw
                .chunks_exact(4)
                .map(|p| ((p[2] as u32) << 16) | ((p[1] as u32) << 8) | (p[0] as u32))
                .collect();
            if frame_tx.send(frame).is_err() {
                break;
            }
        }
    });

    // ── minifb window (Wayland-native on Fedora) ──────────────────────────
    let mut window = minifb::Window::new(
        "RasCar Android Auto",
        WIDTH,
        HEIGHT,
        minifb::WindowOptions::default(),
    )?;
    // Cap the update rate so we don't spin-burn CPU between frames.
    window.set_target_fps(60);

    let mut buffer = vec![0u32; WIDTH * HEIGHT];
    let mut button_held = false;
    let mut last_drag = std::time::Instant::now();

    while window.is_open()
        && !window.is_key_down(minifb::Key::Escape)
        && !stop.load(Ordering::Relaxed)
    {
        // Consume all queued decoded frames; keep only the latest.
        while let Ok(frame) = frame_rx.try_recv() {
            buffer = frame;
        }

        // ── Mouse → touch events ─────────────────────────────────────────
        // get_mouse_pos returns pixel coords clamped to [0, width/height).
        if let Some((mx, my)) = window.get_mouse_pos(minifb::MouseMode::Clamp) {
            let vx = (mx as u32).min(WIDTH as u32 - 1);
            let vy = (my as u32).min(HEIGHT as u32 - 1);
            let btn = window.get_mouse_down(minifb::MouseButton::Left);

            if btn && !button_held {
                button_held = true;
                send_touch(&touch_tx, (vx, vy, 0)); // ACTION_DOWN
            } else if !btn && button_held {
                button_held = false;
                send_touch(&touch_tx, (vx, vy, 1)); // ACTION_UP
            } else if btn && button_held {
                let now = std::time::Instant::now();
                if now.duration_since(last_drag).as_millis() >= 16 {
                    last_drag = now;
                    send_touch(&touch_tx, (vx, vy, 2)); // ACTION_MOVE
                }
            }
        }

        window.update_with_buffer(&buffer, WIDTH, HEIGHT)?;
    }

    Ok(())
}

/// Hand a touch event to the protocol loop without ever hanging the render loop.
///
/// The touch channel is bounded and is only drained between USB reads, so a
/// quiet bridge lets it fill. A blocking send from here would then stop the
/// repaint, the Escape polling and the window-close check — the window freezes.
///
/// ACTION_MOVE is coalescable: dropping one during a drag costs a little
/// smoothness and nothing else, so it is dropped immediately when full.
/// ACTION_DOWN / ACTION_UP carry the gesture (a lost DOWN loses the whole tap),
/// so they get a short bounded retry before being given up on.
fn send_touch(tx: &mpsc::Sender<(u32, u32, u8)>, ev: (u32, u32, u8)) {
    use tokio::sync::mpsc::error::TrySendError;

    let (x, y, action) = ev;
    let coalescable = action == 2; // ACTION_MOVE
    let deadline = std::time::Instant::now() + TOUCH_DELIVER_TIMEOUT;

    loop {
        match tx.try_send(ev) {
            Ok(()) => return,
            Err(TrySendError::Closed(_)) => return, // session over
            Err(TrySendError::Full(_)) => {
                if coalescable || std::time::Instant::now() >= deadline {
                    eprintln!("[touch] dropped action={action} x={x} y={y} (channel full)");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}
