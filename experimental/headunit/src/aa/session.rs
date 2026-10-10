//! Head-unit protocol state machine (sans-IO).
//!
//! Bytes from the phone go in through [`Session::on_bytes`]; everything the
//! runtime must do comes out as [`Event`]s: bytes to write to USB, H.264 to
//! hand to the browser, PCM to play, state changes. No I/O, no clock, no
//! threads in here, so the whole exchange is unit-tested with a fake phone.
//!
//! Sequence (head unit = HU, phone = MD), from aasdk/openauto
//! `AndroidAutoEntity` and the per-service handlers:
//!
//! ```text
//! HU → VersionRequest(1.6)            MD → VersionResponse(major, minor, status)
//! HU ↔ ENCAPSULATED_SSL (TLS 1.2, HU is client, presents the HU cert)
//! HU → AuthComplete{status 0}         (plain)
//! MD → ServiceDiscoveryRequest        HU → ServiceDiscoveryResponse (encrypted)
//! MD → ChannelOpenRequest on each service channel (CONTROL flag)
//!                                     HU → ChannelOpenResponse{SUCCESS} (CONTROL flag)
//! MD → Setup (video/audio)            HU → Config{READY, max_unacked 1, [0]}
//!                                     HU → VideoFocusNotification{PROJECTED}
//! MD → Start{session}  → CodecConfig (SPS/PPS) → Data[ts|H.264]...
//!                                     HU → Ack{session, 1} per media message
//! MD → SensorRequest                  HU → SensorResponse + SensorBatch
//! MD → PingRequest (plain)            HU → PingResponse (plain, same timestamp)
//! MD → AudioFocusRequest              HU → AudioFocusNotification
//! MD → NavFocusRequest                HU → NavFocusNotification{PROJECTED}
//! HU → InputReport{TouchEvent} on the input channel
//! ```

use crate::aa::config::HuConfig;
use crate::aa::framing::{self, Cipher, Deframer, Message};
use crate::aa::ids::*;
use crate::aa::proto::{self, *};
use crate::aa::tls::TlsClient;
use crate::aa::touch::{TouchKind, TouchTracker};
use anyhow::{anyhow, Result};
use prost::Message as _;
use serde::Serialize;
use std::collections::HashMap;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Version,
    Tls,
    Discovery,
    Running,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Bytes to write to the phone, already framed (and encrypted).
    Write(Vec<u8>),
    Phase(Phase),
    /// The phone introduced itself (ServiceDiscoveryRequest).
    Phone { device_name: String, label: String },
    /// H.264 Annex-B. `config` = SPS/PPS (MEDIA_CODEC_CONFIG), no timestamp.
    Video { config: bool, timestamp_us: u64, data: Vec<u8> },
    VideoStart,
    VideoStop,
    AudioStart { channel: u8, rate: u32, channels: u32 },
    Audio { channel: u8, pcm: Vec<u8> },
    AudioStop { channel: u8 },
    /// The phone asked for native focus (user pressed "exit" in AA).
    PhoneWantsNative,
    Closed { reason: String },
}

/// PCM format of each audio sink we declare.
pub fn audio_format(channel: u8) -> Option<(u32, u32, i32)> {
    match channel {
        CH_MEDIA_AUDIO => Some((48_000, 2, audio_stream::MEDIA)),
        CH_GUIDANCE_AUDIO => Some((16_000, 1, audio_stream::GUIDANCE)),
        CH_SYSTEM_AUDIO => Some((16_000, 1, audio_stream::SYSTEM_AUDIO)),
        _ => None,
    }
}

const SERVICE_CHANNELS: [u8; 7] = [CH_SENSOR, CH_VIDEO, CH_MEDIA_AUDIO, CH_GUIDANCE_AUDIO, CH_SYSTEM_AUDIO, CH_MIC, CH_INPUT];
const SENSORS: [i32; 2] = [sensor_type::DRIVING_STATUS_DATA, sensor_type::NIGHT_MODE];

/// Adapter so the framing layer can use the TLS client as its cipher.
struct TlsCipher<'a>(&'a mut dyn TlsClient);
impl Cipher for TlsCipher<'_> {
    fn encrypt(&mut self, p: &[u8]) -> Result<Vec<u8>> {
        self.0.encrypt(p)
    }
    fn decrypt(&mut self, c: &[u8]) -> Result<Vec<u8>> {
        self.0.decrypt(c)
    }
}

pub struct Session {
    cfg: HuConfig,
    tls: Box<dyn TlsClient>,
    tls_up: bool,
    deframer: Deframer,
    phase: Phase,
    touch: TouchTracker,
    /// Media session id per channel (from Start), used in Acks.
    media_session: HashMap<u8, i32>,
    opened: Vec<u8>,
    night_mode: bool,
}

impl Session {
    pub fn new(cfg: HuConfig, tls: Box<dyn TlsClient>) -> Self {
        Self {
            cfg,
            tls,
            tls_up: false,
            deframer: Deframer::new(),
            phase: Phase::Version,
            touch: TouchTracker::new(),
            media_session: HashMap::new(),
            opened: Vec::new(),
            night_mode: false,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn config(&self) -> &HuConfig {
        &self.cfg
    }

    pub fn is_open(&self, channel: u8) -> bool {
        self.opened.contains(&channel)
    }

    /// First bytes on the wire: the VersionRequest.
    pub fn start(&mut self) -> Result<Vec<Event>> {
        let mut out = vec![Event::Phase(Phase::Version)];
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(&VERSION_MAJOR.to_be_bytes());
        body.extend_from_slice(&VERSION_MINOR.to_be_bytes());
        self.send_raw(&mut out, CH_CONTROL, false, false, MSG_VERSION_REQUEST, &body)?;
        Ok(out)
    }

    /// Bytes read from the phone. Errors are fatal for this connection.
    pub fn on_bytes(&mut self, data: &[u8]) -> Result<Vec<Event>> {
        self.deframer.push(data);
        let mut out = Vec::new();
        loop {
            let msg = if self.tls_up {
                self.deframer.next(Some(&mut TlsCipher(self.tls.as_mut())))?
            } else {
                self.deframer.next(None)?
            };
            let Some(msg) = msg else { break };
            self.handle(msg, &mut out)?;
            if self.phase == Phase::Closed {
                break;
            }
        }
        Ok(out)
    }

    /// A pointer event from the page. `u`,`v` are 0..1 over the shown content.
    pub fn touch(&mut self, kind: TouchKind, page_id: u32, u: f64, v: f64, timestamp_us: u64) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if self.phase != Phase::Running || !self.is_open(CH_INPUT) {
            return Ok(out);
        }
        let (x, y) = self.cfg.touch_point(u, v);
        if let Some(ev) = self.touch.event(kind, page_id, x, y) {
            self.send_touch(&mut out, ev, timestamp_us)?;
        }
        Ok(out)
    }

    /// Lift all fingers (the page went away mid-gesture).
    pub fn touch_cancel(&mut self, timestamp_us: u64) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if self.phase != Phase::Running || !self.is_open(CH_INPUT) {
            self.touch.cancel_all();
            return Ok(out);
        }
        for ev in self.touch.cancel_all() {
            self.send_touch(&mut out, ev, timestamp_us)?;
        }
        Ok(out)
    }

    /// Unsolicited video focus change. PROJECTED after NATIVE makes the phone
    /// restart its encoder, i.e. send SPS/PPS + an IDR (how real head units
    /// come back from their native screen) — used to get a keyframe for a
    /// page that joins mid-stream. Hypothesis until checked on the phone.
    pub fn set_video_focus(&mut self, projected: bool) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if self.phase != Phase::Running || !self.is_open(CH_VIDEO) {
            return Ok(out);
        }
        let focus = if projected { video_focus::PROJECTED } else { video_focus::NATIVE };
        let n = VideoFocusNotification { focus: Some(focus), unsolicited: Some(true) };
        self.send_proto(&mut out, CH_VIDEO, false, MEDIA_VIDEO_FOCUS_NOTIFICATION, &n)?;
        Ok(out)
    }

    /// Day/night theme for the phone's UI.
    pub fn set_night_mode(&mut self, night: bool) -> Result<Vec<Event>> {
        self.night_mode = night;
        let mut out = Vec::new();
        if self.phase == Phase::Running && self.is_open(CH_SENSOR) {
            self.send_sensor(&mut out, sensor_type::NIGHT_MODE)?;
        }
        Ok(out)
    }

    /// Ask the phone to end the session cleanly.
    pub fn bye(&mut self) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if self.tls_up {
            // ByeByeReason USER_SELECTION = 1
            self.send_proto(&mut out, CH_CONTROL, false, MSG_BYEBYE_REQUEST, &ByeByeRequest { reason: 1 })?;
        }
        Ok(out)
    }

    // ── Dispatch ─────────────────────────────────────────────────────────

    fn handle(&mut self, msg: Message, out: &mut Vec<Event>) -> Result<()> {
        let Some(id) = msg.id() else {
            warn!("aa: ch{} message shorter than its id ({} bytes)", msg.channel, msg.payload.len());
            return Ok(());
        };
        let body = msg.body();
        if msg.channel != CH_CONTROL && id == MSG_CHANNEL_OPEN_REQUEST {
            return self.on_channel_open(msg.channel, body, out);
        }
        match msg.channel {
            CH_CONTROL => self.on_control(id, body, out),
            CH_VIDEO => self.on_video(id, body, out),
            CH_MEDIA_AUDIO | CH_GUIDANCE_AUDIO | CH_SYSTEM_AUDIO => self.on_audio(msg.channel, id, body, out),
            CH_SENSOR => self.on_sensor(id, body, out),
            CH_INPUT => self.on_input(id, body, out),
            CH_MIC => self.on_mic(id, body, out),
            ch => {
                debug!("aa: message 0x{id:04x} on undeclared channel {ch}");
                Ok(())
            }
        }
    }

    /// Microphone channel. Android Auto stalls right after service discovery
    /// when the head unit declares no microphone (openauto always declares one;
    /// 2026-10-05 an Android 16 phone opened no channel at all without it). We accept
    /// the open/close requests; no car-mic audio is sent yet.
    fn on_mic(&mut self, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        match id {
            MEDIA_MICROPHONE_REQUEST => {
                let req = MicrophoneRequest::decode(body)?;
                info!("aa: microphone {} requested (no car mic yet: silence)", if req.open { "open" } else { "close" });
                self.send_proto(out, CH_MIC, false, MEDIA_MICROPHONE_RESPONSE, &MicrophoneResponse { status: 0, session_id: Some(0) })
            }
            MEDIA_ACK => Ok(()),
            MEDIA_SETUP => self.send_media_config(CH_MIC, out),
            other => {
                debug!("aa: mic message 0x{other:04x} ignored");
                Ok(())
            }
        }
    }

    fn on_control(&mut self, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        match id {
            MSG_VERSION_RESPONSE => {
                if body.len() < 6 {
                    return Err(anyhow!("VersionResponse too short ({} bytes)", body.len()));
                }
                let major = u16::from_be_bytes([body[0], body[1]]);
                let minor = u16::from_be_bytes([body[2], body[3]]);
                let status = i16::from_be_bytes([body[4], body[5]]);
                info!("aa: phone speaks {major}.{minor}, version status {status}");
                // aasdk only gives up on STATUS_NO_COMPATIBLE_VERSION (-1).
                if status == -1 {
                    self.close(out, format!("phone refused protocol {VERSION_MAJOR}.{VERSION_MINOR} (speaks {major}.{minor})"));
                    return Ok(());
                }
                self.set_phase(out, Phase::Tls);
                let hello = self.tls.start()?;
                self.send_raw(out, CH_CONTROL, false, false, MSG_ENCAPSULATED_SSL, &hello)?;
            }
            MSG_ENCAPSULATED_SSL => {
                if self.tls_up {
                    warn!("aa: SSL handshake message after TLS completed — ignored");
                    return Ok(());
                }
                let reply = self.tls.feed(body)?;
                if !reply.is_empty() {
                    self.send_raw(out, CH_CONTROL, false, false, MSG_ENCAPSULATED_SSL, &reply)?;
                }
                if !self.tls.is_handshaking() {
                    self.tls_up = true;
                    info!("aa: TLS up, sending AuthComplete");
                    let auth = AuthResponse { status: proto::status::SUCCESS }.encode_to_vec();
                    self.send_raw(out, CH_CONTROL, false, false, MSG_AUTH_COMPLETE, &auth)?;
                    self.set_phase(out, Phase::Discovery);
                }
            }
            MSG_SERVICE_DISCOVERY_REQUEST => {
                let req = ServiceDiscoveryRequest::decode(body).unwrap_or_default();
                let device_name = req.device_name.unwrap_or_default();
                let label = req.label_text.unwrap_or_default();
                info!("aa: service discovery from {device_name:?} ({label:?})");
                out.push(Event::Phone { device_name, label });
                let sdr = build_service_discovery(&self.cfg);
                self.send_proto(out, CH_CONTROL, false, MSG_SERVICE_DISCOVERY_RESPONSE, &sdr)?;
                self.set_phase(out, Phase::Running);
            }
            MSG_PING_REQUEST => {
                let req = PingRequest::decode(body).unwrap_or_default();
                let resp = PingResponse { timestamp: req.timestamp, data: None }.encode_to_vec();
                // aasdk sends ping messages in the clear (ControlServiceChannel::sendPingResponse).
                self.send_raw(out, CH_CONTROL, false, false, MSG_PING_RESPONSE, &resp)?;
            }
            MSG_PING_RESPONSE => {}
            MSG_AUDIO_FOCUS_REQUEST => {
                let req = AudioFocusRequest::decode(body).unwrap_or_default();
                // openauto: RELEASE → LOSS, any gain request → GAIN.
                let state = if req.audio_focus_type == audio_focus_request::RELEASE {
                    audio_focus_state::LOSS
                } else {
                    audio_focus_state::GAIN
                };
                debug!("aa: audio focus request {} → {}", req.audio_focus_type, state);
                let n = AudioFocusNotification { focus_state: state, unsolicited: None };
                self.send_proto(out, CH_CONTROL, false, MSG_AUDIO_FOCUS_NOTIFICATION, &n)?;
            }
            MSG_NAV_FOCUS_REQUEST => {
                let n = NavFocusNotification { focus_type: nav_focus::PROJECTED };
                self.send_proto(out, CH_CONTROL, false, MSG_NAV_FOCUS_NOTIFICATION, &n)?;
            }
            MSG_BYEBYE_REQUEST => {
                let reason = ByeByeRequest::decode(body).map(|r| r.reason).unwrap_or(0);
                self.send_proto(out, CH_CONTROL, false, MSG_BYEBYE_RESPONSE, &ByeByeResponse {})?;
                self.close(out, format!("phone said bye (reason {reason})"));
            }
            MSG_BYEBYE_RESPONSE => self.close(out, "bye acknowledged".into()),
            MSG_VOICE_SESSION_NOTIFICATION | MSG_BATTERY_STATUS_NOTIFICATION | MSG_CHANNEL_CLOSE_NOTIFICATION => {
                debug!("aa: control notification 0x{id:04x} ({} bytes)", body.len());
            }
            _ => debug!("aa: unhandled control message 0x{id:04x} ({} bytes)", body.len()),
        }
        Ok(())
    }

    fn on_channel_open(&mut self, channel: u8, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        let req = ChannelOpenRequest::decode(body).ok();
        let ok = SERVICE_CHANNELS.contains(&channel);
        info!(
            "aa: open channel {channel} (service {:?}, priority {:?}) → {}",
            req.as_ref().map(|r| r.service_id),
            req.as_ref().map(|r| r.priority),
            if ok { "ok" } else { "invalid" }
        );
        let status = if ok { proto::status::SUCCESS } else { proto::status::INVALID_CHANNEL };
        self.send_proto(out, channel, true, MSG_CHANNEL_OPEN_RESPONSE, &ChannelOpenResponse { status })?;
        if ok && !self.opened.contains(&channel) {
            self.opened.push(channel);
        }
        Ok(())
    }

    fn send_media_config(&mut self, channel: u8, out: &mut Vec<Event>) -> Result<()> {
        let cfg = Config { status: config_status::READY, max_unacked: Some(1), configuration_indices: vec![0] };
        self.send_proto(out, channel, false, MEDIA_CONFIG, &cfg)
    }

    fn ack(&mut self, channel: u8, out: &mut Vec<Event>) -> Result<()> {
        let session_id = self.media_session.get(&channel).copied().unwrap_or(0);
        self.send_proto(out, channel, false, MEDIA_ACK, &Ack { session_id, ack: Some(1) })
    }

    fn on_video(&mut self, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        match id {
            MEDIA_SETUP => {
                let codec = Setup::decode(body).map(|s| s.r#type).unwrap_or(0);
                info!("aa: video setup, codec {codec}");
                self.send_media_config(CH_VIDEO, out)?;
                if self.cfg.video_focus_after_setup {
                    let n = VideoFocusNotification { focus: Some(video_focus::PROJECTED), unsolicited: Some(false) };
                    self.send_proto(out, CH_VIDEO, false, MEDIA_VIDEO_FOCUS_NOTIFICATION, &n)?;
                }
            }
            MEDIA_START => {
                let s = Start::decode(body).unwrap_or_default();
                info!("aa: video start, session {} config {}", s.session_id, s.configuration_index);
                self.media_session.insert(CH_VIDEO, s.session_id);
                out.push(Event::VideoStart);
            }
            MEDIA_STOP => out.push(Event::VideoStop),
            MEDIA_CODEC_CONFIG => {
                out.push(Event::Video { config: true, timestamp_us: 0, data: body.to_vec() });
                self.ack(CH_VIDEO, out)?;
            }
            MEDIA_DATA => {
                if body.len() < 8 {
                    return Err(anyhow!("video data without its timestamp ({} bytes)", body.len()));
                }
                let ts = u64::from_be_bytes(body[..8].try_into().expect("8 bytes"));
                out.push(Event::Video { config: false, timestamp_us: ts, data: body[8..].to_vec() });
                self.ack(CH_VIDEO, out)?;
            }
            MEDIA_VIDEO_FOCUS_REQUEST => {
                let r = VideoFocusRequestNotification::decode(body).unwrap_or_default();
                info!("aa: phone requests video focus mode {:?} (reason {:?})", r.mode, r.reason);
                if r.mode == Some(video_focus::NATIVE) {
                    out.push(Event::PhoneWantsNative);
                }
                // openauto always answers PROJECTED: the page stays the AA screen.
                let n = VideoFocusNotification { focus: Some(video_focus::PROJECTED), unsolicited: Some(false) };
                self.send_proto(out, CH_VIDEO, false, MEDIA_VIDEO_FOCUS_NOTIFICATION, &n)?;
            }
            _ => debug!("aa: unhandled video message 0x{id:04x}"),
        }
        Ok(())
    }

    fn on_audio(&mut self, channel: u8, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        match id {
            MEDIA_SETUP => self.send_media_config(channel, out)?,
            MEDIA_START => {
                let s = Start::decode(body).unwrap_or_default();
                self.media_session.insert(channel, s.session_id);
                let (rate, channels, _) = audio_format(channel).expect("audio channel");
                out.push(Event::AudioStart { channel, rate, channels });
            }
            MEDIA_STOP => out.push(Event::AudioStop { channel }),
            MEDIA_CODEC_CONFIG => self.ack(channel, out)?,
            MEDIA_DATA => {
                if body.len() >= 8 {
                    out.push(Event::Audio { channel, pcm: body[8..].to_vec() });
                }
                self.ack(channel, out)?;
            }
            _ => debug!("aa: unhandled audio message 0x{id:04x} on ch{channel}"),
        }
        Ok(())
    }

    fn on_sensor(&mut self, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        if id != SENSOR_REQUEST {
            debug!("aa: unhandled sensor message 0x{id:04x}");
            return Ok(());
        }
        let req = SensorRequest::decode(body).unwrap_or_default();
        let known = SENSORS.contains(&req.r#type);
        let status = if known { proto::status::SUCCESS } else { proto::status::INVALID_SENSOR };
        debug!("aa: sensor request type {} → status {status}", req.r#type);
        self.send_proto(out, CH_SENSOR, false, SENSOR_RESPONSE, &SensorStartResponse { status })?;
        if known {
            self.send_sensor(out, req.r#type)?;
        }
        Ok(())
    }

    fn send_sensor(&mut self, out: &mut Vec<Event>, kind: i32) -> Result<()> {
        let mut batch = SensorBatch::default();
        if kind == sensor_type::DRIVING_STATUS_DATA {
            batch.driving_status_data.push(DrivingStatusData { status: driving_status::UNRESTRICTED });
        } else if kind == sensor_type::NIGHT_MODE {
            batch.night_mode_data.push(NightModeData { night_mode: Some(self.night_mode) });
        } else {
            return Ok(());
        }
        self.send_proto(out, CH_SENSOR, false, SENSOR_BATCH, &batch)
    }

    fn on_input(&mut self, id: u16, body: &[u8], out: &mut Vec<Event>) -> Result<()> {
        if id == INPUT_KEY_BINDING_REQUEST {
            let req = KeyBindingRequest::decode(body).unwrap_or_default();
            debug!("aa: key binding request {:?}", req.keycodes);
            self.send_proto(out, CH_INPUT, false, INPUT_KEY_BINDING_RESPONSE, &KeyBindingResponse { status: 0 })?;
        } else {
            debug!("aa: unhandled input message 0x{id:04x}");
        }
        Ok(())
    }

    // ── Output helpers ───────────────────────────────────────────────────

    fn send_touch(&mut self, out: &mut Vec<Event>, ev: TouchEvent, timestamp_us: u64) -> Result<()> {
        let report = InputReport { timestamp: timestamp_us, disp_channel_id: None, touch_event: Some(ev) };
        self.send_proto(out, CH_INPUT, false, INPUT_REPORT, &report)
    }

    fn send_proto<M: prost::Message>(&mut self, out: &mut Vec<Event>, ch: u8, control: bool, id: u16, m: &M) -> Result<()> {
        let body = m.encode_to_vec();
        self.send_raw(out, ch, control, true, id, &body)
    }

    fn send_raw(&mut self, out: &mut Vec<Event>, ch: u8, control: bool, encrypted: bool, id: u16, body: &[u8]) -> Result<()> {
        let mut payload = Vec::with_capacity(2 + body.len());
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(body);
        let wire = if encrypted {
            if !self.tls_up {
                return Err(anyhow!("refusing to send 0x{id:04x} on ch{ch} before TLS is up"));
            }
            framing::encode(ch, control, true, &payload, Some(&mut TlsCipher(self.tls.as_mut())))?
        } else {
            framing::encode(ch, control, false, &payload, None)?
        };
        out.push(Event::Write(wire));
        Ok(())
    }

    fn set_phase(&mut self, out: &mut Vec<Event>, p: Phase) {
        if self.phase != p {
            self.phase = p;
            out.push(Event::Phase(p));
        }
    }

    fn close(&mut self, out: &mut Vec<Event>, reason: String) {
        info!("aa: session closed: {reason}");
        self.set_phase(out, Phase::Closed);
        out.push(Event::Closed { reason });
    }
}

/// The services this head unit offers: video, three PCM sinks, touch input,
/// driving-status + night-mode sensors. No microphone and no Bluetooth yet.
pub fn build_service_discovery(cfg: &HuConfig) -> ServiceDiscoveryResponse {
    let (wm, hm) = cfg.margins();
    let (w, h) = cfg.resolution.size();
    let mut channels = vec![
        Service {
            id: CH_SENSOR as i32,
            sensor_source_service: Some(SensorSourceService {
                sensors: SENSORS.iter().map(|&t| Sensor { sensor_type: t }).collect(),
                location_characterization: None,
            }),
            ..Default::default()
        },
        Service {
            id: CH_VIDEO as i32,
            media_sink_service: Some(MediaSinkService {
                available_type: Some(codec::VIDEO_H264_BP),
                available_while_in_call: Some(true),
                video_configs: vec![VideoConfiguration {
                    codec_resolution: Some(cfg.resolution.proto()),
                    frame_rate: Some(if cfg.fps60 { fps::FPS_60 } else { fps::FPS_30 }),
                    width_margin: Some(wm),
                    height_margin: Some(hm),
                    density: Some(cfg.density),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
    ];
    for ch in [CH_MEDIA_AUDIO, CH_GUIDANCE_AUDIO, CH_SYSTEM_AUDIO] {
        let (rate, nch, stream) = audio_format(ch).expect("audio channel");
        channels.push(Service {
            id: ch as i32,
            media_sink_service: Some(MediaSinkService {
                available_type: Some(codec::AUDIO_PCM),
                audio_type: Some(stream),
                audio_configs: vec![AudioConfiguration { sampling_rate: rate, number_of_bits: 16, number_of_channels: nch }],
                available_while_in_call: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    // Microphone: 16 kHz mono 16-bit PCM, as openauto declares it.
    channels.push(Service {
        id: CH_MIC as i32,
        media_source_service: Some(MediaSourceService {
            available_type: Some(codec::AUDIO_PCM),
            audio_config: Some(AudioConfiguration { sampling_rate: 16000, number_of_bits: 16, number_of_channels: 1 }),
            available_while_in_call: Some(true),
        }),
        ..Default::default()
    });
    channels.push(Service {
        id: CH_INPUT as i32,
        input_source_service: Some(InputSourceService {
            keycodes_supported: vec![],
            touchscreen: vec![TouchScreen {
                width: w as i32,
                height: h as i32,
                r#type: Some(touchscreen_type::CAPACITIVE),
                is_secondary: None,
            }],
            display_id: None,
        }),
        ..Default::default()
    });

    ServiceDiscoveryResponse {
        channels,
        // Deprecated identity fields: still filled, older AA versions read them.
        make: Some(cfg.make.clone()),
        model: Some(cfg.model.clone()),
        year: Some(cfg.year.clone()),
        vehicle_id: Some(cfg.vehicle_id.clone()),
        driver_position: Some(if cfg.driver_left { driver_position::LEFT } else { 1 }),
        head_unit_make: Some(cfg.hu_make.clone()),
        head_unit_model: Some(cfg.hu_model.clone()),
        head_unit_software_build: Some(cfg.sw_build.clone()),
        head_unit_software_version: Some(cfg.sw_version.clone()),
        can_play_native_media_during_vr: Some(false),
        session_configuration: None,
        display_name: Some(cfg.display_name.clone()),
        probe_for_support: Some(false),
        // Same ping settings as openauto: the phone pings every second and
        // gives up after 3 s without an answer.
        connection_configuration: Some(ConnectionConfiguration {
            ping_configuration: Some(PingConfiguration {
                timeout_ms: Some(3000),
                interval_ms: Some(1000),
                high_latency_threshold_ms: Some(200),
                tracked_ping_count: Some(5),
            }),
        }),
        headunit_info: Some(HeadUnitInfo {
            make: Some(cfg.make.clone()),
            model: Some(cfg.model.clone()),
            year: Some(cfg.year.clone()),
            vehicle_id: Some(cfg.vehicle_id.clone()),
            head_unit_make: Some(cfg.hu_make.clone()),
            head_unit_model: Some(cfg.hu_model.clone()),
            head_unit_software_build: Some(cfg.sw_build.clone()),
            head_unit_software_version: Some(cfg.sw_version.clone()),
        }),
    }
}

#[cfg(test)]
mod tests {
    //! A scripted fake phone drives the session through the whole lifecycle.
    use super::*;
    use crate::aa::framing::tests::XorCipher;

    /// Fake TLS: ClientHello = "HELLO"; the phone's "SERVER" completes the
    /// handshake and makes the client emit "FINISHED". Records use XorCipher.
    struct FakeTls {
        done: bool,
        x: XorCipher,
    }
    impl Cipher for FakeTls {
        fn encrypt(&mut self, p: &[u8]) -> Result<Vec<u8>> {
            self.x.encrypt(p)
        }
        fn decrypt(&mut self, c: &[u8]) -> Result<Vec<u8>> {
            self.x.decrypt(c)
        }
    }
    impl TlsClient for FakeTls {
        fn start(&mut self) -> Result<Vec<u8>> {
            Ok(b"HELLO".to_vec())
        }
        fn feed(&mut self, i: &[u8]) -> Result<Vec<u8>> {
            assert_eq!(i, b"SERVER");
            self.done = true;
            Ok(b"FINISHED".to_vec())
        }
        fn is_handshaking(&self) -> bool {
            !self.done
        }
    }

    fn session() -> Session {
        Session::new(HuConfig::default(), Box::new(FakeTls { done: false, x: XorCipher }))
    }

    /// Phone-side message bytes.
    fn phone(ch: u8, control: bool, encrypted: bool, id: u16, body: &[u8]) -> Vec<u8> {
        let mut p = id.to_be_bytes().to_vec();
        p.extend_from_slice(body);
        let mut x = XorCipher;
        framing::encode(ch, control, encrypted, &p, if encrypted { Some(&mut x) } else { None }).unwrap()
    }

    /// Decode every Write event back into messages (as the phone would).
    fn written(evs: &[Event]) -> Vec<Message> {
        let mut d = Deframer::new();
        for e in evs {
            if let Event::Write(b) = e {
                d.push(b);
            }
        }
        let mut v = Vec::new();
        while let Some(m) = d.next(Some(&mut XorCipher)).unwrap() {
            v.push(m);
        }
        v
    }

    fn one(evs: &[Event]) -> Message {
        let w = written(evs);
        assert_eq!(w.len(), 1, "expected exactly one message, got {w:?}");
        w.into_iter().next().unwrap()
    }

    /// Run the session up to Running with all service channels open.
    fn running() -> Session {
        let mut s = session();
        s.start().unwrap();
        s.on_bytes(&phone(0, false, false, MSG_VERSION_RESPONSE, &[0, 1, 0, 7, 0, 0])).unwrap();
        s.on_bytes(&phone(0, false, false, MSG_ENCAPSULATED_SSL, b"SERVER")).unwrap();
        s.on_bytes(&phone(0, false, true, MSG_SERVICE_DISCOVERY_REQUEST, &[])).unwrap();
        for ch in SERVICE_CHANNELS {
            let req = ChannelOpenRequest { priority: 0, service_id: ch as i32 }.encode_to_vec();
            s.on_bytes(&phone(ch, true, true, MSG_CHANNEL_OPEN_REQUEST, &req)).unwrap();
        }
        assert_eq!(s.phase(), Phase::Running);
        s
    }

    #[test]
    fn version_request_is_plain_1_6() {
        let mut s = session();
        let evs = s.start().unwrap();
        let m = one(&evs);
        assert_eq!((m.channel, m.encrypted, m.control), (0, false, false));
        assert_eq!(m.payload, vec![0x00, 0x01, 0x00, 0x01, 0x00, 0x06]);
    }

    #[test]
    fn handshake_then_auth_complete_in_the_clear() {
        let mut s = session();
        s.start().unwrap();
        let evs = s.on_bytes(&phone(0, false, false, MSG_VERSION_RESPONSE, &[0, 1, 0, 7, 0, 0])).unwrap();
        assert!(evs.contains(&Event::Phase(Phase::Tls)));
        let m = one(&evs);
        assert_eq!(m.id(), Some(MSG_ENCAPSULATED_SSL));
        assert_eq!(m.body(), b"HELLO");

        let evs = s.on_bytes(&phone(0, false, false, MSG_ENCAPSULATED_SSL, b"SERVER")).unwrap();
        let w = written(&evs);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].id(), w[0].body()), (Some(MSG_ENCAPSULATED_SSL), &b"FINISHED"[..]));
        assert_eq!(w[1].id(), Some(MSG_AUTH_COMPLETE));
        assert!(!w[1].encrypted, "aasdk sends AuthComplete in the clear");
        assert_eq!(w[1].body(), &[0x08, 0x00]);
        assert_eq!(s.phase(), Phase::Discovery);
    }

    #[test]
    fn incompatible_version_closes() {
        let mut s = session();
        s.start().unwrap();
        let evs = s.on_bytes(&phone(0, false, false, MSG_VERSION_RESPONSE, &[0, 1, 0, 0, 0xFF, 0xFF])).unwrap();
        assert!(matches!(evs.last(), Some(Event::Closed { .. })));
    }

    #[test]
    fn service_discovery_response_content() {
        let mut s = session();
        s.start().unwrap();
        s.on_bytes(&phone(0, false, false, MSG_VERSION_RESPONSE, &[0, 1, 0, 7, 0, 0])).unwrap();
        s.on_bytes(&phone(0, false, false, MSG_ENCAPSULATED_SSL, b"SERVER")).unwrap();
        let req = ServiceDiscoveryRequest { device_name: Some("Pixel 9".into()), label_text: Some("Alex".into()), ..Default::default() };
        let evs = s.on_bytes(&phone(0, false, true, MSG_SERVICE_DISCOVERY_REQUEST, &req.encode_to_vec())).unwrap();
        assert!(evs.contains(&Event::Phone { device_name: "Pixel 9".into(), label: "Alex".into() }));
        let m = one(&evs);
        assert!(m.encrypted);
        assert_eq!(m.id(), Some(MSG_SERVICE_DISCOVERY_RESPONSE));
        let sdr = ServiceDiscoveryResponse::decode(m.body()).unwrap();
        let ids: Vec<i32> = sdr.channels.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 3, 4, 5, 6, 7, 8]);
        let mic = sdr.channels.iter().find(|c| c.id == CH_MIC as i32).and_then(|c| c.media_source_service.clone()).unwrap();
        assert_eq!(mic.audio_config, Some(AudioConfiguration { sampling_rate: 16000, number_of_bits: 16, number_of_channels: 1 }));
        let video = sdr.channels[1].media_sink_service.as_ref().unwrap();
        assert_eq!(video.available_type, Some(codec::VIDEO_H264_BP));
        let vc = &video.video_configs[0];
        assert_eq!(vc.codec_resolution, Some(resolution::R1280X720));
        assert_eq!(vc.frame_rate, Some(fps::FPS_30));
        assert_eq!((vc.width_margin, vc.height_margin), (Some(0), Some(190)));
        let media = sdr.channels[2].media_sink_service.as_ref().unwrap();
        assert_eq!(media.audio_type, Some(audio_stream::MEDIA));
        assert_eq!(media.audio_configs[0], AudioConfiguration { sampling_rate: 48000, number_of_bits: 16, number_of_channels: 2 });
        let touch = &sdr.channels.iter().find(|c| c.id == CH_INPUT as i32).unwrap().input_source_service.as_ref().unwrap().touchscreen[0];
        assert_eq!((touch.width, touch.height), (1280, 720));
        let sensors: Vec<i32> = sdr.channels[0].sensor_source_service.as_ref().unwrap().sensors.iter().map(|x| x.sensor_type).collect();
        assert_eq!(sensors, vec![13, 10]);
        assert_eq!(sdr.connection_configuration.unwrap().ping_configuration.unwrap().interval_ms, Some(1000));
        assert_eq!(s.phase(), Phase::Running);
    }

    #[test]
    fn channel_open_response_carries_control_flag() {
        let mut s = running();
        let req = ChannelOpenRequest { priority: 0, service_id: 3 }.encode_to_vec();
        let evs = s.on_bytes(&phone(CH_VIDEO, true, true, MSG_CHANNEL_OPEN_REQUEST, &req)).unwrap();
        let m = one(&evs);
        assert_eq!((m.channel, m.control, m.encrypted), (CH_VIDEO, true, true));
        assert_eq!(m.id(), Some(MSG_CHANNEL_OPEN_RESPONSE));
        assert_eq!(ChannelOpenResponse::decode(m.body()).unwrap().status, 0);
    }

    #[test]
    fn unknown_channel_open_is_refused() {
        let mut s = running();
        let req = ChannelOpenRequest { priority: 0, service_id: 9 }.encode_to_vec();
        let m = one(&s.on_bytes(&phone(9, true, true, MSG_CHANNEL_OPEN_REQUEST, &req)).unwrap());
        assert_eq!(ChannelOpenResponse::decode(m.body()).unwrap().status, proto::status::INVALID_CHANNEL);
        assert!(!s.is_open(9));
    }

    #[test]
    fn video_setup_start_config_data_and_acks() {
        let mut s = running();
        let setup = Setup { r#type: codec::VIDEO_H264_BP }.encode_to_vec();
        let w = written(&s.on_bytes(&phone(CH_VIDEO, false, true, MEDIA_SETUP, &setup)).unwrap());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].id(), Some(MEDIA_CONFIG));
        assert_eq!(Config::decode(w[0].body()).unwrap(), Config { status: config_status::READY, max_unacked: Some(1), configuration_indices: vec![0] });
        assert_eq!(w[1].id(), Some(MEDIA_VIDEO_FOCUS_NOTIFICATION));
        assert_eq!(VideoFocusNotification::decode(w[1].body()).unwrap().focus, Some(video_focus::PROJECTED));

        let start = Start { session_id: 42, configuration_index: 0 }.encode_to_vec();
        let evs = s.on_bytes(&phone(CH_VIDEO, false, true, MEDIA_START, &start)).unwrap();
        assert_eq!(evs, vec![Event::VideoStart]);

        let sps_pps = [0, 0, 0, 1, 0x67, 0x42, 0xE0, 0x1F, 0, 0, 0, 1, 0x68, 0xCE];
        let evs = s.on_bytes(&phone(CH_VIDEO, false, true, MEDIA_CODEC_CONFIG, &sps_pps)).unwrap();
        assert_eq!(evs[0], Event::Video { config: true, timestamp_us: 0, data: sps_pps.to_vec() });
        let ack = one(&evs);
        assert_eq!(ack.id(), Some(MEDIA_ACK));
        assert_eq!(Ack::decode(ack.body()).unwrap(), Ack { session_id: 42, ack: Some(1) });

        // A large IDR split across frames.
        let mut data = 123_456_789u64.to_be_bytes().to_vec();
        let idr: Vec<u8> = [0, 0, 0, 1, 0x65].iter().copied().chain(std::iter::repeat_n(0xAB, 50_000)).collect();
        data.extend_from_slice(&idr);
        let evs = s.on_bytes(&phone(CH_VIDEO, false, true, MEDIA_DATA, &data)).unwrap();
        assert_eq!(evs[0], Event::Video { config: false, timestamp_us: 123_456_789, data: idr });
        assert_eq!(one(&evs).id(), Some(MEDIA_ACK));
    }

    #[test]
    fn audio_start_data_ack() {
        let mut s = running();
        s.on_bytes(&phone(CH_MEDIA_AUDIO, false, true, MEDIA_SETUP, &Setup { r#type: codec::AUDIO_PCM }.encode_to_vec())).unwrap();
        let evs = s.on_bytes(&phone(CH_MEDIA_AUDIO, false, true, MEDIA_START, &Start { session_id: 7, configuration_index: 0 }.encode_to_vec())).unwrap();
        assert_eq!(evs, vec![Event::AudioStart { channel: CH_MEDIA_AUDIO, rate: 48000, channels: 2 }]);
        let mut data = 5u64.to_be_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3, 4]);
        let evs = s.on_bytes(&phone(CH_MEDIA_AUDIO, false, true, MEDIA_DATA, &data)).unwrap();
        assert_eq!(evs[0], Event::Audio { channel: CH_MEDIA_AUDIO, pcm: vec![1, 2, 3, 4] });
        assert_eq!(Ack::decode(one(&evs).body()).unwrap().session_id, 7);
        let evs = s.on_bytes(&phone(CH_MEDIA_AUDIO, false, true, MEDIA_STOP, &[])).unwrap();
        assert_eq!(evs, vec![Event::AudioStop { channel: CH_MEDIA_AUDIO }]);
    }

    #[test]
    fn ping_is_answered_in_the_clear_with_same_timestamp() {
        let mut s = running();
        let req = PingRequest { timestamp: 987_654, bug_report: None, data: None }.encode_to_vec();
        let m = one(&s.on_bytes(&phone(0, false, false, MSG_PING_REQUEST, &req)).unwrap());
        assert_eq!(m.id(), Some(MSG_PING_RESPONSE));
        assert!(!m.encrypted);
        assert_eq!(PingResponse::decode(m.body()).unwrap().timestamp, 987_654);
    }

    #[test]
    fn audio_and_nav_focus() {
        let mut s = running();
        let gain = AudioFocusRequest { audio_focus_type: audio_focus_request::GAIN_TRANSIENT }.encode_to_vec();
        let m = one(&s.on_bytes(&phone(0, false, true, MSG_AUDIO_FOCUS_REQUEST, &gain)).unwrap());
        assert_eq!(AudioFocusNotification::decode(m.body()).unwrap().focus_state, audio_focus_state::GAIN);
        let rel = AudioFocusRequest { audio_focus_type: audio_focus_request::RELEASE }.encode_to_vec();
        let m = one(&s.on_bytes(&phone(0, false, true, MSG_AUDIO_FOCUS_REQUEST, &rel)).unwrap());
        assert_eq!(AudioFocusNotification::decode(m.body()).unwrap().focus_state, audio_focus_state::LOSS);
        let m = one(&s.on_bytes(&phone(0, false, true, MSG_NAV_FOCUS_REQUEST, &[0x08, 0x02])).unwrap());
        assert_eq!(m.id(), Some(MSG_NAV_FOCUS_NOTIFICATION));
        assert_eq!(NavFocusNotification::decode(m.body()).unwrap().focus_type, nav_focus::PROJECTED);
    }

    #[test]
    fn sensor_request_gets_response_and_batch() {
        let mut s = running();
        let req = SensorRequest { r#type: sensor_type::DRIVING_STATUS_DATA, min_update_period: 0 }.encode_to_vec();
        let w = written(&s.on_bytes(&phone(CH_SENSOR, false, true, SENSOR_REQUEST, &req)).unwrap());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].id(), Some(SENSOR_RESPONSE));
        assert_eq!(SensorStartResponse::decode(w[0].body()).unwrap().status, 0);
        assert_eq!(w[1].id(), Some(SENSOR_BATCH));
        assert_eq!(SensorBatch::decode(w[1].body()).unwrap().driving_status_data[0].status, driving_status::UNRESTRICTED);

        let req = SensorRequest { r#type: sensor_type::LOCATION, min_update_period: 0 }.encode_to_vec();
        let m = one(&s.on_bytes(&phone(CH_SENSOR, false, true, SENSOR_REQUEST, &req)).unwrap());
        assert_eq!(SensorStartResponse::decode(m.body()).unwrap().status, proto::status::INVALID_SENSOR);

        let m = one(&s.set_night_mode(true).unwrap());
        assert_eq!(SensorBatch::decode(m.body()).unwrap().night_mode_data[0].night_mode, Some(true));
    }

    #[test]
    fn key_binding_request_succeeds() {
        let mut s = running();
        let m = one(&s.on_bytes(&phone(CH_INPUT, false, true, INPUT_KEY_BINDING_REQUEST, &[])).unwrap());
        assert_eq!(m.id(), Some(INPUT_KEY_BINDING_RESPONSE));
        assert_eq!(KeyBindingResponse::decode(m.body()).unwrap().status, 0);
    }

    #[test]
    fn touch_becomes_input_report_in_content_pixels() {
        let mut s = running();
        let m = one(&s.touch(TouchKind::Down, 1, 0.5, 0.5, 1_000).unwrap());
        assert_eq!((m.channel, m.encrypted, m.id()), (CH_INPUT, true, Some(INPUT_REPORT)));
        let r = InputReport::decode(m.body()).unwrap();
        assert_eq!(r.timestamp, 1_000);
        let t = r.touch_event.unwrap();
        assert_eq!(t.action, Some(proto::pointer_action::DOWN));
        assert_eq!((t.pointer_data[0].x, t.pointer_data[0].y), (640, 265));
        let evs = s.touch_cancel(2_000).unwrap();
        let t = InputReport::decode(one(&evs).body()).unwrap().touch_event.unwrap();
        assert_eq!(t.action, Some(proto::pointer_action::UP));
    }

    #[test]
    fn touch_before_running_is_dropped() {
        let mut s = session();
        assert!(s.touch(TouchKind::Down, 1, 0.5, 0.5, 0).unwrap().is_empty());
    }

    #[test]
    fn video_focus_request_native_is_reported_and_answered_projected() {
        let mut s = running();
        let req = VideoFocusRequestNotification { disp_channel_id: None, mode: Some(video_focus::NATIVE), reason: Some(2) }.encode_to_vec();
        let evs = s.on_bytes(&phone(CH_VIDEO, false, true, MEDIA_VIDEO_FOCUS_REQUEST, &req)).unwrap();
        assert!(evs.contains(&Event::PhoneWantsNative));
        assert_eq!(VideoFocusNotification::decode(one(&evs).body()).unwrap().focus, Some(video_focus::PROJECTED));
    }

    #[test]
    fn unsolicited_focus_toggle() {
        let mut s = running();
        let m = one(&s.set_video_focus(false).unwrap());
        let n = VideoFocusNotification::decode(m.body()).unwrap();
        assert_eq!((n.focus, n.unsolicited), (Some(video_focus::NATIVE), Some(true)));
    }

    #[test]
    fn byebye_from_phone() {
        let mut s = running();
        let evs = s.on_bytes(&phone(0, false, true, MSG_BYEBYE_REQUEST, &[0x08, 0x01])).unwrap();
        assert_eq!(one(&evs).id(), Some(MSG_BYEBYE_RESPONSE));
        assert!(matches!(evs.last(), Some(Event::Closed { .. })));
        assert_eq!(s.phase(), Phase::Closed);
    }

    #[test]
    fn several_messages_in_one_read() {
        let mut s = running();
        let mut bytes = phone(0, false, false, MSG_PING_REQUEST, &PingRequest { timestamp: 1, bug_report: None, data: None }.encode_to_vec());
        bytes.extend(phone(0, false, false, MSG_PING_REQUEST, &PingRequest { timestamp: 2, bug_report: None, data: None }.encode_to_vec()));
        let w = written(&s.on_bytes(&bytes).unwrap());
        assert_eq!(w.len(), 2);
        assert_eq!(PingResponse::decode(w[1].body()).unwrap().timestamp, 2);
    }
}
