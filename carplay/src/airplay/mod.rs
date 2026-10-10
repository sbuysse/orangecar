//! Phase 4 — **AirPlay/CarPlay control handshake + FairPlay SAP (the GO/NO-GO gate).**
//!
//! After Phase 3 yields an [`AirPlayEndpoint`] (the car's `_airplay._tcp` /
//! `_carplay._tcp` service on the CDC-NCM IPv6 link) and iAP2 `SessionReady`,
//! this module connects to that endpoint and runs the sender side of the
//! AirPlay-2 mirroring handshake so the car will accept a mirrored screen:
//!
//! ```text
//!   AirPlaySender::connect(endpoint)
//!        → GET /info            (receiver capabilities, Ed25519 pk, features)
//!        → pair()               (pair-setup SRP-6a / raw, then pair-verify)
//!        → fp_setup()           (FairPlay SAP: m1→m2→m3→m4  ← THE UNKNOWN)
//!        → setup_streams(w,h)   (RTSP SETUP + RECORD, x-apple-binary-plist)
//!        ⇒ SessionHandle        (video data TCP conn + stream key for Phase 5)
//! ```
//!
//! # Licensing (READ)
//!
//! This module and its siblings (`rtsp`, `pairing`, `fairplay`, `fairplay_data`)
//! are **ported from `github.com/omarroth/doubletake` (LGPL-3.0)** — the only
//! known open AirPlay-2 mirroring *sender*. The FairPlay pieces additionally
//! embed **Apple-derived FairPlay constants/tables** reproduced byte-for-byte;
//! reproducing them is a legal gray area. The `rascar-carplay`
//! crate is already GPL-3 (from the iAP2 port) — i.e. GPL/LGPL-viral — so
//! vendoring LGPL-3 code here is compatible.
//!
//! # Reality check
//!
//! **None of this is verifiable without a real car.** FairPlay acceptance at
//! `fp-setup` is THE open question. Every step logs a `→ … / ← …` line so a
//! car run shows exactly where it fails, and every hardware/behaviour assumption
//! is flagged `// TODO(hw)`.

// Phase 4 builds + unit-tests the full sender; Phase 5 wires the session handle,
// feedback keep-alive, and the remaining accessors into the runtime.
#![allow(dead_code)]

pub mod carplay_session;
pub mod fairplay;
pub mod hid;
mod fairplay_data;
pub mod pairing;
pub mod rtsp;

use anyhow::{bail, Context, Result};
use rand::RngCore;
use sha2::{Digest, Sha512};
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::host::AirPlayEndpoint;
use pairing::Pairing;
use rtsp::{bplist, Headers, RtspClient};

/// Every FairPlay SAP message on the wire starts with this 4-byte magic.
const FPLY_MAGIC: &[u8; 4] = b"FPLY";
/// Offset of the SAP message-type byte inside an `FPLY` frame. `FAIRPLAY_M1`
/// carries `0x01` here and `FPSAP_M3_PREFIX` carries `0x03`, so the receiver's
/// replies must carry `0x02` and `0x04`.
const FP_MSG_TYPE_OFFSET: usize = 6;
const FP_MSG_M2: u8 = 0x02;
const FP_MSG_M4: u8 = 0x04;

/// Validate that a `/fp-setup` reply really is the FairPlay message we expect.
///
/// This is what makes the Phase-4 gate interpretable: without it, ANY 2xx with a
/// dozen bytes reads as "FairPlay accepted". `min_len` is the length the next
/// step actually needs (m2 feeds `fpsap_exchange_m3`, which reads `m2[14..142]`).
fn check_fply_message(body: &[u8], want_type: u8, min_len: usize, step: &str) -> Result<()> {
    if body.len() < min_len {
        bail!(
            "fp-setup {step}: body too short — {} bytes, need at least {min_len}",
            body.len()
        );
    }
    if &body[..4] != FPLY_MAGIC {
        bail!(
            "fp-setup {step}: body is not a FairPlay message (expected magic 'FPLY', got {:02x?}) \
             — the receiver answered with something else entirely",
            &body[..4]
        );
    }
    let got = body[FP_MSG_TYPE_OFFSET];
    if got != want_type {
        bail!(
            "fp-setup {step}: FairPlay message type 0x{got:02x}, expected 0x{want_type:02x} — \
             this is an FPLY frame but NOT the reply we asked for (an error/reject type reads \
             like this)"
        );
    }
    Ok(())
}

/// Receiver capabilities parsed from `GET /info`.
#[derive(Debug, Default, Clone)]
pub struct ReceiverInfo {
    pub name: Option<String>,
    pub model: Option<String>,
    pub features: u64,
    pub status_flags: u64,
    /// Ed25519 public key (`pk`) — needed by the raw pair-verify path.
    pub pk: Vec<u8>,
    /// Primary display size in pixels, if advertised.
    pub display: Option<(u32, u32)>,
}

/// A live mirroring session. Phase 5 owns `data_conn` and pushes HEVC/H.264
/// access units to it (encrypted with `stream_key`/`stream_iv`).
pub struct SessionHandle {
    pub data_conn: TcpStream,
    pub data_port: u16,
    /// `shk` — the negotiated 16-byte stream key advertised in the video SETUP.
    /// Phase 5 derives the AES-CTR key/IV from this via `deriveVideoKeys`.
    pub stream_key: [u8; 16],
    /// `shiv` — the negotiated 16-byte stream IV.
    pub stream_iv: [u8; 16],
    /// The video stream's `streamConnectionID` — an input to BOTH the AES-CTR
    /// (`AirPlayStreamKey<id>`) and ChaCha20 (`DataStream-Salt<id>`) key derivations.
    pub stream_connection_id: i64,
    /// True when pair-verify negotiated an encrypted HAP channel — selects the
    /// ChaCha20-Poly1305 video cipher (HKDF from `shared_secret`) over AES-CTR.
    pub channel_encrypted: bool,
    /// The pair-verify X25519 ECDH shared secret (empty on the raw/plaintext
    /// pair path). Used as the ChaCha20 DataStream HKDF IKM.
    pub shared_secret: Vec<u8>,
    /// Receiver's advertised display size in pixels (for touch-coord mapping +
    /// the codec-header presentation size).
    pub display: Option<(u32, u32)>,
    pub session_uri: String,
}

pub struct AirPlaySender {
    rtsp: RtspClient,
    pairing: Pairing,
    info: ReceiverInfo,
    /// Final (SHA-512-hashed) FairPlay stream key.
    fp_key: Option<[u8; 16]>,
    fp_ekey: Option<[u8; 72]>,
    fp_iv: Option<[u8; 16]>,
    shared_secret: Vec<u8>,
    channel_encrypted: bool,
    /// The `Session` id the receiver returned in its SETUP reply. RTSP requires
    /// every subsequent request in the session to echo it.
    rtsp_session: Option<String>,
}

impl AirPlaySender {
    /// Step 1 — TCP-connect to the discovered endpoint.
    pub async fn connect(endpoint: &AirPlayEndpoint) -> Result<Self> {
        info!("→ airplay connect [{}]:{}", endpoint.address, endpoint.port);
        let rtsp = RtspClient::connect(endpoint)
            .await
            .context("airplay: TCP connect to endpoint")?;
        info!("← airplay connected to {}", rtsp.peer());
        Ok(Self {
            rtsp,
            pairing: Pairing::new(),
            info: ReceiverInfo::default(),
            fp_key: None,
            fp_ekey: None,
            fp_iv: None,
            shared_secret: Vec::new(),
            channel_encrypted: false,
            rtsp_session: None,
        })
    }

    /// Step 2 — GET /info.
    pub async fn get_info(&mut self) -> Result<&ReceiverInfo> {
        info!("→ GET /info");
        let resp = self
            .rtsp
            .request("GET", "/info", "application/x-apple-binary-plist", &[], &Vec::new())
            .await
            .context("GET /info request")?;
        info!("← GET /info: HTTP {} body={}B", resp.status, resp.body.len());
        if !resp.is_success() {
            bail!("GET /info returned HTTP {}", resp.status);
        }
        // Do NOT swallow this: `parse_info` is already field-tolerant (every
        // field falls back to a default), so an error here means the body is not
        // a plist dict at all. Silently substituting `ReceiverInfo::default()`
        // would drop `pk` (the raw pair-verify path needs it) and `display`
        // (Phase 5 maps touch coordinates against it) with no visible failure.
        self.info = parse_info(&resp.body).context("GET /info: response is not a usable plist")?;
        if !self.info.pk.is_empty() {
            self.pairing.set_server_ed_pub(&self.info.pk);
        }
        info!(
            "← /info: name={:?} model={:?} features=0x{:x} display={:?} pk={}B",
            self.info.name, self.info.model, self.info.features, self.info.display, self.info.pk.len()
        );
        Ok(&self.info)
    }

    /// Step 3 — pair-setup + pair-verify (transient / PIN-less).
    pub async fn pair(&mut self) -> Result<()> {
        info!("→ pairing");
        let result = self
            .pairing
            .pair_transient(&mut self.rtsp)
            .await
            .context("pairing")?;
        self.shared_secret = result.shared_secret;
        self.channel_encrypted = result.channel_encrypted;
        info!(
            "← pairing complete (channel_encrypted={}, shared_secret={}B)",
            self.channel_encrypted,
            self.shared_secret.len()
        );
        Ok(())
    }

    /// Step 4 — **FairPlay SAP fp-setup. THE go/no-go.**
    ///
    /// Posts m1, receives m2, computes m3 via the ported white-box transform,
    /// and posts m3. Acceptance requires BOTH a 2xx **and** a reply that really
    /// is a FairPlay `FPLY` message of the expected type — a 2xx alone proves
    /// nothing (see [`check_fply_message`]). Only then are the ekey and the AES
    /// stream key derived.
    pub async fn fp_setup(&mut self) -> Result<()> {
        // Feature-bit hint (doubletake checks bit for FPSAP support); we log but
        // don't hard-gate, since CarPlay receivers may advertise differently.
        info!("→ fp-setup m1 (16 bytes) [X-Apple-ET: 32]");
        let et_hdr: Headers = vec![("X-Apple-ET", "32".to_string())];
        let m2 = self
            .rtsp
            .request(
                "POST",
                "/fp-setup",
                "application/octet-stream",
                &fairplay::FAIRPLAY_M1,
                &et_hdr,
            )
            .await
            .context("fp-setup phase 1 (m1)")?;
        log_fp_reply("m1", &m2);
        if m2.status == 404 {
            bail!("← fp-setup REJECTED: /fp-setup returned 404 (receiver does not support FairPlay SAP)");
        }
        if !m2.is_success() {
            bail!("← fp-setup REJECTED at m1: HTTP {}", m2.status);
        }
        check_fply_message(&m2.body, FP_MSG_M2, 142, "m2")?;
        info!("← fp-setup m2 ({} bytes)", m2.body.len());

        let m3 = fairplay::fpsap_exchange_m3(&m2.body).context("FPSAP m2→m3 transform")?;
        info!("→ fp-setup m3 ({} bytes, FPLY-wrapped)", m3.len());
        let m4 = self
            .rtsp
            .request(
                "POST",
                "/fp-setup",
                "application/octet-stream",
                &m3,
                &et_hdr,
            )
            .await
            .context("fp-setup phase 2 (m3)")?;
        log_fp_reply("m3", &m4);
        if m4.status == 404 {
            bail!("← fp-setup REJECTED: /fp-setup returned 404 during phase 2");
        }
        if !m4.is_success() {
            bail!(
                "← fp-setup REJECTED at m3: HTTP {} — the car did NOT accept the FairPlay SAP m3. \
                 This is the Phase-4 go/no-go failing; the ported white-box transform or a \
                 constant is wrong, OR this receiver expects a different fp-setup variant.",
                m4.status
            );
        }
        // A 2xx alone is NOT acceptance: an embedded RTSP stack that answers 200
        // to everything, or a receiver returning a short error plist, would make
        // the project's single most important unknown record a false PASS. The
        // reply must actually be a FairPlay message of the expected type.
        check_fply_message(&m4.body, FP_MSG_M4, 12, "m4").context(
            "← fp-setup REJECTED at m3: the receiver answered 2xx but the body is not a \
             FairPlay m4 — treat this as a FAILED go/no-go, not a pass",
        )?;
        info!("← fp-setup ACCEPTED (m4 {} bytes) 🎉 — FairPlay SAP passed", m4.body.len());

        // Generate a per-session stream IV and build the ekey.
        let mut iv = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut iv);
        self.fp_iv = Some(iv);

        let ekey = fairplay::build_ekey();
        let fp_aes_key = fairplay::unwrap_fairplay_key(&m3, &ekey);
        self.fp_ekey = Some(ekey);

        // Hash with the pair-verify shared secret if we have one (the receiver
        // does SHA-512(fairplay_decrypt(ekey) || ecdh_secret)[:16]).
        let final_key = if !self.shared_secret.is_empty() {
            let mut h = Sha512::new();
            h.update(fp_aes_key);
            h.update(&self.shared_secret);
            let d = h.finalize();
            let mut k = [0u8; 16];
            k.copy_from_slice(&d[..16]);
            info!("fp-setup: stream key hashed with pair-verify shared secret");
            k
        } else {
            info!("fp-setup: using raw FairPlay stream key (no shared secret)");
            fp_aes_key
        };
        self.fp_key = Some(final_key);
        Ok(())
    }

    /// Step 5 — RTSP SETUP + RECORD for the mirroring video stream, returning a
    /// handle Phase 5 uses to push encoded frames.
    ///
    /// `// TODO(hw)`: doubletake sets up an audio (type 96) session first with an
    /// NTP timing UDP responder + a reverse event TCP listener, then the video
    /// (type 110) stream. We implement the video SETUP + RECORD here (the part
    /// that establishes the screen); the audio/timing/reverse-event plumbing is
    /// Phase-5 work and is only needed once frames actually flow. The exact
    /// ordering the CAR requires is unverified.
    pub async fn setup_streams(&mut self, width: u32, height: u32) -> Result<SessionHandle> {
        let (stream_key, stream_iv) = match (self.fp_key, self.fp_iv) {
            (Some(k), Some(iv)) => (k, iv),
            _ => bail!("setup_streams called before a successful fp_setup"),
        };

        info!("→ setup_streams (requested {width}x{height})");
        let session_uuid = generate_uuid();
        let device_id = crate::DEVICE_MAC.to_string(); // stable MAC (was random uuid_to_mac); sessionUUID stays random
        let stream_connection_id = (now_nanos() & 0x7FFF_FFFF_FFFF_FFFF) as i64;
        let peer = self.rtsp.peer();
        let video_uri = format!("rtsp://{}/{}", peer, stream_connection_id);

        info!("→ SETUP video (type=110) sci={stream_connection_id}");
        let mut video_stream = bplist::Value::Dictionary(Default::default());
        {
            let d = video_stream.as_dictionary_mut().unwrap();
            d.insert("type".into(), 110i64.into());
            d.insert("streamConnectionID".into(), stream_connection_id.into());
            d.insert("shk".into(), bplist::Value::Data(stream_key.to_vec()));
            d.insert("shiv".into(), bplist::Value::Data(stream_iv.to_vec()));
        }

        let mut setup = bplist::Value::Dictionary(Default::default());
        {
            let d = setup.as_dictionary_mut().unwrap();
            d.insert("deviceID".into(), device_id.clone().into());
            d.insert("macAddress".into(), device_id.into());
            d.insert("sessionUUID".into(), session_uuid.clone().into());
            d.insert("sourceVersion".into(), "280.33".into());
            d.insert("isScreenMirroringSession".into(), true.into());
            d.insert("timingProtocol".into(), "None".into()); // TODO(hw): "NTP" + UDP timing (Phase 5)
            d.insert("model".into(), "Linux".into());
            d.insert("name".into(), "RasCar".into());
            if let Some(ekey) = self.fp_ekey {
                d.insert("ekey".into(), bplist::Value::Data(ekey.to_vec()));
                d.insert("eiv".into(), bplist::Value::Data(stream_iv.to_vec()));
            }
            d.insert("streams".into(), bplist::Value::Array(vec![video_stream]));
        }
        let body = bplist::to_bytes(&setup).context("marshal video SETUP plist")?;

        let resp = self
            .rtsp
            .request(
                "SETUP",
                &video_uri,
                "application/x-apple-binary-plist",
                &body,
                &Vec::new(),
            )
            .await
            .context("SETUP (video)")?;
        info!("← SETUP video: HTTP {} body={}B", resp.status, resp.body.len());
        if !resp.is_success() {
            bail!("← SETUP video REJECTED: HTTP {}", resp.status);
        }
        // RTSP requires RECORD (and every later request in the session) to echo
        // the Session id the SERVER handed back in its SETUP reply. Sending our
        // own `sessionUUID` instead earns a `454 Session Not Found` — a spurious
        // late failure right after the hard part (fp-setup) succeeded.
        let rtsp_session = resp
            .headers
            .get("session")
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
            .filter(|s| !s.is_empty());
        match &rtsp_session {
            Some(s) => info!("← SETUP video: RTSP Session id '{s}'"),
            // TODO(hw): unverified — a receiver that omits Session on SETUP may
            // still accept RECORD without one. We send no Session rather than a
            // made-up value, so a 454 here means "the car wanted one" and not
            // "we sent the wrong one".
            None => warn!("← SETUP video: no Session header in the reply; RECORD will omit it"),
        }
        self.rtsp_session = rtsp_session.clone();
        let video_resp = bplist::from_bytes(&resp.body).context("parse SETUP response plist")?;
        let data_port = extract_stream_data_port(&video_resp, 110)
            .context("no video data port in SETUP response")?;
        info!("← SETUP video OK, dataPort={data_port}");

        let data_addr = match peer {
            std::net::SocketAddr::V6(v6) => std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                *v6.ip(),
                data_port,
                0,
                v6.scope_id(),
            )),
            std::net::SocketAddr::V4(v4) => {
                std::net::SocketAddr::V4(std::net::SocketAddrV4::new(*v4.ip(), data_port))
            }
        };
        let data_conn = TcpStream::connect(data_addr)
            .await
            .with_context(|| format!("connect video data port {data_addr}"))?;
        data_conn.set_nodelay(true).ok();
        info!("← video data channel connected: {data_addr}");

        info!("→ RECORD {video_uri}");
        let mut record_headers: Headers = vec![
            ("Range", "npt=0-".to_string()),
            ("RTP-Info", "seq=0;rtptime=0".to_string()),
        ];
        if let Some(s) = &rtsp_session {
            record_headers.push(("Session", s.clone()));
        }
        let rec = self
            .rtsp
            .request("RECORD", &video_uri, "", &[], &record_headers)
            .await
            .context("RECORD")?;
        info!("← RECORD: HTTP {} body={}B", rec.status, rec.body.len());
        if !rec.is_success() {
            bail!("← RECORD REJECTED: HTTP {}", rec.status);
        }
        info!("← RECORD OK — mirroring session established");

        Ok(SessionHandle {
            data_conn,
            data_port,
            stream_key,
            stream_iv,
            stream_connection_id,
            channel_encrypted: self.channel_encrypted,
            shared_secret: self.shared_secret.clone(),
            display: self.info.display,
            session_uri: video_uri,
        })
    }

    /// Best-effort `/feedback` keep-alive (Phase 5 calls this periodically).
    pub async fn feedback(&mut self) -> Result<()> {
        let mut headers: Headers = Vec::new();
        if let Some(s) = &self.rtsp_session {
            headers.push(("Session", s.clone()));
        }
        let resp = self
            .rtsp
            .request("POST", "/feedback", "", &[], &headers)
            .await?;
        if !resp.is_success() {
            warn!("/feedback returned HTTP {}", resp.status);
        }
        Ok(())
    }

    pub fn receiver_info(&self) -> &ReceiverInfo {
        &self.info
    }
}

/// Run the whole Phase-4 handshake end-to-end against a discovered endpoint,
/// logging each milestone. Returns the live `AirPlaySender` (kept alive by
/// Phase 5 so the RTSP control connection — and thus the `/feedback` keep-alive
/// — stays open) plus the session handle on success; a precise error naming the
/// failing step otherwise.
pub async fn run(
    endpoint: &AirPlayEndpoint,
    width: u32,
    height: u32,
) -> Result<(AirPlaySender, SessionHandle)> {
    let mut sender = AirPlaySender::connect(endpoint).await?;
    sender.get_info().await?;
    sender.pair().await?;
    sender.fp_setup().await?;
    let session = sender.setup_streams(width, height).await?;
    Ok((sender, session))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Log an fp-setup reply unambiguously BEFORE it is judged: HTTP status, body
/// length, first bytes and the FPLY message-type byte (ACCEPT vs REJECT vs 404).
fn log_fp_reply(step: &str, resp: &rtsp::RtspResponse) {
    let fply = resp.body.len() > FP_MSG_TYPE_OFFSET && resp.body.starts_with(FPLY_MAGIC);
    info!(
        "← fp-setup {step} reply: HTTP {} body={}B first=[{}] fply_magic={fply} msg_type={}",
        resp.status,
        resp.body.len(),
        crate::debug::hex_prefix(&resp.body, 16),
        if fply { format!("0x{:02x}", resp.body[FP_MSG_TYPE_OFFSET]) } else { "n/a".to_string() }
    );
}

fn parse_info(body: &[u8]) -> Result<ReceiverInfo> {
    let v = bplist::from_bytes(body)?;
    let d = v.as_dictionary().context("info: not a dict")?;
    let mut info = ReceiverInfo::default();
    info.name = d.get("name").and_then(|x| x.as_string()).map(str::to_string);
    info.model = d.get("model").and_then(|x| x.as_string()).map(str::to_string);
    info.features = d
        .get("features")
        .and_then(|x| x.as_unsigned_integer().or_else(|| x.as_signed_integer().map(|n| n as u64)))
        .unwrap_or(0);
    info.status_flags = d
        .get("statusFlags")
        .and_then(|x| x.as_unsigned_integer().or_else(|| x.as_signed_integer().map(|n| n as u64)))
        .unwrap_or(0);
    if let Some(pk) = d.get("pk").and_then(|x| x.as_data()) {
        info.pk = pk.to_vec();
    }
    if let Some(displays) = d.get("displays").and_then(|x| x.as_array()) {
        if let Some(first) = displays.first().and_then(|x| x.as_dictionary()) {
            let get = |k: &str| first.get(k).and_then(|x| x.as_signed_integer());
            let (mut w, mut h) = (get("widthPixels").unwrap_or(0), get("heightPixels").unwrap_or(0));
            if w <= 0 || h <= 0 {
                w = get("width").unwrap_or(0);
                h = get("height").unwrap_or(0);
            }
            if w > 0 && h > 0 {
                info.display = Some((w as u32, h as u32));
            }
        }
    }
    Ok(info)
}

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
            if let Some(port) = d
                .get("dataPort")
                .and_then(|x| x.as_signed_integer().or_else(|| x.as_unsigned_integer().map(|n| n as i64)))
            {
                if (0..=u16::MAX as i64).contains(&port) {
                    return Some(port as u16);
                }
            }
        }
    }
    None
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

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

/// Derive a stable pseudo-MAC ("xx:xx:xx:xx:xx:xx") from a UUID, as doubletake's
/// `uuidToMAC` does for the SETUP `deviceID`.
fn uuid_to_mac(uuid: &str) -> String {
    let digest = Sha512::digest(uuid.as_bytes());
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
    )
}
