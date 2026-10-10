//! The YouTube Music player: one long-lived mpv and a queue owned here.
//!
//! mpv used to be started once per track (`--idle=no`), so a song played and
//! then silence: next/previous/shuffle had nothing to act on. Now a single mpv
//! runs `--idle=yes` with a JSON IPC socket, and its playlist mirrors
//! `Inner::queue` entry for entry (same order, same length), so an mpv
//! `playlist-pos` is also an index into our queue — which is how the dashboard
//! knows the exact videoId playing (for "like") and what comes next.
//!
//! Like the YouTube Music app, playing one song continues with its radio, and
//! the queue is topped up with the current track's radio when it nears its end
//! (unless repeat is on). The transport buttons still go through MPRIS
//! (mpv-mpris maps next/previous to mpv's playlist), and mpv reports the new
//! position back here, so both paths stay in step. Shuffle and repeat are done
//! here, not through mpv-mpris, because reshuffling mpv's playlist behind our
//! back would break the index mapping.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

use super::music::MusicService;

/// Start topping up with radio when this few tracks are left after the current one.
const RADIO_TOP_UP_WHEN_LEFT: usize = 2;
/// Never let the queue grow past this (a car trip, not a library).
const MAX_QUEUE: usize = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    #[serde(rename = "videoId")]
    pub video_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub art: String,
}

impl Track {
    fn url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Default)]
struct Inner {
    /// Mirrors mpv's playlist exactly.
    queue: Vec<Track>,
    /// mpv's `playlist-pos` (None when idle / nothing loaded).
    pos: Option<usize>,
    shuffle: bool,
    /// The upcoming tracks in their original order, to restore on unshuffle.
    unshuffled: Vec<Track>,
    repeat: Repeat,
    /// A radio top-up is in flight (don't start a second one).
    topping_up: bool,
    /// Playback as mpv itself reports it. mpv-mpris claims "Stopped" at
    /// position 0 while mpv plays, so the dashboard uses these instead.
    time_pos: Option<f64>,
    duration: Option<f64>,
    paused: bool,
    idle: bool,
    /// mpv command channel; None until mpv is running.
    cmd: Option<mpsc::UnboundedSender<serde_json::Value>>,
    child: Option<tokio::process::Child>,
}

/// What is playing, straight from mpv (see `Inner::time_pos`).
pub struct Playback {
    pub track: Track,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub playing: bool,
}

/// What the dashboard shows about the queue.
#[derive(Debug, Serialize)]
pub struct QueueState {
    pub current: Option<Track>,
    pub pos: Option<usize>,
    pub upcoming: Vec<Track>,
    pub length: usize,
    pub shuffle: bool,
    pub repeat: Repeat,
}

pub struct PlayerService {
    inner: Mutex<Inner>,
    music: Arc<MusicService>,
    socket: std::path::PathBuf,
}

impl PlayerService {
    pub fn new(music: Arc<MusicService>) -> Arc<Self> {
        let dir = std::env::var("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
        Arc::new(Self { inner: Mutex::new(Inner::default()), music, socket: dir.join("rascar-mpv.sock") })
    }

    /// Play `tracks` from `start`, replacing the queue (Play all, or a tap on
    /// a list entry). With a single track, its radio follows.
    pub async fn play_list(self: &Arc<Self>, mut tracks: Vec<Track>, start: usize, shuffle: bool) -> Result<()> {
        tracks.retain(|t| is_valid_video_id(&t.video_id));
        if tracks.is_empty() {
            return Err(anyhow!("nothing playable"));
        }
        let start = start.min(tracks.len() - 1);
        // The tapped track first, then the rest of the list after it.
        let mut queue: Vec<Track> = tracks.drain(start..).collect();
        queue.truncate(MAX_QUEUE);
        let single = queue.len() == 1;
        let cmd = self.ensure_mpv().await?;
        {
            let mut g = self.inner.lock().await;
            g.shuffle = false;
            g.unshuffled.clear();
            if shuffle && queue.len() > 2 {
                let first = queue.remove(0);
                g.unshuffled = queue.clone();
                shuffle_in_place(&mut queue);
                queue.insert(0, first);
                g.shuffle = true;
            }
            g.queue = queue.clone();
            g.pos = Some(0);
            send(&cmd, &["playlist-clear"]);
            send(&cmd, &["loadfile", &queue[0].url(), "replace"]);
            for t in &queue[1..] {
                send(&cmd, &["loadfile", &t.url(), "append"]);
            }
            send(&cmd, &["set_property", "pause", "no"]);
        }
        info!("player: queue of {} (from {})", queue.len(), queue[0].video_id);
        if single {
            self.top_up().await;
        }
        Ok(())
    }

    /// Play one song, then its radio.
    pub async fn play_song(self: &Arc<Self>, track: Track) -> Result<()> {
        self.play_list(vec![track], 0, false).await
    }

    /// Start the radio of `seed` (the "artist radio" / "mix" buttons).
    pub async fn play_radio(self: &Arc<Self>, seed: &str) -> Result<()> {
        let tracks = self.radio_tracks(seed).await?;
        self.play_list(tracks, 0, false).await
    }

    /// Insert `track` right after the current one, or at the end.
    pub async fn enqueue(self: &Arc<Self>, track: Track, next: bool) -> Result<()> {
        if !is_valid_video_id(&track.video_id) {
            return Err(anyhow!("malformed videoId"));
        }
        let cmd = self.ensure_mpv().await?;
        let mut g = self.inner.lock().await;
        if g.queue.len() >= MAX_QUEUE {
            return Err(anyhow!("queue is full"));
        }
        let at = if next { g.pos.map(|p| p + 1).unwrap_or(0).min(g.queue.len()) } else { g.queue.len() };
        send(&cmd, &["loadfile", &track.url(), "append"]);
        let last = g.queue.len();
        if at != last {
            // mpv appends at the end; move it into place (index semantics: before `at`).
            send_json(&cmd, serde_json::json!({ "command": ["playlist-move", last, at] }));
        }
        if g.pos.is_none() {
            send_json(&cmd, serde_json::json!({ "command": ["playlist-play-index", at] }));
        }
        g.queue.insert(at, track);
        Ok(())
    }

    /// Shuffle / unshuffle what comes after the current track.
    pub async fn set_shuffle(self: &Arc<Self>, on: bool) -> Result<()> {
        let cmd = self.ensure_mpv().await?;
        let mut g = self.inner.lock().await;
        if g.shuffle == on {
            return Ok(());
        }
        let cur = g.pos.unwrap_or(0);
        let head = cur + 1;
        if head >= g.queue.len() {
            g.shuffle = on;
            return Ok(());
        }
        let mut upcoming: Vec<Track> = g.queue.split_off(head);
        if on {
            g.unshuffled = upcoming.clone();
            shuffle_in_place(&mut upcoming);
        } else if !g.unshuffled.is_empty() {
            // Restore the original order, keeping anything added since.
            let mut restored = std::mem::take(&mut g.unshuffled);
            restored.retain(|t| upcoming.contains(t));
            for t in &upcoming {
                if !restored.contains(t) {
                    restored.push(t.clone());
                }
            }
            upcoming = restored;
        }
        // Rebuild mpv's tail in the new order: remove it, append again.
        for _ in 0..upcoming.len() {
            send_json(&cmd, serde_json::json!({ "command": ["playlist-remove", head] }));
        }
        for t in &upcoming {
            send(&cmd, &["loadfile", &t.url(), "append"]);
        }
        g.queue.extend(upcoming);
        g.shuffle = on;
        Ok(())
    }

    pub async fn toggle_shuffle(self: &Arc<Self>) -> Result<bool> {
        let on = !self.inner.lock().await.shuffle;
        self.set_shuffle(on).await?;
        Ok(on)
    }

    pub async fn set_repeat(self: &Arc<Self>, mode: Repeat) -> Result<()> {
        let cmd = self.ensure_mpv().await?;
        let (file, list) = match mode {
            Repeat::Off => ("no", "no"),
            Repeat::All => ("no", "inf"),
            Repeat::One => ("inf", "no"),
        };
        send(&cmd, &["set_property", "loop-file", file]);
        send(&cmd, &["set_property", "loop-playlist", list]);
        self.inner.lock().await.repeat = mode;
        Ok(())
    }

    /// Cycle repeat off → all → one → off (the button).
    pub async fn cycle_repeat(self: &Arc<Self>) -> Result<Repeat> {
        let next = match self.inner.lock().await.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        self.set_repeat(next).await?;
        Ok(next)
    }

    pub async fn seek(self: &Arc<Self>, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(anyhow!("bad position"));
        }
        let cmd = self.inner.lock().await.cmd.clone().ok_or_else(|| anyhow!("nothing playing"))?;
        send_json(&cmd, serde_json::json!({ "command": ["seek", seconds, "absolute"] }));
        Ok(())
    }

    /// Jump to an entry of the queue (tap in "up next").
    pub async fn jump(self: &Arc<Self>, index: usize) -> Result<()> {
        let g = self.inner.lock().await;
        let cmd = g.cmd.clone().ok_or_else(|| anyhow!("nothing playing"))?;
        if index >= g.queue.len() {
            return Err(anyhow!("no such entry"));
        }
        send_json(&cmd, serde_json::json!({ "command": ["playlist-play-index", index] }));
        Ok(())
    }

    /// The videoId playing right now, if it is ours.
    pub async fn current_video_id(&self) -> Option<String> {
        let g = self.inner.lock().await;
        g.cmd.as_ref()?;
        g.pos.and_then(|p| g.queue.get(p)).map(|t| t.video_id.clone())
    }

    /// The track playing now with mpv's own position/state, if mpv plays one.
    pub async fn playback(&self) -> Option<Playback> {
        let g = self.inner.lock().await;
        g.cmd.as_ref()?;
        if g.idle {
            return None;
        }
        let track = g.pos.and_then(|p| g.queue.get(p)).cloned()?;
        Some(Playback { track, position: g.time_pos, duration: g.duration, playing: !g.paused })
    }

    pub async fn state(&self) -> QueueState {
        let g = self.inner.lock().await;
        let current = g.pos.and_then(|p| g.queue.get(p)).cloned();
        let upcoming = g.pos.map(|p| g.queue.iter().skip(p + 1).take(50).cloned().collect()).unwrap_or_default();
        QueueState { current, pos: g.pos, upcoming, length: g.queue.len(), shuffle: g.shuffle, repeat: g.repeat }
    }

    // --- internals -----------------------------------------------------

    async fn radio_tracks(&self, seed: &str) -> Result<Vec<Track>> {
        let v = self.music.radio(seed).await?;
        let tracks: Vec<Track> = serde_json::from_value(v["tracks"].clone()).context("radio: unexpected answer")?;
        Ok(tracks.into_iter().filter(|t| is_valid_video_id(&t.video_id)).collect())
    }

    /// Append the current track's radio when few tracks are left (autoplay).
    async fn top_up(self: &Arc<Self>) {
        let seed = {
            let mut g = self.inner.lock().await;
            let Some(pos) = g.pos else { return };
            let left = g.queue.len().saturating_sub(pos + 1);
            if g.topping_up || g.repeat != Repeat::Off || left > RADIO_TOP_UP_WHEN_LEFT || g.queue.len() >= MAX_QUEUE {
                return;
            }
            g.topping_up = true;
            g.queue.last().map(|t| t.video_id.clone())
        };
        let Some(seed) = seed else { return };
        let me = self.clone();
        tokio::spawn(async move {
            let result = me.radio_tracks(&seed).await;
            let mut g = me.inner.lock().await;
            g.topping_up = false;
            let (Ok(tracks), Some(cmd)) = (result, g.cmd.clone()) else {
                warn!("player: radio top-up for {seed} failed");
                return;
            };
            let mut added = 0;
            for t in tracks {
                if g.queue.len() >= MAX_QUEUE {
                    break;
                }
                // The radio repeats its seed and often recent tracks: skip what is queued.
                if g.queue.iter().any(|q| q.video_id == t.video_id) {
                    continue;
                }
                send(&cmd, &["loadfile", &t.url(), "append"]);
                g.queue.push(t);
                added += 1;
            }
            info!("player: radio from {seed} added {added} tracks");
        });
    }

    /// The mpv command channel, starting mpv (and its IPC reader) if needed.
    async fn ensure_mpv(self: &Arc<Self>) -> Result<mpsc::UnboundedSender<serde_json::Value>> {
        let mut g = self.inner.lock().await;
        let alive = g.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if let Some(cmd) = g.cmd.as_ref().filter(|c| alive && !c.is_closed()) {
            return Ok(cmd.clone());
        }
        if !self.music.enabled().await {
            return Err(anyhow!("YouTube Music is disabled in settings"));
        }
        let _ = std::fs::remove_file(&self.socket);
        let child = tokio::process::Command::new("mpv")
            .args([
                "--no-video",
                "--no-terminal",
                "--idle=yes",
                "--prefetch-playlist=yes",
                "--ytdl-format=bestaudio/best",
                &format!("--input-ipc-server={}", self.socket.display()),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("start mpv")?;
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(&self.socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stream = stream.ok_or_else(|| anyhow!("mpv IPC socket never appeared"))?;
        let (rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<serde_json::Value>();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                let mut line = v.to_string();
                line.push('\n');
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                if v["event"] != "property-change" {
                    continue;
                }
                match v["name"].as_str() {
                    Some("playlist-pos") => {
                        let pos = v["data"].as_i64().filter(|p| *p >= 0).map(|p| p as usize);
                        {
                            let mut g = me.inner.lock().await;
                            g.pos = pos;
                            g.time_pos = None;
                            g.duration = None;
                        }
                        me.top_up().await;
                    }
                    Some("time-pos") => me.inner.lock().await.time_pos = v["data"].as_f64(),
                    Some("duration") => me.inner.lock().await.duration = v["data"].as_f64(),
                    Some("pause") => me.inner.lock().await.paused = v["data"].as_bool().unwrap_or(false),
                    Some("idle-active") => me.inner.lock().await.idle = v["data"].as_bool().unwrap_or(false),
                    _ => {}
                }
            }
            // mpv exited or the socket closed: the next command restarts it.
            let mut g = me.inner.lock().await;
            g.cmd = None;
            g.pos = None;
            info!("player: mpv session ended");
        });
        for (id, prop) in [(1, "playlist-pos"), (2, "time-pos"), (3, "duration"), (4, "pause"), (5, "idle-active")] {
            send_json(&tx, serde_json::json!({ "command": ["observe_property", id, prop] }));
        }
        g.queue.clear();
        g.pos = None;
        g.cmd = Some(tx.clone());
        g.child = Some(child);
        info!("player: mpv started (IPC {})", self.socket.display());
        Ok(tx)
    }
}

fn send(cmd: &mpsc::UnboundedSender<serde_json::Value>, args: &[&str]) {
    send_json(cmd, serde_json::json!({ "command": args }));
}

fn send_json(cmd: &mpsc::UnboundedSender<serde_json::Value>, v: serde_json::Value) {
    let _ = cmd.send(v);
}

/// Fisher–Yates with a clock-seeded xorshift (no rand dependency for this).
fn shuffle_in_place<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let j = (x % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

/// YouTube video ids are short base64url tokens; anything else never reaches mpv.
pub fn is_valid_video_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_ids_are_validated() {
        assert!(is_valid_video_id("dQw4w9WgXcQ"));
        assert!(!is_valid_video_id(""));
        assert!(!is_valid_video_id("a b"));
        assert!(!is_valid_video_id("--script=/tmp/x"));
    }

    #[test]
    fn shuffle_keeps_every_item() {
        let mut v: Vec<u32> = (0..50).collect();
        shuffle_in_place(&mut v);
        let mut s = v.clone();
        s.sort();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn tracks_parse_from_the_helper_shape() {
        let v = serde_json::json!([{ "videoId": "dQw4w9WgXcQ", "title": "T", "artist": "A", "album": "", "art": "u" }]);
        let t: Vec<Track> = serde_json::from_value(v).unwrap();
        assert_eq!(t[0].url(), "https://music.youtube.com/watch?v=dQw4w9WgXcQ");
    }
}
