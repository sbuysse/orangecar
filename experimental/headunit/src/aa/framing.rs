//! Android Auto framing as a real phone speaks it (sans-IO).
//!
//! Frame = `[channel u8][flags u8][size u16 BE]([total u32 BE])[payload]`
//!   flags bit0 FIRST, bit1 LAST (FIRST|LAST = BULK, neither = MIDDLE),
//!         bit2 CONTROL (control-type message on a service channel, e.g. the
//!               ChannelOpenRequest/Response), bit3 ENCRYPTED.
//!   The 4-byte `total` (plaintext size of the whole message) is present only
//!   on a FIRST frame that is not also LAST.
//!
//! Encryption is applied PER FRAME, not per message: the plaintext message is
//! cut into chunks of at most 0x4000 bytes, each chunk is encrypted on its own
//! and `size` is the size of that chunk's ciphertext. This is what aasdk does
//! (`MessageOutStream::compoundFrame`, `MessageInStream::receiveFramePayloadHandler`)
//! and it differs from `headunit/src/transport.rs`, which encrypts first and
//! fragments the ciphertext — fine against our own bridge, wrong for a phone
//! (the receiver could not decrypt a TLS record split across two frames, and
//! `total` would no longer match the reassembled size).

use anyhow::{bail, Result};
use std::collections::HashMap;

/// aasdk `MessageOutStream::cMaxFramePayloadSize`.
pub const MAX_FRAME_PLAINTEXT: usize = 0x4000;

/// Cap on one reassembled message. A 1280x720 IDR is typically well under
/// 1 MiB; this only stops a corrupt stream from growing memory forever.
pub const MAX_MESSAGE: usize = 8 << 20;

const FLAG_FIRST: u8 = 0x01;
const FLAG_LAST: u8 = 0x02;
const FLAG_CONTROL: u8 = 0x04;
const FLAG_ENCRYPTED: u8 = 0x08;

/// Per-frame record protection, implemented by the TLS session.
pub trait Cipher {
    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>>;
}

/// One complete (reassembled, decrypted) message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub channel: u8,
    /// Frame CONTROL flag (control-type message on a service channel).
    pub control: bool,
    pub encrypted: bool,
    /// `[message id u16 BE][body]`
    pub payload: Vec<u8>,
}

impl Message {
    pub fn id(&self) -> Option<u16> {
        (self.payload.len() >= 2).then(|| u16::from_be_bytes([self.payload[0], self.payload[1]]))
    }

    pub fn body(&self) -> &[u8] {
        self.payload.get(2..).unwrap_or(&[])
    }
}

/// Build the wire frames for one message. `cipher` must be `Some` when
/// `encrypted` is set.
pub fn encode(
    channel: u8,
    control: bool,
    encrypted: bool,
    payload: &[u8],
    mut cipher: Option<&mut dyn Cipher>,
) -> Result<Vec<u8>> {
    let total = payload.len();
    let mut out = Vec::with_capacity(total + 16);
    // aasdk (`MessageOutStream::stream`) takes the split path when the size is
    // >= 0x4000: frames are FIRST, MIDDLE..., LAST, and the LAST frame may be
    // empty when the size is an exact multiple of 0x4000. Mirror it exactly.
    let chunks: Vec<&[u8]> = if total >= MAX_FRAME_PLAINTEXT {
        let mut c: Vec<&[u8]> = payload.chunks(MAX_FRAME_PLAINTEXT).collect();
        if total % MAX_FRAME_PLAINTEXT == 0 {
            c.push(&[]);
        }
        c
    } else {
        vec![payload]
    };
    let n = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let kind = match (n, i) {
            (1, _) => FLAG_FIRST | FLAG_LAST,
            (_, 0) => FLAG_FIRST,
            (_, i) if i == n - 1 => FLAG_LAST,
            _ => 0,
        };
        let body = if encrypted {
            let c = cipher.as_deref_mut().ok_or_else(|| anyhow::anyhow!("encrypted frame without a cipher"))?;
            c.encrypt(chunk)?
        } else {
            chunk.to_vec()
        };
        if body.len() > u16::MAX as usize {
            bail!("frame body {} bytes does not fit the u16 size field", body.len());
        }
        let mut flags = kind;
        if control {
            flags |= FLAG_CONTROL;
        }
        if encrypted {
            flags |= FLAG_ENCRYPTED;
        }
        out.push(channel);
        out.push(flags);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        if kind == FLAG_FIRST {
            out.extend_from_slice(&(total as u32).to_be_bytes());
        }
        out.extend_from_slice(&body);
    }
    Ok(out)
}

struct Partial {
    control: bool,
    encrypted: bool,
    data: Vec<u8>,
    declared: Option<usize>,
}

/// Incremental frame parser + per-channel reassembler.
#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
    partial: HashMap<u8, Partial>,
}

impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received but not yet consumed as a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Next complete message, decrypting encrypted frames with `cipher`.
    /// Returns `Ok(None)` when more bytes are needed.
    pub fn next(&mut self, mut cipher: Option<&mut dyn Cipher>) -> Result<Option<Message>> {
        loop {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let channel = self.buf[0];
            let flags = self.buf[1];
            let size = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            let first = flags & FLAG_FIRST != 0;
            let last = flags & FLAG_LAST != 0;
            let header = if first && !last { 8 } else { 4 };
            if self.buf.len() < header + size {
                return Ok(None);
            }
            let declared = (header == 8)
                .then(|| u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize);
            let raw: Vec<u8> = self.buf[header..header + size].to_vec();
            self.buf.drain(..header + size);

            let encrypted = flags & FLAG_ENCRYPTED != 0;
            let control = flags & FLAG_CONTROL != 0;
            let plain = if encrypted {
                let c = cipher
                    .as_deref_mut()
                    .ok_or_else(|| anyhow::anyhow!("encrypted frame on channel {channel} before TLS is up"))?;
                c.decrypt(&raw)?
            } else {
                raw
            };

            if first {
                if let Some(d) = declared {
                    if d > MAX_MESSAGE {
                        bail!("channel {channel}: declared message of {d} bytes exceeds {MAX_MESSAGE}");
                    }
                }
                // A FIRST/BULK frame always starts a new message (aasdk drops
                // any half-received one on that channel the same way).
                self.partial.insert(channel, Partial { control, encrypted, data: plain, declared });
            } else {
                let Some(p) = self.partial.get_mut(&channel) else {
                    bail!("channel {channel}: continuation frame without a FIRST frame");
                };
                p.data.extend_from_slice(&plain);
                if p.data.len() > MAX_MESSAGE || p.declared.is_some_and(|d| p.data.len() > d) {
                    let got = p.data.len();
                    let d = p.declared;
                    self.partial.remove(&channel);
                    bail!("channel {channel}: reassembled {got} bytes, declared {d:?} (cap {MAX_MESSAGE})");
                }
            }

            if last {
                let p = self.partial.remove(&channel).expect("partial inserted above");
                return Ok(Some(Message { channel, control: p.control, encrypted: p.encrypted, payload: p.data }));
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Test cipher: XOR with 0x5A plus a 3-byte "MAC" prefix, so ciphertext
    /// is longer than plaintext like a real TLS record.
    pub(crate) struct XorCipher;
    impl Cipher for XorCipher {
        fn encrypt(&mut self, p: &[u8]) -> Result<Vec<u8>> {
            let mut v = vec![0xEE, 0xEE, 0xEE];
            v.extend(p.iter().map(|b| b ^ 0x5A));
            Ok(v)
        }
        fn decrypt(&mut self, c: &[u8]) -> Result<Vec<u8>> {
            if c.len() < 3 || c[..3] != [0xEE, 0xEE, 0xEE] {
                bail!("bad record");
            }
            Ok(c[3..].iter().map(|b| b ^ 0x5A).collect())
        }
    }

    #[test]
    fn small_plain_message_is_one_bulk_frame() {
        let f = encode(0, false, false, &[0x00, 0x01, 0x00, 0x01, 0x00, 0x06], None).unwrap();
        assert_eq!(f, vec![0x00, 0x03, 0x00, 0x06, 0x00, 0x01, 0x00, 0x01, 0x00, 0x06]);
    }

    #[test]
    fn control_and_encrypted_flags() {
        let f = encode(3, true, true, &[0x00, 0x08, 0x08, 0x00], Some(&mut XorCipher)).unwrap();
        assert_eq!(f[0], 3);
        assert_eq!(f[1], 0x0F); // BULK | CONTROL | ENCRYPTED
        assert_eq!(u16::from_be_bytes([f[2], f[3]]), 7); // 3 + 4 ciphertext
    }

    #[test]
    fn large_message_is_encrypted_per_frame_and_round_trips() {
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i * 7) as u8).collect();
        let wire = encode(3, false, true, &payload, Some(&mut XorCipher)).unwrap();
        // FIRST frame: 8-byte header, total = plaintext size, size = chunk ciphertext.
        assert_eq!(wire[1] & 0x03, FLAG_FIRST);
        assert_eq!(u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]) as usize, payload.len());
        assert_eq!(u16::from_be_bytes([wire[2], wire[3]]) as usize, MAX_FRAME_PLAINTEXT + 3);

        let mut d = Deframer::new();
        // Feed in awkward slices to exercise partial headers.
        for chunk in wire.chunks(777) {
            d.push(chunk);
        }
        let m = d.next(Some(&mut XorCipher)).unwrap().unwrap();
        assert_eq!(m.channel, 3);
        assert!(m.encrypted);
        assert_eq!(m.payload, payload);
        assert!(d.next(Some(&mut XorCipher)).unwrap().is_none());
    }

    #[test]
    fn exactly_max_plaintext_is_split_like_aasdk() {
        // aasdk sends FIRST(0x4000) + an empty LAST for a message of exactly 0x4000.
        let payload = vec![1u8; MAX_FRAME_PLAINTEXT];
        let wire = encode(4, false, false, &payload, None).unwrap();
        assert_eq!(wire[1] & 0x03, FLAG_FIRST);
        assert_eq!(wire.len(), 8 + MAX_FRAME_PLAINTEXT + 4);
        assert_eq!(&wire[8 + MAX_FRAME_PLAINTEXT..], &[4, FLAG_LAST, 0, 0]);
        let mut d = Deframer::new();
        d.push(&wire);
        assert_eq!(d.next(None).unwrap().unwrap().payload.len(), MAX_FRAME_PLAINTEXT);
    }

    #[test]
    fn interleaved_channels_reassemble_independently() {
        let a: Vec<u8> = vec![0xAA; 20_000];
        let b = vec![0x00, 0x0b, 0x08, 0x01];
        let wa = encode(3, false, false, &a, None).unwrap();
        let wb = encode(0, false, false, &b, None).unwrap();
        // Split wa after its first frame and slip the control message in between.
        let first_len = 8 + MAX_FRAME_PLAINTEXT;
        let mut wire = wa[..first_len].to_vec();
        wire.extend_from_slice(&wb);
        wire.extend_from_slice(&wa[first_len..]);
        let mut d = Deframer::new();
        d.push(&wire);
        let m1 = d.next(None).unwrap().unwrap();
        assert_eq!((m1.channel, m1.payload.clone()), (0, b));
        let m2 = d.next(None).unwrap().unwrap();
        assert_eq!((m2.channel, m2.payload.len()), (3, a.len()));
    }

    #[test]
    fn continuation_without_first_is_an_error() {
        let mut d = Deframer::new();
        d.push(&[0x03, FLAG_LAST, 0x00, 0x01, 0xAB]);
        assert!(d.next(None).is_err());
    }

    #[test]
    fn encrypted_frame_before_tls_is_an_error() {
        let mut d = Deframer::new();
        d.push(&[0x00, 0x0B, 0x00, 0x01, 0xAB]);
        assert!(d.next(None).is_err());
    }

    #[test]
    fn message_id_and_body() {
        let m = Message { channel: 0, control: false, encrypted: false, payload: vec![0x00, 0x05, 0x01] };
        assert_eq!(m.id(), Some(5));
        assert_eq!(m.body(), &[0x01]);
    }
}
