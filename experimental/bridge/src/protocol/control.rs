//! Android Auto control channel (channel 0).
//!
//! Handles the startup sequence:
//!   1. Version exchange (unencrypted)
//!   2. SSL handshake (via crypto module)
//!   3. Auth complete notification
//!   4. Service discovery (encrypted)
//!   5. Channel open requests (encrypted)
//!
//! After startup, handles ping/pong and shutdown.

/// Control message types.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    VersionRequest = 1,
    VersionResponse = 2,
    SslHandshake = 3,
    AuthComplete = 4,
    ServiceDiscoveryRequest = 5,
    ServiceDiscoveryResponse = 6,
    ChannelOpenRequest = 7,
    ChannelOpenResponse = 8,
    PingRequest = 11,
    PingResponse = 12,
    NavFocusRequest = 13,
    NavFocusNotify = 14,
    ByebyeRequest = 15,
    ByebyeResponse = 16,
    VoiceSessionNotify = 17,
    AudioFocusRequest = 18,
    AudioFocusNotify = 19,
}

impl MsgType {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(Self::VersionRequest),
            2 => Some(Self::VersionResponse),
            3 => Some(Self::SslHandshake),
            4 => Some(Self::AuthComplete),
            5 => Some(Self::ServiceDiscoveryRequest),
            6 => Some(Self::ServiceDiscoveryResponse),
            7 => Some(Self::ChannelOpenRequest),
            8 => Some(Self::ChannelOpenResponse),
            11 => Some(Self::PingRequest),
            12 => Some(Self::PingResponse),
            13 => Some(Self::NavFocusRequest),
            14 => Some(Self::NavFocusNotify),
            15 => Some(Self::ByebyeRequest),
            16 => Some(Self::ByebyeResponse),
            17 => Some(Self::VoiceSessionNotify),
            18 => Some(Self::AudioFocusRequest),
            19 => Some(Self::AudioFocusNotify),
            _ => None,
        }
    }
}

/// Well-known channel IDs.
pub mod channel {
    pub const CONTROL: u8 = 0;
    pub const INPUT: u8 = 1;
    pub const SENSOR: u8 = 2;
    pub const VIDEO: u8 = 3;
    pub const MEDIA_AUDIO: u8 = 4;
    pub const SYSTEM_AUDIO: u8 = 5;
    pub const SPEECH_AUDIO: u8 = 6;
    pub const MIC: u8 = 7;
    pub const BLUETOOTH: u8 = 8;
    pub const PHONE_STATUS: u8 = 9;
    pub const NOTIFICATION: u8 = 10;
    pub const NAVIGATION: u8 = 11;

    /// Human-readable name for a channel id, used when logging traffic on a
    /// channel the bridge does not handle. Keeps the full AA channel map
    /// meaningful instead of printing a bare number.
    pub fn name(id: u8) -> &'static str {
        match id {
            CONTROL => "control",
            INPUT => "input",
            SENSOR => "sensor",
            VIDEO => "video",
            MEDIA_AUDIO => "media_audio",
            SYSTEM_AUDIO => "system_audio",
            SPEECH_AUDIO => "speech_audio",
            MIC => "microphone",
            BLUETOOTH => "bluetooth",
            PHONE_STATUS => "phone_status",
            NOTIFICATION => "notification",
            NAVIGATION => "navigation",
            _ => "unknown",
        }
    }
}

/// AA protocol version we advertise.
const AA_VERSION_MAJOR: u16 = 1;
const AA_VERSION_MINOR: u16 = 7;

/// Encode a control message: 2-byte type + payload.
pub fn encode_message(msg_type: MsgType, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + payload.len());
    buf.extend_from_slice(&(msg_type as u16).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode a control message. Returns (type_u16, payload).
pub fn decode_message(data: &[u8]) -> Option<(u16, &[u8])> {
    if data.len() < 2 {
        return None;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    Some((msg_type, &data[2..]))
}

/// Build a Version Response message.
pub fn make_version_response() -> Vec<u8> {
    let mut payload = Vec::with_capacity(6);
    payload.extend_from_slice(&AA_VERSION_MAJOR.to_be_bytes());
    payload.extend_from_slice(&AA_VERSION_MINOR.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes()); // status OK
    encode_message(MsgType::VersionResponse, &payload)
}

/// Parse a Version Request. Returns (major, minor).
pub fn parse_version_request(data: &[u8]) -> Option<(u16, u16)> {
    let (_, payload) = decode_message(data)?;
    if payload.len() < 4 {
        return None;
    }
    let major = u16::from_be_bytes([payload[0], payload[1]]);
    let minor = u16::from_be_bytes([payload[2], payload[3]]);
    Some((major, minor))
}

/// Build a Service Discovery Request (sent by phone to HU).
///
/// Includes channel descriptors so the headunit knows which channels
/// this "phone" supports, including audio with specific configs.
pub fn make_service_discovery_request(device_name: &str, device_brand: &str) -> Vec<u8> {
    /// Encode a protobuf string field. The length MUST be a varint: writing it as
    /// a raw byte both truncates above 255 and emits an invalid varint above 127.
    fn encode_string(field_num: u8, value: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_len_field(&mut buf, field_num, value.as_bytes());
        buf
    }

    fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value > 0 { byte |= 0x80; }
            buf.push(byte);
            if value == 0 { break; }
        }
    }

    fn encode_varint_field(buf: &mut Vec<u8>, field_num: u8, value: u64) {
        buf.push((field_num << 3) | 0); // wire type 0 = varint
        encode_varint(buf, value);
    }

    fn encode_len_field(buf: &mut Vec<u8>, field_num: u8, inner: &[u8]) {
        buf.push((field_num << 3) | 2); // wire type 2 = length-delimited
        encode_varint(buf, inner.len() as u64);
        buf.extend_from_slice(inner);
    }

    /// Build an AudioConfig sub-message: sample_rate(1), bit_depth(2), channel_count(3)
    fn encode_audio_config(sample_rate: u32, bit_depth: u32, channels: u32) -> Vec<u8> {
        let mut cfg = Vec::new();
        encode_varint_field(&mut cfg, 1, sample_rate as u64);
        encode_varint_field(&mut cfg, 2, bit_depth as u64);
        encode_varint_field(&mut cfg, 3, channels as u64);
        cfg
    }

    /// Build a ChannelDescriptor: channel_id(1), content_type(2)
    /// For audio, also includes audio_configs at field 7.
    fn encode_channel_descriptor(channel_id: u8, content_type: u8, audio_configs: &[Vec<u8>]) -> Vec<u8> {
        let mut desc = Vec::new();
        encode_varint_field(&mut desc, 1, channel_id as u64);
        encode_varint_field(&mut desc, 2, content_type as u64);
        for cfg in audio_configs {
            encode_len_field(&mut desc, 7, cfg);
        }
        desc
    }

    // Content types (from AA protobuf):
    // 0 = INPUT, 1 = SENSOR, 2 = VIDEO, 3 = MEDIA_AUDIO, 4 = SYSTEM_AUDIO, 5 = SPEECH_AUDIO, 6 = MIC
    let channel_descriptors = [
        encode_channel_descriptor(channel::INPUT as u8, 0, &[]),
        encode_channel_descriptor(channel::SENSOR as u8, 1, &[]),
        encode_channel_descriptor(channel::VIDEO as u8, 2, &[]),
        encode_channel_descriptor(channel::MEDIA_AUDIO as u8, 3, &[
            encode_audio_config(48000, 16, 2), // 48kHz stereo
        ]),
        encode_channel_descriptor(channel::SYSTEM_AUDIO as u8, 4, &[
            encode_audio_config(16000, 16, 1), // 16kHz mono
        ]),
        encode_channel_descriptor(channel::MIC as u8, 6, &[
            encode_audio_config(16000, 16, 1), // 16kHz mono
        ]),
    ];

    let mut payload = Vec::new();
    // Field 1 (repeated, LEN) = channel descriptors
    for desc in &channel_descriptors {
        encode_len_field(&mut payload, 1, desc);
    }
    // Field 4 = device_name, Field 5 = device_brand
    payload.extend(encode_string(4, device_name));
    payload.extend(encode_string(5, device_brand));
    encode_message(MsgType::ServiceDiscoveryRequest, &payload)
}

/// Build a Channel Open Request.
pub fn make_channel_open_request(channel_id: u8) -> Vec<u8> {
    // protobuf: tag=0x08 (field 1, wire type 0), value=channel_id
    let payload = vec![0x08, channel_id];
    encode_message(MsgType::ChannelOpenRequest, &payload)
}

/// Echo back ping data as pong.
pub fn make_ping_response(ping_data: &[u8]) -> Vec<u8> {
    if let Some((_, payload)) = decode_message(ping_data) {
        encode_message(MsgType::PingResponse, payload)
    } else {
        encode_message(MsgType::PingResponse, &[])
    }
}

/// Acknowledge shutdown request.
pub fn make_byebye_response() -> Vec<u8> {
    encode_message(MsgType::ByebyeResponse, &[])
}
