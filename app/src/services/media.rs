//! Media service — integrates with MPRIS via playerctl.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Once `playerctl` is found missing we stop re-spawning it every tick.
static PLAYERCTL_MISSING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Default, Serialize)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: String,
    pub is_playing: bool,
    pub player_name: String,
    /// Playback position in seconds, None when the player doesn't report one.
    pub position_sec: Option<f64>,
    /// Track length in seconds, None when the player doesn't report one.
    pub length_sec: Option<f64>,
    /// True when the player's shuffle mode is on.
    pub shuffle: bool,
}

/// Result of running an external command: what it printed, and whether it
/// actually succeeded. The exit status matters — a transport button that
/// silently fails must not be reported to the UI as `{"ok": true}`.
pub struct CmdResult {
    pub stdout: String,
    pub ok: bool,
}

pub struct MediaService {
    pub now_playing: Arc<RwLock<NowPlaying>>,
}

impl MediaService {
    pub fn new() -> Self {
        Self {
            now_playing: Arc::new(RwLock::new(NowPlaying::default())),
        }
    }

    pub async fn start(&self) {
        let np = self.now_playing.clone();
        tokio::spawn(async move {
            loop {
                // 1 s while a player exists (progress bar); 3 s otherwise, so an
                // idle dashboard does not spawn a process every second for nothing.
                let found = update_now_playing(&np).await;
                let wait = if found { 1 } else { 3 };
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        });
        info!("media service started");
    }

    /// Each control returns whether playerctl actually carried the action out.
    pub async fn play_pause(&self) -> bool {
        playerctl(&["play-pause"]).await
    }

    /// Explicit play / pause: the car's media keys send these, not a toggle.
    pub async fn play(&self) -> bool {
        playerctl(&["play"]).await
    }

    pub async fn pause(&self) -> bool {
        playerctl(&["pause"]).await
    }

    pub async fn next_track(&self) -> bool {
        playerctl(&["next"]).await
    }

    pub async fn previous_track(&self) -> bool {
        playerctl(&["previous"]).await
    }

    /// Toggle shuffle. `playerctl shuffle Toggle` flips the player's own mode.
    pub async fn toggle_shuffle(&self) -> bool {
        playerctl(&["shuffle", "Toggle"]).await
    }
}

/// One `playerctl` process per poll: status and shuffle come in the same
/// `--format` as the metadata (it used to be three processes every second).
/// Returns whether a player was found.
async fn update_now_playing(np: &Arc<RwLock<NowPlaying>>) -> bool {
    // MPRIS reports both position and mpris:length in MICROseconds; in a format
    // template shuffle prints "true"/"false".
    let meta = run_cmd(
        "playerctl",
        &[
            "metadata",
            "--format",
            "{{status}}\n{{shuffle}}\n{{title}}\n{{artist}}\n{{album}}\n{{mpris:artUrl}}\n{{playerName}}\n{{position}}\n{{mpris:length}}",
        ],
    )
    .await;
    if !meta.ok {
        // "No players found": nothing is playing.
        *np.write().await = NowPlaying::default();
        return false;
    }

    let lines: Vec<&str> = meta.stdout.lines().collect();
    let field = |i: usize| lines.get(i).unwrap_or(&"").to_string();

    *np.write().await = NowPlaying {
        is_playing: field(0).trim() == "Playing",
        shuffle: field(1).trim().eq_ignore_ascii_case("true"),
        title: field(2),
        artist: field(3),
        album: field(4),
        art_url: field(5),
        player_name: field(6),
        position_sec: micros_to_secs(lines.get(7).copied().unwrap_or("")),
        length_sec: micros_to_secs(lines.get(8).copied().unwrap_or("")),
    };
    true
}

/// Convert an MPRIS microsecond field to seconds. Empty / unparseable /
/// negative values mean "not reported" and become None rather than 0.
fn micros_to_secs(raw: &str) -> Option<f64> {
    let v: f64 = raw.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some(v / 1_000_000.0)
}

async fn playerctl(args: &[&str]) -> bool {
    run_cmd("playerctl", args).await.ok
}

async fn run_cmd(program: &str, args: &[&str]) -> CmdResult {
    // If we already learned playerctl isn't installed, don't keep spawning it.
    if PLAYERCTL_MISSING.load(Ordering::Relaxed) {
        return CmdResult { stdout: String::new(), ok: false };
    }
    match tokio::process::Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // "No players found" is the normal idle state, not an error: it
                // used to be logged three times a second (journal churn on disk).
                if stderr.trim() == "No players found" {
                    debug!("{program} {args:?}: no MPRIS player");
                } else {
                    warn!("{program} {args:?} failed ({}): {}", output.status, stderr.trim());
                }
            }
            CmdResult {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                ok: output.status.success(),
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                warn!("{program} not found — disabling MPRIS media polling");
                PLAYERCTL_MISSING.store(true, Ordering::Relaxed);
            }
            CmdResult { stdout: String::new(), ok: false }
        }
    }
}
