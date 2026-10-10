//! System volume control — reads and sets via amixer/pactl.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

/// Once a backend tool is found missing we stop re-spawning it every tick.
static PACTL_MISSING: AtomicBool = AtomicBool::new(false);
static AMIXER_MISSING: AtomicBool = AtomicBool::new(false);

/// ALSA simple-control name used for get/set. Overridable via
/// `RASCAR_MIXER_CONTROL` because it is `Master` on most desktops but `PCM`,
/// `Headphone` or `Digital` on many Pi audio setups — a wrong name makes every
/// amixer call fail.
fn mixer_control() -> &'static str {
    static CONTROL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CONTROL.get_or_init(|| {
        std::env::var("RASCAR_MIXER_CONTROL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Master".to_string())
    })
}

pub struct VolumeService {
    pub volume_pct: Arc<RwLock<u8>>,
}

impl VolumeService {
    pub fn new() -> Self {
        Self {
            volume_pct: Arc::new(RwLock::new(70)),
        }
    }

    pub async fn start(&self) {
        let vol = self.volume_pct.clone();
        tokio::spawn(async move {
            loop {
                if let Some(pct) = read_volume().await {
                    *vol.write().await = pct;
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    /// Set the mixer volume. Returns `Err(reason)` when the mixer was not
    /// actually changed — the cached percentage is only updated on success, so
    /// the UI slider can't drift away from the real hardware state (e.g. on a
    /// Pi whose control is `PCM`, not `Master`).
    pub async fn set_volume(&self, pct: u8) -> Result<u8, String> {
        let pct = pct.min(100);

        // Skip spawning amixer if we've already learned it isn't installed.
        if AMIXER_MISSING.load(Ordering::Relaxed) {
            return Err("amixer not available".to_string());
        }
        let output = tokio::process::Command::new("amixer")
            .args(["set", mixer_control(), &format!("{}%", pct)])
            .kill_on_drop(true)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                *self.volume_pct.write().await = pct;
                Ok(pct)
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                warn!("amixer set {} {pct}% failed ({}): {stderr}", mixer_control(), out.status);
                Err(if stderr.is_empty() {
                    format!("amixer exited with {}", out.status)
                } else {
                    stderr
                })
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    warn!("amixer not found — disabling amixer volume control");
                    AMIXER_MISSING.store(true, Ordering::Relaxed);
                }
                Err(format!("failed to run amixer: {e}"))
            }
        }
    }
}

async fn read_volume() -> Option<u8> {
    // Try pactl first (skip if we already know it's not installed).
    if !PACTL_MISSING.load(Ordering::Relaxed) {
        match tokio::process::Command::new("pactl")
            .args(["get-sink-volume", "@DEFAULT_SINK@"])
            .output()
            .await
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                for word in text.split_whitespace() {
                    if word.ends_with('%') {
                        if let Ok(v) = word.trim_end_matches('%').parse::<u8>() {
                            return Some(v.min(100));
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    warn!("pactl not found — disabling pactl volume polling");
                    PACTL_MISSING.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    // Fallback: amixer (skip if we already know it's not installed).
    if !AMIXER_MISSING.load(Ordering::Relaxed) {
        match tokio::process::Command::new("amixer")
            .args(["get", mixer_control()])
            .output()
            .await
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                for word in text.split_whitespace() {
                    if word.starts_with('[') && word.ends_with("%]") {
                        let pct_str = word.trim_matches(|c| c == '[' || c == ']' || c == '%');
                        if let Ok(v) = pct_str.parse::<u8>() {
                            return Some(v.min(100));
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    warn!("amixer not found — disabling amixer volume polling");
                    AMIXER_MISSING.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    None
}
