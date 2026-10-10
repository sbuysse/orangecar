//! Android Auto transport layer — frame encoding/decoding.
//!
//! AA protocol uses a simple frame header over USB bulk transfers:
//!
//!   Byte 0:   Channel ID (u8)
//!   Byte 1:   Flags (u8)
//!   Byte 2-3: Payload length (u16 big-endian)
//!   [Byte 4-7: Total message length (u32 BE) — only on first frame]
//!
//! Max frame payload: 16384 bytes (0x4000). Larger messages are fragmented.

use bytes::BufMut;

pub const MAX_FRAME_PAYLOAD: usize = 0x4000;

/// Upper bound on a reassembled multi-frame message. Frames carry at most
/// `MAX_FRAME_PAYLOAD` (16 KiB) each; a legitimate AA message (video setup,
/// service discovery, TLS records) is far below this. 1 MiB caps adversarial
/// or buggy head-unit input that declares a huge `total_length` or never sets
/// the LAST flag, which would otherwise grow the reassembly buffer unbounded.
pub const MAX_MESSAGE_SIZE: usize = 1 << 20; // 1 MiB

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FrameFlags: u8 {
        const FIRST     = 0x01;
        const LAST      = 0x02;
        const CONTROL   = 0x04;
        const ENCRYPTED = 0x08;
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub channel: u8,
    pub flags: FrameFlags,
    pub payload: Vec<u8>,
    /// Only set on first frame of a multi-frame message.
    pub total_length: Option<u32>,
}

impl Frame {
    pub fn is_first(&self) -> bool {
        self.flags.contains(FrameFlags::FIRST)
    }

    pub fn is_last(&self) -> bool {
        self.flags.contains(FrameFlags::LAST)
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags.contains(FrameFlags::ENCRYPTED)
    }

    pub fn is_single(&self) -> bool {
        self.is_first() && self.is_last()
    }
}

/// Encode a single AA frame with header.
pub fn encode_frame(channel: u8, payload: &[u8], flags: FrameFlags, total_length: Option<u32>) -> Vec<u8> {
    // The 4-byte total-message length is present only on a genuine first-of-many
    // frame (FIRST set, LAST clear). A single frame (BULK = FIRST|LAST) carries no
    // total length — matching real Android Auto framing.
    let has_total = flags.contains(FrameFlags::FIRST)
        && !flags.contains(FrameFlags::LAST)
        && total_length.is_some();
    let header_len = if has_total { 8 } else { 4 };
    let mut buf = Vec::with_capacity(header_len + payload.len());

    debug_assert!(
        payload.len() <= u16::MAX as usize,
        "frame payload {} exceeds u16 length field — would truncate",
        payload.len()
    );
    buf.put_u8(channel);
    buf.put_u8(flags.bits());
    buf.put_u16(payload.len() as u16);
    if let Some(total) = total_length.filter(|_| has_total) {
        buf.put_u32(total);
    }
    buf.extend_from_slice(payload);
    buf
}

/// Decode one frame from a byte buffer. Returns (Frame, bytes_consumed).
pub fn decode_frame(data: &[u8]) -> Result<(Frame, usize), DecodeError> {
    if data.len() < 4 {
        return Err(DecodeError::NeedMore(4 - data.len()));
    }

    let channel = data[0];
    let flags = FrameFlags::from_bits_truncate(data[1]);
    let payload_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let mut offset = 4;

    // Only a first-of-many frame (FIRST set, LAST clear) carries the 4-byte total
    // length. A single BULK frame (FIRST|LAST) does not — reading 4 bytes there
    // would consume payload as length and desync every single-frame message.
    let total_length = if flags.contains(FrameFlags::FIRST) && !flags.contains(FrameFlags::LAST) {
        if data.len() < 8 {
            return Err(DecodeError::NeedMore(8 - data.len()));
        }
        let total = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        // Reject absurd declared message sizes up front so the caller never
        // pre-allocates / accumulates an unbounded reassembly buffer.
        if total as usize > MAX_MESSAGE_SIZE {
            return Err(DecodeError::MessageTooLarge(total));
        }
        offset = 8;
        Some(total)
    } else {
        None
    };

    let end = offset + payload_len;
    if data.len() < end {
        return Err(DecodeError::NeedMore(end - data.len()));
    }

    let payload = data[offset..end].to_vec();
    let frame = Frame {
        channel,
        flags,
        payload,
        total_length,
    };
    Ok((frame, end))
}

/// Fragment a message into AA frames, respecting MAX_FRAME_PAYLOAD.
pub fn fragment_message(channel: u8, data: &[u8], encrypted: bool) -> Vec<Vec<u8>> {
    let total = data.len() as u32;
    let mut frames = Vec::new();
    let mut offset = 0usize;

    while offset < data.len() {
        let is_first = offset == 0;
        let max_chunk = if is_first && data.len() > MAX_FRAME_PAYLOAD {
            MAX_FRAME_PAYLOAD - 4 // First frame has 4 extra header bytes
        } else {
            MAX_FRAME_PAYLOAD
        };

        let chunk_end = (offset + max_chunk).min(data.len());
        let chunk = &data[offset..chunk_end];
        let is_last = chunk_end >= data.len();

        let mut flags = FrameFlags::empty();
        if is_first {
            flags |= FrameFlags::FIRST;
        }
        if is_last {
            flags |= FrameFlags::LAST;
        }
        if encrypted {
            flags |= FrameFlags::ENCRYPTED;
        }

        let total_len = if is_first && !is_last { Some(total) } else { None };
        frames.push(encode_frame(channel, chunk, flags, total_len));
        offset = chunk_end;
    }

    frames
}

#[derive(Debug)]
pub enum DecodeError {
    NeedMore(usize),
    /// First frame declared a `total_length` larger than `MAX_MESSAGE_SIZE`.
    MessageTooLarge(u32),
}
