//! iAP2 Now Playing — the device→accessory `NowPlayingUpdate` (0x5001) stream and
//! the now-playing artwork it references over the iAP2 File Transfer session.
//!
//! # What the car asks for
//!
//! The Jaguar sends `StartNowPlayingUpdates` (0x5000) once per session, ~3 s
//! after link up, asking for MediaItem Title(1), PlaybackDurationInMilliseconds(4),
//! AlbumTitle(6), Artist(12), Composer(18), ArtworkFileTransferIdentifier(26) and
//! PlaybackAttributes 0-7 and 12-16. Its SYN declares session 2 as a File Transfer
//! session (type 1, v1), which is where the artwork goes.
//!
//! # Sources for the encoding
//!
//! - Parameter ids: Apple `iAP2Constants.h` (`iAP2NowPlayingUpdate_*`).
//! - Value types: the catplay-labs/catplay CSM schema (`core/catplay_csm`,
//!   used as a reference for the wire format; `MediaItem`: title CsmString, duration u32, artwork id u8;
//!   `PlaybackAttributes`: status/shuffle/repeat u8 enums, elapsed/queue u32,
//!   speed u16, flags bool) and carplayd `crates/metadata/src/tlv.rs`, whose ids
//!   and `msecs32` widths were decoded from Apple's CarPlaySimulator spec.
//! - Strings: UTF-8 + NUL (catplay `CsmString`; Apple's own strings in the car's
//!   0x1D01 carry the NUL too).
//! - Message shape: catplay's device-side `carplay_client_session.rs`
//!   (`send_now_playing`) and a production head-unit log of a real iPhone
//!   (`iap2-spec/hu_iap2.log`) both show updates that carry ONLY the groups and
//!   fields that changed ("message with no media attributes", "... no playback
//!   attributes"), an artwork id announced in a media-only update, then a
//!   `NowPlayingArtworkData` file transfer with id 0x80/0x81 (device ids are
//!   128..=254).
//!
//! # File transfer datagrams
//!
//! `[file_id u8][control u8][payload]` on the File Transfer link session. Control:
//! Start 0x01, Cancel 0x02, Pause 0x03, Setup 0x04, Success 0x05, Failure 0x06,
//! data chunks 0x80 first / 0x00 middle / 0x40 last / 0xC0 first-and-only. Setup
//! payload = `[u64 BE total size][u16 BE file type]`, NowPlayingArtworkData = 2.
//! Source for the byte values: the catplay-labs/catplay iAP2 link and CSM
//! file-transfer definitions (`core/catplay_iap2_link`, `core/catplay_csm`). The
//! production log confirms the type value ("NowPlayingArtworkData setup message:
//! id 0x80, type 0x2, size 0") but NOT the order of the size and type fields —
//! catplay is the only source for that, and its receiver only logs the size. That
//! is why artwork is behind `RASCAR_NOWPLAYING_ARTWORK`, default OFF.
//!
//! # Behaviour
//!
//! A poller task reads `GET /api/media` from rascar-app once per second while the
//! subscription is open and publishes an observation only when it differs
//! materially (track, play/pause, a seek of more than [`SEEK_THRESHOLD_MS`],
//! shuffle). The service loop coalesces those to at most one 0x5001 per
//! [`MIN_UPDATE_GAP`] (latest wins) and diffs against what it last SENT. Elapsed
//! time goes out with the status and speed so the head unit extrapolates; there
//! are no periodic elapsed ticks.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::{timeout, Instant};
use tracing::{debug, info, warn};

use super::control::ControlMessage;

/// A position mismatch above this, against the position extrapolated from the
/// last update, is a seek the head unit must hear about.
pub const SEEK_THRESHOLD_MS: i64 = 2_000;
/// At most one 0x5001 per this interval; a burst of changes collapses into the
/// latest state.
pub const MIN_UPDATE_GAP: Duration = Duration::from_millis(500);
/// How often rascar-app is asked for the current track while subscribed.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Bound on one `/api/media` round trip.
const POLL_TIMEOUT: Duration = Duration::from_millis(800);
/// Longest string sent in one field. Keeps a whole update far below the car's
/// 4 KiB link packet.
const MAX_STRING_BYTES: usize = 255;
/// Wait for the car's Start after a Setup, or for its Success after the last
/// chunk, before the transfer is abandoned (silently: no extra traffic).
const FILE_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest a transfer may stay paused by the car before it is abandoned.
const FILE_PAUSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Artwork larger than this after re-encoding is not sent.
const MAX_ARTWORK_BYTES: usize = 128 * 1024;
/// Artwork box edge, in pixels.
const ARTWORK_EDGE: u32 = 300;

/// Apple `iAP2NowPlayingUpdate_*` parameter ids (iAP2Constants.h).
pub mod pid {
    pub const MEDIA_ITEM_ATTRIBUTES: u16 = 0x0000;
    pub const PLAYBACK_ATTRIBUTES: u16 = 0x0001;

    pub mod media {
        pub const TITLE: u16 = 0x0001;
        pub const DURATION_MS: u16 = 0x0004;
        pub const ALBUM_TITLE: u16 = 0x0006;
        pub const ARTIST: u16 = 0x000C;
        pub const COMPOSER: u16 = 0x0012;
        pub const ARTWORK_FILE_TRANSFER_ID: u16 = 0x001A;
    }

    pub mod playback {
        pub const STATUS: u16 = 0x0000;
        pub const ELAPSED_MS: u16 = 0x0001;
        pub const QUEUE_INDEX: u16 = 0x0002;
        pub const QUEUE_COUNT: u16 = 0x0003;
        pub const QUEUE_CHAPTER_INDEX: u16 = 0x0004;
        pub const SHUFFLE_MODE: u16 = 0x0005;
        pub const REPEAT_MODE: u16 = 0x0006;
        pub const APP_NAME: u16 = 0x0007;
        pub const SPEED: u16 = 0x000C;
        pub const SET_ELAPSED_TIME_AVAILABLE: u16 = 0x000D;
        pub const QUEUE_LIST_AVAIL: u16 = 0x000E;
        pub const APP_BUNDLE_ID: u16 = 0x0010;
    }
}

/// `PlaybackStatus` enum values (catplay `PlaybackStatus`, carplayd `types.rs`).
pub const STATUS_STOPPED: u8 = 0;
pub const STATUS_PLAYING: u8 = 1;
pub const STATUS_PAUSED: u8 = 2;

// ── Configuration ───────────────────────────────────────────────────────────

/// Runtime switches, read once per session.
#[derive(Debug, Clone)]
pub struct Config {
    /// `RASCAR_NOWPLAYING` (default on): real metadata instead of "Stopped".
    pub enabled: bool,
    /// `RASCAR_NOWPLAYING_ARTWORK` (default OFF, see the module docs).
    pub artwork: bool,
    /// `RASCAR_NOWPLAYING_ADDR` (default `127.0.0.1:8080`): rascar-app.
    pub app_addr: String,
    pub app: AppIdentity,
}

impl Config {
    pub fn from_env() -> Self {
        let enabled = crate::debug::env_flag("RASCAR_NOWPLAYING", true);
        Config {
            enabled,
            artwork: enabled && crate::debug::env_flag("RASCAR_NOWPLAYING_ARTWORK", false),
            app_addr: std::env::var("RASCAR_NOWPLAYING_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
            app: AppIdentity::from_env(),
        }
    }
}

/// What the head unit shows as the playing app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    pub name: String,
    pub bundle_id: String,
}

impl AppIdentity {
    /// `RASCAR_NOWPLAYING_APP` / `RASCAR_NOWPLAYING_BUNDLE`. The initial reset
    /// reply keeps sending empty strings (what the reference sends, see
    /// `control::subscription_replies`); real updates name the app, as an iPhone
    /// does once something is playing.
    pub fn from_env() -> Self {
        let name = std::env::var("RASCAR_NOWPLAYING_APP").ok().filter(|s| !s.is_empty());
        let bundle = std::env::var("RASCAR_NOWPLAYING_BUNDLE").ok().filter(|s| !s.is_empty());
        AppIdentity {
            name: name.unwrap_or_else(|| "OrangeCar".to_string()),
            bundle_id: bundle.unwrap_or_else(|| "org.orangecar.dashboard".to_string()),
        }
    }
}

// ── Snapshot of rascar-app's now playing ───────────────────────────────────

/// One reading of `GET /api/media` (app `services/media.rs` `NowPlaying`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaSnapshot {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: String,
    pub playing: bool,
    pub position_ms: Option<u32>,
    pub duration_ms: Option<u32>,
    pub shuffle: bool,
}

impl MediaSnapshot {
    /// Parse the app's JSON. `position_sec`/`length_sec` are floats or null.
    pub fn from_api_json(body: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(body).ok()?;
        let obj = v.as_object()?;
        let s = |k: &str| obj.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let ms = |k: &str| {
            obj.get(k)
                .and_then(|x| x.as_f64())
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| (f * 1000.0).round().min(u32::MAX as f64) as u32)
        };
        Some(MediaSnapshot {
            title: s("title"),
            artist: s("artist"),
            album: s("album"),
            art_url: s("art_url"),
            playing: obj.get("is_playing").and_then(|x| x.as_bool()).unwrap_or(false),
            position_ms: ms("position_sec"),
            duration_ms: ms("length_sec").filter(|d| *d > 0),
            shuffle: obj.get("shuffle").and_then(|x| x.as_bool()).unwrap_or(false),
        })
    }

    pub fn has_track(&self) -> bool {
        !self.title.is_empty()
    }

    pub fn status(&self) -> u8 {
        match (self.has_track(), self.playing) {
            (false, _) => STATUS_STOPPED,
            (true, true) => STATUS_PLAYING,
            (true, false) => STATUS_PAUSED,
        }
    }

    fn same_track(&self, other: &MediaSnapshot) -> bool {
        self.title == other.title && self.artist == other.artist && self.album == other.album
    }
}

/// A snapshot and the instant it was read, so the position can be extrapolated.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub snap: MediaSnapshot,
    pub at: Instant,
}

impl Observed {
    /// The playback position at `now`: advanced by the time since the reading
    /// while playing, clamped to the duration.
    pub fn elapsed_at(&self, now: Instant) -> Option<u32> {
        let pos = self.snap.position_ms?;
        let advanced = if self.snap.playing {
            let dt = now.saturating_duration_since(self.at).as_millis().min(u32::MAX as u128) as u32;
            pos.saturating_add(dt)
        } else {
            pos
        };
        Some(match self.snap.duration_ms {
            Some(d) => advanced.min(d),
            None => advanced,
        })
    }

    /// Artwork identity: a new track or new art URL means new artwork.
    pub fn art_key(&self) -> ArtKey {
        ArtKey {
            title: self.snap.title.clone(),
            artist: self.snap.artist.clone(),
            album: self.snap.album.clone(),
            art_url: self.snap.art_url.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtKey {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: String,
}

// ── What changed ────────────────────────────────────────────────────────────

/// Which groups an update must carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Change {
    /// The track itself (identity or duration) changed: full MediaItem group.
    pub media: bool,
    /// Status, seek or shuffle changed (or the track did): PlaybackAttributes.
    pub playback: bool,
}

impl Change {
    pub fn any(self) -> bool {
        self.media || self.playback
    }
}

/// Compare a new observation with the last one the head unit knows about.
pub fn diff(last: Option<&Observed>, new: &Observed) -> Change {
    let Some(last) = last else {
        return Change { media: true, playback: true };
    };
    let (a, b) = (&last.snap, &new.snap);
    let duration_moved = match (a.duration_ms, b.duration_ms) {
        (Some(x), Some(y)) => (i64::from(x) - i64::from(y)).abs() > 1_000,
        (None, None) => false,
        _ => true,
    };
    let media = !a.same_track(b) || duration_moved;
    let seek = match (last.elapsed_at(new.at), b.position_ms) {
        (Some(expected), Some(actual)) => (i64::from(actual) - i64::from(expected)).abs() > SEEK_THRESHOLD_MS,
        // The position just became known (or unknown): the car must learn it.
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    };
    let playback = media || a.status() != b.status() || a.shuffle != b.shuffle || seek;
    Change { media, playback }
}

// ── What the car subscribed to ──────────────────────────────────────────────

/// The attribute ids listed in `StartNowPlayingUpdates`. `None` = the group was
/// not requested at all, so it is never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requested {
    pub media: Option<Vec<u16>>,
    pub playback: Option<Vec<u16>>,
}

impl Requested {
    /// Param 0 / param 1 of 0x5000 are groups of empty flag TLVs
    /// (`00 04 <id>`), one per wanted attribute.
    pub fn from_start_message(msg: &ControlMessage) -> Self {
        let ids = |p: u16| msg.param(p).map(|d| inner_tlvs(d).into_iter().map(|(id, _)| id).collect::<Vec<_>>());
        Requested {
            media: ids(pid::MEDIA_ITEM_ATTRIBUTES),
            playback: ids(pid::PLAYBACK_ATTRIBUTES),
        }
    }

    fn wants_media(&self, id: u16) -> bool {
        self.media.as_ref().is_some_and(|v| v.contains(&id))
    }

    fn wants_playback(&self, id: u16) -> bool {
        self.playback.as_ref().is_some_and(|v| v.contains(&id))
    }

    pub fn wants_artwork(&self) -> bool {
        self.wants_media(pid::media::ARTWORK_FILE_TRANSFER_ID)
    }
}

/// Walk `[u16 len][u16 id][value]` records (len counts its own header). Stops at
/// the first malformed record.
fn inner_tlvs(data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if len < 4 || i + len > data.len() {
            break;
        }
        out.push((u16::from_be_bytes([data[i + 2], data[i + 3]]), &data[i + 4..i + len]));
        i += len;
    }
    out
}

// ── Encoding ────────────────────────────────────────────────────────────────

fn tlv(out: &mut Vec<u8>, id: u16, value: &[u8]) {
    out.extend_from_slice(&((value.len() + 4) as u16).to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(value);
}

/// UTF-8 + NUL, without interior NULs, cut at a char boundary.
fn csm_string(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| *c != '\0').collect();
    let mut end = clean.len().min(MAX_STRING_BYTES);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = clean.as_bytes()[..end].to_vec();
    out.push(0);
    out
}

/// The parameters of one `NowPlayingUpdate` (0x5001) for `obs`, carrying only
/// the groups `change` names and, inside them, only the requested attributes.
///
/// A playback-only update (play/pause, seek, shuffle) carries status, elapsed,
/// speed and shuffle; a track change carries every requested field.
pub fn encode_update(
    obs: &Observed,
    now: Instant,
    req: &Requested,
    change: Change,
    app: &AppIdentity,
) -> Vec<(u16, Vec<u8>)> {
    use pid::{media as m, playback as p};
    let s = &obs.snap;
    let mut params = Vec::new();

    if change.media && req.media.is_some() {
        let mut g = Vec::new();
        if req.wants_media(m::TITLE) {
            tlv(&mut g, m::TITLE, &csm_string(&s.title));
        }
        if req.wants_media(m::DURATION_MS) {
            tlv(&mut g, m::DURATION_MS, &s.duration_ms.unwrap_or(0).to_be_bytes());
        }
        if req.wants_media(m::ALBUM_TITLE) {
            tlv(&mut g, m::ALBUM_TITLE, &csm_string(&s.album));
        }
        if req.wants_media(m::ARTIST) {
            tlv(&mut g, m::ARTIST, &csm_string(&s.artist));
        }
        if req.wants_media(m::COMPOSER) {
            tlv(&mut g, m::COMPOSER, &csm_string(""));
        }
        if !g.is_empty() {
            params.push((pid::MEDIA_ITEM_ATTRIBUTES, g));
        }
    }

    if change.playback && req.playback.is_some() {
        let full = change.media;
        let status = s.status();
        let mut g = Vec::new();
        let put = |g: &mut Vec<u8>, id: u16, v: &[u8], always: bool| {
            if (always || full) && req.wants_playback(id) {
                tlv(g, id, v);
            }
        };
        put(&mut g, p::STATUS, &[status], true);
        put(&mut g, p::ELAPSED_MS, &obs.elapsed_at(now).unwrap_or(0).to_be_bytes(), true);
        put(&mut g, p::QUEUE_INDEX, &0u32.to_be_bytes(), false);
        put(&mut g, p::QUEUE_COUNT, &u32::from(s.has_track()).to_be_bytes(), false);
        put(&mut g, p::QUEUE_CHAPTER_INDEX, &0u32.to_be_bytes(), false);
        // PlaybackShuffle: Off 0, Songs 1. PlaybackRepeat: Off 0.
        put(&mut g, p::SHUFFLE_MODE, &[u8::from(s.shuffle)], true);
        put(&mut g, p::REPEAT_MODE, &[0], false);
        put(&mut g, p::APP_NAME, &csm_string(&app.name), false);
        // Percent of normal speed (catplay sends 100 while playing, 0 otherwise).
        let speed: u16 = if status == STATUS_PLAYING { 100 } else { 0 };
        put(&mut g, p::SPEED, &speed.to_be_bytes(), true);
        // We do not implement SetNowPlayingInformation (0x5003), nor a queue list.
        put(&mut g, p::SET_ELAPSED_TIME_AVAILABLE, &[0], false);
        put(&mut g, p::QUEUE_LIST_AVAIL, &[0], false);
        put(&mut g, p::APP_BUNDLE_ID, &csm_string(&app.bundle_id), false);
        if !g.is_empty() {
            params.push((pid::PLAYBACK_ATTRIBUTES, g));
        }
    }
    params
}

/// A media-only update that announces the artwork's file transfer id, as the
/// reference sends it right before the transfer.
pub fn encode_artwork_announce(file_id: u8) -> Vec<(u16, Vec<u8>)> {
    let mut g = Vec::new();
    tlv(&mut g, pid::media::ARTWORK_FILE_TRANSFER_ID, &[file_id]);
    vec![(pid::MEDIA_ITEM_ATTRIBUTES, g)]
}

/// One-line description for the logs.
pub fn describe(s: &MediaSnapshot) -> String {
    if !s.has_track() {
        return "nothing playing".to_string();
    }
    let dur = s
        .duration_ms
        .map(|d| format!("{}:{:02}", d / 60_000, (d / 1000) % 60))
        .unwrap_or_else(|| "live".to_string());
    format!(
        "'{}' — '{}' ('{}'), {dur}, {}",
        s.title,
        s.artist,
        s.album,
        if s.playing { "playing" } else { "paused" }
    )
}

// ── Coalescing publisher (service-loop side) ────────────────────────────────

/// What the service loop must send next.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    /// A 0x5001 for a state change; `track_changed` drives the INFO log.
    Update { params: Vec<(u16, Vec<u8>)>, track_changed: bool, summary: String },
    /// A 0x5001 announcing artwork, then a file transfer of `data` under `file_id`.
    Artwork { file_id: u8, params: Vec<(u16, Vec<u8>)>, data: Arc<Vec<u8>> },
}

/// Artwork bytes for one track (empty = "no image", as a real iPhone sends).
#[derive(Debug, Clone, PartialEq)]
pub struct ArtworkReady {
    pub key: ArtKey,
    pub jpeg: Arc<Vec<u8>>,
}

/// Holds the subscription, the latest unsent observation and the last sent
/// state. Pure: time comes in as arguments, so it is unit-tested directly.
#[derive(Debug)]
pub struct Publisher {
    app: AppIdentity,
    requested: Option<Requested>,
    last_sent: Option<Observed>,
    last_send_at: Option<Instant>,
    pending: Option<Observed>,
    pending_art: Option<ArtworkReady>,
    next_file_id: u8,
}

impl Publisher {
    pub fn new(app: AppIdentity) -> Self {
        Publisher {
            app,
            requested: None,
            last_sent: None,
            last_send_at: None,
            pending: None,
            pending_art: None,
            next_file_id: FIRST_DEVICE_FILE_ID,
        }
    }

    pub fn subscribed(&self) -> bool {
        self.requested.is_some()
    }

    pub fn wants_artwork(&self) -> bool {
        self.requested.as_ref().is_some_and(Requested::wants_artwork)
    }

    /// 0x5000 arrived and its reset reply ("Stopped", empty) just went out at `now`.
    pub fn on_subscribe(&mut self, requested: Requested, now: Instant) {
        self.requested = Some(requested);
        self.last_sent = Some(Observed { snap: MediaSnapshot::default(), at: now });
        self.last_send_at = Some(now);
        self.pending = None;
        self.pending_art = None;
    }

    /// 0x5002: nothing more may be sent.
    pub fn on_stop(&mut self) {
        self.requested = None;
        self.pending = None;
        self.pending_art = None;
    }

    pub fn offer(&mut self, obs: Observed) {
        if self.subscribed() {
            self.pending = Some(obs);
        }
    }

    pub fn offer_artwork(&mut self, art: ArtworkReady) {
        if self.wants_artwork() {
            self.pending_art = Some(art);
        }
    }

    fn gate_open_at(&self) -> Option<Instant> {
        self.last_send_at.map(|t| t + MIN_UPDATE_GAP)
    }

    /// When [`poll`](Self::poll) next has something to do, if anything waits.
    pub fn next_deadline(&self) -> Option<Instant> {
        if !self.subscribed() || (self.pending.is_none() && self.pending_art.is_none()) {
            return None;
        }
        Some(self.gate_open_at().unwrap_or_else(Instant::now))
    }

    /// The next message to send at `now`, honouring the rate gate. Text before
    /// artwork; artwork for a track other than the one last sent is dropped.
    pub fn poll(&mut self, now: Instant) -> Option<Outgoing> {
        let req = self.requested.clone()?;
        if self.gate_open_at().is_some_and(|t| now < t) {
            return None;
        }
        if let Some(obs) = self.pending.take() {
            let change = diff(self.last_sent.as_ref(), &obs);
            if change.any() {
                let params = encode_update(&obs, now, &req, change, &self.app);
                let track_changed = change.media;
                let summary = describe(&obs.snap);
                // Re-anchor at `now` with the position we just announced, so the
                // next seek check compares against what the car extrapolates.
                let mut sent = obs;
                sent.snap.position_ms = sent.elapsed_at(now);
                sent.at = now;
                self.last_sent = Some(sent);
                self.last_send_at = Some(now);
                if !params.is_empty() {
                    return Some(Outgoing::Update { params, track_changed, summary });
                }
            }
        }
        if let Some(art) = self.pending_art.take() {
            let current = self.last_sent.as_ref().map(Observed::art_key);
            if current.as_ref() != Some(&art.key) || !self.last_sent.as_ref().is_some_and(|o| o.snap.has_track()) {
                debug!("NowPlaying: dropping artwork for a track that is no longer current");
                return None;
            }
            let file_id = self.alloc_file_id();
            self.last_send_at = Some(now);
            return Some(Outgoing::Artwork { file_id, params: encode_artwork_announce(file_id), data: art.jpeg });
        }
        None
    }

    /// Device-originated file ids are 128..=254 (catplay `FileTransferTransmitter`,
    /// "server" range; a real iPhone uses 0x80, 0x81). Rotating instead of reusing
    /// means a late reply for an abandoned transfer can never match a new one.
    fn alloc_file_id(&mut self) -> u8 {
        let id = self.next_file_id;
        self.next_file_id = if id >= LAST_DEVICE_FILE_ID { FIRST_DEVICE_FILE_ID } else { id + 1 };
        id
    }

    /// Heartbeat payload while something real was sent: the current playback
    /// attributes (status, elapsed, speed), so a beat never contradicts the
    /// display. `None` = nothing real yet; the caller keeps its old beat.
    pub fn heartbeat(&self, now: Instant) -> Option<Vec<(u16, Vec<u8>)>> {
        let req = self.requested.as_ref()?;
        let last = self.last_sent.as_ref().filter(|o| o.snap.has_track())?;
        let params = encode_update(last, now, req, Change { media: false, playback: true }, &self.app);
        (!params.is_empty()).then_some(params)
    }
}

// ── File transfer (artwork) ─────────────────────────────────────────────────

pub const FIRST_DEVICE_FILE_ID: u8 = 0x80;
pub const LAST_DEVICE_FILE_ID: u8 = 0xFE;

/// File transfer control byte values (iAP2 file-transfer session).
pub mod ft_op {
    pub const START: u8 = 0x01;
    pub const CANCEL: u8 = 0x02;
    pub const PAUSE: u8 = 0x03;
    pub const SETUP: u8 = 0x04;
    pub const SUCCESS: u8 = 0x05;
    pub const FAILURE: u8 = 0x06;
    pub const DATA_FIRST: u8 = 0x80;
    pub const DATA_MIDDLE: u8 = 0x00;
    pub const DATA_LAST: u8 = 0x40;
    pub const DATA_ONLY: u8 = 0xC0;
}

/// `FileTransferSetupMetaType::NowPlayingArtworkData`.
pub const FILE_TYPE_NOW_PLAYING_ARTWORK: u16 = 0x0002;

/// The Setup datagram: `[id][0x04][u64 BE size][u16 BE type]`.
pub fn setup_datagram(file_id: u8, size: u64, file_type: u16) -> Vec<u8> {
    let mut out = vec![file_id, ft_op::SETUP];
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&file_type.to_be_bytes());
    out
}

/// The data datagram for `data[offset..]`, at most `chunk` bytes, with the
/// first/last flags. Returns the datagram and the new offset.
pub fn data_datagram(file_id: u8, data: &[u8], offset: usize, chunk: usize) -> (Vec<u8>, usize) {
    let end = (offset + chunk.max(1)).min(data.len());
    // The data control byte is two flags: 0x80 = first chunk, 0x40 = last
    // chunk (so a single-chunk file is 0xC0 and a middle chunk 0x00).
    let mut op = ft_op::DATA_MIDDLE;
    if offset == 0 {
        op |= ft_op::DATA_FIRST;
    }
    if end >= data.len() {
        op |= ft_op::DATA_LAST;
    }
    let mut out = Vec::with_capacity(2 + end - offset);
    out.extend_from_slice(&[file_id, op]);
    out.extend_from_slice(&data[offset.min(end)..end]);
    (out, end)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FtState {
    /// Setup not sent yet.
    SetupDue,
    /// Setup sent; waiting for the car's Start.
    AwaitStart,
    Sending,
    /// The car sent Pause; wait for Start again.
    Paused,
    /// Last chunk sent; waiting for Success/Failure.
    AwaitResult,
}

#[derive(Debug)]
struct OutgoingFile {
    id: u8,
    data: Arc<Vec<u8>>,
    offset: usize,
    state: FtState,
    deadline: Option<Instant>,
}

/// One outgoing artwork transfer at a time; a newer one waits (latest wins).
#[derive(Debug, Default)]
pub struct FileTransfers {
    active: Option<OutgoingFile>,
    queued: Option<(u8, Arc<Vec<u8>>)>,
}

/// How a transfer ended, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtOutcome {
    Success,
    Failure,
    Cancelled,
    TimedOut,
}

impl FileTransfers {
    pub fn enqueue(&mut self, id: u8, data: Arc<Vec<u8>>) {
        if self.active.is_some() {
            self.queued = Some((id, data));
        } else {
            self.start(id, data);
        }
    }

    fn start(&mut self, id: u8, data: Arc<Vec<u8>>) {
        self.active = Some(OutgoingFile { id, data, offset: 0, state: FtState::SetupDue, deadline: None });
    }

    fn finish(&mut self) {
        self.active = None;
        if let Some((id, data)) = self.queued.take() {
            self.start(id, data);
        }
    }

    pub fn is_idle(&self) -> bool {
        self.active.is_none()
    }

    /// The next datagram to put on the File Transfer session, if one is due.
    pub fn next_datagram(&mut self, chunk: usize, now: Instant) -> Option<Vec<u8>> {
        let f = self.active.as_mut()?;
        match f.state {
            FtState::SetupDue => {
                f.state = FtState::AwaitStart;
                f.deadline = Some(now + FILE_REPLY_TIMEOUT);
                Some(setup_datagram(f.id, f.data.len() as u64, FILE_TYPE_NOW_PLAYING_ARTWORK))
            }
            FtState::Sending => {
                let (dg, end) = data_datagram(f.id, &f.data, f.offset, chunk);
                f.offset = end;
                if end >= f.data.len() {
                    f.state = FtState::AwaitResult;
                    f.deadline = Some(now + FILE_REPLY_TIMEOUT);
                }
                Some(dg)
            }
            _ => None,
        }
    }

    /// Feed a datagram the car sent on the File Transfer session. Replies for any
    /// id other than the active one are ignored, so a late answer to an abandoned
    /// transfer cannot drive the current one. Returns the outcome when the active
    /// transfer ended.
    pub fn on_datagram(&mut self, dg: &[u8]) -> Option<(u8, FtOutcome)> {
        let (&id, &op) = (dg.first()?, dg.get(1)?);
        let f = self.active.as_mut().filter(|f| f.id == id);
        let Some(f) = f else {
            debug!("iAP2 file transfer: ignoring op 0x{op:02X} for inactive id 0x{id:02X}");
            return None;
        };
        let outcome = match op {
            ft_op::START if matches!(f.state, FtState::AwaitStart | FtState::Paused) => {
                f.state = FtState::Sending;
                f.deadline = None;
                None
            }
            ft_op::PAUSE if f.state == FtState::Sending => {
                // Bounded: a car that never resumes must not block later artwork.
                f.state = FtState::Paused;
                f.deadline = Some(Instant::now() + FILE_PAUSE_TIMEOUT);
                None
            }
            ft_op::SUCCESS => Some(FtOutcome::Success),
            ft_op::FAILURE => Some(FtOutcome::Failure),
            ft_op::CANCEL => Some(FtOutcome::Cancelled),
            _ => {
                debug!("iAP2 file transfer: unexpected op 0x{op:02X} for id 0x{id:02X} in {:?}", f.state);
                None
            }
        };
        if let Some(o) = outcome {
            self.finish();
            return Some((id, o));
        }
        None
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.active.as_ref().and_then(|f| f.deadline)
    }

    /// Abandon the active transfer once its reply deadline passed. Nothing is
    /// sent to the car: an unanswered transfer is dropped, never retried.
    pub fn on_timeout(&mut self, now: Instant) -> Option<(u8, FtOutcome)> {
        let f = self.active.as_ref()?;
        if f.deadline.is_some_and(|d| now >= d) {
            let id = f.id;
            self.finish();
            return Some((id, FtOutcome::TimedOut));
        }
        None
    }
}

// ── Poller (task side) ──────────────────────────────────────────────────────

/// Minimal HTTP/1.0 GET against rascar-app on the loopback. HTTP/1.0 makes the
/// server close the connection after the body, so the body is "everything until
/// EOF" (chunked encoding is still decoded, defensively). Bounded by `limit`.
pub async fn http_get(addr: &str, path: &str, limit: usize) -> std::io::Result<(u16, Vec<u8>)> {
    use std::io::{Error, ErrorKind};
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > limit + 16 * 1024 {
            return Err(Error::new(ErrorKind::InvalidData, "response too large"));
        }
    }
    parse_http_response(&raw).ok_or_else(|| Error::new(ErrorKind::InvalidData, "malformed HTTP response"))
}

fn parse_http_response(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Some((status, body))
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..eol]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

/// Re-encode artwork to a baseline JPEG no larger than `ARTWORK_EDGE` square,
/// through ffmpeg (already a runtime dependency of this crate). Bounded, and
/// the child is killed if the future is dropped.
async fn downscale_to_jpeg(image: Vec<u8>) -> Option<Vec<u8>> {
    use std::process::Stdio;
    let filter =
        format!("scale=w='min({ARTWORK_EDGE},iw)':h='min({ARTWORK_EDGE},ih)':force_original_aspect_ratio=decrease");
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "image2pipe", "-i", "pipe:0", "-vf", &filter])
        .args(["-frames:v", "1", "-pix_fmt", "yuvj420p", "-q:v", "5", "-f", "image2pipe", "-vcodec", "mjpeg", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| warn!("NowPlaying artwork: cannot run ffmpeg: {e}"))
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&image).await;
    });
    let out = match timeout(Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            warn!("NowPlaying artwork: ffmpeg failed: {e}");
            return None;
        }
        Err(_) => {
            warn!("NowPlaying artwork: ffmpeg timed out");
            return None;
        }
    };
    writer.abort();
    if !out.status.success() || !out.stdout.starts_with(&[0xFF, 0xD8, 0xFF]) {
        warn!(
            "NowPlaying artwork: ffmpeg produced no JPEG ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    (out.stdout.len() <= MAX_ARTWORK_BYTES).then_some(out.stdout)
}

/// The artwork for `obs`, or empty bytes for "no image". Only local art is
/// reachable: rascar-app serves `file://` art at `/api/media/art` and refuses a
/// remote URL (and this crate carries no TLS client).
async fn fetch_artwork(addr: &str, obs: &Observed) -> Vec<u8> {
    let url = &obs.snap.art_url;
    if url.is_empty() {
        return Vec::new();
    }
    if !url.starts_with("file://") {
        info!("NowPlaying artwork: remote art URL is not served by rascar-app, sending no image ({url})");
        return Vec::new();
    }
    let image = match timeout(Duration::from_secs(2), http_get(addr, "/api/media/art", 16 * 1024 * 1024)).await {
        Ok(Ok((200, body))) if !body.is_empty() => body,
        Ok(Ok((status, _))) => {
            debug!("NowPlaying artwork: /api/media/art answered {status}");
            return Vec::new();
        }
        Ok(Err(e)) => {
            debug!("NowPlaying artwork: /api/media/art failed: {e}");
            return Vec::new();
        }
        Err(_) => {
            debug!("NowPlaying artwork: /api/media/art timed out");
            return Vec::new();
        }
    };
    downscale_to_jpeg(image).await.unwrap_or_default()
}

/// Aborts the wrapped task when dropped, so the poller ends with the session.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Poll rascar-app while `subscribed` is true and publish observations that
/// differ materially from the last published one, plus artwork per track when
/// `cfg.artwork`. Exits when the service loop drops its end of `subscribed`.
pub fn spawn_poller(
    cfg: Config,
    mut subscribed: watch::Receiver<bool>,
    text_tx: watch::Sender<Option<Observed>>,
    art_tx: watch::Sender<Option<ArtworkReady>>,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let mut last: Option<Observed> = None;
        let mut last_art: Option<ArtKey> = None;
        let mut _art_task: Option<AbortOnDrop> = None;
        let mut failures: u32 = 0;
        loop {
            while !*subscribed.borrow_and_update() {
                last = None;
                last_art = None;
                _art_task = None;
                if subscribed.changed().await.is_err() {
                    return;
                }
            }

            match timeout(POLL_TIMEOUT, http_get(&cfg.app_addr, "/api/media", 64 * 1024)).await {
                Ok(Ok((200, body))) => match MediaSnapshot::from_api_json(&body) {
                    Some(snap) => {
                        failures = 0;
                        let obs = Observed { snap, at: Instant::now() };
                        if diff(last.as_ref(), &obs).any() {
                            text_tx.send_replace(Some(obs.clone()));
                            last = Some(obs.clone());
                        }
                        let key = obs.art_key();
                        if cfg.artwork && obs.snap.has_track() && last_art.as_ref() != Some(&key) {
                            last_art = Some(key.clone());
                            let (art_tx, addr) = (art_tx.clone(), cfg.app_addr.clone());
                            // A newer track aborts an older fetch (drop = abort).
                            _art_task = Some(AbortOnDrop(tokio::spawn(async move {
                                let jpeg = fetch_artwork(&addr, &obs).await;
                                info!("NowPlaying artwork: {} byte(s) ready for '{}'", jpeg.len(), key.title);
                                art_tx.send_replace(Some(ArtworkReady { key, jpeg: Arc::new(jpeg) }));
                            })));
                        }
                    }
                    None => debug!("NowPlaying: /api/media returned unparseable JSON"),
                },
                other => {
                    failures += 1;
                    // One warning per outage, not one per second.
                    if failures == 3 {
                        warn!("NowPlaying: rascar-app /api/media unreachable ({other:?}); keeping the last state");
                    } else {
                        debug!("NowPlaying: /api/media poll failed: {other:?}");
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                r = subscribed.changed() => if r.is_err() { return; },
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iap2::control::{encode_control_message, parse_control_message};

    /// The Jaguar's real StartNowPlayingUpdates, captured 2026-10-09.
    const CAR_5000: &str = "4040005a5000001c00000004000100040004000400060004000c00040012\
        0004001a0038000100040000000400010004000200040003000400040004000500040006000400070004000c0004000d0004000e0004000f00040010";

    fn car_request() -> Requested {
        let raw = hex::decode(CAR_5000.replace(' ', "")).unwrap();
        Requested::from_start_message(&parse_control_message(&raw).unwrap())
    }

    fn app() -> AppIdentity {
        AppIdentity { name: "X".into(), bundle_id: "b.x".into() }
    }

    fn snap(title: &str, playing: bool, pos: u32) -> MediaSnapshot {
        MediaSnapshot {
            title: title.into(),
            artist: "A".into(),
            album: "B".into(),
            art_url: String::new(),
            playing,
            position_ms: Some(pos),
            duration_ms: Some(180_000),
            shuffle: false,
        }
    }

    fn encode(params: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let b: Vec<(u16, &[u8])> = params.iter().map(|(i, d)| (*i, d.as_slice())).collect();
        encode_control_message(0x5001, &b).to_vec()
    }

    #[test]
    fn parses_the_cars_real_request() {
        let r = car_request();
        assert_eq!(r.media, Some(vec![0x01, 0x04, 0x06, 0x0C, 0x12, 0x1A]));
        assert_eq!(r.playback, Some(vec![0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16]));
        assert!(r.wants_artwork());
    }

    /// Byte-exact track-change update, hand-assembled from the TLV rules
    /// (len counts its 4-byte header; strings UTF-8 + NUL; u32 BE durations).
    #[test]
    fn track_change_update_is_byte_exact() {
        let now = Instant::now();
        let obs = Observed { snap: snap("Hey", true, 1_000), at: now };
        let params = encode_update(&obs, now, &car_request(), Change { media: true, playback: true }, &app());
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0x40, 0x40, 0x00, 0x7C, 0x50, 0x01,             // marker, len 124, NowPlayingUpdate
            0x00, 0x25, 0x00, 0x00,                         // MediaItemAttributes group, len 37
              0x00, 0x08, 0x00, 0x01, b'H', b'e', b'y', 0,  //   Title "Hey"
              0x00, 0x08, 0x00, 0x04, 0x00, 0x02, 0xBF, 0x20, // Duration 180000 ms
              0x00, 0x06, 0x00, 0x06, b'B', 0,              //   AlbumTitle "B"
              0x00, 0x06, 0x00, 0x0C, b'A', 0,              //   Artist "A"
              0x00, 0x05, 0x00, 0x12, 0,                    //   Composer ""
            0x00, 0x51, 0x00, 0x01,                         // PlaybackAttributes group, len 81
              0x00, 0x05, 0x00, 0x00, 0x01,                 //   Status Playing
              0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x03, 0xE8, // Elapsed 1000 ms
              0x00, 0x08, 0x00, 0x02, 0, 0, 0, 0,           //   QueueIndex 0
              0x00, 0x08, 0x00, 0x03, 0, 0, 0, 1,           //   QueueCount 1
              0x00, 0x08, 0x00, 0x04, 0, 0, 0, 0,           //   QueueChapterIndex 0
              0x00, 0x05, 0x00, 0x05, 0,                    //   Shuffle Off
              0x00, 0x05, 0x00, 0x06, 0,                    //   Repeat Off
              0x00, 0x06, 0x00, 0x07, b'X', 0,              //   AppName "X"
              0x00, 0x06, 0x00, 0x0C, 0x00, 0x64,           //   Speed 100
              0x00, 0x05, 0x00, 0x0D, 0,                    //   SetElapsedTimeAvailable false
              0x00, 0x05, 0x00, 0x0E, 0,                    //   QueueListAvail false
              0x00, 0x08, 0x00, 0x10, b'b', b'.', b'x', 0,  //   AppBundleID "b.x"
        ];
        assert_eq!(encode(&params), expected);
        // And it parses back as two groups.
        assert_eq!(parse_control_message(&expected).unwrap().params.len(), 2);
    }

    #[test]
    fn pause_update_carries_only_the_playback_essentials() {
        let now = Instant::now();
        let obs = Observed { snap: snap("Hey", false, 65_000), at: now };
        let params = encode_update(&obs, now, &car_request(), Change { media: false, playback: true }, &app());
        #[rustfmt::skip]
        assert_eq!(encode(&params), vec![
            0x40, 0x40, 0x00, 0x22, 0x50, 0x01,
            0x00, 0x1C, 0x00, 0x01,
              0x00, 0x05, 0x00, 0x00, 0x02,                     // Paused
              0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0xFD, 0xE8,   // 65000 ms
              0x00, 0x05, 0x00, 0x05, 0x00,                     // Shuffle Off
              0x00, 0x06, 0x00, 0x0C, 0x00, 0x00,               // Speed 0
        ]);
    }

    #[test]
    fn only_requested_attributes_are_sent() {
        let now = Instant::now();
        let req = Requested { media: Some(vec![pid::media::TITLE]), playback: None };
        let obs = Observed { snap: snap("Hey", true, 0), at: now };
        let params = encode_update(&obs, now, &req, Change { media: true, playback: true }, &app());
        assert_eq!(params, vec![(0x0000, vec![0x00, 0x08, 0x00, 0x01, b'H', b'e', b'y', 0])]);
    }

    #[test]
    fn strings_are_nul_terminated_and_bounded() {
        assert_eq!(csm_string("é"), vec![0xC3, 0xA9, 0]);
        assert_eq!(csm_string("a\0b"), b"ab\0".to_vec());
        let long = "é".repeat(200); // 400 bytes
        let s = csm_string(&long);
        assert!(s.len() <= MAX_STRING_BYTES + 1 && std::str::from_utf8(&s[..s.len() - 1]).is_ok());
    }

    #[test]
    fn parses_the_apps_json() {
        let s = MediaSnapshot::from_api_json(
            br#"{"album":"Al","art_url":"file:///x.jpg","artist":"Ar","is_playing":true,"length_sec":215.4,"player_name":"p","position_sec":12.5,"shuffle":true,"title":"T"}"#,
        )
        .unwrap();
        assert_eq!((s.position_ms, s.duration_ms, s.playing, s.shuffle), (Some(12_500), Some(215_400), true, true));
        let idle = MediaSnapshot::from_api_json(
            br#"{"album":"","art_url":"","artist":"","is_playing":false,"length_sec":null,"player_name":"","position_sec":null,"shuffle":false,"title":""}"#,
        )
        .unwrap();
        assert_eq!(idle.status(), STATUS_STOPPED);
        assert_eq!(idle.duration_ms, None);
    }

    #[test]
    fn diff_ignores_normal_progress_and_catches_real_changes() {
        let t0 = Instant::now();
        let a = Observed { snap: snap("One", true, 10_000), at: t0 };
        // 1 s later, 1 s further: nothing to say.
        let b = Observed { snap: snap("One", true, 11_050), at: t0 + Duration::from_secs(1) };
        assert!(!diff(Some(&a), &b).any());
        // Jumped 30 s: a seek, playback group only.
        let c = Observed { snap: snap("One", true, 41_000), at: t0 + Duration::from_secs(1) };
        assert_eq!(diff(Some(&a), &c), Change { media: false, playback: true });
        // Paused.
        let d = Observed { snap: snap("One", false, 11_000), at: t0 + Duration::from_secs(1) };
        assert_eq!(diff(Some(&a), &d), Change { media: false, playback: true });
        // New track.
        let e = Observed { snap: snap("Two", true, 0), at: t0 + Duration::from_secs(1) };
        assert_eq!(diff(Some(&a), &e), Change { media: true, playback: true });
        // Paused and still paused: position does not extrapolate.
        let f = Observed { snap: snap("One", false, 11_000), at: t0 + Duration::from_secs(5) };
        assert!(!diff(Some(&d), &f).any());
        // Shuffle toggled.
        let mut g = b.clone();
        g.snap.shuffle = true;
        assert_eq!(diff(Some(&b), &g), Change { media: false, playback: true });
    }

    #[test]
    fn publisher_coalesces_to_one_update_per_gap_latest_wins() {
        let t0 = Instant::now();
        let mut p = Publisher::new(app());
        assert!(p.poll(t0).is_none(), "nothing before a subscription");
        p.offer(Observed { snap: snap("One", true, 0), at: t0 });
        assert!(p.poll(t0).is_none(), "offers before 0x5000 are dropped");

        p.on_subscribe(car_request(), t0);
        p.offer(Observed { snap: snap("One", true, 0), at: t0 });
        p.offer(Observed { snap: snap("Two", true, 0), at: t0 + Duration::from_millis(100) });
        // The reset reply just went out: gate closed for 500 ms.
        assert!(p.poll(t0 + Duration::from_millis(200)).is_none());
        assert_eq!(p.next_deadline(), Some(t0 + MIN_UPDATE_GAP));
        let t1 = t0 + MIN_UPDATE_GAP;
        match p.poll(t1) {
            Some(Outgoing::Update { track_changed: true, summary, .. }) => assert!(summary.contains("Two")),
            other => panic!("expected the latest track, got {other:?}"),
        }
        assert!(p.poll(t1 + Duration::from_secs(5)).is_none(), "nothing pending");
        // Same state again: no update even with the gate open.
        p.offer(Observed { snap: snap("Two", true, 2_000), at: t1 + Duration::from_secs(2) });
        assert!(p.poll(t1 + Duration::from_secs(2)).is_none());
        // Stop: nothing more, ever.
        p.on_stop();
        p.offer(Observed { snap: snap("Three", true, 0), at: t1 + Duration::from_secs(3) });
        assert!(p.poll(t1 + Duration::from_secs(4)).is_none());
        assert!(p.heartbeat(t1 + Duration::from_secs(4)).is_none());
    }

    #[test]
    fn artwork_follows_the_text_and_stale_artwork_is_dropped() {
        let t0 = Instant::now();
        let mut p = Publisher::new(app());
        p.on_subscribe(car_request(), t0);
        let one = Observed { snap: snap("One", true, 0), at: t0 };
        let t1 = t0 + MIN_UPDATE_GAP;
        p.offer(one.clone());
        assert!(matches!(p.poll(t1), Some(Outgoing::Update { .. })));
        p.offer_artwork(ArtworkReady { key: one.art_key(), jpeg: Arc::new(vec![1, 2, 3]) });
        assert!(p.poll(t1 + Duration::from_millis(100)).is_none(), "gate");
        let t2 = t1 + MIN_UPDATE_GAP;
        match p.poll(t2) {
            Some(Outgoing::Artwork { file_id: 0x80, params, data }) => {
                assert_eq!(encode(&params), vec![0x40, 0x40, 0x00, 0x0F, 0x50, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x05, 0x00, 0x1A, 0x80]);
                assert_eq!(*data, vec![1, 2, 3]);
            }
            other => panic!("expected artwork, got {other:?}"),
        }
        // Artwork for a track that is no longer current is dropped.
        p.offer_artwork(ArtworkReady {
            key: Observed { snap: snap("Old", true, 0), at: t0 }.art_key(),
            jpeg: Arc::new(vec![9]),
        });
        assert!(p.poll(t2 + Duration::from_secs(1)).is_none());
        // Ids rotate within the device range.
        let mut q = Publisher::new(app());
        let ids: Vec<u8> = (0..128).map(|_| q.alloc_file_id()).collect();
        assert_eq!((ids[0], ids[126], ids[127]), (0x80, 0xFE, 0x80));
    }

    #[test]
    fn heartbeat_repeats_the_current_playback_state_only() {
        let t0 = Instant::now();
        let mut p = Publisher::new(app());
        p.on_subscribe(car_request(), t0);
        assert!(p.heartbeat(t0).is_none(), "nothing real sent yet");
        p.offer(Observed { snap: snap("One", true, 1_000), at: t0 });
        p.poll(t0 + MIN_UPDATE_GAP).unwrap();
        let hb = p.heartbeat(t0 + MIN_UPDATE_GAP + Duration::from_secs(2)).unwrap();
        assert_eq!(hb.len(), 1);
        assert_eq!(hb[0].0, pid::PLAYBACK_ATTRIBUTES);
        // Elapsed extrapolated: 1000 ms read at t0, sent at t0+500 ms (1500 ms),
        // beat 2 s later = 3500 ms.
        let g = inner_tlvs(&hb[0].1);
        assert_eq!(g[1], (pid::playback::ELAPSED_MS, &3_500u32.to_be_bytes()[..]));
    }

    #[test]
    fn file_transfer_framing() {
        assert_eq!(
            setup_datagram(0x80, 0x1234, FILE_TYPE_NOW_PLAYING_ARTWORK),
            vec![0x80, 0x04, 0, 0, 0, 0, 0, 0, 0x12, 0x34, 0x00, 0x02]
        );
        let data: Vec<u8> = (0..10).collect();
        let (a, o) = data_datagram(0x81, &data, 0, 4);
        let (b, o) = data_datagram(0x81, &data, o, 4);
        let (c, o) = data_datagram(0x81, &data, o, 4);
        assert_eq!(a, vec![0x81, 0x80, 0, 1, 2, 3]);
        assert_eq!(b, vec![0x81, 0x00, 4, 5, 6, 7]);
        assert_eq!(c, vec![0x81, 0x40, 8, 9]);
        assert_eq!(o, 10);
        assert_eq!(data_datagram(0x82, &data, 0, 64).0[..2], [0x82, 0xC0]);
        assert_eq!(data_datagram(0x83, &[], 0, 64).0, vec![0x83, 0xC0], "zero-size = one empty only-chunk");
    }

    #[test]
    fn file_transfer_state_machine() {
        let t0 = Instant::now();
        let mut ft = FileTransfers::default();
        ft.enqueue(0x80, Arc::new((0..10).collect()));
        ft.enqueue(0x81, Arc::new(vec![7])); // queued behind 0x80
        assert_eq!(ft.next_datagram(4, t0).unwrap()[..2], [0x80, 0x04]);
        assert!(ft.next_datagram(4, t0).is_none(), "waits for Start");
        // A late Start for some other id must not move this transfer.
        assert!(ft.on_datagram(&[0x7F, ft_op::START]).is_none());
        assert!(ft.next_datagram(4, t0).is_none());
        ft.on_datagram(&[0x80, ft_op::START]);
        assert_eq!(ft.next_datagram(4, t0).unwrap()[1], ft_op::DATA_FIRST);
        ft.on_datagram(&[0x80, ft_op::PAUSE]);
        assert!(ft.next_datagram(4, t0).is_none(), "paused");
        ft.on_datagram(&[0x80, ft_op::START]);
        assert_eq!(ft.next_datagram(4, t0).unwrap()[1], ft_op::DATA_MIDDLE);
        assert_eq!(ft.next_datagram(4, t0).unwrap()[1], ft_op::DATA_LAST);
        assert!(ft.next_datagram(4, t0).is_none(), "awaiting the result");
        assert_eq!(ft.on_datagram(&[0x80, ft_op::SUCCESS]), Some((0x80, FtOutcome::Success)));
        // The queued transfer starts; with no reply it is abandoned, silently.
        assert_eq!(ft.next_datagram(4, t0).unwrap(), setup_datagram(0x81, 1, 2));
        assert!(ft.on_timeout(t0 + Duration::from_secs(1)).is_none());
        assert_eq!(ft.on_timeout(t0 + FILE_REPLY_TIMEOUT), Some((0x81, FtOutcome::TimedOut)));
        assert!(ft.is_idle());
        assert!(ft.next_datagram(4, t0).is_none());
    }

    #[test]
    fn http_response_parsing() {
        assert_eq!(
            parse_http_response(b"HTTP/1.0 200 OK\r\ncontent-type: a\r\n\r\n{\"x\":1}"),
            Some((200, b"{\"x\":1}".to_vec()))
        );
        assert_eq!(
            parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x\r\nde\r\n0\r\n\r\n"),
            Some((200, b"abcde".to_vec()))
        );
        assert_eq!(parse_http_response(b"garbage"), None);
    }

    #[tokio::test]
    async fn http_get_reads_a_local_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            assert!(buf[..n].starts_with(b"GET /api/media HTTP/1.0\r\n"));
            s.write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"title\":\"T\",\"is_playing\":true}").await.unwrap();
        });
        let (status, body) = http_get(&addr, "/api/media", 1024).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(MediaSnapshot::from_api_json(&body).unwrap().title, "T");
    }
}
