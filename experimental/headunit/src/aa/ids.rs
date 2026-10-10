//! Android Auto channel and message identifiers (head-unit side).
//!
//! Message ids are copied from opencardev/aasdk `protobuf/aap_protobuf`
//! (commit 9bf6adf, 2026-06-11): `ControlMessageType.proto`,
//! `MediaMessageId.proto`, `InputMessageId.proto`, `SensorMessageId.proto`.
//!
//! Channel ids are NOT fixed by the protocol: the head unit assigns one per
//! service in its ServiceDiscoveryResponse and the phone opens those numbers.
//! We reuse aasdk's static numbering so captures read the same in both tools.

// ── Channels (our allocation, mirrors aasdk `ChannelId.hpp`) ─────────────
pub const CH_CONTROL: u8 = 0;
pub const CH_SENSOR: u8 = 1;
pub const CH_VIDEO: u8 = 3;
pub const CH_MEDIA_AUDIO: u8 = 4;
pub const CH_GUIDANCE_AUDIO: u8 = 5;
pub const CH_SYSTEM_AUDIO: u8 = 6;
/// Microphone (MediaSourceService, aasdk AV_INPUT).
pub const CH_MIC: u8 = 7;
pub const CH_INPUT: u8 = 8;

// ── Control channel (ControlMessageType) ─────────────────────────────────
pub const MSG_VERSION_REQUEST: u16 = 1;
pub const MSG_VERSION_RESPONSE: u16 = 2;
pub const MSG_ENCAPSULATED_SSL: u16 = 3;
pub const MSG_AUTH_COMPLETE: u16 = 4;
pub const MSG_SERVICE_DISCOVERY_REQUEST: u16 = 5;
pub const MSG_SERVICE_DISCOVERY_RESPONSE: u16 = 6;
/// Sent by the phone on the SERVICE channel it opens (frame CONTROL flag set).
pub const MSG_CHANNEL_OPEN_REQUEST: u16 = 7;
pub const MSG_CHANNEL_OPEN_RESPONSE: u16 = 8;
pub const MSG_CHANNEL_CLOSE_NOTIFICATION: u16 = 9;
pub const MSG_PING_REQUEST: u16 = 11;
pub const MSG_PING_RESPONSE: u16 = 12;
pub const MSG_NAV_FOCUS_REQUEST: u16 = 13;
pub const MSG_NAV_FOCUS_NOTIFICATION: u16 = 14;
pub const MSG_BYEBYE_REQUEST: u16 = 15;
pub const MSG_BYEBYE_RESPONSE: u16 = 16;
pub const MSG_VOICE_SESSION_NOTIFICATION: u16 = 17;
pub const MSG_AUDIO_FOCUS_REQUEST: u16 = 18;
pub const MSG_AUDIO_FOCUS_NOTIFICATION: u16 = 19;
pub const MSG_BATTERY_STATUS_NOTIFICATION: u16 = 23;

// ── Media sink channels (MediaMessageId) ─────────────────────────────────
/// `[id][timestamp u64 BE][payload]`
pub const MEDIA_DATA: u16 = 0x0000;
/// `[id][payload]` — H.264 SPS/PPS for video, no timestamp.
pub const MEDIA_CODEC_CONFIG: u16 = 0x0001;
pub const MEDIA_SETUP: u16 = 0x8000;
pub const MEDIA_START: u16 = 0x8001;
pub const MEDIA_STOP: u16 = 0x8002;
pub const MEDIA_CONFIG: u16 = 0x8003;
pub const MEDIA_ACK: u16 = 0x8004;
/// MediaMessageId MEDIA_MESSAGE_MICROPHONE_REQUEST / _RESPONSE (mic channel).
pub const MEDIA_MICROPHONE_REQUEST: u16 = 0x8005;
pub const MEDIA_MICROPHONE_RESPONSE: u16 = 0x8006;
pub const MEDIA_VIDEO_FOCUS_REQUEST: u16 = 0x8007;
pub const MEDIA_VIDEO_FOCUS_NOTIFICATION: u16 = 0x8008;

// ── Input source channel (InputMessageId) ────────────────────────────────
pub const INPUT_REPORT: u16 = 0x8001;
pub const INPUT_KEY_BINDING_REQUEST: u16 = 0x8002;
pub const INPUT_KEY_BINDING_RESPONSE: u16 = 0x8003;

// ── Sensor source channel (SensorMessageId) ──────────────────────────────
pub const SENSOR_REQUEST: u16 = 0x8001;
pub const SENSOR_RESPONSE: u16 = 0x8002;
pub const SENSOR_BATCH: u16 = 0x8003;

/// Protocol version the head unit announces. aasdk `Version.hpp` (1.6) — the
/// value every working open-source head unit sends today.
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 6;
