//! What travels between the head unit and the dashboard page (`aa.html`).
//!
//! Server → page, binary (one H.264 access unit per message, no transcode):
//!   `[0] = 1 (video)`, `[1] = flags` (bit0 CONFIG = SPS/PPS only,
//!   bit1 KEY = contains an IDR), `[2..10] = timestamp µs (u64 BE)`,
//!   `[10..] = Annex-B bytes exactly as the phone sent them`.
//! Server → page, text: JSON status `{"type":"status", ...}`.
//! Page → server, text: JSON `{"type":"touch","action":"down|move|up|cancel",
//!   "id":<pointerId>,"x":0..1,"y":0..1}` with x/y normalised over the shown
//!   content, and `{"type":"keyframe"}` when its decoder needs to restart.
//!
//! The GOP cache lets a page that (re)connects mid-stream start decoding at
//! once: it gets the last SPS/PPS + the frames since the last IDR. Phones send
//! IDRs rarely, so when the cache overflowed the runtime asks the phone for a
//! new one instead (video focus toggle, see `Session::set_video_focus`).

use crate::aa::config::{HuConfig, Rect};
use crate::aa::session::Phase;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const KIND_VIDEO: u8 = 1;
pub const FLAG_CONFIG: u8 = 0x01;
pub const FLAG_KEY: u8 = 0x02;

/// One video message, shared by every connected page.
#[derive(Debug)]
pub struct VideoMsg {
    pub seq: u64,
    pub key: bool,
    pub config: bool,
    /// Ready-to-send WebSocket payload (cheap to clone per page).
    pub bytes: Bytes,
}

/// NAL unit types present in an Annex-B buffer (3- or 4-byte start codes).
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(&h) = data.get(i + 3) {
                types.push(h & 0x1F);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

pub fn encode_video(seq: u64, config: bool, timestamp_us: u64, data: &[u8]) -> VideoMsg {
    let types = nal_types(data);
    let key = types.contains(&5);
    // An access unit that carries its own SPS counts as config too.
    let config = config || (types.contains(&7) && !key);
    let mut bytes = Vec::with_capacity(10 + data.len());
    bytes.push(KIND_VIDEO);
    bytes.push((if config { FLAG_CONFIG } else { 0 }) | (if key { FLAG_KEY } else { 0 }));
    bytes.extend_from_slice(&timestamp_us.to_be_bytes());
    bytes.extend_from_slice(data);
    VideoMsg { seq, key, config, bytes: Bytes::from(bytes) }
}

/// Last codec config + every frame since the last IDR, bounded.
pub struct GopCache {
    config: Option<Arc<VideoMsg>>,
    frames: Vec<Arc<VideoMsg>>,
    bytes: usize,
    cap: usize,
    /// Frame cap: a joining page decodes the whole cached GOP at once, so a
    /// long GOP means seconds of catch-up CPU. Past this, ask for a keyframe.
    cap_frames: usize,
    /// False after an overflow (or before the first IDR): a joining page
    /// cannot be served from the cache and needs a fresh keyframe.
    valid: bool,
    last_seq: u64,
}

impl GopCache {
    pub fn new(cap_bytes: usize, cap_frames: usize) -> Self {
        Self { config: None, frames: Vec::new(), bytes: 0, cap: cap_bytes, cap_frames, valid: false, last_seq: 0 }
    }

    pub fn push(&mut self, m: Arc<VideoMsg>) {
        self.last_seq = m.seq;
        if m.config {
            self.config = Some(m);
            return;
        }
        if m.key {
            self.frames.clear();
            self.bytes = 0;
            self.valid = true;
        }
        if !self.valid {
            return;
        }
        self.bytes += m.bytes.len();
        self.frames.push(m);
        if self.bytes > self.cap || self.frames.len() > self.cap_frames {
            self.frames.clear();
            self.bytes = 0;
            self.valid = false;
        }
    }

    /// Forget everything (new phone session: old SPS/PPS are meaningless).
    pub fn reset(&mut self) {
        self.config = None;
        self.frames.clear();
        self.bytes = 0;
        self.valid = false;
    }

    /// Messages that let a new page start decoding, and the sequence number
    /// they reach up to (forward only live messages with a higher seq).
    /// `None` when the cache cannot serve a decodable start.
    pub fn snapshot(&self) -> (Option<Vec<Arc<VideoMsg>>>, u64) {
        match (&self.config, self.valid) {
            (Some(c), true) => {
                let mut v = Vec::with_capacity(self.frames.len() + 1);
                v.push(c.clone());
                v.extend(self.frames.iter().cloned());
                (Some(v), self.last_seq)
            }
            _ => (None, self.last_seq),
        }
    }
}

/// Status pushed to the page as JSON text.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Status {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// "waiting" (no phone), or the protocol phase.
    pub state: String,
    pub phone: Option<String>,
    pub streaming: bool,
    pub width: u32,
    pub height: u32,
    /// Part of the frame the phone draws into; the page shows only this.
    pub crop: Rect,
    pub error: Option<String>,
}

impl Status {
    pub fn new(cfg: &HuConfig) -> Self {
        let (w, h) = cfg.resolution.size();
        Self {
            kind: "status",
            state: "waiting".into(),
            phone: None,
            streaming: false,
            width: w,
            height: h,
            crop: cfg.content_rect(),
            error: None,
        }
    }

    pub fn with_phase(mut self, p: Phase) -> Self {
        self.state = serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        self
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PageMsg {
    Touch { action: String, id: u32, x: f64, y: f64 },
    Keyframe,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_PPS: [u8; 14] = [0, 0, 0, 1, 0x67, 0x42, 0xE0, 0x1F, 0, 0, 1, 0x68, 0xCE, 0x3C];
    const IDR: [u8; 6] = [0, 0, 0, 1, 0x65, 0x88];
    const P: [u8; 6] = [0, 0, 0, 1, 0x41, 0x9A];

    #[test]
    fn nal_type_scan_handles_3_and_4_byte_start_codes() {
        assert_eq!(nal_types(&SPS_PPS), vec![7, 8]);
        assert_eq!(nal_types(&IDR), vec![5]);
        assert_eq!(nal_types(&[0, 0, 1]), Vec::<u8>::new());
    }

    #[test]
    fn video_message_header() {
        let m = encode_video(1, false, 0x0102030405060708, &IDR);
        assert!(m.key && !m.config);
        assert_eq!(&m.bytes[..10], &[1, FLAG_KEY, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&m.bytes[10..], &IDR);
        let c = encode_video(2, true, 0, &SPS_PPS);
        assert_eq!(c.bytes[1], FLAG_CONFIG);
    }

    fn arc(seq: u64, config: bool, d: &[u8]) -> Arc<VideoMsg> {
        Arc::new(encode_video(seq, config, seq, d))
    }

    #[test]
    fn gop_cache_serves_config_plus_frames_since_idr() {
        let mut g = GopCache::new(1 << 20, 1000);
        assert!(g.snapshot().0.is_none());
        g.push(arc(1, true, &SPS_PPS));
        g.push(arc(2, false, &P)); // before any IDR: unusable
        assert!(g.snapshot().0.is_none());
        g.push(arc(3, false, &IDR));
        g.push(arc(4, false, &P));
        let (snap, seq) = g.snapshot();
        let seqs: Vec<u64> = snap.unwrap().iter().map(|m| m.seq).collect();
        assert_eq!((seqs, seq), (vec![1, 3, 4], 4));
        g.push(arc(5, false, &IDR)); // new GOP drops the old frames
        assert_eq!(g.snapshot().0.unwrap().iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 5]);
    }

    #[test]
    fn gop_cache_overflow_invalidates_until_next_idr() {
        let mut g = GopCache::new(40, 1000);
        g.push(arc(1, true, &SPS_PPS));
        g.push(arc(2, false, &IDR));
        for s in 3..10 {
            g.push(arc(s, false, &P));
        }
        assert!(g.snapshot().0.is_none(), "overflowed cache must not serve a partial GOP");
        g.push(arc(10, false, &IDR));
        assert!(g.snapshot().0.is_some());
        g.reset();
        assert!(g.snapshot().0.is_none());
    }

    #[test]
    fn gop_cache_frame_cap() {
        let mut g = GopCache::new(1 << 20, 3);
        g.push(arc(1, true, &SPS_PPS));
        g.push(arc(2, false, &IDR));
        g.push(arc(3, false, &P));
        g.push(arc(4, false, &P));
        assert!(g.snapshot().0.is_some());
        g.push(arc(5, false, &P));
        assert!(g.snapshot().0.is_none());
    }

    #[test]
    fn page_messages_parse() {
        let t: PageMsg = serde_json::from_str(r#"{"type":"touch","action":"down","id":3,"x":0.25,"y":0.5}"#).unwrap();
        assert_eq!(t, PageMsg::Touch { action: "down".into(), id: 3, x: 0.25, y: 0.5 });
        let k: PageMsg = serde_json::from_str(r#"{"type":"keyframe"}"#).unwrap();
        assert_eq!(k, PageMsg::Keyframe);
        assert!(serde_json::from_str::<PageMsg>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn status_json_shape() {
        let s = Status::new(&HuConfig::default()).with_phase(Phase::Running);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "status");
        assert_eq!(v["state"], "running");
        assert_eq!(v["crop"]["y"], 95);
        assert_eq!(v["width"], 1280);
    }
}
