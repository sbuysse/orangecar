//! WiFi signal strength — reads /proc/net/wireless every 5s

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

/// Once `iwconfig` is found missing we stop re-spawning it every tick.
static IWCONFIG_MISSING: AtomicBool = AtomicBool::new(false);

pub struct SignalService {
    pub signal_dbm: Arc<RwLock<i32>>,
}

impl SignalService {
    pub fn new() -> Self {
        Self {
            signal_dbm: Arc::new(RwLock::new(-100)),
        }
    }

    pub async fn start(&self) {
        let signal_dbm = self.signal_dbm.clone();
        tokio::spawn(async move {
            loop {
                if let Some(dbm) = read_signal_dbm().await {
                    *signal_dbm.write().await = dbm;
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }
}

async fn read_signal_dbm() -> Option<i32> {
    // Try /proc/net/wireless first
    if let Ok(data) = tokio::fs::read_to_string("/proc/net/wireless").await {
        for line in data.lines().skip(2) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                // Column 3 is signal level (may have trailing dot)
                let raw = parts[3].trim_end_matches('.');
                if let Ok(val) = raw.parse::<i32>() {
                    // Values > 0 are in 0-255 range (link quality), convert to dBm approx
                    let dbm = if val > 0 { val - 256 } else { val };
                    return Some(dbm);
                }
            }
        }
    }

    // Fallback: parse iwconfig output (skip if we know it's not installed).
    if IWCONFIG_MISSING.load(Ordering::Relaxed) {
        return None;
    }
    match tokio::process::Command::new("iwconfig")
        .output()
        .await
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(pos) = line.find("Signal level=") {
                    let rest = &line[pos + 13..];
                    let dbm_str: String = rest.chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '-')
                        .collect();
                    if let Ok(v) = dbm_str.parse::<i32>() {
                        return Some(v);
                    }
                }
            }
            None
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                warn!("iwconfig not found — disabling iwconfig signal fallback");
                IWCONFIG_MISSING.store(true, Ordering::Relaxed);
            } else {
                warn!("iwconfig failed: {e}");
            }
            None
        }
    }
}
