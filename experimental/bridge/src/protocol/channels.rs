//! Android Auto channel manager.
//!
//! Manages the lifecycle of AA channels after service discovery.
//! Each channel carries a specific type of data (video, audio, touch, sensors).
//!
//! Message types on non-control channels (base 0x8000):
//!   0x8000 = Media Setup Request
//!   0x8001 = Start Request
//!   0x8002 = Stop Request / Config
//!   0x8003 = Media Setup Response / Sensor Notification
//!   0x8004 = Media Data Ack
//!   0x8005 = Mic Start/Stop
//!   0x8008 = Video Focus Notification
//!   0x0000 = Media Data (raw video/audio)
//!   0x0001 = Codec Config

use tracing::warn;

pub const MSG_MEDIA_SETUP_REQUEST: u16 = 0x8000;
pub const MSG_START_REQUEST: u16 = 0x8001;
pub const MSG_STOP_REQUEST: u16 = 0x8002;
pub const MSG_MEDIA_SETUP_RESPONSE: u16 = 0x8003;
pub const MSG_MEDIA_DATA_ACK: u16 = 0x8004;
pub const MSG_MIC_REQUEST: u16 = 0x8005;
pub const MSG_VIDEO_FOCUS: u16 = 0x8008;
pub const MSG_MEDIA_DATA: u16 = 0x0000;
/// Codec configuration (SPS/PPS out of band). We embed headers in the elementary
/// stream instead (`repeat-headers=1`), so this id is only used to recognise the
/// message if a head unit sends one.
#[allow(dead_code)]
pub const MSG_CODEC_CONFIG: u16 = 0x0001;

// Protocol-level messages (< 0x8000) on audio channels
pub const MSG_AUDIO_FOCUS_REQUEST: u16 = 0x0012;
pub const MSG_AUDIO_FOCUS_RESPONSE: u16 = 0x0013;

// ── Input channel (channel 1) message ids ────────────────────────────────────
// The input channel reuses the 0x8000 numbering space with its own meanings, so
// these must never be compared against the AV channel ids above.
pub const MSG_INPUT_EVENT_INDICATION: u16 = 0x8001;
pub const MSG_INPUT_BINDING_REQUEST: u16 = 0x8002;
pub const MSG_INPUT_BINDING_RESPONSE: u16 = 0x8003;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoResolution {
    Res480p = 1,  // 800x480
    Res720p = 2,  // 1280x720
    Res1080p = 3, // 1920x1080
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFps {
    Fps30 = 1,
    Fps60 = 2,
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub resolution: VideoResolution,
    pub fps: VideoFps,
    pub dpi: u32,
    pub margin_width: u32,
    pub margin_height: u32,
}

impl VideoConfig {
    pub fn width(&self) -> u32 {
        match self.resolution {
            VideoResolution::Res480p => 800,
            VideoResolution::Res720p => 1280,
            VideoResolution::Res1080p => 1920,
        }
    }

    pub fn height(&self) -> u32 {
        match self.resolution {
            VideoResolution::Res480p => 480,
            VideoResolution::Res720p => 720,
            VideoResolution::Res1080p => 1080,
        }
    }

    /// Map encoder dimensions (the `--width/--height/--fps` CLI flags) onto the
    /// resolution/fps enums advertised to the head unit.
    ///
    /// Android Auto only defines 800x480, 1280x720 and 1920x1080 at 30 or 60 fps.
    /// Advertising a value that does not match what ffmpeg actually produces makes
    /// the head unit allocate a decoder surface of the wrong size, so an
    /// unsupported request is snapped to the nearest defined entry and warned
    /// about rather than silently ignored.
    pub fn from_dimensions(width: u32, height: u32, fps: u32) -> Self {
        let resolution = match (width, height) {
            (800, 480) => VideoResolution::Res480p,
            (1280, 720) => VideoResolution::Res720p,
            (1920, 1080) => VideoResolution::Res1080p,
            _ => {
                let nearest = if height >= 1080 {
                    VideoResolution::Res1080p
                } else if height >= 720 {
                    VideoResolution::Res720p
                } else {
                    VideoResolution::Res480p
                };
                let probe = Self {
                    resolution: nearest,
                    ..Self::default()
                };
                warn!(
                    "video size {width}x{height} is not an Android Auto resolution — \
                     advertising {}x{}; the encoder output will not match the head unit's \
                     decoder surface (use 800x480, 1280x720 or 1920x1080)",
                    probe.width(),
                    probe.height()
                );
                nearest
            }
        };

        let fps_enum = match fps {
            30 => VideoFps::Fps30,
            60 => VideoFps::Fps60,
            other => {
                let nearest = if other > 45 {
                    VideoFps::Fps60
                } else {
                    VideoFps::Fps30
                };
                warn!("video fps {other} is not an Android Auto rate — advertising {nearest:?}");
                nearest
            }
        };

        Self {
            resolution,
            fps: fps_enum,
            ..Self::default()
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            resolution: VideoResolution::Res480p,
            fps: VideoFps::Fps30,
            dpi: 160,
            margin_width: 0,
            margin_height: 0,
        }
    }
}

/// Holds AA channel configuration. Incoming-message routing is done inline in the
/// bridge's protocol loop (`run_protocol`), so this type only carries the video
/// config and the message encoders used during channel setup.
pub struct ChannelManager {
    pub video_config: VideoConfig,
}

impl ChannelManager {
    /// Build a manager whose advertised video config matches the encoder that
    /// will actually run (the `--width/--height/--fps` CLI flags).
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            video_config: VideoConfig::from_dimensions(width, height, fps),
        }
    }

    /// Encode a video media setup response.
    pub fn encode_video_setup(&self) -> Vec<u8> {
        let config = &self.video_config;
        let mut payload = Vec::new();
        encode_varint_field(&mut payload, 1, config.resolution as u64);
        encode_varint_field(&mut payload, 2, config.fps as u64);
        encode_varint_field(&mut payload, 3, config.margin_width as u64);
        encode_varint_field(&mut payload, 4, config.margin_height as u64);
        encode_varint_field(&mut payload, 5, config.dpi as u64);

        let mut msg = Vec::with_capacity(2 + payload.len());
        msg.extend_from_slice(&MSG_MEDIA_SETUP_RESPONSE.to_be_bytes());
        msg.extend(payload);
        msg
    }

    /// Encode AVChannelSetupRequest (0x8000): phone asks headunit to set up audio.
    /// Field 1 = config_index (index into audio_configs from service discovery).
    pub fn encode_av_setup_request(config_index: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        encode_varint_field(&mut payload, 1, config_index as u64);

        let mut msg = Vec::with_capacity(2 + payload.len());
        msg.extend_from_slice(&MSG_MEDIA_SETUP_REQUEST.to_be_bytes());
        msg.extend(payload);
        msg
    }

    /// Encode AudioFocusRequest (0x0012): phone requests audio focus.
    /// Field 1 = focus_type: 1=GAIN, 2=GAIN_TRANSIENT, 4=RELEASE
    pub fn encode_audio_focus_request(focus_type: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        encode_varint_field(&mut payload, 1, focus_type as u64);

        let mut msg = Vec::with_capacity(2 + payload.len());
        msg.extend_from_slice(&MSG_AUDIO_FOCUS_REQUEST.to_be_bytes());
        msg.extend(payload);
        msg
    }

    /// Encode AVChannelStartIndication (0x8001): phone tells headunit streaming starts.
    /// Field 1 = session_id, Field 2 = config_index
    pub fn encode_av_start(session_id: i32, config_index: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        encode_varint_field(&mut payload, 1, session_id as u64);
        encode_varint_field(&mut payload, 2, config_index as u64);

        let mut msg = Vec::with_capacity(2 + payload.len());
        msg.extend_from_slice(&MSG_START_REQUEST.to_be_bytes());
        msg.extend(payload);
        msg
    }

    /// Encode a driving status sensor notification.
    /// Status 0 = UNRESTRICTED (parked). Required before AA will start.
    pub fn encode_sensor_driving_status(status: u8) -> Vec<u8> {
        // Simplified protobuf: SensorEventIndication.driving_status
        let inner = vec![0x08, status]; // field 1 varint
        let mut outer = vec![0x8a, 0x01, inner.len() as u8]; // field 17, LEN
        outer.extend(inner);

        let mut msg = Vec::with_capacity(2 + outer.len());
        msg.extend_from_slice(&MSG_MEDIA_SETUP_RESPONSE.to_be_bytes());
        msg.extend(outer);
        msg
    }
}

fn encode_varint_field(buf: &mut Vec<u8>, field_num: u8, value: u64) {
    let tag = (field_num << 3) | 0; // wire type 0 = varint
    buf.push(tag);
    encode_varint(buf, value);
}

fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value > 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}
