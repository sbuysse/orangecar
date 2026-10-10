//! Audio capture and playback for Android Auto.
//!
//! Captures system audio from PulseAudio/PipeWire via `parec` and encodes as
//! raw PCM 16-bit. Also plays microphone audio received from the car.
//!
//! AA audio streams:
//!   - Media:  48kHz stereo PCM 16-bit
//!   - System: 16kHz mono PCM 16-bit
//!   - Mic in: 16kHz mono PCM 16-bit (car → Pi)

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tracing::info;

/// Resolve the user's XDG runtime dir for talking to PulseAudio/PipeWire.
/// Prefers `$XDG_RUNTIME_DIR`; falls back to `/run/user/<current uid>`.
fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // SAFETY: getuid() is always safe — it has no preconditions and
            // cannot fail.
            let uid = unsafe { libc::getuid() };
            format!("/run/user/{uid}")
        })
}

/// Resolve the PulseAudio cookie path from `$HOME`, falling back to the runtime
/// dir's pulse cookie when HOME is unset.
fn pulse_cookie() -> String {
    if let Some(home) = std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        format!("{home}/.config/pulse/cookie")
    } else {
        format!("{}/pulse/cookie", runtime_dir())
    }
}

/// Apply the PulseAudio environment a root-owned process needs to reach the
/// desktop user's sound server. Both `parec` and `pacat` need it: without
/// PULSE_SERVER/XDG_RUNTIME_DIR/PULSE_COOKIE the client connects to nothing,
/// `spawn()` still succeeds, and the failure only shows up as EPIPE on the
/// first read/write.
fn apply_pulse_env(cmd: &mut Command) {
    let xdg = runtime_dir();
    cmd.env("PULSE_SERVER", format!("unix:{xdg}/pulse/native"))
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("PULSE_COOKIE", pulse_cookie());
}

pub struct AudioCapture {
    sample_rate: u32,
    channels: u32,
    process: Option<Child>,
}

impl AudioCapture {
    pub fn new(sample_rate: u32, channels: u32) -> Self {
        Self {
            sample_rate,
            channels,
            process: None,
        }
    }

    pub async fn start(&mut self, device: Option<&str>) -> Result<()> {
        let mut args = vec![
            "--format=s16le".to_string(),
            format!("--rate={}", self.sample_rate),
            format!("--channels={}", self.channels),
            "--raw".to_string(),
        ];
        if let Some(dev) = device {
            args.push(format!("--device={dev}"));
        }
        // Bridge may run as root; PulseAudio belongs to the desktop user.
        // XDG_RUNTIME_DIR + PULSE_COOKIE are required to connect across users.
        // Derive both from the environment so this isn't pinned to one machine.
        let mut cmd = Command::new("parec");
        cmd.args(&args);
        apply_pulse_env(&mut cmd);
        let child = cmd
            .stdout(std::process::Stdio::piped())
            // Surface parec's own diagnostics in the journal — a silent failure
            // here is indistinguishable from "the car sent no audio".
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("spawning parec")?;

        info!(
            "audio capture started: {}Hz {}ch",
            self.sample_rate, self.channels
        );
        self.process = Some(child);
        Ok(())
    }

    /// Read a chunk of raw PCM audio.
    /// Default 20ms = 960 frames @ 48kHz stereo = 3840 bytes.
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let child = self.process.as_mut().context("capture not started")?;
        let stdout = child.stdout.as_mut().context("no stdout")?;
        let n = stdout.read(buf).await?;
        Ok(n)
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            info!("audio capture stopped");
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

pub struct AudioPlayer {
    sample_rate: u32,
    channels: u32,
    process: Option<Child>,
}

impl AudioPlayer {
    pub fn new(sample_rate: u32, channels: u32) -> Self {
        Self {
            sample_rate,
            channels,
            process: None,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        // Same PulseAudio environment as AudioCapture — the bridge runs as root
        // while PulseAudio belongs to the desktop user.
        let mut cmd = Command::new("pacat");
        cmd.args([
            "--format=s16le",
            &format!("--rate={}", self.sample_rate),
            &format!("--channels={}", self.channels),
            "--raw",
            "--playback",
        ]);
        apply_pulse_env(&mut cmd);
        let child = cmd
            .stdin(std::process::Stdio::piped())
            // Not Stdio::null(): a pacat that cannot reach the sound server still
            // spawns successfully and only fails on the first write, so its stderr
            // is the only explanation an operator gets.
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("spawning pacat")?;

        info!(
            "audio player started: {}Hz {}ch",
            self.sample_rate, self.channels
        );
        self.process = Some(child);
        Ok(())
    }

    pub async fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        let child = self.process.as_mut().context("player not started")?;
        let stdin = child.stdin.as_mut().context("no stdin")?;
        stdin.write_all(data).await?;
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            info!("audio player stopped");
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.start_kill();
        }
    }
}
