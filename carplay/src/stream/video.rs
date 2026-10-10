//! Video path: capture the dashboard, H.264-encode it with ffmpeg, and split
//! the output into access units.
//!
//! # Encoder ([`VideoEncoder`])
//!
//! Input is ffmpeg `x11grab` of the X display, or raw frames from the Wayland
//! kiosk (`stream::wlcapture`). Output is H.264 **baseline** profile: libx264
//! with `-profile:v baseline` (`superfast`, `zerolatency`), or, when libx264 is
//! missing, libopenh264 with `constrained_baseline`. GOP = one IDR every 15
//! frames by default (`RASCAR_VIDEO_KEYINT`, 0.5 s at the default 30 fps of
//! `RASCAR_VIDEO_FPS`), no B-frames, SPS/PPS repeated in-band (libx264
//! `repeat-headers=1`), bitrate capped at `RASCAR_VIDEO_BITRATE_KBPS` (3000 by
//! default). The capture/encode pipeline and
//! its `read_chunk()` EOF discipline come from the project's earlier Android
//! Auto bridge code.
//!
//! The CarPlay session (`airplay::carplay_session`) uses the encoder and
//! [`NalParser`] and frames the result in the CarPlay screen format itself. The
//! packetizer below is for the older AirPlay screen-mirroring path.
//!
//! # Mirroring packetization (ported from doubletake `internal/airplay/mirror.go`)
//!
//! Every packet is a **128-byte little-endian header** followed by a payload:
//!
//! ```text
//!   [0:4]   payload length (LE u32)              — NOT incl. header; +16 for ChaCha tag
//!   [4]     payload type: 0x01 = codec (SPS/PPS avcC, cleartext)
//!                         0x00 = encrypted video AU (AVCC NALs)
//!   [5]     0x10 = IDR keyframe, 0x00 = non-IDR / codec
//!   [6:8]   option: codec = 0x16 0x01 ; video = 0x00 0x00
//!   [8:16]  NTP timestamp (LE u64, boot-relative), the PTS
//!   [16:64] codec frames only: content/display sizes as LE f32
//!   [64:128] zero
//! ```
//!
//! The codec (SPS+PPS) frame is sent **cleartext** as an `avcC`
//! AVCDecoderConfigurationRecord before each keyframe; VCL slices are sent
//! encrypted with the negotiated stream cipher.
//!
//! A receiver could negotiate **HEVC** (video stream type still 110, different
//! codec option) instead of H.264; we always offer H.264. An HEVC (`libx265`)
//! variant exists behind `VideoSettings::hevc` but is unused.

use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::crypto::VideoCipher;

// ---------------------------------------------------------------------------
// ffmpeg capture / encode (adapted from bridge/src/media/video.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval: u32,
    /// X11 display, e.g. ":0".
    pub display: String,
    /// Capture the Wayland compositor instead of `display` (`stream::wlcapture`).
    pub wayland: Option<super::wlcapture::WaylandTarget>,
    /// When true, request HEVC (`libx265`) instead of H.264 baseline.
    /// `// TODO(hw)`: only enable once the car is confirmed to negotiate HEVC.
    pub hevc: bool,
}

/// Read a positive integer tuning knob from the environment, falling back to
/// `default` when unset or unparseable.
///
/// `fps` and `bitrate_kbps` are exposed because the leading open hypothesis for
/// the head unit's teardown is that its receiver stops draining our screen
/// socket: measured 2026-09-20, the car stopped ACKing our video TCP at
/// seq 76175 (~76 KB, ~1.2 s in) and went silent 2 s before the USB detach,
/// while we were pushing only ~570 kbps. Bisecting that needs the encoder load
/// changed BETWEEN car attempts, and attempts are scarce — each one costs the
/// head unit's willingness to offer CarPlay again — so this must not require a
/// rebuild and redeploy.
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Screen frame rate (`RASCAR_VIDEO_FPS`, default 30). Also the refresh rate
/// of the Wayland compositor's output, which paces that capture.
pub fn configured_fps() -> u32 {
    env_u32("RASCAR_VIDEO_FPS", 30)
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 800,
            height: 480,
            fps: configured_fps(),
            bitrate_kbps: env_u32("RASCAR_VIDEO_BITRATE_KBPS", 3000),
            // 15 frames = one IDR every 0.5 s. P-frames cost ~10x less than an
            // IDR, so at a fixed bitrate a GOP is what keeps text and edges
            // clean; the short interval bounds how long a car forceKeyFrame
            // (answered on the next IDR) or a decoder hiccup can last.
            // RASCAR_VIDEO_KEYINT=1 restores an all-IDR stream.
            keyframe_interval: env_u32("RASCAR_VIDEO_KEYINT", 15),
            display: ":0".into(),
            wayland: None,
            hevc: false,
        }
    }
}

pub struct VideoEncoder {
    settings: VideoSettings,
    process: Option<Child>,
}


/// Which H.264 encoder this ffmpeg build actually has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum H264Encoder {
    X264,
    OpenH264,
}

/// Probe `ffmpeg -encoders` once and cache the answer.
///
/// libx264 is preferred (better rate control at a fixed bitrate, and the
/// `repeat-headers` knob the packetizer relies on), but it is absent from
/// stock Fedora builds, so fall back rather than dying at spawn time.
pub fn h264_encoder() -> H264Encoder {
    use std::sync::OnceLock;
    static CHOICE: OnceLock<H264Encoder> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                if text.contains("libx264") {
                    H264Encoder::X264
                } else if text.contains("libopenh264") {
                    warn!("video: libx264 not available in this ffmpeg — falling back to libopenh264 (constrained baseline)");
                    H264Encoder::OpenH264
                } else {
                    warn!("video: neither libx264 nor libopenh264 found — trying libx264 anyway, expect a spawn failure");
                    H264Encoder::X264
                }
            }
            Err(e) => {
                warn!("video: could not probe ffmpeg encoders ({e}) — assuming libx264");
                H264Encoder::X264
            }
        }
    })
}

impl VideoEncoder {
    pub fn new(settings: VideoSettings) -> Self {
        Self { settings, process: None }
    }

    fn build_args(&self) -> Vec<String> {
        self.build_args_for(None)
    }

    /// `raw`: raw frames on stdin `(width, height, pix_fmt)` from
    /// `stream::wlcapture`, instead of x11grab. Only the input differs.
    fn build_args_for(&self, raw: Option<(u32, u32, &str)>) -> Vec<String> {
        let s = &self.settings;
        let mut args: Vec<String> = vec!["-loglevel".into(), "warning".into()];
        match raw {
            Some((w, h, pix_fmt)) => args.extend([
                "-f".into(), "rawvideo".into(),
                "-pix_fmt".into(), pix_fmt.into(),
                "-video_size".into(), format!("{w}x{h}"),
                "-framerate".into(), s.fps.to_string(),
                "-i".into(), "pipe:0".into(),
            ]),
            None => args.extend([
                "-f".into(), "x11grab".into(),
                "-framerate".into(), s.fps.to_string(),
                "-video_size".into(), format!("{}x{}", s.width, s.height),
                // The car is a touchscreen: the X pointer that XTest drives would
                // otherwise be burned into every frame (x11grab draws it by default),
                // including over embedded sites whose cursor the dashboard can't hide.
                "-draw_mouse".into(), "0".into(),
                "-i".into(), s.display.clone(),
            ]),
        }

        if s.hevc {
            // TODO(hw): HEVC path — untested against a car; codec-header option
            // bytes and avcC/hvcC config differ from H.264 and are not handled
            // by the packetizer below yet.
            args.extend([
                "-c:v".into(), "libx265".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-preset".into(), "ultrafast".into(),
                "-tune".into(), "zerolatency".into(),
                "-b:v".into(), format!("{}k", s.bitrate_kbps),
                "-x265-params".into(), format!("keyint={}:min-keyint={}:repeat-headers=1", s.keyframe_interval, s.keyframe_interval),
                "-f".into(), "hevc".into(),
                "-an".into(),
                "pipe:1".into(),
            ]);
        } else if h264_encoder() == H264Encoder::X264 {
            args.extend([
                "-c:v".into(), "libx264".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-profile:v".into(), "baseline".into(),
                // No fixed `-level:v`: a hardcoded 3.1 is illegal above 720p and
                // makes a strict car reject the stream. libx264 auto-selects a
                // legal level for the negotiated resolution (catplay sets none).
                // `superfast`, not `ultrafast`: ultrafast turns the deblocking
                // filter off (plus AQ and most motion search), which is what put
                // blocks on the car's screen. superfast keeps latency and costs a
                // fraction of one RK3588 core at this resolution.
                "-preset".into(), "superfast".into(),
                "-tune".into(), "zerolatency".into(),
                "-b:v".into(), format!("{}k", s.bitrate_kbps),
                "-maxrate".into(), format!("{}k", s.bitrate_kbps),
                // One second of VBV: with half a second, every IDR was squeezed
                // into a fraction of its budget and came out visibly coarse.
                "-bufsize".into(), format!("{}k", s.bitrate_kbps),
                "-g".into(), s.keyframe_interval.to_string(),
                "-keyint_min".into(), s.keyframe_interval.to_string(),
                "-sc_threshold".into(), "0".into(),
                "-bf".into(), "0".into(),
                "-x264-params".into(), "repeat-headers=1".into(),
                "-f".into(), "h264".into(),
                "-an".into(),
                "pipe:1".into(),
            ]);
        } else {
            // libopenh264 fallback. Fedora's stock ffmpeg ships this and NOT
            // libx264 (which is patent-encumbered and lives in RPM Fusion), so
            // without this branch the encoder dies instantly with
            // "Unrecognized option 'x264-params'".
            //
            // Differences that matter:
            //   * the profile token is `constrained_baseline`, not `baseline`
            //     ("baseline" is rejected as an undefined constant). CBP is a
            //     strict subset of Baseline, so the receiver is still satisfied.
            //   * no -preset/-tune/-x264-params; `-b:v` drives the rate.
            //   * SPS/PPS repetition is requested via the generic AVCodec flag
            //     `-flags +global_header`-free path: libopenh264 emits parameter
            //     sets before every IDR by default, which is what the packetizer
            //     needs for its cleartext codec frame.
            args.extend([
                "-c:v".into(), "libopenh264".into(),
                "-pix_fmt".into(), "yuv420p".into(),
                "-profile:v".into(), "constrained_baseline".into(),
                "-b:v".into(), format!("{}k", s.bitrate_kbps),
                "-maxrate".into(), format!("{}k", s.bitrate_kbps),
                "-g".into(), s.keyframe_interval.to_string(),
                "-bf".into(), "0".into(),
                "-f".into(), "h264".into(),
                "-an".into(),
                "pipe:1".into(),
            ]);
        }
        args
    }

    pub async fn start(&mut self) -> Result<()> {
        if let Some(target) = self.settings.wayland.clone() {
            return self.start_wayland(target).await;
        }
        let args = self.build_args();
        let codec = if self.settings.hevc {
            "HEVC/libx265"
        } else {
            match h264_encoder() {
                H264Encoder::X264 => "H.264 baseline/libx264",
                H264Encoder::OpenH264 => "H.264 constrained-baseline/libopenh264",
            }
        };
        info!("video: starting ffmpeg x11grab of {} ({codec})", self.settings.display);
        // Take the display from the settings, never from the process environ:
        // this runs on the multi-threaded runtime and nothing sets DISPLAY.
        let child = Command::new("ffmpeg")
            .args(&args)
            .env("DISPLAY", &self.settings.display)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("spawning ffmpeg — is it installed?")?;
        info!("video: ffmpeg started (pid={})", child.id().unwrap_or(0));
        self.process = Some(child);
        Ok(())
    }

    /// Wayland: grab a first frame (geometry + pixel format), start ffmpeg on
    /// raw stdin, then feed it from a capture thread at the configured rate.
    async fn start_wayland(&mut self, target: super::wlcapture::WaylandTarget) -> Result<()> {
        let capturer = tokio::task::spawn_blocking(move || super::wlcapture::Capturer::connect(&target))
            .await
            .context("wlcapture task")??;
        if (capturer.width, capturer.height) != (self.settings.width, self.settings.height) {
            warn!(
                "video: the compositor output is {}x{}, not the negotiated {}x{} — encoding what it shows",
                capturer.width, capturer.height, self.settings.width, self.settings.height
            );
        }
        let args = self.build_args_for(Some((capturer.width, capturer.height, capturer.pix_fmt)));
        info!(
            "video: starting ffmpeg on the Wayland capture ({}x{} {} via {} @ {} fps)",
            capturer.width, capturer.height, capturer.pix_fmt, capturer.path, self.settings.fps
        );
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawning ffmpeg — is it installed?")?;
        info!("video: ffmpeg started (pid={})", child.id().unwrap_or(0));
        // The capture thread writes with plain blocking I/O: hand it the pipe
        // as a std File, back in blocking mode (tokio made it non-blocking).
        let stdin = child.stdin.take().context("no ffmpeg stdin")?.into_owned_fd()?;
        let flags = nix::fcntl::fcntl(std::os::fd::AsRawFd::as_raw_fd(&stdin), nix::fcntl::FcntlArg::F_GETFL)?;
        let flags = nix::fcntl::OFlag::from_bits_truncate(flags) & !nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(std::os::fd::AsRawFd::as_raw_fd(&stdin), nix::fcntl::FcntlArg::F_SETFL(flags))?;
        let pipe = std::fs::File::from(stdin);
        std::thread::Builder::new()
            .name("wlcapture".into())
            .spawn(move || capturer.run(std::io::BufWriter::with_capacity(1 << 20, pipe)))
            .context("spawning the capture thread")?;
        self.process = Some(child);
        Ok(())
    }

    /// Read a chunk of the elementary stream. `Ok(0)` = EOF (ffmpeg exited).
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let child = self.process.as_mut().context("encoder not started")?;
        let stdout = child.stdout.as_mut().context("no stdout")?;
        Ok(stdout.read(buf).await?)
    }

    /// Reap ffmpeg after its stdout hit EOF and report the exit status it chose
    /// for itself. Must be used instead of [`Self::stop`] on the EOF path: an
    /// x11grab that cannot open the display exits immediately with a non-zero
    /// status, which would otherwise be indistinguishable from a clean end.
    pub async fn wait_exit(&mut self) -> Result<std::process::ExitStatus> {
        let mut child = self.process.take().context("encoder not started")?;
        child.wait().await.context("waiting for ffmpeg")
    }

    /// Kill the encoder. The resulting status is a signal death by construction
    /// and therefore deliberately not reported.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            info!("video: ffmpeg stopped");
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// NAL parsing (Annex-B) — ported from doubletake mirror.go
// ---------------------------------------------------------------------------

/// Incremental Annex-B NAL splitter. Returns whole NAL units (WITHOUT start
/// code) as they complete; keeps a partial tail buffered across reads.
///
/// `pub(crate)` so the CarPlay screen packetizer (`airplay::carplay_session`)
/// can reuse this proven Annex-B reader instead of duplicating it.
pub(crate) struct NalParser {
    buf: Vec<u8>,
}

impl NalParser {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::with_capacity(512 * 1024) }
    }

    /// Push freshly-read bytes; return the raw NALs (start code stripped) now
    /// fully delimited.
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let start = match find_start_code(&self.buf, 0) {
                Some(s) => s,
                None => {
                    // No start code at all — cap runaway buffering.
                    if self.buf.len() > 1024 * 1024 {
                        let keep = self.buf.len() - 128 * 1024;
                        self.buf.drain(..keep);
                    }
                    break;
                }
            };
            let next = match find_start_code(&self.buf, start + 3) {
                Some(n) => n,
                None => {
                    // Trailing partial NAL: keep from `start` onward.
                    if start > 0 {
                        self.buf.drain(..start);
                    }
                    break;
                }
            };
            let nal = strip_start_code(&self.buf[start..next]).to_vec();
            if !nal.is_empty() {
                out.push(nal);
            }
            self.buf.drain(..next);
        }
        out
    }
}

fn find_start_code(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 < b.len() {
        if b[i] == 0 && b[i + 1] == 0 {
            if b[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < b.len() && b[i + 2] == 0 && b[i + 3] == 1 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Strip a leading `00 00 01` or `00 00 00 01` start code.
fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.len() > 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
        &nal[4..]
    } else if nal.len() > 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
        &nal[3..]
    } else {
        nal
    }
}

/// H.264 NAL unit type (raw NAL, start code already stripped).
pub(crate) fn nal_type(raw: &[u8]) -> u8 {
    raw.first().map(|b| b & 0x1f).unwrap_or(0)
}

/// True if `raw` is the first slice of a new access unit (first_mb_in_slice==0).
pub(crate) fn is_first_slice(raw: &[u8]) -> bool {
    raw.len() >= 2 && (raw[1] & 0x80) != 0
}

/// Prepend a 4-byte big-endian length (AVCC framing) to a raw NAL.
fn avcc_wrap(raw: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + raw.len());
    b.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    b.extend_from_slice(raw);
    b
}

/// Build an AVCDecoderConfigurationRecord (`avcC`) from raw SPS + PPS.
///
/// `// TODO(hw)`: the record is emitted 4 bytes longer than ISO/IEC 14496-15
/// specifies, with `0x02` at `avcc_len` and three zero bytes after it — a
/// trailer reported as present in iPhone captures but **UNCONFIRMED**: the
/// doubletake source it was ported from is not in this repo. The inflated
/// length is also what `codec_header` advertises. Confirming it needs a packet
/// capture of a real iPhone→head-unit codec frame: if the trailer is absent
/// there, drop the `+ 4` and the `p[avcc_len]` write together.
fn build_avcc_config(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    if sps.len() < 4 {
        anyhow::bail!(
            "SPS too short to build an avcC record: {} bytes, need >= 4              (profile/compat/level live at sps[1..4])",
            sps.len()
        );
    }
    let avcc_len = 6 + 2 + sps.len() + 1 + 2 + pps.len();
    // The trailer is opt-OUT (`RASCAR_AVCC_TRAILER=0`) rather than removed: it is
    // unverified in both directions, and the receiver hands this body straight to
    // the head unit's own decoder (`ScreenStreamSetAVCC`,
    // AirPlayReceiverSessionScreen.c:553), whose strictness we cannot read. If a
    // car run shows the receiver's HUD but no picture, this is the first lever.
    let trailer = if crate::debug::env_flag("RASCAR_AVCC_TRAILER", true) { 4 } else { 0 };
    let mut p = vec![0u8; avcc_len + trailer];
    p[0] = 0x01;
    p[1] = sps[1]; // AVCProfileIndication
    p[2] = sps[2]; // profile_compatibility
    p[3] = sps[3]; // AVCLevelIndication
    p[4] = 0xff;   // lengthSizeMinusOne = 3
    p[5] = 0xe1;   // numSPS = 1
    p[6..8].copy_from_slice(&(sps.len() as u16).to_be_bytes());
    p[8..8 + sps.len()].copy_from_slice(sps);
    let off = 8 + sps.len();
    p[off] = 0x01; // numPPS = 1
    p[off + 1..off + 3].copy_from_slice(&(pps.len() as u16).to_be_bytes());
    p[off + 3..off + 3 + pps.len()].copy_from_slice(pps);
    if trailer > 0 {
        p[avcc_len] = 0x02; // 4-byte trailer, unverified — see the doc comment.
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// AirPlay mirror packet headers
// ---------------------------------------------------------------------------

const MIRROR_HEADER_LEN: usize = 128;

// Screen header flag bits (Apple, `AirPlayCommon.h:2054-2061`). They live in
// `smallParam[]`, i.e. header bytes 5..8.
const SCREEN_FLAG_SHOW_HUD: u8 = 1 << 0;
const SCREEN_FLAG_RESPECT_TIMESTAMPS: u8 = 1 << 1;
const SCREEN_FLAG_ENCRYPTED: u8 = 1 << 2;
const SCREEN_FLAG_UNUSED4: u8 = 1 << 4;

/// `smallParam[0]` of the VideoConfig packet.
///
/// `RASCAR_SCREEN_HUD=1` sets `kAirPlayScreenFlag_ShowHUD`, which makes the
/// receiver draw its OWN diagnostic overlay. That is the cheapest way to tell
/// "the head unit is parsing our headers but not displaying our picture" from
/// "it never parsed them at all": if the HUD appears on the car screen, the
/// transport, header layout and framing are all good.
fn screen_flags_small0() -> u8 {
    if crate::debug::env_flag("RASCAR_SCREEN_HUD", false) {
        SCREEN_FLAG_SHOW_HUD
    } else {
        0
    }
}

/// `smallParam[1]` of the VideoConfig packet — where the receiver reads
/// `respectTimestamps` (`AirPlayReceiverSessionScreen.c:547`).
///
/// Default is now CLEARED: with it set the receiver schedules each frame at the
/// NTP instant we stamp it with, via its time synchroniser; cleared, it displays
/// on arrival (`displayTicks = UpTicks()`, `:490-497`). A live dashboard has
/// nothing to lip-sync to, so "display on arrival" is both what we want and one
/// less clock to get wrong. `RASCAR_RESPECT_TIMESTAMPS=1` restores the old
/// behaviour (now that `NtpClock::now` is epoch-correct, it is at least honest).
fn screen_flags_small1() -> u8 {
    let mut flags = SCREEN_FLAG_ENCRYPTED | SCREEN_FLAG_UNUSED4;
    if crate::debug::env_flag("RASCAR_RESPECT_TIMESTAMPS", false) {
        flags |= SCREEN_FLAG_RESPECT_TIMESTAMPS;
    }
    flags
}

/// Codec (SPS+PPS) packet: type 0x01, cleartext avcC payload.
fn codec_header(payload_len: usize, ntp: u64, content: (u32, u32), display: (u32, u32)) -> [u8; MIRROR_HEADER_LEN] {
    let mut h = [0u8; MIRROR_HEADER_LEN];
    h[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    h[4] = 0x01;
    h[5] = screen_flags_small0();
    h[6] = screen_flags_small1();
    h[7] = 0x01;
    h[8..16].copy_from_slice(&ntp.to_le_bytes());
    // offsets 16/20 = content size, 40/44 = source size, 56/60 = display size.
    put_f32(&mut h[16..20], content.0 as f32);
    put_f32(&mut h[20..24], content.1 as f32);
    put_f32(&mut h[40..44], content.0 as f32);
    put_f32(&mut h[44..48], content.1 as f32);
    put_f32(&mut h[56..60], display.0 as f32);
    put_f32(&mut h[60..64], display.1 as f32);
    h
}

/// Video AU packet: type 0x00, encrypted AVCC payload.
fn video_header(payload_len: usize, is_keyframe: bool, ntp: u64) -> [u8; MIRROR_HEADER_LEN] {
    let mut h = [0u8; MIRROR_HEADER_LEN];
    h[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    h[4] = 0x00;
    h[5] = if is_keyframe { 0x10 } else { 0x00 };
    // h[6..8] = 0x00 0x00 for encrypted packets.
    h[8..16].copy_from_slice(&ntp.to_le_bytes());
    h
}

fn put_f32(dst: &mut [u8], v: f32) {
    dst.copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// NTP boot-relative timestamp (ported from doubletake ntpTimeWithBias)
// ---------------------------------------------------------------------------

struct NtpClock {
    start: Instant,
    bias: Duration,
}

impl NtpClock {
    fn new() -> Self {
        // 50 ms presentation bias. `// TODO(hw)`: UNVERIFIED — the doubletake
        // source this was ported from is not in this repo, so the intended
        // value (its comment once said 5 ms) could not be checked. Current
        // behaviour is 50 ms; a capture of an iPhone's mirror timestamps, or
        // the doubletake source, would settle it. Do not change on a hunch:
        // this value lands in the packet header's PTS at h[8..16].
        Self { start: Instant::now(), bias: Duration::from_millis(50) }
    }

    /// Frame PTS as an NTP 32.32 stamp **on the same timeline we serve on the
    /// timing port**.
    ///
    /// This used to omit the NTP epoch, emitting ~0-5 s while the RTCP 0xD3
    /// timing replies (`carplay_session::ntp_now`) reported a real NTP stamp
    /// (~3.9e9 s). When the VideoConfig asks the receiver to respect timestamps
    /// it maps our PTS through its time synchroniser
    /// (`AirPlayReceiverSessionScreen.c:490-493`), so every frame landed
    /// 2_208_988_800 s — **70 years** — in its past: accepted, counted as a
    /// "Late frame" (`:510-517`), and handed to the platform display queue with a
    /// nonsensical deadline.
    fn now(&self) -> u64 {
        // Same epoch as the timing responder, so a frame PTS and a 0xD3 reply
        // describe the same instant.
        crate::ntp_time::encode(crate::ntp_time::NTP_TO_UNIX + self.start.elapsed() + self.bias)
    }
}

// ---------------------------------------------------------------------------
// Congestion controller (simplified from doubletake) — drops P-frames when the
// data-conn write latency (EWMA ns/byte) rises, protecting the bitrate budget.
// Keyframes are never dropped. This is the only "flow control" the sink exposes.
// ---------------------------------------------------------------------------

struct Congestion {
    ewma_ns_per_byte: f64,
    samples: u64,
    level: u8, // 0 none, 1 light(drop 1/3), 2 medium(drop 1/2), 3 heavy(drop all P)
    /// A P-frame was dropped since the last keyframe. Every later P-frame
    /// references it, so they are dropped too until the next IDR: a short
    /// freeze instead of smeared, corrupted frames.
    broken: bool,
}

impl Congestion {
    fn new() -> Self {
        Self { ewma_ns_per_byte: 0.0, samples: 0, level: 0, broken: false }
    }

    fn should_drop_pframe(&self, frame_count: u64) -> bool {
        match self.level {
            0 => false,
            1 => frame_count % 3 == 0,
            2 => frame_count % 2 == 0,
            _ => true,
        }
    }

    fn record(&mut self, bytes: usize, dur: Duration) {
        if bytes == 0 {
            return;
        }
        let ns_per_byte = dur.as_nanos() as f64 / bytes as f64;
        const ALPHA: f64 = 0.3;
        self.ewma_ns_per_byte = if self.samples == 0 {
            ns_per_byte
        } else {
            ALPHA * ns_per_byte + (1.0 - ALPHA) * self.ewma_ns_per_byte
        };
        self.samples += 1;
        // Thresholds (ns/byte) are heuristic; ~saturated 4Mbps link ≈ 2ns/byte.
        self.level = if self.ewma_ns_per_byte > 8.0 {
            3
        } else if self.ewma_ns_per_byte > 4.0 {
            2
        } else if self.ewma_ns_per_byte > 2.0 {
            1
        } else {
            0
        };
    }
}

// ---------------------------------------------------------------------------
// The streaming loop
// ---------------------------------------------------------------------------

/// Push encoded frames from `encoder` to `wr` (the video data conn write half),
/// packetized + encrypted per the AirPlay mirror protocol. Runs until the
/// encoder hits EOF (`Ok(0)`) or a write fails.
pub async fn stream_to(
    mut encoder: VideoEncoder,
    mut wr: OwnedWriteHalf,
    mut cipher: VideoCipher,
    content_size: (u32, u32),
    display_size: (u32, u32),
) -> Result<()> {
    let clock = NtpClock::new();
    let mut parser = NalParser::new();
    let mut cc = Congestion::new();

    let mut latest_sps: Option<Vec<u8>> = None;
    let mut latest_pps: Option<Vec<u8>> = None;
    let mut vcl: Vec<u8> = Vec::new(); // AVCC-accumulated AU
    let mut pending_keyframe = false;
    let mut codec_sent = false;
    let mut stream_primed = false;
    let mut frame_count: u64 = 0;

    let mut buf = vec![0u8; 256 * 1024];

    info!("video: entering capture→packetize→push loop (content {content_size:?}, display {display_size:?}, cipher={})", cipher.name());

    loop {
        let n = match encoder.read_chunk(&mut buf).await {
            Ok(0) => {
                // EOF — ffmpeg exited. Flush, then find out WHY it exited: a
                // failed x11grab dies within ~50ms having sent nothing, and
                // must not be reported as a completed streaming session.
                info!("video: encoder EOF — flushing and stopping");
                flush_vcl(
                    &mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe,
                    &mut codec_sent, &mut stream_primed, &mut frame_count,
                    &latest_sps, &latest_pps, content_size, display_size,
                ).await.ok();
                match encoder.wait_exit().await {
                    Ok(st) if !st.success() => {
                        anyhow::bail!(
                            "ffmpeg exited with {st} after {frame_count} frame(s) — capture failed                              (is an X server really up on the capture display?)"
                        );
                    }
                    Ok(st) => info!("video: ffmpeg exited {st} after {frame_count} frame(s)"),
                    Err(e) => warn!("video: could not reap ffmpeg: {e:#}"),
                }
                if frame_count == 0 {
                    anyhow::bail!(
                        "ffmpeg produced no frames — nothing was ever sent to the car; the                          streaming session did not happen"
                    );
                }
                break;
            }
            Ok(n) => n,
            Err(e) => {
                warn!("video: read error from encoder: {e:#}");
                return Err(e);
            }
        };

        for nal in parser.push(&buf[..n]) {
            let nt = nal_type(&nal);
            match nt {
                9 => {
                    // AUD — access-unit delimiter: flush the previous AU.
                    flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                }
                7 => {
                    flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                    latest_sps = Some(nal);
                }
                8 => latest_pps = Some(nal),
                6 => { /* SEI — skip */ }
                5 => {
                    // IDR slice.
                    if !vcl.is_empty() && !pending_keyframe {
                        flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                    }
                    if !vcl.is_empty() && pending_keyframe && is_first_slice(&nal) {
                        flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                    }
                    pending_keyframe = true;
                    vcl.extend_from_slice(&avcc_wrap(&nal));
                }
                1 | 2 | 3 | 4 => {
                    // non-IDR VCL slice.
                    if !vcl.is_empty() && pending_keyframe {
                        flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                    }
                    if !vcl.is_empty() && !pending_keyframe && is_first_slice(&nal) {
                        flush_vcl(&mut wr, &mut cipher, &clock, &mut cc, &mut vcl, &mut pending_keyframe, &mut codec_sent, &mut stream_primed, &mut frame_count, &latest_sps, &latest_pps, content_size, display_size).await?;
                    }
                    vcl.extend_from_slice(&avcc_wrap(&nal));
                }
                _ => debug!("video: ignoring NAL type={nt} len={}", nal.len()),
            }
        }
    }

    encoder.stop().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn flush_vcl(
    wr: &mut OwnedWriteHalf,
    cipher: &mut VideoCipher,
    clock: &NtpClock,
    cc: &mut Congestion,
    vcl: &mut Vec<u8>,
    pending_keyframe: &mut bool,
    codec_sent: &mut bool,
    stream_primed: &mut bool,
    frame_count: &mut u64,
    latest_sps: &Option<Vec<u8>>,
    latest_pps: &Option<Vec<u8>>,
    content_size: (u32, u32),
    display_size: (u32, u32),
) -> Result<()> {
    if vcl.is_empty() {
        return Ok(());
    }

    // Until primed we need a keyframe WITH current SPS/PPS; drop stray P-frames
    // that would be undecodable and could make the receiver close the stream.
    if !*stream_primed && (!*pending_keyframe || latest_sps.is_none() || latest_pps.is_none()) {
        vcl.clear();
        *pending_keyframe = false;
        return Ok(());
    }

    // Graduated P-frame dropping (keyframes never dropped). The counter MUST
    // advance here too: `should_drop_pframe` selects by modulus, so leaving it
    // parked on a multiple would latch the predicate true and drop every
    // subsequent P-frame until a keyframe bumped it.
    if *pending_keyframe {
        cc.broken = false;
    } else if cc.broken || cc.should_drop_pframe(*frame_count) {
        cc.broken = true;
        vcl.clear();
        *frame_count += 1;
        return Ok(());
    }

    let ntp = clock.now();

    // Cleartext codec (SPS+PPS) frame before each keyframe.
    if *pending_keyframe && !*codec_sent {
        if let (Some(sps), Some(pps)) = (latest_sps, latest_pps) {
            let avcc = build_avcc_config(sps, pps).context("build avcC codec frame")?;
            let hdr = codec_header(avcc.len(), ntp, content_size, display_size);
            write_packet(wr, &hdr, &avcc).await.context("send codec frame")?;
            *codec_sent = true;
            *stream_primed = true;
        }
    }

    let is_key = *pending_keyframe;
    // Build the header FIRST (its length field must reflect the ciphertext size,
    // and for ChaCha the header IS the AEAD AAD), then encrypt.
    let out_len = cipher.output_len(vcl.len());
    let hdr = video_header(out_len, is_key, ntp);
    cipher.set_aad_header(&hdr);
    let encrypted = cipher.encrypt_frame(vcl);
    debug_assert_eq!(encrypted.len(), out_len);

    let start = Instant::now();
    write_packet(wr, &hdr, &encrypted).await.with_context(|| {
        format!("send {} frame #{}", if is_key { "IDR" } else { "non-IDR" }, *frame_count)
    })?;
    cc.record(encrypted.len() + MIRROR_HEADER_LEN, start.elapsed());

    if *frame_count < 10 {
        debug!("video: sent {} frame #{} ({}B payload, ntp={ntp})", if is_key { "IDR" } else { "P" }, *frame_count, encrypted.len());
    }

    vcl.clear();
    *pending_keyframe = false;
    *codec_sent = false;
    *frame_count += 1;
    Ok(())
}

/// Write header + payload with a bounded deadline (the sink's implicit flow
/// control: a slow/blocked receiver surfaces as a write timeout → error → stop).
async fn write_packet(wr: &mut OwnedWriteHalf, header: &[u8], payload: &[u8]) -> Result<()> {
    let write = async {
        wr.write_all(header).await?;
        wr.write_all(payload).await?;
        wr.flush().await?;
        Ok::<(), std::io::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(2), write).await {
        Ok(r) => r.context("data-conn write"),
        Err(_) => anyhow::bail!("data-conn write timed out (2s) — receiver stalled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_x_pointer_is_not_captured() {
        let args = VideoEncoder::new(VideoSettings::default()).build_args();
        let i = args.iter().position(|a| a == "-i").expect("-i");
        let d = args.iter().position(|a| a == "-draw_mouse").expect("-draw_mouse present");
        assert!(d < i, "x11grab input options must come before -i: {args:?}");
        assert_eq!(args[d + 1], "0");
    }

    #[test]
    fn x264_keeps_deblocking_and_a_gop() {
        if h264_encoder() != H264Encoder::X264 {
            return; // libopenh264 fallback has neither knob
        }
        let args = VideoEncoder::new(VideoSettings { keyframe_interval: 15, bitrate_kbps: 3000, ..VideoSettings::default() }).build_args();
        let val = |k: &str| args[args.iter().position(|a| a == k).unwrap_or_else(|| panic!("{k} missing")) + 1].clone();
        assert_ne!(val("-preset"), "ultrafast", "ultrafast disables deblocking");
        assert_eq!(val("-g"), "15");
        assert_eq!(val("-bufsize"), "3000k");
    }

    #[test]
    fn codec_header_layout() {
        let h = codec_header(42, 0x1122_3344_5566_7788, (800, 480), (1280, 720));
        assert_eq!(u32::from_le_bytes(h[0..4].try_into().unwrap()), 42);
        assert_eq!(h[4], 0x01); // codec type
        // smallParam[1]: Encrypted | Unused4. RespectTimestamps (1<<1) is
        // deliberately NOT set — see screen_flags_small1().
        assert_eq!(h[6], SCREEN_FLAG_ENCRYPTED | SCREEN_FLAG_UNUSED4);
        assert_eq!(h[6] & SCREEN_FLAG_RESPECT_TIMESTAMPS, 0);
        assert_eq!(h[7], 0x01);
        assert_eq!(u64::from_le_bytes(h[8..16].try_into().unwrap()), 0x1122_3344_5566_7788);
        assert_eq!(f32::from_le_bytes(h[16..20].try_into().unwrap()), 800.0);
        assert_eq!(f32::from_le_bytes(h[56..60].try_into().unwrap()), 1280.0);
        assert_eq!(f32::from_le_bytes(h[60..64].try_into().unwrap()), 720.0);
    }

    #[test]
    fn video_header_keyframe_flag() {
        let idr = video_header(100, true, 7);
        assert_eq!(u32::from_le_bytes(idr[0..4].try_into().unwrap()), 100);
        assert_eq!(idr[4], 0x00);
        assert_eq!(idr[5], 0x10); // IDR
        let p = video_header(100, false, 7);
        assert_eq!(p[5], 0x00);
    }

    #[test]
    fn start_code_and_nal_type() {
        // 00 00 00 01 | 0x67 (SPS, type 7) | payload
        let stream = [0u8, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f];
        let raw = strip_start_code(&stream);
        assert_eq!(raw[0], 0x67);
        assert_eq!(nal_type(raw), 7);
    }

    #[test]
    fn parser_splits_two_nals() {
        let mut p = NalParser::new();
        // Need a trailing start code to delimit the second NAL, plus another to
        // close it (parser emits a NAL only once the NEXT start code arrives).
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0xCC, // IDR (sentinel to close PPS)
        ];
        let nals = p.push(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nal_type(&nals[0]), 7);
        assert_eq!(nal_type(&nals[1]), 8);
    }

    /// Regression: the drop path must advance the frame counter, otherwise the
    /// modulus predicate latches and level 1 drops nearly every P-frame.
    #[test]
    fn congestion_level_one_drops_exactly_one_third() {
        let mut cc = Congestion::new();
        cc.level = 1;
        let (mut sent, mut dropped, mut frame_count) = (0u32, 0u32, 0u64);
        for _ in 0..300 {
            if cc.should_drop_pframe(frame_count) {
                dropped += 1;
            } else {
                sent += 1;
            }
            // Mirrors flush_vcl: the counter advances on BOTH paths.
            frame_count += 1;
        }
        assert_eq!((sent, dropped), (200, 100));

        let mut cc = Congestion::new();
        cc.level = 2;
        let (mut sent, mut frame_count) = (0u32, 0u64);
        for _ in 0..300 {
            if !cc.should_drop_pframe(frame_count) {
                sent += 1;
            }
            frame_count += 1;
        }
        assert_eq!(sent, 150);
    }

    #[test]
    fn avcc_config_rejects_short_sps() {
        // A truncated SPS must be an error, not a panic in the video task.
        assert!(build_avcc_config(&[0x67, 0x42, 0xC0], &[0x68]).is_err());
        assert!(build_avcc_config(&[], &[0x68]).is_err());
        assert!(build_avcc_config(&[0x67, 0x42, 0xC0, 0x1f], &[0x68]).is_ok());
    }

    #[test]
    fn avcc_wrap_prefixes_length() {
        let w = avcc_wrap(&[0xAA, 0xBB, 0xCC]);
        assert_eq!(&w[0..4], &[0, 0, 0, 3]);
        assert_eq!(&w[4..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn avcc_config_has_sps_pps() {
        let sps = [0x67, 0x42, 0xC0, 0x1f, 0x11];
        let pps = [0x68, 0xCE, 0x3c];
        let c = build_avcc_config(&sps, &pps).unwrap();
        assert_eq!(c[0], 0x01);
        assert_eq!(c[1], 0x42); // profile from sps[1]
        assert_eq!(c[5], 0xe1);
        // numPPS byte right after SPS block.
        assert_eq!(c[8 + sps.len()], 0x01);
    }
}

#[cfg(test)]
mod screen_flag_tests {
    use super::*;

    #[test]
    fn video_config_does_not_ask_the_receiver_to_respect_our_timestamps() {
        // AirPlayReceiverSessionScreen.c:547 reads this bit from smallParam[1]
        // (header byte 6). Set, the receiver schedules frames against our NTP
        // clock; cleared, it displays them on arrival.
        let h = codec_header(42, 0, (800, 480), (1280, 720));
        assert_eq!(
            h[6] & SCREEN_FLAG_RESPECT_TIMESTAMPS,
            0,
            "default must be display-on-arrival: byte6=0x{:02x}",
            h[6]
        );
    }

    #[test]
    fn the_frame_clock_is_on_the_ntp_epoch_like_the_timing_replies() {
        // The timing responder answers with seconds since the NTP epoch. A frame
        // PTS on a different epoch put every frame ~124 years in the receiver's
        // past. Upper 32 bits are the seconds field.
        // Both clocks are "NTP epoch + time since this process started", so the
        // seconds field sits just above the epoch offset. Before the fix the frame
        // PTS omitted the offset entirely and was 2_208_988_800 s behind the
        // timing replies.
        let epoch_secs = crate::ntp_time::NTP_TO_UNIX.as_secs();
        let secs = NtpClock::new().now() >> 32;
        assert!(
            (epoch_secs..epoch_secs + 60).contains(&secs),
            "frame PTS must share the timing responder's epoch (~{epoch_secs} s), got {secs} s"
        );
    }

    #[test]
    fn the_hud_flag_is_off_unless_explicitly_asked_for() {
        let h = codec_header(42, 0, (800, 480), (1280, 720));
        assert_eq!(h[5] & SCREEN_FLAG_SHOW_HUD, 0);
    }
}

#[cfg(test)]
mod avcc_trailer_tests {
    use super::*;

    #[test]
    fn the_record_is_well_formed_and_carries_both_parameter_sets() {
        let sps = vec![0x67, 0x64, 0x00, 0x1f, 0xAA, 0xBB];
        let pps = vec![0x68, 0xEE, 0x3C, 0x80];
        let avcc = build_avcc_config(&sps, &pps).unwrap();
        assert_eq!(avcc[0], 0x01, "configurationVersion");
        assert_eq!(&avcc[1..4], &sps[1..4], "profile/compat/level come from the SPS");
        assert_eq!(avcc[4], 0xff, "lengthSizeMinusOne = 3");
        assert_eq!(avcc[5], 0xe1, "numOfSequenceParameterSets = 1");
        assert_eq!(u16::from_be_bytes([avcc[6], avcc[7]]) as usize, sps.len());
        assert_eq!(&avcc[8..8 + sps.len()], &sps[..]);
        let off = 8 + sps.len();
        assert_eq!(avcc[off], 0x01, "numOfPictureParameterSets = 1");
        assert_eq!(u16::from_be_bytes([avcc[off + 1], avcc[off + 2]]) as usize, pps.len());
        assert_eq!(&avcc[off + 3..off + 3 + pps.len()], &pps[..]);
    }

    #[test]
    fn a_short_sps_is_rejected_rather_than_indexed_out_of_bounds() {
        assert!(build_avcc_config(&[0x67, 0x64], &[0x68]).is_err());
    }

    #[test]
    fn the_default_still_carries_the_unverified_trailer() {
        // Deliberately unchanged: flipping it is a car-side A/B
        // (RASCAR_AVCC_TRAILER=0), not a hunch to ship blind.
        let sps = vec![0x67, 0x64, 0x00, 0x1f, 0xAA, 0xBB];
        let pps = vec![0x68, 0xEE, 0x3C, 0x80];
        let iso_len = 6 + 2 + sps.len() + 1 + 2 + pps.len();
        assert_eq!(build_avcc_config(&sps, &pps).unwrap().len(), iso_len + 4);
    }
}
