//! Spectrum of whatever the Pi plays, for the dashboard's visualizer.
//!
//! The page's visualizer used to analyse its own `<audio>` element only, so it
//! was blind to YouTube Music (mpv) and anything else. AndroidAuto2 analysed
//! the phone's whole output; this is the same on Linux: record the default
//! sink's monitor (`parec`), FFT it, and push `{"type":"viz","f":[128],"w":[128]}`
//! over the dashboard WebSocket about 30 times a second. The two arrays have
//! the shape the 16 drawing modes already use (Web Audio's
//! `getByteFrequencyData` / `getByteTimeDomainData`, 0..255), so the modes are
//! unchanged. Nothing is sent while the output is silent or nobody listens.

use std::f32::consts::PI;

use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use tracing::{info, warn};

const RATE: u32 = 48_000;
const FFT: usize = 2048;
/// One frame every HOP samples: 48000 / 1600 = 30 fps.
const HOP: usize = 1600;
const BANDS: usize = 128;
const LOW_HZ: f32 = 40.0;
const HIGH_HZ: f32 = 16_000.0;
/// Bars show the last RANGE_DB below a ceiling that follows the music's own
/// loudness. A fixed -72..-18 dBFS window put loud, compressed radio in its
/// top third: measured on two pop/rock stations, the bars never went
/// below ~45-65 % height and barely moved (frame-to-frame motion 9/255).
/// With this auto-gain the same audio spans the whole height (motion 20/255).
const RANGE_DB: f32 = 30.0;
/// The ceiling tracks the 95th-percentile band of each frame: it rises with a
/// ~0.3 s time constant (so a beat overshoots it and hits the top) and falls
/// by CEIL_RELEASE_DB per second in quieter passages...
const CEIL_ATTACK: f32 = 0.1;
const CEIL_RELEASE_DB: f32 = 4.0;
/// ...but never below this, or a quiet intro would blow noise up to full height.
const CEIL_MIN_DB: f32 = -50.0;
/// Nor more than this below the loudest band: on a sparse spectrum (a lone
/// tone) the 95th percentile is near silence and window leakage filled the
/// neighbouring bars. On music it never binds.
const CEIL_HEADROOM_DB: f32 = 12.0;
/// Music falls ~3 dB per octave (pink): without this tilt the top third of
/// the bars stayed low whatever played.
const TILT_DB_PER_OCTAVE: f32 = 3.0;
/// Fast rise, quicker fall than before (0.85 held the bars up between beats).
const RISE: f32 = 0.35;
const FALL: f32 = 0.7;
const FPS: f32 = RATE as f32 / HOP as f32;
/// RMS below this (of 1.0 full scale) counts as silence.
const SILENCE_RMS: f32 = 0.002;

pub fn spawn(events: broadcast::Sender<String>) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&events).await {
                warn!("visualizer: {e:#}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

async fn monitor_source() -> anyhow::Result<String> {
    if let Ok(s) = std::env::var("RASCAR_VIZ_SOURCE") {
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let out = tokio::process::Command::new("pactl").arg("get-default-sink").output().await?;
    let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // At boot the default is PulseAudio's placeholder until the card appears;
    // listening to `auto_null.monitor` ended up on the microphone (2026-09-28).
    if sink.is_empty() || sink == "auto_null" {
        anyhow::bail!("sound output not ready yet ({sink:?})");
    }
    Ok(format!("{sink}.monitor"))
}

async fn run(events: &broadcast::Sender<String>) -> anyhow::Result<()> {
    let source = monitor_source().await?;
    let mut child = tokio::process::Command::new("parec")
        .args(["-d", &source, "--format=s16le", &format!("--rate={RATE}"), "--channels=1", "--raw", "--latency-msec=30"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut out = child.stdout.take().ok_or_else(|| anyhow::anyhow!("parec has no stdout"))?;
    info!("visualizer: listening to {source}");
    let mut an = Analyzer::new();
    let mut ring = vec![0f32; FFT];
    let mut buf = vec![0u8; HOP * 2];
    let mut check = tokio::time::interval(std::time::Duration::from_secs(10));
    check.tick().await;
    loop {
        tokio::select! {
            r = out.read_exact(&mut buf) => { r?; }
            _ = check.tick() => {
                // Follow the default output if it changes (a new card, a Bluetooth speaker).
                if monitor_source().await.ok().as_deref() != Some(source.as_str()) {
                    anyhow::bail!("default output changed — re-attaching");
                }
                continue;
            }
        }
        // Slide the window by HOP new samples.
        ring.copy_within(HOP.., 0);
        for (i, ch) in buf.chunks_exact(2).enumerate() {
            ring[FFT - HOP + i] = i16::from_le_bytes([ch[0], ch[1]]) as f32 / 32768.0;
        }
        if events.receiver_count() == 0 {
            continue;
        }
        if let Some((f, w)) = an.frame(&ring) {
            let msg = serde_json::json!({ "type": "viz", "f": f, "w": w });
            let _ = events.send(msg.to_string());
        }
    }
}

pub struct Analyzer {
    window: Vec<f32>,
    /// FFT bin range for each band.
    bands: Vec<(usize, usize)>,
    smooth: Vec<f32>,
    /// Per-band dB offset of the pink tilt.
    tilt: Vec<f32>,
    /// Auto-gain ceiling in dBFS (None until the first frame).
    ceil: Option<f32>,
    /// Frames of silence in a row (stop sending after a short tail).
    silent: u32,
}

impl Analyzer {
    pub fn new() -> Self {
        let window = (0..FFT).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (FFT - 1) as f32).cos()).collect();
        let bin_hz = RATE as f32 / FFT as f32;
        let bands = (0..BANDS)
            .map(|b| {
                let f0 = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(b as f32 / BANDS as f32);
                let f1 = LOW_HZ * (HIGH_HZ / LOW_HZ).powf((b + 1) as f32 / BANDS as f32);
                let lo = (f0 / bin_hz).floor() as usize;
                let hi = ((f1 / bin_hz).ceil() as usize).max(lo + 1).min(FFT / 2);
                (lo.min(FFT / 2 - 1), hi)
            })
            .collect();
        let tilt = (0..BANDS)
            .map(|b| {
                let centre = LOW_HZ * (HIGH_HZ / LOW_HZ).powf((b as f32 + 0.5) / BANDS as f32);
                TILT_DB_PER_OCTAVE * (centre / 1000.0).log2()
            })
            .collect();
        Self { window, bands, smooth: vec![0.0; BANDS], tilt, ceil: None, silent: 0 }
    }

    /// Spectrum + waveform of one window, or None when there is nothing to show.
    pub fn frame(&mut self, samples: &[f32]) -> Option<(Vec<u8>, Vec<u8>)> {
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        if rms < SILENCE_RMS {
            self.silent += 1;
            // A few frames of tail so the bars fall instead of freezing.
            if self.silent > 15 {
                return None;
            }
        } else {
            self.silent = 0;
        }
        let mut re: Vec<f32> = samples.iter().zip(&self.window).map(|(s, w)| s * w).collect();
        let mut im = vec![0f32; FFT];
        fft(&mut re, &mut im);
        let norm = 2.0 / FFT as f32 * 2.0; // one-sided, Hann coherent gain 0.5
        let db: Vec<f32> = self
            .bands
            .iter()
            .zip(&self.tilt)
            .map(|(&(lo, hi), t)| {
                let peak = (lo..hi).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() * norm).fold(0.0f32, f32::max);
                20.0 * peak.max(1e-9).log10() + t
            })
            .collect();
        let mut sorted = db.clone();
        sorted.sort_by(f32::total_cmp);
        let top = sorted[BANDS * 95 / 100].max(sorted[BANDS - 1] - CEIL_HEADROOM_DB);
        let ceil = match self.ceil {
            Some(c) if top > c => c + (top - c) * CEIL_ATTACK,
            Some(c) => c - CEIL_RELEASE_DB / FPS,
            None => top,
        }
        .max(CEIL_MIN_DB);
        self.ceil = Some(ceil);
        let mut f = Vec::with_capacity(BANDS);
        for (b, d) in db.iter().enumerate() {
            let v = ((d - (ceil - RANGE_DB)) / RANGE_DB).clamp(0.0, 1.0) * 255.0;
            let k = if v > self.smooth[b] { RISE } else { FALL };
            self.smooth[b] = k * self.smooth[b] + (1.0 - k) * v;
            f.push(self.smooth[b].round() as u8);
        }
        // Waveform: 128 points over the newest 1024 samples, 128 = silence.
        let tail = &samples[samples.len() - 1024..];
        let w = (0..BANDS).map(|i| ((tail[i * 8] * 127.0) + 128.0).clamp(0.0, 255.0) as u8).collect();
        Some((f, w))
    }
}

/// In-place iterative radix-2 FFT (len must be a power of two).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, amp: f32) -> Vec<f32> {
        (0..FFT).map(|i| amp * (2.0 * PI * hz * i as f32 / RATE as f32).sin()).collect()
    }

    #[test]
    fn a_tone_lights_up_its_band_only() {
        let mut a = Analyzer::new();
        a.smooth = vec![0.0; BANDS];
        // Several frames so the smoothing settles.
        let mut f = vec![];
        for _ in 0..30 {
            f = a.frame(&tone(1000.0, 0.5)).unwrap().0;
        }
        let top = f.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
        let expected = (BANDS as f32 * (1000.0f32 / LOW_HZ).ln() / (HIGH_HZ / LOW_HZ).ln()) as usize;
        assert!((top as i32 - expected as i32).abs() <= 1, "peak band {top}, expected ~{expected}");
        assert!(f[top] > 200, "loud tone should be near the top: {}", f[top]);
        assert!(f[10] < 40 && f[120] < 40, "far bands stay low: {} {}", f[10], f[120]);
    }

    /// A two-tone "mix" whose loud part is a 60 Hz bass, at a given level.
    fn mix(amp: f32) -> Vec<f32> {
        (0..FFT)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                amp * (0.8 * (2.0 * PI * 60.0 * t).sin() + 0.2 * (2.0 * PI * 2000.0 * t).sin())
            })
            .collect()
    }

    #[test]
    fn auto_gain_shows_loud_and_quiet_masters_alike() {
        // The fixed window pinned loud radio near the top; with the auto-gain
        // the same music 12 dB louder settles on the same bars.
        let settle = |amp: f32| {
            let mut a = Analyzer::new();
            let mut f = vec![];
            for _ in 0..200 {
                f = a.frame(&mix(amp)).unwrap().0;
            }
            f
        };
        let (quiet, loud) = (settle(0.1), settle(0.4));
        let diff = quiet.iter().zip(&loud).map(|(q, l)| (*q as i32 - *l as i32).abs()).max().unwrap();
        assert!(diff <= 8, "bars differ by {diff} between -12 dB and 0 dB masters");
        // And not everything is pinned: the loudest band is high, silent bands are down.
        assert!(*loud.iter().max().unwrap() > 200);
        assert!(loud.iter().filter(|v| **v < 40).count() > BANDS / 2);
    }

    #[test]
    fn silence_stops_after_a_short_tail() {
        let mut a = Analyzer::new();
        let quiet = vec![0f32; FFT];
        let sent = (0..40).filter(|_| a.frame(&quiet).is_some()).count();
        assert_eq!(sent, 15);
    }

    #[test]
    fn waveform_is_centred_on_128() {
        let mut a = Analyzer::new();
        let (_, w) = a.frame(&tone(440.0, 0.5)).unwrap();
        let mean = w.iter().map(|v| *v as f32).sum::<f32>() / w.len() as f32;
        assert!((mean - 128.0).abs() < 12.0, "{mean}");
    }
}
