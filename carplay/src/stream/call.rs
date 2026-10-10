//! Car microphone and phone line for meetings and modem calls.
//!
//! CarPlay has one way to get the car's microphone: a MainAudio stream with
//! `audioType: "telephony"` and `input: true` (catplay `StreamDescriptionAudio`),
//! announced by `modesChanged { phoneCall: Controller }`. The car then plays
//! what we send on it through its hands-free path and records its microphone
//! back to the `dataPort` we put in the SETUP.
//!
//! This module holds the local ends of that:
//!   * [`subscribe`] — the state of the line. rascar-app sends `call 1` to
//!     [`CALL_CTRL_ADDR`] every few seconds while a meeting frame is open or a
//!     modem call is dialing/alerting/active (`call 0` at the end), and
//!     `ring 1` every few seconds while an incoming modem call rings (`ring 0`
//!     when it is answered, declined or missed). Each state also ends when its
//!     heartbeats stop (app restarted, page closed, backend crashed), so a
//!     crash never leaves the car stuck in a phone call or ringing.
//!   * [`CarMic`] — a PulseAudio source "Car microphone" fed with the PCM the
//!     car sends, made the default source for the duration of the call so the
//!     browser's getUserMedia (and the modem audio pump) picks it up.
//!   * [`ringtone_cycle`] — the tone we play on the car's "alert" stream while
//!     the line rings (Apple CarPlay spec Table 23-46: "alert" = "Ringtones,
//!     alarms, and other high-priority sounds").

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tracing::{info, warn};

use super::audio::PulseEnv;

/// UDP, loopback only. Next to the touch echo (7702).
pub const CALL_CTRL_ADDR: &str = "127.0.0.1:7703";
/// A call without a heartbeat for this long is over.
const CALL_TIMEOUT: Duration = Duration::from_secs(8);
/// Same for ringing: the backend repeats `ring 1` every 3 s, so 8 s tolerates
/// one lost datagram and still stops a crashed backend's ringtone quickly.
const RING_TIMEOUT: Duration = Duration::from_secs(8);

const SINK: &str = "rascar_car_mic";
const SOURCE: &str = "rascar_car_mic_in";

/// Kill switch: `RASCAR_CARPLAY_MIC=off` never switches the car to telephony.
pub fn enabled() -> bool {
    crate::debug::env_flag("RASCAR_CARPLAY_MIC", true)
}

/// Kill switch for the ringtone: `RASCAR_PHONE_RING=off` ignores `ring 1`, so
/// the car keeps its media stream while the modem rings (the dashboard still
/// shows the incoming call). On by default, but only reachable when the phone
/// backend sends `ring`, which nothing else does.
pub fn ring_enabled() -> bool {
    crate::debug::env_flag("RASCAR_PHONE_RING", true)
}

/// What the line is doing, as announced by rascar-app. Both can be true at
/// once (a modem call ringing during a meeting); the audio path decides which
/// one the car hears (`carplay_session::desired_audio_mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// A meeting or an established/outgoing modem call: telephony stream.
    pub call: bool,
    /// An incoming modem call is ringing: alert stream with the ringtone.
    pub ring: bool,
}

/// One control datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctl {
    Call(bool),
    Ring(bool),
}

/// Heartbeat bookkeeping for [`Line`], separate from the socket so the
/// timeouts can be tested with explicit instants.
#[derive(Debug, Default)]
pub struct LineTracker {
    last_call: Option<Instant>,
    last_ring: Option<Instant>,
}

impl LineTracker {
    pub fn on_ctl(&mut self, ctl: Ctl, now: Instant) {
        match ctl {
            Ctl::Call(true) => self.last_call = Some(now),
            Ctl::Call(false) => self.last_call = None,
            Ctl::Ring(true) => self.last_ring = Some(now),
            Ctl::Ring(false) => self.last_ring = None,
        }
    }

    /// The line at `now`; a state whose heartbeat is older than its timeout
    /// is dropped for good (a late heartbeat starts it again).
    pub fn line(&mut self, now: Instant) -> Line {
        let alive = |t: &mut Option<Instant>, timeout: Duration| {
            let on = t.is_some_and(|t| now.saturating_duration_since(t) < timeout);
            if !on {
                *t = None;
            }
            on
        };
        Line { call: alive(&mut self.last_call, CALL_TIMEOUT), ring: alive(&mut self.last_ring, RING_TIMEOUT) }
    }
}

/// The line as announced by rascar-app. The listener starts on first use.
pub fn subscribe() -> watch::Receiver<Line> {
    static STATE: OnceLock<watch::Sender<Line>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            let (tx, _) = watch::channel(Line::default());
            let txc = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = listen(txc).await {
                    warn!("call: control listener stopped: {e:#}");
                }
            });
            tx
        })
        .subscribe()
}

async fn listen(tx: watch::Sender<Line>) -> Result<()> {
    let sock = tokio::net::UdpSocket::bind(CALL_CTRL_ADDR).await.with_context(|| format!("bind {CALL_CTRL_ADDR}"))?;
    info!("call: listening for meeting/call/ring state on {CALL_CTRL_ADDR}");
    let mut tracker = LineTracker::default();
    let mut buf = [0u8; 64];
    loop {
        match tokio::time::timeout(Duration::from_secs(1), sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from.ip().is_loopback() => match parse(&buf[..n]) {
                // The kill switch drops `ring` at the door, so nothing
                // downstream can ever switch the car to the alert stream.
                Some(Ctl::Ring(_)) if !ring_enabled() => {}
                Some(ctl) => tracker.on_ctl(ctl, Instant::now()),
                None => {}
            },
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("call: recv: {e}"),
            Err(_) => {}
        }
        let line = tracker.line(Instant::now());
        tx.send_if_modified(|cur| {
            let changed = *cur != line;
            if changed {
                if cur.call != line.call {
                    info!("call: call {}", if line.call { "started" } else { "ended" });
                }
                if cur.ring != line.ring {
                    info!("call: ringing {}", if line.ring { "started" } else { "stopped" });
                }
            }
            *cur = line;
            changed
        });
    }
}

/// `call 1` / `call 0` / `ring 1` / `ring 0` (anything else is ignored).
pub fn parse(msg: &[u8]) -> Option<Ctl> {
    match std::str::from_utf8(msg).ok()?.trim() {
        "call 1" => Some(Ctl::Call(true)),
        "call 0" => Some(Ctl::Call(false)),
        "ring 1" => Some(Ctl::Ring(true)),
        "ring 0" => Some(Ctl::Ring(false)),
        _ => None,
    }
}

/// One cycle of the ringtone as 16-bit big-endian PCM (CarPlay's wire order,
/// not little-endian), `channels` interleaved, to be looped.
///
/// Generated rather than bundled: no asset to ship or find at run time, and
/// it is produced at whatever rate the car's alert format asks for, so no
/// resampling. The pattern is two short two-note chimes (E6 then B5, a decaying
/// sine with a soft octave partial, like a marimba) followed by silence:
/// 0.0–0.9 s chimes, 0.9–3.0 s quiet. Peak stays below -6 dBFS so the car's
/// own ringtone volume category (spec 23.2.8.2.9) has headroom.
pub fn ringtone_cycle(rate: u32, channels: u32) -> Vec<u8> {
    const CYCLE_S: f64 = 3.0;
    const NOTE_S: f64 = 0.20;
    const NOTE_GAP_S: f64 = 0.225;
    const NOTES_HZ: [f64; 4] = [1318.51, 987.77, 1318.51, 987.77];
    const AMP: f64 = 0.38;
    let frames = (rate as f64 * CYCLE_S) as usize;
    let mut out = Vec::with_capacity(frames * channels as usize * 2);
    for i in 0..frames {
        let t = i as f64 / rate as f64;
        let k = (t / NOTE_GAP_S) as usize;
        let mut v = 0.0;
        if k < NOTES_HZ.len() {
            let nt = t - k as f64 * NOTE_GAP_S;
            if nt < NOTE_S {
                let f = NOTES_HZ[k];
                // 4 ms attack and 15 ms release ramps: no clicks at the edges.
                let attack = (nt / 0.004).min(1.0);
                let release = ((NOTE_S - nt) / 0.015).min(1.0);
                let decay = (-nt * 14.0).exp();
                let w = std::f64::consts::TAU * f * nt;
                v = AMP * attack * release * decay * (w.sin() + 0.25 * (2.0 * w).sin()) / 1.25;
            }
        }
        let s = (v * i16::MAX as f64).round() as i16;
        for _ in 0..channels {
            out.extend_from_slice(&s.to_be_bytes());
        }
    }
    out
}

/// The "Car microphone" PulseAudio source for one call.
pub struct CarMic {
    pulse: PulseEnv,
    player: Option<Child>,
    previous_source: Option<String>,
}

impl CarMic {
    /// Create the source (null sink + remap of its monitor), make it the
    /// default, and start the writer that feeds it `rate` Hz mono s16be.
    pub async fn open(rate: u32) -> Result<Self> {
        let pulse = PulseEnv::discover();
        let r = Self::try_open(&pulse, rate).await;
        if r.is_err() {
            unload_ours(&pulse).await;
        }
        r
    }

    async fn try_open(pulse: &PulseEnv, rate: u32) -> Result<Self> {
        let pulse = pulse.clone();
        unload_ours(&pulse).await; // leftovers of a crash
        let previous_source = pactl(&pulse, &["get-default-source"]).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        pactl(
            &pulse,
            &["load-module", "module-null-sink", &format!("sink_name={SINK}"), "sink_properties=device.description=Car-microphone-feed",
              &format!("rate={rate}"), "channels=1"],
        )
        .await
        .context("load null sink")?;
        pactl(
            &pulse,
            &["load-module", "module-remap-source", &format!("master={SINK}.monitor"), &format!("source_name={SOURCE}"),
              "source_properties=device.description=Car-microphone"],
        )
        .await
        .context("load remap source")?;
        pactl(&pulse, &["set-default-source", SOURCE]).await.context("set default source")?;
        // A meeting page that already opened a microphone (Meet's preview
        // opens it before we get here) keeps it: move those streams over.
        move_recordings(&pulse, SOURCE).await;
        let mut cmd = Command::new("pacat");
        cmd.args(["--playback", &format!("--device={SINK}"), "--format=s16be", &format!("--rate={rate}"), "--channels=1", "--raw",
                  "--latency-msec=40"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        pulse.apply(&mut cmd);
        let player = cmd.spawn().context("spawn pacat")?;
        info!("call: car microphone ready as PulseAudio source {SOURCE} ({rate} Hz mono; default source was {previous_source:?})");
        Ok(Self { pulse, player: Some(player), previous_source })
    }

    pub async fn write(&mut self, pcm_be: &[u8]) -> Result<()> {
        let stdin = self.player.as_mut().and_then(|p| p.stdin.as_mut()).context("pacat has no stdin")?;
        stdin.write_all(pcm_be).await?;
        Ok(())
    }
}

/// Always cleaned up, also when the call's future is simply dropped (the
/// meeting ended mid-read): give the default source back and remove ours.
impl Drop for CarMic {
    fn drop(&mut self) {
        if let Some(mut p) = self.player.take() {
            let _ = p.start_kill();
        }
        let pulse = self.pulse.clone();
        let prev = self.previous_source.take();
        tokio::spawn(async move {
            if let Some(prev) = prev {
                let _ = pactl(&pulse, &["set-default-source", &prev]).await;
                move_recordings(&pulse, &prev).await;
            }
            unload_ours(&pulse).await;
            info!("call: car microphone removed");
        });
    }
}

async fn pactl(pulse: &PulseEnv, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("pactl");
    cmd.args(args);
    pulse.apply(&mut cmd);
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await.context("pactl timed out")??;
    if !out.status.success() {
        anyhow::bail!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Move the microphone recordings (source-outputs reading a real input) to
/// `source`. Streams reading a `.monitor` are left alone: those are the Pi
/// capturing what it plays (our car stream, the visualizer, the remap module
/// itself) — moving them would feed the car its own microphone.
async fn move_recordings(pulse: &PulseEnv, source: &str) {
    let Ok(list) = pactl(pulse, &["list", "short", "source-outputs"]).await else { return };
    let Ok(sources) = pactl(pulse, &["list", "short", "sources"]).await else { return };
    let monitors: Vec<&str> = sources
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.get(1)?.ends_with(".monitor")).then(|| f[0])
        })
        .collect();
    for l in list.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let (Some(id), Some(src)) = (f.first(), f.get(1)) else { continue };
        if !monitors.contains(src) {
            let _ = pactl(pulse, &["move-source-output", id, source]).await;
        }
    }
}

/// Unload every module we created (matched by our sink/source names).
async fn unload_ours(pulse: &PulseEnv) {
    let Ok(list) = pactl(pulse, &["list", "short", "modules"]).await else { return };
    // Remap source first: it depends on the null sink's monitor.
    let mut ids: Vec<(bool, String)> = list
        .lines()
        .filter(|l| l.contains(SINK))
        .filter_map(|l| {
            let id = l.split('\t').next()?.to_string();
            Some((l.contains("module-remap-source"), id))
        })
        .collect();
    ids.sort_by_key(|(remap, _)| !*remap);
    for (_, id) in ids {
        let _ = pactl(pulse, &["unload-module", &id]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_messages() {
        assert_eq!(parse(b"call 1"), Some(Ctl::Call(true)));
        assert_eq!(parse(b"call 0\n"), Some(Ctl::Call(false)));
        assert_eq!(parse(b"ring 1"), Some(Ctl::Ring(true)));
        assert_eq!(parse(b" ring 0 \n"), Some(Ctl::Ring(false)));
        assert_eq!(parse(b"call"), None);
        assert_eq!(parse(b"ring 2"), None);
        assert_eq!(parse(&[0xff, 0xfe]), None);
    }

    const CALL: Line = Line { call: true, ring: false };
    const RING: Line = Line { call: false, ring: true };
    const IDLE: Line = Line { call: false, ring: false };

    #[test]
    fn ring_then_answer_hands_over_to_the_call() {
        let t0 = Instant::now();
        let mut l = LineTracker::default();
        l.on_ctl(Ctl::Ring(true), t0);
        assert_eq!(l.line(t0), RING);
        // Answered: the backend starts `call 1` and stops ringing.
        l.on_ctl(Ctl::Call(true), t0 + Duration::from_secs(2));
        l.on_ctl(Ctl::Ring(false), t0 + Duration::from_secs(2));
        assert_eq!(l.line(t0 + Duration::from_secs(2)), CALL);
        // Call heartbeats keep it up; hang-up ends it.
        l.on_ctl(Ctl::Call(true), t0 + Duration::from_secs(5));
        assert_eq!(l.line(t0 + Duration::from_secs(9)), CALL);
        l.on_ctl(Ctl::Call(false), t0 + Duration::from_secs(10));
        assert_eq!(l.line(t0 + Duration::from_secs(10)), IDLE);
    }

    #[test]
    fn ring_stops_explicitly_or_when_heartbeats_stop() {
        let t0 = Instant::now();
        let mut l = LineTracker::default();
        l.on_ctl(Ctl::Ring(true), t0);
        l.on_ctl(Ctl::Ring(false), t0 + Duration::from_secs(1));
        assert_eq!(l.line(t0 + Duration::from_secs(1)), IDLE);
        // A crashed backend: ringing alone times out after RING_TIMEOUT.
        l.on_ctl(Ctl::Ring(true), t0 + Duration::from_secs(10));
        l.on_ctl(Ctl::Ring(true), t0 + Duration::from_secs(13));
        assert_eq!(l.line(t0 + Duration::from_secs(13) + RING_TIMEOUT - Duration::from_millis(1)), RING);
        assert_eq!(l.line(t0 + Duration::from_secs(13) + RING_TIMEOUT), IDLE);
        // Once dropped it stays dropped until a new heartbeat.
        assert_eq!(l.line(t0 + Duration::from_secs(14)), IDLE);
    }

    #[test]
    fn call_times_out_like_before() {
        let t0 = Instant::now();
        let mut l = LineTracker::default();
        l.on_ctl(Ctl::Call(true), t0);
        assert_eq!(l.line(t0 + CALL_TIMEOUT - Duration::from_millis(1)), CALL);
        assert_eq!(l.line(t0 + CALL_TIMEOUT), IDLE);
    }

    #[test]
    fn ring_during_a_meeting_keeps_both_and_survives_the_meeting_end() {
        let t0 = Instant::now();
        let mut l = LineTracker::default();
        l.on_ctl(Ctl::Call(true), t0); // meeting
        l.on_ctl(Ctl::Ring(true), t0 + Duration::from_secs(1));
        assert_eq!(l.line(t0 + Duration::from_secs(1)), Line { call: true, ring: true });
        // `ring 0` must not end the meeting, `call 0` must not stop the ringing.
        l.on_ctl(Ctl::Call(false), t0 + Duration::from_secs(2));
        assert_eq!(l.line(t0 + Duration::from_secs(2)), RING);
        l.on_ctl(Ctl::Call(true), t0 + Duration::from_secs(3));
        l.on_ctl(Ctl::Ring(false), t0 + Duration::from_secs(3));
        assert_eq!(l.line(t0 + Duration::from_secs(3)), CALL);
    }

    #[test]
    fn ringtone_shape() {
        // The Jaguar's alert format is 0x800 = 44.1 kHz 16-bit stereo.
        let pcm = ringtone_cycle(44100, 2);
        assert_eq!(pcm.len(), 44100 * 3 * 2 * 2);
        let sample = |frame: usize| i16::from_be_bytes([pcm[frame * 4], pcm[frame * 4 + 1]]);
        // Starts at silence (no click when the loop wraps) and channels match.
        assert_eq!(sample(0), 0);
        assert!((0..pcm.len() / 4).all(|f| pcm[f * 4..f * 4 + 2] == pcm[f * 4 + 2..f * 4 + 4]));
        // Audible in the chime part, below -6 dBFS everywhere, silent in the tail.
        let peak = |a: usize, b: usize| (a..b).map(|f| sample(f).unsigned_abs()).max().unwrap();
        assert!(peak(0, 44100) > 6000);
        assert!(peak(0, pcm.len() / 4) < i16::MAX as u16 / 2);
        assert_eq!(peak(44100, pcm.len() / 4), 0);
        // Mono at 48 kHz is just as long.
        assert_eq!(ringtone_cycle(48000, 1).len(), 48000 * 3 * 2);
    }
}
