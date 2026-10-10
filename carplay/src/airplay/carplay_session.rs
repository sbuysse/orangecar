//! Phase 4/5 — **wired CarPlay screen-streaming session (legacy path).**
//!
//! This is the CarPlay counterpart to the AppleTV/FairPlay [`super::run`] path.
//! It targets a legacy head unit (verified against a Jaguar F-PACE) that does
//! **no pairing, no MFi, no FairPlay**: plaintext RTSP plus AES-128-CTR screen
//! encryption with an all-zero key. After the iAP2/CDC-NCM layers are up and the
//! car's AirPlay receiver has been discovered, this module:
//!
//! ```text
//!   RtspClient::connect(endpoint)  (UA = AirPlay/566.25.21, no pairing headers)
//!     bind timingPort + spawn RTCP 0xD2→0xD3 time-sync responder
//!     → SETUP /setup   (InitialSetup plist)            ← eventPort, keepAlivePort
//!     → GET /info      (InfoMessage plist)             ← display W×H + uuid + hidDevices
//!     open TCP car:eventPort, serve `POST /command` (requestUI/changeModes/
//!       forceKeyFrame/hidSendReport), wait for `requestUI` (or 3 s)
//!     → RECORD /
//!     → POST /command  (modesChanged, full body)       enforce 2xx
//!     → SETUP /        (per-stream, type 110)          ← dataPort
//!     open TCP car:dataPort  →  screen data channel
//!     [optional] → SETUP / (audio type 100 PCM 48k)    ← audio dataPort → RTP/UDP
//!     → VideoConfig (avcC, plaintext)  → keyframe VideoFrame → …  (AES-128-CTR,
//!       key derived with the ADVERTISED streamConnectionID)
//!     keepAlivePort UDP ping /1 s · POST /feedback /1 s · KeepAlive frame /1 s
//!     on exit: close data socket → TEARDOWN /
//! ```
//!
//! # Wire format (VERIFIED from catplay-labs/catplay `screen/screen_frame.rs`)
//!
//! The screen data channel carries a fixed **128-byte little-endian header** per
//! frame, followed by an optional payload:
//!
//! ```text
//!   [0..4]    body_size : u32   (payload length; AES-CTR adds no tag)
//!   [4]       opcode    : u8    (VideoFrame=0, VideoConfig=1, KeepAlive=2)
//!   [5..8]    small_param : [u8; 3]
//!   [8..128]  params      : 15 × u64  (little-endian)
//! ```
//!
//! # Reuse
//!
//! * ffmpeg `x11grab`→H.264-baseline capture and the Annex-B NAL splitter come
//!   from [`crate::stream::video`] ([`VideoEncoder`], [`NalParser`]).
//! * the screen cipher is [`crate::stream::crypto::VideoCipher`]: ChaCha20-
//!   Poly1305 (DataStream) for a HAP-paired session, else the all-zero-key
//!   AES-128-CTR cipher with cross-frame keystream continuity.
//! * the RTSP/1.0 client and binary-plist helpers are [`crate::airplay::rtsp`].
//! * the dashboard render prerequisite is [`crate::stream::render`].
//!
//! Note the catplay screen packet format differs from the doubletake AirPlay
//! mirror packet in [`crate::stream::video`]; the packetizer here is new.

use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, info, warn};

use crate::airplay::hid;
use crate::airplay::pairing::{hkdf_sha512, Pairing};
use crate::airplay::rtsp::{bplist, hap_frame_nonce, seal_control_frame, RtspClient};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use crate::host::AirPlayEndpoint;
use crate::stream::audio::AudioCapture;
use crate::stream::crypto::VideoCipher;
use crate::stream::input::{map_coords, InputInjector, TouchAction};
use crate::stream::render;
use crate::stream::video::{is_first_slice, nal_type, NalParser, VideoEncoder, VideoSettings};

/// Shared RTSP control client: the handshake owns it directly, then it is wrapped
/// so the `/feedback` keep-alive, the event responder's triggers, and the final
/// TEARDOWN can all use the one control connection.
type SharedRtsp = Arc<Mutex<RtspClient>>;

/// iOS AirPlay build string the legacy CarPlay path impersonates.
const USER_AGENT: &str = "AirPlay/566.25.21";
/// Fixed CarPlay HomeKit pairing PIN (catplay `CARPLAY_MAGIC_PIN`). Used only on
/// the pairing fallback when a head unit rejects the plaintext et=NONE SETUP.
const CARPLAY_PIN: &str = "3939";
/// Apple binary-plist MIME type used for every plist body.
const PLIST_CT: &str = "application/x-apple-binary-plist";

/// Where the persistent HomeKit controller identity + learned accessory key live,
/// so we pair-setup once and pair-verify on reconnect (avoids the "Connexion
/// impossible" accretion). Override with `RASCAR_HAP_STORE`.
fn hap_store_path() -> std::path::PathBuf {
    std::env::var_os("RASCAR_HAP_STORE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/rascar/hap_identity"))
}
/// Fixed screen-frame header length.
const HEADER_LEN: usize = 128;
/// Screen data-channel opcodes.
const OP_VIDEO_FRAME: u8 = 0;
const OP_VIDEO_CONFIG: u8 = 1;
const OP_KEEPALIVE: u8 = 2;
/// KeepAlive carrying a statistics body (see [`keepalive_stats_header`]).
const OP_KEEPALIVE_WITH_BODY: u8 = 5;
/// `small_param[1]` bit that marks the session's video as encrypted (catplay
/// sets it on the VideoConfig frame; individual VideoFrame headers do not carry
/// it even though their payload is encrypted).
const SMALL_PARAM_ENCRYPTED: u8 = 0x04;

/// ControllerFeature strings advertised in the InitialSetup `features` array.
///
/// EMPTY to match catplay's proven-good wire (`catplay_carplay_tx_gadget` passes
/// `controller_features: vec![]`): the car intersects its own capabilities with
/// what we advertise, so advertising *nothing extra* yields the minimal,
/// deterministic session catplay uses — and base screen/video works without any
/// of these. We previously advertised 11 features we do NOT implement
/// (`iAPChannel`, `SessionManagement`, `VehicleStateProtocol`, `AltScreen`, …);
/// a capability promised in the handshake but never fulfilled is a prime
/// "session incomplete → tear down" trigger. Re-add a feature ONLY when the
/// receiving behavior actually exists behind it.
const CONTROLLER_FEATURES: &[&str] = &[];

/// CarPlay minimum surface, and the fallback when `/info` advertises no display.
const DEFAULT_W: u32 = 800;
const DEFAULT_H: u32 = 480;

// ---------------------------------------------------------------------------
// Screen data-channel packet headers (128-byte LE)
// ---------------------------------------------------------------------------

/// VideoConfig (opcode 1): plaintext avcC payload. `params[0]` = 0, `params[1]`
/// packs (width_f32_le, height_f32_le); the Encrypted flag is always set.
fn video_config_header(payload_len: u32, width: f32, height: f32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&payload_len.to_le_bytes());
    h[4] = OP_VIDEO_CONFIG;
    // small_param = h[5..8]; small_param[1] is h[6].
    h[6] |= SMALL_PARAM_ENCRYPTED;
    // params[0] = h[8..16] (0). params[1] = h[16..24] = [width][height] f32 LE.
    h[16..20].copy_from_slice(&width.to_le_bytes());
    h[20..24].copy_from_slice(&height.to_le_bytes());
    h
}

/// VideoFrame (opcode 0): encrypted AVCC payload. `params[0]` = timestamp.
fn video_frame_header(payload_len: u32, timestamp: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&payload_len.to_le_bytes());
    h[4] = OP_VIDEO_FRAME;
    // params[0] = h[8..16] = timestamp.
    h[8..16].copy_from_slice(&timestamp.to_le_bytes());
    h
}

/// KeepAlive (opcode 2): header only, no payload. The LEGACY form, used only
/// when the receiver does not ask for stats-as-body.
fn keepalive_header() -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[4] = OP_KEEPALIVE;
    h
}

/// A canonical binary plist holding an EMPTY dictionary: the body of the
/// stats-as-body keepalive.
///
/// The receiver refreshes its activity timestamp on a non-empty opcode-5 frame
/// without inspecting the plist, so an empty dictionary is the honest payload —
/// the optional statistics keys are unknown and fabricating measurements would
/// be worse than sending none.
const KEEPALIVE_STATS_BODY: &[u8; 42] = b"bplist00\xd0\x08\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x09";

/// KeepAlive with a stats body (opcode 5), for receivers whose `/info`
/// advertises `keepAliveSendStatsAsBody`.
///
/// **This is load-bearing for session survival.** Analysis of the stock Harman
/// MHI2Q receiver's `AirPlayReceiverSessionScreen_ProcessFrames` shows a
/// ZERO-BODY REJECTION *before* opcode dispatch: such a frame only increments
/// `badframes` and does NOT refresh the receiver's activity timestamp. Only a
/// non-empty opcode 2/4/5 refreshes it. So against a head unit that asks for
/// stats-as-body, our old empty opcode-2 heartbeat was silently discarded, the
/// activity clock went stale while video still flowed, and the receiver
/// abandoned the session — which matches our measurements exactly: an idle link
/// with no screen stream lives 26-52 s, but a streaming one dies within seconds.
///
/// `params[14]` carries the body length in its SECOND f32 (iOS places it there),
/// i.e. `(0.0f32, 42.0f32)`.
fn keepalive_stats_header() -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&(KEEPALIVE_STATS_BODY.len() as u32).to_le_bytes());
    h[4] = OP_KEEPALIVE_WITH_BODY;
    // params[14] = h[120..128]; first f32 stays 0.0, second carries the length.
    h[124..128].copy_from_slice(&(KEEPALIVE_STATS_BODY.len() as f32).to_le_bytes());
    h
}

// ---------------------------------------------------------------------------
// Timing port: AirPlay RTCP time-sync
// ---------------------------------------------------------------------------
//
// The head unit pings the `timingPort` we advertise and does not answer the
// screen SETUP until those pings get replies. Every ping and reply is one
// 32-byte RTCP-style datagram:
//
//   byte 0      0x80        version 2, no padding, no count
//   byte 1      0xD2 / 0xD3 packet type: time-sync request / response
//   bytes 2-3   0x0007      length in 32-bit words minus one
//   bytes 4-7   RTP time    unused here, zero in our replies
//   bytes 8-15  origin      NTP 32.32
//   bytes 16-23 receive     NTP 32.32
//   bytes 24-31 transmit    NTP 32.32 (a request carries its send time here)
//
// The reply copies the request's transmit stamp into its origin field and puts
// our clock in both receive and transmit, which is enough for the head unit to
// estimate offset and round-trip time.

const TIME_PING_LEN: usize = 32;
const TIME_REQUEST: u8 = 0xD2;
const TIME_RESPONSE: u8 = 0xD3;
/// First four bytes of a time-sync packet, minus the type byte at index 1.
const fn time_sync_header(kind: u8) -> [u8; 4] {
    [0x80, kind, 0x00, 0x07]
}

/// Our timing-port clock: NTP-epoch-based, advancing with the monotonic clock
/// from `start` (the absolute value is arbitrary, the head unit only needs a
/// steady clock).
fn ntp_now(start: Instant) -> u64 {
    crate::ntp_time::encode(crate::ntp_time::NTP_TO_UNIX + start.elapsed())
}

/// Reply to one datagram received on the timing port, or `None` when it is not
/// a time-sync request (wrong version, type or length, or shorter than 32 B).
fn answer_time_ping(datagram: &[u8], now: u64) -> Option<[u8; TIME_PING_LEN]> {
    let ping = datagram.get(..TIME_PING_LEN)?;
    let expected = time_sync_header(TIME_REQUEST);
    // Only the two version bits of byte 0 are checked.
    let is_request = ping[0] & 0xC0 == expected[0] && ping[1..4] == expected[1..4];
    if !is_request {
        return None;
    }
    let mut reply = [0u8; TIME_PING_LEN];
    reply[..4].copy_from_slice(&time_sync_header(TIME_RESPONSE));
    reply[8..16].copy_from_slice(&ping[24..32]);
    reply[16..24].copy_from_slice(&now.to_be_bytes());
    reply[24..32].copy_from_slice(&now.to_be_bytes());
    Some(reply)
}

/// Spawn the timing responder: answer every RTCP 0xD2 ping on `sock` with a
/// 0xD3 response. Runs until the socket errors or the task is aborted.
fn spawn_timing_responder(sock: tokio::net::UdpSocket) -> tokio::task::JoinHandle<()> {
    let start = Instant::now();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        let mut answered: u64 = 0;
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, peer)) => {
                    if let Some(reply) = answer_time_ping(&buf[..n], ntp_now(start)) {
                        if let Err(e) = sock.send_to(&reply, peer).await {
                            warn!("timing: send 0xD3 reply failed: {e:#} — stopping responder");
                            break;
                        }
                        answered += 1;
                        if crate::debug::sampled(answered - 1, 3, 30) {
                            debug!("timing: answered RTCP 0xD2 #{answered} from {peer}");
                        }
                    } else {
                        debug!("timing: dropping non-timesync datagram ({n}B) from {peer}");
                    }
                }
                Err(e) => {
                    warn!("timing: recv_from failed: {e:#} — stopping responder");
                    break;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// modes bodies (POST /command modesChanged + ChangeModesResponse)
// ---------------------------------------------------------------------------
//
// Wire content for the default state (as in the catplay-labs/catplay
// reference, used for the wire format) (all resources granted to the Controller):
//   resources: MainScreen(1)/MainAudio(2) → entity=Controller(1), permanentEntity=Controller(1)
//   appStates: phoneCall(2)/turnByTurn(3) entity=None(0); speech(1) entity=None(0), speechMode=None(-1)

/// The shared `params` dict (resources + appStates) used by both `modesChanged`
/// and the `ChangeModesResponse` echo.
/// Resource ids in `modesChanged`: 1 = MainScreen, 2 = MainAudio.
const RESOURCE_MAIN_SCREEN: i64 = 1;
const RESOURCE_MAIN_AUDIO: i64 = 2;
/// Entity ids: 0 = None, 1 = Controller (us, the phone), 2 = Accessory (the car).
const ENTITY_CONTROLLER: i64 = 1;
const ENTITY_ACCESSORY: i64 = 2;

/// Do we take the head unit's MainAudio resource?
///
/// This is the SINGLE source of truth for the audio decision, because
/// `modesChanged` and the audio `SETUP` must agree. They did not: we handed
/// MainAudio to the ACCESSORY in `modesChanged` and then issued `SETUP` for a
/// MainAudio stream (type 100) anyway — telling the receiver "you own the audio"
/// and immediately demanding the audio stream. On the Jaguar that SETUP (cseq=8)
/// is the one request it NEVER answers: it stops reading the video TCP 1.1 s
/// later and pulls its USB gadget 2.3 s after that
/// (boot-20260920-185642 run 005). Both settings are now derived here, so each
/// configuration is internally consistent.
///
/// DEFAULT: true. The reference implementation (catplay-labs/catplay, used as
/// a reference for the wire format) starts a session with both MainScreen and
/// MainAudio permanently owned by the Controller, i.e. the phone.
///
/// We defaulted to FALSE, i.e. we handed MainAudio to the ACCESSORY in the very
/// message that tells the head unit who owns screen and audio. That is a
/// deviation from the only implementation known to complete a session on a
/// Harman QNX unit -- and the Jaguar F-PACE is Harman. Both in-car sessions that
/// got furthest (120 and 75 frames, the two best ever) ran with this ON.
///
/// `RASCAR_CARPLAY_AUDIO=off` restores the old behaviour for an A/B;
/// `RASCAR_MODES_CLAIM_AUDIO` still overrides the claim independently.
pub(crate) fn claims_main_audio() -> bool {
    match std::env::var("RASCAR_CARPLAY_AUDIO") {
        Ok(v) if v.eq_ignore_ascii_case("off") => false,
        Ok(v) if v.eq_ignore_ascii_case("on") => true,
        _ => crate::debug::env_flag("RASCAR_MODES_CLAIM_AUDIO", true),
    }
}

/// Whether to open the audio stream at all. Follows the resource claim so we can
/// never again ask for a stream we told the car we were not taking.
pub(crate) fn audio_enabled() -> bool {
    claims_main_audio()
}

fn build_modes_params(phone_call: bool) -> plist::Dictionary {
    modes_params(
        phone_call,
        screen_with_car(),
        accessory_borrows(RESOURCE_MAIN_SCREEN),
        accessory_borrows(RESOURCE_MAIN_AUDIO),
    )
}

/// `build_modes_params` for an explicit state, so tests do not depend on the
/// process-wide flags.
///
/// `screen_to_car`: we handed MainScreen to the car's native UI (see
/// `stream::screen`); it is then the car's both as `entity` and as
/// `permanentEntity` (catplay `ResourceState::AccessoryHas`), exactly what the
/// reference sends before the car shows its own UI.
fn modes_params(phone_call: bool, screen_to_car: bool, screen_borrowed: bool, audio_borrowed: bool) -> plist::Dictionary {
    // While the car borrows a resource it is the current `entity`, and the
    // owner stays the `permanentEntity` (catplay `ResourceState::to_pair`).
    let resource = |resource_id: i64, entity: i64| -> bplist::Value {
        let borrowed = match resource_id {
            RESOURCE_MAIN_SCREEN => screen_borrowed,
            RESOURCE_MAIN_AUDIO => audio_borrowed,
            _ => false,
        };
        let mut r = plist::Dictionary::new();
        r.insert("resourceID".into(), resource_id.into());
        r.insert("entity".into(), (if borrowed { ENTITY_ACCESSORY } else { entity }).into());
        r.insert("permanentEntity".into(), entity.into());
        bplist::Value::Dictionary(r)
    };
    fn app_state(app_state_id: i64, entity: i64, speech_mode: Option<i64>) -> bplist::Value {
        let mut a = plist::Dictionary::new();
        a.insert("appStateID".into(), app_state_id.into());
        a.insert("entity".into(), entity.into());
        if let Some(sm) = speech_mode {
            a.insert("speechMode".into(), sm.into());
        }
        bplist::Value::Dictionary(a)
    }
    // We claim MainScreen, and by default LEAVE MainAudio TO THE CAR.
    //
    // We used to claim both unconditionally while never opening an audio stream —
    // i.e. the phone seizes the head unit's main audio path and then emits
    // nothing, which is exactly the "session incomplete" state a start-up
    // watchdog fires on. The reference transmitter, in this same scenario (its own
    // UI overlay, no phone peer behind it), explicitly sets MainAudio to the
    // accessory and takes only the screen. `RASCAR_MODES_CLAIM_AUDIO=on` restores
    // the old behaviour for an A/B at the car.
    let audio_entity = if claims_main_audio() { ENTITY_CONTROLLER } else { ENTITY_ACCESSORY };
    let mut params = plist::Dictionary::new();
    params.insert(
        "resources".into(),
        bplist::Value::Array(vec![
            resource(RESOURCE_MAIN_SCREEN, if screen_to_car { ENTITY_ACCESSORY } else { ENTITY_CONTROLLER }),
            resource(RESOURCE_MAIN_AUDIO, audio_entity),
        ]),
    );
    params.insert(
        "appStates".into(),
        bplist::Value::Array(vec![
            // phoneCall: us while a meeting runs (catplay `ModesState.phone_call`)
            app_state(2, if phone_call { ENTITY_CONTROLLER } else { 0 }, None),
            app_state(3, 0, None),     // turnByTurn
            app_state(1, 0, Some(-1)), // speech, speechMode=None(-1)
        ]),
    );
    params
}

/// Full `modesChanged` command body: `{type:"modesChanged", params:{…}}`.
///
/// It always states our ownership, even while the car borrows a resource:
/// announcing MainAudio as the car's and then SETting UP an audio stream (a
/// meeting starting during a borrow) is what the Jaguar tears the session down
/// for. Only the `ChangeModesResponse` reports the borrow.
///
/// The screen owner is the one exception to "always ours": after a hand-back
/// (`stream::screen`) the car owns MainScreen for real, and saying otherwise
/// while no screen stream exists would be the same kind of contradiction.
fn build_modes_changed_body(phone_call: bool) -> Result<Vec<u8>> {
    modes_changed_body(phone_call, screen_with_car())
}

/// [`build_modes_changed_body`] for an explicit screen owner (tests).
fn modes_changed_body(phone_call: bool, screen_to_car: bool) -> Result<Vec<u8>> {
    let mut d = plist::Dictionary::new();
    d.insert("type".into(), "modesChanged".into());
    d.insert("params".into(), bplist::Value::Dictionary(modes_params(phone_call, screen_to_car, false, false)));
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// MainScreen as we last announced it: true once a `modesChanged` gave it to
/// the car (hand-back), false again once one took it back. The SINGLE source
/// for the screen entry of every `modesChanged` and `ChangeModesResponse`, and
/// the gate for screen SETUPs and frames: nothing is streamed while it is set.
///
/// Only written while holding the RTSP control lock, in the same critical
/// section that sends the announcing `modesChanged` (see
/// [`send_screen_modes_changed`]), so a concurrent audio-side `modesChanged`
/// can never go out with a stale screen owner after ours.
static SCREEN_TO_CAR: AtomicBool = AtomicBool::new(false);

fn screen_with_car() -> bool {
    SCREEN_TO_CAR.load(Ordering::Relaxed)
}

/// The car is borrowing MainScreen / MainAudio (a `changeModes` Borrow it has
/// not released yet). Borrow and Unborrow need no ownership transfer on our
/// side, so unlike Take we can accept them honestly.
static SCREEN_BORROWED: AtomicBool = AtomicBool::new(false);
static AUDIO_BORROWED: AtomicBool = AtomicBool::new(false);

fn borrow_flag(resource_id: i64) -> Option<&'static AtomicBool> {
    match resource_id {
        RESOURCE_MAIN_SCREEN => Some(&SCREEN_BORROWED),
        RESOURCE_MAIN_AUDIO => Some(&AUDIO_BORROWED),
        _ => None,
    }
}

fn accessory_borrows(resource_id: i64) -> bool {
    borrow_flag(resource_id).is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Apply a car `changeModes` request. Returns false (refuse, nothing changed)
/// unless every resource in it is one we can honour:
///
/// `transferType`: 1 = Take, 2 = Untake, 3 = Borrow, 4 = Unborrow.
/// - Untake: the car hands a resource back to us; nothing to change.
/// - Borrow: the car uses the resource for a while (the Jaguar borrows
///   MainAudio, "Request Main Audio", priority 500). catplay's resource manager
///   accepts it and reports the car as the current entity. We used to refuse it
///   with status 1.
/// - Unborrow: the car gives a borrowed resource back.
/// - Take: a permanent transfer, which we do not model: refused.
///
/// A request with no resources array at all is refused.
fn apply_change_modes(body: &[u8]) -> bool {
    let Some(changes) = change_modes_borrows(body) else { return false };
    for (resource_id, borrowed) in changes {
        if let Some(flag) = borrow_flag(resource_id) {
            flag.store(borrowed, Ordering::Relaxed);
        }
    }
    true
}

/// The borrow changes a `changeModes` request asks for, as `(resourceID,
/// borrowed)`; `None` = refuse (see [`apply_change_modes`]).
fn change_modes_borrows(body: &[u8]) -> Option<Vec<(i64, bool)>> {
    const TRANSFER_UNTAKE: i64 = 2;
    const TRANSFER_BORROW: i64 = 3;
    const TRANSFER_UNBORROW: i64 = 4;
    let v = bplist::from_bytes(body).ok()?;
    let params = v.as_dictionary()?.get("params")?.as_dictionary()?;
    let resources = params.get("resources")?.as_array()?;
    if resources.is_empty() {
        return None;
    }
    let mut changes = Vec::new();
    for r in resources {
        let d = r.as_dictionary()?;
        match d.get("transferType").and_then(plist_int) {
            Some(TRANSFER_UNTAKE) => {}
            Some(t @ (TRANSFER_BORROW | TRANSFER_UNBORROW)) => {
                let id = d.get("resourceID").and_then(plist_int)?;
                borrow_flag(id)?;
                changes.push((id, t == TRANSFER_BORROW));
            }
            _ => return None,
        }
    }
    Some(changes)
}

/// `reasonStr` and `resourceID:transferType` pairs of a `changeModes`, for the log.
fn change_modes_summary(body: &[u8]) -> String {
    let Ok(v) = bplist::from_bytes(body) else { return "<undecodable>".into() };
    let params = v.as_dictionary().and_then(|d| d.get("params")).and_then(|p| p.as_dictionary());
    let reason = params.and_then(|p| p.get("reasonStr")).and_then(|r| r.as_string()).unwrap_or("");
    let pairs: Vec<String> = params
        .and_then(|p| p.get("resources"))
        .and_then(|r| r.as_array())
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r.as_dictionary())
                .map(|d| {
                    let f = |k: &str| d.get(k).and_then(plist_int).map_or("?".to_string(), |n| n.to_string());
                    format!("res{}:t{}", f("resourceID"), f("transferType"))
                })
                .collect()
        })
        .unwrap_or_default();
    format!("{reason:?} [{}]", pairs.join(" "))
}

/// An integer that the car may send as a plist integer or as a string, decimal
/// or `0x` hex (the Jaguar's `/info.audioFormats` is all strings: `"100"`,
/// `"0x8000"`).
fn plist_int(v: &bplist::Value) -> Option<i64> {
    if let Some(n) = v.as_signed_integer().or_else(|| v.as_unsigned_integer().map(|n| n as i64)) {
        return Some(n);
    }
    let s = v.as_string()?.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// `ChangeModesResponse` refusing the request: `{status: 1}` with no params.
///
/// An honest "no" is safer than accepting a transfer we do not implement.
fn build_change_modes_refusal_body() -> Result<Vec<u8>> {
    let mut d = plist::Dictionary::new();
    d.insert("status".into(), 1i64.into());
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// `ChangeModesResponse` body: `{status:0, params:{…echo…}}`.
fn build_change_modes_response_body() -> Result<Vec<u8>> {
    let mut d = plist::Dictionary::new();
    d.insert("status".into(), 0i64.into());
    d.insert("params".into(), bplist::Value::Dictionary(build_modes_params(PHONE_CALL.load(Ordering::Relaxed))));
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// What our last `modesChanged` said about phoneCall, echoed in every
/// `ChangeModesResponse` so the two never disagree.
static PHONE_CALL: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Event channel — RTSP command responder (car → phone `POST /command`)
// ---------------------------------------------------------------------------

/// A parsed touch event forwarded from a `hidSendReport` command to the touch
/// injector. Coordinates are in the car's display pixel space.
#[derive(Debug, PartialEq)]
struct HidTouch {
    down: bool,
    x: u16,
    y: u16,
}

/// Parse the 5-byte single-touch HID report `[touch:u8 bit0=down][x:u16 LE][y:u16 LE]`.
fn parse_hid_touch(report: &[u8]) -> Option<HidTouch> {
    if report.len() < 5 {
        return None;
    }
    Some(HidTouch {
        down: report[0] & 0x01 != 0,
        x: u16::from_le_bytes([report[1], report[2]]),
        y: u16::from_le_bytes([report[3], report[4]]),
    })
}

/// A parsed RTSP request off the event channel.
struct EventRequest {
    method: String,
    path: String,
    cseq: Option<i64>,
    body: Vec<u8>,
}

/// HAP encryption state for the AirPlay EVENT channel. On a paired session the
/// car sends its `POST /command` (requestUI/changeModes/forceKeyFrame/hidSendReport)
/// ChaCha20-Poly1305-encrypted with the "Events" keys, and expects our 200 OK
/// replies encrypted too. The phone is the *server* on this socket, so the
/// Read/Write key roles are INVERTED vs the control channel: we DECRYPT the car's
/// frames with the "Events-Write" key and ENCRYPT our replies with "Events-Read"
/// (verified against catplay `create_chacha_ciphers(Events, server=true)`).
struct EventCrypto {
    rx_key: [u8; 32], // decrypt car -> us  (Events-Write-Encryption-Key)
    tx_key: [u8; 32], // encrypt us -> car  (Events-Read-Encryption-Key)
    rx_nonce: u64,
    tx_nonce: u64,
}

impl EventCrypto {
    /// Derive the Events-channel keys from the pair-verify X25519 shared secret.
    fn from_shared(secret: &[u8]) -> Self {
        let rx = hkdf_sha512(secret, b"Events-Salt", b"Events-Write-Encryption-Key", 32);
        let tx = hkdf_sha512(secret, b"Events-Salt", b"Events-Read-Encryption-Key", 32);
        let mut rx_key = [0u8; 32];
        let mut tx_key = [0u8; 32];
        rx_key.copy_from_slice(&rx);
        tx_key.copy_from_slice(&tx);
        EventCrypto { rx_key, tx_key, rx_nonce: 0, tx_nonce: 0 }
    }
}

/// Append more request plaintext into `buf`, reading one HAP frame when the
/// channel is encrypted or a raw chunk otherwise. Returns `false` on clean EOF.
async fn event_fill(
    conn: &mut TcpStream,
    buf: &mut Vec<u8>,
    crypto: &mut Option<EventCrypto>,
) -> Result<bool> {
    match crypto.as_mut() {
        Some(c) => {
            let mut len_buf = [0u8; 2];
            match conn.read_exact(&mut len_buf).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(e) => return Err(e).context("event channel frame length"),
            }
            let plaintext_len = u16::from_le_bytes(len_buf) as usize;
            if plaintext_len == 0 || plaintext_len > 16384 {
                bail!("event channel: suspicious HAP frame length {plaintext_len}");
            }
            let mut ct = vec![0u8; plaintext_len + 16];
            conn.read_exact(&mut ct).await.context("event channel frame ciphertext")?;
            let cipher = ChaCha20Poly1305::new_from_slice(&c.rx_key).expect("32-byte key");
            let nonce = hap_frame_nonce(c.rx_nonce);
            c.rx_nonce += 1;
            let pt = cipher
                .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: &len_buf })
                .map_err(|_| anyhow!("event channel: HAP frame decrypt failed"))?;
            buf.extend_from_slice(&pt);
            Ok(true)
        }
        None => {
            let mut tmp = [0u8; 4096];
            let n = conn.read(&mut tmp).await.context("event channel read")?;
            if n == 0 {
                return Ok(false);
            }
            buf.extend_from_slice(&tmp[..n]);
            Ok(true)
        }
    }
}

/// Read one full RTSP request (header block + Content-Length body) from `conn`,
/// carrying surplus bytes in `buf`. Returns `Ok(None)` on clean EOF.
async fn read_event_request(
    conn: &mut TcpStream,
    buf: &mut Vec<u8>,
    crypto: &mut Option<EventCrypto>,
) -> Result<Option<EventRequest>> {
    const MAX_HEADER: usize = 16 * 1024;
    const MAX_BODY: usize = 1024 * 1024;
    // 1. Read until the end of the header block.
    let header_end = loop {
        if let Some(pos) = find_subslice(buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > MAX_HEADER {
            bail!("event channel: RTSP header exceeded {MAX_HEADER} bytes");
        }
        if !event_fill(conn, buf, crypto).await? {
            return Ok(None);
        }
    };

    // 2. Parse the request line + headers.
    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut cseq: Option<i64> = None;
    let mut content_length: usize = 0;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "cseq" => cseq = val.parse::<i64>().ok(),
                "content-length" => {
                    content_length = val.parse::<usize>().unwrap_or(0);
                    if content_length > MAX_BODY {
                        bail!("event channel: Content-Length {content_length} exceeds {MAX_BODY}");
                    }
                }
                _ => {}
            }
        }
    }

    // 3. Ensure the whole body is buffered.
    while buf.len() < header_end + content_length {
        if !event_fill(conn, buf, crypto).await? {
            bail!("event channel: EOF mid-body (wanted {content_length} bytes)");
        }
    }
    let body = buf[header_end..header_end + content_length].to_vec();
    buf.drain(..header_end + content_length);
    Ok(Some(EventRequest { method, path, cseq, body }))
}

/// Write an RTSP `200 OK` reply, optionally with a binary-plist body.
async fn reply_event_ok(
    conn: &mut TcpStream,
    cseq: Option<i64>,
    body: Option<&[u8]>,
    crypto: &mut Option<EventCrypto>,
) -> Result<()> {
    let mut head = String::from("RTSP/1.0 200 OK\r\n");
    if let Some(n) = cseq {
        head.push_str(&format!("CSeq: {n}\r\n"));
    }
    let mut out = match body {
        Some(b) if !b.is_empty() => {
            head.push_str(&format!("Content-Type: {PLIST_CT}\r\n"));
            head.push_str(&format!("Content-Length: {}\r\n\r\n", b.len()));
            let mut out = head.into_bytes();
            out.extend_from_slice(b);
            out
        }
        _ => {
            head.push_str("\r\n");
            head.into_bytes()
        }
    };
    // On a paired session the reply must be HAP-framed with the Events-Read key,
    // chunked at 1024 bytes like the control channel.
    if let Some(c) = crypto.as_mut() {
        let cipher = ChaCha20Poly1305::new_from_slice(&c.tx_key).expect("32-byte key");
        let mut framed = Vec::with_capacity(out.len() + 32);
        for chunk in out.chunks(1024) {
            framed.extend_from_slice(&seal_control_frame(&cipher, c.tx_nonce, chunk));
            c.tx_nonce += 1;
        }
        out = framed;
    }
    conn.write_all(&out).await.context("event channel reply")?;
    conn.flush().await.ok();
    Ok(())
}

/// Extract the `type` string from a command body (binary plist).
fn command_type(body: &[u8]) -> Option<String> {
    bplist::from_bytes(body)
        .ok()?
        .as_dictionary()?
        .get("type")?
        .as_string()
        .map(str::to_string)
}

/// A short `{key: value, …}` view of a command body for the logs (values
/// truncated), so an unknown command shows what it carries.
fn command_summary(body: &[u8]) -> String {
    let Ok(v) = bplist::from_bytes(body) else { return format!("<undecodable {} B>", body.len()) };
    let Some(d) = v.as_dictionary() else { return "<not a dictionary>".into() };
    let fields: Vec<String> = d
        .iter()
        .filter(|(k, _)| k.as_str() != "type")
        .map(|(k, v)| {
            let mut s = format!("{v:?}");
            if s.len() > 80 {
                s.truncate(77);
                s.push_str("...");
            }
            format!("{k}: {s}")
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// Extract the flat `hidReport` Data from a `hidSendReport` command body.
fn command_hid_report(body: &[u8]) -> Option<Vec<u8>> {
    bplist::from_bytes(body)
        .ok()?
        .as_dictionary()?
        .get("hidReport")?
        .as_data()
        .map(<[u8]>::to_vec)
}

/// The `uuid` of the HID device a `hidSendReport` comes from (Apple
/// AirPlayReceiverSession.c:5420 `kAirPlayKey_UUID`). The Jaguar sends the
/// device's `/info` uuid string.
fn command_hid_uuid(body: &[u8]) -> Option<String> {
    bplist::from_bytes(body)
        .ok()?
        .as_dictionary()?
        .get("uuid")?
        .as_string()
        .map(str::to_string)
}

/// Where one `hidSendReport` goes.
#[derive(Debug, PartialEq)]
enum HidRoute {
    /// A touch for the injector.
    Touch(HidTouch),
    /// A report from a key device, as key transitions (empty: no change, e.g.
    /// a repeat of the held state).
    Keys(Vec<hid::KeyEdge>),
    /// Nothing we can decode: logged raw so the next drive teaches us.
    Unknown,
}

/// Route a report by the device that sent it. Touch is decided by the
/// device's descriptor (a Digitizer application), never by the report length;
/// a report whose uuid is not in `/info` keeps the old behaviour (5 bytes =
/// touch) so a car that omits the uuid still has touch.
fn route_hid_report(
    devices: &[hid::HidDevice],
    keys: &mut hid::KeyTracker,
    uuid: Option<&str>,
    report: &[u8],
    now: Instant,
) -> HidRoute {
    let device = uuid.and_then(|u| devices.iter().find(|d| d.uuid == u));
    match device {
        Some(d) if d.is_touch() => parse_hid_touch(report).map_or(HidRoute::Unknown, HidRoute::Touch),
        Some(d) => match hid::pressed_usages(&d.layout, report) {
            Some(pressed) => HidRoute::Keys(keys.update(&d.uuid, &pressed, now)),
            None => HidRoute::Unknown,
        },
        None => parse_hid_touch(report).map_or(HidRoute::Unknown, HidRoute::Touch),
    }
}

/// Log a key device's report and relay its media keys to the dashboard.
///
/// Keys are relayed whoever holds the screen: they are not touches on a UI. The
/// head unit only sends them to CarPlay when it routes its media keys to the
/// phone (hypothesis, consistent with catplay forwarding every hidSendReport
/// unconditionally, `c2a/proxy_rpc/car_manager.rs:506-520`). Who holds
/// MainAudio is logged with each press so a drive can confirm it.
fn handle_key_edges(device: &str, uuid: &str, report: &[u8], edges: &[hid::KeyEdge]) {
    if edges.is_empty() {
        info!("car key: {device} uuid={uuid} report={} → no change", hex::encode(report));
    }
    for edge in edges {
        match edge {
            hid::KeyEdge::Down(u) => match hid::CarKey::from_usage(*u) {
                Some(k) => {
                    let sent = crate::stream::carkeys::send(k);
                    info!(
                        "car key: {device} uuid={uuid} report={} → press {} → dashboard '{}'{} (car borrows audio={})",
                        hex::encode(report),
                        hid::usage_name(*u),
                        k.wire_name(),
                        if sent { "" } else { " — send to rascar-app FAILED" },
                        accessory_borrows(RESOURCE_MAIN_AUDIO),
                    );
                }
                None => info!(
                    "car key: {device} uuid={uuid} report={} → press {} (no dashboard action)",
                    hex::encode(report),
                    hid::usage_name(*u),
                ),
            },
            hid::KeyEdge::Up(u, held) => info!(
                "car key: {device} uuid={uuid} report={} → release {} (held {} ms)",
                hex::encode(report),
                hid::usage_name(*u),
                held.map_or(0, |d| d.as_millis()),
            ),
        }
    }
}

/// Serve the event channel: reply to every `POST /command` and surface the
/// commands the streaming loop cares about (requestUI, changeModes, forceKeyFrame,
/// hidSendReport). Robust to unknown/undecodable commands — they still get a 200.
async fn run_event_responder(
    mut conn: TcpStream,
    mut crypto: Option<EventCrypto>,
    requestui: Arc<Notify>,
    touch_tx: mpsc::UnboundedSender<HidTouch>,
    force_kf: Arc<AtomicBool>,
    modes_dirty: Arc<Notify>,
    hid_devices: Vec<hid::HidDevice>,
) {
    let mut key_tracker = hid::KeyTracker::default();
    if crypto.is_some() {
        info!("event channel: HAP-encrypted (paired session, Events keys)");
    }
    let mut buf: Vec<u8> = Vec::new();
    // Set once the car has moved a resource with an accepted changeModes: from
    // then on a requestUI while we think we hold the screen means our view and
    // the car's disagree. Before that, a requestUI is the start-up "ready".
    let mut car_changed_modes = false;
    loop {
        let req = match read_event_request(&mut conn, &mut buf, &mut crypto).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                info!("event channel: closed by car (EOF)");
                break;
            }
            Err(e) => {
                warn!("event channel: {e:#} — stopping responder");
                break;
            }
        };

        if req.method != "POST" || req.path != "/command" {
            // Anything else still gets acked so a strict car does not stall.
            let _ = reply_event_ok(&mut conn, req.cseq, None, &mut crypto).await;
            continue;
        }

        let ty = command_type(&req.body);
        let mut reply_body: Option<Vec<u8>> = None;
        let mut announce_modes = false;
        match ty.as_deref() {
            Some("requestUI") => {
                // Two meanings. At start-up it is the car ready to show us
                // (awaited once in `run_carplay`). While MainScreen is the
                // car's (hand-back), it is the driver touching the car's
                // CarPlay icon: take the screen back (catplay
                // `car_manager.rs:609-614` → `modes_arbiter.rs:89-111`).
                //
                // A third: the car only BORROWED the screen (its own UI over
                // ours, our stream still running). catplay takes it back on any
                // requestUI while it does not hold the screen (`try_steal_screen`
                // → modesChanged). Ignoring it left the Jaguar on its own UI
                // after two CarPlay-icon presses, then it reset the USB link
                // (seen on a drive, two presses 10 s apart).
                if screen_with_car() {
                    info!("screen: requestUI — taking the screen back");
                    crate::stream::screen::request(crate::stream::screen::ScreenOwner::Phone);
                } else if SCREEN_BORROWED.swap(false, Ordering::Relaxed) {
                    info!("screen: requestUI while the car borrows the screen — taking it back");
                    force_kf.store(true, Ordering::Relaxed);
                    announce_modes = true;
                } else if car_changed_modes {
                    // We believe we hold the screen, but the car asking for UI
                    // says it is not showing us. After a borrow + release the
                    // Jaguar sent two requestUI that we ignored and it stayed
                    // off CarPlay for 3 min. Re-assert our
                    // modes and send a keyframe: harmless if we did hold it.
                    info!("screen: requestUI while we hold the screen — re-announcing modes + keyframe");
                    force_kf.store(true, Ordering::Relaxed);
                    announce_modes = true;
                } else {
                    info!("event: requestUI");
                }
                requestui.notify_one();
            }
            Some("changeModes") => {
                // The car is asking to take/borrow a resource (its own UI, a
                // reverse camera, a chime). We used to answer a CONSTANT
                // `{status: 0, params: <we own everything>}`: status 0 means
                // "accepted", so the same body accepted the request and then
                // asserted the transfer had not happened. That contradiction is
                // exactly what a resource arbiter treats as a protocol violation.
                //
                // We still do not model resource ownership, so we cannot honestly
                // accept. Answer with an explicit refusal unless every resource in
                // the request is an Untake (transferType 2 = the car handing a
                // resource back to us), which needs no state change on our side.
                let accepted = apply_change_modes(&req.body);
                info!(
                    "event: changeModes {} → {} (car borrows: screen={} audio={})",
                    change_modes_summary(&req.body),
                    if accepted { "accepted" } else { "refused (status 1)" },
                    accessory_borrows(RESOURCE_MAIN_SCREEN),
                    accessory_borrows(RESOURCE_MAIN_AUDIO),
                );
                announce_modes = accepted;
                car_changed_modes |= accepted;
                reply_body = if accepted {
                    build_change_modes_response_body().ok()
                } else {
                    build_change_modes_refusal_body().ok()
                };
            }
            Some("forceKeyFrame") => {
                debug!("event: forceKeyFrame");
                force_kf.store(true, Ordering::Relaxed);
            }
            Some("hidSendReport") => {
                let report = command_hid_report(&req.body).unwrap_or_default();
                let uuid = command_hid_uuid(&req.body);
                let uuid_s = uuid.as_deref().unwrap_or("<none>");
                let device = uuid
                    .as_deref()
                    .and_then(|u| hid_devices.iter().find(|d| d.uuid == u))
                    .map_or("unknown device", |d| d.name.as_str());
                match route_hid_report(&hid_devices, &mut key_tracker, uuid.as_deref(), &report, Instant::now()) {
                    HidRoute::Touch(t) => {
                        if screen_with_car() {
                            // The car's own UI is on screen: a touch there is not ours.
                            debug!("event: touch while the car owns the screen — ignored");
                        } else {
                            let _ = touch_tx.send(t);
                        }
                    }
                    HidRoute::Keys(edges) => handle_key_edges(device, uuid_s, &report, &edges),
                    HidRoute::Unknown => info!(
                        "car HID: unmapped report from {device} uuid={uuid_s} report={} ({} B) — not acted on",
                        hex::encode(&report),
                        report.len()
                    ),
                }
            }
            // The voice / Siri button (Apple AirPlayCommon.h:668, `siriAction`
            // 1 prewarm, 2 button down, 3 button up). Not seen from the Jaguar
            // yet; logged so a drive shows whether it sends one.
            Some("requestSiri") => {
                info!("event: requestSiri {} (acked, no voice assistant wired)", command_summary(&req.body));
            }
            Some(other) => info!("event: command {other} {} (acked, no action)", command_summary(&req.body)),
            None => debug!("event: undecodable command body ({}B, acked)", req.body.len()),
        }

        if let Err(e) = reply_event_ok(&mut conn, req.cseq, reply_body.as_deref(), &mut crypto).await {
            warn!("event channel: reply failed: {e:#} — stopping responder");
            break;
        }
        if announce_modes {
            // After the reply, never before: see `send_state_modes_changed`.
            modes_dirty.notify_one();
        }
    }
}

// ---------------------------------------------------------------------------
// Touch injection (event-channel hidSendReport → dashboard)
// ---------------------------------------------------------------------------

/// Coordinate range of the car's "Touch Screen" HID report. The Jaguar's
/// descriptor (from /info hidDevices) declares X and Y as 16-bit with logical
/// range 0..32767, independent of the 1200x512 panel. Scaling from the display
/// size instead clamped every touch to the bottom-right dashboard pixel.
const HID_TOUCH_LOGICAL: (u32, u32) = (32768, 32768);

/// Consume parsed touch events, derive Press/Drag/Release from the touch-down
/// transition, map car coords → dashboard coords, and inject via [`InputInjector`].
fn spawn_touch_injector(
    mut touch_rx: mpsc::UnboundedReceiver<HidTouch>,
    car_res: (u32, u32),
    dash_res: (u32, u32),
    display: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut injector = InputInjector::new(&display, dash_res.0, dash_res.1);
        if let Err(e) = injector.setup() {
            warn!("touch: injector setup failed ({e:#}) — touch disabled for this session");
            // Still drain the channel so the sender never backpressures.
            while touch_rx.recv().await.is_some() {}
            return;
        }
        let mut was_down = false;
        while let Some(t) = touch_rx.recv().await {
            let action = match (was_down, t.down) {
                (false, true) => TouchAction::Press,
                (true, true) => TouchAction::Drag,
                (true, false) => TouchAction::Release,
                (false, false) => continue, // spurious up-while-up
            };
            was_down = t.down;
            let (mx, my) = map_coords(t.x as u32, t.y as u32, car_res, dash_res);
            debug!("touch {action:?} car=({},{}) → dash=({mx},{my})", t.x, t.y);
            injector.inject_touch(action, mx, my, 0);
        }
        info!("touch: event source closed — stopping injector");
    })
}

// ---------------------------------------------------------------------------
// keepAlivePort UDP keepalive
// ---------------------------------------------------------------------------

/// If the SETUP /setup reply advertised a `keepAlivePort`, send a 4-byte
/// `AirPlayIdleKeepAlivePacket` to it on [`keepalive_interval`].
///
/// The reference transmitter (catplay-labs/catplay) pings this port every
/// 10 s, introduced there against a ~10 s session drop on cars that police the
/// keepalive.
///
/// 2026-09-21: a theory that the Jaguar polices this port was **tested and
/// refuted**. Across 28 in-car runs each sent exactly one datagram (10 s
/// interval, ~5 s sessions), and the interval from that ping to the car going
/// silent was mean 1.276 s, **sd 0.702**, range 0.65-3.87 -- looser than the
/// auth anchor (sd 0.573) over the same runs. A policed deadline would be the
/// tight one. It is not: the car tolerated anything from 0.65 s to 3.87 s.
///
/// The 1 s default below is kept only because it is harmless (4 bytes/s, the
/// `/feedback` cadence) and strictly more conservative. It is NOT a fix, and
/// nothing here should be cited as the cause of the teardown.
/// How often to ping the accessory's `keepAlivePort`.
///
/// Default 1 s: conservative, 4 bytes a second, matching the `/feedback`
/// cadence. Override with `RASCAR_KEEPALIVE_SECS` (clamped to 1..=30); the
/// reference transmitter uses 10. See `spawn_keepalive` -- the theory that
/// this interval caused the Jaguar teardown was tested and refuted.
pub(crate) fn keepalive_interval() -> Duration {
    const DEFAULT_SECS: u64 = 1;
    let secs = std::env::var("RASCAR_KEEPALIVE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SECS)
        .clamp(1, 30);
    Duration::from_secs(secs)
}

fn spawn_keepalive(peer: SocketAddr, port: u16) -> tokio::task::JoinHandle<()> {
    let addr = with_port(peer, port);
    tokio::spawn(async move {
        let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let sock = match tokio::net::UdpSocket::bind(bind).await {
            Ok(s) => s,
            Err(e) => {
                warn!("keepalive: bind failed ({e:#}) — keepalive disabled");
                return;
            }
        };
        // tokio's interval fires its FIRST tick immediately, so the loop below
        // sends the first keepalive datagram at t≈0, as the reference transmitter
        // does (immediately on connect, then periodically). We used to
        // swallow that first tick, which pushed our first ping to t+10 s — i.e.
        // AFTER the car's ~7-11 s keepalive-policing teardown, so it never arrived
        // in time. The whole point of this keepalive is to beat that deadline.
        let mut tick = tokio::time::interval(keepalive_interval());
        loop {
            tick.tick().await;
            // Apple's `AirPlayIdleKeepAlivePacket`: byte 0 carries a 2-bit
            // VERSION in bits 7-6 (must be 0) and a "sleep" request in bit 5;
            // the rest is the session id. We used to send four RANDOM bytes,
            // which declares a bogus version roughly three times in four. It
            // happened to work because any datagram resets the receiver's select
            // timeout, but a receiver that validates the header would drop it.
            let pkt = [0u8; 4];
            if let Err(e) = sock.send_to(&pkt, addr).await {
                warn!("keepalive: send to {addr} failed ({e:#}) — stopping");
                break;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// TEARDOWN
// ---------------------------------------------------------------------------

/// Send `TEARDOWN /` on the control channel: bplist `{streams:[{type:110, uuid}]}`.
/// Each stream entry carries its `type` and, for the screen, its `uuid`.
async fn send_teardown(rtsp: &SharedRtsp, display_uuid: &str) {
    let mut stream = plist::Dictionary::new();
    stream.insert("type".into(), 110i64.into());
    stream.insert("uuid".into(), display_uuid.into());
    let mut d = plist::Dictionary::new();
    d.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(stream)]));
    let body = match bplist::to_bytes(&bplist::Value::Dictionary(d)) {
        Ok(b) => b,
        Err(e) => {
            warn!("teardown: marshal failed: {e:#}");
            return;
        }
    };
    let mut guard = rtsp.lock().await;
    match guard.request("TEARDOWN", "/", PLIST_CT, &body, &Vec::new()).await {
        Ok(r) => info!("← TEARDOWN HTTP {}", r.status),
        Err(e) => warn!("teardown: request failed: {e:#}"),
    }
}

// ---------------------------------------------------------------------------
// Screen stream (open / reopen after a hand-back)
// ---------------------------------------------------------------------------

/// One negotiated screen stream: its id (keys the cipher) and the data socket.
struct ScreenStream {
    stream_connection_id: i64,
    wr: OwnedWriteHalf,
    /// Drains the reverse direction; aborted with the stream.
    drain: AbortOnDrop,
}

/// `SETUP /` of a screen stream (type 110) and connect its data channel.
///
/// Used at start-up and again after every hand-back: a torn-down stream cannot
/// be resumed, the come-back needs a fresh SETUP (catplay
/// `modes_arbiter.rs:89-111` re-runs the whole screen setup). Must only be
/// called while MainScreen is announced as ours.
async fn open_screen_stream(rtsp: &mut RtspClient, peer: SocketAddr, display_uuid: &str) -> Result<ScreenStream> {
    debug_assert!(!screen_with_car(), "screen SETUP while MainScreen is announced as the car's");
    let stream_connection_id = random_positive_i64();
    info!("→ SETUP screen (streamConnectionID={stream_connection_id}, type=110)");
    let stream_body = build_stream_setup(stream_connection_id, display_uuid).context("marshal screen SETUP plist")?;
    let resp = rtsp.request("SETUP", "/", PLIST_CT, &stream_body, &Vec::new()).await.context("SETUP / (screen)")?;
    if !resp.is_success() {
        bail!("← SETUP screen REJECTED: HTTP {}", resp.status);
    }
    let stream_reply = bplist::from_bytes(&resp.body).context("parse screen SETUP reply plist")?;
    let data_port = extract_stream_data_port(&stream_reply, 110).context("screen SETUP reply carried no type-110 dataPort")?;
    info!("← SETUP screen dataPort={data_port}");

    let data_addr = with_port(peer, data_port);
    let data_conn = TcpStream::connect(data_addr).await.with_context(|| format!("connect screen data channel {data_addr}"))?;
    data_conn.set_nodelay(true).ok();
    info!("data channel connected: {data_addr}");
    let (data_rd, wr) = data_conn.into_split();
    // Drain the reverse data direction so it never backpressures us. Touch/HID
    // arrives on the EVENT channel (hidSendReport), not here.
    let drain = AbortOnDrop(tokio::spawn(async move { drain(data_rd).await }));
    Ok(ScreenStream { stream_connection_id, wr, drain })
}

/// Screen encryption, keyed with the SAME streamConnectionID advertised in the
/// screen SETUP:
///   * HAP-paired session → ChaCha20-Poly1305 (DataStream), IKM = the
///     pair-verify shared secret (HKDF salt "DataStream-Salt<id>", info
///     "DataStream-Output-Encryption-Key"); the 128-byte frame header is AAD.
///   * legacy et=NONE session → AES-128-CTR with an all-zero session key
///     (catplay `derive_aes_stream_keys(&[0;16], id)`); the car's keystream
///     comes from that id.
///
/// VideoConfig / KeepAlive frames stay plaintext and do NOT advance the cipher.
fn screen_cipher(paired_secret: Option<&[u8]>, stream_connection_id: i64) -> VideoCipher {
    let cipher = match paired_secret {
        Some(secret) => VideoCipher::from_session(&[0u8; 16], true, secret, stream_connection_id),
        None => VideoCipher::from_session(&[0u8; 16], false, &[], stream_connection_id),
    };
    info!("screen cipher = {} (streamConnectionID={stream_connection_id})", cipher.name());
    cipher
}

/// Resolve once the desired screen owner is `want` (now or later).
async fn wait_for_screen_owner(
    rx: &mut tokio::sync::watch::Receiver<crate::stream::screen::ScreenOwner>,
    want: crate::stream::screen::ScreenOwner,
) {
    loop {
        if *rx.borrow_and_update() == want {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// While the car owns the screen there is no screen stream whose failure would
/// end the session, so watch its other two lifelines: Ok once the screen is
/// wanted back, Err if the event channel or `/feedback` stopped first.
async fn wait_screen_back(
    rx: &mut tokio::sync::watch::Receiver<crate::stream::screen::ScreenOwner>,
    event_task: &tokio::task::JoinHandle<()>,
    feedback: &tokio::task::JoinHandle<()>,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = wait_for_screen_owner(rx, crate::stream::screen::ScreenOwner::Phone) => return Ok(()),
            _ = tick.tick() => {
                if event_task.is_finished() {
                    bail!("event channel closed while the car owned the screen");
                }
                if feedback.is_finished() {
                    bail!("/feedback stopped while the car owned the screen");
                }
            }
        }
    }
}

/// Parse `/info` → `hidDevices[]`: each device's uuid, name and report
/// descriptor (Apple `AirPlayInfoArrayAddHIDDevice`). The uuid is what every
/// `hidSendReport` names; the descriptor says what its bytes mean.
fn parse_hid_devices(body: &[u8]) -> Vec<hid::HidDevice> {
    let Ok(v) = bplist::from_bytes(body) else { return Vec::new() };
    let Some(arr) = v.as_dictionary().and_then(|d| d.get("hidDevices")).and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let d = e.as_dictionary()?;
            let uuid = d.get("uuid")?.as_string()?.to_string();
            let name = d.get("name").and_then(|n| n.as_string()).unwrap_or("").to_string();
            let desc = d.get("hidDescriptor").and_then(|x| x.as_data()).unwrap_or(&[]);
            Some(hid::HidDevice { uuid, name, layout: hid::parse_descriptor(desc) })
        })
        .collect()
}

/// One line per car HID device: what it is and which usages it can send, so a
/// car we have never seen documents its own buttons in the first log.
fn log_hid_devices(devices: &[hid::HidDevice]) {
    for d in devices {
        let usages: Vec<String> = d
            .layout
            .fields
            .iter()
            .flat_map(|f| f.usages.iter())
            .filter(|u| hid::usage_id(**u) != 0)
            .map(|u| hid::usage_name(*u))
            .collect();
        info!(
            "← /info HID device '{}' uuid={} application={} usages=[{}]",
            d.name,
            d.uuid,
            d.layout.application.map_or_else(|| "?".to_string(), hid::usage_name),
            usages.join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// Audio (type 100 PCM 48k stereo over RTP/UDP) — et=NONE, no crypto
// ---------------------------------------------------------------------------
//
// Field names + enum values as in the catplay-labs/catplay stream description
// (used as a reference for the wire format):
//   type=MainAudio(100), audioFormat=PCM_48000_STEREO (bit 15), audioType=Media.
// NOTE (INFERRED vs task text): catplay's `AudioType` is a plain `plist_enum!`,
// so it serializes as the STRING "media", not an integer — the task's
// "audioType:Media(int)" is imprecise; the byte-accurate value is the string.
// StreamType and AudioFormat ARE integers (plist_enum_repr / bitflags).

const AUDIO_STREAM_TYPE_MAIN: i64 = 100;
const AUDIO_FMT_PCM_48000_STEREO: i64 = 1 << 15;
const AUDIO_LATENCY_MS: i64 = 100;
const AUDIO_SAMPLE_RATE: u32 = 48000;
const AUDIO_CHANNELS: u32 = 2;
/// Frames per RTP packet (catplay: 352, also for PCM telephony).
const AUDIO_FRAMES_PER_PACKET: u32 = 352;
const AUDIO_PAYLOAD_TYPE: u8 = 100;
/// Redundancy is a modern-CarPlay feature we do not implement.
const AUDIO_SUPPORTS_RTP_REDUNDANCY: bool = false;
/// catplay always advertises high-accuracy timestamps on the audio stream.
const AUDIO_SUPPORTS_HIGH_ACCURACY_TS: bool = true;

/// Telephony PCM formats we can use, best first, as (audioFormat bit, rate).
/// All mono 16-bit (catplay's receiver offers 16/24/32 kHz for telephony; its
/// own transmitter test uses 16 kHz).
const TELEPHONY_FORMATS: &[(i64, u32)] = &[(1 << 6, 24000), (1 << 4, 16000), (1 << 8, 32000), (1 << 14, 48000)];
/// When `/info` gives no usable list: catplay's transmitter default.
const TELEPHONY_FALLBACK: (i64, u32) = (1 << 4, 16000);

/// What the MainAudio stream carries.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AudioMode {
    /// Music etc.: 48 kHz stereo, output only.
    Media,
    /// A meeting: `format` (mono PCM at `rate`) both ways, the car's
    /// microphone coming back to us.
    Telephony { format: i64, rate: u32 },
    /// An incoming modem call rings: our generated ringtone, output only, in
    /// the format the car lists for `audioType: "alert"` (spec Table 23-46:
    /// "Ringtones, alarms, and other high-priority sounds").
    Alert { format: i64, rate: u32, channels: u32 },
}

impl AudioMode {
    fn rate(self) -> u32 {
        match self {
            AudioMode::Media => AUDIO_SAMPLE_RATE,
            AudioMode::Telephony { rate, .. } | AudioMode::Alert { rate, .. } => rate,
        }
    }
    fn channels(self) -> u32 {
        match self {
            AudioMode::Media => AUDIO_CHANNELS,
            AudioMode::Telephony { .. } => 1,
            AudioMode::Alert { channels, .. } => channels,
        }
    }
    fn is_call(self) -> bool {
        matches!(self, AudioMode::Telephony { .. })
    }
}

/// The telephony mode the car supports, from `/info.audioFormats`
/// (`[{type, audioType, audioInputFormats, audioOutputFormats}]`, catplay
/// `AudioFormatStruct`). `None` = the car lists formats but none for
/// telephony: it has no hands-free path for us, so meetings keep the
/// dashboard's own audio. Also logs the list: the full `/info` dump is cut
/// by the car's embedded PNG icons before it ever reaches it.
fn telephony_mode_from_info(body: &[u8]) -> Option<AudioMode> {
    let fallback = AudioMode::Telephony { format: TELEPHONY_FALLBACK.0, rate: TELEPHONY_FALLBACK.1 };
    let Ok(v) = bplist::from_bytes(body) else { return Some(fallback) };
    let Some(list) = v.as_dictionary().and_then(|d| d.get("audioFormats")).and_then(|x| x.as_array()) else {
        info!("← /info audioFormats: absent — telephony will try {}Hz mono", TELEPHONY_FALLBACK.1);
        return Some(fallback);
    };
    let int = |d: &plist::Dictionary, k: &str| d.get(k).and_then(plist_int);
    let mut telephony = None;
    for e in list.iter().filter_map(|e| e.as_dictionary()) {
        let ty = int(e, "type");
        let at = e.get("audioType").and_then(|x| x.as_string()).unwrap_or("-");
        let (inp, out) = (int(e, "audioInputFormats"), int(e, "audioOutputFormats"));
        info!("← /info audioFormats: type={ty:?} audioType={at} in={inp:?} out={out:?}");
        if ty == Some(AUDIO_STREAM_TYPE_MAIN) && at == "telephony" {
            telephony = Some(inp.unwrap_or(0) & out.unwrap_or(0));
        }
    }
    let mask = telephony?;
    let (format, rate) = TELEPHONY_FORMATS.iter().copied().find(|(bit, _)| mask & bit != 0).unwrap_or(TELEPHONY_FALLBACK);
    Some(AudioMode::Telephony { format, rate })
}

/// Alert PCM formats we can generate, best first, as (audioFormat bit, rate,
/// channels): `AirPlayCommon.h:346-351`. The spec requires the car to take
/// 44.1 or 48 kHz 16-bit for alerts (23.2.8.2.5, Table 23-16); the Jaguar
/// lists exactly `0x800` = PCM_44KHz_16Bit_Stereo (2026-09-29 pcap).
const ALERT_FORMATS: &[(i64, u32, u32)] = &[(1 << 11, 44100, 2), (1 << 15, 48000, 2), (1 << 10, 44100, 1), (1 << 14, 48000, 1)];

/// The alert (ringtone) mode the car offers in `/info.audioFormats`.
///
/// Unlike telephony there is NO fallback: this car tears CarPlay down on
/// traffic it did not declare, and ringing has a harmless alternative (the
/// dashboard shows the call, the media stream keeps playing). So we only ever
/// SETUP an alert stream the car listed, in a format it listed.
fn alert_mode_from_info(body: &[u8]) -> Option<AudioMode> {
    let v = bplist::from_bytes(body).ok()?;
    let list = v.as_dictionary()?.get("audioFormats")?.as_array()?;
    let mask = list.iter().filter_map(|e| e.as_dictionary()).find_map(|e| {
        let ty = e.get("type").and_then(plist_int);
        let at = e.get("audioType").and_then(|x| x.as_string());
        (ty == Some(AUDIO_STREAM_TYPE_MAIN) && at == Some("alert")).then(|| e.get("audioOutputFormats").and_then(plist_int).unwrap_or(0))
    })?;
    let (format, rate, channels) = ALERT_FORMATS.iter().copied().find(|(bit, _, _)| mask & bit != 0)?;
    Some(AudioMode::Alert { format, rate, channels })
}

/// What the MainAudio stream should carry for a line state.
///
/// A call (meeting or modem) beats ringing: the user is talking, and a second
/// call ringing then is shown on the dashboard only (the backend has no call
/// waiting). Each mode needs the car to offer its stream type (`None` = the
/// car did not list it); otherwise the line state falls through to the next.
fn desired_audio_mode(line: crate::stream::call::Line, telephony: Option<AudioMode>, alert: Option<AudioMode>) -> AudioMode {
    match (line.call, telephony, line.ring, alert) {
        (true, Some(t), _, _) => t,
        (_, _, true, Some(a)) => a,
        _ => AudioMode::Media,
    }
}

/// Whether `modesChanged` announces `phoneCall: Controller` for `mode`.
///
/// Telephony: yes, that is what lets us SETUP a telephony stream (catplay
/// `ModesState.phone_call`). Alert (ringing): the spec defines phoneCall as
/// "an entity is on a phone call" (23.3.3.2) and says nothing about ringing,
/// so by default we leave it at None and only re-state our MainAudio
/// ownership before the alert SETUP. `RASCAR_PHONE_RING_CALLSTATE=on` asserts
/// phoneCall while ringing instead, an A/B for whether that makes the Jaguar
/// bring the CarPlay screen forward (hypothesis, never tested at the car).
fn announces_phone_call(mode: AudioMode) -> bool {
    match mode {
        AudioMode::Telephony { .. } => true,
        AudioMode::Alert { .. } => crate::debug::env_flag("RASCAR_PHONE_RING_CALLSTATE", false),
        AudioMode::Media => false,
    }
}

/// Build the audio stream `SETUP /` body (single type-100 PCM stream).
///
/// The key set + values are those the catplay-labs/catplay transmitter puts on
/// the wire for an audio stream (camelCase keys, absent optional keys omitted):
///   streamConnectionID, type, audioFormat, audioLatencyMs, audioType, input,
///   supportsRTPPacketRedundancy, supportsHighAccuracyTimestamps.
/// There is NO `uuid` on an audio stream (that field is screen-only,
/// `StreamDescriptionScreen`), and NO per-stream encryption key: stream
/// encryption is negotiated once at the session level in the initial
/// `SETUP /setup` (`et`), never in the per-stream SETUP.
///
/// `scid` is the streamConnectionID: `0` for an unencrypted (et=NONE) session —
/// exactly what catplay's `do_setup_audio` emits when `cipher != ChaCha` — and a
/// random strictly-positive value on a HAP-paired session (it keys the audio
/// RTP ChaCha stream, `DataStream-Salt<scid>`, just like the screen).
///
/// Telephony adds `input: true` and our own `dataPort`: the car records its
/// microphone and sends it there (catplay `RtpReceiver::new_common` connects
/// its recorder to `peer:data_port`, and rejects an input stream without one).
fn build_audio_setup(scid: i64, control_port: u16, mode: AudioMode, input_port: Option<u16>) -> Result<Vec<u8>> {
    let mut s = plist::Dictionary::new();
    s.insert("streamConnectionID".into(), scid.into());
    s.insert("type".into(), AUDIO_STREAM_TYPE_MAIN.into());
    match mode {
        AudioMode::Media => {
            s.insert("audioType".into(), "media".into());
            s.insert("audioFormat".into(), AUDIO_FMT_PCM_48000_STEREO.into());
            s.insert("input".into(), false.into());
        }
        AudioMode::Alert { format, .. } => {
            s.insert("audioType".into(), "alert".into());
            s.insert("audioFormat".into(), format.into());
            s.insert("input".into(), false.into());
        }
        AudioMode::Telephony { format, .. } => {
            s.insert("audioType".into(), "telephony".into());
            s.insert("audioFormat".into(), format.into());
            s.insert("input".into(), true.into());
            if let Some(p) = input_port {
                s.insert("dataPort".into(), (p as i64).into());
            }
        }
    }
    s.insert("audioLatencyMs".into(), AUDIO_LATENCY_MS.into());
    s.insert("supportsRTPPacketRedundancy".into(), AUDIO_SUPPORTS_RTP_REDUNDANCY.into());
    s.insert("supportsHighAccuracyTimestamps".into(), AUDIO_SUPPORTS_HIGH_ACCURACY_TS.into());
    // Advertise our RTCP control port (catplay's TX always binds an RTCP socket
    // and carries `controlPort` in the audio SETUP request; a receiver that
    // wants RTCP clock/liveness on the audio stream sees an RTP-only stream as
    // incomplete). Our screen stream logged controlPort=None because we never
    // offered one — now we do, and hold the socket open for the session.
    s.insert("controlPort".into(), (control_port as i64).into());
    let mut d = plist::Dictionary::new();
    d.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(s)]));
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// Extract `(dataPort, controlPort)` for a stream `type` from a SETUP reply.
fn extract_audio_ports(v: &bplist::Value, stream_type: i64) -> (Option<u16>, Option<u16>) {
    let Some(streams) = v.as_dictionary().and_then(|d| d.get("streams")).and_then(|x| x.as_array()) else {
        return (None, None);
    };
    for s in streams {
        let Some(d) = s.as_dictionary() else { continue };
        let ty = d
            .get("type")
            .and_then(|x| x.as_signed_integer().or_else(|| x.as_unsigned_integer().map(|n| n as i64)));
        if ty == Some(stream_type) {
            return (plist_u16(d, "dataPort"), plist_u16(d, "controlPort"));
        }
    }
    (None, None)
}

/// Audio RTP payload cipher, selected to MATCH the negotiated session encryption
/// (exactly like the screen). The catplay receiver keys audio decryption off the
/// SESSION cipher (`self.stream_encryption`, set in the initial `SETUP /setup`),
/// NOT off any per-stream field — so a HAP-paired session (which our Jaguar
/// forces by 466-ing et=NONE) expects ChaCha-encrypted audio, and only a true
/// et=NONE session accepts plaintext PCM.
///
/// The ChaCha derivation is identical to the screen DataStream cipher
/// (`crate::stream::crypto::derive_chacha_key`, HKDF-SHA512, salt
/// `DataStream-Salt<scid>`, info `DataStream-Output-Encryption-Key`); only the
/// per-packet framing differs (catplay `rtp/rtp_cipher_chacha.rs`):
/// `[RTP hdr 12][ciphertext N][Poly1305 tag 16][nonce 8]`, AAD =
/// `((timestamp<<32)|ssrc)` big-endian (8 bytes), nonce = `[0;4] ++ LE64(counter)`.
enum AudioRtpCipher {
    /// Plaintext PCM — only for a genuine et=NONE session.
    None,
    /// ChaCha20-Poly1305, keyed with the audio stream's streamConnectionID.
    ChaCha { aead: Box<ChaCha20Poly1305>, counter: u64 },
}

impl AudioRtpCipher {
    /// Build the audio cipher from the session's pairing secret. `None`/empty
    /// secret → plaintext (et=NONE). A paired secret → ChaCha keyed with `scid`.
    fn from_session(paired_secret: Option<&[u8]>, scid: i64) -> Self {
        if let Some(secret) = paired_secret.filter(|s| !s.is_empty()) {
            match crate::stream::crypto::derive_chacha_key(secret, scid) {
                Ok(key) => match ChaCha20Poly1305::new_from_slice(&key) {
                    Ok(aead) => {
                        return AudioRtpCipher::ChaCha { aead: Box::new(aead), counter: 0 };
                    }
                    Err(e) => warn!("audio: chacha init failed ({e}) — sending plaintext"),
                },
                Err(e) => warn!("audio: chacha HKDF failed ({e:#}) — sending plaintext"),
            }
        }
        AudioRtpCipher::None
    }

    fn name(&self) -> &'static str {
        match self {
            AudioRtpCipher::None => "none(plaintext)",
            AudioRtpCipher::ChaCha { .. } => "chacha20-poly1305",
        }
    }

    /// Assemble a full RTP packet (12-byte header, PT=100) for `pcm`, encrypting
    /// the payload per the session cipher.
    fn build_packet(&mut self, seq: u16, ts: u32, ssrc: u32, pcm: &[u8]) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(12 + pcm.len() + 24);
        pkt.push(2u8 << 6); // v=2, p=0, x=0, cc=0
        pkt.push(AUDIO_PAYLOAD_TYPE); // marker=0, PT=100
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&ts.to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        match self {
            AudioRtpCipher::None => pkt.extend_from_slice(pcm),
            AudioRtpCipher::ChaCha { aead, counter } => {
                let aad = (((ts as u64) << 32) | ssrc as u64).to_be_bytes();
                let mut n = [0u8; 12];
                n[4..].copy_from_slice(&counter.to_le_bytes());
                let ct = aead
                    .encrypt(Nonce::from_slice(&n), Payload { msg: pcm, aad: &aad })
                    .expect("audio chacha seal");
                pkt.extend_from_slice(&ct); // ciphertext + 16-byte Poly1305 tag
                pkt.extend_from_slice(&counter.to_le_bytes()); // 8-byte nonce trailer
                *counter = counter.wrapping_add(1);
            }
        }
        pkt
    }
}

/// Decrypts what the car sends on a telephony stream (its microphone): the
/// same RTP/ChaCha framing as [`AudioRtpCipher`], keyed with the "Input"
/// direction (catplay: the receiver's write cipher is the Input key).
enum MicCipher {
    None,
    ChaCha(Box<ChaCha20Poly1305>),
}

impl MicCipher {
    fn from_session(paired_secret: Option<&[u8]>, scid: i64) -> Self {
        if let Some(secret) = paired_secret.filter(|s| !s.is_empty()) {
            match crate::stream::crypto::derive_chacha_input_key(secret, scid)
                .map_err(|e| anyhow!("{e:#}"))
                .and_then(|k| ChaCha20Poly1305::new_from_slice(&k).map_err(|e| anyhow!("{e}")))
            {
                Ok(aead) => return MicCipher::ChaCha(Box::new(aead)),
                Err(e) => warn!("call: mic chacha init failed ({e:#}) — treating it as plaintext"),
            }
        }
        MicCipher::None
    }

    /// One received RTP packet → its PCM payload (None when it does not verify).
    fn open<'a>(&self, pkt: &'a [u8]) -> Option<std::borrow::Cow<'a, [u8]>> {
        if pkt.len() < 12 || pkt[0] >> 6 != 2 {
            return None;
        }
        match self {
            MicCipher::None => Some(std::borrow::Cow::Borrowed(&pkt[12..])),
            MicCipher::ChaCha(aead) => {
                if pkt.len() < 12 + 16 + 8 {
                    return None;
                }
                let ts = u32::from_be_bytes(pkt[4..8].try_into().ok()?);
                let ssrc = u32::from_be_bytes(pkt[8..12].try_into().ok()?);
                let aad = (((ts as u64) << 32) | ssrc as u64).to_be_bytes();
                let mut n = [0u8; 12];
                n[4..].copy_from_slice(&pkt[pkt.len() - 8..]);
                aead.decrypt(Nonce::from_slice(&n), Payload { msg: &pkt[12..pkt.len() - 8], aad: &aad })
                    .ok()
                    .map(std::borrow::Cow::Owned)
            }
        }
    }
}

/// If enabled (default; `RASCAR_CARPLAY_AUDIO=off` disables), run the audio path
/// entirely on a spawned task so it can NEVER stall the picture: the SETUP is
/// issued on the SHARED control channel (after the screen data channel and
/// encoder are already up), and the RTP sender streams for the whole session.
/// Best-effort — any failure just leaves the session without audio; the screen
/// keeps running regardless. Returns the task handle for teardown.
///
/// While a meeting or modem call runs (rascar-app says so, see `stream::call`),
/// the stream is swapped for a telephony one carrying the car's microphone;
/// while an incoming modem call rings, for an "alert" one carrying our
/// ringtone; and swapped back afterwards. `telephony` / `alert` are what
/// `/info` says the car accepts (None = the car has no such stream: that line
/// state keeps the media stream).
fn maybe_start_audio(
    rtsp: SharedRtsp,
    peer: SocketAddr,
    paired_secret: Option<Vec<u8>>,
    telephony: Option<AudioMode>,
    alert: Option<AudioMode>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !audio_enabled() {
        info!(
            "audio: NOT opening a MainAudio stream — modesChanged gives MainAudio to the \
             accessory, so asking for the stream would contradict what we just told the car \
             (RASCAR_CARPLAY_AUDIO=on to take the resource and stream it)"
        );
        return None;
    }
    let telephony = telephony.filter(|_| crate::stream::call::enabled());
    let alert = alert.filter(|_| crate::stream::call::ring_enabled());
    Some(tokio::spawn(async move {
        let mut line = crate::stream::call::subscribe();
        // A line already up when the session starts (the car reconnected
        // mid-call or mid-ring) is evaluated once the media stream runs,
        // instead of waiting for its next state change.
        line.mark_changed();
        let mut mode = AudioMode::Media;
        // A call/alert stream the car refused: not asked again while the line
        // still wants it (a refusal per heartbeat would hammer the car).
        let mut refused: Option<AudioMode> = None;
        loop {
            wait_while_car_borrows_audio().await;
            let running = open_audio_stream(&rtsp, peer, paired_secret.as_deref(), mode).await;
            let Some(running) = running else {
                if mode != AudioMode::Media {
                    // The car refused the telephony/alert stream: give it its
                    // media stream back rather than leave it silent, and say
                    // so first (ownership before SETUP).
                    warn!("audio: {mode:?} stream refused — back to media audio until the line changes");
                    let _ = send_modes_changed(&rtsp, false).await;
                    refused = Some(mode);
                    mode = AudioMode::Media;
                    continue;
                }
                return;
            };
            // Stream until the line wants another mode (or the stream dies).
            let next = tokio::select! {
                _ = running.run() => return,
                next = async {
                    loop {
                        if line.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                        let desired = desired_audio_mode(*line.borrow(), telephony, alert);
                        if refused.is_some_and(|r| r != desired) {
                            refused = None;
                        }
                        let next = if refused == Some(desired) { AudioMode::Media } else { desired };
                        if next != mode {
                            return next;
                        }
                    }
                } => next,
            };
            info!("audio: switching {mode:?} → {next:?}");
            send_audio_teardown(&rtsp).await;
            // iPhone order: announce the call, then its stream; end the stream,
            // then the call. Every switch re-states ownership before the SETUP,
            // including ring → call and alert → media.
            let _ = send_modes_changed(&rtsp, announces_phone_call(next)).await;
            mode = next;
        }
    }))
}

/// Aborts a helper task when the stream that owns it goes away.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One negotiated MainAudio stream, ready to run.
struct RunningAudio {
    sock: tokio::net::UdpSocket,
    dst: SocketAddr,
    cipher: AudioRtpCipher,
    mode: AudioMode,
    /// Telephony: the socket the car sends its microphone to.
    mic: Option<(tokio::net::UdpSocket, MicCipher)>,
    _rtcp: AbortOnDrop,
}

impl RunningAudio {
    /// Send (and, for a call, receive) until the stream dies. Dropping this
    /// future (a meeting starts/ends) stops parec and removes the car mic.
    async fn run(self) {
        let RunningAudio { sock, dst, cipher, mode, mic, _rtcp } = self;
        if let AudioMode::Alert { rate, channels, .. } = mode {
            return run_ringtone_rtp(sock, dst, cipher, rate, channels).await;
        }
        match mic {
            None => run_audio_rtp(sock, dst, cipher, mode.rate(), mode.channels()).await,
            Some((mic_sock, mic_cipher)) => {
                tokio::select! {
                    _ = run_audio_rtp(sock, dst, cipher, mode.rate(), mode.channels()) => {}
                    _ = run_car_mic(mic_sock, mic_cipher, mode.rate()) => {}
                }
            }
        }
    }
}

/// SETUP one MainAudio stream in `mode`. None = no audio (logged).
async fn open_audio_stream(rtsp: &SharedRtsp, peer: SocketAddr, paired_secret: Option<&[u8]>, mode: AudioMode) -> Option<RunningAudio> {
    let paired = paired_secret.map(|s| !s.is_empty()).unwrap_or(false);
    // scid = 0 for an unencrypted session (catplay emits 0 when cipher!=ChaCha),
    // random strictly-positive for a paired session (keys the audio ChaCha).
    let scid: i64 = if paired { random_positive_i64() } else { 0 };
    info!("→ SETUP audio ({mode:?}, type=100, streamConnectionID={scid}, {})", if paired { "chacha" } else { "unencrypted" });
    // Bind our RTCP control socket BEFORE the SETUP so we can advertise its
    // port in the request (catplay pattern); held open for the session below.
    let ubind = if peer.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let bind = |what: &'static str| async move {
        match tokio::net::UdpSocket::bind(ubind).await {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("audio: {what} bind failed: {e:#} — audio disabled");
                None
            }
        }
    };
    let rtcp_sock = bind("RTCP").await?;
    let our_control_port = rtcp_sock.local_addr().ok()?.port();
    let mic_sock = if mode.is_call() { Some(bind("microphone").await?) } else { None };
    let mic_port = match &mic_sock {
        Some(s) => Some(s.local_addr().ok()?.port()),
        None => None,
    };
    let body = match build_audio_setup(scid, our_control_port, mode, mic_port) {
        Ok(b) => b,
        Err(e) => {
            warn!("audio: marshal SETUP failed: {e:#} — audio disabled");
            return None;
        }
    };
    // Audio is OPTIONAL and must never cost us the picture. The RTSP
    // connection is shared behind a mutex, so this request holds the control
    // channel for as long as it waits — and the car may simply not answer it
    // (measured 2026-09-20 one capture run: audio SETUP went out as cseq=8,
    // was never answered, and `/feedback` then never ticked once before the
    // head unit detached 3.2 s later). The generic RTSP timeout is 10 s,
    // which is longer than an entire session, so bound this one far tighter.
    //
    // The car may also answer LATE: 3.2 s on 2026-10-01 (drive 2), after which
    // the whole session ran without audio. So on timeout we release the channel
    // and keep waiting for the reply off the lock: the RTSP client sets a late
    // reply aside by CSeq while `/feedback` reads its own.
    const AUDIO_SETUP_TIMEOUT: Duration = Duration::from_millis(1500);
    const AUDIO_SETUP_LATE_WAIT: Duration = Duration::from_secs(10);
    let (resp, cseq) = {
        let mut guard = rtsp.lock().await;
        let r = tokio::time::timeout(AUDIO_SETUP_TIMEOUT, guard.request("SETUP", "/", PLIST_CT, &body, &Vec::new())).await;
        (r, guard.last_cseq())
    };
    let resp = match resp {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!("audio: SETUP request failed: {e:#} — audio disabled");
            return None;
        }
        Err(_) => {
            warn!(
                "audio: SETUP unanswered within {AUDIO_SETUP_TIMEOUT:?} — releasing the control channel, \
                 waiting up to {AUDIO_SETUP_LATE_WAIT:?} more for a late reply (cseq={cseq})"
            );
            let deadline = tokio::time::Instant::now() + AUDIO_SETUP_LATE_WAIT;
            loop {
                if let Some(r) = rtsp.lock().await.take_late_reply(cseq) {
                    info!("audio: late SETUP reply received (cseq={cseq})");
                    break r;
                }
                if tokio::time::Instant::now() >= deadline {
                    warn!("audio: SETUP still unanswered after {AUDIO_SETUP_LATE_WAIT:?} more — audio disabled");
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    };
    if !resp.is_success() {
        warn!("audio: SETUP REJECTED HTTP {} — continuing without audio", resp.status);
        return None;
    }
    let reply = match bplist::from_bytes(&resp.body) {
        Ok(v) => v,
        Err(e) => {
            warn!("audio: SETUP reply not a plist: {e:#} — audio disabled");
            return None;
        }
    };
    let (data_port, control_port) = extract_audio_ports(&reply, AUDIO_STREAM_TYPE_MAIN);
    let Some(data_port) = data_port else {
        warn!("audio: SETUP reply carried no type-100 dataPort — audio disabled");
        return None;
    };
    info!("← audio SETUP dataPort={data_port} controlPort={control_port:?}");

    let sock = bind("RTP").await?;
    let dst = with_port(peer, data_port);
    // Hold the RTCP socket open for the whole stream: connect the reverse to
    // the car's controlPort if it gave one, and drain anything it sends so the
    // socket stays live. RTP+RTCP together = a complete audio stream (catplay).
    if let Some(cp) = control_port {
        let _ = rtcp_sock.connect(with_port(peer, cp)).await;
    }
    let rtcp = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while rtcp_sock.recv(&mut buf).await.is_ok() {}
    });
    let cipher = AudioRtpCipher::from_session(paired_secret, scid);
    let mic = mic_sock.map(|s| (s, MicCipher::from_session(paired_secret, scid)));
    Some(RunningAudio { sock, dst, cipher, mode, mic, _rtcp: AbortOnDrop(rtcp) })
}

/// `TEARDOWN /` of the MainAudio stream only (catplay `TeardownStream{type}`).
async fn send_audio_teardown(rtsp: &SharedRtsp) {
    let mut stream = plist::Dictionary::new();
    stream.insert("type".into(), AUDIO_STREAM_TYPE_MAIN.into());
    let mut d = plist::Dictionary::new();
    d.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(stream)]));
    let Ok(body) = bplist::to_bytes(&bplist::Value::Dictionary(d)) else { return };
    let mut guard = rtsp.lock().await;
    match tokio::time::timeout(Duration::from_millis(1500), guard.request("TEARDOWN", "/", PLIST_CT, &body, &Vec::new())).await {
        Ok(Ok(r)) => info!("← TEARDOWN audio HTTP {}", r.status),
        Ok(Err(e)) => warn!("audio: TEARDOWN failed: {e:#}"),
        Err(_) => warn!("audio: TEARDOWN unanswered"),
    }
}

/// Hold an audio `SETUP` while the car borrows MainAudio. Since every accepted
/// `changeModes` is followed by our `modesChanged` reporting the borrow (car =
/// current entity), a `SETUP` during the borrow would ask for a stream the car
/// was just told it holds — the contradiction the Jaguar never answers. Seen on
/// the bench 2026-09-30: a slow (software-rendered) start put our SETUP after
/// the car's borrow. Bounded, so a borrow that is never released cannot leave
/// the session silent forever.
async fn wait_while_car_borrows_audio() {
    if !accessory_borrows(RESOURCE_MAIN_AUDIO) {
        return;
    }
    info!("audio: the car borrows MainAudio — holding the audio SETUP until it gives it back");
    let start = std::time::Instant::now();
    while accessory_borrows(RESOURCE_MAIN_AUDIO) {
        if start.elapsed() > Duration::from_secs(60) {
            warn!("audio: MainAudio still borrowed after 60 s — opening the audio stream anyway");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    info!("audio: MainAudio given back after {:.1} s — opening the audio stream", start.elapsed().as_secs_f64());
}

/// `POST /command modesChanged` with the phoneCall app state set to us (a
/// meeting is running) or to nobody.
async fn send_modes_changed(rtsp: &SharedRtsp, phone_call: bool) -> Result<()> {
    // Build under the lock: the screen owner it carries must be the one in
    // force when it goes out (a hand-back may be in flight on another task).
    let mut guard = rtsp.lock().await;
    let body = build_modes_changed_body(phone_call)?;
    let r = tokio::time::timeout(Duration::from_millis(1500), guard.request("POST", "/command", PLIST_CT, &body, &Vec::new()))
        .await
        .map_err(|_| anyhow!("modesChanged unanswered"))??;
    info!("← modesChanged(phoneCall={phone_call}, screen={}) HTTP {}", screen_entity_name(screen_with_car()), r.status);
    PHONE_CALL.store(phone_call, Ordering::Relaxed);
    Ok(())
}

/// `modesChanged` with the state a car `changeModes` just left us in, borrows
/// included (catplay `ResourceController::serialize_to_state`).
///
/// catplay answers the request, then immediately sends this: the response alone
/// does not settle the transfer for the car. We only answered, and every silent
/// stretch in the car started at a `changeModes`: one parked session
/// (borrow + release at 31 s → 70 min of silence) and one drive (music
/// heard until the Borrow at 16:08:28, nothing after the release at 16:11:11,
/// although the level log shows we kept sending sound and the car kept
/// consuming it).
async fn send_state_modes_changed(rtsp: &SharedRtsp) -> Result<()> {
    let mut guard = rtsp.lock().await;
    let (screen_borrowed, audio_borrowed) =
        (accessory_borrows(RESOURCE_MAIN_SCREEN), accessory_borrows(RESOURCE_MAIN_AUDIO));
    let body = state_modes_changed_body(
        PHONE_CALL.load(Ordering::Relaxed),
        screen_with_car(),
        screen_borrowed,
        audio_borrowed,
    )?;
    let r = tokio::time::timeout(Duration::from_millis(1500), guard.request("POST", "/command", PLIST_CT, &body, &Vec::new()))
        .await
        .map_err(|_| anyhow!("modesChanged(after changeModes) unanswered"))??;
    info!(
        "← modesChanged(after changeModes: screen borrowed={screen_borrowed}, audio borrowed={audio_borrowed}) HTTP {}",
        r.status
    );
    Ok(())
}

/// Body of [`send_state_modes_changed`] for an explicit state (tests).
fn state_modes_changed_body(phone_call: bool, screen_to_car: bool, screen_borrowed: bool, audio_borrowed: bool) -> Result<Vec<u8>> {
    let mut d = plist::Dictionary::new();
    d.insert("type".into(), "modesChanged".into());
    d.insert(
        "params".into(),
        bplist::Value::Dictionary(modes_params(phone_call, screen_to_car, screen_borrowed, audio_borrowed)),
    );
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

fn screen_entity_name(to_car: bool) -> &'static str {
    if to_car { "car 2/2" } else { "phone 1/1" }
}

/// Announce the MainScreen owner: `modesChanged` with MainScreen 2/2 (the
/// car's native UI) or 1/1 (us), phoneCall and MainAudio unchanged.
///
/// Giving the screen away, [`SCREEN_TO_CAR`] is set BEFORE the request: our
/// stream is already torn down, so from here on everything must say "car",
/// whatever the car answers. Taking it back, it is cleared only once the car
/// accepted the `modesChanged`; on a failure we stay "car" and the caller must
/// not SETUP a screen stream (that is the ownership contradiction the Jaguar
/// kills sessions for).
async fn send_screen_modes_changed(rtsp: &SharedRtsp, to_car: bool) -> Result<()> {
    let mut guard = rtsp.lock().await;
    if to_car {
        SCREEN_TO_CAR.store(true, Ordering::Relaxed);
    }
    let body = modes_changed_body(PHONE_CALL.load(Ordering::Relaxed), to_car)?;
    let r = tokio::time::timeout(Duration::from_millis(1500), guard.request("POST", "/command", PLIST_CT, &body, &Vec::new()))
        .await
        .map_err(|_| anyhow!("modesChanged(screen) unanswered"))??;
    info!("← modesChanged(screen={}) HTTP {}", screen_entity_name(to_car), r.status);
    if !r.is_success() {
        bail!("modesChanged(screen={}) REJECTED: HTTP {}", screen_entity_name(to_car), r.status);
    }
    if !to_car {
        SCREEN_TO_CAR.store(false, Ordering::Relaxed);
    }
    Ok(())
}

/// The car's microphone → the "Car microphone" PulseAudio source, for as long
/// as the telephony stream lives.
async fn run_car_mic(sock: tokio::net::UdpSocket, cipher: MicCipher, rate: u32) {
    let mut mic = match crate::stream::call::CarMic::open(rate).await {
        Ok(m) => m,
        Err(e) => {
            warn!("call: cannot create the car microphone source: {e:#} — the car still gets the meeting audio");
            // Keep the stream alive (the car may need it read) without a sink.
            let mut buf = [0u8; 2048];
            while sock.recv(&mut buf).await.is_ok() {}
            return;
        }
    };
    let (mut got, mut bad) = (0u64, 0u64);
    let mut buf = [0u8; 4096];
    loop {
        let n = match tokio::time::timeout(Duration::from_secs(5), sock.recv(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                warn!("call: mic recv: {e:#}");
                break;
            }
            Err(_) => {
                if got == 0 {
                    warn!("call: no microphone packet from the car yet (5 s)");
                }
                continue;
            }
        };
        match cipher.open(&buf[..n]) {
            Some(pcm) => {
                got += 1;
                if got == 1 {
                    info!("call: first microphone packet from the car ({} bytes PCM)", pcm.len());
                }
                if let Err(e) = mic.write(&pcm).await {
                    warn!("call: mic write: {e:#}");
                    break;
                }
            }
            None => {
                bad += 1;
                if bad == 1 || bad % 500 == 0 {
                    warn!("call: {bad} microphone packet(s) failed to decrypt/parse");
                }
            }
        }
    }
    // `mic` drops here: that removes the source and restores the default.
    drop(mic);
}

/// How often the send loop logs the level of the PCM it sends.
const AUDIO_LEVEL_EVERY: Duration = Duration::from_secs(10);

/// RMS / peak of 16-bit big-endian PCM (CarPlay's byte order) since the last report.
struct PcmLevel {
    since: Instant,
    sum_sq: f64,
    count: u64,
    peak: i32,
}

impl PcmLevel {
    fn new() -> Self {
        Self { since: Instant::now(), sum_sq: 0.0, count: 0, peak: 0 }
    }

    fn add(&mut self, pcm_be: &[u8]) {
        for b in pcm_be.chunks_exact(2) {
            let v = i16::from_be_bytes([b[0], b[1]]) as i32;
            self.sum_sq += (v * v) as f64;
            self.count += 1;
            self.peak = self.peak.max(v.abs());
        }
    }

    fn rms_peak(&self) -> (u32, i32) {
        let rms = if self.count == 0 { 0.0 } else { (self.sum_sq / self.count as f64).sqrt() };
        (rms.round() as u32, self.peak)
    }

    /// `(rms, peak)` and a reset, once `every` has passed since the last report.
    fn take_if_older(&mut self, every: Duration) -> Option<(u32, i32)> {
        if self.since.elapsed() < every {
            return None;
        }
        let r = self.rms_peak();
        *self = Self::new();
        Some(r)
    }
}

/// The alert stream: our generated ringtone, looped at the RTP packet cadence,
/// until the line stops ringing (the caller drops this future). Not parec: the
/// dashboard's own audio is irrelevant while ringing and the tone must not
/// depend on PulseAudio being healthy.
async fn run_ringtone_rtp(sock: tokio::net::UdpSocket, dst: SocketAddr, mut cipher: AudioRtpCipher, rate: u32, channels: u32) {
    let packet_bytes = AUDIO_FRAMES_PER_PACKET as usize * channels as usize * 2;
    let packet_period = Duration::from_micros(AUDIO_FRAMES_PER_PACKET as u64 * 1_000_000 / rate as u64);
    let tone = crate::stream::call::ringtone_cycle(rate, channels);
    info!("audio: ringing — ringtone {rate} Hz {channels}ch → {dst} (cipher={})", cipher.name());
    let (mut seq, mut ts, mut at) = (0u16, 0u32, 0usize);
    let mut pkt_pcm = vec![0u8; packet_bytes];
    let mut tick = tokio::time::interval(packet_period);
    loop {
        tick.tick().await;
        // Copy one packet out of the cycle, wrapping around its end.
        for b in pkt_pcm.iter_mut() {
            *b = tone[at];
            at = (at + 1) % tone.len();
        }
        let pkt = cipher.build_packet(seq, ts, 0, &pkt_pcm);
        if let Err(e) = sock.send_to(&pkt, dst).await {
            warn!("audio: ringtone send_to {dst} failed: {e:#} — stopping");
            return;
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(AUDIO_FRAMES_PER_PACKET);
    }
}

/// Stream audio RTP to the car for the whole session. Tries live PCM capture
/// (`parec`); if capture is unavailable or ends, it falls back to CONTINUOUS
/// SILENCE so the negotiated stream stays live — a complete CarPlay session has
/// audio *data flowing*, not just an open port, and the car's ~7.8 s completion
/// watchdog appears to require it. Every payload is framed + encrypted by
/// `cipher` to match the session encryption.
async fn run_audio_rtp(sock: tokio::net::UdpSocket, dst: SocketAddr, mut cipher: AudioRtpCipher, rate: u32, channels: u32) {
    // 16-bit samples.
    let packet_bytes = AUDIO_FRAMES_PER_PACKET as usize * channels as usize * 2;
    let packet_period = Duration::from_micros(AUDIO_FRAMES_PER_PACKET as u64 * 1_000_000 / rate as u64);
    let ssrc: u32 = 0;
    let mut seq: u16 = 0;
    let mut ts: u32 = 0;
    let mut sent: u64 = 0;
    // What we actually send: music vs silence is invisible otherwise.
    let mut level = PcmLevel::new();

    let device = std::env::var("RASCAR_AUDIO_SOURCE").ok().filter(|s| !s.is_empty());
    let silence = vec![0u8; packet_bytes];
    // Car drive 2026-09-28: capture died 2 ms after starting and the session
    // then sent silence for the whole trip. Now silence only bridges the gap
    // while capture is (re)started, retried every few seconds.
    const RETRY: Duration = Duration::from_secs(3);
    let mut failures: u32 = 0;
    loop {
        // parec resamples the Pi's output to what the stream carries.
        let mut capture = AudioCapture::new(rate, channels);
        match capture.start(device.as_deref()).await {
            Ok(()) => {
                info!("audio: streaming PCM {rate} Hz {channels}ch → {dst} (cipher={}, PT={AUDIO_PAYLOAD_TYPE})", cipher.name());
                let started = Instant::now();
                let mut last_data = Instant::now();
                let mut pcm: Vec<u8> = Vec::with_capacity(packet_bytes * 2);
                let mut read_buf = vec![0u8; packet_bytes * 2];
                loop {
                    // While nothing plays, PulseAudio suspends the output and its
                    // monitor delivers no data at all (not zeros): once nothing has
                    // come for 200 ms, fill with silence at the packet cadence
                    // (5 packets ≈ 37 ms ≈ the read timeout) so the car never sees
                    // a stalled stream. Normal gaps between parec chunks (20 ms
                    // latency) never reach that, so music is never padded.
                    match tokio::time::timeout(packet_period * 5, capture.read_chunk(&mut read_buf)).await {
                        Err(_) => {
                            if last_data.elapsed() > Duration::from_millis(200) {
                                pcm.extend(std::iter::repeat_n(0u8, packet_bytes * 5));
                            }
                        }
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            last_data = Instant::now();
                            pcm.extend_from_slice(&read_buf[..n]);
                        }
                        Ok(Err(e)) => {
                            warn!("audio: capture read error: {e:#}");
                            break;
                        }
                    }
                    while pcm.len() >= packet_bytes {
                        level.add(&pcm[..packet_bytes]);
                        if let Some((rms, peak)) = level.take_if_older(AUDIO_LEVEL_EVERY) {
                            info!("audio: level sent to the car over the last {}s: rms={rms} peak={peak} (packets={sent})", AUDIO_LEVEL_EVERY.as_secs());
                        }
                        let pkt = cipher.build_packet(seq, ts, ssrc, &pcm[..packet_bytes]);
                        if let Err(e) = sock.send_to(&pkt, dst).await {
                            warn!("audio: send_to {dst} failed: {e:#} — stopping");
                            capture.stop().await;
                            return;
                        }
                        seq = seq.wrapping_add(1);
                        ts = ts.wrapping_add(AUDIO_FRAMES_PER_PACKET);
                        sent += 1;
                        if crate::debug::sampled(sent - 1, 3, 300) {
                            debug!("audio: RTP packet #{sent} (seq={seq}, ts={ts})");
                        }
                        pcm.drain(..packet_bytes);
                    }
                }
                capture.stop().await;
                let lived = started.elapsed();
                // An immediate EOF is a failure (see parec's own message above), not an end.
                if lived < Duration::from_secs(1) {
                    failures += 1;
                    warn!("audio: capture ended after {} ms (failure #{failures}) — silence, retry in {}s", lived.as_millis(), RETRY.as_secs());
                } else {
                    failures = 0;
                    info!("audio: capture ended after {:.0}s — silence, retry in {}s", lived.as_secs_f32(), RETRY.as_secs());
                }
            }
            Err(e) => {
                failures += 1;
                if failures <= 3 || failures % 20 == 0 {
                    warn!("audio: capture start failed (#{failures}): {e:#} — silence, retry in {}s", RETRY.as_secs());
                }
            }
        }
        // Keep the car's stream fed while waiting: zeroed PCM at the packet
        // cadence (352 frames: 7.333 ms at 48 kHz, 22 ms at 16 kHz).
        let mut tick = tokio::time::interval(packet_period);
        let until = Instant::now() + RETRY;
        while Instant::now() < until {
            tick.tick().await;
            let pkt = cipher.build_packet(seq, ts, ssrc, &silence);
            if let Err(e) = sock.send_to(&pkt, dst).await {
                warn!("audio: silence send_to {dst} failed: {e:#} — stopping");
                return;
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(AUDIO_FRAMES_PER_PACKET);
            sent += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// H.264 → catplay screen payloads
// ---------------------------------------------------------------------------

/// Build the `avcC` AVCDecoderConfigurationRecord from raw SPS + PPS, using the
/// minimal ISO/IEC 14496-15 layout (no length-inflating trailer):
/// `[0x01, sps[1], sps[2], sps[3], 0xFF, 0xE1, sps_len_be16, sps, 0x01, pps_len_be16, pps]`.
fn build_avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    if sps.len() < 4 {
        bail!(
            "SPS too short to build avcC: {} bytes, need >= 4 (profile/compat/level live at sps[1..4])",
            sps.len()
        );
    }
    let mut v = Vec::with_capacity(11 + sps.len() + pps.len());
    v.push(0x01); // configurationVersion
    v.push(sps[1]); // AVCProfileIndication
    v.push(sps[2]); // profile_compatibility
    v.push(sps[3]); // AVCLevelIndication
    v.push(0xFF); // 6 reserved bits + lengthSizeMinusOne = 3 (4-byte NAL lengths)
    v.push(0xE1); // 3 reserved bits + numOfSequenceParameterSets = 1
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(0x01); // numOfPictureParameterSets = 1
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    Ok(v)
}

/// Length-prefix a set of already-split NALs into AVCC (4-byte BE length) form.
fn nals_to_avcc(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Convert a raw Annex-B buffer (start codes `00 00 01` / `00 00 00 01`) into
/// AVCC length-prefixed form. Standalone/tested; the streaming loop uses the
/// incremental [`NalParser`] + [`nals_to_avcc`] instead.
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    // marks: (start_code_begin, nal_begin)
    let mut marks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 <= n {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            marks.push((i, i + 3));
            i += 3;
            continue;
        }
        if i + 4 <= n && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            marks.push((i, i + 4));
            i += 4;
            continue;
        }
        i += 1;
    }
    let mut out = Vec::new();
    for k in 0..marks.len() {
        let nal_begin = marks[k].1;
        let nal_end = if k + 1 < marks.len() { marks[k + 1].0 } else { n };
        let nal = &data[nal_begin..nal_end];
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Assembles Annex-B NALs into access units. Emits the PREVIOUS access unit when
/// the incoming NAL begins a new one (a new first-slice VCL, a parameter set /
/// SEI after VCL, or an AUD boundary).
struct AuAssembler {
    cur: Vec<Vec<u8>>,
    have_vcl: bool,
}

impl AuAssembler {
    fn new() -> Self {
        Self { cur: Vec::new(), have_vcl: false }
    }

    /// Offer one NAL; return a completed access unit if this NAL starts a new one.
    fn offer(&mut self, nal: Vec<u8>) -> Option<Vec<Vec<u8>>> {
        let t = nal_type(&nal);
        // AUD (type 9): access-unit delimiter — a boundary marker we do not carry.
        if t == 9 {
            return if self.have_vcl {
                self.have_vcl = false;
                Some(std::mem::take(&mut self.cur))
            } else {
                self.cur.clear();
                None
            };
        }
        let is_vcl = (1..=5).contains(&t);
        let mut done = None;
        if is_vcl {
            if self.have_vcl && is_first_slice(&nal) {
                done = Some(std::mem::take(&mut self.cur));
                self.have_vcl = false;
            }
            self.cur.push(nal);
            self.have_vcl = true;
        } else {
            // Non-VCL (SPS 7 / PPS 8 / SEI 6): a new parameter set after a VCL
            // slice begins the next access unit.
            if self.have_vcl {
                done = Some(std::mem::take(&mut self.cur));
                self.have_vcl = false;
            }
            self.cur.push(nal);
        }
        done
    }

    /// Flush the trailing access unit at EOF (only if it carries a VCL slice).
    fn finish(&mut self) -> Option<Vec<Vec<u8>>> {
        if self.have_vcl && !self.cur.is_empty() {
            self.have_vcl = false;
            Some(std::mem::take(&mut self.cur))
        } else {
            None
        }
    }
}

/// VideoFrame presentation-timestamp clock. `params[0]` is the monotonic elapsed
/// time encoded as an NTP 32.32 fixed-point value, NOT raw nanoseconds.
/// The car ignores it unless RespectTimestamps is set (we don't), but the fixed
/// point form is the correct wire value.
struct Clock {
    start: Instant,
}
impl Clock {
    fn new() -> Self {
        Self { start: Instant::now() }
    }
    fn now(&self) -> u64 {
        crate::ntp_time::encode(self.start.elapsed())
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the whole legacy CarPlay screen session against a discovered endpoint.
///
/// The iAP2 link is serviced elsewhere (the caller keeps it alive concurrently),
/// so this owns only the AirPlay side. Returns when the video encoder ends
/// (EOF / write failure) or a handshake step fails.
/// Candidate car addresses to race for the AirPlay `:7000` server: the mDNS
/// address, the car's IPv4 (`.1` of our NCM /24), and every IPv6 neighbour on the
/// NCM link (the car's link-locals). All scoped to the discovering interface.
fn car_candidates(endpoint: &AirPlayEndpoint) -> Vec<std::net::SocketAddr> {
    let iface = &endpoint.interface;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut add = |addr: &str, port: u16, iface: &str| {
        if let Ok(sa) = RtspClient::resolve(addr, port, iface) {
            if seen.insert(sa) {
                out.push(sa);
            }
        }
    };
    // 1. The mDNS-advertised address first.
    add(&endpoint.address, endpoint.port, iface);
    // 2. The car's IPv4 (DHCP server / .1). Derive from our own v4 on the iface.
    if let Ok(o) = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .output()
    {
        let text = String::from_utf8_lossy(&o.stdout);
        if let Some(cidr) = text.split_whitespace().skip_while(|w| *w != "inet").nth(1) {
            if let Some(ip) = cidr.split('/').next() {
                let mut parts: Vec<&str> = ip.split('.').collect();
                if parts.len() == 4 {
                    parts[3] = "1";
                    add(&parts.join("."), endpoint.port, "");
                }
            }
        }
    }
    // 3. Every IPv6 neighbour on the NCM link (the car's link-locals).
    if let Ok(o) = std::process::Command::new("ip")
        .args(["-6", "neigh", "show", "dev", iface])
        .output()
    {
        let text = String::from_utf8_lossy(&o.stdout);
        for w in text.split_whitespace() {
            if w.starts_with("fe80") {
                add(w, endpoint.port, iface);
            }
        }
    }
    out
}

/// Race a TCP connect to `:7000` across all candidate car addresses; keep the
/// first that succeeds. Fixes "clean handshake but the dial to the mDNS address
/// hangs because the AirPlay server is on a different car interface".
async fn connect_best(endpoint: &AirPlayEndpoint) -> Result<RtspClient> {
    let candidates = car_candidates(endpoint);
    info!("CarPlay: racing {} candidate address(es) to :{}: {:?}",
          candidates.len(), endpoint.port, candidates);
    if candidates.is_empty() {
        return RtspClient::connect(endpoint).await.context("CarPlay: RTSP TCP connect (no candidates)");
    }
    let mut set = tokio::task::JoinSet::new();
    for addr in candidates {
        set.spawn(async move {
            let r = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect(addr),
            )
            .await;
            (addr, r)
        });
    }
    let mut last_err = String::from("no candidate connected");
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((addr, Ok(Ok(stream)))) => {
                info!("CarPlay: connected to {addr} (won the race)");
                set.abort_all();
                return Ok(RtspClient::from_stream(stream, addr));
            }
            Ok((addr, Ok(Err(e)))) => last_err = format!("{addr}: {e}"),
            Ok((addr, Err(_))) => last_err = format!("{addr}: timed out"),
            Err(e) => last_err = format!("join error: {e}"),
        }
    }
    anyhow::bail!("CarPlay: no candidate address accepted :{} ({last_err})", endpoint.port)
}

/// CarPlay MFiSAP `/auth-setup` (client/phone side).
///
/// Wire format:
///   request  = `0x01` (version) followed by our X25519 public key: 33 bytes
///   response = accessory X25519 public key (32 bytes), then the accessory's MFi
///              certificate and its signature, each prefixed by a big-endian
///              u32 length, with nothing after the signature
///
/// The accessory (car) signs over both public keys with its MFi coprocessor and
/// returns its certificate; the phone side only performs the X25519 exchange
/// (and, for audio, would decrypt the signature with an AES-128-CTR key derived
/// from the shared secret). The certificate is not checked against Apple's CA,
/// so no MFi chip is needed on our side. Returns the raw X25519 shared secret
/// (it keys the audio stream; performing the exchange is the point, see the
/// call site).
async fn auth_setup(rtsp: &mut RtspClient) -> Result<[u8; 32]> {
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

    let client_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let hello = sap_hello(XPublicKey::from(&client_secret).as_bytes());

    info!("→ POST /auth-setup (MFiSAP m1: X25519 pubkey)");
    let resp = rtsp
        .request("POST", "/auth-setup", "application/octet-stream", &hello, &Vec::new())
        .await
        .context("POST /auth-setup")?;
    if !resp.is_success() {
        bail!("/auth-setup rejected: HTTP {}", resp.status);
    }

    let reply = SapReply::parse(&resp.body).context("/auth-setup reply")?;
    let shared = client_secret.diffie_hellman(&XPublicKey::from(reply.accessory_key));
    info!(
        "← /auth-setup m2: accessory pub 32B, cert {}B, sig {}B → MFiSAP shared secret derived",
        reply.certificate.len(),
        reply.signature.len()
    );
    Ok(*shared.as_bytes())
}

/// MFiSAP request body: version byte 1, then our public key.
fn sap_hello(our_key: &[u8; 32]) -> [u8; 33] {
    let mut hello = [0u8; 33];
    hello[0] = 0x01;
    hello[1..].copy_from_slice(our_key);
    hello
}

/// The accessory's MFiSAP reply, borrowed from the response body.
#[derive(Debug)]
struct SapReply<'a> {
    accessory_key: [u8; 32],
    certificate: &'a [u8],
    signature: &'a [u8],
}

impl<'a> SapReply<'a> {
    fn parse(body: &'a [u8]) -> Result<Self> {
        let (key, mut rest) = body
            .split_first_chunk::<32>()
            .with_context(|| format!("{} bytes, too short for the accessory key", body.len()))?;
        let certificate = take_length_prefixed(&mut rest)
            .with_context(|| format!("certificate does not fit in the {}-byte reply", body.len()))?;
        let signature = take_length_prefixed(&mut rest)
            .with_context(|| format!("signature does not fit in the {}-byte reply", body.len()))?;
        if !rest.is_empty() {
            bail!("{} unexpected bytes after the signature", rest.len());
        }
        Ok(Self { accessory_key: *key, certificate, signature })
    }
}

/// Split a `u32 BE length ++ bytes` field off the front of `input`.
fn take_length_prefixed<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (len, tail) = input.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
    if len > tail.len() {
        return None;
    }
    let (field, after) = tail.split_at(len);
    *input = after;
    Some(field)
}

pub async fn run_carplay(endpoint: &AirPlayEndpoint) -> Result<()> {
    // The reference transmitter never dials the car's AirPlay receiver on its own:
    // it waits in `WaitingForInvite` until the car sends `GET /ctrl-int/1/connect`
    // on the `_carplay-ctrl` port, and the invite's `AirPlay-Receiver-Device-ID`
    // picks which receiver to dial. We connect as soon as discovery finds one. If
    // the car never invites us, its own session manager has no session object —
    // which would explain, all at once, why it never sends `requestUI`, never
    // opens any TCP connection to us, and tears the link down on a timer anchored
    // to enumeration rather than to anything we stream. So state it in the log.
    //
    // `RASCAR_WAIT_FOR_INVITE_MS` gives the car that long to invite us before we
    // dial anyway (0 = dial immediately, the behaviour we have always had). It is
    // a knob rather than a default because the car's whole session budget is only
    // ~5 s from the network coming up: waiting is only worth it if the invite
    // actually arrives, which is precisely what this measures.
    let wait_ms: u64 = std::env::var("RASCAR_WAIT_FOR_INVITE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if wait_ms > 0 && crate::host::ctrl_advertise::invites_received() == 0 {
        info!("→ waiting up to {wait_ms} ms for the car's /ctrl-int/1/connect invite before dialling");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        while std::time::Instant::now() < deadline
            && crate::host::ctrl_advertise::invites_received() == 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    let invites = crate::host::ctrl_advertise::invites_received();
    if invites == 0 {
        warn!(
            "→ CarPlay connect UNINVITED: the car has sent no /ctrl-int/1/connect invite; \
             the reference transmitter would still be waiting"
        );
    } else {
        info!("→ CarPlay connect (invited: {invites} control-channel invite(s) received)");
    }
    info!("→ CarPlay connect [{}]:{}", endpoint.address, endpoint.port);
    // The car's AirPlay server listens on ONE of its several addresses, and the
    // address mDNS advertises is not always it (measured: mDNS AAAA + v4 both
    // refuse, a different car link-local accepts). Race :7000 across every
    // candidate — the mDNS address, the car's v4 (.1 of our /24), and every IPv6
    // neighbour on the NCM link — and keep the first that connects.
    let mut rtsp = connect_best(endpoint).await?;
    rtsp.set_user_agent(USER_AGENT);
    let peer = rtsp.peer();
    info!("← CarPlay connected to {peer} (UA={USER_AGENT})");
    // Anchor for the session postmortem at the end of this function.
    let session_started = Instant::now();

    // A UDP timing port to advertise, with an active RTCP time-sync responder:
    // the head unit pings this port (PT 0xD2) and gates the screen SETUP on
    // getting 0xD3 answers, so it MUST be served, not merely bound.
    let timing_sock = tokio::net::UdpSocket::bind("[::]:0")
        .await
        .context("bind UDP timing port")?;
    let timing_port = timing_sock.local_addr().context("timing socket local_addr")?.port();
    let timing_task = spawn_timing_responder(timing_sock);

    // --- 1. SETUP /setup (et=NONE) ---
    info!("→ SETUP /setup (timingPort={timing_port}, et=NONE)");
    let setup_body = build_initial_setup(timing_port, false).context("marshal InitialSetup plist")?;
    let resp = rtsp
        .request("SETUP", "/setup", PLIST_CT, &setup_body, &Vec::new())
        .await
        .context("SETUP /setup")?;

    // A 4xx here is NOT, by itself, evidence that the receiver demands HomeKit
    // pairing. In Apple's receiver a `466 Key Management Error` has exactly one
    // source — the `et` value not being `kAirPlayEncryptionType_None` (= 1), which
    // is what we used to send (0). See `encryption_type_none()`. The fallback below
    // is kept as a genuine fallback for receivers that really do require pairing,
    // but it must no longer be read as a diagnosis of one. On a 4xx we do NOT bail — we run CarPlay PIN pair-setup +
    // pair-verify (PIN "3939"), which leaves the RTSP control channel
    // ChaCha20-Poly1305-encrypted, then re-send SETUP with `et` omitted so the
    // screen stream is ChaCha-encrypted too. The `shared_secret` returned keys the
    // DataStream screen cipher.
    let (setup_reply, paired_secret): (bplist::Value, Option<Vec<u8>>) = if resp.is_success() {
        let reply = bplist::from_bytes(&resp.body).context("parse SETUP /setup reply plist")?;
        (reply, None)
    } else if (400..500).contains(&resp.status) {
        warn!(
            "← SETUP /setup rejected with HTTP {} (466 = Key Management Error, normally a bad \
             `et`; we sent et={}) — falling back to pair-setup(PIN {CARPLAY_PIN}) + pair-verify",
            resp.status,
            encryption_type_none()
        );
        // Measured at the Jaguar F-PACE: after it answers SETUP with 466 it STOPS
        // servicing that TCP socket — it never ACKs the following pair-setup POST
        // (we retransmit for ~14 s), then FINs. HomeKit pairing must therefore run
        // on a FRESH connection. Reconnect to the same peer that just answered.
        let peer = rtsp.peer();
        info!("→ reconnecting to {peer} for pair-setup (car abandons the 4xx'd socket)");
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::TcpStream::connect(peer),
        )
        .await
        .context("reconnect for pairing: timed out")?
        .context("reconnect for pairing")?;
        rtsp = RtspClient::from_stream(stream, peer);
        rtsp.set_user_agent(USER_AGENT);
        // Persistent identity: pair-SETUP once, then pair-VERIFY on every reconnect.
        // Re-running pair-setup each time makes the head unit accrete controller
        // entries and eventually refuse with "Connexion impossible".
        let mut pairing = Pairing::load_or_create(hap_store_path());
        let pair = if pairing.stored_accessory_ed_pub().is_some() {
            match pairing.pair_verify_pin(&mut rtsp).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        "pair-verify with the stored identity failed ({e:#}) — the car may have \
                         dropped the pairing; discarding it and running a fresh pair-setup"
                    );
                    pairing.forget_accessory();
                    // pair-verify may have consumed/soured the socket → fresh one.
                    let stream = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        tokio::net::TcpStream::connect(peer),
                    )
                    .await
                    .context("reconnect for pair-setup: timed out")?
                    .context("reconnect for pair-setup")?;
                    rtsp = RtspClient::from_stream(stream, peer);
                    rtsp.set_user_agent(USER_AGENT);
                    pairing
                        .pair_pin(&mut rtsp, CARPLAY_PIN)
                        .await
                        .context("CarPlay HomeKit pair-setup (after pair-verify failed)")?
                }
            }
        } else {
            pairing
                .pair_pin(&mut rtsp, CARPLAY_PIN)
                .await
                .context("CarPlay HomeKit pairing (pair-setup + pair-verify)")?
        };
        info!(
            "← pairing complete (channel_encrypted={}, shared_secret {} bytes)",
            pair.channel_encrypted,
            pair.shared_secret.len()
        );
        // MFiSAP `/auth-setup` — the step a real iPhone (and catplay's proven TX)
        // runs BETWEEN pair-verify and the encrypted SETUP. The phone side is pure
        // software (X25519 + verify the accessory's cert/sig): the MFi coprocessor
        // lives on the *accessory* (car), same accessory→device direction as iAP2
        // auth, so no chip is needed here. We skipped it entirely until now, and a
        // head unit that expects the exchange can silently watchdog-teardown the
        // USB/iAP2 session a few seconds after the AV session comes up even while
        // it is receiving correct video (the observed fixed ~7.7 s Jaguar drop).
        // Non-fatal: a receiver that does not require it answers 4xx and we carry
        // on (we streamed without it before), so this can only add, never regress.
        // /auth-setup (MFiSAP): the Jaguar answers this 403 (it does NOT want it),
        // and the round-trip burns ~0.7s inside the car's tight ~7.8s-from-enum
        // teardown budget — pure waste. OFF by default; opt in with
        // RASCAR_AUTH_SETUP=1 for a receiver that actually requires it.
        if std::env::var("RASCAR_AUTH_SETUP").ok().as_deref() == Some("1") {
            match auth_setup(&mut rtsp).await {
                Ok(_secret) => info!("← /auth-setup (MFiSAP) complete"),
                Err(e) => warn!(
                    "/auth-setup failed ({e:#}) — continuing without MFiSAP \
                     (this receiver may not require it)"
                ),
            }
        } else {
            info!("/auth-setup skipped (RASCAR_AUTH_SETUP!=1; the Jaguar 403s it and it wastes the pre-teardown budget)");
        }
        info!("→ re-sending SETUP /setup on the encrypted control channel");
        // Re-send SETUP on the now-encrypted control channel with `et` omitted
        // (catplay `do_initial_setup` sets `req.et = None` when a pairing shared
        // secret exists → ChaCha stream encryption).
        let setup_body2 =
            build_initial_setup(timing_port, true).context("marshal encrypted InitialSetup plist")?;
        let resp2 = rtsp
            .request("SETUP", "/setup", PLIST_CT, &setup_body2, &Vec::new())
            .await
            .context("SETUP /setup (post-pair, encrypted)")?;
        if !resp2.is_success() {
            bail!("← SETUP /setup (post-pair) REJECTED: HTTP {}", resp2.status);
        }
        let reply =
            bplist::from_bytes(&resp2.body).context("parse post-pair SETUP /setup reply plist")?;
        (reply, Some(pair.shared_secret))
    } else {
        bail!("← SETUP /setup REJECTED: HTTP {}", resp.status);
    };
    let event_port = setup_reply
        .as_dictionary()
        .and_then(|d| plist_u16(d, "eventPort"))
        .context("SETUP /setup reply carried no eventPort")?;
    // keepAlivePort is optional; when present the car expects a UDP keepalive.
    let keep_alive_port = setup_reply.as_dictionary().and_then(|d| plist_u16(d, "keepAlivePort"));
    info!("← eventPort={event_port} keepAlivePort={keep_alive_port:?}");
    // Start the UDP keepalive HERE, the moment the car tells us the port —
    // not after RECORD + modesChanged + screen SETUP + the data-channel connect,
    // which cost about a second of a session budget measured at roughly five.
    // The reference transmitter creates this socket during bootstrap, right after
    // /info, and pings immediately, then periodically.
    let keepalive_task = keep_alive_port.map(|port| {
        info!("keepalive: pinging keepAlivePort={port} every {:?}", keepalive_interval());
        spawn_keepalive(peer, port)
    });

    // --- 2. GET /info ---
    info!("→ GET /info");
    let info_body = bplist::to_bytes(&bplist::Value::Dictionary(Default::default()))
        .context("marshal InfoMessage plist")?;
    let resp = rtsp
        .request("GET", "/info", PLIST_CT, &info_body, &Vec::new())
        .await
        .context("GET /info")?;
    if !resp.is_success() {
        bail!("← GET /info REJECTED: HTTP {}", resp.status);
    }
    let (width, height, display_uuid) = parse_info_display(&resp.body);
    let hid_devices = parse_hid_devices(&resp.body);
    let hid_uuids: Vec<&str> = hid_devices.iter().map(|d| d.uuid.as_str()).collect();
    info!("← /info: display {width}x{height} uuid={display_uuid} hidDevices={hid_uuids:?}");
    log_hid_devices(&hid_devices);
    // The car states in `/info.modes` who currently owns MainScreen / MainAudio and
    // under what constraints. Logged in full; the MainScreen Take is also used by
    // step 5 (initial screen sync).
    log_info_modes(&resp.body);
    let info_screen_take = info_screen_take_constraint(&resp.body);
    let telephony = telephony_mode_from_info(&resp.body);
    info!("← /info telephony: {telephony:?} (meetings use the car microphone when Some)");
    let alert = alert_mode_from_info(&resp.body);
    info!("← /info alert: {alert:?} (incoming modem calls ring on it when Some)");
    // Which keepalive form refreshes the receiver's screen-activity timestamp.
    // A zero-body frame is rejected before opcode dispatch on at least the Harman
    // MHI2Q receiver, so getting this wrong silently starves the watchdog while
    // video still flows — see `keepalive_stats_header`.
    let stats_as_body = plist_bool(&resp.body, "keepAliveSendStatsAsBody");
    info!(
        "← /info: keepAliveSendStatsAsBody={stats_as_body} → screen keepalive = {}",
        if stats_as_body { "opcode 5 + 42 B empty-dict body" } else { "legacy opcode 2, empty" }
    );

    // --- 3. Event channel: connect and serve `POST /command` ---
    // The car pushes requestUI / changeModes / forceKeyFrame / hidSendReport on
    // this channel and expects an RTSP reply to each. The responder runs for the
    // whole session; it signals requestUI (below), forwards touch to the injector,
    // and flags forceKeyFrame to the encoder loop.
    let event_addr = with_port(peer, event_port);
    info!("→ event channel connect {event_addr}");
    let event_conn = TcpStream::connect(event_addr)
        .await
        .with_context(|| format!("connect event channel {event_addr}"))?;
    event_conn.set_nodelay(true).ok();

    // `notify_one` stores a permit, so a requestUI that arrives before we await
    // below is not lost (unlike `notify_waiters`).
    let requestui = Arc::new(Notify::new());
    let force_kf = Arc::new(AtomicBool::new(false));
    let modes_dirty = Arc::new(Notify::new());
    let (touch_tx, touch_rx) = mpsc::unbounded_channel::<HidTouch>();
    // On a HAP-paired session the event channel is ChaCha20-encrypted with the
    // Events keys — without this the car's `requestUI` (the signal to actually
    // start displaying) arrives as ciphertext we can't parse, so the screen never
    // comes up and the car resets. Derive the Events cipher from the pair-verify
    // shared secret (None on a legacy plaintext session).
    let event_crypto = paired_secret.as_ref().map(|s| EventCrypto::from_shared(s));
    let event_task = {
        let requestui = requestui.clone();
        let force_kf = force_kf.clone();
        let modes_dirty = modes_dirty.clone();
        tokio::spawn(async move {
            run_event_responder(event_conn, event_crypto, requestui, touch_tx, force_kf, modes_dirty, hid_devices).await
        })
    };

    // Give the car a brief grace to send requestUI, then proceed regardless.
    // The car tears the USB CarPlay session down at a fixed ~7.8 s from enumeration,
    // and we only reach here at ~t+5.3 s; a 3 s wait here was guillotined mid-wait
    // and never reached RECORD. requestUI often arrives only AFTER RECORD anyway,
    // and `run_event_responder` keeps handling it (and touch / forceKeyFrame)
    // asynchronously, so there is no need to block the handshake on it.
    match tokio::time::timeout(Duration::from_millis(150), requestui.notified()).await {
        Ok(()) => info!("requestUI received — proceeding"),
        Err(_) => info!("event channel: no requestUI within 150 ms — proceeding to RECORD"),
    }

    // --- 4. RECORD / ---
    info!("→ RECORD");
    let resp = rtsp
        .request("RECORD", "/", "", &[], &Vec::new())
        .await
        .context("RECORD /")?;
    if !resp.is_success() {
        bail!("← RECORD REJECTED: HTTP {}", resp.status);
    }
    info!("← RECORD OK");

    // --- 5. POST /command modesChanged (full body) ---
    // The car must learn the phone owns MainScreen (the gate for the screen
    // SETUP). A body with no `params` is rejected by a strict car, so send the
    // full modesChanged and treat a non-2xx as fatal.
    info!("→ POST /command modesChanged");
    PHONE_CALL.store(false, Ordering::Relaxed);
    // Every session starts with the dashboard on the car screen, whatever a
    // previous session in this process left behind.
    SCREEN_TO_CAR.store(false, Ordering::Relaxed);
    crate::stream::screen::request(crate::stream::screen::ScreenOwner::Phone);
    let mut screen_owner = crate::stream::screen::subscribe();
    // Initial screen sync. When the car's `/info.modes` says its native UI TAKES
    // MainScreen (Anytime), first echo that state (screen 2/2), then take the
    // screen (1/1) with the usual modesChanged below: the car then sees a
    // transfer car → phone, as from an iPhone. Apple CarPlay spec §23.3.4.1: the
    // Apple device "interprets this initial state as a resource transfer to the
    // accessory and will only then evaluate whether any mode changes are
    // required"; catplay does exactly this (`ModesArbiter::new(&info.modes)` →
    // first flush = car's state, then `try_steal_screen(NiceToHave)` → second
    // modesChanged + SETUP, `car_manager.rs:242-248`, `resource_manager.rs:257-263`).
    //
    // Sending only 1/1 left the Jaguar on its own UI at every start of
    // 2026-10-05 (5 sessions): it accepted the modesChanged (HTTP 200) but
    // showed CarPlay only when the driver pressed the CarPlay icon, and that
    // press sent no requestUI (the car already counted the screen as ours).
    // When the car starts in a Borrow (its start-up screen) this does not apply:
    // its later Release returns the screen to us and it switches by itself.
    //
    // SCREEN_TO_CAR is deliberately NOT set: no stream exists yet, and a
    // concurrent ChangeModesResponse must keep stating the screen as ours.
    if initial_screen_sync(info_screen_take) {
        info!("→ POST /command modesChanged (initial sync: MainScreen is the car's, as /info says)");
        let sync_body = modes_changed_body(false, true).context("marshal initial-sync modesChanged plist")?;
        let resp = rtsp
            .request("POST", "/command", PLIST_CT, &sync_body, &Vec::new())
            .await
            .context("POST /command modesChanged (initial sync)")?;
        if resp.is_success() {
            info!("← modesChanged(initial sync: screen=car 2/2) HTTP {} — now taking the screen", resp.status);
        } else {
            // Not fatal: the take below is the state we have always sent.
            warn!("← modesChanged(initial sync: screen=car 2/2) REJECTED: HTTP {} — taking the screen anyway", resp.status);
        }
    } else {
        info!("initial screen sync skipped (/info MainScreen takeConstraint={info_screen_take:?})");
    }
    let modes_body = build_modes_changed_body(false).context("marshal modesChanged plist")?;
    let resp = rtsp
        .request("POST", "/command", PLIST_CT, &modes_body, &Vec::new())
        .await
        .context("POST /command modesChanged")?;
    if !resp.is_success() {
        bail!("← modesChanged REJECTED: HTTP {}", resp.status);
    }
    info!("← modesChanged OK");

    // --- 6+7. SETUP / (per-stream, type 110) and open the screen data channel ---
    let screen = open_screen_stream(&mut rtsp, peer, &display_uuid).await?;

    // --- 8. Render the dashboard and start the encoder ---
    // The capture surface is sized to the car's negotiated dims (or, if a real
    // kiosk `:0` already exists, to its actual geometry). Capture/encode at the
    // EFFECTIVE geometry so x11grab never asks for a region larger than the root.
    let (_render_guard, cap_w, cap_h) =
        render::ensure_dashboard(width, height).await.context("render dashboard")?;

    // Touch injector: map the car's HID logical space to the dashboard surface.
    // `(width, height)` is the display size, NOT the touch range — see HID_TOUCH_LOGICAL.
    // Wayland: no X display to drive with XTest — an empty display selects the
    // uinput touchscreen, which the compositor reads through libinput.
    let wayland = render::wayland_target();
    let touch_display = if wayland.is_some() { String::new() } else { render::x_display() };
    let _touch_task = spawn_touch_injector(touch_rx, HID_TOUCH_LOGICAL, (cap_w, cap_h), touch_display);

    let mut encoder = VideoEncoder::new(VideoSettings {
        display: render::x_display(),
        wayland,
        width: cap_w,
        height: cap_h,
        // GOP from VideoSettings (IDR every 0.5 s by default). This used to force
        // an IDR on every frame, which spent the whole bitrate on intra frames
        // and made the car's screen blocky. A forceKeyFrame is still honoured:
        // `stream_screen` withholds frames until the next IDR and resends the
        // VideoConfig with it.
        ..Default::default()
    });
    encoder.start().await.context("start ffmpeg encoder")?;

    // Screen encryption, keyed with the SAME streamConnectionID advertised in the
    // screen SETUP above:
    //   * HAP-paired session → ChaCha20-Poly1305 (DataStream), IKM = the
    //     pair-verify shared secret (HKDF salt "DataStream-Salt<id>", info
    //     "DataStream-Output-Encryption-Key"); the 128-byte frame header is AAD.
    //   * legacy et=NONE session → AES-128-CTR with an all-zero session key
    //     (catplay `derive_aes_stream_keys(&[0;16], id)`); the car's keystream
    //     comes from that id.
    // VideoConfig / KeepAlive frames stay plaintext and do NOT advance the cipher.
    let mut cipher = screen_cipher(paired_secret.as_deref(), screen.stream_connection_id);

    // Share the control client so `/feedback`, keepalive, the audio SETUP and
    // TEARDOWN can all use it (it is no longer moved into the feedback task).
    let rtsp: SharedRtsp = Arc::new(Mutex::new(rtsp));

    // Audio: default ON (gate with RASCAR_CARPLAY_AUDIO=off). Spawned ENTIRELY on
    // its own task on the shared control channel, so neither its SETUP nor its
    // RTP stream can ever block the picture (the day-1 failure where a
    // never-answered audio SETUP was awaited inline and stalled the video loop).
    // Audio encryption follows the session (ChaCha when paired, else plaintext),
    // matching the screen and catplay's proven TX.
    let audio_task = maybe_start_audio(rtsp.clone(), peer, paired_secret.clone(), telephony, alert);

    // Every accepted car `changeModes` is followed by our `modesChanged` with the
    // resulting state (catplay `car_manager.rs` `flush_modes`).
    let modes_task = {
        let rtsp = rtsp.clone();
        tokio::spawn(async move {
            loop {
                modes_dirty.notified().await;
                if let Err(e) = send_state_modes_changed(&rtsp).await {
                    warn!("{e:#}");
                }
            }
        })
    };

    // --- 9. /feedback keep-alive on the RTSP control channel (every 1 s) ---
    //
    // Counted, because "did /feedback actually run" was the question that took a
    // whole car session to answer by forensics on 2026-09-20. The postmortem line
    // at the end of this function reports it directly.
    let feedback_ticks = Arc::new(AtomicU64::new(0));
    let feedback = {
        let rtsp = rtsp.clone();
        let feedback_ticks = feedback_ticks.clone();
        tokio::spawn(async move {
            // Fire the FIRST /feedback immediately (catplay seeds next_feedback =
            // now), then every 1 s. The car expects steady feedback right after
            // SETUP; a 1 s initial gap eats into its ~7.8 s teardown budget.
            let mut tick = tokio::time::interval(Duration::from_millis(1000));
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                let r = {
                    let mut guard = rtsp.lock().await;
                    guard.request("POST", "/feedback", "", &[], &Vec::<(&'static str, String)>::new()).await
                };
                match r {
                    Ok(r) if r.is_success() => {
                        n += 1;
                        feedback_ticks.store(n, Ordering::Relaxed);
                        if crate::debug::sampled(n - 1, 3, 30) {
                            debug!("feedback tick #{n} (HTTP {})", r.status);
                            // The car answers /feedback with per-stream timing
                            // (sr, sampleTime, streamConnectionID). We used to
                            // throw it away — it is the one channel where the car
                            // can signal displeasure before it tears the session
                            // down, so decode it.
                            if !r.body.is_empty() {
                                match bplist::from_bytes(&r.body) {
                                    Ok(v) => debug!("feedback tick #{n} body: {v:?}"),
                                    Err(e) => debug!(
                                        "feedback tick #{n} body ({} B) is not a plist: {e}",
                                        r.body.len()
                                    ),
                                }
                            }
                        }
                    }
                    Ok(r) => warn!("/feedback returned HTTP {} — continuing", r.status),
                    Err(e) => {
                        warn!("/feedback failed: {e:#} — stopping keep-alive");
                        break;
                    }
                }
            }
        })
    };

    // --- 10. Capture → packetize → push, plus the data-channel KeepAlive ---
    //
    // In a loop, because the screen can be handed to the car's native UI and
    // taken back mid-session (`stream::screen`, rascar-app's "car screen"
    // button → `screen car` on 127.0.0.1:7704). The order of each transition
    // is the reference's (catplay `c2a/proxy_rpc/car_manager.rs`):
    //
    //   give away:  stop the stream (encoder + data socket) → TEARDOWN type 110
    //               → modesChanged MainScreen 2/2          (`:257-305`)
    //   come back:  requestUI → modesChanged MainScreen 1/1 → fresh SETUP
    //               type 110 → VideoConfig + keyframe      (`:609-614`,
    //               `modes_arbiter.rs:89-111`; resuming the old stream is not
    //               enough — it was torn down)
    //
    // Audio, the event channel, /feedback and the keepalive run throughout.
    // The VideoConfig header advertises the ENCODED frame dims (= capture dims).
    let mut screen = Some(screen);
    let stream_result = loop {
        let Some(ScreenStream { wr, drain, .. }) = screen.take() else {
            break Err(anyhow!("screen loop without a screen stream"));
        };
        let handed_back = tokio::select! {
            r = stream_screen(&mut encoder, wr, &mut cipher, cap_w, cap_h, force_kf.clone(), stats_as_body) => Some(r),
            _ = wait_for_screen_owner(&mut screen_owner, crate::stream::screen::ScreenOwner::Car) => None,
        };
        if let Some(r) = handed_back {
            break r;
        }

        // --- 10a. Hand MainScreen to the car ---
        let t0 = Instant::now();
        info!("screen: handing MainScreen to the car");
        // The stream future is gone, so its write half is closed: no frame and
        // no data-channel keepalive can follow. Stop the capture too.
        drop(drain);
        encoder.stop().await;
        send_teardown(&rtsp, &display_uuid).await;
        info!("screen: screen stream torn down ({} ms)", t0.elapsed().as_millis());
        match send_screen_modes_changed(&rtsp, true).await {
            Ok(()) => info!(
                "screen: MainScreen is the car's ({} ms) — waiting for requestUI (car's CarPlay icon) or `screen phone`",
                t0.elapsed().as_millis()
            ),
            // Stay "car": the stream is torn down whatever it answered.
            Err(e) => warn!("screen: modesChanged(screen=car) failed: {e:#} — the screen stays the car's"),
        }

        // --- 10b. Wait for the come-back, or the session to end ---
        let given_at = Instant::now();
        let reopened = loop {
            if let Err(e) = wait_screen_back(&mut screen_owner, &event_task, &feedback).await {
                break Err(e);
            }
            let t1 = Instant::now();
            info!(
                "screen: taking MainScreen back after {:.1} s with the car",
                given_at.elapsed().as_secs_f32()
            );
            if let Err(e) = send_screen_modes_changed(&rtsp, false).await {
                // Still the car's: never SETUP a stream it was not told about.
                warn!("screen: modesChanged(screen=phone) failed: {e:#} — leaving the screen to the car");
                crate::stream::screen::request(crate::stream::screen::ScreenOwner::Car);
                continue;
            }
            let reopened = {
                let mut guard = rtsp.lock().await;
                tokio::time::timeout(Duration::from_secs(3), open_screen_stream(&mut guard, peer, &display_uuid))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("screen SETUP unanswered within 3 s")))
            };
            break reopened.map(|s| (s, t1));
        };
        let (s, t1) = match reopened {
            Ok(v) => v,
            Err(e) => break Err(e.context("taking the screen back")),
        };
        cipher = screen_cipher(paired_secret.as_deref(), s.stream_connection_id);
        // The new encoder opens with an IDR; a stale forceKeyFrame is moot.
        force_kf.store(false, Ordering::Relaxed);
        if let Err(e) = encoder.start().await {
            break Err(e.context("restart ffmpeg encoder after the hand-back"));
        }
        info!("screen: new screen stream ready ({} ms) — VideoConfig + keyframe follow", t1.elapsed().as_millis());
        screen = Some(s);
    };

    // --- 10b. POSTMORTEM ---
    //
    // One line that answers, without log forensics, the questions every car
    // attempt has cost us: how long did the session actually live, and were the
    // two liveness channels running while it did? On 2026-09-20 the answer was
    // "/feedback: 0 ticks" — starved behind an unanswered audio SETUP — and it
    // took a whole car session and a log archaeology pass to establish.
    let session_secs = session_started.elapsed().as_secs_f32();
    let ticks = feedback_ticks.load(Ordering::Relaxed);
    info!(
        "**** SESSION POSTMORTEM **** lived {session_secs:.2}s since RTSP connect ·          /feedback ticks={ticks} · screen keepalive={} · outcome={}",
        if stats_as_body { "opcode5+body" } else { "legacy opcode2" },
        match &stream_result {
            Ok(()) => "encoder stopped".to_string(),
            Err(e) => format!("{e:#}"),
        }
    );
    if ticks == 0 {
        warn!(
            "**** /feedback NEVER TICKED **** the control channel was silent for the whole              session — for a screen-only session that is the ONLY signal refreshing the              receiver's activity counter"
        );
    }

    // --- 11. Teardown: stop the session cleanly. ---
    feedback.abort();
    timing_task.abort();
    event_task.abort();
    modes_task.abort();
    if let Some(t) = keepalive_task {
        t.abort();
    }
    if let Some(t) = audio_task {
        t.abort();
    }
    encoder.stop().await;
    // Send TEARDOWN on the control channel (media data socket already dropped).
    send_teardown(&rtsp, &display_uuid).await;
    // _render_guard drops here (kills Xvfb/Chromium if we started them).
    stream_result
}

/// The screen push loop: read the H.264 elementary stream, assemble access units,
/// send the plaintext VideoConfig once, then encrypted VideoFrames, interleaving a
/// KeepAlive frame every second. Runs until the encoder hits EOF or a write fails.
async fn stream_screen(
    encoder: &mut VideoEncoder,
    mut wr: OwnedWriteHalf,
    cipher: &mut VideoCipher,
    width: u32,
    height: u32,
    force_kf: Arc<AtomicBool>,
    // The receiver's `/info.keepAliveSendStatsAsBody`: selects which keepalive
    // form refreshes its activity timestamp. See `keepalive_stats_header`.
    stats_as_body: bool,
) -> Result<()> {
    let clock = Clock::new();
    let mut parser = NalParser::new();
    let mut assembler = AuAssembler::new();
    let mut latest_sps: Option<Vec<u8>> = None;
    let mut latest_pps: Option<Vec<u8>> = None;
    let mut config_sent = false;
    let mut keyframe_sent = false;
    let mut frames: u64 = 0;
    let mut frames_logged: u64 = 0;

    let mut keepalive = tokio::time::interval(Duration::from_millis(1000));
    keepalive.tick().await; // consume the immediate first tick

    let mut buf = vec![0u8; 256 * 1024];
    info!("**** STREAMING dashboard to car **** ({width}x{height}, screen cipher = {})", cipher.name());

    loop {
        tokio::select! {
            r = encoder.read_chunk(&mut buf) => {
                let n = match r {
                    Ok(0) => break, // encoder EOF
                    Ok(n) => n,
                    Err(e) => {
                        warn!("screen: read from encoder failed: {e:#}");
                        return Err(e);
                    }
                };
                // A forceKeyFrame command from the event channel: re-emit the
                // VideoConfig before the next IDR: resetting `config_sent` makes
                // `push_au` hold back P-frames until a keyframe with SPS/PPS
                // arrives (at most one GOP, 0.5 s), then send a fresh avcC with it.
                if force_kf.swap(false, Ordering::Relaxed) {
                    debug!("screen: forceKeyFrame honoured — re-sending VideoConfig on next AU");
                    config_sent = false;
                }
                for nal in parser.push(&buf[..n]) {
                    if let Some(au) = assembler.offer(nal) {
                        push_au(
                            &mut wr, cipher, &clock, &au, width, height,
                            &mut latest_sps, &mut latest_pps,
                            &mut config_sent, &mut keyframe_sent, &mut frames,
                        ).await?;
                    }
                }
                // Frame-flow visibility: prove the encoder keeps producing (not a
                // one-keyframe stall) and how many frames the car received before
                // any teardown.
                if frames >= frames_logged + 15 {
                    info!("screen: {frames} frames streamed to car");
                    frames_logged = frames;
                }
            }
            _ = keepalive.tick() => {
                if stats_as_body {
                    send_frame(&mut wr, &keepalive_stats_header(), KEEPALIVE_STATS_BODY)
                        .await
                        .context("send KeepAlive(stats body)")?;
                } else {
                    send_frame(&mut wr, &keepalive_header(), &[]).await.context("send KeepAlive")?;
                }
            }
        }
    }

    // Flush the trailing access unit.
    if let Some(au) = assembler.finish() {
        push_au(
            &mut wr, cipher, &clock, &au, width, height,
            &mut latest_sps, &mut latest_pps,
            &mut config_sent, &mut keyframe_sent, &mut frames,
        )
        .await
        .ok();
    }

    // Distinguish a real end from a capture that never produced a frame.
    match encoder.wait_exit().await {
        Ok(st) if !st.success() => bail!(
            "ffmpeg exited with {st} after {frames} frame(s) — capture failed (is an X server up on the capture display?)"
        ),
        Ok(st) => info!("screen: ffmpeg exited {st} after {frames} frame(s)"),
        Err(e) => warn!("screen: could not reap ffmpeg: {e:#}"),
    }
    if frames == 0 {
        bail!("ffmpeg produced no frames — nothing was streamed to the car");
    }
    info!("screen: streaming ended after {frames} frame(s)");
    Ok(())
}

/// Send one access unit: the plaintext VideoConfig (once, gated on the first
/// keyframe carrying SPS+PPS), then the encrypted VideoFrame.
#[allow(clippy::too_many_arguments)]
async fn push_au(
    wr: &mut OwnedWriteHalf,
    cipher: &mut VideoCipher,
    clock: &Clock,
    au: &[Vec<u8>],
    width: u32,
    height: u32,
    latest_sps: &mut Option<Vec<u8>>,
    latest_pps: &mut Option<Vec<u8>>,
    config_sent: &mut bool,
    keyframe_sent: &mut bool,
    frames: &mut u64,
) -> Result<()> {
    if au.is_empty() {
        return Ok(());
    }
    for nal in au {
        match nal_type(nal) {
            7 => *latest_sps = Some(nal.clone()),
            8 => *latest_pps = Some(nal.clone()),
            _ => {}
        }
    }
    let is_keyframe = au.iter().any(|n| nal_type(n) == 5);

    // First data frame MUST be VideoConfig; first VideoFrame MUST be a keyframe.
    if !*config_sent {
        match (is_keyframe, latest_sps.as_ref(), latest_pps.as_ref()) {
            (true, Some(sps), Some(pps)) => {
                let avcc = build_avcc(sps, pps).context("build avcC")?;
                let hdr = video_config_header(avcc.len() as u32, width as f32, height as f32);
                send_frame(wr, &hdr, &avcc).await.context("send VideoConfig")?;
                *config_sent = true;
                info!("→ VideoConfig (avcC {}B, {width}x{height})", avcc.len());
            }
            // Drop stray non-keyframes (or keyframes still lacking SPS/PPS) until a
            // decodable keyframe arrives — sending one first would break the car.
            _ => return Ok(()),
        }
    }

    let payload = nals_to_avcc(au);
    // The 128-byte VideoFrame header carries `body_size` and, for the ChaCha
    // DataStream cipher, is the AEAD AAD — so it must be built with the CIPHERTEXT
    // length (payload + 16-byte Poly1305 tag) and set as AAD BEFORE encryption.
    // For AES-128-CTR `output_len` == payload length and `set_aad_header` is a
    // no-op, so this reduces to the plaintext-length header used before.
    let out_len = cipher.output_len(payload.len());
    let hdr = video_frame_header(out_len as u32, clock.now());
    cipher.set_aad_header(&hdr);
    let ct = cipher.encrypt_frame(&payload);
    debug_assert_eq!(ct.len(), out_len, "cipher output length must match the header body_size");
    send_frame(wr, &hdr, &ct).await.context("send VideoFrame")?;

    if is_keyframe && !*keyframe_sent {
        *keyframe_sent = true;
        info!("→ keyframe (first VideoFrame, {}B payload)", ct.len());
    }
    *frames += 1;
    Ok(())
}

// ---------------------------------------------------------------------------
// plist builders / parsers
// ---------------------------------------------------------------------------

/// Build the InitialSetup plist for `SETUP /setup` (no streams in phase 1).
///
/// `encrypted` selects the stream encryption negotiation: `false` sends
/// `et = 0` (NONE, the legacy plaintext path); `true` OMITS `et` entirely, which
/// The `et` (encryption type) value for a plaintext SETUP.
///
/// Apple's `kAirPlayEncryptionType_None` is **`1 << 0` = 1**, NOT 0 — 0 is
/// `kAirPlayEncryptionType_Undefined` (`apple-carplay-sdk/source/Sources/
/// AirPlayCommon.h:224-226`). The receiver branches on inequality:
///
/// ```c
/// // AirPlayReceiverServer.c:3241-3247
/// et = CFDictionaryGetInt64( requestParams, kAirPlayKey_EncryptionType, &err );
/// if( !err && ( et != kAirPlayEncryptionType_None ) ) {
///     err = _requestDecryptKey( inCnx, requestParams, et, key, iv );
///     require_noerr_action( err, exit, status = kHTTPStatus_KeyManagementError ); // 466
/// }
/// ```
///
/// so ANY value other than 1 — including the 0 we used to send under a comment
/// that called it "NONE" — sends the receiver looking for an `ekey` we never
/// provide, and the only exit from that arm is **466 Key Management Error**.
/// That 466 is what put every one of our sessions on the PIN pair-setup path.
///
/// `RASCAR_ET` overrides it for a car-side A/B (`RASCAR_ET=0` restores the old,
/// known-broken behaviour).
fn encryption_type_none() -> i64 {
    const APPLE_ENCRYPTION_TYPE_NONE: i64 = 1; // kAirPlayEncryptionType_None = (1 << 0)
    std::env::var("RASCAR_ET")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(APPLE_ENCRYPTION_TYPE_NONE)
}

/// is how catplay's `do_initial_setup` signals ChaCha20-Poly1305 stream
/// encryption once a HAP pairing shared secret exists.
fn build_initial_setup(timing_port: u16, encrypted: bool) -> Result<Vec<u8>> {
    let mut d = plist::Dictionary::new();
    d.insert("deviceID".into(), crate::DEVICE_MAC.into());
    d.insert("macAddress".into(), crate::DEVICE_MAC.into());
    d.insert("model".into(), "iPhone8.1".into());
    d.insert("name".into(), "iPhone".into());
    d.insert("osName".into(), "iPhone OS".into());
    d.insert("osBuildVersion".into(), "19A404".into());
    d.insert("osVersion".into(), "15.0.2".into());
    d.insert("sessionUUID".into(), generate_uuid().into());
    d.insert("sessionCorrelationUUID".into(), generate_uuid().into());
    d.insert("sourceVersion".into(), "566.25.21".into());
    d.insert("timingPort".into(), (timing_port as i64).into());
    d.insert("statsCollectionEnabled".into(), false.into());
    d.insert("updateSessionRequest".into(), false.into());
    d.insert("keepAliveLowPower".into(), true.into());
    if !encrypted {
        d.insert("et".into(), encryption_type_none().into());
    }
    // else: omit `et` — catplay `do_initial_setup` sets `req.et = None` when a
    // pairing shared secret exists, negotiating ChaCha stream encryption.
    d.insert("isScreenMirroringSession".into(), false.into());
    let features = CONTROLLER_FEATURES
        .iter()
        .map(|f| bplist::Value::String((*f).into()))
        .collect();
    d.insert("features".into(), bplist::Value::Array(features));
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// Build the per-stream screen `SETUP /` plist (type 110).
fn build_stream_setup(stream_connection_id: i64, display_uuid: &str) -> Result<Vec<u8>> {
    let mut s = plist::Dictionary::new();
    s.insert("streamConnectionID".into(), stream_connection_id.into());
    s.insert("type".into(), 110i64.into());
    s.insert("latencyMs".into(), 25i64.into());
    s.insert("uuid".into(), display_uuid.into());
    let mut d = plist::Dictionary::new();
    d.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(s)]));
    bplist::to_bytes(&bplist::Value::Dictionary(d))
}

/// Parse `displays[0]` width/height (either `widthPixels`/`heightPixels` or
/// `width`/`height`) and a stream uuid from the `/info` reply, with fallbacks.
/// Read a boolean from the top level of a binary-plist body, defaulting to false
/// when absent or unparseable.
fn plist_bool(body: &[u8], key: &str) -> bool {
    bplist::from_bytes(body)
        .ok()
        .as_ref()
        .and_then(|v| v.as_dictionary())
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_boolean())
        .unwrap_or(false)
}

/// Log the `modes` dictionary the car reports in `/info`.
///
/// Pure instrumentation: it records who the head unit thinks owns MainScreen and
/// MainAudio, and the take/borrow constraints on each, at the moment we are about
/// to assert our own ownership in `modesChanged`.
fn log_info_modes(body: &[u8]) {
    match bplist::from_bytes(body) {
        Ok(bplist::Value::Dictionary(d)) => match d.get("modes") {
            Some(modes) => info!("← /info modes: {modes:?}"),
            None => warn!("← /info carries NO `modes` dictionary"),
        },
        Ok(_) => warn!("← /info is not a plist dictionary"),
        Err(e) => warn!("← /info could not be parsed as a plist: {e}"),
    }
}

/// AirPlay `takeConstraint` "Anytime" (Apple `kAirPlayConstraint_Anytime`): a
/// nice-to-have take by the other side succeeds.
const CONSTRAINT_ANYTIME: i64 = 100;

/// The `takeConstraint` of MainScreen when the car's `/info.modes` says its
/// native UI TAKES the screen (`transferType` 1); `None` for any other initial
/// state (a Borrow during the car's start-up screen, no modes, unparsable).
///
/// The Jaguar reports `{resourceID:1, transferType:1, transferPriority:100,
/// takeConstraint:100, borrowConstraint:100}` once its UI is up (every session
/// of 2026-10-05), and a Borrow with `unborrowConstraint:1000` while its own
/// start-up screen is still showing (2026-10-04).
fn info_screen_take_constraint(body: &[u8]) -> Option<i64> {
    const TRANSFER_TAKE: i64 = 1;
    let v = bplist::from_bytes(body).ok()?;
    let resources = v.as_dictionary()?.get("modes")?.as_dictionary()?.get("resources")?.as_array()?;
    let screen = resources
        .iter()
        .filter_map(|r| r.as_dictionary())
        .find(|r| r.get("resourceID").and_then(plist_int) == Some(RESOURCE_MAIN_SCREEN))?;
    if screen.get("transferType").and_then(plist_int) != Some(TRANSFER_TAKE) {
        return None;
    }
    screen.get("takeConstraint").and_then(plist_int)
}

/// Whether our first `modesChanged` must first state the car's own initial
/// screen ownership (2/2) before taking the screen (1/1). See step 5 of
/// [`run_carplay`]. Only when the car TAKES the screen with a constraint a
/// nice-to-have take may override; kill switch `RASCAR_INITIAL_SCREEN_SYNC=off`.
fn initial_screen_sync(info_take_constraint: Option<i64>) -> bool {
    info_take_constraint == Some(CONSTRAINT_ANYTIME) && crate::debug::env_flag("RASCAR_INITIAL_SCREEN_SYNC", true)
}

fn parse_info_display(body: &[u8]) -> (u32, u32, String) {
    let fallback = (DEFAULT_W, DEFAULT_H, generate_uuid());
    let v = match bplist::from_bytes(body) {
        Ok(v) => v,
        Err(_) => return fallback,
    };
    let d = match v.as_dictionary() {
        Some(d) => d,
        None => return fallback,
    };
    let (mut w, mut h) = (DEFAULT_W, DEFAULT_H);
    let mut uuid: Option<String> = None;
    if let Some(first) = d.get("displays").and_then(|x| x.as_array()).and_then(|a| a.first()).and_then(|x| x.as_dictionary()) {
        let get = |k: &str| {
            first
                .get(k)
                .and_then(|x| x.as_signed_integer().or_else(|| x.as_unsigned_integer().map(|n| n as i64)))
        };
        let (mut pw, mut ph) = (get("widthPixels").unwrap_or(0), get("heightPixels").unwrap_or(0));
        if pw <= 0 || ph <= 0 {
            pw = get("width").unwrap_or(0);
            ph = get("height").unwrap_or(0);
        }
        if pw > 0 && ph > 0 {
            w = pw as u32;
            h = ph as u32;
        }
        uuid = first.get("uuid").and_then(|x| x.as_string()).map(str::to_string);
    }
    // Fall back to a top-level uuid, then a random one.
    let uuid = uuid
        .or_else(|| d.get("uuid").and_then(|x| x.as_string()).map(str::to_string))
        .unwrap_or_else(generate_uuid);
    (w, h, uuid)
}

/// Read `streams[]` for the entry whose `type` matches, returning its `dataPort`.
fn extract_stream_data_port(v: &bplist::Value, stream_type: i64) -> Option<u16> {
    let streams = v.as_dictionary()?.get("streams")?.as_array()?;
    for s in streams {
        let d = match s.as_dictionary() {
            Some(d) => d,
            None => continue,
        };
        let ty = d
            .get("type")
            .and_then(|x| x.as_signed_integer().or_else(|| x.as_unsigned_integer().map(|n| n as i64)));
        if ty == Some(stream_type) {
            if let Some(port) = plist_u16(d, "dataPort") {
                return Some(port);
            }
        }
    }
    None
}

fn plist_u16(d: &plist::Dictionary, key: &str) -> Option<u16> {
    d.get(key)
        .and_then(|v| v.as_unsigned_integer().or_else(|| v.as_signed_integer().map(|n| n as u64)))
        .and_then(|n| u16::try_from(n).ok())
}

// ---------------------------------------------------------------------------
// channel helpers
// ---------------------------------------------------------------------------

/// Rebuild `peer` with a different port, preserving the IPv6 scope id (the CDC-NCM
/// link-local scope). The event and data channels share the RTSP peer's scope.
fn with_port(peer: SocketAddr, port: u16) -> SocketAddr {
    match peer {
        SocketAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(*v6.ip(), port, 0, v6.scope_id())),
        SocketAddr::V4(v4) => SocketAddr::V4(SocketAddrV4::new(*v4.ip(), port)),
    }
}

/// Discard everything from a channel until it closes (keeps a reverse channel
/// from backpressuring us).
async fn drain<R: AsyncRead + Unpin>(mut r: R) {
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

/// Write a 128-byte header + optional payload with a bounded deadline (a stalled
/// car surfaces as a write timeout → error → stop).
async fn send_frame(wr: &mut OwnedWriteHalf, header: &[u8; HEADER_LEN], payload: &[u8]) -> Result<()> {
    let write = async {
        wr.write_all(header).await?;
        if !payload.is_empty() {
            wr.write_all(payload).await?;
        }
        wr.flush().await?;
        Ok::<(), std::io::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(2), write).await {
        Ok(r) => r.context("data-channel write"),
        Err(_) => bail!("data-channel write timed out (2 s) — car stalled"),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Random RFC-4122 v4 UUID string (uppercase, hyphenated).
fn generate_uuid() -> String {
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// A random strictly-positive `i64` (streamConnectionID for the screen SETUP).
fn random_positive_i64() -> i64 {
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let n = (u64::from_le_bytes(b) & 0x7FFF_FFFF_FFFF_FFFF) as i64;
    n.max(1)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::crypto::derive_video_keys;

    #[test]
    fn avcc_layout_is_minimal_iso14496_record() {
        let sps = [0x67u8, 0x42, 0xC0, 0x1F, 0x11, 0x22];
        let pps = [0x68u8, 0xCE, 0x3C];
        let a = build_avcc(&sps, &pps).unwrap();
        assert_eq!(a[0], 0x01); // configurationVersion
        assert_eq!(a[1], sps[1]); // profile
        assert_eq!(a[2], sps[2]); // compat
        assert_eq!(a[3], sps[3]); // level
        assert_eq!(a[4], 0xFF);
        assert_eq!(a[5], 0xE1);
        assert_eq!(&a[6..8], &(sps.len() as u16).to_be_bytes());
        assert_eq!(&a[8..8 + sps.len()], &sps);
        let off = 8 + sps.len();
        assert_eq!(a[off], 0x01); // numPPS
        assert_eq!(&a[off + 1..off + 3], &(pps.len() as u16).to_be_bytes());
        assert_eq!(&a[off + 3..off + 3 + pps.len()], &pps);
        // Exact length: 11 fixed/length bytes + sps + pps, no trailer.
        assert_eq!(a.len(), 11 + sps.len() + pps.len());
    }

    #[test]
    fn avcc_rejects_short_sps() {
        assert!(build_avcc(&[0x67, 0x42, 0xC0], &[0x68]).is_err());
        assert!(build_avcc(&[], &[0x68]).is_err());
        assert!(build_avcc(&[0x67, 0x42, 0xC0, 0x1F], &[0x68]).is_ok());
    }

    #[test]
    fn annexb_to_avcc_converts_both_start_code_lengths() {
        // 4-byte start code + NAL(3 bytes), 3-byte start code + NAL(2 bytes).
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0xBB, // SPS-ish, 3 bytes
            0, 0, 1, 0x68, 0xCC, // PPS-ish, 2 bytes
        ];
        let avcc = annexb_to_avcc(&stream);
        // [00 00 00 03][67 AA BB][00 00 00 02][68 CC]
        assert_eq!(&avcc[0..4], &[0, 0, 0, 3]);
        assert_eq!(&avcc[4..7], &[0x67, 0xAA, 0xBB]);
        assert_eq!(&avcc[7..11], &[0, 0, 0, 2]);
        assert_eq!(&avcc[11..13], &[0x68, 0xCC]);
        assert_eq!(avcc.len(), 13);
    }

    #[test]
    fn nals_to_avcc_length_prefixes_each() {
        let nals = vec![vec![0x65u8, 0x01, 0x02], vec![0x41u8, 0x03]];
        let avcc = nals_to_avcc(&nals);
        assert_eq!(&avcc[0..4], &[0, 0, 0, 3]);
        assert_eq!(&avcc[4..7], &[0x65, 0x01, 0x02]);
        assert_eq!(&avcc[7..11], &[0, 0, 0, 2]);
        assert_eq!(&avcc[11..13], &[0x41, 0x03]);
    }

    #[test]
    fn video_config_header_layout() {
        let h = video_config_header(42, 800.0, 480.0);
        assert_eq!(u32::from_le_bytes(h[0..4].try_into().unwrap()), 42);
        assert_eq!(h[4], OP_VIDEO_CONFIG);
        assert_eq!(h[6] & SMALL_PARAM_ENCRYPTED, SMALL_PARAM_ENCRYPTED); // Encrypted flag
        // params[0] at [8..16] is zero.
        assert_eq!(u64::from_le_bytes(h[8..16].try_into().unwrap()), 0);
        // params[1] at [16..24] packs the two f32 dimensions.
        assert_eq!(f32::from_le_bytes(h[16..20].try_into().unwrap()), 800.0);
        assert_eq!(f32::from_le_bytes(h[20..24].try_into().unwrap()), 480.0);
        assert_eq!(h.len(), HEADER_LEN);
    }

    #[test]
    fn video_frame_header_layout() {
        let h = video_frame_header(1234, 0x1122_3344_5566_7788);
        assert_eq!(u32::from_le_bytes(h[0..4].try_into().unwrap()), 1234);
        assert_eq!(h[4], OP_VIDEO_FRAME);
        // No Encrypted flag on the frame header (only on VideoConfig).
        assert_eq!(h[6] & SMALL_PARAM_ENCRYPTED, 0);
        assert_eq!(u64::from_le_bytes(h[8..16].try_into().unwrap()), 0x1122_3344_5566_7788);
    }

    #[test]
    fn keepalive_header_is_bare() {
        let h = keepalive_header();
        assert_eq!(u32::from_le_bytes(h[0..4].try_into().unwrap()), 0); // no payload
        assert_eq!(h[4], OP_KEEPALIVE);
        assert!(h[5..].iter().all(|&b| b == 0));
    }

    /// Golden vectors: the legacy et=NONE key/iv derivation for id 0 must match
    /// SHA-512("AirPlayStreamKey0"||0^16)[..16] / SHA-512("AirPlayStreamIV0"||0^16)[..16].
    #[test]
    fn legacy_all_zero_key_iv_golden() {
        let (key, iv) = derive_video_keys(&[0u8; 16], 0);
        assert_eq!(
            key,
            [0x51, 0x37, 0x6c, 0xf5, 0xcf, 0x85, 0x23, 0x19, 0x63, 0xce, 0xc9, 0xba, 0x9f, 0x48, 0x06, 0xb6]
        );
        assert_eq!(
            iv,
            [0x02, 0x56, 0x41, 0x10, 0x1b, 0xed, 0x4a, 0x53, 0xc8, 0x61, 0xa3, 0x3a, 0x23, 0x40, 0x74, 0xe2]
        );
    }

    /// The screen cipher MUST be keyed with the ADVERTISED streamConnectionID,
    /// not 0 — otherwise our keystream diverges from the car's and the picture is
    /// garbled. This locks that the id materially changes the derived key (so a
    /// regression back to `derive_video_keys(&[0;16], 0)` would fail here) and
    /// that a non-zero id follows the AirPlay stream-key layout
    /// SHA-512("AirPlayStreamKey" || id_str || key).
    #[test]
    fn screen_key_uses_advertised_stream_id() {
        let scid: i64 = 0x1122_3344_5566_7788;
        let (key_id, iv_id) = derive_video_keys(&[0u8; 16], scid);
        let (key_0, _iv_0) = derive_video_keys(&[0u8; 16], 0);
        assert_ne!(key_id, key_0, "keying with the advertised id must differ from id 0");

        // Byte-exact recomputation of the stream-key layout.
        use sha2::{Digest, Sha512};
        let id = scid as u64;
        let mut hk = Sha512::new();
        hk.update(format!("AirPlayStreamKey{id}").as_bytes());
        hk.update([0u8; 16]);
        let mut hi = Sha512::new();
        hi.update(format!("AirPlayStreamIV{id}").as_bytes());
        hi.update([0u8; 16]);
        assert_eq!(&key_id[..], &hk.finalize()[..16]);
        assert_eq!(&iv_id[..], &hi.finalize()[..16]);
    }

    #[test]
    fn au_assembler_splits_all_idr_stream() {
        // Two frames, each SPS(7) PPS(8) IDR(5, first_mb_in_slice=0 → byte1 msb set).
        let sps = vec![0x67u8, 0x42, 0xC0, 0x1F];
        let pps = vec![0x68u8, 0xCE];
        let idr = vec![0x65u8, 0x80]; // type 5, first slice
        let mut a = AuAssembler::new();
        assert!(a.offer(sps.clone()).is_none());
        assert!(a.offer(pps.clone()).is_none());
        assert!(a.offer(idr.clone()).is_none());
        // Next frame's SPS closes the first AU.
        let au1 = a.offer(sps.clone()).expect("first AU emitted");
        assert_eq!(au1.len(), 3);
        assert_eq!(nal_type(&au1[0]), 7);
        assert_eq!(nal_type(&au1[2]), 5);
        a.offer(pps).unwrap_or_default();
        a.offer(idr).unwrap_or_default();
        let au2 = a.finish().expect("trailing AU");
        assert_eq!(au2.len(), 3);
    }

    #[test]
    fn au_assembler_splits_p_frames() {
        let idr = vec![0x65u8, 0x80]; // keyframe, first slice
        let p1 = vec![0x41u8, 0x80]; // non-IDR (type 1), first slice
        let p2 = vec![0x41u8, 0x80];
        let mut a = AuAssembler::new();
        assert!(a.offer(idr).is_none());
        let au1 = a.offer(p1).expect("IDR AU emitted when P starts");
        assert_eq!(au1.len(), 1);
        assert_eq!(nal_type(&au1[0]), 5);
        let au2 = a.offer(p2).expect("P AU emitted when next P starts");
        assert_eq!(au2.len(), 1);
        assert_eq!(nal_type(&au2[0]), 1);
    }

    #[test]
    fn with_port_preserves_ipv6_scope() {
        let peer = SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 7000, 0, 42));
        match with_port(peer, 5001) {
            SocketAddr::V6(v6) => {
                assert_eq!(v6.port(), 5001);
                assert_eq!(v6.scope_id(), 42);
            }
            other => panic!("expected V6, got {other}"),
        }
    }

    #[test]
    fn find_subslice_finds_requestui() {
        assert_eq!(find_subslice(b"POST /command requestUI xyz", b"requestUI"), Some(14));
        assert_eq!(find_subslice(b"nothing here", b"requestUI"), None);
    }

    #[test]
    fn initial_setup_plist_roundtrips_with_features() {
        let bytes = build_initial_setup(6002, false).unwrap();
        let v = bplist::from_bytes(&bytes).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.get("model").unwrap().as_string(), Some("iPhone8.1"));
        // Apple: kAirPlayEncryptionType_None = (1 << 0) = 1. Sending 0
        // (kAirPlayEncryptionType_Undefined) makes the receiver demand an `ekey`
        // and answer 466 — see encryption_type_none().
        assert_eq!(d.get("et").unwrap().as_signed_integer(), Some(1));
        assert_eq!(d.get("timingPort").unwrap().as_signed_integer(), Some(6002));
        assert_eq!(d.get("isScreenMirroringSession").unwrap().as_boolean(), Some(false));
        let feats = d.get("features").unwrap().as_array().unwrap();
        // CONTROLLER_FEATURES is now empty (match catplay's proven-good wire), so
        // the advertised `features` array is present but empty. Re-add a concrete
        // assertion here if a feature is ever re-introduced with backing behavior.
        assert_eq!(feats.len(), CONTROLLER_FEATURES.len());
        assert!(feats.is_empty(), "features must be empty, as on the reference wire");
    }

    #[test]
    fn initial_setup_omits_et_when_encrypted() {
        // The post-pair (ChaCha) SETUP must OMIT `et` entirely — catplay signals
        // stream encryption by the ABSENCE of the key, not by a distinct value.
        let bytes = build_initial_setup(6002, true).unwrap();
        let v = bplist::from_bytes(&bytes).unwrap();
        let d = v.as_dictionary().unwrap();
        assert!(d.get("et").is_none(), "encrypted SETUP must not carry `et`");
        assert_eq!(d.get("timingPort").unwrap().as_signed_integer(), Some(6002));
    }

    #[test]
    fn stream_setup_and_data_port_extraction() {
        let body = build_stream_setup(1234567, "ABCD-UUID").unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        // Round-trip the stream fields.
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(s.get("type").unwrap().as_signed_integer(), Some(110));
        assert_eq!(s.get("streamConnectionID").unwrap().as_signed_integer(), Some(1234567));
        assert_eq!(s.get("latencyMs").unwrap().as_signed_integer(), Some(25));

        // A reply-shaped plist with a type-110 dataPort must be found.
        let mut reply_stream = plist::Dictionary::new();
        reply_stream.insert("type".into(), 110i64.into());
        reply_stream.insert("dataPort".into(), 49200i64.into());
        let mut reply = plist::Dictionary::new();
        reply.insert(
            "streams".into(),
            bplist::Value::Array(vec![bplist::Value::Dictionary(reply_stream)]),
        );
        let reply = bplist::Value::Dictionary(reply);
        assert_eq!(extract_stream_data_port(&reply, 110), Some(49200));
        assert_eq!(extract_stream_data_port(&reply, 96), None);
    }

    #[test]
    fn parse_info_display_reads_dimensions_and_falls_back() {
        // widthPixels/heightPixels present.
        let mut disp = plist::Dictionary::new();
        disp.insert("widthPixels".into(), 1280i64.into());
        disp.insert("heightPixels".into(), 720i64.into());
        disp.insert("uuid".into(), "DISP-UUID".into());
        let mut top = plist::Dictionary::new();
        top.insert(
            "displays".into(),
            bplist::Value::Array(vec![bplist::Value::Dictionary(disp)]),
        );
        let body = bplist::to_bytes(&bplist::Value::Dictionary(top)).unwrap();
        let (w, h, uuid) = parse_info_display(&body);
        assert_eq!((w, h), (1280, 720));
        assert_eq!(uuid, "DISP-UUID");

        // Bogus body → fallback dimensions and a generated uuid.
        let (w, h, uuid) = parse_info_display(b"not a plist");
        assert_eq!((w, h), (DEFAULT_W, DEFAULT_H));
        assert_eq!(uuid.len(), 36); // generated
    }

    #[test]
    fn random_positive_i64_is_strictly_positive() {
        for _ in 0..1000 {
            assert!(random_positive_i64() >= 1);
        }
    }

    /// A time-sync request built from raw bytes: header, zero RTP time, zero
    /// origin and receive stamps, `transmit` in the last eight bytes.
    fn raw_time_ping(transmit: u64) -> Vec<u8> {
        let mut v = vec![0x80, 0xD2, 0x00, 0x07, 0, 0, 0, 0];
        v.extend_from_slice(&[0u8; 16]);
        v.extend_from_slice(&transmit.to_be_bytes());
        v
    }

    /// The reply is pinned byte for byte: header 80 D3 00 07, zero RTP time,
    /// origin = the request's transmit stamp, receive = transmit = our clock.
    #[test]
    fn time_ping_reply_bytes() {
        let ping = raw_time_ping(0x0102_0304_0506_0708);
        let reply = answer_time_ping(&ping, 0x1122_3344_5566_7788).expect("valid request");
        let expected: [u8; 32] = [
            0x80, 0xD3, 0x00, 0x07, 0, 0, 0, 0, //
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, //
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, //
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
        ];
        assert_eq!(reply, expected);
        // Bits other than the version in byte 0, and trailing bytes, are ignored.
        let mut loose = ping.clone();
        loose[0] = 0xBF;
        loose.extend_from_slice(&[0xEE; 8]);
        assert_eq!(answer_time_ping(&loose, 0x1122_3344_5566_7788), Some(expected));
    }

    #[test]
    fn time_ping_rejects_malformed() {
        assert_eq!(answer_time_ping(&[0u8; 8], 1), None, "too short");
        assert_eq!(answer_time_ping(&raw_time_ping(1)[..31], 1), None, "one byte short");
        let mut v1 = raw_time_ping(1);
        v1[0] = 0x40;
        assert_eq!(answer_time_ping(&v1, 1), None, "version 1");
        let mut resp = raw_time_ping(1);
        resp[1] = TIME_RESPONSE;
        assert_eq!(answer_time_ping(&resp, 1), None, "a response is not a request");
        let mut len = raw_time_ping(1);
        len[3] = 8;
        assert_eq!(answer_time_ping(&len, 1), None, "wrong length field");
    }

    #[test]
    fn timing_clock_is_on_the_ntp_epoch() {
        let secs = ntp_now(Instant::now()) >> 32;
        assert!((2_208_988_800..2_208_988_860).contains(&secs), "{secs}");
    }

    #[test]
    fn keepalive_stats_frame_matches_the_ios_layout() {
        // Body: a canonical binary plist holding an EMPTY dictionary, 42 bytes.
        assert_eq!(KEEPALIVE_STATS_BODY.len(), 42);
        assert!(KEEPALIVE_STATS_BODY.starts_with(b"bplist00"));

        let h = keepalive_stats_header();
        // body_size
        assert_eq!(u32::from_le_bytes(h[0..4].try_into().unwrap()), 42);
        // opcode 5, NOT the legacy 2 — a zero-body frame is rejected before
        // opcode dispatch and never refreshes the receiver's activity timestamp.
        assert_eq!(h[4], OP_KEEPALIVE_WITH_BODY);
        assert_eq!(h[4], 5);
        // params[14] = h[120..128] = (0.0f32, 42.0f32); iOS puts the body length
        // in the SECOND float.
        assert_eq!(f32::from_le_bytes(h[120..124].try_into().unwrap()), 0.0);
        assert_eq!(f32::from_le_bytes(h[124..128].try_into().unwrap()), 42.0);
        // Everything else stays zero.
        assert!(h[5..8].iter().all(|&b| b == 0));
        assert!(h[8..120].iter().all(|&b| b == 0));

        // The legacy form remains available and unchanged for receivers that do
        // not advertise stats-as-body.
        let legacy = keepalive_header();
        assert_eq!(legacy[4], OP_KEEPALIVE);
        assert_eq!(u32::from_le_bytes(legacy[0..4].try_into().unwrap()), 0);
    }

    #[test]
    fn change_modes_take_is_refused_borrow_and_untake_are_accepted() {
        fn request(resource_id: i64, transfer_type: i64) -> Vec<u8> {
            let mut r = plist::Dictionary::new();
            r.insert("resourceID".into(), resource_id.into());
            r.insert("transferType".into(), transfer_type.into());
            let mut params = plist::Dictionary::new();
            params.insert(
                "resources".into(),
                bplist::Value::Array(vec![bplist::Value::Dictionary(r)]),
            );
            let mut d = plist::Dictionary::new();
            d.insert("type".into(), "changeModes".into());
            d.insert("params".into(), bplist::Value::Dictionary(params));
            bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap()
        }
        // Take(1) is a permanent transfer, which we do not model: refuse.
        assert_eq!(change_modes_borrows(&request(1, 1)), None);
        assert_eq!(change_modes_borrows(&request(2, 1)), None);
        // Untake(2) hands the resource back to us: nothing to do, accept.
        assert_eq!(change_modes_borrows(&request(1, 2)), Some(vec![]));
        // Borrow(3) / Unborrow(4), as the Jaguar sends them for MainAudio.
        assert_eq!(change_modes_borrows(&request(2, 3)), Some(vec![(2, true)]));
        assert_eq!(change_modes_borrows(&request(2, 4)), Some(vec![(2, false)]));
        // An unknown resource and garbage are refused.
        assert_eq!(change_modes_borrows(&request(9, 3)), None);
        assert_eq!(change_modes_borrows(b"not a plist"), None);

        // The refusal carries status 1 and NO params — it must not assert an
        // ownership state alongside a rejection.
        let body = build_change_modes_refusal_body().unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.get("status").unwrap().as_signed_integer(), Some(1));
        assert!(d.get("params").is_none());
    }

    /// Replays the Jaguar's two `changeModes` of 2026-09-29 (pcap, event
    /// channel): borrow MainAudio, then release it.
    #[test]
    fn jaguar_main_audio_borrow_is_reported_back_as_borrowed() {
        let car = |reason: &str, transfer: i64| {
            let mut r = plist::Dictionary::new();
            r.insert("resourceID".into(), 2i64.into());
            r.insert("transferType".into(), transfer.into());
            if transfer == 3 {
                r.insert("transferPriority".into(), 500i64.into());
                r.insert("unborrowConstraint".into(), 500i64.into());
            }
            let mut params = plist::Dictionary::new();
            params.insert("reasonStr".into(), reason.into());
            params.insert("resources".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(r)]));
            let mut d = plist::Dictionary::new();
            d.insert("type".into(), "changeModes".into());
            d.insert("params".into(), bplist::Value::Dictionary(params));
            bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap()
        };
        let borrow = car("Request Main Audio", 3);
        assert_eq!(change_modes_borrows(&borrow), Some(vec![(2, true)]));
        assert_eq!(change_modes_summary(&borrow), "\"Request Main Audio\" [res2:t3]");
        assert_eq!(change_modes_borrows(&car("Release Main Audio Request", 4)), Some(vec![(2, false)]));

        // While borrowed: the car is the current entity, we stay the owner.
        let entry = |p: &plist::Dictionary, id: i64| {
            let rs = p.get("resources").unwrap().as_array().unwrap();
            let d = rs
                .iter()
                .map(|r| r.as_dictionary().unwrap())
                .find(|d| d.get("resourceID").unwrap().as_signed_integer() == Some(id))
                .unwrap();
            let get = |k: &str| d.get(k).unwrap().as_signed_integer().unwrap();
            (get("entity"), get("permanentEntity"))
        };
        let owner = if claims_main_audio() { ENTITY_CONTROLLER } else { ENTITY_ACCESSORY };
        let borrowed = modes_params(false, false, false, true);
        assert_eq!(entry(&borrowed, RESOURCE_MAIN_AUDIO), (ENTITY_ACCESSORY, owner));
        assert_eq!(entry(&borrowed, RESOURCE_MAIN_SCREEN), (ENTITY_CONTROLLER, ENTITY_CONTROLLER));
        let released = modes_params(false, false, false, false);
        assert_eq!(entry(&released, RESOURCE_MAIN_AUDIO), (owner, owner));
    }

    #[test]
    fn integers_may_come_as_strings() {
        assert_eq!(plist_int(&bplist::Value::Integer(100i64.into())), Some(100));
        assert_eq!(plist_int(&bplist::Value::String("100".into())), Some(100));
        assert_eq!(plist_int(&bplist::Value::String("0x8000".into())), Some(0x8000));
        assert_eq!(plist_int(&bplist::Value::String("0X54".into())), Some(0x54));
        assert_eq!(plist_int(&bplist::Value::String("media".into())), None);
    }

    /// The Jaguar's `/info.audioFormats` (2026-09-29 pcap): every number is a string.
    #[test]
    fn jaguar_string_audio_formats_give_telephony() {
        let entry = |ty: &str, at: &str, inp: Option<&str>, out: &str| {
            let mut d = plist::Dictionary::new();
            d.insert("type".into(), ty.into());
            d.insert("audioType".into(), at.into());
            if let Some(i) = inp {
                d.insert("audioInputFormats".into(), i.into());
            }
            d.insert("audioOutputFormats".into(), out.into());
            bplist::Value::Dictionary(d)
        };
        let list = vec![
            entry("100", "compatibility", Some("0x54"), "0x854"),
            entry("101", "compatibility", None, "0x8000"),
            entry("100", "media", None, "0x8000"),
            entry("100", "alert", None, "0x800"),
            entry("100", "telephony", Some("0x54"), "0x54"),
            entry("100", "speechRecognition", Some("0x40"), "0x40"),
        ];
        let mut d = plist::Dictionary::new();
        d.insert("audioFormats".into(), bplist::Value::Array(list));
        let info = bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap();
        // 0x54 = 8k | 16k | 24k mono: we prefer 24 kHz.
        assert_eq!(telephony_mode_from_info(&info), Some(AudioMode::Telephony { format: 1 << 6, rate: 24000 }));
        // alert "0x800" = kAirPlayAudioFormat_PCM_44KHz_16Bit_Stereo (AirPlayCommon.h:347).
        assert_eq!(alert_mode_from_info(&info), Some(AudioMode::Alert { format: 0x800, rate: 44100, channels: 2 }));
    }

    #[test]
    fn pcm_level_reads_big_endian() {
        let mut l = PcmLevel::new();
        // +1000 and -1000, big-endian.
        l.add(&[0x03, 0xE8, 0xFC, 0x18]);
        assert_eq!(l.rms_peak(), (1000, 1000));
        assert_eq!(l.take_if_older(Duration::from_secs(3600)), None);
        let (rms, peak) = l.take_if_older(Duration::ZERO).unwrap();
        assert_eq!((rms, peak), (1000, 1000));
        assert_eq!(l.rms_peak(), (0, 0));
    }

    #[test]
    fn modes_changed_body_matches_reference_default_state() {
        let body = build_modes_changed_body(false).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.get("type").unwrap().as_string(), Some("modesChanged"));
        let params = d.get("params").unwrap().as_dictionary().unwrap();
        // resources: BOTH MainScreen(1) and MainAudio(2) are claimed by the
        // Controller(1).
        //
        // This test previously asserted MainAudio -> Accessory(2), with a comment
        // claiming that is what the reference transmitter does. That was wrong.
        // catplay `modes/change_modes.rs` documents its initial state as
        // "granting all resources to the `Controller`" and lists
        // `InitialPermanentEntity::controller(MainAudio)` +
        // `...controller(MainScreen)`, with the accessory variants commented out.
        // Verified against the source 2026-09-21.
        //
        // The claim is env-overridable, so skip when a developer shell pins it.
        if std::env::var("RASCAR_CARPLAY_AUDIO").is_ok()
            || std::env::var("RASCAR_MODES_CLAIM_AUDIO").is_ok()
        {
            return;
        }
        let res = params.get("resources").unwrap().as_array().unwrap();
        assert_eq!(res.len(), 2);
        for (i, id, entity) in [(0usize, 1i64, 1i64), (1, 2, 1)] {
            let r = res[i].as_dictionary().unwrap();
            assert_eq!(r.get("resourceID").unwrap().as_signed_integer(), Some(id));
            assert_eq!(r.get("entity").unwrap().as_signed_integer(), Some(entity));
            assert_eq!(r.get("permanentEntity").unwrap().as_signed_integer(), Some(entity));
        }
        // appStates: phoneCall(2)/turnByTurn(3) None(0); speech(1) None(0) speechMode=-1.
        let app = params.get("appStates").unwrap().as_array().unwrap();
        assert_eq!(app.len(), 3);
        let a0 = app[0].as_dictionary().unwrap();
        assert_eq!(a0.get("appStateID").unwrap().as_signed_integer(), Some(2));
        assert_eq!(a0.get("entity").unwrap().as_signed_integer(), Some(0));
        assert!(a0.get("speechMode").is_none());
        let a2 = app[2].as_dictionary().unwrap();
        assert_eq!(a2.get("appStateID").unwrap().as_signed_integer(), Some(1));
        assert_eq!(a2.get("speechMode").unwrap().as_signed_integer(), Some(-1));
    }

    /// `/info` body carrying only `modes.resources`, as the Jaguar sends it.
    fn info_with_screen_mode(screen: Vec<(&str, i64)>) -> Vec<u8> {
        let dict = |pairs: Vec<(&str, i64)>| {
            let mut d = plist::Dictionary::new();
            for (k, v) in pairs {
                d.insert(k.into(), v.into());
            }
            bplist::Value::Dictionary(d)
        };
        let audio = dict(vec![
            ("resourceID", 2),
            ("transferType", 1),
            ("transferPriority", 100),
            ("takeConstraint", 100),
            ("borrowConstraint", 100),
        ]);
        let mut modes = plist::Dictionary::new();
        modes.insert("resources".into(), bplist::Value::Array(vec![dict(screen), audio]));
        let mut d = plist::Dictionary::new();
        d.insert("modes".into(), bplist::Value::Dictionary(modes));
        bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap()
    }

    /// 2026-10-05: every session started with the car's UI taking the screen
    /// (Take, nice-to-have, Anytime) → we must echo 2/2 before taking it.
    /// 2026-10-04: the car's start-up screen BORROWS it (unborrow Never) → no
    /// sync, the car's Release hands it back.
    #[test]
    fn initial_screen_sync_only_when_the_car_takes_the_screen_anytime() {
        if std::env::var("RASCAR_INITIAL_SCREEN_SYNC").is_ok() {
            return;
        }
        let take = info_with_screen_mode(vec![
            ("resourceID", 1),
            ("transferType", 1),
            ("transferPriority", 100),
            ("takeConstraint", 100),
            ("borrowConstraint", 100),
        ]);
        assert_eq!(info_screen_take_constraint(&take), Some(CONSTRAINT_ANYTIME));
        assert!(initial_screen_sync(info_screen_take_constraint(&take)));

        let borrow = info_with_screen_mode(vec![
            ("resourceID", 1),
            ("transferType", 3),
            ("transferPriority", 500),
            ("unborrowConstraint", 1000),
        ]);
        assert_eq!(info_screen_take_constraint(&borrow), None);
        assert!(!initial_screen_sync(info_screen_take_constraint(&borrow)));

        let user_initiated = info_with_screen_mode(vec![
            ("resourceID", 1),
            ("transferType", 1),
            ("transferPriority", 500),
            ("takeConstraint", 500),
            ("borrowConstraint", 100),
        ]);
        assert_eq!(info_screen_take_constraint(&user_initiated), Some(500));
        assert!(!initial_screen_sync(info_screen_take_constraint(&user_initiated)));

        assert_eq!(info_screen_take_constraint(b"not a plist"), None);
        let no_modes = bplist::to_bytes(&bplist::Value::Dictionary(plist::Dictionary::new())).unwrap();
        assert_eq!(info_screen_take_constraint(&no_modes), None);
    }

    /// The sync body states MainScreen as the car's in both fields; the take
    /// that follows states it as ours.
    #[test]
    fn initial_sync_then_take_bodies_differ_only_in_the_screen_owner() {
        let screen = |body: &[u8]| {
            let v = bplist::from_bytes(body).unwrap();
            let res = v.as_dictionary().unwrap().get("params").unwrap().as_dictionary().unwrap().get("resources").unwrap().as_array().unwrap().clone();
            let s = res.iter().filter_map(|r| r.as_dictionary()).find(|r| plist_int(r.get("resourceID").unwrap()) == Some(1)).unwrap().clone();
            (plist_int(s.get("entity").unwrap()).unwrap(), plist_int(s.get("permanentEntity").unwrap()).unwrap())
        };
        let sync = modes_changed_body(false, true).unwrap();
        let take = modes_changed_body(false, false).unwrap();
        assert_eq!(screen(&sync), (ENTITY_ACCESSORY, ENTITY_ACCESSORY));
        assert_eq!(screen(&take), (ENTITY_CONTROLLER, ENTITY_CONTROLLER));
        assert_eq!(main_audio_entities(&sync), main_audio_entities(&take));
    }

    /// (entity, permanentEntity) of MainAudio in a `modesChanged` body.
    fn main_audio_entities(body: &[u8]) -> (i64, i64) {
        let v = bplist::from_bytes(body).unwrap();
        let d = v.as_dictionary().unwrap();
        assert_eq!(d.get("type").unwrap().as_string(), Some("modesChanged"));
        let res = d.get("params").unwrap().as_dictionary().unwrap().get("resources").unwrap().as_array().unwrap();
        let a = res.iter().filter_map(|r| r.as_dictionary()).find(|r| plist_int(r.get("resourceID").unwrap()) == Some(2)).unwrap();
        (plist_int(a.get("entity").unwrap()).unwrap(), plist_int(a.get("permanentEntity").unwrap()).unwrap())
    }

    #[test]
    fn state_modes_changed_reports_the_audio_borrow_then_its_release() {
        if std::env::var("RASCAR_CARPLAY_AUDIO").is_ok() || std::env::var("RASCAR_MODES_CLAIM_AUDIO").is_ok() {
            return;
        }
        // During the car's Borrow: the car is the current entity, we stay the owner.
        let borrowed = state_modes_changed_body(false, false, false, true).unwrap();
        assert_eq!(main_audio_entities(&borrowed), (ENTITY_ACCESSORY, ENTITY_CONTROLLER));
        // After its Unborrow: ours again, in both fields.
        let released = state_modes_changed_body(false, false, false, false).unwrap();
        assert_eq!(main_audio_entities(&released), (ENTITY_CONTROLLER, ENTITY_CONTROLLER));
    }

    #[test]
    fn change_modes_response_body_is_status0_with_params() {
        let body = build_change_modes_response_body().unwrap();
        let d = bplist::from_bytes(&body).unwrap();
        let d = d.as_dictionary().unwrap();
        assert_eq!(d.get("status").unwrap().as_signed_integer(), Some(0));
        assert!(d.get("params").unwrap().as_dictionary().is_some());
    }

    #[test]
    fn hid_touch_report_parses_5_bytes() {
        // down bit set, x=0x0102 LE, y=0x0304 LE.
        let t = parse_hid_touch(&[0x01, 0x02, 0x01, 0x04, 0x03]).unwrap();
        assert!(t.down);
        assert_eq!(t.x, 0x0102);
        assert_eq!(t.y, 0x0304);
        // up (bit clear).
        let up = parse_hid_touch(&[0x00, 0, 0, 0, 0]).unwrap();
        assert!(!up.down);
        // too short.
        assert!(parse_hid_touch(&[0x01, 0x02]).is_none());
    }

    #[test]
    fn audio_setup_body_and_port_extraction() {
        let body = build_audio_setup(4242, 55555, AudioMode::Media, None).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(s.get("type").unwrap().as_signed_integer(), Some(100));
        assert_eq!(s.get("audioType").unwrap().as_string(), Some("media"));
        assert_eq!(s.get("audioFormat").unwrap().as_signed_integer(), Some(1 << 15));
        assert_eq!(s.get("audioLatencyMs").unwrap().as_signed_integer(), Some(AUDIO_LATENCY_MS));
        assert_eq!(s.get("streamConnectionID").unwrap().as_signed_integer(), Some(4242));
        // Matches catplay StreamDescriptionAudio exactly: bool fields present …
        assert_eq!(s.get("input").unwrap().as_boolean(), Some(false));
        assert_eq!(s.get("supportsRTPPacketRedundancy").unwrap().as_boolean(), Some(false));
        assert_eq!(s.get("supportsHighAccuracyTimestamps").unwrap().as_boolean(), Some(true));
        // … and NO screen-only `uuid`, NO per-stream encryption key.
        assert!(s.get("uuid").is_none());
        assert!(s.get("ekey").is_none());
        assert!(s.get("eiv").is_none());

        // Unencrypted session → streamConnectionID must be 0 (catplay's et=NONE value).
        let body0 = build_audio_setup(0, 55555, AudioMode::Media, None).unwrap();
        let v0 = bplist::from_bytes(&body0).unwrap();
        let s0 = v0.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(s0.get("streamConnectionID").unwrap().as_signed_integer(), Some(0));

        // A reply with a type-100 dataPort + controlPort.
        let mut rs = plist::Dictionary::new();
        rs.insert("type".into(), 100i64.into());
        rs.insert("dataPort".into(), 50000i64.into());
        rs.insert("controlPort".into(), 50001i64.into());
        let mut reply = plist::Dictionary::new();
        reply.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(rs)]));
        let reply = bplist::Value::Dictionary(reply);
        assert_eq!(extract_audio_ports(&reply, 100), (Some(50000), Some(50001)));
        assert_eq!(extract_audio_ports(&reply, 110), (None, None));
    }

    #[test]
    fn telephony_setup_asks_for_the_microphone() {
        let mode = AudioMode::Telephony { format: 1 << 4, rate: 16000 };
        let body = build_audio_setup(7, 55555, mode, Some(40000)).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0].as_dictionary().unwrap();
        assert_eq!(s.get("type").unwrap().as_signed_integer(), Some(100));
        assert_eq!(s.get("audioType").unwrap().as_string(), Some("telephony"));
        assert_eq!(s.get("audioFormat").unwrap().as_signed_integer(), Some(1 << 4));
        assert_eq!(s.get("input").unwrap().as_boolean(), Some(true));
        assert_eq!(s.get("dataPort").unwrap().as_signed_integer(), Some(40000));
        // Media never carries a dataPort.
        let m = build_audio_setup(7, 55555, AudioMode::Media, Some(40000)).unwrap();
        let v = bplist::from_bytes(&m).unwrap();
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0].as_dictionary().unwrap();
        assert!(s.get("dataPort").is_none());
        assert_eq!(s.get("input").unwrap().as_boolean(), Some(false));
    }

    #[test]
    fn alert_setup_is_output_only_in_the_cars_format() {
        let mode = AudioMode::Alert { format: 1 << 11, rate: 44100, channels: 2 };
        let body = build_audio_setup(7, 55555, mode, Some(40000)).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0].as_dictionary().unwrap();
        assert_eq!(s.get("type").unwrap().as_signed_integer(), Some(100));
        assert_eq!(s.get("audioType").unwrap().as_string(), Some("alert"));
        assert_eq!(s.get("audioFormat").unwrap().as_signed_integer(), Some(1 << 11));
        assert_eq!(s.get("input").unwrap().as_boolean(), Some(false));
        assert!(s.get("dataPort").is_none());
        assert_eq!((mode.rate(), mode.channels(), mode.is_call()), (44100, 2, false));
    }

    #[test]
    fn alert_only_when_the_car_lists_it() {
        // No alert entry, or no audioFormats at all: never an alert stream
        // (unlike telephony, which has a fallback).
        assert_eq!(alert_mode_from_info(&info_with(vec![(100, "media", None, Some(1 << 15))])), None);
        let mut d = plist::Dictionary::new();
        d.insert("displays".into(), bplist::Value::Array(vec![]));
        assert_eq!(alert_mode_from_info(&bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap()), None);
        assert_eq!(alert_mode_from_info(b"not a plist"), None);
        // Listed but only in formats we do not generate (24-bit): none.
        assert_eq!(alert_mode_from_info(&info_with(vec![(100, "alert", None, Some(1 << 13))])), None);
        // 48 kHz stereo only (catplay receiver also offers it).
        assert_eq!(
            alert_mode_from_info(&info_with(vec![(100, "alert", None, Some(1 << 15))])),
            Some(AudioMode::Alert { format: 1 << 15, rate: 48000, channels: 2 })
        );
        // An alt-audio (type 101) "alert" entry is not a main-audio alert.
        assert_eq!(alert_mode_from_info(&info_with(vec![(101, "alert", None, Some(1 << 11))])), None);
    }

    #[test]
    fn line_state_picks_the_stream() {
        use crate::stream::call::Line;
        let tel = Some(AudioMode::Telephony { format: 1 << 6, rate: 24000 });
        let alert = Some(AudioMode::Alert { format: 1 << 11, rate: 44100, channels: 2 });
        let idle = Line { call: false, ring: false };
        let ring = Line { call: false, ring: true };
        let call = Line { call: true, ring: false };
        let both = Line { call: true, ring: true };
        assert_eq!(desired_audio_mode(idle, tel, alert), AudioMode::Media);
        assert_eq!(desired_audio_mode(ring, tel, alert), alert.unwrap());
        assert_eq!(desired_audio_mode(call, tel, alert), tel.unwrap());
        // Ringing during a meeting/call: the call keeps the car's audio.
        assert_eq!(desired_audio_mode(both, tel, alert), tel.unwrap());
        // A car without an alert stream keeps media while ringing.
        assert_eq!(desired_audio_mode(ring, tel, None), AudioMode::Media);
        // Without telephony a call keeps media, and a ring during it can still ring.
        assert_eq!(desired_audio_mode(call, None, alert), AudioMode::Media);
        assert_eq!(desired_audio_mode(both, None, alert), alert.unwrap());
        // phoneCall is announced for telephony, not for media, and not for an
        // alert unless the A/B flag asks for it (unset in tests).
        assert!(announces_phone_call(tel.unwrap()));
        assert!(!announces_phone_call(AudioMode::Media));
        if std::env::var_os("RASCAR_PHONE_RING_CALLSTATE").is_none() {
            assert!(!announces_phone_call(alert.unwrap()));
        }
    }

    fn info_with(formats: Vec<(i64, &str, Option<i64>, Option<i64>)>) -> Vec<u8> {
        let list = formats
            .into_iter()
            .map(|(ty, at, i, o)| {
                let mut d = plist::Dictionary::new();
                d.insert("type".into(), ty.into());
                d.insert("audioType".into(), at.into());
                if let Some(i) = i {
                    d.insert("audioInputFormats".into(), i.into());
                }
                if let Some(o) = o {
                    d.insert("audioOutputFormats".into(), o.into());
                }
                bplist::Value::Dictionary(d)
            })
            .collect();
        let mut d = plist::Dictionary::new();
        d.insert("audioFormats".into(), bplist::Value::Array(list));
        bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap()
    }

    #[test]
    fn telephony_format_follows_the_car() {
        // catplay receiver defaults: 16/24/32 kHz mono both ways → 24 kHz preferred.
        let both = (1 << 4) | (1 << 6) | (1 << 8);
        let info = info_with(vec![(100, "media", None, Some(1 << 15)), (100, "telephony", Some(both), Some(both))]);
        assert_eq!(telephony_mode_from_info(&info), Some(AudioMode::Telephony { format: 1 << 6, rate: 24000 }));
        // Only 16 kHz offered in BOTH directions.
        let info = info_with(vec![(100, "telephony", Some(1 << 4), Some((1 << 4) | (1 << 6)))]);
        assert_eq!(telephony_mode_from_info(&info), Some(AudioMode::Telephony { format: 1 << 4, rate: 16000 }));
        // A list without telephony: no hands-free path.
        let info = info_with(vec![(100, "media", None, Some(1 << 15))]);
        assert_eq!(telephony_mode_from_info(&info), None);
        // No list at all: catplay's 16 kHz default.
        let empty = bplist::to_bytes(&bplist::Value::Dictionary(Default::default())).unwrap();
        assert_eq!(telephony_mode_from_info(&empty), Some(AudioMode::Telephony { format: 1 << 4, rate: 16000 }));
    }

    #[test]
    fn car_microphone_packets_decrypt_with_the_input_key() {
        let secret = [9u8; 32];
        let scid = 123_456;
        // What the car does: seal with the Input key (catplay receiver write cipher).
        let key = crate::stream::crypto::derive_chacha_input_key(&secret, scid).unwrap();
        let mut car = AudioRtpCipher::ChaCha { aead: Box::new(ChaCha20Poly1305::new_from_slice(&key).unwrap()), counter: 5 };
        let pcm: Vec<u8> = (0..704u32).map(|i| i as u8).collect();
        let pkt = car.build_packet(3, 352, 0, &pcm);
        let mic = MicCipher::from_session(Some(&secret), scid);
        assert_eq!(mic.open(&pkt).as_deref(), Some(&pcm[..]));
        // Our own (Output-key) packets must NOT verify as microphone data.
        let mut ours = AudioRtpCipher::from_session(Some(&secret), scid);
        assert!(mic.open(&ours.build_packet(3, 352, 0, &pcm)).is_none());
        // Plaintext session: payload after the 12-byte header.
        assert_eq!(MicCipher::None.open(&AudioRtpCipher::None.build_packet(1, 0, 0, &pcm)).as_deref(), Some(&pcm[..]));
        assert!(MicCipher::None.open(&[0u8; 4]).is_none());
    }

    #[test]
    fn modes_changed_announces_the_call() {
        let body = build_modes_changed_body(true).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let app = v.as_dictionary().unwrap().get("params").unwrap().as_dictionary().unwrap().get("appStates").unwrap().as_array().unwrap().clone();
        let call = app[0].as_dictionary().unwrap();
        assert_eq!(call.get("appStateID").unwrap().as_signed_integer(), Some(2));
        assert_eq!(call.get("entity").unwrap().as_signed_integer(), Some(ENTITY_CONTROLLER));
    }

    #[test]
    fn audio_rtp_plaintext_packet_framing() {
        let mut c = AudioRtpCipher::None;
        let pcm = [0xABu8; 32];
        let pkt = c.build_packet(7, 0x0000_0160, 0, &pcm);
        // 12-byte RTP header + raw payload, no tag/nonce trailer.
        assert_eq!(pkt.len(), 12 + pcm.len());
        assert_eq!(pkt[0], 2u8 << 6); // v=2
        assert_eq!(pkt[1], AUDIO_PAYLOAD_TYPE);
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), 7); // seq
        assert_eq!(&pkt[12..], &pcm);
    }

    #[test]
    fn audio_rtp_chacha_packet_roundtrips_reference_layout() {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};

        // A paired session ChaCha-encrypts audio exactly like the screen; the
        // packet layout is catplay's `[hdr 12][ct N][tag 16][nonce 8]`.
        let secret = [0x42u8; 32];
        let scid: i64 = 1234;
        let mut c = AudioRtpCipher::from_session(Some(&secret), scid);
        assert_eq!(c.name(), "chacha20-poly1305");

        let pcm = [0x5Au8; 64];
        let (seq, ts, ssrc) = (9u16, 0xDEAD_BEEFu32, 0x1122_3344u32);
        let pkt = c.build_packet(seq, ts, ssrc, &pcm);
        assert_eq!(pkt.len(), 12 + pcm.len() + 16 + 8);
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), seq);
        assert_eq!(u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]), ts);
        assert_eq!(u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]), ssrc);

        // Decrypt as catplay's receiver does: key = DataStream-Output (same as TX),
        // nonce read from the 8-byte trailer, AAD = (ts<<32|ssrc) big-endian.
        let n = pkt.len();
        let nonce_ctr = u64::from_le_bytes(pkt[n - 8..].try_into().unwrap());
        assert_eq!(nonce_ctr, 0, "first packet nonce counter starts at 0");
        let ciphertext = &pkt[12..n - 8]; // ct + tag
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce_ctr.to_le_bytes());
        let aad = (((ts as u64) << 32) | ssrc as u64).to_be_bytes();
        let key = crate::stream::crypto::derive_chacha_key(&secret, scid).unwrap();
        let aead = ChaCha20Poly1305::new_from_slice(&key).unwrap();
        let pt = aead
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad: &aad })
            .expect("catplay-layout decrypt");
        assert_eq!(pt, pcm);

        // The nonce counter advances per packet.
        let pkt2 = c.build_packet(seq + 1, ts, ssrc, &pcm);
        let n2 = pkt2.len();
        assert_eq!(u64::from_le_bytes(pkt2[n2 - 8..].try_into().unwrap()), 1);
    }

    #[test]
    fn command_type_and_hid_report_extract() {
        // A changeModes command envelope.
        let mut d = plist::Dictionary::new();
        d.insert("type".into(), "changeModes".into());
        d.insert("params".into(), bplist::Value::Dictionary(plist::Dictionary::new()));
        let body = bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap();
        assert_eq!(command_type(&body).as_deref(), Some("changeModes"));

        // A flat hidSendReport with a 5-byte report.
        let mut h = plist::Dictionary::new();
        h.insert("type".into(), "hidSendReport".into());
        h.insert("hidReport".into(), bplist::Value::Data(vec![1, 2, 3, 4, 5]));
        h.insert("uuid".into(), "T".into());
        let body = bplist::to_bytes(&bplist::Value::Dictionary(h)).unwrap();
        assert_eq!(command_type(&body).as_deref(), Some("hidSendReport"));
        assert_eq!(command_hid_report(&body), Some(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn teardown_stream_type_110() {
        // send_teardown builds {streams:[{type:110, uuid}]}; verify the body shape
        // via the same plist construction it uses.
        let mut stream = plist::Dictionary::new();
        stream.insert("type".into(), 110i64.into());
        stream.insert("uuid".into(), "X".into());
        let mut d = plist::Dictionary::new();
        d.insert("streams".into(), bplist::Value::Array(vec![bplist::Value::Dictionary(stream)]));
        let body = bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap();
        let v = bplist::from_bytes(&body).unwrap();
        let s = v.as_dictionary().unwrap().get("streams").unwrap().as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        assert_eq!(s.get("type").unwrap().as_signed_integer(), Some(110));
        assert_eq!(s.get("uuid").unwrap().as_string(), Some("X"));
    }

    #[test]
    fn frame_clock_matches_the_shared_ntp_encoding() {
        // Screen-frame PTS uses the same 32.32 encoding as the timing port.
        assert_eq!(crate::ntp_time::encode(Duration::from_millis(1500)), 0x0000_0001_8000_0000);
        assert_eq!(Clock::new().now() >> 32, 0, "frame clock starts at zero seconds");
    }
}

#[cfg(test)]
mod audio_resource_consistency_tests {
    use super::*;

    /// `modesChanged` and the audio `SETUP` must never disagree.
    ///
    /// The bug this pins: we handed MainAudio to the ACCESSORY in `modesChanged`
    /// and then issued `SETUP` for a MainAudio stream anyway. The Jaguar never
    /// answered that SETUP and tore the USB gadget down ~2 s later. Whatever the
    /// knobs say, the entity we publish for resource 2 must match whether we
    /// actually open the stream.
    fn main_audio_entity(params: &plist::Dictionary) -> i64 {
        params
            .get("resources")
            .and_then(|v| v.as_array())
            .expect("resources array")
            .iter()
            .find_map(|r| {
                let d = r.as_dictionary()?;
                (d.get("resourceID")?.as_signed_integer()? == RESOURCE_MAIN_AUDIO)
                    .then(|| d.get("entity")?.as_signed_integer())?
            })
            .expect("MainAudio resource present")
    }

    #[test]
    fn claiming_audio_and_opening_the_stream_are_the_same_decision() {
        let claimed = claims_main_audio();
        let entity = main_audio_entity(&build_modes_params(false));

        if claimed {
            assert_eq!(entity, ENTITY_CONTROLLER, "claimed audio but gave it to the car");
            assert!(audio_enabled(), "claimed MainAudio but would not open the stream");
        } else {
            assert_eq!(entity, ENTITY_ACCESSORY, "did not claim audio but took it anyway");
            assert!(
                !audio_enabled(),
                "would SETUP a MainAudio stream after telling the car IT owns MainAudio \
                 — this is the contradiction that killed the session"
            );
        }
    }
}

#[cfg(test)]
mod keepalive_interval_tests {
    use super::*;

    /// The default must stay in a sane band: frequent enough that a single lost
    /// datagram is not significant, infrequent enough not to flood a receiver
    /// that validates the packet. (This does NOT encode any teardown theory --
    /// that one was refuted; see `spawn_keepalive`.)
    #[test]
    fn default_stays_in_a_sane_band() {
        const UPPER_BOUND: Duration = Duration::from_millis(3870);
        let d = match std::env::var("RASCAR_KEEPALIVE_SECS") {
            Ok(_) => Duration::from_secs(1), // the documented default
            Err(_) => keepalive_interval(),
        };
        assert!(d < UPPER_BOUND, "keepalive must stay frequent");
        assert!(d >= Duration::from_secs(1), "must not flood the receiver");
    }

    /// The override is what we A/B with at the car, so it must actually apply
    /// and must be clamped to a sane band.
    #[test]
    fn override_is_parsed_and_clamped() {
        for (raw, want) in [("1", 1u64), ("10", 10), ("30", 30), ("999", 30), ("0", 1), ("x", 1)] {
            let parsed = raw
                .parse::<u64>()
                .ok()
                .filter(|v| *v > 0)
                .unwrap_or(1)
                .clamp(1, 30);
            assert_eq!(parsed, want, "RASCAR_KEEPALIVE_SECS={raw}");
        }
    }
}

#[cfg(test)]
mod modes_reference_alignment_tests {
    use super::*;

    /// catplay's initial state grants BOTH resources to the Controller
    /// (`modes/change_modes.rs`, accessory variants commented out). Our
    /// `modesChanged` must say the same thing, because that message is what
    /// tells the head unit who owns the screen and the audio.
    #[test]
    fn both_resources_go_to_the_controller_by_default() {
        let params = build_modes_params(false);
        let resources = params
            .get("resources")
            .and_then(|v| v.as_array())
            .expect("modesChanged must carry a resources array");
        let mut seen = std::collections::HashMap::new();
        for r in resources {
            let d = r.as_dictionary().expect("resource entry must be a dict");
            let id = d.get("resourceID").and_then(|v| v.as_signed_integer()).expect("resourceID");
            let entity = d.get("entity").and_then(|v| v.as_signed_integer()).expect("entity");
            seen.insert(id, entity);
        }
        // Guard against the env overriding the default in a developer shell.
        if std::env::var("RASCAR_CARPLAY_AUDIO").is_ok() || std::env::var("RASCAR_MODES_CLAIM_AUDIO").is_ok() {
            return;
        }
        assert_eq!(seen.get(&RESOURCE_MAIN_SCREEN), Some(&ENTITY_CONTROLLER), "MainScreen -> Controller");
        assert_eq!(seen.get(&RESOURCE_MAIN_AUDIO), Some(&ENTITY_CONTROLLER), "MainAudio -> Controller");
    }

    /// The claim and the audio SETUP must never disagree again: telling the car
    /// "you own the audio" and then demanding the audio stream is the
    /// contradiction that made the Jaguar stop answering (cseq=8).
    #[test]
    fn claim_and_stream_stay_consistent() {
        assert_eq!(claims_main_audio(), audio_enabled());
    }
}

#[cfg(test)]
mod encryption_type_tests {
    use super::*;

    #[test]
    fn plaintext_setup_uses_apples_none_which_is_one_not_zero() {
        // kAirPlayEncryptionType_None = (1 << 0), AirPlayCommon.h:225.
        // 0 is kAirPlayEncryptionType_Undefined and drives the receiver into
        // _requestDecryptKey -> 466 Key Management Error.
        assert_eq!(
            encryption_type_none(),
            1,
            "et must be Apple's kAirPlayEncryptionType_None (1); 0 guarantees a 466"
        );
    }

    #[test]
    fn the_setup_plist_never_carries_the_undefined_encryption_type() {
        let bytes = build_initial_setup(6002, false).unwrap();
        let v = bplist::from_bytes(&bytes).unwrap();
        let et = v
            .as_dictionary()
            .unwrap()
            .get("et")
            .unwrap()
            .as_signed_integer()
            .unwrap();
        assert_ne!(et, 0, "et=0 is Undefined, not None — the receiver answers 466");
        assert_eq!(et, 1);
    }

    #[test]
    fn an_encrypted_setup_omits_et_entirely() {
        let bytes = build_initial_setup(6002, true).unwrap();
        let v = bplist::from_bytes(&bytes).unwrap();
        assert!(
            v.as_dictionary().unwrap().get("et").is_none(),
            "with a pairing secret the session negotiates ChaCha and `et` must be absent"
        );
    }
}

#[cfg(test)]
mod hid_touch_scaling_tests {
    use super::*;

    #[test]
    fn hid_logical_range_maps_across_the_whole_dashboard() {
        let dash = (1200, 512);
        // Real 5-byte report: down, X=16384, Y=16384 → centre of the dashboard.
        let t = parse_hid_touch(&[0x01, 0x00, 0x40, 0x00, 0x40]).unwrap();
        assert_eq!(map_coords(t.x as u32, t.y as u32, HID_TOUCH_LOGICAL, dash), (600, 256));
        assert_eq!(map_coords(0, 0, HID_TOUCH_LOGICAL, dash), (0, 0));
        assert_eq!(map_coords(32767, 32767, HID_TOUCH_LOGICAL, dash), (1199, 511));
    }

    #[test]
    fn display_size_as_source_would_pin_touches_to_the_corner() {
        // Regression guard: the old call passed the panel size as the source range.
        assert_eq!(map_coords(16384, 16384, (1200, 512), (1200, 512)), (1199, 511));
    }

    /// `(entity, permanentEntity)` of one resource in a modes `params` dict.
    fn resource_pair(p: &plist::Dictionary, id: i64) -> (i64, i64) {
        let d = p
            .get("resources")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_dictionary().unwrap())
            .find(|d| d.get("resourceID").unwrap().as_signed_integer() == Some(id))
            .unwrap();
        let get = |k: &str| d.get(k).unwrap().as_signed_integer().unwrap();
        (get("entity"), get("permanentEntity"))
    }

    fn body_params(body: &[u8]) -> plist::Dictionary {
        let v = bplist::from_bytes(body).unwrap();
        v.as_dictionary().unwrap().get("params").unwrap().as_dictionary().unwrap().clone()
    }

    /// Hand-back: MainScreen is the car's as entity AND permanentEntity
    /// (catplay `AccessoryHas`); MainAudio and phoneCall do not move.
    #[test]
    fn modes_changed_hands_the_screen_to_the_car_and_back() {
        let audio_owner = if claims_main_audio() { ENTITY_CONTROLLER } else { ENTITY_ACCESSORY };
        for phone_call in [false, true] {
            let car = body_params(&modes_changed_body(phone_call, true).unwrap());
            assert_eq!(resource_pair(&car, RESOURCE_MAIN_SCREEN), (ENTITY_ACCESSORY, ENTITY_ACCESSORY));
            assert_eq!(resource_pair(&car, RESOURCE_MAIN_AUDIO), (audio_owner, audio_owner));
            let ours = body_params(&modes_changed_body(phone_call, false).unwrap());
            assert_eq!(resource_pair(&ours, RESOURCE_MAIN_SCREEN), (ENTITY_CONTROLLER, ENTITY_CONTROLLER));
            assert_eq!(resource_pair(&ours, RESOURCE_MAIN_AUDIO), (audio_owner, audio_owner));
            // appStates identical in both owner states.
            assert_eq!(car.get("appStates"), ours.get("appStates"));
            let call = car.get("appStates").unwrap().as_array().unwrap()[0].as_dictionary().unwrap().clone();
            assert_eq!(call.get("entity").unwrap().as_signed_integer(), Some(if phone_call { ENTITY_CONTROLLER } else { 0 }));
        }
    }

    /// The ChangeModesResponse snapshot must tell the same screen story as
    /// the last modesChanged, also while the car borrows MainAudio.
    #[test]
    fn change_modes_snapshot_follows_the_screen_owner() {
        let audio_owner = if claims_main_audio() { ENTITY_CONTROLLER } else { ENTITY_ACCESSORY };
        let car_audio_borrow = modes_params(false, true, false, true);
        assert_eq!(resource_pair(&car_audio_borrow, RESOURCE_MAIN_SCREEN), (ENTITY_ACCESSORY, ENTITY_ACCESSORY));
        assert_eq!(resource_pair(&car_audio_borrow, RESOURCE_MAIN_AUDIO), (ENTITY_ACCESSORY, audio_owner));
        // A screen borrow on top of a hand-back leaves it the car's.
        let both = modes_params(false, true, true, false);
        assert_eq!(resource_pair(&both, RESOURCE_MAIN_SCREEN), (ENTITY_ACCESSORY, ENTITY_ACCESSORY));
        // A screen borrow while it is ours: the car is current, we stay owner.
        let borrowed = modes_params(false, false, true, false);
        assert_eq!(resource_pair(&borrowed, RESOURCE_MAIN_SCREEN), (ENTITY_ACCESSORY, ENTITY_CONTROLLER));
    }

    /// The process-wide default is the dashboard on screen: nothing in the
    /// normal start-up path announces the screen as the car's.
    #[test]
    fn default_modes_changed_keeps_the_screen() {
        assert!(!screen_with_car());
        let p = body_params(&build_modes_changed_body(false).unwrap());
        assert_eq!(resource_pair(&p, RESOURCE_MAIN_SCREEN), (ENTITY_CONTROLLER, ENTITY_CONTROLLER));
    }
}

#[cfg(test)]
mod hid_routing_tests {
    use super::*;
    use std::time::Duration;

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    // pcap captures/boot-20260922-181311-f4a15a9d/001-181311-net.pcap0 (plaintext
    // session). The `/info` `hidDevices` array, re-encoded as a binary plist by
    // Python plistlib — values verbatim (uuids, names, descriptors, ids).
    const INFO_HID_DEVICES: &str = "62706c6973743030a3011015d702030405060708090a0b0c0d0e0f5b646973706c6179555549445e686964436f756e747279436f64655d68696444657363726970746f725c68696450726f6475637449445b68696456656e646f724944546e616d6554757569645f102430303030303030302d303030302d343030302d383030302d30303030303030303030303010004f1047050d0904a101050d0922a102050d0933150025013500466400550f651175019501810275079501810105010930150026ff7f7510950181020931150026ff7f751095018102c0c01199991180875c546f7563682053637265656e5f102430303030303030302d303030302d343030302d383030302d303030303030303030303031d702030405060708090a11120d13144f101b050b0901a1011500250f050b0900092009210926750895018101c01199985f101054656c6570686f6e79206b65797061645f102430303030303030302d303030302d343030302d383030302d303030303030303030303032d702030405060708090a161718191a4f1022050c0901a10115002506050c0a00000ab0000ab1000ab5000ab600750895018101c01199971180885f100f436f6e73756d6572206b65797061645f102430303030303030302d303030302d343030302d383030302d3030303030303030303030330008000c001b0027003600440051005d00620067008e009000da00dd00e000ed01140123014101440157017e018d01b201b501b801ca0000000000000201000000000000001b000000000000000000000000000001f1";
    // The car's two `POST /command` bodies, byte for byte off the wire
    // (16:13:19.160 and 16:13:19.274): Consumer keypad `03`, then `00`.
    const CMD_NEXT_DOWN: &str = "62706c6973743030d3010203040506596869645265706f72745475756964547479706541035f102430303030303030302d303030302d343030302d383030302d3030303030303030303030335d68696453656e645265706f7274080f191e23254c000000000000010100000000000000070000000000000000000000000000005a";
    const CMD_NEXT_UP: &str = "62706c6973743030d3010203040506596869645265706f72745475756964547479706541005f102430303030303030302d303030302d343030302d383030302d3030303030303030303030335d68696453656e645265706f7274080f191e23254c000000000000010100000000000000070000000000000000000000000000005a";

    fn jaguar_devices() -> Vec<hid::HidDevice> {
        let arr = bplist::from_bytes(&unhex(INFO_HID_DEVICES)).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("hidDevices".into(), arr);
        parse_hid_devices(&bplist::to_bytes(&bplist::Value::Dictionary(info)).unwrap())
    }

    #[test]
    fn jaguar_info_declares_touch_telephony_and_consumer_devices() {
        let devs = jaguar_devices();
        let names: Vec<(&str, bool)> = devs.iter().map(|d| (d.name.as_str(), d.is_touch())).collect();
        assert_eq!(names, [("Touch Screen", true), ("Telephony keypad", false), ("Consumer keypad", false)]);
        assert_eq!(devs[2].uuid, "00000000-0000-4000-8000-000000000003");
    }

    #[test]
    fn real_scan_next_command_routes_to_a_next_key_press_then_release() {
        let devs = jaguar_devices();
        let mut keys = hid::KeyTracker::default();
        let t0 = Instant::now();
        let route = |keys: &mut hid::KeyTracker, body: &str, t: Instant| {
            let body = unhex(body);
            assert_eq!(command_type(&body).as_deref(), Some("hidSendReport"));
            let uuid = command_hid_uuid(&body);
            route_hid_report(&devs, keys, uuid.as_deref(), &command_hid_report(&body).unwrap(), t)
        };
        let down = route(&mut keys, CMD_NEXT_DOWN, t0);
        let next = hid::usage(hid::PAGE_CONSUMER, 0xB5);
        assert_eq!(down, HidRoute::Keys(vec![hid::KeyEdge::Down(next)]));
        assert_eq!(hid::CarKey::from_usage(next).map(|k| k.wire_name()), Some("next"));
        let up = route(&mut keys, CMD_NEXT_UP, t0 + Duration::from_millis(114));
        assert_eq!(up, HidRoute::Keys(vec![hid::KeyEdge::Up(next, Some(Duration::from_millis(114)))]));
    }

    #[test]
    fn touch_is_routed_by_device_not_by_length() {
        let devs = jaguar_devices();
        let mut keys = hid::KeyTracker::default();
        let now = Instant::now();
        // A real touch-down report (01 ff2f ff51, captured 2026-09-25) from the Touch Screen uuid.
        let touch = route_hid_report(&devs, &mut keys, Some("00000000-0000-4000-8000-000000000001"), &unhex("01ff2fff51"), now);
        assert_eq!(touch, HidRoute::Touch(HidTouch { down: true, x: 0x2fff, y: 0x51ff }));
        // A 1-byte report from the touch device is not a key, it is unmapped.
        let odd = route_hid_report(&devs, &mut keys, Some("00000000-0000-4000-8000-000000000001"), &[0x03], now);
        assert_eq!(odd, HidRoute::Unknown);
        // Telephony keypad: Hook Switch press, decoded but not a media key.
        let hook = route_hid_report(&devs, &mut keys, Some("00000000-0000-4000-8000-000000000002"), &[0x01], now);
        assert_eq!(hook, HidRoute::Keys(vec![hid::KeyEdge::Down(hid::usage(hid::PAGE_TELEPHONY, 0x20))]));
    }

    #[test]
    fn reports_without_a_known_uuid_keep_the_legacy_touch_path() {
        let mut keys = hid::KeyTracker::default();
        let now = Instant::now();
        let t = route_hid_report(&[], &mut keys, None, &unhex("00ad477f17"), now);
        assert_eq!(t, HidRoute::Touch(HidTouch { down: false, x: 0x47ad, y: 0x177f }));
        assert_eq!(route_hid_report(&[], &mut keys, Some("nope"), &[0x03], now), HidRoute::Unknown);
    }

    #[test]
    fn command_summary_names_the_fields_of_an_unknown_command() {
        let mut d = plist::Dictionary::new();
        d.insert("type".into(), "requestSiri".into());
        d.insert("siriAction".into(), 2i64.into());
        let s = command_summary(&bplist::to_bytes(&bplist::Value::Dictionary(d)).unwrap());
        assert_eq!(s, "{siriAction: Integer(2)}");
    }
}

#[cfg(test)]
mod mfisap_tests {
    use super::*;

    fn reply(cert: &[u8], sig: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = (0u8..32).collect();
        v.extend_from_slice(&(cert.len() as u32).to_be_bytes());
        v.extend_from_slice(cert);
        v.extend_from_slice(&(sig.len() as u32).to_be_bytes());
        v.extend_from_slice(sig);
        v
    }

    #[test]
    fn hello_is_version_one_then_the_key() {
        let key = [0xAB; 32];
        let hello = sap_hello(&key);
        assert_eq!(hello.len(), 33);
        assert_eq!(hello[0], 0x01);
        assert_eq!(&hello[1..], &key);
    }

    #[test]
    fn well_formed_reply_is_split_into_its_fields() {
        let body = reply(b"CERT-BYTES", &[9u8; 128]);
        let r = SapReply::parse(&body).unwrap();
        assert_eq!(r.accessory_key, core::array::from_fn::<u8, 32, _>(|i| i as u8));
        assert_eq!(r.certificate, b"CERT-BYTES");
        assert_eq!(r.signature, &[9u8; 128][..]);
        // Smallest legal reply: key + two empty fields = 40 bytes.
        let empty = reply(&[], &[]);
        assert_eq!(empty.len(), 40);
        assert!(SapReply::parse(&empty).is_ok());
        assert!(SapReply::parse(&empty[..39]).is_err());
    }

    #[test]
    fn malformed_replies_are_rejected() {
        let good = reply(b"cert", b"sig");
        assert!(SapReply::parse(&good[..20]).is_err(), "short key");
        assert!(SapReply::parse(&good[..38]).is_err(), "certificate cut off");
        assert!(SapReply::parse(&good[..good.len() - 1]).is_err(), "signature cut off");
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(SapReply::parse(&trailing).is_err(), "trailing byte");
        let mut huge = good.clone();
        huge[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(SapReply::parse(&huge).is_err(), "cert_len overflow");
    }
}
