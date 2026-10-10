//! Phase 5 video-frame encryption — ported byte-for-byte from doubletake
//! (`internal/airplay/client.go` `mirrorCipher`, `mirror.go` `deriveVideoKeys` /
//! `deriveChaChaKey`).
//!
//! Two schemes, selected exactly as doubletake selects them:
//!   - **ChaCha20-Poly1305** when the HAP channel is encrypted (pair-verify
//!     produced a shared secret). Key = HKDF-SHA512(IKM=shared_secret,
//!     salt="DataStream-Salt<id>", info="DataStream-Output-Encryption-Key").
//!     The 128-byte packet header is the AEAD AAD; nonce = [0;4] ++ LE64(counter).
//!   - **AES-128-CTR** otherwise (UxPlay-style plaintext pair). Key/IV =
//!     SHA-512("AirPlayStreamKey<id>"||shk)[..16] / SHA-512("AirPlayStreamIV<id>"||shk)[..16].
//!     The CTR keystream must stay block-contiguous across frames — see
//!     [`MirrorCipher`].

use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use hkdf::Hkdf;
use sha2::{Digest, Sha512};
use tracing::info;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Video frame cipher. `header` (128 bytes) is only used by the ChaCha AEAD as
/// AAD; the AES-CTR path ignores it.
pub enum VideoCipher {
    /// No frame encryption (only reachable via `--no-encrypt`-style config, kept
    /// so the packetizer can run against permissive receivers).
    #[allow(dead_code)]
    None,
    Aes(MirrorCipher),
    ChaCha { aead: Box<ChaCha20Poly1305>, nonce: u64, last_header: [u8; 128] },
}

impl VideoCipher {
    /// Build the cipher from the negotiated session parameters, mirroring
    /// doubletake's branch: prefer ChaCha when the channel is encrypted and a
    /// shared secret exists; else AES-CTR from `shk`; else no encryption.
    pub fn from_session(
        shk: &[u8; 16],
        channel_encrypted: bool,
        shared_secret: &[u8],
        stream_connection_id: i64,
    ) -> Self {
        if channel_encrypted && !shared_secret.is_empty() {
            match derive_chacha_key(shared_secret, stream_connection_id) {
                Ok(key) => match ChaCha20Poly1305::new_from_slice(&key) {
                    Ok(aead) => {
                        info!("crypto: video cipher = ChaCha20-Poly1305 (HKDF-SHA512, sci={stream_connection_id})");
                        return VideoCipher::ChaCha { aead: Box::new(aead), nonce: 0, last_header: [0u8; 128] };
                    }
                    Err(e) => tracing::warn!("crypto: chacha init failed ({e}); falling back to AES-CTR"),
                },
                Err(e) => tracing::warn!("crypto: chacha HKDF failed ({e}); falling back to AES-CTR"),
            }
        }
        let (key, iv) = derive_video_keys(shk, stream_connection_id);
        info!("crypto: video cipher = AES-128-CTR (SHA-512 derived, sci={stream_connection_id})");
        VideoCipher::Aes(MirrorCipher::new(&key, &iv))
    }

    /// Ciphertext length for an `input_len`-byte payload (ChaCha adds the
    /// 16-byte Poly1305 tag). Used to fill the packet header's length field
    /// BEFORE encryption, since that header is the ChaCha AAD.
    pub fn output_len(&self, input_len: usize) -> usize {
        match self {
            VideoCipher::ChaCha { .. } => input_len + 16,
            _ => input_len,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            VideoCipher::None => "none",
            VideoCipher::Aes(_) => "aes-128-ctr",
            VideoCipher::ChaCha { .. } => "chacha20-poly1305",
        }
    }

    /// Set the header that will be used as ChaCha AAD for the NEXT
    /// `encrypt_frame` (no-op for other variants). Must be called with the
    /// packet header before encrypting each frame.
    pub fn set_aad_header(&mut self, header: &[u8; 128]) {
        if let VideoCipher::ChaCha { last_header, .. } = self {
            last_header.copy_from_slice(header);
        }
    }

    /// Encrypt one AVCC access-unit payload, returning the ciphertext (ChaCha
    /// appends its 16-byte Poly1305 tag).
    pub fn encrypt_frame(&mut self, payload: &[u8]) -> Vec<u8> {
        match self {
            VideoCipher::None => payload.to_vec(),
            VideoCipher::Aes(mc) => mc.encrypt_frame(payload),
            VideoCipher::ChaCha { aead, nonce, last_header } => {
                // IETF nonce: [0,0,0,0] ++ LE64(counter) to match the receiver's
                // 64x64 chacha state (state[13]=0, state[14..16]=LE64(n)).
                let mut n = [0u8; 12];
                n[4..].copy_from_slice(&nonce.to_le_bytes());
                let ct = aead
                    .encrypt(
                        (&n).into(),
                        Payload { msg: payload, aad: &last_header[..] },
                    )
                    .expect("chacha seal");
                *nonce += 1;
                ct
            }
        }
    }
}

/// AES-128-CTR with doubletake's cross-frame block-alignment. The CTR keystream
/// is contiguous across frames: the trailing partial block's leftover keystream
/// is cached (`og`) and consumed as the next frame's prefix, so no keystream
/// byte is reused or skipped.
pub struct MirrorCipher {
    stream: Aes128Ctr,
    og: [u8; 16],
    next_crypt_count: usize,
}

impl MirrorCipher {
    pub fn new(key: &[u8; 16], iv: &[u8; 16]) -> Self {
        Self {
            stream: Aes128Ctr::new(key.into(), iv.into()),
            og: [0u8; 16],
            next_crypt_count: 0,
        }
    }

    pub fn encrypt_frame(&mut self, payload: &[u8]) -> Vec<u8> {
        let input_len = payload.len();
        let mut out = vec![0u8; input_len];
        let mut pos = 0;

        // Step 1: XOR prefix with cached keystream from the previous frame's
        // trailing partial block. Only the bytes actually consumed are retired
        // from the cache: a frame shorter than the cache leaves the remainder
        // available for the next frame, otherwise the keystream would jump
        // forward here while the receiver's continuous CTR stream does not.
        if self.next_crypt_count > 0 {
            let n = self.next_crypt_count.min(input_len);
            let og_start = 16 - self.next_crypt_count;
            for i in 0..n {
                out[i] = payload[i] ^ self.og[og_start + i];
            }
            pos = n;
            self.next_crypt_count -= n;
            if pos == input_len {
                // Whole frame served from the cache; `og` / `next_crypt_count`
                // stay valid for the next frame.
                return out;
            }
        }

        let remaining = input_len - pos;

        // Step 3: full 16-byte blocks (block-aligned CTR).
        let full = (remaining / 16) * 16;
        if full > 0 {
            out[pos..pos + full].copy_from_slice(&payload[pos..pos + full]);
            self.stream.apply_keystream(&mut out[pos..pos + full]);
            pos += full;
        }

        // Step 4: trailing partial block — pad to 16, encrypt the full block,
        // use `rest` bytes, cache the rest of the keystream for next frame.
        let rest = remaining % 16;
        if rest > 0 {
            let mut padded = [0u8; 16];
            padded[..rest].copy_from_slice(&payload[pos..pos + rest]);
            self.stream.apply_keystream(&mut padded);
            out[pos..pos + rest].copy_from_slice(&padded[..rest]);
            // padded[rest..] were zero plaintext → pure keystream now.
            self.og = padded;
            self.next_crypt_count = 16 - rest;
        } else {
            // Block-aligned: nothing cached.
            self.next_crypt_count = 0;
        }

        out
    }
}

/// SHA-512 derived AES-CTR key + IV from `shk` and the streamConnectionID.
pub fn derive_video_keys(shk: &[u8; 16], stream_connection_id: i64) -> ([u8; 16], [u8; 16]) {
    let id = stream_connection_id as u64;

    let mut h = Sha512::new();
    h.update(format!("AirPlayStreamKey{id}").as_bytes());
    h.update(shk);
    let key_full = h.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&key_full[..16]);

    let mut h = Sha512::new();
    h.update(format!("AirPlayStreamIV{id}").as_bytes());
    h.update(shk);
    let iv_full = h.finalize();
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&iv_full[..16]);

    (key, iv)
}

/// HKDF-SHA512 32-byte ChaCha20 key (DataStream output-encryption direction).
pub fn derive_chacha_key(ikm: &[u8], stream_connection_id: i64) -> anyhow::Result<[u8; 32]> {
    derive_data_stream_key(ikm, stream_connection_id, b"DataStream-Output-Encryption-Key")
}

/// The key for what the CAR sends us on a data stream (its microphone on a
/// telephony audio stream): catplay `compute_keys` pairs "Input" with the
/// phone's read side and the receiver writes with it.
pub fn derive_chacha_input_key(ikm: &[u8], stream_connection_id: i64) -> anyhow::Result<[u8; 32]> {
    derive_data_stream_key(ikm, stream_connection_id, b"DataStream-Input-Encryption-Key")
}

fn derive_data_stream_key(ikm: &[u8], stream_connection_id: i64, info: &[u8]) -> anyhow::Result<[u8; 32]> {
    let id = stream_connection_id as u64;
    let salt = format!("DataStream-Salt{id}");
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut key = [0u8; 32];
    hk.expand(info, &mut key)
        .map_err(|e| anyhow::anyhow!("hkdf expand: {e}"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_keys_are_deterministic_and_distinct() {
        let shk = [7u8; 16];
        let (k1, iv1) = derive_video_keys(&shk, 12345);
        let (k2, iv2) = derive_video_keys(&shk, 12345);
        assert_eq!(k1, k2);
        assert_eq!(iv1, iv2);
        // key and iv use different label prefixes → must differ.
        assert_ne!(k1, iv1);
        // different stream id → different key.
        let (k3, _) = derive_video_keys(&shk, 999);
        assert_ne!(k1, k3);
    }

    #[test]
    fn chacha_key_is_32_bytes_and_deterministic() {
        let ikm = [3u8; 32];
        let a = derive_chacha_key(&ikm, 42).unwrap();
        let b = derive_chacha_key(&ikm, 42).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, derive_chacha_key(&ikm, 43).unwrap());
    }

    /// The mirror cipher must produce a contiguous CTR keystream across frame
    /// boundaries: encrypting [A][B] as two frames must equal encrypting A||B
    /// as one, because the receiver decrypts a single continuous stream.
    #[test]
    fn mirror_cipher_is_contiguous_across_frames() {
        let key = [1u8; 16];
        let iv = [2u8; 16];

        // Split point NOT on a 16-byte boundary to exercise the og cache.
        let part_a = vec![0xABu8; 20];
        let part_b = vec![0xCDu8; 33];
        let mut whole = part_a.clone();
        whole.extend_from_slice(&part_b);

        let mut split = MirrorCipher::new(&key, &iv);
        let mut ea = split.encrypt_frame(&part_a);
        let eb = split.encrypt_frame(&part_b);
        ea.extend_from_slice(&eb);

        let mut one = MirrorCipher::new(&key, &iv);
        let ew = one.encrypt_frame(&whole);

        assert_eq!(ea, ew, "framing must not disturb the CTR keystream");
    }

    /// Regression: a frame SHORTER than the cached partial-block keystream must
    /// only retire the bytes it consumes. Previously the leftover was dropped,
    /// so every subsequent frame decrypted to noise on the receiver.
    #[test]
    fn mirror_cipher_survives_a_tiny_middle_frame() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];

        // 20 leaves 12 cached keystream bytes; the 10-byte frame consumes only
        // 10 of them, so 2 must remain for the 40-byte frame.
        let parts: [&[u8]; 3] = [&[0xA1u8; 20], &[0xB2u8; 10], &[0xC3u8; 40]];

        let mut enc = MirrorCipher::new(&key, &iv);
        let mut ct = Vec::new();
        let mut pt = Vec::new();
        for p in parts {
            ct.extend_from_slice(&enc.encrypt_frame(p));
            pt.extend_from_slice(p);
        }

        let mut dec = Aes128Ctr::new((&key).into(), (&iv).into());
        let mut recovered = ct;
        dec.apply_keystream(&mut recovered);
        assert_eq!(recovered, pt, "a sub-block frame must not skip keystream");
    }

    /// A run of tiny frames that never fills a block, plus an empty frame,
    /// must still decrypt as one continuous CTR stream.
    #[test]
    fn mirror_cipher_handles_runs_of_tiny_frames() {
        let key = [0x33u8; 16];
        let iv = [0x44u8; 16];
        let sizes = [17usize, 1, 0, 2, 3, 1, 64, 5];

        let mut enc = MirrorCipher::new(&key, &iv);
        let mut ct = Vec::new();
        let mut pt = Vec::new();
        let mut byte = 0u8;
        for n in sizes {
            let frame: Vec<u8> = (0..n)
                .map(|_| {
                    byte = byte.wrapping_add(7);
                    byte
                })
                .collect();
            ct.extend_from_slice(&enc.encrypt_frame(&frame));
            pt.extend_from_slice(&frame);
        }

        let mut dec = Aes128Ctr::new((&key).into(), (&iv).into());
        let mut recovered = ct;
        dec.apply_keystream(&mut recovered);
        assert_eq!(recovered, pt);
    }

    #[test]
    fn mirror_cipher_roundtrips_via_plain_ctr() {
        // Decrypting the concatenated ciphertext with a fresh plain CTR stream
        // must recover the plaintext (CTR is symmetric).
        let key = [9u8; 16];
        let iv = [4u8; 16];
        let pt = vec![0x5Au8; 50];

        let mut enc = MirrorCipher::new(&key, &iv);
        let mut ct = enc.encrypt_frame(&pt[..17]);
        ct.extend_from_slice(&enc.encrypt_frame(&pt[17..]));

        let mut dec = Aes128Ctr::new((&key).into(), (&iv).into());
        let mut recovered = ct.clone();
        dec.apply_keystream(&mut recovered);
        assert_eq!(recovered, pt);
    }
}
