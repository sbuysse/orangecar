//! Resume the last media session after a reboot.
//!
//! The car cuts power without warning, so the last thing the driver was
//! listening to is written to `~/.config/rascar/last_media.json` (NVMe, atomic
//! replace + fsync, like settings.json) whenever it changes: a radio station,
//! a TV channel, or the YouTube Music queue (current track + what comes next,
//! position, repeat). A pause/stop by the user is recorded as `stopped`, and a
//! stopped session is never resumed.
//!
//! After a boot, once the internet answers:
//! - YouTube Music plays in our own mpv, so the backend resumes it itself.
//! - Radio and TV play in the dashboard page (an `<audio>` / `<video>`), so the
//!   page asks `POST /api/resume/claim` at load and plays what it is handed. The
//!   claim answers "retry in N s" while offline, which is how the page follows
//!   the network coming up without its own connectivity logic.
//!
//! Loop guards: every attempt is counted on disk BEFORE it starts (so a stream
//! that crashes the player or the browser is not retried forever across
//! boots), at most `MAX_ATTEMPTS` attempts, at least `MIN_SPACING` apart, and
//! the counter only goes back to 0 when playback is confirmed or the user picks
//! something. Age limits: see `AUDIO_MAX_AGE` / `TV_MAX_AGE`.
//!
//! Volume is not stored here: PulseAudio's module-device-restore already
//! persists the sink volume (`~/.config/pulse/*-device-volumes.tdb`), and
//! setting it again at boot would fight it and the car's own volume knob.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::music::MusicService;
use super::player::{PlayerService, Repeat, Track};

/// Give up after this many resume attempts of the same session.
pub const MAX_ATTEMPTS: u32 = 3;
/// Radio / YouTube Music older than this is not resumed. 72 h covers the
/// longest normal gap between two drives (Friday evening to Monday morning);
/// a car radio resumes its last source whatever the gap, and the only cost of
/// a stale resume is a sound the driver can stop with one tap.
pub const AUDIO_MAX_AGE: Duration = Duration::from_secs(72 * 3600);
/// TV is only resumed after a short stop (refuelling, a kiosk restart): it
/// floats a video window over every page, including navigation, and streams
/// video over the metered 4G. Hours later it would be an unwelcome surprise.
pub const TV_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// Minimum time between two attempts (lets a flaky link settle).
const MIN_SPACING: Duration = Duration::from_secs(20);
/// While playing, a position-only change is persisted at most this often.
const POSITION_PERSIST_EVERY: Duration = Duration::from_secs(30);
/// Heartbeats of a browser source are persisted at most this often.
const ALIVE_PERSIST_EVERY: Duration = Duration::from_secs(300);
/// The YouTube Music queue kept for a resume (the current track first).
const MAX_SAVED_QUEUE: usize = 51;
/// Host whose reachability means "online" (DNS + a route + TCP).
const PROBE_HOST: &str = "music.youtube.com:443";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    None,
    Radio,
    Tv,
    Ytm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Station {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct YtmSession {
    /// The current track first, then what was queued after it.
    pub queue: Vec<Track>,
    /// Position in the current track, seconds.
    pub position: Option<f64>,
    pub repeat: Repeat,
}

/// The persisted session (`last_media.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LastMedia {
    pub source: Source,
    /// Paused / stopped by the user: never resumed.
    pub stopped: bool,
    /// Unix seconds when the session was last known to be playing (or picked).
    pub updated: u64,
    /// Resume attempts since playback was last confirmed.
    pub attempts: u32,
    pub radio: Option<Station>,
    pub tv: Option<Station>,
    pub ytm: Option<YtmSession>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Whether `last` should be resumed at `now`, or why not. A timestamp in the
/// future (clock not yet set after boot) counts as fresh: failing open only
/// costs a resume, failing closed would make the feature look broken.
pub fn due(last: &LastMedia, now: u64) -> Result<(), &'static str> {
    let max_age = match last.source {
        Source::None => return Err("nothing was playing"),
        Source::Radio if last.radio.is_none() => return Err("no station"),
        Source::Tv if last.tv.is_none() => return Err("no channel"),
        Source::Ytm if last.ytm.as_ref().is_none_or(|y| y.queue.is_empty()) => return Err("empty queue"),
        Source::Tv => TV_MAX_AGE,
        Source::Radio | Source::Ytm => AUDIO_MAX_AGE,
    };
    if last.stopped {
        return Err("stopped by the user");
    }
    if last.attempts >= MAX_ATTEMPTS {
        return Err("gave up after repeated failures");
    }
    if now.saturating_sub(last.updated) > max_age.as_secs() {
        return Err("too old");
    }
    Ok(())
}

/// What `POST /api/resume/claim` answers.
#[derive(Debug, Serialize, PartialEq)]
pub struct Claim {
    /// Play this now (radio / tv).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<ClaimItem>,
    /// Ask again in this many seconds (offline, or too soon after an attempt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in: Option<u64>,
    /// Why nothing is handed out (diagnostics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ClaimItem {
    pub source: Source,
    pub name: String,
    pub url: String,
    pub attempt: u32,
}

/// What the page reports (`POST /api/resume/report`).
#[derive(Debug, Deserialize)]
pub struct Report {
    /// "play" (user picked it), "stop" (user stopped it), "ok" (a resume is
    /// confirmed playing), "alive" (still playing), "fail" (a resume failed).
    pub event: String,
    pub source: Source,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

struct Inner {
    last: LastMedia,
    /// When `last` was last written to disk.
    persisted_at: Option<Instant>,
    /// When the last resume attempt started (this process).
    last_attempt: Option<Instant>,
    /// Bumped on every user choice: an in-flight resume that sees it change
    /// stops, the driver has taken over.
    generation: u64,
    /// Cached connectivity probe.
    online: Option<(Instant, bool)>,
}

pub struct ResumeService {
    inner: Mutex<Inner>,
    path: PathBuf,
    player: Arc<PlayerService>,
    music: Arc<MusicService>,
}

impl ResumeService {
    pub fn new(player: Arc<PlayerService>, music: Arc<MusicService>) -> Arc<Self> {
        let path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root")).join(".config/rascar/last_media.json");
        Self::with_path(path, player, music)
    }

    fn with_path(path: PathBuf, player: Arc<PlayerService>, music: Arc<MusicService>) -> Arc<Self> {
        let last: LastMedia = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                warn!("resume: unreadable {}: {e}", path.display());
                LastMedia::default()
            }),
            Err(_) => LastMedia::default(),
        };
        let verdict = due(&last, now_secs());
        info!(
            "resume: last session {:?} stopped={} attempts={} age={} -> {}",
            last.source,
            last.stopped,
            last.attempts,
            if last.updated == 0 { "-".to_string() } else { format!("{}s", now_secs().saturating_sub(last.updated)) },
            verdict.err().unwrap_or("will resume once online")
        );
        Arc::new(Self {
            inner: Mutex::new(Inner { last, persisted_at: None, last_attempt: None, generation: 0, online: None }),
            path,
            player,
            music,
        })
    }

    /// Background work: the YouTube Music snapshot poller and, if the last
    /// session was YouTube Music, its resume.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move { me.poll_ytm().await });
        let me = self.clone();
        tokio::spawn(async move { me.resume_ytm().await });
    }

    /// The routes of this service, mergeable into any router.
    pub fn routes<S: Clone + Send + Sync + 'static>(self: &Arc<Self>) -> Router<S> {
        Router::new()
            .route("/api/resume", get(api_state))
            .route("/api/resume/claim", post(api_claim))
            .route("/api/resume/report", post(api_report))
            .with_state(self.clone())
    }

    /// The user started YouTube Music (a song, a list, a radio).
    pub async fn user_started_ytm(&self) {
        let snapshot = self.ytm_snapshot().await;
        let mut g = self.inner.lock().await;
        g.generation += 1;
        g.last = LastMedia {
            source: Source::Ytm,
            stopped: false,
            updated: now_secs(),
            attempts: 0,
            ytm: snapshot.or_else(|| g.last.ytm.clone()),
            ..g.last.clone()
        };
        self.persist(&mut g).await;
    }

    /// Answer a claim from the page (radio / TV live there).
    pub async fn claim(&self) -> Claim {
        let nothing = |reason| Claim { resume: None, retry_in: None, reason: Some(reason) };
        let snapshot = self.inner.lock().await.last.clone();
        if !matches!(snapshot.source, Source::Radio | Source::Tv) {
            return nothing(if snapshot.source == Source::Ytm { "resumed by the backend" } else { "nothing was playing" });
        }
        if let Err(why) = due(&snapshot, now_secs()) {
            return nothing(why);
        }
        if !self.online().await {
            return Claim { resume: None, retry_in: Some(5), reason: Some("offline") };
        }
        let mut g = self.inner.lock().await;
        // Re-check under the lock: the user may have acted while probing.
        if g.last != snapshot {
            return Claim { resume: None, retry_in: Some(1), reason: Some("changed") };
        }
        if let Some(t) = g.last_attempt {
            let since = t.elapsed();
            if since < MIN_SPACING {
                return Claim { resume: None, retry_in: Some((MIN_SPACING - since).as_secs().max(1)), reason: Some("too soon") };
            }
        }
        g.last.attempts += 1;
        g.last_attempt = Some(Instant::now());
        let attempt = g.last.attempts;
        self.persist(&mut g).await;
        let station = if g.last.source == Source::Radio { g.last.radio.clone() } else { g.last.tv.clone() }.unwrap_or_default();
        info!("resume: handing {:?} '{}' to the page (attempt {attempt}/{MAX_ATTEMPTS})", g.last.source, station.name);
        Claim {
            resume: Some(ClaimItem { source: g.last.source, name: station.name, url: station.url, attempt }),
            retry_in: None,
            reason: None,
        }
    }

    /// A report from the page about its own sources.
    pub async fn report(&self, r: Report) -> Result<(), &'static str> {
        if !matches!(r.source, Source::Radio | Source::Tv) {
            return Err("source must be radio or tv");
        }
        let mut g = self.inner.lock().await;
        let current = g.last.source == r.source;
        match r.event.as_str() {
            "play" => {
                let ok_url = (r.url.starts_with("http://") || r.url.starts_with("https://")) && r.url.len() <= 2048;
                if !ok_url || r.name.len() > 200 {
                    return Err("bad station");
                }
                let station = Some(Station { name: r.name, url: r.url });
                g.generation += 1;
                g.last.source = r.source;
                g.last.stopped = false;
                g.last.attempts = 0;
                g.last.updated = now_secs();
                if r.source == Source::Radio { g.last.radio = station } else { g.last.tv = station }
                self.persist(&mut g).await;
            }
            "stop" if current && !g.last.stopped => {
                g.generation += 1;
                g.last.stopped = true;
                g.last.updated = now_secs();
                self.persist(&mut g).await;
            }
            "ok" if current => {
                info!("resume: {:?} confirmed playing", r.source);
                g.last.attempts = 0;
                g.last.updated = now_secs();
                self.persist(&mut g).await;
            }
            "alive" if current && !g.last.stopped => {
                g.last.updated = now_secs();
                if g.last.attempts > 0 || g.persisted_at.is_none_or(|t| t.elapsed() >= ALIVE_PERSIST_EVERY) {
                    g.last.attempts = 0;
                    self.persist(&mut g).await;
                }
            }
            "fail" => warn!("resume: the page could not resume {:?} ({})", r.source, r.name),
            "stop" | "ok" | "alive" => {}
            _ => return Err("unknown event"),
        }
        Ok(())
    }

    // --- YouTube Music ---------------------------------------------------

    /// The player's current track + upcoming, position and repeat.
    async fn ytm_snapshot(&self) -> Option<YtmSession> {
        let pb = self.player.playback().await;
        let q = self.player.state().await;
        let current = q.current?;
        let mut queue = vec![current];
        queue.extend(q.upcoming);
        queue.truncate(MAX_SAVED_QUEUE);
        let position = pb.filter(|p| p.track.video_id == queue[0].video_id).and_then(|p| p.position);
        Some(YtmSession { queue, position, repeat: q.repeat })
    }

    /// Keep the saved YouTube Music session in step with mpv: track changes,
    /// pause/resume by the user, and the position every 30 s.
    async fn poll_ytm(self: Arc<Self>) {
        let mut was_playing = false;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Only while mpv has a track loaded: an idle or dead mpv says
            // nothing about what the user wants.
            let Some(pb) = self.player.playback().await else {
                was_playing = false;
                continue;
            };
            let resumed_by_user = pb.playing && !was_playing;
            was_playing = pb.playing;
            if self.inner.lock().await.last.source != Source::Ytm {
                // mpv going from paused to playing while another source was the
                // last choice = the driver pressed play on YouTube Music again
                // (our own resume only runs when YouTube Music is the source).
                if resumed_by_user {
                    self.user_started_ytm().await;
                }
                continue;
            }
            let Some(snap) = self.ytm_snapshot().await else { continue };
            let mut g = self.inner.lock().await;
            if g.last.source != Source::Ytm {
                continue;
            }
            let stopped = !pb.playing;
            let old = g.last.ytm.clone().unwrap_or_default();
            let track_changed = old.queue.first().map(|t| &t.video_id) != snap.queue.first().map(|t| &t.video_id)
                || old.queue.len() != snap.queue.len()
                || old.repeat != snap.repeat;
            let moved = match (old.position, snap.position) {
                (Some(a), Some(b)) => (a - b).abs() >= 1.0,
                (None, None) => false,
                _ => true,
            };
            let stop_changed = stopped != g.last.stopped;
            if stop_changed {
                info!("resume: YouTube Music {}", if stopped { "paused by the user" } else { "playing again" });
                g.generation += 1;
            }
            g.last.stopped = stopped;
            if !stopped {
                g.last.updated = now_secs();
            }
            g.last.ytm = Some(snap);
            let stale = g.persisted_at.is_none_or(|t| t.elapsed() >= POSITION_PERSIST_EVERY);
            if stop_changed || track_changed || (moved && stale) {
                self.persist(&mut g).await;
            }
        }
    }

    /// Resume a YouTube Music session once online, with the attempt guards.
    async fn resume_ytm(self: Arc<Self>) {
        let mut net = super::net_changes();
        loop {
            let (last, generation) = {
                let g = self.inner.lock().await;
                (g.last.clone(), g.generation)
            };
            if last.source != Source::Ytm {
                return;
            }
            if let Err(why) = due(&last, now_secs()) {
                info!("resume: not resuming YouTube Music: {why}");
                return;
            }
            if !self.music.enabled().await {
                info!("resume: YouTube Music is disabled, not resuming");
                return;
            }
            if !self.online().await {
                // Wake on a route change (4G / Wi-Fi up) or every 10 s.
                tokio::select! {
                    _ = net.changed() => {}
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
                continue;
            }
            let ytm = last.ytm.clone().unwrap_or_default();
            {
                let mut g = self.inner.lock().await;
                if g.generation != generation {
                    continue; // the user acted meanwhile: re-evaluate
                }
                g.last.attempts += 1;
                g.last_attempt = Some(Instant::now());
                info!(
                    "resume: YouTube Music '{}' + {} queued at {:.0}s (attempt {}/{MAX_ATTEMPTS})",
                    ytm.queue[0].title,
                    ytm.queue.len() - 1,
                    ytm.position.unwrap_or(0.0),
                    g.last.attempts
                );
                self.persist(&mut g).await;
            }
            match self.start_ytm(&ytm, generation).await {
                Ok(true) => {
                    let mut g = self.inner.lock().await;
                    if g.last.source == Source::Ytm {
                        g.last.attempts = 0;
                        self.persist(&mut g).await;
                    }
                    info!("resume: YouTube Music confirmed playing");
                    return;
                }
                Ok(false) => {
                    info!("resume: the user took over, stopping the YouTube Music resume");
                    return;
                }
                Err(e) => warn!("resume: YouTube Music attempt failed: {e:#}"),
            }
            tokio::select! {
                _ = net.changed() => {}
                _ = tokio::time::sleep(MIN_SPACING) => {}
            }
        }
    }

    /// Load the queue, restore repeat and the position, and wait until mpv is
    /// really playing. Ok(false) = the user took over meanwhile.
    async fn start_ytm(&self, ytm: &YtmSession, generation: u64) -> anyhow::Result<bool> {
        self.player.play_list(ytm.queue.clone(), 0, false).await?;
        if ytm.repeat != Repeat::Off {
            self.player.set_repeat(ytm.repeat).await?;
        }
        let first = ytm.queue[0].video_id.clone();
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut sought = false;
        let mut start_pos: Option<f64> = None;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.inner.lock().await.generation != generation {
                return Ok(false);
            }
            let Some(pb) = self.player.playback().await else { continue };
            let (Some(pos), Some(dur)) = (pb.position, pb.duration) else { continue };
            if !sought {
                sought = true;
                // Seek only within the track we saved, and not into its last seconds.
                if let Some(at) = ytm.position.filter(|p| *p > 5.0 && *p < dur - 15.0 && pb.track.video_id == first) {
                    self.player.seek(at).await?;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
            let base = *start_pos.get_or_insert(pos);
            if pb.playing && pos - base >= 3.0 {
                return Ok(true);
            }
        }
        anyhow::bail!("mpv did not start playing within 90 s")
    }

    // --- helpers ---------------------------------------------------------

    /// True when the internet answers (DNS + TCP to YouTube Music), cached 3 s.
    async fn online(&self) -> bool {
        if let Some((at, ok)) = self.inner.lock().await.online {
            if at.elapsed() < Duration::from_secs(3) {
                return ok;
            }
        }
        let ok = matches!(
            tokio::time::timeout(Duration::from_secs(4), tokio::net::TcpStream::connect(PROBE_HOST)).await,
            Ok(Ok(_))
        );
        self.inner.lock().await.online = Some((Instant::now(), ok));
        ok
    }

    /// Write `g.last` durably. Errors are logged: losing the resume state must
    /// never break playback.
    async fn persist(&self, g: &mut Inner) {
        g.persisted_at = Some(Instant::now());
        let path = self.path.clone();
        let data = match serde_json::to_vec_pretty(&g.last) {
            Ok(d) => d,
            Err(e) => return warn!("resume: serialise: {e}"),
        };
        let res = tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            super::settings::write_durably(&path, &data)
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("resume: write {}: {e}", self.path.display()),
            Err(e) => warn!("resume: write task: {e}"),
        }
    }
}

async fn api_state(State(s): State<Arc<ResumeService>>) -> Json<serde_json::Value> {
    let last = s.inner.lock().await.last.clone();
    let verdict = due(&last, now_secs());
    Json(serde_json::json!({ "last": last, "due": verdict.is_ok(), "reason": verdict.err() }))
}

async fn api_claim(State(s): State<Arc<ResumeService>>) -> Json<Claim> {
    Json(s.claim().await)
}

async fn api_report(State(s): State<Arc<ResumeService>>, Json(r): Json<Report>) -> Json<serde_json::Value> {
    match s.report(r).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(updated: u64) -> LastMedia {
        LastMedia {
            source: Source::Radio,
            updated,
            radio: Some(Station { name: "Test Radio".into(), url: "https://x/stream".into() }),
            ..Default::default()
        }
    }

    #[test]
    fn due_rules() {
        let now = 1_000_000;
        assert!(due(&radio(now - 3600), now).is_ok());
        assert_eq!(due(&LastMedia::default(), now), Err("nothing was playing"));
        assert_eq!(due(&LastMedia { stopped: true, ..radio(now) }, now), Err("stopped by the user"));
        assert_eq!(due(&LastMedia { attempts: MAX_ATTEMPTS, ..radio(now) }, now), Err("gave up after repeated failures"));
        assert!(due(&LastMedia { attempts: MAX_ATTEMPTS - 1, ..radio(now) }, now).is_ok());
        assert_eq!(due(&radio(now - AUDIO_MAX_AGE.as_secs() - 1), now), Err("too old"));
        // Clock not yet set after boot: the saved time is "in the future".
        assert!(due(&radio(now + 86_400), now).is_ok());
        let tv = LastMedia { source: Source::Tv, tv: Some(Station::default()), ..radio(now - 3600) };
        assert_eq!(due(&tv, now), Err("too old"));
        assert!(due(&LastMedia { updated: now - 600, ..tv }, now).is_ok());
        let ytm = LastMedia { source: Source::Ytm, ytm: Some(YtmSession::default()), ..radio(now) };
        assert_eq!(due(&ytm, now), Err("empty queue"));
    }

    #[test]
    fn file_format_round_trips_and_tolerates_missing_keys() {
        let last = LastMedia {
            source: Source::Ytm,
            updated: 42,
            ytm: Some(YtmSession {
                queue: vec![Track { video_id: "dQw4w9WgXcQ".into(), title: "T".into(), artist: "A".into(), album: String::new(), art: String::new() }],
                position: Some(61.5),
                repeat: Repeat::All,
            }),
            ..Default::default()
        };
        let text = serde_json::to_string(&last).unwrap();
        assert!(text.contains("\"source\":\"ytm\""));
        assert_eq!(serde_json::from_str::<LastMedia>(&text).unwrap(), last);
        let partial: LastMedia = serde_json::from_str(r#"{"source":"radio"}"#).unwrap();
        assert_eq!(partial.source, Source::Radio);
        assert_eq!(partial.attempts, 0);
    }

    fn service(dir: &std::path::Path) -> Arc<ResumeService> {
        let settings = Arc::new(super::super::settings::SettingsService::new());
        let music = Arc::new(MusicService::new(settings));
        let player = PlayerService::new(music.clone());
        ResumeService::with_path(dir.join("last_media.json"), player, music)
    }

    #[tokio::test]
    async fn page_reports_drive_the_saved_session() {
        let dir = std::env::temp_dir().join(format!("rascar-resume-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = service(&dir);
        let play = |event: &str| Report { event: event.into(), source: Source::Radio, name: "Test Radio".into(), url: "https://x/stream".into() };
        s.report(play("play")).await.unwrap();
        let saved: LastMedia = serde_json::from_str(&std::fs::read_to_string(dir.join("last_media.json")).unwrap()).unwrap();
        assert_eq!(saved.source, Source::Radio);
        assert!(!saved.stopped);
        assert_eq!(saved.radio.unwrap().name, "Test Radio");

        // A restart re-reads it; a claim while online hands it out and counts
        // the attempt on disk; a second claim right after is spaced out.
        let s = service(&dir);
        s.inner.lock().await.online = Some((Instant::now(), true));
        let c = s.claim().await;
        assert_eq!(c.resume.as_ref().map(|r| (r.name.as_str(), r.attempt)), Some(("Test Radio", 1)));
        let saved: LastMedia = serde_json::from_str(&std::fs::read_to_string(dir.join("last_media.json")).unwrap()).unwrap();
        assert_eq!(saved.attempts, 1);
        let again = s.claim().await;
        assert!(again.resume.is_none() && again.retry_in.is_some());
        // Confirmed: attempts back to 0.
        s.report(play("ok")).await.unwrap();
        assert_eq!(s.inner.lock().await.last.attempts, 0);
        // Stopped by the user: nothing to claim, no retry.
        s.report(play("stop")).await.unwrap();
        let c = s.claim().await;
        assert_eq!((c.resume.is_none(), c.retry_in, c.reason), (true, None, Some("stopped by the user")));
        // Bad input is refused.
        assert!(s.report(Report { event: "play".into(), source: Source::Radio, name: "x".into(), url: "file:///etc/passwd".into() }).await.is_err());
        assert!(s.report(Report { event: "play".into(), source: Source::Ytm, name: String::new(), url: String::new() }).await.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn offline_claims_retry_without_spending_attempts() {
        let dir = std::env::temp_dir().join(format!("rascar-resume-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("last_media.json"), serde_json::to_string(&radio(now_secs())).unwrap()).unwrap();
        let s = service(&dir);
        s.inner.lock().await.online = Some((Instant::now(), false));
        let c = s.claim().await;
        assert_eq!((c.resume.is_none(), c.retry_in, c.reason), (true, Some(5), Some("offline")));
        assert_eq!(s.inner.lock().await.last.attempts, 0);
        // Three failed attempts, then it gives up for good.
        for n in 1..=MAX_ATTEMPTS {
            let mut g = s.inner.lock().await;
            g.online = Some((Instant::now(), true));
            g.last_attempt = None;
            drop(g);
            assert_eq!(s.claim().await.resume.map(|r| r.attempt), Some(n));
        }
        s.inner.lock().await.last_attempt = None;
        assert_eq!(s.claim().await.reason, Some("gave up after repeated failures"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
