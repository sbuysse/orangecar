//! Audio capture: read the board's audio output as PCM so it can be sent to
//! the car.
//!
//! [`AudioCapture`] runs `parec` (s16le) with the cross-user PulseAudio
//! environment (`PULSE_SERVER`/`XDG_RUNTIME_DIR`/`PULSE_COOKIE`) so it works
//! when the transmitter runs as root. The source defaults to
//! `$RASCAR_AUDIO_SOURCE`, else the default sink's monitor. The capture code
//! comes from the project's earlier Android Auto bridge.
//!
//! In a CarPlay session `airplay::carplay_session` uses [`AudioCapture`] for the
//! MainAudio RTP stream (media audio to the car); telephony and the ringtone
//! live in `stream::call`.
//!
//! [`run`] belongs to the older AirPlay screen-mirroring path (`stream::run`),
//! which negotiates no audio stream: there it captures nothing and only logs
//! that audio is not negotiated, unless an [`AudioStreamInfo`] is supplied.

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// Parameters of a negotiated AirPlay audio stream (populated once the SETUP
/// grows an audio descriptor). `None` today.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed once the RTP audio sender is ported (TODO(hw))
pub struct AudioStreamInfo {
    pub data_port: u16,
    pub control_port: u16,
    pub sample_rate: u32,
}

/// Default capture format for AirPlay media audio.
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

/// Where the PulseAudio server of the logged-in user lives, for a process
/// that may run as root (rascar-carplay does, with no XDG_RUNTIME_DIR/HOME).
///
/// Car drive 2026-09-28: the old code assumed `/run/user/<own uid>` — i.e.
/// `/run/user/0`, which does not exist — so `parec` died 2 ms after starting
/// and the car got a silent audio stream for the whole trip.
#[derive(Debug, Clone, PartialEq)]
pub struct PulseEnv {
    pub runtime_dir: String,
    pub cookie: String,
}

impl PulseEnv {
    pub fn server(&self) -> String {
        format!("unix:{}/pulse/native", self.runtime_dir)
    }

    /// Environment first, then this process's own user, then the user whose
    /// session actually runs a server (lowest uid ≥ 1000 wins).
    pub fn discover() -> Self {
        let has_socket = |dir: &str| std::path::Path::new(&format!("{dir}/pulse/native")).exists();
        // SAFETY: getuid() has no preconditions and cannot fail.
        let own = format!("/run/user/{}", unsafe { libc::getuid() });
        let env_dir = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty());
        let mut candidates: Vec<String> = env_dir.into_iter().chain(std::iter::once(own.clone())).collect();
        let mut users: Vec<(u32, String)> = std::fs::read_dir("/run/user")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let uid: u32 = e.file_name().to_str()?.parse().ok()?;
                Some((uid, e.path().to_string_lossy().into_owned()))
            })
            .filter(|(uid, _)| *uid >= 1000)
            .collect();
        users.sort();
        candidates.extend(users.into_iter().map(|(_, d)| d));
        let runtime_dir = candidates.into_iter().find(|d| has_socket(d)).unwrap_or(own);
        let cookie = std::env::var("PULSE_COOKIE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            owner_home(&runtime_dir)
                .map(|h| format!("{h}/.config/pulse/cookie"))
                .unwrap_or_else(|| format!("{runtime_dir}/pulse/cookie"))
        });
        Self { runtime_dir, cookie }
    }

    pub(crate) fn apply(&self, cmd: &mut Command) {
        cmd.env("PULSE_SERVER", self.server()).env("XDG_RUNTIME_DIR", &self.runtime_dir).env("PULSE_COOKIE", &self.cookie);
    }

    /// The monitor of the default output (what the Pi plays), or None while
    /// the output is not ready (PulseAudio's placeholder `auto_null`).
    pub async fn default_monitor(&self) -> Option<String> {
        let mut cmd = Command::new("pactl");
        cmd.arg("get-default-sink");
        self.apply(&mut cmd);
        let out = cmd.output().await.ok()?;
        let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
        monitor_of(&sink)
    }
}

/// `<sink>.monitor`, unless the sink is missing or the null placeholder.
pub fn monitor_of(sink: &str) -> Option<String> {
    (!sink.is_empty() && sink != "auto_null").then(|| format!("{sink}.monitor"))
}

/// Home directory of the user owning `path` (for their PulseAudio cookie).
fn owner_home(path: &str) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(path).ok()?.uid();
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 5 && f[2].parse::<u32>().ok() == Some(uid)).then(|| f[5].to_string())
    })
}

/// PCM capture via `parec` (copied from the AA bridge).
pub struct AudioCapture {
    sample_rate: u32,
    channels: u32,
    process: Option<Child>,
}

impl AudioCapture {
    pub fn new(sample_rate: u32, channels: u32) -> Self {
        Self { sample_rate, channels, process: None }
    }

    /// Start capturing `device`, or — when None — the monitor of the default
    /// output (what the Pi plays). Without an explicit device, parec used to
    /// record the default SOURCE: the USB microphone, not the music.
    pub async fn start(&mut self, device: Option<&str>) -> Result<()> {
        let pulse = PulseEnv::discover();
        let device = match device {
            Some(d) => d.to_string(),
            None => pulse
                .default_monitor()
                .await
                .ok_or_else(|| anyhow::anyhow!("sound output not ready (no default sink yet, server {})", pulse.server()))?,
        };
        let args = vec![
            // CarPlay LinearPCM is 16-bit BIG-endian (catplay `copy_pcm16_be_to_samples`);
            // little-endian samples play as loud noise on the car.
            "--format=s16be".to_string(),
            format!("--rate={}", self.sample_rate),
            format!("--channels={}", self.channels),
            "--raw".to_string(),
            // Small steady chunks: the sender pads real gaps with silence, and
            // parec's default (large) latency would look like gaps mid-song.
            "--latency-msec=20".to_string(),
            format!("--device={device}"),
        ];
        let mut cmd = Command::new("parec");
        cmd.args(&args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        pulse.apply(&mut cmd);
        let mut child = cmd.spawn().context("spawning parec — is pulseaudio/pipewire-pulse installed?")?;
        // parec's reason for failing ("Connection refused", "Access denied")
        // used to go to /dev/null; that is what hid the silent car trip.
        if let Some(err) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    warn!("audio: parec: {l}");
                }
            });
        }
        info!("audio: capture started ({}Hz {}ch, device={device}, server={})", self.sample_rate, self.channels, pulse.server());
        self.process = Some(child);
        Ok(())
    }

    /// Read a PCM chunk. `Ok(0)` = EOF.
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let child = self.process.as_mut().context("capture not started")?;
        let stdout = child.stdout.as_mut().context("no stdout")?;
        Ok(stdout.read(buf).await?)
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            info!("audio: capture stopped");
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.start_kill();
        }
    }
}

/// Run the audio path. Gated: streams only if `audio` describes a negotiated
/// stream. Today `audio` is always `None` (video-only SETUP), so this captures
/// and immediately stops, logging the gate — it never fights the media path.
pub async fn run(audio: Option<AudioStreamInfo>) -> Result<()> {
    let Some(info) = audio else {
        info!("audio: gated — no AirPlay audio stream negotiated in SETUP (video-only). Skipping audio capture. (TODO(hw): add type=96 audio SETUP + RTP sender.)");
        return Ok(());
    };

    let source = std::env::var("RASCAR_AUDIO_SOURCE").ok().filter(|s| !s.is_empty());
    let rate = if info.sample_rate > 0 { info.sample_rate } else { SAMPLE_RATE };
    let mut capture = AudioCapture::new(rate, CHANNELS);
    capture.start(source.as_deref()).await?;

    // 20ms @ 48k stereo s16 = 3840 bytes.
    let mut buf = vec![0u8; 3840];
    warn!("audio: capturing PCM but the RTP/UDP audio sender is not yet implemented — frames are read and dropped (TODO(hw)).");
    loop {
        match capture.read_chunk(&mut buf).await {
            Ok(0) => {
                info!("audio: capture EOF — stopping");
                break;
            }
            Ok(_n) => {
                // TODO(hw): encode (ALAC/PCM) + RTP-packetize + encrypt +
                // sendto(audioDataPort) per doubletake StreamAudio.
            }
            Err(e) => {
                warn!("audio: read error: {e:#}");
                break;
            }
        }
    }
    capture.stop().await;
    Ok(())
}

#[cfg(test)]
mod pulse_tests {
    use super::*;

    #[test]
    fn the_null_placeholder_is_not_a_monitor() {
        assert_eq!(monitor_of("alsa_output.platform-sound.stereo-fallback").as_deref(), Some("alsa_output.platform-sound.stereo-fallback.monitor"));
        assert_eq!(monitor_of("auto_null"), None);
        assert_eq!(monitor_of(""), None);
    }

    #[test]
    fn discovery_never_panics_and_points_under_run_user() {
        let p = PulseEnv::discover();
        assert!(p.server().starts_with("unix:"), "{p:?}");
        assert!(p.server().ends_with("/pulse/native"), "{p:?}");
    }
}
