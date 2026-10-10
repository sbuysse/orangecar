//! Lyrics for the karaoke view, synced whenever any source has timestamps.
//!
//! The dashboard asked LRCLIB once and took its first hit, synced or not, on
//! every open. AndroidAuto2 walked several sources, preferred the first one
//! WITH timestamps and cached the answer on disk ("not found" included). Here:
//!   1. LRCLIB `/api/get` — exact match on title/artist/duration (when known);
//!   2. LRCLIB `/api/search` — the first result that has synced lyrics;
//!   3. YouTube Music's own lyrics for the exact videoId playing (often
//!      timestamped; see `ytmusic_helper.py lyrics`);
//!   4. plain lyrics from 2 or 3, unsynced.
//! Answers are cached under `~/.cache/rascar/lyrics/`; a miss is re-tried
//! after `MISS_TTL`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::Digest;
use tracing::{info, warn};

use super::music::MusicService;

const MISS_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub time_sec: Option<f64>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Lyrics {
    pub lines: Vec<Line>,
    pub synced: bool,
    pub source: String,
}

pub struct LyricsService {
    http: reqwest::Client,
    music: Arc<MusicService>,
    dir: PathBuf,
}

impl LyricsService {
    pub fn new(music: Arc<MusicService>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("RasCar car dashboard")
            .build()
            .unwrap_or_default();
        let dir = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("rascar").join("lyrics");
        Self { http, music, dir }
    }

    pub async fn get(&self, title: &str, artist: &str, video_id: Option<&str>, duration: Option<f64>) -> Lyrics {
        let path = self.dir.join(format!("{}.json", cache_key(title, artist)));
        if let Some(hit) = read_cache(&path) {
            // A cached miss from a lookup without a videoId: YouTube Music's own
            // lyrics were never asked, so try again now that we can.
            if !(hit.lines.is_empty() && video_id.is_some()) {
                return hit;
            }
        }
        let found = self.fetch(title, artist, video_id, duration).await;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec(&found) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        info!("lyrics: {artist} – {title}: {} lines, synced={} ({})", found.lines.len(), found.synced, found.source);
        found
    }

    async fn fetch(&self, title: &str, artist: &str, video_id: Option<&str>, duration: Option<f64>) -> Lyrics {
        let mut plain: Option<Lyrics> = None;
        // 1. Exact match (LRCLIB wants the duration within a couple of seconds).
        if let Some(d) = duration.filter(|d| *d > 0.0) {
            let url = format!(
                "https://lrclib.net/api/get?track_name={}&artist_name={}&duration={}",
                urlencoding::encode(title), urlencoding::encode(artist), d.round() as u64
            );
            if let Some(v) = self.json(&url).await {
                if let Some(l) = from_lrclib(&v, "LRCLIB") {
                    if l.synced {
                        return l;
                    }
                    plain.get_or_insert(l);
                }
            }
        }
        // 2. Search: the first result that is synced (not merely the first).
        let url = format!(
            "https://lrclib.net/api/search?track_name={}&artist_name={}",
            urlencoding::encode(title), urlencoding::encode(artist)
        );
        if let Some(serde_json::Value::Array(results)) = self.json(&url).await {
            for r in &results {
                if let Some(l) = from_lrclib(r, "LRCLIB") {
                    if l.synced {
                        return l;
                    }
                    plain.get_or_insert(l);
                }
            }
        }
        // 3. YouTube Music's own lyrics for the track playing.
        if let Some(id) = video_id {
            match self.music.lyrics(id).await {
                Ok(v) => {
                    if let Ok(l) = serde_json::from_value::<Lyrics>(v) {
                        if !l.lines.is_empty() {
                            if l.synced {
                                return l;
                            }
                            plain.get_or_insert(l);
                        }
                    }
                }
                Err(e) => warn!("lyrics: YouTube Music lyrics failed: {e:#}"),
            }
        }
        plain.unwrap_or_default()
    }

    async fn json(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }
}

fn cache_key(title: &str, artist: &str) -> String {
    let norm = |s: &str| s.trim().to_lowercase();
    let h = sha2::Sha256::digest(format!("{}\u{1f}{}", norm(artist), norm(title)).as_bytes());
    h.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// A cached answer; a cached miss only while younger than `MISS_TTL`.
fn read_cache(path: &std::path::Path) -> Option<Lyrics> {
    let l: Lyrics = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if l.lines.is_empty() {
        let age = std::fs::metadata(path).ok()?.modified().ok()?;
        if SystemTime::now().duration_since(age).unwrap_or_default() > MISS_TTL {
            return None;
        }
    }
    Some(l)
}

/// One LRCLIB record → lyrics (synced preferred), or None when it has none.
fn from_lrclib(v: &serde_json::Value, source: &str) -> Option<Lyrics> {
    if v["instrumental"].as_bool() == Some(true) {
        return Some(Lyrics { lines: vec![Line { time_sec: None, text: "Instrumental".into() }], synced: false, source: source.into() });
    }
    if let Some(s) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let lines: Vec<Line> = s.lines().filter_map(parse_lrc_line).collect();
        if lines.iter().any(|l| l.time_sec.is_some()) {
            return Some(Lyrics { lines, synced: true, source: source.into() });
        }
    }
    let p = v["plainLyrics"].as_str().filter(|s| !s.trim().is_empty())?;
    let lines = p.lines().map(str::trim).filter(|t| !t.is_empty()).map(|t| Line { time_sec: None, text: t.into() }).collect();
    Some(Lyrics { lines, synced: false, source: source.into() })
}

/// `[mm:ss.xx] text` → timed line; metadata tags and empty lines are dropped.
pub fn parse_lrc_line(line: &str) -> Option<Line> {
    let (time_sec, text) = match line.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((stamp, rest)) => (parse_lrc_timestamp(stamp), rest.trim()),
        None => (None, line.trim()),
    };
    if text.is_empty() {
        return None;
    }
    Some(Line { time_sec, text: text.to_string() })
}

fn parse_lrc_timestamp(stamp: &str) -> Option<f64> {
    let parts: Vec<&str> = stamp.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for p in &parts {
        total = total * 60.0 + p.trim().parse::<f64>().ok()?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_lines_parse() {
        assert_eq!(parse_lrc_line("[00:33.55] We're talking away"), Some(Line { time_sec: Some(33.55), text: "We're talking away".into() }));
        // Metadata tags carry no lyric text: dropped.
        assert_eq!(parse_lrc_line("[ar:a-ha]"), None);
        assert_eq!(parse_lrc_line("[01:02:03.5] x").unwrap().time_sec, Some(3723.5));
        assert!(parse_lrc_line("[00:10.00]   ").is_none());
    }

    #[test]
    fn lrclib_prefers_synced_and_flags_instrumental() {
        let v = serde_json::json!({ "syncedLyrics": "[00:01.00] a\n[00:02.00] b", "plainLyrics": "a\nb" });
        let l = from_lrclib(&v, "LRCLIB").unwrap();
        assert!(l.synced);
        assert_eq!(l.lines.len(), 2);
        let p = from_lrclib(&serde_json::json!({ "syncedLyrics": null, "plainLyrics": "x\n\ny" }), "LRCLIB").unwrap();
        assert!(!p.synced);
        assert_eq!(p.lines.len(), 2);
        let i = from_lrclib(&serde_json::json!({ "instrumental": true }), "LRCLIB").unwrap();
        assert_eq!(i.lines[0].text, "Instrumental");
        assert!(from_lrclib(&serde_json::json!({}), "LRCLIB").is_none());
    }

    #[test]
    fn cache_keys_ignore_case_and_spaces() {
        assert_eq!(cache_key("Take On Me ", "a-ha"), cache_key("take on me", " A-HA"));
        assert_ne!(cache_key("Take On Me", "a-ha"), cache_key("Hunting High and Low", "a-ha"));
    }
}
