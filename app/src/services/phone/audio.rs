//! Call audio between the SIM7600's PCM port (USB interface 4, "Audio 9001")
//! and PulseAudio, while `AT+CPCMREG=1` is on.
//!
//! * modem → car: read the PCM port, write into `pacat --playback` on the
//!   default sink (what rascar-carplay sends to the car's telephony stream).
//! * car → modem: `parec` from `rascar_car_mic_in` (the car microphone source
//!   rascar-carplay creates on `call 1`), 20 ms frames, written to the port.
//!   Until that source exists (carplay creates it a moment after our first
//!   heartbeat) parec fails; we retry every second and feed the modem
//!   silence meanwhile so its PCM clock never starves.
//!
//! Format (SIMCom USB AUDIO Application Note V1.03): 8 kHz 16-bit mono
//! little-endian; 16 kHz when `AT+CPCMFRM?` reports 1. 20 ms = 320 bytes at
//! 8 kHz. Every child is `kill_on_drop` and its stderr inherited (never
//! /dev/null). Both directions log their RMS every 5 s:
//! the bench gate is "both directions non-zero".

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub const CAR_MIC_SOURCE: &str = "rascar_car_mic_in";
const METER_PERIOD: Duration = Duration::from_secs(5);

/// Start/stop of the call audio; a recorder in the engine tests.
pub trait AudioCtl: Send {
    fn start(&mut self, rate: u32) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn stop(&mut self);
    fn running(&self) -> bool;
}

/// Bytes in one 20 ms frame of s16 mono.
pub fn frame_bytes(rate: u32) -> usize {
    (rate as usize / 50) * 2
}

/// RMS of s16le samples, in sample units (0..32768) — the formula [`Meter`] logs.
#[cfg(test)]
pub fn rms_s16le(pcm: &[u8]) -> f64 {
    let n = pcm.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let sum: f64 = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64).map(|s| s * s).sum();
    (sum / n as f64).sqrt()
}

/// Running level of one direction, logged every [`METER_PERIOD`].
struct Meter {
    name: &'static str,
    sum_sq: f64,
    samples: u64,
    bytes: u64,
    since: tokio::time::Instant,
}

impl Meter {
    fn new(name: &'static str) -> Self {
        Self { name, sum_sq: 0.0, samples: 0, bytes: 0, since: tokio::time::Instant::now() }
    }

    fn feed(&mut self, pcm: &[u8]) {
        for c in pcm.chunks_exact(2) {
            let s = i16::from_le_bytes([c[0], c[1]]) as f64;
            self.sum_sq += s * s;
        }
        self.samples += (pcm.len() / 2) as u64;
        self.bytes += pcm.len() as u64;
        if self.since.elapsed() >= METER_PERIOD {
            let rms = if self.samples > 0 { (self.sum_sq / self.samples as f64).sqrt() } else { 0.0 };
            let secs = self.since.elapsed().as_secs_f64();
            info!("phone audio {}: rms={rms:.0} bytes={} ({:.0} B/s)", self.name, self.bytes, self.bytes as f64 / secs);
            self.sum_sq = 0.0;
            self.samples = 0;
            self.bytes = 0;
            self.since = tokio::time::Instant::now();
        }
    }
}

/// PulseAudio clients need the user's runtime dir; the app's unit normally
/// provides it, but a missing one must not silently send audio nowhere
/// (Chromium without PULSE_SERVER plays into the null sink).
fn pulse_env(cmd: &mut Command) {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }
}

fn pacat(rate: u32) -> std::io::Result<Child> {
    let mut cmd = Command::new("pacat");
    cmd.args([
        "--playback",
        "--raw",
        "--format=s16le",
        &format!("--rate={rate}"),
        "--channels=1",
        "--latency-msec=60",
        "--client-name=rascar-phone",
        "--stream-name=Phone call",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    pulse_env(&mut cmd);
    cmd.spawn()
}

fn parec(rate: u32) -> std::io::Result<Child> {
    let mut cmd = Command::new("parec");
    cmd.args([
        &format!("--device={CAR_MIC_SOURCE}"),
        "--raw",
        "--format=s16le",
        &format!("--rate={rate}"),
        "--channels=1",
        "--latency-msec=20",
        "--client-name=rascar-phone",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    pulse_env(&mut cmd);
    cmd.spawn()
}

/// The real pump on the PCM tty.
pub struct PcmAudio {
    pcm_port: PathBuf,
    tasks: Vec<JoinHandle<()>>,
}

impl PcmAudio {
    pub fn new(pcm_port: PathBuf) -> Self {
        Self { pcm_port, tasks: Vec::new() }
    }
}

impl Drop for PcmAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

impl AudioCtl for PcmAudio {
    async fn start(&mut self, rate: u32) -> anyhow::Result<()> {
        use tokio_serial::SerialPortBuilderExt;
        self.stop();
        // serialport opens exclusively (TIOCEXCL + flock) and in raw mode.
        let port = tokio_serial::new(self.pcm_port.to_string_lossy(), 115_200).open_native_async()?;
        let (mut rd, mut wr) = tokio::io::split(port);
        let frame = frame_bytes(rate);
        info!("phone audio: pumping {} at {rate} Hz ({frame} B / 20 ms)", self.pcm_port.display());

        // modem → car
        self.tasks.push(tokio::spawn(async move {
            let mut meter = Meter::new("down (modem→car)");
            let mut buf = vec![0u8; frame * 4];
            let mut player: Option<Child> = None;
            loop {
                let n = match rd.read(&mut buf).await {
                    Ok(0) => {
                        warn!("phone audio: PCM port closed");
                        return;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        warn!("phone audio: PCM read: {e}");
                        return;
                    }
                };
                meter.feed(&buf[..n]);
                if player.is_none() {
                    match pacat(rate) {
                        Ok(c) => player = Some(c),
                        Err(e) => {
                            warn!("phone audio: pacat: {e} (the caller cannot be heard)");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    }
                }
                let stdin = player.as_mut().and_then(|p| p.stdin.as_mut());
                let ok = match stdin {
                    Some(s) => s.write_all(&buf[..n]).await.is_ok(),
                    None => false,
                };
                if !ok {
                    // pacat died (Pulse restarted?): respawn on the next chunk.
                    warn!("phone audio: pacat stopped, restarting");
                    player = None;
                }
            }
        }));

        // car → modem
        self.tasks.push(tokio::spawn(async move {
            let mut meter = Meter::new("up (car mic→modem)");
            let silence = vec![0u8; frame];
            let mut frame_buf = vec![0u8; frame];
            let mut failures = 0u32;
            loop {
                let mut rec = match parec(rate) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("phone audio: parec: {e}");
                        None
                    }
                };
                let mut out = rec.as_mut().and_then(|c| c.stdout.take());
                let started = tokio::time::Instant::now();
                if let Some(o) = out.as_mut() {
                    // Real microphone frames, paced by parec itself.
                    while o.read_exact(&mut frame_buf).await.is_ok() {
                        meter.feed(&frame_buf);
                        if let Err(e) = wr.write_all(&frame_buf).await {
                            warn!("phone audio: PCM write: {e}");
                            return;
                        }
                    }
                }
                drop(rec);
                // parec ended at once: the car mic source is not there yet.
                if started.elapsed() < Duration::from_secs(2) {
                    failures += 1;
                    if failures == 5 {
                        warn!("phone audio: no '{CAR_MIC_SOURCE}' after 5 s (is rascar-carplay in a CarPlay session?), sending silence");
                    }
                } else {
                    failures = 0;
                }
                // One second of paced silence before trying again.
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                for _ in 0..50 {
                    tick.tick().await;
                    if let Err(e) = wr.write_all(&silence).await {
                        warn!("phone audio: PCM write: {e}");
                        return;
                    }
                    meter.feed(&silence);
                }
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        if !self.tasks.is_empty() {
            info!("phone audio: stopped");
        }
        // Aborting drops each task's children (kill_on_drop) and its half of
        // the port, which closes the tty once both halves are gone.
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }

    fn running(&self) -> bool {
        !self.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_20_ms() {
        assert_eq!(frame_bytes(8000), 320);
        assert_eq!(frame_bytes(16000), 640);
    }

    #[test]
    fn rms_of_known_signals() {
        assert_eq!(rms_s16le(&[]), 0.0);
        assert_eq!(rms_s16le(&[0, 0, 0, 0]), 0.0);
        let full: Vec<u8> = [1000i16, -1000, 1000, -1000].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert!((rms_s16le(&full) - 1000.0).abs() < 1e-9);
    }
}
