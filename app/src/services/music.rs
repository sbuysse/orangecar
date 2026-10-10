//! Music service — YouTube Music integration via a Python `ytmusicapi` sidecar.
//!
//! YouTube Music has no official API, so `ytmusicapi` (Python) is the de-facto
//! client. This service shells out to `app/scripts/ytmusic_helper.py`, which
//! prints exactly one JSON object per invocation. The helper holds no
//! credentials: its API calls run inside the car browser, with the browser's
//! own Google session (ytbridge.rs). Every method degrades cleanly to an `Err`
//! when python3 / ytmusicapi / the browser are unavailable, so the dashboard
//! never breaks when YouTube Music isn't set up.
//!
//! Errors returned to callers are coarse *categories* (see `MusicError`), never
//! raw sidecar output: that output can contain Python tracebacks and absolute
//! home-directory paths, and it is served to unauthenticated LAN clients. The
//! detail is logged server-side instead.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use super::settings::SettingsService;

/// Once python3 is found missing we stop re-spawning it on every call.
static PYTHON_MISSING: AtomicBool = AtomicBool::new(false);
/// Same latch for the audio player used by `play`.

/// Audio player used to actually play a track. mpv is the least surprising
/// choice on this box: it publishes an MPRIS interface, so the existing

/// Error categories exposed to HTTP clients. Deliberately coarse — the
/// underlying message is logged, not returned.
pub enum MusicError {
    /// Auth file missing or rejected — the user must re-authenticate.
    Auth,
    /// Integration not usable at all (python3/ytmusicapi/mpv absent, or the
    /// feature is switched off in settings).
    Unavailable,
    /// The remote service failed, timed out or answered unusably.
    Upstream,
    /// The request was well-formed but matched nothing.
    NotFound,
}

impl MusicError {
    fn as_str(&self) -> &'static str {
        match self {
            MusicError::Auth => "auth",
            MusicError::Unavailable => "unavailable",
            MusicError::Upstream => "upstream",
            MusicError::NotFound => "notfound",
        }
    }

    fn from_code(code: &str) -> Self {
        match code {
            "auth" => MusicError::Auth,
            "unavailable" => MusicError::Unavailable,
            "notfound" => MusicError::NotFound,
            _ => MusicError::Upstream,
        }
    }
}

/// Log the detail, return only the category.
fn fail(category: MusicError, detail: impl std::fmt::Display) -> anyhow::Error {
    warn!("music helper failure [{}]: {detail}", category.as_str());
    anyhow!("{}", category.as_str())
}

pub struct MusicService {
    /// Path to the Python sidecar (`ytmusic_helper.py`).
    helper_path: PathBuf,
    /// Live settings, re-read on every call: the enable flag is user-editable
    /// at runtime via `POST /api/settings` and must take effect without a restart.
    settings: Arc<SettingsService>,
    /// Last answer per read-only helper call (key = its args). Every call
    /// spawns python + a round trip to YouTube Music, 1-3.5 s each; opening
    /// the YT Music page made four of them. See `cached`.
    cache: tokio::sync::Mutex<HashMap<String, CacheEntry>>,
    /// Keys with a background refresh in flight (one at a time per key).
    refreshing: std::sync::Mutex<HashSet<String>>,
    /// Persisted copy of `cache`, so the first open after a boot is instant.
    cache_path: PathBuf,
    /// `http://127.0.0.1:<port>/internal/ytm/fetch` once known (main.rs).
    bridge_url: std::sync::OnceLock<Option<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    value: Value,
    /// Unix seconds.
    fetched: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// How long a cached answer counts as fresh. Older ones are still served
/// (except `status`) while a background refresh replaces them.
const TTL_STATUS: Duration = Duration::from_secs(60);
const TTL_HISTORY: Duration = Duration::from_secs(2 * 60);
const TTL_LIKED: Duration = Duration::from_secs(10 * 60);
const TTL_HOME: Duration = Duration::from_secs(15 * 60);
const TTL_LIBRARY: Duration = Duration::from_secs(30 * 60);
const TTL_ARTIST: Duration = Duration::from_secs(6 * 3600);
/// A background refresh gives up after this (the old value stays).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(40);

impl MusicService {
    /// Sidecar: `$RASCAR_YTMUSIC_HELPER`, else the first existing of the
    /// deployed layout (`<root>/target/<profile>/rascar-app` next to
    /// `<root>/app/scripts/`) and the source tree. `CARGO_MANIFEST_DIR` alone is
    /// the BUILD machine's path, so a cross-built binary never found it on the Pi.
    pub fn new(settings: Arc<SettingsService>) -> Self {
        let helper_path = std::env::var("RASCAR_YTMUSIC_HELPER")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let mut candidates = Vec::new();
                if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(std::path::Path::parent) {
                    candidates.push(dir.join("../../app/scripts/ytmusic_helper.py"));
                }
                let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/ytmusic_helper.py");
                candidates.into_iter().find(|p| p.is_file()).unwrap_or(source)
            });
        info!("ytmusic helper: {}", helper_path.display());

        let cache_path = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rascar/ytmusic-cache.json");
        let cache = std::fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, CacheEntry>>(&b).ok())
            .unwrap_or_default();
        Self {
            helper_path,
            settings,
            cache: tokio::sync::Mutex::new(cache),
            refreshing: std::sync::Mutex::new(HashSet::new()),
            cache_path,
            bridge_url: std::sync::OnceLock::new(),
        }
    }

    /// Stale-while-revalidate: a cached answer comes back at once; when it is
    /// older than `ttl` a background refresh replaces it (and keeps it on
    /// failure). Only a cache miss waits for the helper. With `serve_stale`
    /// false (the session status) an expired entry is fetched again instead,
    /// so a session that died is never reported as alive.
    async fn cached(self: &Arc<Self>, ttl: Duration, serve_stale: bool, args: &[&str]) -> Result<Value> {
        let key = args.join("\u{1f}");
        let now = unix_now();
        let hit = self.cache.lock().await.get(&key).map(|e| (e.value.clone(), now.saturating_sub(e.fetched)));
        if let Some((value, age)) = hit {
            if age < ttl.as_secs() {
                return Ok(value);
            }
            if serve_stale {
                self.refresh_in_background(key, args);
                return Ok(value);
            }
        }
        let value = self.run(args).await?;
        self.store(key, value.clone()).await;
        Ok(value)
    }

    fn refresh_in_background(self: &Arc<Self>, key: String, args: &[&str]) {
        if !self.refreshing.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone()) {
            return;
        }
        let me = Arc::clone(self);
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        tokio::spawn(async move {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Ok(Ok(value)) = tokio::time::timeout(REFRESH_TIMEOUT, me.run(&refs)).await {
                me.store(key.clone(), value).await;
            }
            me.refreshing.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
        });
    }

    async fn store(&self, key: String, value: Value) {
        let mut cache = self.cache.lock().await;
        cache.insert(key, CacheEntry { value, fetched: unix_now() });
        self.save(&cache);
    }

    /// Atomic, owner-only write: the cache holds the user's library and history.
    fn save(&self, cache: &HashMap<String, CacheEntry>) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(bytes) = serde_json::to_vec(cache) else { return };
        if let Some(dir) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.cache_path.with_extension("json.tmp");
        let written = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&bytes));
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, &self.cache_path);
        }
    }

    /// Forget cached answers whose key starts with one of `prefixes` (all when empty).
    async fn invalidate(&self, prefixes: &[&str]) {
        let mut cache = self.cache.lock().await;
        if prefixes.is_empty() {
            cache.clear();
        } else {
            cache.retain(|k, _| !prefixes.iter().any(|p| k.starts_with(p)));
        }
        self.save(&cache);
    }

    /// Keep the page's first screen warm: 60 s after start and every 30 min,
    /// refresh what opening YouTube Music asks for, with the exact same
    /// arguments as the page (they are the cache keys). Every refresh opens
    /// the browser's YouTube Music tab and makes Google rotate session
    /// cookies; Chromium only writes those to disk every ~30 s and the car
    /// cuts power without warning, so refreshes are kept rare (ytbridge.rs).
    pub fn start_warmup(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                if me.status().await.is_ok() {
                    let _ = me.home(6).await;
                    let _ = me.playlists().await;
                    let _ = me.history(12).await;
                }
                tokio::time::sleep(Duration::from_secs(30 * 60)).await;
            }
        });
    }

    /// Marker that keeps the helper off the browser session after a sign-out
    /// on the dashboard (the Google session itself stays: Maps and the other
    /// apps use it). The next explicit sign-in removes it.
    pub fn signed_out_marker() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root")).join(".config/rascar/ytmusic_signed_out")
    }

    pub fn signed_out() -> bool {
        Self::signed_out_marker().exists()
    }

    pub async fn sign_out(&self) -> Result<()> {
        // Nothing of the old account may be shown after this.
        self.invalidate(&[]).await;
        if let Some(dir) = Self::signed_out_marker().parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(Self::signed_out_marker(), b"")?;
        info!("YouTube Music: signed out on the dashboard");
        Ok(())
    }

    /// The user just signed in (sign-in tab closed): lift a sign-out, reload
    /// the browser tab with the new session and forget what was fetched
    /// before. Returns the session state for the dashboard ("ok", "none",
    /// "unavailable").
    pub async fn after_signin(self: &Arc<Self>) -> &'static str {
        let _ = std::fs::remove_file(Self::signed_out_marker());
        if let Err(e) = super::ytbridge::reload().await {
            warn!("YouTube Music: could not reload the browser tab: {e:#}");
        }
        self.invalidate(&[]).await;
        match self.status().await {
            Ok(_) => "ok",
            Err(e) if e.to_string() == "auth" => "none",
            Err(_) => "unavailable",
        }
    }

    /// Where the helper reaches the browser bridge (`None`: bridge off, the
    /// helper then only makes anonymous calls of its own).
    pub fn set_bridge_url(&self, url: Option<String>) {
        let _ = self.bridge_url.set(url);
    }

    /// YouTube Music is on unless explicitly disabled (the field is absent in
    /// settings files written before it existed).
    pub async fn enabled(&self) -> bool {
        self.settings
            .settings
            .read()
            .await
            .ytmusic_enabled
            .unwrap_or(true)
    }

    /// Run the sidecar with the given command args and parse its JSON object.
    async fn run(&self, args: &[&str]) -> Result<Value> {
        if !self.enabled().await {
            return Err(fail(
                MusicError::Unavailable,
                "ytmusic_enabled is false in settings",
            ));
        }
        // If we already learned python3 isn't installed, don't keep spawning it.
        if PYTHON_MISSING.load(Ordering::Relaxed) {
            return Err(fail(MusicError::Unavailable, "python3 not found"));
        }

        let mut command = tokio::process::Command::new("python3");
        command.arg(&self.helper_path).args(args);
        // Signed out on the dashboard: the helper stays anonymous.
        match self.bridge_url.get().cloned().flatten().filter(|_| !Self::signed_out()) {
            Some(url) => {
                command.env("RASCAR_YTM_BRIDGE", url).env("RASCAR_YTM_BRIDGE_TOKEN", super::ytbridge::token());
            }
            None => {
                command.env_remove("RASCAR_YTM_BRIDGE").env_remove("RASCAR_YTM_BRIDGE_TOKEN");
            }
        }
        let spawned = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Without this, a `tokio::time::timeout` around this future drops
            // the Child while the interpreter keeps running — every timed-out
            // request would leak a wedged ~30MB python3 forever.
            .kill_on_drop(true)
            .spawn();
        let output = match async { spawned?.wait_with_output().await }.await {
            Ok(o) => o,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    warn!("python3 not found — disabling YouTube Music integration");
                    PYTHON_MISSING.store(true, Ordering::Relaxed);
                }
                return Err(fail(MusicError::Unavailable, e));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // The helper always prints exactly one JSON object (even on failure).
        let val: Value = match serde_json::from_str(stdout.trim()) {
            Ok(v) => v,
            Err(_) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // stderr can carry a full Python traceback with absolute paths:
                // log it, never return it.
                return Err(fail(
                    MusicError::Upstream,
                    format!(
                        "invalid helper output (stdout={:?}, stderr={:?})",
                        stdout.trim(),
                        stderr.trim()
                    ),
                ));
            }
        };

        // A helper-reported error takes precedence over the exit code. The
        // helper supplies a machine-readable `code`; `error` is detail for us.
        if let Some(detail) = val.get("error").and_then(|e| e.as_str()) {
            let code = val.get("code").and_then(|c| c.as_str()).unwrap_or("upstream");
            return Err(fail(MusicError::from_code(code), detail));
        }
        if !output.status.success() {
            return Err(fail(
                MusicError::Upstream,
                format!("helper exited with {}", output.status),
            ));
        }

        Ok(val)
    }

    /// Confirm the session: an authenticated account call through the browser succeeds.
    pub async fn status(self: &Arc<Self>) -> Result<Value> {
        self.cached(TTL_STATUS, false, &["status"]).await
    }

    /// Liked songs. `{ "tracks": [...] }`.
    pub async fn liked(self: &Arc<Self>, limit: u32) -> Result<Value> {
        let limit = limit.to_string();
        self.cached(TTL_LIKED, true, &["liked", &limit]).await
    }

    /// Library playlists. `{ "playlists": [...] }`.
    pub async fn playlists(self: &Arc<Self>) -> Result<Value> {
        self.cached(TTL_LIBRARY, true, &["playlists"]).await
    }

    /// Home feed shelves. `{ "shelves": [{ "title", "items": [...] }] }`.
    pub async fn home(self: &Arc<Self>, shelves: u32) -> Result<Value> {
        self.cached(TTL_HOME, true, &["home", &shelves.clamp(1, 12).to_string()]).await
    }

    /// Recently played tracks (account history). `{ "tracks": [...] }`.
    pub async fn history(self: &Arc<Self>, limit: u32) -> Result<Value> {
        self.cached(TTL_HISTORY, true, &["history", &limit.clamp(1, 200).to_string()]).await
    }

    /// A single playlist's tracks. `{ "title": "...", "tracks": [...] }`.
    pub async fn playlist(self: &Arc<Self>, id: &str, limit: u32) -> Result<Value> {
        self.cached(TTL_LIBRARY, true, &["playlist", id, &limit.clamp(1, 2000).to_string()]).await
    }

    /// YouTube Music's own lyrics for a track (timestamped when available).
    pub async fn lyrics(&self, video_id: &str) -> Result<Value> {
        self.run(&["lyrics", video_id]).await
    }

    /// An artist's page (top songs), public data.
    pub async fn artist(self: &Arc<Self>, browse_id: &str) -> Result<Value> {
        self.cached(TTL_ARTIST, true, &["artist", browse_id]).await
    }

    /// Search songs + artists. `{ "tracks": [...], "artists": [...] }`.
    pub async fn search(&self, q: &str) -> Result<Value> {
        self.run(&["search", q]).await
    }

    /// Rate a track. `rating` must be LIKE / DISLIKE / INDIFFERENT.
    /// YouTube Music's radio for a track (what its app plays after it).
    pub async fn radio(&self, video_id: &str) -> Result<Value> {
        self.run(&["radio", video_id]).await
    }

    pub async fn rate(&self, video_id: &str, rating: &str) -> Result<Value> {
        let result = self.run(&["rate", video_id, rating]).await;
        if result.is_ok() {
            // The liked-songs list ("LM") just changed.
            self.invalidate(&["liked\u{1f}", "playlist\u{1f}LM\u{1f}"]).await;
        }
        result
    }

    /// Resolve the top song match for a title/artist and return its videoId.
    pub async fn search_one(&self, title: &str, artist: &str) -> Result<String> {
        let v = self.run(&["search_one", title, artist]).await?;
        v.get("videoId")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| fail(MusicError::NotFound, "helper returned no videoId"))
    }

    /// Like the currently-playing track (search for it, then rate LIKE).
    pub async fn like_current(&self, title: &str, artist: &str) -> Result<()> {
        let video_id = self.search_one(title, artist).await?;
        self.rate(&video_id, "LIKE").await?;
        Ok(())
    }

    /// Dislike the currently-playing track (search for it, then rate DISLIKE).
    pub async fn dislike_current(&self, title: &str, artist: &str) -> Result<()> {
        let video_id = self.search_one(title, artist).await?;
        self.rate(&video_id, "DISLIKE").await?;
        Ok(())
    }
}

