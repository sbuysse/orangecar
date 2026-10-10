//! Capture of the Wayland dashboard for the car's screen stream.
//!
//! With `RASCAR_COMPOSITOR=wayland` the dashboard runs in a headless wlroots
//! compositor (cage) that renders on the Mali GPU (see `stream::render`).
//! Nothing like `x11grab` exists there, so this client asks the compositor for
//! a full copy of its output with `wlr-screencopy` — every frame, whether the
//! screen changed or not: the car wants a steady frame rate, and a
//! damage-driven recorder (wf-recorder) sent nothing at all while the page was
//! still. Frames are written raw to ffmpeg's stdin; the encoder settings stay
//! those of the X11 path.
//!
//! Measured on the Pi, 2026-09-30, 1200x512, real dashboard:
//! - Copy target. Into `wl_shm` memory the compositor burnt 50-63 % of a core:
//!   the Mali renders tiled, and reading that into plain memory untiles on the
//!   CPU. Into a linear DMA-BUF (from `/dev/dma_heap/system`) the copy is a GPU
//!   blit: cage 5-6 %. `wl_shm` stays as the fallback.
//! - Pacing. The copy completes on the compositor's next refresh, so the
//!   output runs at exactly the video frame rate and the next copy is requested
//!   as soon as the previous one is ready (double buffering): 25 fps with
//!   Chromium drawing 25 frames/s, not 60. Whole pipeline ≈ 91 % of a core
//!   (x264 ≈ 62 %) against ≈ 96 % for Xvfb + x11grab — with the GPU in use.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as dmabuf};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager};

/// Where the compositor listens: `<runtime_dir>/<socket>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandTarget {
    pub runtime_dir: String,
    pub socket: String,
}

impl WaylandTarget {
    pub fn socket_path(&self) -> String {
        format!("{}/{}", self.runtime_dir, self.socket)
    }
}

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    dmabuf: Option<dmabuf::ZwpLinuxDmabufV1>,
    output: Option<wl_output::WlOutput>,
    manager: Option<manager::ZwlrScreencopyManagerV1>,
    shm_format: Option<(wl_shm::Format, u32, u32, u32)>,
    dmabuf_format: Option<(u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "zwp_linux_dmabuf_v1" => s.dmabuf = Some(reg.bind(name, version.min(3), qh, ())),
                // One output: the compositor's single headless output.
                "wl_output" if s.output.is_none() => s.output = Some(reg.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => s.manager = Some(reg.bind(name, version.min(3), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &frame::ZwlrScreencopyFrameV1, ev: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            frame::Event::Buffer { format: WEnum::Value(f), width, height, stride } => s.shm_format = Some((f, width, height, stride)),
            frame::Event::LinuxDmabuf { format, width, height } => s.dmabuf_format = Some((format, width, height)),
            frame::Event::BufferDone => s.buffer_done = true,
            frame::Event::Flags { flags: WEnum::Value(f) } => s.y_invert = f.contains(frame::Flags::YInvert),
            frame::Event::Ready { .. } => s.ready = true,
            frame::Event::Failed => s.failed = true,
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(impl Dispatch<$t, ()> for State {
        fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    })*};
}
ignore_events!(
    wl_shm::WlShm,
    wl_output::WlOutput,
    manager::ZwlrScreencopyManagerV1,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    dmabuf::ZwpLinuxDmabufV1,
    params::ZwpLinuxBufferParamsV1
);

/// ffmpeg `-pix_fmt` for a `wl_shm` format (little-endian names → byte order).
pub fn shm_pix_fmt(format: wl_shm::Format) -> Option<&'static str> {
    match format {
        wl_shm::Format::Xrgb8888 => Some("bgr0"),
        wl_shm::Format::Argb8888 => Some("bgra"),
        wl_shm::Format::Xbgr8888 => Some("rgb0"),
        wl_shm::Format::Abgr8888 => Some("rgba"),
        _ => None,
    }
}

/// ffmpeg `-pix_fmt` for a DRM fourcc (same little-endian convention).
pub fn drm_pix_fmt(fourcc: u32) -> Option<&'static str> {
    match &fourcc.to_le_bytes() {
        b"XR24" => Some("bgr0"),
        b"AR24" => Some("bgra"),
        b"XB24" => Some("rgb0"),
        b"AB24" => Some("rgba"),
        _ => None,
    }
}

/// A linear buffer from the kernel's system DMA heap.
fn dma_heap_alloc(len: usize) -> Result<OwnedFd> {
    #[repr(C)]
    struct Allocation {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    const DMA_HEAP_IOCTL_ALLOC: u64 = 0xC018_4800;
    let heap = std::fs::OpenOptions::new().read(true).write(true).open("/dev/dma_heap/system").context("open /dev/dma_heap/system")?;
    let mut a = Allocation { len: len as u64, fd: 0, fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32, heap_flags: 0 };
    // SAFETY: DMA_HEAP_IOCTL_ALLOC takes a pointer to `struct dma_heap_allocation_data`, laid out above.
    if unsafe { libc::ioctl(heap.as_raw_fd(), DMA_HEAP_IOCTL_ALLOC as _, &mut a as *mut Allocation) } < 0 {
        bail!("DMA_HEAP_IOCTL_ALLOC: {}", std::io::Error::last_os_error());
    }
    // SAFETY: the ioctl returned a new fd that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(a.fd as i32) })
}

/// `DMA_BUF_IOCTL_SYNC` around CPU reads of a DMA-BUF (cache coherency).
fn dmabuf_sync(fd: &OwnedFd, end: bool) {
    const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
    const SYNC_READ: u64 = 1;
    const SYNC_END: u64 = 4;
    let flags = if end { SYNC_READ | SYNC_END } else { SYNC_READ };
    // SAFETY: DMA_BUF_IOCTL_SYNC takes a pointer to a u64 of flags.
    unsafe { libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC as _, &flags as *const u64) };
}

struct Buffer {
    wl: wl_buffer::WlBuffer,
    map: memmap2::MmapMut,
    /// Some for a DMA-BUF (needs sync around CPU reads).
    dmabuf: Option<OwnedFd>,
}

/// A connected capturer. [`Capturer::connect`] grabs the first frame so the
/// caller learns the geometry and pixel format before starting ffmpeg.
pub struct Capturer {
    queue: EventQueue<State>,
    state: State,
    buffers: Vec<Buffer>,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: &'static str,
    stride: u32,
    /// Human-readable copy path, for the log.
    pub path: &'static str,
}

impl Capturer {
    pub fn connect(target: &WaylandTarget) -> Result<Self> {
        // Connect to the explicit socket: the process environment is not ours
        // to set (multi-threaded runtime), so no WAYLAND_DISPLAY lookup.
        let stream = UnixStream::connect(target.socket_path())
            .with_context(|| format!("connect to the compositor at {}", target.socket_path()))?;
        let conn = Connection::from_socket(stream).context("Wayland handshake")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).context("Wayland registry roundtrip")?;
        if state.shm.is_none() || state.output.is_none() || state.manager.is_none() {
            bail!("the compositor offers no wl_shm / wl_output / zwlr_screencopy_manager_v1");
        }
        let mut c = Self { queue, state, buffers: Vec::new(), width: 0, height: 0, pix_fmt: "", stride: 0, path: "" };
        let first = c.request()?;
        c.allocate()?;
        c.copy_into(first, 0)?;
        c.wait_ready()?;
        info!("wlcapture: {}x{} {} (stride {}) via {} from {}", c.width, c.height, c.pix_fmt, c.stride, c.path, target.socket_path());
        Ok(c)
    }

    /// Ask for the next frame and wait for the compositor to describe it.
    fn request(&mut self) -> Result<frame::ZwlrScreencopyFrameV1> {
        let qh = self.queue.handle();
        let s = &mut self.state;
        s.shm_format = None;
        s.dmabuf_format = None;
        s.buffer_done = false;
        s.ready = false;
        s.failed = false;
        let (manager, output) = (s.manager.clone().expect("checked"), s.output.clone().expect("checked"));
        let f = manager.capture_output(0, &output, &qh, ());
        // v3 lists every buffer type, then buffer_done; older versions send only shm.
        while !(self.state.buffer_done || (self.state.shm_format.is_some() && manager.version() < 3)) {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        Ok(f)
    }

    /// Two buffers of the offered geometry: DMA-BUF if possible, else shm.
    fn allocate(&mut self) -> Result<()> {
        if let Some((fourcc, w, h)) = self.state.dmabuf_format {
            match (drm_pix_fmt(fourcc), self.state.dmabuf.clone()) {
                (Some(pix_fmt), Some(dmabuf)) => match self.allocate_dmabuf(&dmabuf, fourcc, w, h) {
                    Ok(buffers) => {
                        (self.buffers, self.width, self.height, self.stride, self.pix_fmt, self.path) =
                            (buffers, w, h, w * 4, pix_fmt, "DMA-BUF");
                        return Ok(());
                    }
                    Err(e) => warn!("wlcapture: DMA-BUF unavailable ({e:#}) — falling back to wl_shm (costs CPU)"),
                },
                _ => warn!("wlcapture: DMA-BUF format {fourcc:#x} not handled — falling back to wl_shm (costs CPU)"),
            }
        }
        let (format, w, h, stride) = self.state.shm_format.context("no shm buffer offered")?;
        let pix_fmt = shm_pix_fmt(format).with_context(|| format!("unsupported shm format {format:?}"))?;
        let qh = self.queue.handle();
        let shm = self.state.shm.clone().expect("checked");
        let size = (stride * h) as usize;
        let mut buffers = Vec::new();
        for _ in 0..2 {
            // SAFETY: memfd_create with a static NUL-terminated name.
            let raw = unsafe { libc::memfd_create(c"rascar-wlcapture".as_ptr(), libc::MFD_CLOEXEC) };
            if raw < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh fd we own.
            let file = unsafe { std::fs::File::from_raw_fd(raw) };
            file.set_len(size as u64)?;
            // SAFETY: the memfd is private to us and sized above.
            let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
            let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
            let wl = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());
            pool.destroy();
            buffers.push(Buffer { wl, map, dmabuf: None });
        }
        (self.buffers, self.width, self.height, self.stride, self.pix_fmt, self.path) = (buffers, w, h, stride, pix_fmt, "wl_shm");
        Ok(())
    }

    fn allocate_dmabuf(&self, dmabuf: &dmabuf::ZwpLinuxDmabufV1, fourcc: u32, w: u32, h: u32) -> Result<Vec<Buffer>> {
        let qh = self.queue.handle();
        let stride = w * 4;
        let mut buffers = Vec::new();
        for _ in 0..2 {
            let fd = dma_heap_alloc((stride * h) as usize)?;
            // SAFETY: a DMA-BUF of exactly this size, mapped for reading only by us.
            let map = unsafe { memmap2::MmapMut::map_mut(fd.as_raw_fd())? };
            let p = dmabuf.create_params(&qh, ());
            // Plane 0, offset 0, LINEAR modifier (0): the compositor blits into it.
            p.add(fd.as_fd(), 0, 0, stride, 0, 0);
            let wl = p.create_immed(w as i32, h as i32, fourcc, params::Flags::empty(), &qh, ());
            p.destroy();
            buffers.push(Buffer { wl, map, dmabuf: Some(fd) });
        }
        Ok(buffers)
    }

    fn copy_into(&mut self, f: frame::ZwlrScreencopyFrameV1, i: usize) -> Result<frame::ZwlrScreencopyFrameV1> {
        f.copy(&self.buffers[i].wl);
        Ok(f)
    }

    fn wait_ready(&mut self) -> Result<()> {
        while !(self.state.ready || self.state.failed) {
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        if self.state.failed {
            bail!("the compositor failed the screencopy");
        }
        Ok(())
    }

    /// Write buffer `i`, packed (no stride padding), top row first.
    fn write_frame(&self, i: usize, y_invert: bool, out: &mut impl Write) -> std::io::Result<()> {
        let b = &self.buffers[i];
        if let Some(fd) = &b.dmabuf {
            dmabuf_sync(fd, false);
        }
        let row = (self.width * 4) as usize;
        let stride = self.stride as usize;
        let h = self.height as usize;
        let r = if !y_invert && stride == row {
            out.write_all(&b.map[..row * h])
        } else {
            (0..h).try_for_each(|y| {
                let src = if y_invert { h - 1 - y } else { y };
                out.write_all(&b.map[src * stride..src * stride + row])
            })
        };
        if let Some(fd) = &b.dmabuf {
            dmabuf_sync(fd, true);
        }
        r
    }

    /// Feed frames to `out` until writing fails (ffmpeg gone) or the
    /// compositor does. Paced by the compositor's refresh (see module doc):
    /// the next copy is requested before the current frame is written out.
    /// Blocking: run it on its own thread.
    pub fn run(mut self, mut out: impl Write) {
        let (mut cur, mut frames) = (0usize, 0u64);
        let (mut window_start, mut window_frames) = (Instant::now(), 0u64);
        loop {
            let y_invert = self.state.y_invert;
            let next = 1 - cur;
            let pending = match self.request().and_then(|f| self.copy_into(f, next)) {
                Ok(f) => f,
                Err(e) => {
                    warn!("wlcapture: {e:#} after {frames} frames — stopping (the encoder sees EOF)");
                    return;
                }
            };
            if let Err(e) = self.write_frame(cur, y_invert, &mut out) {
                info!("wlcapture: encoder input closed ({e}) after {frames} frames — stopping");
                return;
            }
            let ready = self.wait_ready();
            pending.destroy();
            if let Err(e) = ready {
                warn!("wlcapture: {e:#} after {frames} frames — stopping (the encoder sees EOF)");
                return;
            }
            frames += 1;
            window_frames += 1;
            cur = next;
            if window_start.elapsed() >= Duration::from_secs(60) {
                info!("wlcapture: {:.1} fps over the last minute ({frames} frames)", window_frames as f64 / window_start.elapsed().as_secs_f64());
                (window_start, window_frames) = (Instant::now(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_formats_map_to_the_ffmpeg_byte_order() {
        // wl_shm names are little-endian words: XRGB8888 is B,G,R,X in memory.
        assert_eq!(shm_pix_fmt(wl_shm::Format::Xrgb8888), Some("bgr0"));
        // What cage offers in shm on the Pi's Mali (measured): XBGR8888 = R,G,B,X.
        assert_eq!(shm_pix_fmt(wl_shm::Format::Xbgr8888), Some("rgb0"));
        assert_eq!(shm_pix_fmt(wl_shm::Format::Rgb565), None);
    }

    #[test]
    fn drm_fourccs_map_to_the_ffmpeg_byte_order() {
        // What cage offers as DMA-BUF on the Pi (measured): "XR24".
        assert_eq!(drm_pix_fmt(u32::from_le_bytes(*b"XR24")), Some("bgr0"));
        assert_eq!(drm_pix_fmt(u32::from_le_bytes(*b"XB24")), Some("rgb0"));
        assert_eq!(drm_pix_fmt(u32::from_le_bytes(*b"NV12")), None);
    }
}
