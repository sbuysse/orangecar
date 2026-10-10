//! Android Auto head unit handshake.
//!
//! Implements the HU (client) side of the AA protocol sequence:
//!   version exchange → TLS → auth complete → service discovery →
//!   channel opens → driving status → first video frame
//!
//! The Transport trait decouples this from the real USB layer,
//! enabling unit testing with a MockTransport.

use crate::checks::hex_dump;
use crate::transport::{decode_frame, fragment_message, DecodeError, MAX_FRAME_PAYLOAD, MAX_MESSAGE_SIZE};
use crate::tls::AaTlsSession;
use anyhow::{bail, Result};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::debug;

/// How many bytes of a frame are kept for diagnostics (the `--debug` hex dump
/// and the `last_sent` / `last_received` shown next to a failed check). A video
/// frame is up to 16 KiB; dumping all of it buries the header that matters.
const DIAG_BYTES: usize = 64;

fn diag_prefix(data: &[u8]) -> Vec<u8> {
    data[..data.len().min(DIAG_BYTES)].to_vec()
}

// Channel IDs
pub const CH_CONTROL: u8 = 0;
pub const CH_INPUT: u8 = 1;
pub const CH_SENSOR: u8 = 2;
pub const CH_VIDEO: u8 = 3;
pub const CH_MEDIA_AUDIO: u8 = 4;
pub const CH_SYSTEM_AUDIO: u8 = 5;
pub const CH_MIC: u8 = 7;

// Control message types
const MSG_VERSION_REQUEST: u16 = 1;
const MSG_VERSION_RESPONSE: u16 = 2;
const MSG_SSL_HANDSHAKE: u16 = 3;
const MSG_AUTH_COMPLETE: u16 = 4;
const MSG_SERVICE_DISCOVERY_REQUEST: u16 = 5;
const MSG_SERVICE_DISCOVERY_RESPONSE: u16 = 6;
const MSG_CHANNEL_OPEN_REQUEST: u16 = 7;
const MSG_CHANNEL_OPEN_RESPONSE: u16 = 8;
const MSG_PING_REQUEST: u16 = 11;
const MSG_PING_RESPONSE: u16 = 12;

// Non-control channel message types
const MSG_MEDIA_DATA: u16 = 0x0000;
const MSG_MEDIA_SETUP_REQUEST: u16 = 0x8000;
const MSG_START_REQUEST: u16 = 0x8001;
const MSG_MEDIA_SETUP_RESPONSE: u16 = 0x8003;
const MSG_MEDIA_DATA_ACK: u16 = 0x8004;

// Control channel audio focus messages
const MSG_AUDIO_FOCUS_REQUEST: u16 = 18;
const MSG_AUDIO_FOCUS_NOTIFY: u16 = 19;

/// Raw byte I/O — implemented by UsbTransport for real hardware,
/// MockTransport for tests.
pub trait Transport: Send {
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
}

pub struct AaHandshake<T: Transport> {
    pub transport: T,
    read_buf: Vec<u8>,
    tls: Option<AaTlsSession>,
    tls_active: bool,
    reassembly: HashMap<u8, Vec<u8>>,
    /// Per-channel declared total message length from a FIRST frame, used to
    /// bound reassembly so a lying/absent LAST flag can't grow memory forever.
    reassembly_total: HashMap<u8, usize>,
    /// First bytes of the most recent frame written / read, kept so a failed
    /// check can show what was actually on the wire.
    last_sent: Option<Vec<u8>>,
    last_received: Option<Vec<u8>>,
    /// Audio PCM chunks dropped because the playback channel was full.
    audio_dropped: u64,
}

impl<T: Transport> AaHandshake<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            read_buf: Vec::new(),
            tls: None,
            tls_active: false,
            reassembly: HashMap::new(),
            reassembly_total: HashMap::new(),
            last_sent: None,
            last_received: None,
            audio_dropped: 0,
        }
    }

    /// First bytes of the last frame written to the transport, for diagnostics.
    pub fn last_sent(&self) -> Option<Vec<u8>> {
        self.last_sent.clone()
    }

    /// First bytes of the last chunk read from the transport, for diagnostics.
    pub fn last_received(&self) -> Option<Vec<u8>> {
        self.last_received.clone()
    }

    // ── Low-level frame I/O ───────────────────────────────────────────────

    async fn send_raw(&mut self, channel: u8, data: &[u8]) -> Result<()> {
        for frame in fragment_message(channel, data, false) {
            debug!("→ ch{channel} cleartext {} bytes: {}", frame.len(), hex_dump(&diag_prefix(&frame)));
            self.last_sent = Some(diag_prefix(&frame));
            self.transport.write(&frame).await?;
        }
        Ok(())
    }

    async fn send_encrypted(&mut self, channel: u8, data: &[u8]) -> Result<()> {
        if self.tls_active {
            if let Some(ref mut tls) = self.tls {
                let encrypted = tls.encrypt(data)?;
                for frame in fragment_message(channel, &encrypted, true) {
                    debug!("→ ch{channel} encrypted {} bytes: {}", frame.len(), hex_dump(&diag_prefix(&frame)));
                    self.last_sent = Some(diag_prefix(&frame));
                    self.transport.write(&frame).await?;
                }
                return Ok(());
            }
        }
        // Every caller of send_encrypted runs after the TLS handshake, so a
        // missing/inactive session is a bug. Refuse rather than silently leak
        // the message as cleartext on the wire.
        bail!("send_encrypted on channel {channel}: TLS session not active — refusing to transmit cleartext")
    }

    /// Read one complete reassembled message. Returns (channel, payload).
    pub async fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        // Sized to hold the largest possible frame: MAX_FRAME_PAYLOAD plus the
        // 8-byte worst-case frame header. A smaller buffer would let UsbTransport
        // truncate an oversized transfer and corrupt frame alignment.
        let mut tmp = [0u8; MAX_FRAME_PAYLOAD + 8];
        loop {
            // Decode from bytes already buffered BEFORE issuing another USB read.
            // A bulk transfer can deliver several messages at once (e.g.
            // [ServiceDiscoveryRequest][PingRequest]); blocking on a fresh read
            // first would strand the second message until more traffic arrives.
            let mut offset = 0;
            while offset < self.read_buf.len() {
                match decode_frame(&self.read_buf[offset..]) {
                    Ok((frame, consumed)) => {
                        offset += consumed;
                        let ch = frame.channel;

                        // Reassemble multi-frame messages. A FIRST frame starts
                        // (or restarts) a buffer; a continuation/LAST frame must
                        // append to an existing buffer — receiving one with no
                        // recorded FIRST is a malformed stream, not silently OK.
                        if frame.is_first() {
                            match frame.total_length {
                                Some(total) => {
                                    self.reassembly_total.insert(ch, total as usize);
                                }
                                None => {
                                    self.reassembly_total.remove(&ch);
                                }
                            }
                            self.reassembly.insert(ch, frame.payload.clone());
                        } else if let Some(buf) = self.reassembly.get_mut(&ch) {
                            buf.extend_from_slice(&frame.payload);
                        } else {
                            bail!(
                                "continuation/LAST frame on channel {ch} with no preceding FIRST frame"
                            );
                        }

                        // Bound the reassembly buffer for this channel. A hostile
                        // or buggy bridge that never sets LAST (or lies about
                        // total_length) must not be able to grow memory without
                        // limit.
                        let accumulated = self.reassembly.get(&ch).map(Vec::len).unwrap_or(0);
                        let declared = self.reassembly_total.get(&ch).copied();
                        if accumulated > MAX_MESSAGE_SIZE
                            || declared.is_some_and(|total| accumulated > total)
                        {
                            self.reassembly.remove(&ch);
                            self.reassembly_total.remove(&ch);
                            bail!(
                                "reassembly buffer for channel {ch} exceeded bound \
                                 (accumulated {accumulated}, declared {declared:?}, cap {MAX_MESSAGE_SIZE})"
                            );
                        }

                        if !frame.is_last() {
                            continue;
                        }

                        let is_encrypted = frame.is_encrypted();
                        // Safe: the branch above guarantees a reassembly entry
                        // exists for `ch` by the time a LAST frame is processed
                        // (a single FIRST|LAST frame inserts then removes here).
                        let data = self
                            .reassembly
                            .remove(&ch)
                            .ok_or_else(|| anyhow::anyhow!("missing reassembly buffer for channel {ch}"))?;
                        self.reassembly_total.remove(&ch);

                        // Decrypt if needed
                        let data = if is_encrypted {
                            if let Some(ref mut tls) = self.tls {
                                tls.decrypt(&data)?
                            } else {
                                data
                            }
                        } else {
                            data
                        };

                        self.read_buf.drain(..offset);
                        return Ok((ch, data));
                    }
                    Err(DecodeError::NeedMore(_)) => break,
                    Err(DecodeError::MessageTooLarge(total)) => {
                        // The FIRST frame declared an impossible message size.
                        // Drop any partial state and abort — the stream is
                        // unrecoverable at this point.
                        self.reassembly.clear();
                        self.reassembly_total.clear();
                        bail!("bridge declared message of {total} bytes, exceeds {MAX_MESSAGE_SIZE}");
                    }
                }
            }
            if offset > 0 {
                self.read_buf.drain(..offset);
            }

            // No complete message buffered yet — pull more bytes from USB.
            let n = self.transport.read(&mut tmp).await?;
            if n > 0 {
                debug!("← {n} bytes: {}", hex_dump(&diag_prefix(&tmp[..n])));
                self.last_received = Some(diag_prefix(&tmp[..n]));
                self.read_buf.extend_from_slice(&tmp[..n]);
            }
        }
    }

    // ── Protocol helpers ──────────────────────────────────────────────────

    fn decode_control_type(data: &[u8]) -> Option<(u16, &[u8])> {
        if data.len() < 2 {
            return None;
        }
        let t = u16::from_be_bytes([data[0], data[1]]);
        Some((t, &data[2..]))
    }

    fn make_control(msg_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(2 + payload.len());
        buf.extend_from_slice(&msg_type.to_be_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    // ── Handshake steps ───────────────────────────────────────────────────

    /// Step 2: Send VersionRequest, receive VersionResponse, validate 1.7.
    pub async fn version_exchange(&mut self) -> Result<(u16, u16)> {
        // [type=1 u16 BE][major=1 u16 BE][minor=7 u16 BE]
        let req = Self::make_control(
            MSG_VERSION_REQUEST,
            &[0x00, 0x01, 0x00, 0x07],
        );
        self.send_raw(CH_CONTROL, &req).await?;

        loop {
            let (ch, data) = self.read_message().await?;
            if ch != CH_CONTROL {
                continue;
            }
            let Some((t, payload)) = Self::decode_control_type(&data) else {
                continue;
            };
            if t != MSG_VERSION_RESPONSE {
                continue;
            }
            if payload.len() < 6 {
                bail!("version response too short: {} bytes", payload.len());
            }
            let major = u16::from_be_bytes([payload[0], payload[1]]);
            let minor = u16::from_be_bytes([payload[2], payload[3]]);
            // Third u16 is the version-match status: 0 = MATCH, anything else
            // means the bridge rejected our version. Ignoring it would report
            // a passing version exchange for a handshake the peer refused.
            let status = u16::from_be_bytes([payload[4], payload[5]]);
            if status != 0 {
                bail!("bridge reported version status {status} (expected 0 = MATCH)");
            }
            if major != 1 || minor != 7 {
                bail!("unsupported AA version {major}.{minor}, expected 1.7");
            }
            return Ok((major, minor));
        }
    }

    /// Step 3: TLS handshake — we are client, bridge is server.
    pub async fn tls_handshake(&mut self) -> Result<()> {
        let session = AaTlsSession::new()?;
        self.tls = Some(session);

        // Kick off handshake by feeding empty bytes — rustls produces ClientHello
        let hello = self.tls.as_mut().unwrap().feed(&[])?;
        if !hello.is_empty() {
            let msg = Self::make_control(MSG_SSL_HANDSHAKE, &hello);
            self.send_raw(CH_CONTROL, &msg).await?;
        }

        loop {
            if self.tls.as_ref().unwrap().is_handshake_complete() {
                self.tls_active = true;
                return Ok(());
            }

            let (ch, data) = self.read_message().await?;
            if ch != CH_CONTROL {
                continue;
            }
            let Some((t, payload)) = Self::decode_control_type(&data) else {
                continue;
            };
            if t != MSG_SSL_HANDSHAKE {
                continue;
            }

            let response = self.tls.as_mut().unwrap().feed(payload)?;
            if !response.is_empty() {
                let msg = Self::make_control(MSG_SSL_HANDSHAKE, &response);
                self.send_raw(CH_CONTROL, &msg).await?;
            }
        }
    }

    /// Step 4: Send AuthComplete.
    pub async fn auth_complete(&mut self) -> Result<()> {
        let msg = Self::make_control(MSG_AUTH_COMPLETE, &[]);
        self.send_encrypted(CH_CONTROL, &msg).await
    }

    /// Steps 5+5b: Receive ServiceDiscoveryRequest, send ServiceDiscoveryResponse.
    /// Returns (device_name, brand) from the request.
    pub async fn service_discovery(&mut self) -> Result<(String, String)> {
        let (device_name, brand) = loop {
            let (ch, data) = self.read_message().await?;
            if ch != CH_CONTROL {
                continue;
            }
            let Some((t, payload)) = Self::decode_control_type(&data) else {
                continue;
            };

            // Handle pings while waiting
            if t == MSG_PING_REQUEST {
                let pong = Self::make_control(MSG_PING_RESPONSE, payload);
                self.send_encrypted(CH_CONTROL, &pong).await?;
                continue;
            }

            if t == MSG_SERVICE_DISCOVERY_REQUEST {
                // Parse device_name (field 4) and brand (field 5) from payload
                let (name, brand) = parse_service_discovery_request(payload);
                break (name, brand);
            }
        };

        // Send ServiceDiscoveryResponse listing supported channels with audio configs.
        let mut sdr_payload = Vec::new();

        // Helper: encode varint
        fn wr_varint(buf: &mut Vec<u8>, mut v: u64) {
            loop {
                if v < 0x80 { buf.push(v as u8); break; }
                buf.push((v as u8 & 0x7F) | 0x80);
                v >>= 7;
            }
        }
        fn wr_varint_field(buf: &mut Vec<u8>, field: u8, val: u64) {
            buf.push((field << 3) | 0);
            wr_varint(buf, val);
        }
        fn wr_len_field(buf: &mut Vec<u8>, field: u8, inner: &[u8]) {
            buf.push((field << 3) | 2);
            wr_varint(buf, inner.len() as u64);
            buf.extend_from_slice(inner);
        }

        // AudioConfig: sample_rate(1), bit_depth(2), channel_count(3)
        fn encode_audio_config(rate: u32, bits: u32, ch: u32) -> Vec<u8> {
            let mut c = Vec::new();
            wr_varint_field(&mut c, 1, rate as u64);
            wr_varint_field(&mut c, 2, bits as u64);
            wr_varint_field(&mut c, 3, ch as u64);
            c
        }

        // AvChannel: stream_type(1), audio_type(2), audio_configs(3)
        fn encode_av_channel(stream_type: u8, audio_type: u8, configs: &[Vec<u8>]) -> Vec<u8> {
            let mut av = Vec::new();
            wr_varint_field(&mut av, 1, stream_type as u64);
            wr_varint_field(&mut av, 2, audio_type as u64);
            for cfg in configs {
                wr_len_field(&mut av, 3, cfg);
            }
            av
        }

        // Channel descriptors
        let channels_desc: Vec<(u8, Option<Vec<u8>>)> = vec![
            (CH_INPUT, None),
            (CH_SENSOR, None),
            (CH_VIDEO, None),
            (CH_MEDIA_AUDIO, Some(encode_av_channel(0, 0, &[
                encode_audio_config(48000, 16, 2),
            ]))),
            (CH_SYSTEM_AUDIO, Some(encode_av_channel(0, 1, &[
                encode_audio_config(16000, 16, 1),
            ]))),
            (CH_MIC, None),
        ];

        for (ch_id, av_channel) in &channels_desc {
            let mut desc = Vec::new();
            wr_varint_field(&mut desc, 1, *ch_id as u64); // channel_id
            if let Some(av) = av_channel {
                wr_len_field(&mut desc, 5, av); // av_channel
            }
            wr_len_field(&mut sdr_payload, 1, &desc); // field 1 of ServiceDiscoveryResponse
        }

        let resp = Self::make_control(MSG_SERVICE_DISCOVERY_RESPONSE, &sdr_payload);
        self.send_encrypted(CH_CONTROL, &resp).await?;

        Ok((device_name, brand))
    }

    /// Steps 6–9: Receive ChannelOpenRequest for each channel, send ChannelOpenResponse.
    /// Returns the list of channel IDs opened.
    pub async fn channel_opens(&mut self) -> Result<Vec<u8>> {
        let expected = [CH_INPUT, CH_SENSOR, CH_VIDEO, CH_MEDIA_AUDIO, CH_SYSTEM_AUDIO, CH_MIC];
        let mut opened = Vec::new();

        while opened.len() < expected.len() {
            let (ch, data) = self.read_message().await?;
            if ch != CH_CONTROL {
                continue;
            }
            let Some((t, payload)) = Self::decode_control_type(&data) else {
                continue;
            };

            if t == MSG_PING_REQUEST {
                let pong = Self::make_control(MSG_PING_RESPONSE, payload);
                self.send_encrypted(CH_CONTROL, &pong).await?;
                continue;
            }

            if t == MSG_CHANNEL_OPEN_REQUEST {
                // ChannelOpenRequest protobuf: field 1 (varint) = channel_id.
                // Tag byte 0x08 = field 1, wire type 0 (varint); decode the
                // varint properly rather than assuming a single byte.
                let channel_id = if !payload.is_empty() && payload[0] == 0x08 {
                    match read_varint(payload, 1) {
                        Some((id, _)) if id <= u8::MAX as u64 => Some(id as u8),
                        _ => None, // malformed or out-of-range channel_id
                    }
                } else {
                    None // unrecognized field-1 encoding
                };
                let Some(channel_id) = channel_id else {
                    // Don't push a bogus 0 — that would inflate the opened count
                    // and let the loop terminate without all channels open.
                    eprintln!("[handshake] ignoring ChannelOpenRequest with unparseable channel_id");
                    continue;
                };
                // ChannelOpenResponse: field 1 varint = 0 (status OK)
                let resp = Self::make_control(MSG_CHANNEL_OPEN_RESPONSE, &[0x08, 0x00]);
                self.send_encrypted(CH_CONTROL, &resp).await?;
                // Only count each expected channel once. A retried
                // ChannelOpenRequest (duplicate id) or a request for an id
                // outside the expected set must not inflate `opened` and let
                // the loop exit before every expected channel has opened.
                if expected.contains(&channel_id) && !opened.contains(&channel_id) {
                    opened.push(channel_id);
                }
            }
        }

        Ok(opened)
    }

    /// Step 10: Send driving status UNRESTRICTED on SENSOR channel.
    pub async fn send_driving_status(&mut self) -> Result<()> {
        // Copied from bridge/src/protocol/channels.rs encode_sensor_driving_status(0)
        // MSG_MEDIA_SETUP_RESPONSE (0x8003) is dual-use here as sensor notification discriminator
        let inner = vec![0x08, 0x00]; // field 1 varint = 0 (UNRESTRICTED)
        let mut outer = vec![0x8a, 0x01, 0x02]; // field 17, LEN=2
        outer.extend_from_slice(&inner);

        let mut msg = Vec::with_capacity(2 + outer.len());
        msg.extend_from_slice(&MSG_MEDIA_SETUP_RESPONSE.to_be_bytes());
        msg.extend_from_slice(&outer);

        self.send_encrypted(CH_SENSOR, &msg).await
    }

    /// Step 11: Wait for first H.264 keyframe on VIDEO channel.
    /// Handles pings, audio focus requests, and audio setup concurrently.
    /// Returns the raw H.264 bytes (skipping the 2-byte type + 8-byte timestamp prefix).
    pub async fn wait_for_video_frame(&mut self) -> Result<Vec<u8>> {
        loop {
            let (ch, data) = self.read_message().await?;

            if ch == CH_CONTROL {
                if let Some((t, payload)) = Self::decode_control_type(&data) {
                    if t == MSG_PING_REQUEST {
                        let pong = Self::make_control(MSG_PING_RESPONSE, payload);
                        self.send_encrypted(CH_CONTROL, &pong).await?;
                    } else if t == MSG_AUDIO_FOCUS_REQUEST {
                        eprintln!("[audio] ← AudioFocusRequest on control channel (during init)");
                        let resp = Self::make_control(MSG_AUDIO_FOCUS_NOTIFY, &[0x08, 0x01]);
                        self.send_encrypted(CH_CONTROL, &resp).await?;
                        eprintln!("[audio] → AudioFocusNotify (GAIN)");
                    }
                }
                continue;
            }

            if (ch == CH_MEDIA_AUDIO || ch == CH_SYSTEM_AUDIO) && data.len() >= 2 {
                let msg_type = u16::from_be_bytes([data[0], data[1]]);
                if msg_type == MSG_MEDIA_SETUP_REQUEST {
                    eprintln!("[audio] ← AVChannelSetupRequest on ch{ch} (during init)");
                    let mut resp_payload = Vec::new();
                    resp_payload.push(0x08); resp_payload.push(0x00); // status OK
                    resp_payload.push(0x10); resp_payload.push(0x05); // max_unacked = 5
                    let mut resp_msg = Vec::with_capacity(2 + resp_payload.len());
                    resp_msg.extend_from_slice(&MSG_MEDIA_SETUP_RESPONSE.to_be_bytes());
                    resp_msg.extend(resp_payload);
                    self.send_encrypted(ch, &resp_msg).await?;
                    eprintln!("[audio] → AVChannelSetupResponse (OK) on ch{ch}");
                } else if msg_type == MSG_START_REQUEST {
                    eprintln!("[audio] ← AVChannelStartIndication on ch{ch} (during init)");
                }
                continue;
            }

            if ch == CH_VIDEO && data.len() > 10 {
                let msg_type = u16::from_be_bytes([data[0], data[1]]);
                if msg_type == MSG_MEDIA_DATA {
                    // data layout: [type(2)][timestamp(8)][h264...]
                    return Ok(data[10..].to_vec());
                }
            }
        }
    }

    /// Stream H.264 video continuously via a channel, forwarding touch events
    /// from `touch_rx` to the bridge on CH_INPUT.  H.264 NAL units are sent
    /// on `h264_tx` with backpressure (never drops — H.264 can't tolerate gaps).
    /// Audio PCM data is forwarded on `audio_tx`.
    ///
    /// Touch events are drained non-blockingly between reads rather than raced
    /// against the read in a `select!`: cancelling `read_message()` mid-flight
    /// drops the pending `bulk_in()` USB request, which can discard bytes that
    /// have already been received and corrupt frame-boundary alignment, causing
    /// TLS decryption failures.
    pub async fn stream_video_chan(
        &mut self,
        h264_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        audio_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        touch_rx: &mut mpsc::Receiver<(u32, u32, u8)>,
    ) -> Result<()> {
        let mut audio_ack_counter: u32 = 0;
        let mut video_ack_counter: u32 = 0;

        loop {
            while let Ok((x, y, action)) = touch_rx.try_recv() {
                eprintln!("[touch] → action={action} x={x} y={y}");
                let msg = encode_touch_event(x, y, 0, action);
                // Post-auth traffic is encrypted on every other channel; input
                // must be too. A real head unit (or a bridge that stops
                // decrypting conditionally) drops cleartext frames here.
                if let Err(e) = self.send_encrypted(CH_INPUT, &msg).await {
                    eprintln!("[touch] send error: {e}");
                }
            }

            let (ch, data) = self.read_message().await?;
            match ch {
                CH_CONTROL => {
                    if let Some((t, payload)) = Self::decode_control_type(&data) {
                        match t {
                            _ if t == MSG_PING_REQUEST => {
                                let pong = Self::make_control(MSG_PING_RESPONSE, payload);
                                self.send_encrypted(CH_CONTROL, &pong).await?;
                            }
                            _ if t == MSG_AUDIO_FOCUS_REQUEST => {
                                eprintln!("[audio] ← AudioFocusRequest on control channel");
                                // Respond with AudioFocusNotify: field 1 = GAIN (1)
                                let resp = Self::make_control(MSG_AUDIO_FOCUS_NOTIFY, &[0x08, 0x01]);
                                self.send_encrypted(CH_CONTROL, &resp).await?;
                                eprintln!("[audio] → AudioFocusNotify (GAIN)");
                            }
                            _ => {}
                        }
                    }
                }
                CH_VIDEO if data.len() > 10 => {
                    let msg_type = u16::from_be_bytes([data[0], data[1]]);
                    if msg_type == MSG_MEDIA_DATA {
                        // Backpressure: wait for display to consume.  Dropping
                        // H.264 frames corrupts every subsequent P-frame until
                        // the next IDR keyframe, so we must never drop.
                        // A send error means the display pipeline is gone
                        // (ffmpeg missing, window closed, no compositor).
                        // Swallowing it here is what let the prober run forever
                        // with no window and no non-zero exit — so it is fatal.
                        if h264_tx.send(data[10..].to_vec()).await.is_err() {
                            bail!("display pipeline gone — H.264 receiver dropped");
                        }

                        // Ack for flow control (same pattern as audio)
                        video_ack_counter += 1;
                        if video_ack_counter % 5 == 0 {
                            let mut ack = Vec::with_capacity(6);
                            ack.extend_from_slice(&MSG_MEDIA_DATA_ACK.to_be_bytes());
                            ack.push(0x08);
                            ack.push(0x01);
                            let _ = self.send_encrypted(CH_VIDEO, &ack).await;
                        }
                    }
                }
                CH_MEDIA_AUDIO | CH_SYSTEM_AUDIO => {
                    if data.len() >= 2 {
                        let msg_type = u16::from_be_bytes([data[0], data[1]]);
                        match msg_type {
                            MSG_MEDIA_SETUP_REQUEST => {
                                eprintln!("[audio] ← AVChannelSetupRequest on ch{ch}");
                                // Respond with AVChannelSetupResponse (0x8003)
                                // field 1 (varint) = status (0 = OK)
                                // field 2 (varint) = max_unacked (5)
                                // field 3 (LEN) = configs (repeat of AudioConfig)
                                let mut resp_payload = Vec::new();
                                resp_payload.push(0x08); resp_payload.push(0x00); // status OK
                                resp_payload.push(0x10); resp_payload.push(0x05); // max_unacked = 5
                                let mut resp_msg = Vec::with_capacity(2 + resp_payload.len());
                                resp_msg.extend_from_slice(&MSG_MEDIA_SETUP_RESPONSE.to_be_bytes());
                                resp_msg.extend(resp_payload);
                                self.send_encrypted(ch, &resp_msg).await?;
                                eprintln!("[audio] → AVChannelSetupResponse (OK) on ch{ch}");
                            }
                            MSG_START_REQUEST => {
                                eprintln!("[audio] ← AVChannelStartIndication on ch{ch}");
                                // No explicit response needed, just ready to receive data
                                eprintln!("[audio] audio streaming started on ch{ch}");
                            }
                            MSG_MEDIA_DATA => {
                                // data layout: [type(2)][timestamp(8)][pcm...]
                                if data.len() > 10 {
                                    let pcm = &data[10..];
                                    // Audio, unlike video, tolerates a gap — a
                                    // dropped chunk is a click, not permanent
                                    // corruption. But drop it *loudly*: a
                                    // silent drop looks identical to a bridge
                                    // that stopped sending audio.
                                    if audio_tx.try_send(pcm.to_vec()).is_err() {
                                        self.audio_dropped += 1;
                                        if self.audio_dropped == 1 || self.audio_dropped % 50 == 0 {
                                            eprintln!(
                                                "[audio] dropped {} PCM chunk(s) — playback not keeping up",
                                                self.audio_dropped
                                            );
                                        }
                                    }

                                    // Send MediaDataAck periodically for flow control
                                    audio_ack_counter += 1;
                                    if audio_ack_counter % 5 == 0 {
                                        let mut ack = Vec::with_capacity(6);
                                        ack.extend_from_slice(&MSG_MEDIA_DATA_ACK.to_be_bytes());
                                        ack.push(0x08); // field 1 varint
                                        ack.push(0x01); // session_id = 1
                                        let _ = self.send_encrypted(ch, &ack).await;
                                    }
                                }
                            }
                            _ => {
                                eprintln!("[audio] ← unknown msg 0x{msg_type:04x} on ch{ch} ({} bytes)", data.len());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

}

/// Encode an AA touch event: 2-byte msg type + minimal protobuf.
///
/// Bridge's parse_touch_event expects:
///   field 1 (LEN): TouchLocation { x, y, pointer_id }
///   field 3 (varint): action (0=Press, 1=Release, 2=Drag)
fn encode_touch_event(x: u32, y: u32, pointer_id: u32, action: u8) -> Vec<u8> {
    let loc = encode_touch_location(x, y, pointer_id);

    let mut payload = Vec::new();
    // field 1, wire type 2 (LEN)
    payload.push((1 << 3) | 2);
    write_varint(&mut payload, loc.len() as u64);
    payload.extend_from_slice(&loc);
    // field 3, wire type 0 (varint)
    payload.push((3 << 3) | 0);
    write_varint(&mut payload, action as u64);

    let mut msg = Vec::with_capacity(2 + payload.len());
    msg.extend_from_slice(&0x8001u16.to_be_bytes()); // InputEventIndication
    msg.extend_from_slice(&payload);
    msg
}

fn encode_touch_location(x: u32, y: u32, pointer_id: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push((1 << 3) | 0); write_varint(&mut buf, x as u64);
    buf.push((2 << 3) | 0); write_varint(&mut buf, y as u64);
    buf.push((3 << 3) | 0); write_varint(&mut buf, pointer_id as u64);
    buf
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        if v < 0x80 { buf.push(v as u8); break; }
        buf.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

/// Decode a protobuf base-128 varint from `data` starting at `start`.
/// Returns `(value, next_index)`, or `None` if the buffer ends mid-varint or
/// the varint exceeds 10 bytes (the maximum for a u64). Never panics.
fn read_varint(data: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    let mut i = start;
    while i < data.len() {
        let byte = data[i];
        i += 1;
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i));
        }
        shift += 7;
        if shift >= 64 {
            return None; // varint too long for u64 — malformed
        }
    }
    None // ran off the end before the terminating byte
}

/// Parse device_name and brand from a ServiceDiscoveryRequest payload.
/// Payload uses the same minimal protobuf as bridge/src/protocol/control.rs:
/// field 4 (string) = device_name, field 5 (string) = brand.
fn parse_service_discovery_request(payload: &[u8]) -> (String, String) {
    let mut name = String::from("unknown");
    let mut brand = String::from("unknown");
    let mut i = 0;
    while i < payload.len() {
        let tag = payload[i];
        i += 1;
        let field = tag >> 3;
        let wire = tag & 0x07;
        if wire == 2 {
            // LEN field: length is a protobuf varint, not a single byte.
            let Some((len, next)) = read_varint(payload, i) else {
                break; // truncated/over-long varint — stop rather than panic
            };
            i = next;
            let len = len as usize;
            // Bounds check guards against a malformed length running past the buffer.
            if i + len <= payload.len() {
                let s = String::from_utf8_lossy(&payload[i..i + len]).into_owned();
                match field {
                    4 => name = s,
                    5 => brand = s,
                    _ => {}
                }
                i += len;
            } else {
                break; // declared length exceeds remaining payload — malformed
            }
        } else if wire == 0 {
            // varint: skip past it (bytes with MSB set are continuation bytes)
            match read_varint(payload, i) {
                Some((_, next)) => i = next,
                None => break,
            }
        } else if wire == 1 {
            i += 8; // fixed64
        } else if wire == 5 {
            i += 4; // fixed32
        } else {
            break; // unknown wire type — stop parsing rather than corrupt state
        }
    }
    (name, brand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockTransport {
        incoming: VecDeque<u8>,
        pub sent: Vec<u8>,
    }

    impl MockTransport {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            let mut incoming = VecDeque::new();
            for r in responses { incoming.extend(r); }
            Self { incoming, sent: Vec::new() }
        }
    }

    impl Transport for MockTransport {
        async fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
            self.sent.extend_from_slice(data);
            Ok(())
        }
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            if self.incoming.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                return Ok(0);
            }
            let n = buf.len().min(self.incoming.len());
            for b in buf.iter_mut().take(n) { *b = self.incoming.pop_front().unwrap(); }
            Ok(n)
        }
    }

    /// Put the handshake into an "encryption active" state so `send_encrypted`
    /// succeeds. A fresh client session is enough: `encrypt()` buffers the
    /// plaintext and emits TLS traffic without erroring, which is all these
    /// protocol-flow tests need (they assert on parsing/flow, not ciphertext).
    fn activate_tls<T: Transport>(hs: &mut AaHandshake<T>) {
        hs.tls = Some(AaTlsSession::new().expect("tls session"));
        hs.tls_active = true;
    }

    fn make_version_response_bytes() -> Vec<u8> {
        let payload: Vec<u8> = vec![0x00, 0x02, 0x00, 0x01, 0x00, 0x07, 0x00, 0x00];
        crate::transport::encode_frame(
            0, &payload,
            crate::transport::FrameFlags::FIRST | crate::transport::FrameFlags::LAST,
            Some(payload.len() as u32),
        )
    }

    fn make_control_frame(ch: u8, msg_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&msg_type.to_be_bytes());
        data.extend_from_slice(payload);
        crate::transport::encode_frame(
            ch, &data,
            crate::transport::FrameFlags::FIRST | crate::transport::FrameFlags::LAST,
            Some(data.len() as u32),
        )
    }

    #[tokio::test]
    async fn version_exchange_sends_request_and_validates_response() {
        let mock = MockTransport::new(vec![make_version_response_bytes()]);
        let mut hs = AaHandshake::new(mock);
        let (major, minor) = hs.version_exchange().await.unwrap();
        assert_eq!(major, 1);
        assert_eq!(minor, 7);
        assert!(hs.transport.sent.len() > 6);
    }

    #[tokio::test]
    async fn version_exchange_rejects_wrong_version() {
        let payload: Vec<u8> = vec![0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
        let frame = crate::transport::encode_frame(
            0, &payload,
            crate::transport::FrameFlags::FIRST | crate::transport::FrameFlags::LAST,
            Some(payload.len() as u32),
        );
        let mock = MockTransport::new(vec![frame]);
        let mut hs = AaHandshake::new(mock);
        assert!(hs.version_exchange().await.is_err());
    }

    #[tokio::test]
    async fn version_exchange_rejects_non_zero_status() {
        // major/minor match 1.7 but the bridge reports a version MISMATCH in
        // the third u16 — the exchange must fail, not report [PASS].
        let payload: Vec<u8> = vec![0x00, 0x02, 0x00, 0x01, 0x00, 0x07, 0x00, 0x01];
        let frame = crate::transport::encode_frame(
            0, &payload,
            crate::transport::FrameFlags::FIRST | crate::transport::FrameFlags::LAST,
            Some(payload.len() as u32),
        );
        let mock = MockTransport::new(vec![frame]);
        let mut hs = AaHandshake::new(mock);
        let err = hs.version_exchange().await.unwrap_err();
        assert!(
            err.to_string().contains("status"),
            "expected a version-status error, got: {err}"
        );
    }

    #[tokio::test]
    async fn frame_io_records_last_sent_and_received() {
        let mock = MockTransport::new(vec![make_version_response_bytes()]);
        let mut hs = AaHandshake::new(mock);
        assert!(hs.last_sent().is_none());
        assert!(hs.last_received().is_none());
        hs.version_exchange().await.unwrap();
        // Both directions must be populated, or the hex dumps printed next to a
        // failed check are dead code again.
        let sent = hs.last_sent().expect("last_sent recorded");
        let received = hs.last_received().expect("last_received recorded");
        assert_eq!(sent[0], CH_CONTROL, "last sent frame was not the control frame");
        assert_eq!(received[0], CH_CONTROL);
        assert!(sent.len() <= DIAG_BYTES && received.len() <= DIAG_BYTES);
    }

    #[tokio::test]
    async fn service_discovery_parses_device_name() {
        let name_field = b"\x22\x06RasCar";
        let brand_field = b"\x2a\x06RasCar";
        let mut payload = Vec::new();
        payload.extend_from_slice(name_field);
        payload.extend_from_slice(brand_field);
        let frame = make_control_frame(0, MSG_SERVICE_DISCOVERY_REQUEST, &payload);
        let mock = MockTransport::new(vec![frame]);
        let mut hs = AaHandshake::new(mock);
        activate_tls(&mut hs); // service_discovery sends its response encrypted
        let (name, brand) = hs.service_discovery().await.unwrap();
        assert_eq!(name, "RasCar");
        assert_eq!(brand, "RasCar");
    }

    #[tokio::test]
    async fn ping_is_handled_before_service_discovery() {
        let ping_frame = make_control_frame(0, MSG_PING_REQUEST, &[0xAA, 0xBB]);
        let sdr_frame = make_control_frame(0, MSG_SERVICE_DISCOVERY_REQUEST, &[]);
        let mock = MockTransport::new(vec![ping_frame, sdr_frame]);
        let mut hs = AaHandshake::new(mock);
        activate_tls(&mut hs);
        // A ping arriving before the ServiceDiscoveryRequest must be answered
        // (its pong sent via send_encrypted, whose `?` would surface an error)
        // and then discovery must still complete — i.e. the leading ping neither
        // stalls the loop nor aborts it. The pong is now sent encrypted, so its
        // plaintext bytes are no longer observable on the wire; we assert the
        // functional outcome (discovery completes, encrypted send path ran).
        hs.service_discovery().await.unwrap();
        let sent_any_encrypted = {
            let mut off = 0;
            let mut found = false;
            while off < hs.transport.sent.len() {
                match crate::transport::decode_frame(&hs.transport.sent[off..]) {
                    Ok((f, consumed)) => {
                        off += consumed;
                        if f.channel == CH_CONTROL && f.is_encrypted() {
                            found = true;
                        }
                    }
                    Err(_) => break,
                }
            }
            found
        };
        assert!(sent_any_encrypted, "expected an encrypted control response to be sent");
        assert!(hs.read_buf.is_empty(), "both ping and SDR frames should be consumed");
    }

    #[tokio::test]
    async fn channel_opens_acknowledges_all_channels() {
        let channels = [1u8, 2, 3, 4, 5, 7];
        let frames: Vec<Vec<u8>> = channels.iter()
            .map(|&id| make_control_frame(0, MSG_CHANNEL_OPEN_REQUEST, &[0x08, id]))
            .collect();
        let mock = MockTransport::new(frames);
        let mut hs = AaHandshake::new(mock);
        activate_tls(&mut hs); // channel_opens sends each response encrypted
        let opened = hs.channel_opens().await.unwrap();
        assert_eq!(opened.len(), 6);
        for &id in &channels { assert!(opened.contains(&id)); }
    }

    #[tokio::test]
    async fn wait_for_video_frame_returns_on_media_data() {
        let mut video_msg = Vec::new();
        video_msg.extend_from_slice(&0x0000u16.to_be_bytes()); // type
        video_msg.extend_from_slice(&[0u8; 8]); // timestamp
        video_msg.extend_from_slice(&[0xABu8; 100]); // H.264 payload
        let frame = crate::transport::encode_frame(
            CH_VIDEO, &video_msg,
            crate::transport::FrameFlags::FIRST | crate::transport::FrameFlags::LAST,
            Some(video_msg.len() as u32),
        );
        let mock = MockTransport::new(vec![frame]);
        let mut hs = AaHandshake::new(mock);
        let bytes = hs.wait_for_video_frame().await.unwrap();
        assert_eq!(bytes.len(), 100);
        assert!(bytes.iter().all(|&b| b == 0xAB));
    }
}
