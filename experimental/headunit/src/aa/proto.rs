//! Protobuf messages of the Android Auto protocol, head-unit subset.
//!
//! Hand-declared with `prost` derives (no protoc / build script needed) from
//! opencardev/aasdk `protobuf/aap_protobuf` (commit 9bf6adf, 2026-06-11).
//! Every field keeps the upstream TAG and proto2 presence: `required` fields
//! are always encoded (a proto2 peer rejects a message missing one), and
//! `optional` ones map to `Option` so an explicit zero is still sent.
//! Enums are carried as plain `i32` with the upstream values as constants.
//!
//! Path of each upstream file is given above its struct.

#![allow(clippy::derive_partial_eq_without_eq)]

// ── Shared enums ─────────────────────────────────────────────────────────

/// shared/MessageStatus.proto (subset).
pub mod status {
    pub const SUCCESS: i32 = 0;
    pub const INVALID_CHANNEL: i32 = -5;
    pub const INTERNAL_ERROR: i32 = -7;
    pub const INVALID_SENSOR: i32 = -9;
}

/// service/media/shared/message/MediaCodecType.proto
pub mod codec {
    pub const AUDIO_PCM: i32 = 1;
    pub const VIDEO_H264_BP: i32 = 3;
}

/// service/media/sink/message/AudioStreamType.proto
pub mod audio_stream {
    pub const GUIDANCE: i32 = 1;
    pub const SYSTEM_AUDIO: i32 = 2;
    pub const MEDIA: i32 = 3;
}

/// service/media/sink/message/VideoCodecResolutionType.proto
pub mod resolution {
    pub const R800X480: i32 = 1;
    pub const R1280X720: i32 = 2;
    pub const R1920X1080: i32 = 3;
}

/// service/media/sink/message/VideoFrameRateType.proto
pub mod fps {
    pub const FPS_60: i32 = 1;
    pub const FPS_30: i32 = 2;
}

/// service/media/video/message/VideoFocusMode.proto
pub mod video_focus {
    pub const PROJECTED: i32 = 1;
    pub const NATIVE: i32 = 2;
}

/// service/control/message/AudioFocusRequestType.proto
pub mod audio_focus_request {
    pub const GAIN: i32 = 1;
    pub const GAIN_TRANSIENT: i32 = 2;
    pub const GAIN_TRANSIENT_MAY_DUCK: i32 = 3;
    pub const RELEASE: i32 = 4;
}

/// service/control/message/AudioFocusStateType.proto
pub mod audio_focus_state {
    pub const GAIN: i32 = 1;
    pub const GAIN_TRANSIENT: i32 = 2;
    pub const LOSS: i32 = 3;
}

/// service/control/message/NavFocusType.proto
pub mod nav_focus {
    pub const PROJECTED: i32 = 2;
}

/// service/control/message/DriverPosition.proto
pub mod driver_position {
    pub const LEFT: i32 = 0;
}

/// service/inputsource/message/PointerAction.proto
pub mod pointer_action {
    pub const DOWN: i32 = 0;
    pub const UP: i32 = 1;
    pub const MOVED: i32 = 2;
    pub const POINTER_DOWN: i32 = 5;
    pub const POINTER_UP: i32 = 6;
}

/// service/inputsource/message/TouchScreenType.proto
pub mod touchscreen_type {
    pub const CAPACITIVE: i32 = 1;
}

/// service/sensorsource/message/SensorType.proto (subset)
pub mod sensor_type {
    pub const LOCATION: i32 = 1;
    pub const NIGHT_MODE: i32 = 10;
    pub const DRIVING_STATUS_DATA: i32 = 13;
}

/// service/sensorsource/message/DrivingStatus.proto
pub mod driving_status {
    pub const UNRESTRICTED: i32 = 0;
}

/// service/media/shared/message/Config.proto `Config.Status`
pub mod config_status {
    pub const WAIT: i32 = 1;
    pub const READY: i32 = 2;
}

// ── Control channel ──────────────────────────────────────────────────────

/// shared/PhoneInfo.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct PhoneInfo {
    #[prost(string, optional, tag = "1")]
    pub instance_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub connectivity_lifetime_id: Option<String>,
}

/// service/control/message/ServiceDiscoveryRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ServiceDiscoveryRequest {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub small_icon: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub medium_icon: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub large_icon: Option<Vec<u8>>,
    #[prost(string, optional, tag = "4")]
    pub label_text: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub device_name: Option<String>,
    #[prost(message, optional, tag = "6")]
    pub phone_info: Option<PhoneInfo>,
}

/// service/control/message/PingConfiguration.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingConfiguration {
    #[prost(uint32, optional, tag = "1")]
    pub timeout_ms: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub interval_ms: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub high_latency_threshold_ms: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub tracked_ping_count: Option<u32>,
}

/// service/control/message/ConnectionConfiguration.proto (no wireless part)
#[derive(Clone, PartialEq, prost::Message)]
pub struct ConnectionConfiguration {
    #[prost(message, optional, tag = "1")]
    pub ping_configuration: Option<PingConfiguration>,
}

/// service/control/message/HeadUnitInfo.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct HeadUnitInfo {
    #[prost(string, optional, tag = "1")]
    pub make: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub model: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub year: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub vehicle_id: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub head_unit_make: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub head_unit_model: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub head_unit_software_build: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub head_unit_software_version: Option<String>,
}

/// service/media/shared/message/AudioConfiguration.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct AudioConfiguration {
    #[prost(uint32, required, tag = "1")]
    pub sampling_rate: u32,
    #[prost(uint32, required, tag = "2")]
    pub number_of_bits: u32,
    #[prost(uint32, required, tag = "3")]
    pub number_of_channels: u32,
}

/// service/media/shared/message/Insets.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct Insets {
    #[prost(uint32, optional, tag = "1")]
    pub top: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub bottom: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub left: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub right: Option<u32>,
}

/// service/media/shared/message/UiConfig.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct UiConfig {
    #[prost(message, optional, tag = "1")]
    pub margins: Option<Insets>,
    #[prost(message, optional, tag = "2")]
    pub content_insets: Option<Insets>,
    #[prost(message, optional, tag = "3")]
    pub stable_content_insets: Option<Insets>,
    #[prost(int32, optional, tag = "4")]
    pub ui_theme: Option<i32>,
}

/// service/media/sink/message/VideoConfiguration.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoConfiguration {
    #[prost(int32, optional, tag = "1")]
    pub codec_resolution: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub frame_rate: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub width_margin: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub height_margin: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub density: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub decoder_additional_depth: Option<u32>,
    #[prost(uint32, optional, tag = "7")]
    pub viewing_distance: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub pixel_aspect_ratio_e4: Option<u32>,
    #[prost(uint32, optional, tag = "9")]
    pub real_density: Option<u32>,
    #[prost(int32, optional, tag = "10")]
    pub video_codec_type: Option<i32>,
    #[prost(message, optional, tag = "11")]
    pub ui_config: Option<UiConfig>,
}

/// service/media/sink/MediaSinkService.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct MediaSinkService {
    #[prost(int32, optional, tag = "1")]
    pub available_type: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub audio_type: Option<i32>,
    #[prost(message, repeated, tag = "3")]
    pub audio_configs: Vec<AudioConfiguration>,
    #[prost(message, repeated, tag = "4")]
    pub video_configs: Vec<VideoConfiguration>,
    #[prost(bool, optional, tag = "5")]
    pub available_while_in_call: Option<bool>,
    #[prost(uint32, optional, tag = "6")]
    pub display_id: Option<u32>,
    #[prost(int32, optional, tag = "7")]
    pub display_type: Option<i32>,
}

/// service/inputsource/InputSourceService.proto `TouchScreen`
#[derive(Clone, PartialEq, prost::Message)]
pub struct TouchScreen {
    #[prost(int32, required, tag = "1")]
    pub width: i32,
    #[prost(int32, required, tag = "2")]
    pub height: i32,
    #[prost(int32, optional, tag = "3")]
    pub r#type: Option<i32>,
    #[prost(bool, optional, tag = "4")]
    pub is_secondary: Option<bool>,
}

/// service/inputsource/InputSourceService.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct InputSourceService {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub keycodes_supported: Vec<i32>,
    #[prost(message, repeated, tag = "2")]
    pub touchscreen: Vec<TouchScreen>,
    #[prost(uint32, optional, tag = "5")]
    pub display_id: Option<u32>,
}

/// service/sensorsource/message/Sensor.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct Sensor {
    #[prost(int32, required, tag = "1")]
    pub sensor_type: i32,
}

/// service/sensorsource/SensorSourceService.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct SensorSourceService {
    #[prost(message, repeated, tag = "1")]
    pub sensors: Vec<Sensor>,
    #[prost(uint32, optional, tag = "2")]
    pub location_characterization: Option<u32>,
}

/// service/Service.proto (the services this head unit offers)
#[derive(Clone, PartialEq, prost::Message)]
pub struct Service {
    #[prost(int32, required, tag = "1")]
    pub id: i32,
    #[prost(message, optional, tag = "2")]
    pub sensor_source_service: Option<SensorSourceService>,
    #[prost(message, optional, tag = "3")]
    pub media_sink_service: Option<MediaSinkService>,
    #[prost(message, optional, tag = "4")]
    pub input_source_service: Option<InputSourceService>,
    #[prost(message, optional, tag = "5")]
    pub media_source_service: Option<MediaSourceService>,
}

/// service/media/source/MediaSourceService.proto (the head unit's microphone)
#[derive(Clone, PartialEq, prost::Message)]
pub struct MediaSourceService {
    #[prost(int32, optional, tag = "1")]
    pub available_type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub audio_config: Option<AudioConfiguration>,
    #[prost(bool, optional, tag = "3")]
    pub available_while_in_call: Option<bool>,
}

/// service/media/source/message/MicrophoneRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct MicrophoneRequest {
    #[prost(bool, required, tag = "1")]
    pub open: bool,
    #[prost(bool, optional, tag = "2")]
    pub anc_enabled: Option<bool>,
    #[prost(bool, optional, tag = "3")]
    pub ec_enabled: Option<bool>,
    #[prost(int32, optional, tag = "4")]
    pub max_unacked: Option<i32>,
}

/// service/media/source/message/MicrophoneResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct MicrophoneResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
    #[prost(int32, optional, tag = "2")]
    pub session_id: Option<i32>,
}

/// service/control/message/ServiceDiscoveryResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ServiceDiscoveryResponse {
    #[prost(message, repeated, tag = "1")]
    pub channels: Vec<Service>,
    #[prost(string, optional, tag = "2")]
    pub make: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub model: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub year: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub vehicle_id: Option<String>,
    #[prost(int32, optional, tag = "6")]
    pub driver_position: Option<i32>,
    #[prost(string, optional, tag = "7")]
    pub head_unit_make: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub head_unit_model: Option<String>,
    #[prost(string, optional, tag = "9")]
    pub head_unit_software_build: Option<String>,
    #[prost(string, optional, tag = "10")]
    pub head_unit_software_version: Option<String>,
    #[prost(bool, optional, tag = "11")]
    pub can_play_native_media_during_vr: Option<bool>,
    #[prost(int32, optional, tag = "13")]
    pub session_configuration: Option<i32>,
    #[prost(string, optional, tag = "14")]
    pub display_name: Option<String>,
    #[prost(bool, optional, tag = "15")]
    pub probe_for_support: Option<bool>,
    #[prost(message, optional, tag = "16")]
    pub connection_configuration: Option<ConnectionConfiguration>,
    #[prost(message, optional, tag = "17")]
    pub headunit_info: Option<HeadUnitInfo>,
}

/// service/control/message/AuthResponse.proto (payload of AUTH_COMPLETE)
#[derive(Clone, PartialEq, prost::Message)]
pub struct AuthResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

/// service/control/message/ChannelOpenRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChannelOpenRequest {
    #[prost(sint32, required, tag = "1")]
    pub priority: i32,
    #[prost(int32, required, tag = "2")]
    pub service_id: i32,
}

/// service/control/message/ChannelOpenResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChannelOpenResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

/// service/control/message/PingRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    #[prost(int64, required, tag = "1")]
    pub timestamp: i64,
    #[prost(bool, optional, tag = "2")]
    pub bug_report: Option<bool>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub data: Option<Vec<u8>>,
}

/// service/control/message/PingResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingResponse {
    #[prost(int64, required, tag = "1")]
    pub timestamp: i64,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub data: Option<Vec<u8>>,
}

/// service/control/message/AudioFocusRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct AudioFocusRequest {
    #[prost(int32, required, tag = "1")]
    pub audio_focus_type: i32,
}

/// service/control/message/AudioFocusNotification.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct AudioFocusNotification {
    #[prost(int32, required, tag = "1")]
    pub focus_state: i32,
    #[prost(bool, optional, tag = "2")]
    pub unsolicited: Option<bool>,
}

/// service/control/message/NavFocusRequestNotification.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct NavFocusRequestNotification {
    #[prost(int32, optional, tag = "1")]
    pub focus_type: Option<i32>,
}

/// service/control/message/NavFocusNotification.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct NavFocusNotification {
    #[prost(int32, required, tag = "1")]
    pub focus_type: i32,
}

/// service/control/message/ByeByeRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ByeByeRequest {
    #[prost(int32, required, tag = "1")]
    pub reason: i32,
}

/// service/control/message/ByeByeResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct ByeByeResponse {}

// ── Media sink channels ──────────────────────────────────────────────────

/// service/media/shared/message/Setup.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct Setup {
    #[prost(int32, required, tag = "1")]
    pub r#type: i32,
}

/// service/media/shared/message/Config.proto (answer to Setup)
#[derive(Clone, PartialEq, prost::Message)]
pub struct Config {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
    #[prost(uint32, optional, tag = "2")]
    pub max_unacked: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "3")]
    pub configuration_indices: Vec<u32>,
}

/// service/media/shared/message/Start.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct Start {
    #[prost(int32, required, tag = "1")]
    pub session_id: i32,
    #[prost(uint32, required, tag = "2")]
    pub configuration_index: u32,
}

/// service/media/source/message/Ack.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct Ack {
    #[prost(int32, required, tag = "1")]
    pub session_id: i32,
    #[prost(uint32, optional, tag = "2")]
    pub ack: Option<u32>,
}

/// service/media/video/message/VideoFocusRequestNotification.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoFocusRequestNotification {
    #[prost(int32, optional, tag = "1")]
    pub disp_channel_id: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub mode: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub reason: Option<i32>,
}

/// service/media/video/message/VideoFocusNotification.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct VideoFocusNotification {
    #[prost(int32, optional, tag = "1")]
    pub focus: Option<i32>,
    #[prost(bool, optional, tag = "2")]
    pub unsolicited: Option<bool>,
}

// ── Input source channel ─────────────────────────────────────────────────

/// service/inputsource/message/TouchEvent.proto `Pointer`
#[derive(Clone, PartialEq, prost::Message)]
pub struct Pointer {
    #[prost(uint32, required, tag = "1")]
    pub x: u32,
    #[prost(uint32, required, tag = "2")]
    pub y: u32,
    #[prost(uint32, required, tag = "3")]
    pub pointer_id: u32,
}

/// service/inputsource/message/TouchEvent.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    pub pointer_data: Vec<Pointer>,
    #[prost(uint32, optional, tag = "2")]
    pub action_index: Option<u32>,
    #[prost(int32, optional, tag = "3")]
    pub action: Option<i32>,
}

/// service/inputsource/message/InputReport.proto (touch only)
#[derive(Clone, PartialEq, prost::Message)]
pub struct InputReport {
    #[prost(uint64, required, tag = "1")]
    pub timestamp: u64,
    #[prost(int32, optional, tag = "2")]
    pub disp_channel_id: Option<i32>,
    #[prost(message, optional, tag = "3")]
    pub touch_event: Option<TouchEvent>,
}

/// service/media/sink/message/KeyBindingRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyBindingRequest {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub keycodes: Vec<i32>,
}

/// service/media/sink/message/KeyBindingResponse.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct KeyBindingResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

// ── Sensor source channel ────────────────────────────────────────────────

/// service/sensorsource/message/SensorRequest.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct SensorRequest {
    #[prost(int32, required, tag = "1")]
    pub r#type: i32,
    #[prost(int64, required, tag = "2")]
    pub min_update_period: i64,
}

/// service/sensorsource/message/SensorStartResponseMessage.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct SensorStartResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

/// service/sensorsource/message/DrivingStatusData.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct DrivingStatusData {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

/// service/sensorsource/message/NightModeData.proto
#[derive(Clone, PartialEq, prost::Message)]
pub struct NightModeData {
    #[prost(bool, optional, tag = "1")]
    pub night_mode: Option<bool>,
}

/// service/sensorsource/message/SensorBatch.proto (subset)
#[derive(Clone, PartialEq, prost::Message)]
pub struct SensorBatch {
    #[prost(message, repeated, tag = "10")]
    pub night_mode_data: Vec<NightModeData>,
    #[prost(message, repeated, tag = "13")]
    pub driving_status_data: Vec<DrivingStatusData>,
}

#[cfg(test)]
mod tests {
    //! Byte-level checks against hand-assembled proto2 encodings, so a wrong
    //! tag or wire type in a derive shows up here and not on the phone.
    use super::*;
    use prost::Message;

    #[test]
    fn required_zero_is_encoded() {
        // proto2 `required int32 status = 1` with value 0 must still be on the wire.
        assert_eq!(ChannelOpenResponse { status: 0 }.encode_to_vec(), vec![0x08, 0x00]);
        assert_eq!(AuthResponse { status: 0 }.encode_to_vec(), vec![0x08, 0x00]);
    }

    #[test]
    fn negative_status_is_ten_byte_varint() {
        // int32 -5 → sign-extended 64-bit varint (10 bytes), like protobuf-c++.
        let b = ChannelOpenResponse { status: status::INVALID_CHANNEL }.encode_to_vec();
        assert_eq!(b, vec![0x08, 0xFB, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    }

    #[test]
    fn channel_open_request_uses_zigzag_priority() {
        // priority is sint32 (zigzag): -1 → 1, service_id 3.
        let b = [0x08, 0x01, 0x10, 0x03];
        let r = ChannelOpenRequest::decode(&b[..]).unwrap();
        assert_eq!(r.priority, -1);
        assert_eq!(r.service_id, 3);
    }

    #[test]
    fn touch_event_layout() {
        let r = InputReport {
            timestamp: 1,
            disp_channel_id: None,
            touch_event: Some(TouchEvent {
                pointer_data: vec![Pointer { x: 300, y: 2, pointer_id: 0 }],
                action_index: Some(0),
                action: Some(pointer_action::DOWN),
            }),
        };
        // 08 01 | 1a len { 0a len { 08 ac02 10 02 18 00 } 10 00 18 00 }
        assert_eq!(
            r.encode_to_vec(),
            vec![0x08, 0x01, 0x1a, 0x0d, 0x0a, 0x07, 0x08, 0xac, 0x02, 0x10, 0x02, 0x18, 0x00, 0x10, 0x00, 0x18, 0x00]
        );
    }

    #[test]
    fn sensor_batch_driving_status_tag_13() {
        let b = SensorBatch {
            night_mode_data: vec![],
            driving_status_data: vec![DrivingStatusData { status: driving_status::UNRESTRICTED }],
        }
        .encode_to_vec();
        // field 13, wire type 2 → tag byte 0x6a
        assert_eq!(b, vec![0x6a, 0x02, 0x08, 0x00]);
    }

    #[test]
    fn config_indices_unpacked() {
        // proto2 repeated scalars are unpacked unless [packed=true].
        let b = Config { status: config_status::READY, max_unacked: Some(1), configuration_indices: vec![0] }
            .encode_to_vec();
        assert_eq!(b, vec![0x08, 0x02, 0x10, 0x01, 0x18, 0x00]);
    }

    #[test]
    fn service_discovery_request_parses_device_name() {
        // field 4 label "Pixel", field 5 device_name "Alex"
        let mut b = vec![0x22, 0x05];
        b.extend_from_slice(b"Pixel");
        b.extend_from_slice(&[0x2a, 0x04]);
        b.extend_from_slice(b"Alex");
        let r = ServiceDiscoveryRequest::decode(&b[..]).unwrap();
        assert_eq!(r.label_text.as_deref(), Some("Pixel"));
        assert_eq!(r.device_name.as_deref(), Some("Alex"));
    }
}
