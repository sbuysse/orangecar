//! iAP2 control session — **Device side**.
//!
//! Derived from usenocturne/iap2-rs (`src/session/control.rs` + `src/auth.rs`,
//! GPL-3). See `mod.rs` for the full licensing note.
//!
//! # Direction inversion vs iap2-rs
//!
//! iap2-rs is the *accessory*: it **receives** `StartIdentification`, **sends**
//! `IdentificationInformation`, and in auth **sends** the certificate / response.
//! We are the *Device* (we impersonate the iPhone), so every arrow flips — this
//! matches the Apple AIS ch.12/13 sequence (verified against the reference PDF):
//!
//! ```text
//!   Identification (§13):    Device --StartIdentification(0x1D00)-->        Accessory
//!                            Device <--IdentificationInformation(0x1D01)--  Accessory
//!                            Device --IdentificationAccepted(0x1D02)-->     Accessory
//!                            (or IdentificationRejected 0x1D03 to retry)
//!
//!   Authentication (§12):    Device --RequestAuthenticationCertificate(0xAA00)-->        Accessory
//!                            Device <--AuthenticationCertificate(0xAA01)--               Accessory
//!                            Device --RequestAuthenticationChallengeResponse(0xAA02)-->  Accessory  (32-byte challenge)
//!                            Device <--AuthenticationResponse(0xAA03)--                  Accessory
//!                            Device --AuthenticationSucceeded(0xAA05)-->                 Accessory
//! ```
//!
//! The **control message framing** is confirmed against iap2-rs: a `40 40`
//! prefix, a `u16` total length (counting the prefix and the length field
//! itself), a `u16` message id, then zero or more parameter groups
//! `[u16 param_len][u16 param_id][data]` where `param_len` counts its own 4-byte
//! header.

use bytes::{BufMut, Bytes, BytesMut};
use super::link::Iap2Transport;
use tracing::{debug, info, warn};

use super::error::{Iap2Error, Result};
use super::link::Iap2Link;

// --- Control message ids (verified against iap2-rs source + Apple AIS PDF) ---
pub const MSG_START_IDENTIFICATION: u16 = 0x1D00;
pub const MSG_IDENTIFICATION_INFORMATION: u16 = 0x1D01;
pub const MSG_IDENTIFICATION_ACCEPTED: u16 = 0x1D02;
pub const MSG_IDENTIFICATION_REJECTED: u16 = 0x1D03;

pub const MSG_REQUEST_AUTH_CERTIFICATE: u16 = 0xAA00;
pub const MSG_AUTH_CERTIFICATE: u16 = 0xAA01;
pub const MSG_REQUEST_AUTH_CHALLENGE_RESPONSE: u16 = 0xAA02;
pub const MSG_AUTH_RESPONSE: u16 = 0xAA03;
pub const MSG_AUTH_FAILED: u16 = 0xAA04;
pub const MSG_AUTH_SUCCEEDED: u16 = 0xAA05;

const CONTROL_MSG_MARKER: [u8; 2] = [0x40, 0x40];
const CONTROL_HEADER_LEN: usize = 6; // marker(2) + len(2) + id(2)
const PARAM_HEADER_LEN: usize = 4; // len(2) + id(2)

/// A parsed iAP2 control-session message.
#[derive(Debug, Clone)]
pub struct ControlMessage {
    pub msg_id: u16,
    /// Parameter groups as `(param_id, data)`. `data` excludes the 4-byte header.
    pub params: Vec<(u16, Bytes)>,
}

impl ControlMessage {
    /// First parameter with the given id, if present.
    pub fn param(&self, id: u16) -> Option<&Bytes> {
        self.params.iter().find(|(pid, _)| *pid == id).map(|(_, d)| d)
    }
}

/// Encode a control message: `40 40 <u16 len> <u16 msg_id> [<u16 plen><u16 pid><data>]*`.
pub fn encode_control_message(msg_id: u16, params: &[(u16, &[u8])]) -> Bytes {
    let mut body = BytesMut::new();
    body.put_u16(msg_id);
    for (pid, data) in params {
        body.put_u16((data.len() + PARAM_HEADER_LEN) as u16);
        body.put_u16(*pid);
        body.put_slice(data);
    }

    let mut ctrl = BytesMut::with_capacity(4 + body.len());
    ctrl.put_slice(&CONTROL_MSG_MARKER);
    ctrl.put_u16((4 + body.len()) as u16);
    ctrl.put_slice(&body);
    ctrl.freeze()
}

/// Parse a control message payload (`40 40 …`).
pub fn parse_control_message(payload: &[u8]) -> Result<ControlMessage> {
    if payload.len() < CONTROL_HEADER_LEN {
        return Err(Iap2Error::Protocol(format!(
            "Control message too short: {} bytes",
            payload.len()
        )));
    }
    if payload[0..2] != CONTROL_MSG_MARKER {
        return Err(Iap2Error::Protocol("Missing 0x4040 control marker".to_string()));
    }

    let msg_len = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    if msg_len < CONTROL_HEADER_LEN || msg_len > payload.len() {
        return Err(Iap2Error::Protocol(format!(
            "Bad control message length {} (payload {} bytes)",
            msg_len,
            payload.len()
        )));
    }

    let msg_id = u16::from_be_bytes([payload[4], payload[5]]);

    let mut params = Vec::new();
    let mut i = CONTROL_HEADER_LEN;
    while i + PARAM_HEADER_LEN <= msg_len {
        let param_len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
        let param_id = u16::from_be_bytes([payload[i + 2], payload[i + 3]]);
        if param_len < PARAM_HEADER_LEN || i + param_len > msg_len {
            warn!(
                "Malformed parameter (len={}) in control msg 0x{:04X}, stopping parse",
                param_len, msg_id
            );
            break;
        }
        let data = Bytes::copy_from_slice(&payload[i + PARAM_HEADER_LEN..i + param_len]);
        params.push((param_id, data));
        i += param_len;
    }

    Ok(ControlMessage { msg_id, params })
}

/// Incremental reassembler for one control session's byte stream.
///
/// One link packet does not necessarily carry one whole control message: the
/// `msg_len` declared in the `40 40` header can exceed the payload that arrived,
/// and a large `AuthenticationCertificate` (0xAA01) in particular is expected to
/// span several link packets. Parsing each payload in isolation rejects those as
/// "unparseable" and spins until the authentication timeout, so payloads are
/// appended here and only parsed once `msg_len` bytes are present.
#[derive(Debug, Default)]
pub struct ControlAssembler {
    buf: BytesMut,
}

impl ControlAssembler {
    /// Append one link packet's payload.
    pub fn push(&mut self, payload: &[u8]) {
        self.buf.extend_from_slice(payload);
    }

    /// Bytes buffered but not yet forming a complete message.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Drop everything buffered (used to resync after garbage).
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Pop the next complete control message.
    ///
    /// `Ok(None)` means "not enough bytes yet, feed me another link packet".
    /// `Err(..)` means the buffer does not start on a control message at all and
    /// the caller should [`reset`](Self::reset) — it never means "incomplete".
    pub fn next_message(&mut self) -> Result<Option<ControlMessage>> {
        if self.buf.len() < CONTROL_HEADER_LEN {
            return Ok(None);
        }
        if self.buf[0..2] != CONTROL_MSG_MARKER {
            return Err(Iap2Error::Protocol(
                "Control stream does not start with the 0x4040 marker".to_string(),
            ));
        }
        let msg_len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
        if msg_len < CONTROL_HEADER_LEN {
            return Err(Iap2Error::Protocol(format!(
                "Control message declares an impossible length {msg_len}"
            )));
        }
        if self.buf.len() < msg_len {
            // Genuinely incomplete — wait for the next link packet.
            return Ok(None);
        }
        let frame = self.buf.split_to(msg_len);
        parse_control_message(&frame).map(Some)
    }
}

/// Identification details reported by the accessory (car) in its
/// `IdentificationInformation` (0x1D01) message. Best-effort decode of the
/// common string parameters; all parameter groups are also retained raw.
#[derive(Debug, Clone, Default)]
pub struct AccessoryIdentification {
    pub name: Option<String>,
    pub model_identifier: Option<String>,
    pub manufacturer: Option<String>,
    pub raw_params: Vec<(u16, Bytes)>,
}

/// Parameter id of `MessagesSentByAccessory` inside `IdentificationInformation`.
pub const PARAM_MESSAGES_SENT_BY_ACCESSORY: u16 = 0x0006;
/// Parameter id of `MessagesReceivedFromDevice` inside `IdentificationInformation`.
pub const PARAM_MESSAGES_RECEIVED_FROM_DEVICE: u16 = 0x0007;

impl AccessoryIdentification {
    /// The message ids carried by one of the declaration parameters, or `None`
    /// when the accessory did not declare that list at all.
    ///
    /// Both lists are a flat array of big-endian u16 message ids.
    pub fn declared_ids(&self, param: u16) -> Option<Vec<u16>> {
        let data = self.raw_params.iter().find(|(id, _)| *id == param).map(|(_, d)| d)?;
        Some(
            data.chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect(),
        )
    }

    /// True when the accessory said it can receive `msg_id`.
    ///
    /// iAP2 requires the device to send only messages the accessory declared in
    /// `MessagesReceivedFromDevice`; an undeclared message is the textbook
    /// "protocol violation" a head unit tears the session down on. When no list
    /// was declared we must not block anything, so this answers `true`.
    pub fn accepts(&self, msg_id: u16) -> bool {
        match self.declared_ids(PARAM_MESSAGES_RECEIVED_FROM_DEVICE) {
            Some(ids) if !ids.is_empty() => ids.contains(&msg_id),
            _ => true,
        }
    }

    /// Log both declared message lists once, at `SessionReady`.
    ///
    /// Nothing decoded these before, so every question of the form "is this
    /// message legal to send to THIS car" was inference. This turns it into a
    /// measurement that lands in the run log of the very next attempt.
    pub fn log_declared_messages(&self) {
        for (param, what) in [
            (PARAM_MESSAGES_SENT_BY_ACCESSORY, "MessagesSentByAccessory (car -> us)"),
            (PARAM_MESSAGES_RECEIVED_FROM_DEVICE, "MessagesReceivedFromDevice (us -> car)"),
        ] {
            match self.declared_ids(param) {
                Some(ids) if !ids.is_empty() => {
                    let list: Vec<String> = ids.iter().map(|id| format!("{id:04X}")).collect();
                    info!("iAP2 accessory declares {what}: {}", list.join(" "));
                }
                _ => warn!("iAP2 accessory declared NO {what} (param 0x{param:04X} absent or empty)"),
            }
        }
    }

    fn from_message(msg: &ControlMessage) -> Self {
        let s = |id: u16| -> Option<String> {
            msg.param(id).map(|b| {
                // iAP2 strings are NUL-terminated UTF-8.
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                String::from_utf8_lossy(&b[..end]).into_owned()
            })
        };
        AccessoryIdentification {
            name: s(0x0000),
            model_identifier: s(0x0001),
            manufacturer: s(0x0002),
            raw_params: msg.params.clone(),
        }
    }

    /// A top-level parameter decoded as an iAP2 string (UTF-8, NUL-terminated).
    pub fn string_param(&self, id: u16) -> Option<String> {
        let b = self.raw_params.iter().find(|(pid, _)| *pid == id).map(|(_, d)| d)?;
        Some(nul_string(b))
    }

    /// The sub-parameters of a component (group) parameter, e.g.
    /// `BluetoothTransportComponent` (17). `None` when the car declared no such
    /// component; the first instance wins when it declared several.
    pub fn component(&self, id: u16) -> Option<Vec<(u16, &[u8])>> {
        let data = self.raw_params.iter().find(|(pid, _)| *pid == id).map(|(_, d)| d)?;
        Some(sub_params(data))
    }

    /// The NMEA sentence types the car's `LocationInformationComponent` offers,
    /// in Apple's order (`GPGGA`, `GPRMC`, `GPGSV`, `PASCD`, ...).
    pub fn location_sentences(&self) -> Vec<&'static str> {
        let Some(sub) = self.component(PARAM_LOCATION_INFORMATION_COMPONENT) else {
            return Vec::new();
        };
        LOCATION_SENTENCES
            .iter()
            .filter(|(sub_id, _, _)| sub.iter().any(|(id, _)| id == sub_id))
            .map(|(_, _, name)| *name)
            .collect()
    }
}

/// Decode an iAP2 string (UTF-8, NUL-terminated; a missing NUL is tolerated).
pub fn nul_string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Split a group parameter's data into its `[u16 len][u16 id][data]` sub-parameters.
/// Same framing as top-level parameters; a malformed length stops the walk.
pub fn sub_params(data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + PARAM_HEADER_LEN <= data.len() {
        let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        let id = u16::from_be_bytes([data[i + 2], data[i + 3]]);
        if len < PARAM_HEADER_LEN || i + len > data.len() {
            break;
        }
        out.push((id, &data[i + PARAM_HEADER_LEN..i + len]));
        i += len;
    }
    out
}

/// `IdentificationInformation` component parameters we decode.
pub const PARAM_BLUETOOTH_TRANSPORT_COMPONENT: u16 = 17;
pub const PARAM_VEHICLE_INFORMATION_COMPONENT: u16 = 20;
pub const PARAM_LOCATION_INFORMATION_COMPONENT: u16 = 22;

/// The NMEA sentences of iAP2 location: (sub-id inside the car's
/// `LocationInformationComponent`, parameter id of `StartLocationInformation`
/// 0xFFFA, sentence). Both id columns match the catplay CSM schema
/// (LocationInformationComponent 17-23, StartLocationInformation 1-7) and the
/// Jaguar's own 0x1D01, which offers
/// sub-ids 17, 18, 19 and 20.
pub const LOCATION_SENTENCES: [(u16, u16, &str); 7] = [
    (17, 1, "GPGGA"),
    (18, 2, "GPRMC"),
    (19, 3, "GPGSV"),
    (20, 4, "PASCD"),
    (21, 5, "PAGCD"),
    (22, 6, "PAACD"),
    (23, 7, "GPHDT"),
];

/// Parameters of our `StartLocationInformation` (0xFFFA): one empty flag per
/// sentence type requested.
///
/// GGA (1), RMC (2) and PASCD (4) are what every session has requested since
/// the location stream was first opened, and stay unconditional so the bytes
/// are unchanged when GPGSV is not added. GPGSV (3, per-satellite SNR) is added
/// only when BOTH `gpgsv` is on (`RASCAR_IAP2_GPGSV`, default on) AND the car's
/// `LocationInformationComponent` declares it. An unknown identification (no
/// 0x1D01 decoded) never adds anything.
pub fn start_location_information_params(
    ident: Option<&AccessoryIdentification>,
    gpgsv: bool,
) -> Vec<(u16, Vec<u8>)> {
    let declared = ident.is_some_and(|i| i.location_sentences().contains(&"GPGSV"));
    let mut ids = vec![0x0001u16, 0x0002];
    if gpgsv && declared {
        ids.push(0x0003);
    }
    ids.push(0x0004);
    ids.into_iter().map(|id| (id, Vec::new())).collect()
}

/// Human name of a control message id (only the ids this stack knows).
pub fn msg_name(msg_id: u16) -> &'static str {
    match msg_id {
        MSG_START_IDENTIFICATION => "StartIdentification",
        MSG_IDENTIFICATION_INFORMATION => "IdentificationInformation",
        MSG_IDENTIFICATION_ACCEPTED => "IdentificationAccepted",
        MSG_IDENTIFICATION_REJECTED => "IdentificationRejected",
        MSG_REQUEST_AUTH_CERTIFICATE => "RequestAuthenticationCertificate",
        MSG_AUTH_CERTIFICATE => "AuthenticationCertificate",
        MSG_REQUEST_AUTH_CHALLENGE_RESPONSE => "RequestAuthenticationChallengeResponse",
        MSG_AUTH_RESPONSE => "AuthenticationResponse",
        MSG_AUTH_FAILED => "AuthenticationFailed",
        MSG_AUTH_SUCCEEDED => "AuthenticationSucceeded",
        0x4C00 => "StartMediaLibraryInformation",
        0x4C01 => "MediaLibraryInformation",
        0x4C02 => "StopMediaLibraryInformation",
        0x4C03 => "StartMediaLibraryUpdates",
        0x4C04 => "MediaLibraryUpdate",
        0x4C05 => "StopMediaLibraryUpdates",
        0x4E01 => "BluetoothComponentInformation",
        0x4E03 => "StartBluetoothConnectionUpdates",
        0x4E04 => "BluetoothConnectionUpdate",
        0x4E05 => "StopBluetoothConnectionUpdates",
        0x4E09 => "DeviceInformationUpdate",
        0x4E0A => "DeviceLanguageUpdate",
        0x4E0B => "DeviceTimeUpdate",
        0x4E0C => "DeviceUUIDUpdate",
        0x4E0D => "WirelessCarPlayUpdate",
        0x5000 => "StartNowPlayingUpdates",
        0x5001 => "NowPlayingUpdate",
        // The car declares it can send 0x5002/0xAE02 (the Stop halves of the two
        // subscriptions we answer). They were not even named, so a trace showed
        // them as "(?)" and any later update we sent on that closed subscription
        // would have looked innocent in the log.
        0x5002 => "StopNowPlayingUpdates",
        0x4157 => "StartCommunicationsUpdates",
        0x4158 => "CommunicationsUpdate",
        0x4159 => "StopCommunicationsUpdates",
        0xAE00 => "StartPowerUpdates",
        0xAE01 => "PowerUpdate",
        0xAE02 => "StopPowerUpdates",
        0xAE03 => "PowerSourceUpdate",
        0x4154 => "StartCallStateUpdates",
        0x4155 => "CallStateUpdate",
        0x4170 => "StartListUpdates",
        0x4171 => "ListUpdate",
        0xFFFA => "StartLocationInformation",
        0xFFFC => "StopLocationInformation",
        _ => "?",
    }
}

/// Device→accessory replies the car's control-session subscriptions require.
///
/// Behaviour cross-checked against catplay-labs/catplay's transmitter (used as a
/// reference for the wire format): the head unit sends `Start*` subscription
/// requests and expects the corresponding `*Update` stream back. Leaving
/// `StartNowPlayingUpdates` (0x5000) and `StartPowerUpdates` (0xAE00) unanswered
/// makes the car tear down the USB link a few seconds after enumeration. Returns
/// `(msg_id, params, name)` tuples to send, in order. Param ids and values follow
/// the catplay CSM schema and Apple's iAP2Constants.h.
pub fn subscription_replies(msg_id: u16) -> Vec<(u16, Vec<(u16, Vec<u8>)>, &'static str)> {
    match msg_id {
        // StartPowerUpdates → PowerUpdate ×2: max_current_drawn=1000 (u16), then
        // will_charge=true (bool).
        //
        // 0xAE03 (PowerSourceUpdate) is NOT answered here. This car sends BOTH
        // 0xAE00 and 0xAE03, and answering each with the pair emitted four
        // PowerUpdates where the reference emits two — it flushes the pair once
        // for the subscription and then only reacts to a source change while the
        // subscription is open and not yet flushed. The caller tracks that flush
        // (see `service_link`) and asks for the 0xAE03 reply only if no pair has
        // gone out yet, so a car that leads with 0xAE03 instead of 0xAE00 is
        // still answered exactly once.
        0xAE00 => vec![
            (0xAE01, vec![(0x0000, vec![0x03, 0xE8])], "PowerUpdate(max_current=1000)"),
            (0xAE01, vec![(0x0001, vec![0x01])], "PowerUpdate(will_charge=true)"),
        ],
        // StartNowPlayingUpdates → NowPlayingUpdate(0x5001) carrying a single
        // `playback_attributes` group (csm_id 1). Per the catplay CSM schema
        // `PlaybackAttributes`: playback_status(id0), playback_app_name(id7) and
        // playback_app_bundle_id(id16 = 0x10) are all fields NESTED INSIDE this
        // group — they are NOT top-level params of 0x5001. Inner TLV =
        // [u16 len BE][u16 id BE][value], len = value_len + 4. Getting this
        // nesting wrong makes the car discard the update at the CSM layer and
        // tear the USB link down ~7.7 s after enumeration.
        //
        // The app strings default to EMPTY, which is what the reference
        // implementation sends (both strings are `""` there).
        // `RASCAR_NOWPLAYING_APP`
        // / `RASCAR_NOWPLAYING_BUNDLE` restore concrete values for an A/B at the car.
        0x5000 => {
            let app = std::env::var("RASCAR_NOWPLAYING_APP").unwrap_or_default();
            let bundle = std::env::var("RASCAR_NOWPLAYING_BUNDLE").unwrap_or_default();
            let mut group = inner_tlv(0x0000, &[0x00]); // playback_status = Stopped
            group.extend_from_slice(&inner_tlv(0x0007, &csm_string(&app)));
            group.extend_from_slice(&inner_tlv(0x0010, &csm_string(&bundle)));
            vec![(0x5001, vec![(0x0001, group)], "NowPlayingUpdate(reset+app)")]
        }
        // StartMediaLibraryInformation (0x4C00) → MediaLibraryInformation (0x4C01).
        // Message IDs + parameter schema from Apple's iAP2Constants.h (ATS 6):
        // top-level param 0x0000 is a repeatable *group* whose sub-params are
        // MediaLibraryName(0x0000, utf8), MediaLibraryUniqueIdentifier(0x0001, utf8)
        // and MediaLibraryType(0x0002, u8 enum: LocalDevice=0x00). We advertise one
        // empty local library with unique id "1"; StartMediaLibraryUpdates below
        // must echo the same id. Leaving 0x4C00 unanswered is one of the two
        // subscriptions (with 0x4E03) whose silence made the Jaguar tear the USB
        // link down ~7.7 s after enumeration (real capture, run 006).
        0x4C00 => vec![(
            0x4C01,
            vec![(
                0x0000, // MediaLibraryInformation group (repeatable), nested TLVs:
                vec![
                    // MediaLibraryName (sub id 0x0000) = "iPhone"
                    0x00, 0x0B, 0x00, 0x00, 0x69, 0x50, 0x68, 0x6F, 0x6E, 0x65, 0x00,
                    // MediaLibraryUniqueIdentifier (sub id 0x0001) = "1"
                    0x00, 0x06, 0x00, 0x01, 0x31, 0x00,
                    // MediaLibraryType (sub id 0x0002) = LocalDevice (0x00)
                    0x00, 0x05, 0x00, 0x02, 0x00,
                ],
            )],
            "MediaLibraryInformation",
        )],
        // StartMediaLibraryUpdates (0x4C03) → MediaLibraryUpdate (0x4C04): the same
        // library unique id "1" plus a revision, and no MediaItem groups = an empty
        // library with nothing to sync. (The car only reaches 0x4C03 once 0x4C00 is
        // answered; handled defensively.)
        0x4C03 => vec![(
            0x4C04,
            vec![
                (0x0000, vec![0x31, 0x00]), // MediaLibraryUniqueIdentifier = "1"
                (0x0001, vec![0x31, 0x00]), // MediaLibraryRevision = "1"
            ],
            "MediaLibraryUpdate",
        )],
        // StartBluetoothConnectionUpdates (0x4E03) → BluetoothConnectionUpdate
        // (0x4E04). Apple iAP2Constants.h: param 0x0000 =
        // BluetoothTransportComponentIdentifier (u16), param 0x0001 =
        // ConnectedBluetoothProfiles (group of flags). The car's 0x4E03 carries the
        // component id it wants (u16=1, = the bluetooth_transport_component the car
        // declared in its IdentificationInformation). We are on WIRED USB, so we
        // report component 1 with an EMPTY profile group = no Bluetooth profiles
        // connected. This is the second subscription whose silence tore the link
        // down (real capture, run 006).
        0x4E03 => vec![(
            0x4E04,
            vec![
                (0x0000, vec![0x00, 0x01]), // BluetoothTransportComponentIdentifier = 1 (u16)
                (0x0001, Vec::new()),       // ConnectedBluetoothProfiles = empty (none connected)
            ],
            "BluetoothConnectionUpdate",
        )],
        // StartCallStateUpdates → CallStateUpdate default (status=Disconnected(0), id2).
        0x4154 => vec![(0x4155, vec![(0x0002, vec![0x00])], "CallStateUpdate")],
        // StartListUpdates → ListUpdate: recents_available=true (id0), count=0 (id2, u16).
        0x4170 => vec![(
            0x4171,
            vec![(0x0000, vec![0x01]), (0x0002, vec![0x00, 0x00])],
            "ListUpdate",
        )],
        _ => Vec::new(),
    }
}

/// One CSM inner TLV: `[u16 len BE][u16 id BE][value]`, where `len` counts the
/// whole record (value length + 4).
fn inner_tlv(id: u16, value: &[u8]) -> Vec<u8> {
    let len = (value.len() + 4) as u16;
    let mut out = Vec::with_capacity(value.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// A CSM string payload: UTF-8 followed by a NUL terminator (so `""` is a single
/// `0x00` byte).
fn csm_string(s: &str) -> Vec<u8> {
    let mut out = s.as_bytes().to_vec();
    out.push(0);
    out
}

/// Unix seconds at the CoreFoundation reference date (2001-01-01T00:00:00Z).
/// iAP2 DeviceTimeUpdate carries CFAbsoluteTime, not the Unix epoch.
pub const CF_REFERENCE_EPOCH_UNIX: i64 = 978_307_200;

/// `DeviceLanguageUpdate` (0x4E0A) parameters: `DeviceLanguage` (param 0x0000,
/// UTF-8 + NUL) = "en". A device→accessory notification the car declares it
/// expects (0x0007 want-list); a real iPhone pushes it unsolicited after
/// identification. Verified against Apple iAP2Constants.h.
pub fn device_language_update_params() -> Vec<(u16, Vec<u8>)> {
    vec![(0x0000, b"en\0".to_vec())]
}

/// `DeviceTimeUpdate` (0x4E0B) parameters:
///   0x0000 SecondsSinceEpoch            — unsigned 64-bit BE
///   0x0001 TimeZoneOffsetMinutes        — unsigned 16-bit BE
///   0x0002 DaylightSavingsOffsetMinutes — **ONE** byte
///
/// Widths and epoch follow the catplay CSM schema, which documents the first
/// field as seconds since 1970-01-01 GMT (a Unix timestamp) and types the two
/// offsets as signed (i16 and i8; identical bytes for the zeros we send). We used
/// to emit a 2-byte DST field (a 5-byte TLV where the schema specifies 4) and a
/// CFAbsoluteTime, which a car reading Unix seconds renders as 1995. A head unit
/// that length-checks parameters sees a malformed message — on a message this
/// car explicitly declares it accepts, so it *will* parse it.
///
/// We report UTC, so both offsets are zero.
pub fn device_time_update_params(unix_secs: u64) -> Vec<(u16, Vec<u8>)> {
    vec![
        (0x0000, unix_secs.to_be_bytes().to_vec()), // SecondsSinceEpoch (u64)
        (0x0001, 0u16.to_be_bytes().to_vec()),      // TimeZoneOffsetMinutes (u16)
        (0x0002, vec![0u8]),                        // DaylightSavingsOffsetMinutes (u8)
    ]
}

/// Wire-trace one control message: id, name and every parameter id/size (with a
/// short hex prefix). No-op unless `RASCAR_WIRE_TRACE=1`.
pub fn trace_message(direction: &str, session_id: u8, msg_id: u16, params: &[(u16, &[u8])]) {
    if !crate::debug::wire_trace() {
        return;
    }
    let rendered = if params.is_empty() {
        "none".to_string()
    } else {
        params
            .iter()
            .map(|(id, data)| {
                format!(
                    "0x{id:04X}({}B: {})",
                    data.len(),
                    crate::debug::hex_prefix(data, 16)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    debug!(
        "iap2 ctrl {direction} session=0x{session_id:02X} msg=0x{msg_id:04X} {} params=[{rendered}]",
        msg_name(msg_id)
    );
}

/// [`trace_message`] for a parsed incoming message.
pub fn trace_received(session_id: u8, msg: &ControlMessage) {
    if !crate::debug::wire_trace() {
        return;
    }
    let params: Vec<(u16, &[u8])> = msg.params.iter().map(|(id, d)| (*id, d.as_ref())).collect();
    trace_message("<-", session_id, msg.msg_id, &params);
}

/// Render a message's parameter ids and sizes for diagnostics.
fn describe_params(msg: &ControlMessage) -> String {
    if msg.params.is_empty() {
        return "none".to_string();
    }
    msg.params
        .iter()
        .map(|(id, data)| format!("0x{:04X} ({} bytes)", id, data.len()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Device-side control session: drives identification then authentication over an
/// established [`Iap2Link`].
pub struct ControlSession {
    session_id: u8,
}

impl ControlSession {
    pub fn new(session_id: u8) -> Self {
        ControlSession { session_id }
    }

    pub fn session_id(&self) -> u8 {
        self.session_id
    }

    /// Send one control message on the control session.
    async fn send<T>(&self, link: &mut Iap2Link<T>, msg_id: u16, params: &[(u16, &[u8])]) -> Result<()>
    where
        T: Iap2Transport,
    {
        trace_message("->", self.session_id, msg_id, params);
        let bytes = encode_control_message(msg_id, params);
        link.send_data(self.session_id, bytes).await
    }

    /// Receive control messages until one with `expected` id arrives. Unexpected
    /// control messages are logged and skipped (mirrors iap2-rs's loop). An
    /// `AuthenticationFailed` (0xAA04) while waiting is surfaced as an error.
    ///
    /// Link payloads are fed through a [`ControlAssembler`] so a message split
    /// across several link packets is reassembled instead of being discarded as
    /// unparseable.
    async fn recv_until<T>(&self, link: &mut Iap2Link<T>, expected: u16) -> Result<ControlMessage>
    where
        T: Iap2Transport,
    {
        let mut assembler = ControlAssembler::default();

        loop {
            let packet = link.receive_data().await?;

            // Only our own control session feeds the assembler; mixing another
            // session's bytes in would corrupt a partially-received message.
            if let Some(sid) = packet.session_id {
                if sid != self.session_id {
                    warn!(
                        "Ignoring packet for session 0x{:02X} while waiting on control session 0x{:02X}",
                        sid, self.session_id
                    );
                    continue;
                }
            }

            assembler.push(&packet.payload);

            loop {
                match assembler.next_message() {
                    Ok(Some(msg)) => {
                        trace_received(self.session_id, &msg);
                        if msg.msg_id == expected {
                            return Ok(msg);
                        }
                        if msg.msg_id == MSG_AUTH_FAILED {
                            return Err(Iap2Error::AuthenticationFailed(
                                "Accessory sent AuthenticationFailed".to_string(),
                            ));
                        }
                        warn!(
                            "Waiting for 0x{:04X} ({}), got unexpected control msg 0x{:04X} ({}), skipping",
                            expected,
                            msg_name(expected),
                            msg.msg_id,
                            msg_name(msg.msg_id)
                        );
                    }
                    Ok(None) => {
                        if assembler.pending() > 0 {
                            debug!(
                                "Control message incomplete ({} bytes buffered), awaiting the next link packet",
                                assembler.pending()
                            );
                        }
                        break;
                    }
                    Err(e) => {
                        warn!(
                            "Control stream out of sync ({}) — discarding {} buffered byte(s) and resyncing",
                            e,
                            assembler.pending()
                        );
                        assembler.reset();
                        break;
                    }
                }
            }
        }
    }

    /// §13 Accessory Identification (Device side).
    ///
    /// Sends `StartIdentification`, waits for the accessory's
    /// `IdentificationInformation`, and accepts it with `IdentificationAccepted`.
    /// (Per the PDF the accessory must answer within 1 s; enforcement of that
    /// deadline is left to the driver's wrapping timeout.)
    pub async fn run_identification<T>(
        &self,
        link: &mut Iap2Link<T>,
    ) -> Result<AccessoryIdentification>
    where
        T: Iap2Transport,
    {
        info!("Identification: sending StartIdentification (0x1D00)");
        self.send(link, MSG_START_IDENTIFICATION, &[]).await?;

        let msg = self.recv_until(link, MSG_IDENTIFICATION_INFORMATION).await?;
        let identity = AccessoryIdentification::from_message(&msg);
        info!(
            "Identification: received IdentificationInformation (0x1D01) — name={:?}, manufacturer={:?}, {} params",
            identity.name,
            identity.manufacturer,
            identity.raw_params.len()
        );

        info!("Identification: sending IdentificationAccepted (0x1D02)");
        self.send(link, MSG_IDENTIFICATION_ACCEPTED, &[]).await?;

        Ok(identity)
    }

    /// Reject the accessory's identification (0x1D03) — for the retry path.
    pub async fn reject_identification<T>(&self, link: &mut Iap2Link<T>) -> Result<()>
    where
        T: Iap2Transport,
    {
        warn!("Identification: sending IdentificationRejected (0x1D03)");
        self.send(link, MSG_IDENTIFICATION_REJECTED, &[]).await
    }

    /// §12 Accessory Authentication (Device side), **accept-all**.
    ///
    /// Requests the certificate, sends a 32-byte challenge, parses the response,
    /// and unconditionally declares success. We are the verifier and carry no MFi
    /// coprocessor, so we do not (yet) validate the certificate or the signature.
    ///
    /// Returns the certificate bytes the accessory presented (for logging /
    /// future verification).
    pub async fn run_authentication<T>(
        &self,
        link: &mut Iap2Link<T>,
        challenge: &[u8; 32],
    ) -> Result<Bytes>
    where
        T: Iap2Transport,
    {
        info!("Authentication: sending RequestAuthenticationCertificate (0xAA00)");
        self.send(link, MSG_REQUEST_AUTH_CERTIFICATE, &[]).await?;

        let cert_msg = self.recv_until(link, MSG_AUTH_CERTIFICATE).await?;
        // An absent parameter is a protocol mismatch, not an empty certificate.
        // Substituting `Bytes::new()` here used to carry a broken accessory all
        // the way to "**** SessionReady ****", where the real cause only
        // resurfaced as an unrelated AirPlay failure.
        let certificate = cert_msg.param(0x0000).cloned().ok_or_else(|| {
            Iap2Error::AuthenticationFailed(format!(
                "AuthenticationCertificate (0xAA01) carries no certificate parameter 0x0000 (parameters present: {})",
                describe_params(&cert_msg)
            ))
        })?;
        info!(
            "Authentication: received AuthenticationCertificate (0xAA01), {} bytes",
            certificate.len()
        );
        // TODO: verify cert against Apple MFi root CA (optional hardening; see mod.rs).

        info!("Authentication: sending RequestAuthenticationChallengeResponse (0xAA02) with 32-byte challenge");
        self.send(
            link,
            MSG_REQUEST_AUTH_CHALLENGE_RESPONSE,
            &[(0x0000, challenge.as_slice())],
        )
        .await?;

        let resp_msg = self.recv_until(link, MSG_AUTH_RESPONSE).await?;
        let response = resp_msg.param(0x0000).cloned().ok_or_else(|| {
            Iap2Error::AuthenticationFailed(format!(
                "AuthenticationResponse (0xAA03) carries no challenge-response parameter 0x0000 (parameters present: {})",
                describe_params(&resp_msg)
            ))
        })?;
        info!(
            "Authentication: received AuthenticationResponse (0xAA03), {} bytes (accepting unconditionally)",
            response.len()
        );
        // TODO: verify the ECDSA challenge-response signature (accept-all for now).

        info!("Authentication: sending AuthenticationSucceeded (0xAA05)");
        self.send(link, MSG_AUTH_SUCCEEDED, &[]).await?;

        Ok(certificate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subscription replies must match the reference transmitter's bytes, or
    /// the car rejects them. Expected bytes derived from the CSM serialization
    /// rules (magic → last payload byte).
    #[test]
    fn subscription_reply_bytes_match_reference_layout() {
        fn enc(mid: u16, params: &[(u16, Vec<u8>)]) -> Vec<u8> {
            let b: Vec<(u16, &[u8])> = params.iter().map(|(i, d)| (*i, d.as_slice())).collect();
            encode_control_message(mid, &b).to_vec()
        }
        let power = subscription_replies(0xAE00);
        assert_eq!(enc(power[0].0, &power[0].1), vec![0x40,0x40,0x00,0x0C,0xAE,0x01,0x00,0x06,0x00,0x00,0x03,0xE8]);
        assert_eq!(enc(power[1].0, &power[1].1), vec![0x40,0x40,0x00,0x0B,0xAE,0x01,0x00,0x05,0x00,0x01,0x01]);
        let np = subscription_replies(0x5000);
        // Single playback_attributes group (id1) with playback_status(id0),
        // playback_app_name(id7) and playback_app_bundle_id(id16) NESTED inside it
        // (CSM PlaybackAttributes field layout). Both app fields are
        // REQUIRED but EMPTY — a CsmString payload is UTF-8 + NUL, so "" is a
        // single 0x00 and each field is a 5-byte TLV. That is what the reference
        // transmitter sends. Group len = 4 + 5 + 5 + 5 = 0x13.
        assert_eq!(
            enc(np[0].0, &np[0].1),
            vec![
                0x40, 0x40, 0x00, 0x19, 0x50, 0x01, // magic, len=25, msgid 0x5001
                0x00, 0x13, 0x00, 0x01, // playback_attributes group: len=19, id=1
                0x00, 0x05, 0x00, 0x00, 0x00, // playback_status = Stopped (id0)
                0x00, 0x05, 0x00, 0x07, 0x00, // playback_app_name = "" (id7)
                0x00, 0x05, 0x00, 0x10, 0x00, // playback_app_bundle_id = "" (id16)
            ]
        );
        // 0xAE03 (PowerSourceUpdate) has NO reply of its own: the PowerUpdate pair
        // is flushed exactly once per session by the service loop, which maps a
        // leading 0xAE03 onto the 0xAE00 reply itself. Answering both here emitted
        // four PowerUpdates where a phone sends two.
        assert!(subscription_replies(0xAE03).is_empty());
        let call = subscription_replies(0x4154);
        assert_eq!(enc(call[0].0, &call[0].1), vec![0x40,0x40,0x00,0x0B,0x41,0x55,0x00,0x05,0x00,0x02,0x00]);
        let list = subscription_replies(0x4170);
        assert_eq!(enc(list[0].0, &list[0].1), vec![0x40,0x40,0x00,0x11,0x41,0x71,0x00,0x05,0x00,0x00,0x01,0x00,0x06,0x00,0x02,0x00,0x00]);
        assert!(subscription_replies(0x4E01).is_empty());
    }

    /// MediaLibrary (0x4C00/0x4C03) and Bluetooth-connection (0x4E03) subscription
    /// replies, plus the unsolicited DeviceLanguage/DeviceTime notifications.
    /// Message IDs and parameter schemas are from Apple's iAP2Constants.h; byte
    /// framing follows the CSM TLV rules (len counts its own 4-byte header;
    /// CsmString = UTF-8 + NUL; groups nest the same TLVs).
    #[test]
    fn media_library_and_device_notification_reply_bytes_match_apple_schema() {
        fn enc(mid: u16, params: &[(u16, Vec<u8>)]) -> Vec<u8> {
            let b: Vec<(u16, &[u8])> = params.iter().map(|(i, d)| (*i, d.as_slice())).collect();
            encode_control_message(mid, &b).to_vec()
        }

        // StartMediaLibraryInformation (0x4C00) → MediaLibraryInformation (0x4C01):
        // one MediaLibraryInformation group (id 0x0000) nesting name/uid/type.
        let mli = subscription_replies(0x4C00);
        assert_eq!(mli.len(), 1);
        assert_eq!(mli[0].0, 0x4C01);
        assert_eq!(
            enc(mli[0].0, &mli[0].1),
            vec![
                0x40, 0x40, 0x00, 0x20, 0x4C, 0x01, // magic, len=32, msgid 0x4C01
                0x00, 0x1A, 0x00, 0x00, // MediaLibraryInformation group: len=26, id=0
                0x00, 0x0B, 0x00, 0x00, 0x69, 0x50, 0x68, 0x6F, 0x6E, 0x65, 0x00, // Name = "iPhone"
                0x00, 0x06, 0x00, 0x01, 0x31, 0x00, // UniqueIdentifier = "1"
                0x00, 0x05, 0x00, 0x02, 0x00, // Type = LocalDevice (0x00)
            ]
        );

        // StartMediaLibraryUpdates (0x4C03) → MediaLibraryUpdate (0x4C04): uid + revision.
        let mlu = subscription_replies(0x4C03);
        assert_eq!(mlu.len(), 1);
        assert_eq!(
            enc(mlu[0].0, &mlu[0].1),
            vec![
                0x40, 0x40, 0x00, 0x12, 0x4C, 0x04, // magic, len=18, msgid 0x4C04
                0x00, 0x06, 0x00, 0x00, 0x31, 0x00, // MediaLibraryUniqueIdentifier = "1"
                0x00, 0x06, 0x00, 0x01, 0x31, 0x00, // MediaLibraryRevision = "1"
            ]
        );

        // StartBluetoothConnectionUpdates (0x4E03) → BluetoothConnectionUpdate (0x4E04):
        // component id 1 (u16) + empty ConnectedBluetoothProfiles group.
        let bcu = subscription_replies(0x4E03);
        assert_eq!(bcu.len(), 1);
        assert_eq!(
            enc(bcu[0].0, &bcu[0].1),
            vec![
                0x40, 0x40, 0x00, 0x10, 0x4E, 0x04, // magic, len=16, msgid 0x4E04
                0x00, 0x06, 0x00, 0x00, 0x00, 0x01, // BluetoothTransportComponentIdentifier = 1
                0x00, 0x04, 0x00, 0x01, // ConnectedBluetoothProfiles = empty group
            ]
        );

        // DeviceLanguageUpdate (0x4E0A): device_language = "en".
        assert_eq!(
            enc(0x4E0A, &device_language_update_params()),
            vec![0x40, 0x40, 0x00, 0x0D, 0x4E, 0x0A, 0x00, 0x07, 0x00, 0x00, 0x65, 0x6E, 0x00]
        );

        // DeviceTimeUpdate (0x4E0B): SecondsSinceEpoch (u64) + TimeZoneOffsetMinutes
        // (u16) + DaylightSavingsOffsetMinutes (**one** byte). The DST field used to
        // be emitted as 2 bytes, making a 6-byte TLV where the schema specifies 5 —
        // a malformed parameter on a message this car declares it accepts.
        assert_eq!(
            enc(0x4E0B, &device_time_update_params(12345)),
            vec![
                0x40, 0x40, 0x00, 0x1D, 0x4E, 0x0B, // magic, len=29, msgid 0x4E0B
                0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x39, // secs = 12345
                0x00, 0x06, 0x00, 0x01, 0x00, 0x00, // TimeZoneOffsetMinutes = 0
                0x00, 0x05, 0x00, 0x02, 0x00, // DaylightSavingsOffsetMinutes = 0 (u8)
            ]
        );

        // Stop*/notification messages the car sends but wants no reply to.
        for pure_ack in [0x4C02, 0x4C05, 0x4E01, 0x4E05] {
            assert!(
                subscription_replies(pure_ack).is_empty(),
                "0x{pure_ack:04X} must not produce a reply"
            );
        }
    }

    #[test]
    fn encode_then_parse_roundtrip() {
        let bytes = encode_control_message(
            MSG_REQUEST_AUTH_CHALLENGE_RESPONSE,
            &[(0x0000, &[0xAB; 32])],
        );
        // 40 40 | len | id | [plen pid data]
        assert_eq!(&bytes[0..2], &[0x40, 0x40]);
        let msg = parse_control_message(&bytes).unwrap();
        assert_eq!(msg.msg_id, MSG_REQUEST_AUTH_CHALLENGE_RESPONSE);
        assert_eq!(msg.param(0x0000).unwrap().len(), 32);
    }

    #[test]
    fn parse_identification_information_strings() {
        // Build an IdentificationInformation with name/model/manufacturer params.
        let msg_bytes = encode_control_message(
            MSG_IDENTIFICATION_INFORMATION,
            &[
                (0x0000, b"Test Car\0"),
                (0x0001, b"HU-1\0"),
                (0x0002, b"Acme\0"),
            ],
        );
        let msg = parse_control_message(&msg_bytes).unwrap();
        let id = AccessoryIdentification::from_message(&msg);
        assert_eq!(id.name.as_deref(), Some("Test Car"));
        assert_eq!(id.model_identifier.as_deref(), Some("HU-1"));
        assert_eq!(id.manufacturer.as_deref(), Some("Acme"));
        assert_eq!(id.raw_params.len(), 3);
    }

    #[test]
    fn parse_rejects_missing_marker() {
        assert!(parse_control_message(&[0x00, 0x00, 0x00, 0x06, 0x1D, 0x00]).is_err());
    }

    // A control message larger than one link packet must be reassembled, not
    // dropped as "unparseable" (which used to spin until the auth timeout).
    #[test]
    fn assembler_reassembles_message_split_across_link_packets() {
        let msg = encode_control_message(MSG_AUTH_CERTIFICATE, &[(0x0000, &[0x7Eu8; 900])]);
        let (first, second) = msg.split_at(64);

        let mut asm = ControlAssembler::default();
        asm.push(first);
        assert!(
            asm.next_message().unwrap().is_none(),
            "a partial message must report 'need more bytes', not an error"
        );
        assert_eq!(asm.pending(), first.len());

        asm.push(second);
        let parsed = asm.next_message().unwrap().expect("message completes");
        assert_eq!(parsed.msg_id, MSG_AUTH_CERTIFICATE);
        assert_eq!(parsed.param(0x0000).unwrap().len(), 900);
        assert_eq!(asm.pending(), 0);
    }

    #[test]
    fn assembler_pops_two_messages_from_one_payload() {
        let mut joined = Vec::new();
        joined.extend_from_slice(&encode_control_message(MSG_START_IDENTIFICATION, &[]));
        joined.extend_from_slice(&encode_control_message(MSG_IDENTIFICATION_ACCEPTED, &[]));

        let mut asm = ControlAssembler::default();
        asm.push(&joined);
        assert_eq!(
            asm.next_message().unwrap().unwrap().msg_id,
            MSG_START_IDENTIFICATION
        );
        assert_eq!(
            asm.next_message().unwrap().unwrap().msg_id,
            MSG_IDENTIFICATION_ACCEPTED
        );
        assert!(asm.next_message().unwrap().is_none());
    }

    #[test]
    fn assembler_errors_on_garbage_so_the_caller_can_resync() {
        let mut asm = ControlAssembler::default();
        asm.push(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11]);
        assert!(
            asm.next_message().is_err(),
            "a stream that does not start on 0x4040 must be reported, not buffered forever"
        );
    }
}

/// `DeviceTransportIdentifierNotification` (0x4E0E) parameters.
///
/// param 0x0000 `BluetoothTransportIdentifier` — MAC as a NUL-terminated string.
/// param 0x0001 `USBTransportIdentifier`       — NUL-terminated string.
///
/// Layout from a shipping head unit's own log (HaToan/carplay-wifi-extractor
/// `iap2.log`): `bluetoothTransportIdentifier=94:89:78:xx:xx:xx,
/// usbTransportIdentifier=0000815000190xxxxxxxxxxx` — i.e. a colon MAC and a
/// 24-hex-character USB identifier. We reuse our advertised device MAC and derive
/// the USB identifier from the gadget serial so the two stay consistent with what
/// the car already saw at enumeration.
pub fn device_transport_identifier_params() -> Vec<(u16, Vec<u8>)> {
    let bt = format!("{}\0", crate::DEVICE_MAC);
    let usb = format!("{}\0", usb_transport_identifier());
    vec![(0x0000, bt.into_bytes()), (0x0001, usb.into_bytes())]
}

/// 24 uppercase hex characters, matching the shape a real iPhone reports.
/// Derived from the MAC we advertise so a capture can be tied back to a run.
fn usb_transport_identifier() -> String {
    let hex: String = crate::DEVICE_MAC.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let mut s = hex.to_ascii_uppercase();
    while s.len() < 24 {
        s.push('0');
    }
    s.truncate(24);
    s
}

#[cfg(test)]
mod transport_identifier_tests {
    use super::*;

    #[test]
    fn usb_identifier_is_24_hex_chars() {
        let id = usb_transport_identifier();
        assert_eq!(id.len(), 24, "production trace shows a 24-character identifier");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "must be hex: {id}");
        assert_eq!(id, id.to_ascii_uppercase());
    }

    #[test]
    fn params_are_two_nul_terminated_strings_in_apple_order() {
        let p = device_transport_identifier_params();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].0, 0x0000, "param 0 is the Bluetooth transport identifier");
        assert_eq!(p[1].0, 0x0001, "param 1 is the USB transport identifier");
        for (id, v) in &p {
            assert_eq!(*v.last().unwrap(), 0, "param 0x{id:04X} must be NUL-terminated");
        }
    }
}

/// The car's `LocationInformation` (0xFFFB) carries ONE NMEA sentence in
/// param 0x0000, ASCII, newline-terminated.
///
/// Captured from the Jaguar (boot-20260921-191924 run 019) once we stopped
/// cancelling the location subscription:
///
///     $GPGGA,120000.00,4851.402000,N,00221.102000,E,6,17,0.8,35.0,M,44.8,M,,*54
///     $GPRMC,120000.00,A,4851.402000,N,00221.102000,E,21.208,218.18,210926,,,E*50
///     $PASCD,43200.000,C,D,1,1,0.00,10.556*42
///
/// `$PASCD` is Apple's proprietary speed/distance sentence; standard NMEA parsers
/// ignore it, which is correct — we pass everything through untouched.
///
/// This is the vehicle's own GPS, which is better than a separate receiver: it is
/// already fused with wheel speed, so it keeps working in tunnels and car parks.
pub fn location_nmea(msg: &ControlMessage) -> Option<&str> {
    let raw = msg.params.iter().find(|(id, _)| *id == 0x0000).map(|(_, v)| &v[..])?;
    let text = std::str::from_utf8(raw).ok()?.trim_end_matches(['\0', '\n', '\r']);
    text.starts_with('$').then_some(text)
}

/// Verify an NMEA `*HH` checksum: XOR of everything between `$` and `*`.
///
/// Worth doing because these sentences arrive over a link we are simultaneously
/// debugging; a corrupt fix silently poisoning the dashboard would be worse than
/// a dropped one.
pub fn nmea_checksum_ok(sentence: &str) -> bool {
    let body = match sentence.strip_prefix('$') {
        Some(b) => b,
        None => return false,
    };
    let (payload, want) = match body.rsplit_once('*') {
        Some((p, c)) => (p, c),
        None => return false,
    };
    match u8::from_str_radix(want.trim(), 16) {
        Ok(want) => payload.bytes().fold(0u8, |a, b| a ^ b) == want,
        Err(_) => false,
    }
}

#[cfg(test)]
mod location_information_tests {
    use super::*;

    fn msg(params: Vec<(u16, Vec<u8>)>) -> ControlMessage {
        ControlMessage {
            msg_id: 0xFFFB,
            params: params.into_iter().map(|(i, v)| (i, bytes::Bytes::from(v))).collect(),
        }
    }

    /// Sentences captured from the car, position moved to a neutral place.
    const GPGGA: &str =
        "$GPGGA,120000.00,4851.402000,N,00221.102000,E,6,17,0.8,35.0,M,44.8,M,,*54";
    const GPRMC: &str =
        "$GPRMC,120000.00,A,4851.402000,N,00221.102000,E,21.208,218.18,210926,,,E*50";
    const PASCD: &str = "$PASCD,43200.000,C,D,1,1,0.00,10.556*42";

    #[test]
    fn extracts_the_sentence_and_strips_the_trailing_newline() {
        for s in [GPGGA, GPRMC, PASCD] {
            let m = msg(vec![(0x0000, format!("{s}\n").into_bytes())]);
            assert_eq!(location_nmea(&m), Some(s));
        }
    }

    #[test]
    fn real_car_sentences_pass_their_checksum() {
        for s in [GPGGA, GPRMC, PASCD] {
            assert!(nmea_checksum_ok(s), "captured sentence failed its checksum: {s}");
        }
    }

    #[test]
    fn a_corrupted_sentence_is_rejected() {
        // Flip one digit of the latitude; the stored checksum no longer matches.
        let bad = GPRMC.replace("4851.402000", "4851.402001");
        assert!(!nmea_checksum_ok(&bad), "corruption must not pass as a valid fix");
    }

    #[test]
    fn non_nmea_and_missing_params_are_ignored() {
        assert_eq!(location_nmea(&msg(vec![(0x0000, b"not nmea\n".to_vec())])), None);
        assert_eq!(location_nmea(&msg(vec![(0x0001, b"$GPGGA,1*00\n".to_vec())])), None);
        assert_eq!(location_nmea(&msg(vec![])), None);
    }
}

/// What to do about `WirelessCarPlayUpdate` (0x4E0D) in the post-auth burst.
///
/// Apple's spec (`spec.rs`, generated from Apple's own message archive) has it as
/// `Source::Device` with 1 parameter; param 0x0000 is a uint8 status,
/// 0 = Unavailable, 1 = Available.
///
/// Two independent sources show a device sending it right after authentication:
/// a production head-unit logcat ("Received iAP2 WirelessCarPlayUpdate - status
/// AVAILABLE") and the CarlinKit T-Box Plus firmware, whose device-side message
/// table in `libSdCarplay.so` lists 0x4E0D alongside 0x4E09/0x4E0A/0x4E0B/0x4E0C/
/// 0x4E0E. We send everything in that list except this one.
///
/// DEFAULT OFF. We are a WIRED-only device with no wireless CarPlay to offer, so
/// "Available" would be a lie and "Unavailable" is information the car did not ask
/// for — it lists neither 0x4E0D nor 0x4E0E in its MessagesReceivedFromDevice.
/// Undeclared messages are tolerated (a production head unit logs "Unrecognized
/// iAP2 message received. Type: 0X4E0A" and carries on), but on this car small
/// changes have moved results unpredictably, so this ships as an A/B rather than
/// a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WirelessCarPlay {
    /// Do not send 0x4E0D at all.
    Omit,
    /// Send 0x4E0D with status = 0 (Unavailable) — truthful for a wired-only box.
    Unavailable,
    /// Send 0x4E0D with status = 1 (Available) — what the production iPhone sent.
    Available,
}

impl WirelessCarPlay {
    /// Read `RASCAR_IAP2_WIRELESS_CARPLAY`. Unset or unrecognised = `Omit`.
    pub fn from_env() -> Self {
        std::env::var("RASCAR_IAP2_WIRELESS_CARPLAY")
            .map(|v| Self::parse(&v))
            .unwrap_or(Self::Omit)
    }

    /// Pure parser, so the behaviour can be tested without mutating process
    /// environment (unsafe in Rust 2024, and racy across parallel tests).
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "available" | "on" | "1" | "true" | "yes" => Self::Available,
            "unavailable" | "0" => Self::Unavailable,
            _ => Self::Omit,
        }
    }

    /// The 0x4E0D parameters, or `None` when this message must not be sent.
    pub fn params(self) -> Option<Vec<(u16, Vec<u8>)>> {
        let status: u8 = match self {
            Self::Omit => return None,
            Self::Unavailable => 0,
            Self::Available => 1,
        };
        Some(vec![(0x0000, vec![status])])
    }
}

#[cfg(test)]
mod wireless_carplay_tests {
    use super::*;

    #[test]
    fn omit_sends_nothing() {
        assert_eq!(WirelessCarPlay::Omit.params(), None);
    }

    #[test]
    fn status_is_a_single_uint8_in_param_zero() {
        // Apple: 0x4E0D has exactly 1 parameter; 0 = Unavailable, 1 = Available.
        assert_eq!(WirelessCarPlay::Unavailable.params(), Some(vec![(0x0000, vec![0u8])]));
        assert_eq!(WirelessCarPlay::Available.params(), Some(vec![(0x0000, vec![1u8])]));
    }

    #[test]
    fn junk_means_omit() {
        // The default must be "do not send" — an A/B knob that silently turns
        // itself on because of a typo is worse than no knob. Note "off" and "no"
        // land here too: they mean don't send, which is exactly Omit.
        for v in ["", "maybe", "off", "please", "no", "   "] {
            assert_eq!(WirelessCarPlay::parse(v), WirelessCarPlay::Omit, "value {v:?}");
        }
    }

    #[test]
    fn explicit_values_select_the_right_status() {
        for (v, want) in [
            ("available", WirelessCarPlay::Available),
            ("AVAILABLE", WirelessCarPlay::Available),
            (" on ", WirelessCarPlay::Available),
            ("1", WirelessCarPlay::Available),
            ("unavailable", WirelessCarPlay::Unavailable),
            ("0", WirelessCarPlay::Unavailable),
        ] {
            assert_eq!(WirelessCarPlay::parse(v), want, "value {v:?}");
        }
    }

    #[test]
    fn omit_and_unavailable_are_not_the_same_thing() {
        // "do not send the message" vs "send it saying wireless is unavailable"
        // are different on the wire, and the A/B depends on telling them apart.
        assert_eq!(WirelessCarPlay::parse("off").params(), None);
        assert_eq!(WirelessCarPlay::parse("0").params(), Some(vec![(0x0000, vec![0u8])]));
    }
}

/// The Jaguar F-PACE's real `IdentificationInformation`, shared by the tests
/// that decode it.
#[cfg(test)]
pub(crate) mod jaguar_fixture {
    use super::*;

    /// Control message (`40 40 ... 1D 01 ...`) of the Jaguar F-PACE's 0x1D01,
    /// byte for byte from a real capture ("Received raw packet", link header and
    /// checksum stripped). The head unit's serial number and the device part of
    /// its Bluetooth address have been replaced by placeholders of the same
    /// length, so the TLV framing is unchanged.
    pub const IDENTIFICATION_HEX: &str = "404001631d01001200004a616775617220462d5041434500001200014a616775617220462d5041434500000b00024a616775617200001b0003585858582d3030303030302d58585f303030303030300000180004533232415f32322e33322e342d39323835303500000c00054978432e312e3000002600064c004c024c034c0550005002fffb41574159415441564e014e034e05ae00ae02ae03001c00074c014c045001fffafffc415841554e094e0a4e0b4e04ae0100050008020006000900020007000c656e000007000d656e000020001000060000001600090001694150320000040002000500030100040004001f0011000600000001000b00014a616775617200000a00030024d600000100330014000600000077001200014a616775617220462d50414345000005000200001200064a616775617220462d504143450000220016000600000089000800014750530000040011000400120004001300040014";

    pub fn identification() -> AccessoryIdentification {
        let bytes = hex::decode(IDENTIFICATION_HEX).unwrap();
        let msg = parse_control_message(&bytes).unwrap();
        assert_eq!(msg.msg_id, MSG_IDENTIFICATION_INFORMATION);
        AccessoryIdentification::from_message(&msg)
    }
}

#[cfg(test)]
mod start_location_tests {
    use super::*;

    fn wire(params: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let borrowed: Vec<(u16, &[u8])> = params.iter().map(|(id, d)| (*id, d.as_slice())).collect();
        encode_control_message(0xFFFA, &borrowed).to_vec()
    }

    /// What every session sent before GPGSV: GGA, RMC, PASCD.
    const BEFORE: [u8; 18] = [
        0x40, 0x40, 0x00, 0x12, 0xFF, 0xFA,
        0x00, 0x04, 0x00, 0x01, 0x00, 0x04, 0x00, 0x02, 0x00, 0x04, 0x00, 0x04,
    ];

    #[test]
    fn the_real_car_offers_gga_rmc_gsv_and_pascd() {
        let ident = jaguar_fixture::identification();
        assert_eq!(ident.location_sentences(), vec!["GPGGA", "GPRMC", "GPGSV", "PASCD"]);
    }

    #[test]
    fn gpgsv_is_requested_when_the_car_declares_it() {
        let ident = jaguar_fixture::identification();
        let bytes = wire(&start_location_information_params(Some(&ident), true));
        assert_eq!(
            bytes,
            [
                0x40, 0x40, 0x00, 0x16, 0xFF, 0xFA,
                0x00, 0x04, 0x00, 0x01, 0x00, 0x04, 0x00, 0x02,
                0x00, 0x04, 0x00, 0x03, 0x00, 0x04, 0x00, 0x04,
            ]
        );
    }

    #[test]
    fn flag_off_restores_the_previous_bytes_exactly() {
        let ident = jaguar_fixture::identification();
        assert_eq!(wire(&start_location_information_params(Some(&ident), false)), BEFORE);
    }

    #[test]
    fn gpgsv_is_never_requested_from_a_car_that_does_not_offer_it() {
        // Same car minus sub-id 19 (GPGSV) in its LocationInformationComponent.
        let mut ident = jaguar_fixture::identification();
        let loc = encode_sub_params(&[(0, &[0x00, 0x89]), (1, b"GPS\0"), (17, &[]), (18, &[]), (20, &[])]);
        for (id, data) in ident.raw_params.iter_mut() {
            if *id == PARAM_LOCATION_INFORMATION_COMPONENT {
                *data = Bytes::from(loc.clone());
            }
        }
        assert_eq!(ident.location_sentences(), vec!["GPGGA", "GPRMC", "PASCD"]);
        assert_eq!(wire(&start_location_information_params(Some(&ident), true)), BEFORE);
        // No identification at all: nothing is added either.
        assert_eq!(wire(&start_location_information_params(None, true)), BEFORE);
    }

    fn encode_sub_params(params: &[(u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (id, d) in params {
            out.extend_from_slice(&((d.len() + 4) as u16).to_be_bytes());
            out.extend_from_slice(&id.to_be_bytes());
            out.extend_from_slice(d);
        }
        out
    }
}
