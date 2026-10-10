pub mod video;
pub mod audio;
pub mod input;

/// Wrap a payload in the AA media-data frame header: msg_type(2) + timestamp(8).
/// Shared by the video and audio senders so the header layout lives in one place.
pub fn encode_media_frame(msg_type: u16, payload: &[u8], timestamp_us: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10 + payload.len());
    buf.extend_from_slice(&msg_type.to_be_bytes());
    buf.extend_from_slice(&timestamp_us.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}
