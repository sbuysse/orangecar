//! iAP2 protocol stack — **Device side** (we impersonate the iPhone; the car is
//! the Accessory), running over a generic async byte transport.
//!
//! # Licensing
//!
//! This module is **derived from `usenocturne/iap2-rs`** (link layer, control
//! session, auth logic), which is licensed **GPL-3.0**. Because we vendor and
//! adapt that code, **the `rascar-carplay` crate is therefore GPL-3.0** (viral
//! copyleft — accepted by the project owner). Upstream:
//! <https://github.com/usenocturne/iap2-rs>.
//!
//! Per-file notes record exactly what was lifted verbatim vs rewritten/inverted:
//!   - [`packet`] — lifted nearly verbatim (only the error-import path changed).
//!   - [`link`]   — send/receive/framing lifted verbatim; the `bluer` transport
//!                  replaced with a generic `T: AsyncRead + AsyncWrite + Unpin`;
//!                  negotiation **inverted** to the Device role (we answer the
//!                  accessory's SYN instead of sending it).
//!   - [`control`] — control framing kept; identification & auth **inverted** so
//!                  we initiate (Device) rather than respond (Accessory).
//!
//! # Transport-generic
//!
//! Nothing here touches USB. The stack drives any `T: AsyncRead + AsyncWrite +
//! Unpin`, so it is unit-tested today with an in-memory `tokio::io::duplex`
//! mock accessory and will bind to the real USB-host bulk endpoint in Phase 3.
//!
//! # Device-side flow ([`Iap2Device::run_until_ready`])
//!
//! ```text
//!   1. Link negotiate   — receive accessory SYN → send SYN-ACK → receive ACK
//!   2. Identification    — send StartIdentification → recv IdentificationInformation
//!                          → send IdentificationAccepted            (Apple AIS §13)
//!   3. Authentication    — send RequestAuthenticationCertificate → recv cert
//!                          → send RequestAuthenticationChallengeResponse(32B)
//!                          → recv AuthenticationResponse
//!                          → send AuthenticationSucceeded (accept-all)  (AIS §12)
//!   4. ready             — session established; hand the transport to Phase 3.
//! ```
//!
//! Accept-all authentication: we are the verifier but carry no MFi coprocessor,
//! so the certificate and challenge-response signature are **not** validated yet
//! (`// TODO: verify cert against Apple MFi root CA` — optional later hardening).

#![allow(dead_code)] // Phase 2 builds + tests the stack; Phase 3 wires it into main's runtime.

pub mod control;
pub mod error;
pub mod link;
pub mod nowplaying;
pub mod packet;

use bytes::Bytes;
use std::convert::Infallible;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

pub use control::{AccessoryIdentification, ControlSession};
pub use error::{Iap2Error, Result};
pub use link::{Iap2Link, Iap2Transport, LinkConfig};

/// Spec deadline (Apple AIS §13): the accessory must answer StartIdentification
/// with IdentificationInformation within 1 s. We allow generous slack.
const IDENTIFICATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Spec deadline (Apple AIS §12): once authentication starts it must complete
/// within 15 s (challenge response ≤ 2 s).
const AUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Log a Device-side bring-up state transition (grep marker `iAP2 DEVICE STATE`).
fn device_state(state: &str, detail: &str) {
    info!("iAP2 DEVICE STATE: {state} ({detail}; t+{} ms)", crate::debug::elapsed_ms());
}

/// Result of a completed Device-side session bring-up.
#[derive(Debug, Clone)]
pub struct SessionReady {
    /// Control session id negotiated at the link layer.
    pub control_session_id: u8,
    /// What the accessory reported in IdentificationInformation.
    pub identification: AccessoryIdentification,
    /// The accessory's authentication certificate bytes (unverified).
    pub certificate: Bytes,
}

/// Optional async hooks so Phase 3 can observe bring-up and then take over.
#[async_trait::async_trait]
pub trait Iap2Observer: Send {
    async fn on_link_established(&mut self, _control_session_id: u8) {}
    async fn on_identification(&mut self, _info: &AccessoryIdentification) {}
    async fn on_certificate(&mut self, _cert: &[u8]) {}
    async fn on_ready(&mut self, _ready: &SessionReady) {}
}

/// No-op observer.
#[async_trait::async_trait]
impl Iap2Observer for () {}

/// The Device-side iAP2 driver: owns the transport (via the link) and sequences
/// link negotiation → identification → authentication.
pub struct Iap2Device<T> {
    link: Iap2Link<T>,
    /// What the accessory declared about itself, captured at identification.
    ///
    /// Kept so the service loop can honour `MessagesReceivedFromDevice`: iAP2
    /// requires the device to send only messages the accessory said it accepts.
    identification: Option<AccessoryIdentification>,
}

impl<T> Iap2Device<T>
where
    T: Iap2Transport,
{
    pub fn new(transport: T, config: LinkConfig) -> Self {
        Iap2Device {
            link: Iap2Link::new(transport, config),
            identification: None,
        }
    }

    pub fn with_default_config(transport: T) -> Self {
        Self::new(transport, LinkConfig::default())
    }

    pub fn is_ready(&self) -> bool {
        self.link.is_established()
    }

    /// The control session id (valid only after [`Iap2Device::negotiate`]).
    pub fn control_session_id(&self) -> u8 {
        self.link.control_session_id()
    }

    /// Step 1: link-layer negotiation (Device answers the accessory's SYN).
    pub async fn negotiate(&mut self) -> Result<()> {
        self.link.negotiate_device().await
    }

    /// Step 2: §13 identification. Requires [`Iap2Device::negotiate`] first.
    pub async fn identify(&mut self) -> Result<AccessoryIdentification> {
        let control = ControlSession::new(self.link.control_session_id());
        control.run_identification(&mut self.link).await
    }

    /// Step 3: §12 accept-all authentication. `challenge` is the 32-byte random
    /// value we present to the accessory — Phase 3 supplies fresh randomness.
    pub async fn authenticate(&mut self, challenge: &[u8; 32]) -> Result<Bytes> {
        let control = ControlSession::new(self.link.control_session_id());
        control.run_authentication(&mut self.link, challenge).await
    }

    /// Run the full Device-side bring-up, invoking `observer` hooks along the way.
    /// Returns once the session is established and ready to hand off to Phase 3.
    pub async fn run_until_ready<O: Iap2Observer>(
        &mut self,
        challenge: &[u8; 32],
        observer: &mut O,
    ) -> Result<SessionReady> {
        device_state("Negotiating", "waiting for the accessory SYN");
        self.negotiate()
            .await
            .inspect_err(|e| device_state("Failed", &format!("link negotiation: {e}")))?;
        let control_session_id = self.link.control_session_id();
        observer.on_link_established(control_session_id).await;

        device_state("Identifying", &format!("control session 0x{control_session_id:02X}"));
        let identification = timeout(IDENTIFICATION_TIMEOUT, self.identify())
            .await
            .map_err(|_| Iap2Error::Timeout)
            .and_then(|r| r)
            .inspect_err(|e| {
                device_state("Failed", &format!("identification (budget {IDENTIFICATION_TIMEOUT:?}): {e}"))
            })?;
        observer.on_identification(&identification).await;
        // Publish who the head unit is (/var/lib/rascar/headunit.json). Off the
        // async thread; observes only, sends nothing.
        crate::headunit::record_identification(&identification);

        device_state("Authenticating", "requesting the accessory certificate");
        let certificate = timeout(AUTHENTICATION_TIMEOUT, self.authenticate(challenge))
            .await
            .map_err(|_| Iap2Error::Timeout)
            .and_then(|r| r)
            .inspect_err(|e| {
                device_state("Failed", &format!("authentication (budget {AUTHENTICATION_TIMEOUT:?}): {e}"))
            })?;
        observer.on_certificate(&certificate).await;
        device_state("Ready", "authentication accepted");

        identification.log_declared_messages();
        self.identification = Some(identification.clone());
        let ready = SessionReady {
            control_session_id,
            identification,
            certificate,
        };
        observer.on_ready(&ready).await;
        info!("iAP2 Device session ready (control session 0x{:02X})", control_session_id);
        Ok(ready)
    }

    /// Keep the established link serviced for the rest of the session.
    ///
    /// `run_until_ready` only takes the link as far as `SessionReady`; the
    /// accessory then keeps sending control-session traffic for the whole
    /// mirroring session. If nobody reads it, nothing is ACKed either: the
    /// accessory's link layer retransmits, exhausts its budget and drops CarPlay
    /// mid-stream. This loop reads every packet and lets the link layer ACK it.
    ///
    /// An idle link is normal, so a read timeout is not an error here — the loop
    /// has no deadline of its own and returns only when the link genuinely fails.
    /// The [`Infallible`] success type makes "it exited cleanly" unrepresentable:
    /// a return is always a failure the caller must surface.
    pub async fn service_link(mut self) -> Result<Infallible> {
        let session_id = self.link.control_session_id();
        info!(
            "iAP2 link service loop started (control session 0x{:02X})",
            session_id
        );

        device_state("ServiceLoop", &format!("servicing control session 0x{session_id:02X}"));
        let mut assembler = control::ControlAssembler::default();
        // Whether the PowerUpdate pair has already gone out for this session.
        let mut power_flushed = false;

        // Unprompted device→accessory declarations sent right after authentication
        // (sequence cross-checked against catplay-labs/catplay's transmitter, used
        // as a reference for the wire format). Without at least the identity
        // updates the car considers the CarPlay handshake incomplete. Byte layouts
        // follow Apple's iAP2Constants.h and the catplay CSM schema.
        //
        // DeviceLanguageUpdate (0x4E0A) and DeviceTimeUpdate (0x4E0B) are the two
        // device notifications the Jaguar declares it expects (0x1D01 param 0x0007
        // want-list: 4E09 4E0A 4E0B 4E04) and that a real iPhone pushes unsolicited
        // after identification — we previously sent only 4E09.
        // GPGSV (per-satellite SNR) — A/B knob RASCAR_IAP2_GPGSV, default ON
        // because the Jaguar declares it (LocationInformationComponent sub-id 19).
        // Off sends the exact 0xFFFA of every session before it.
        let gpgsv = crate::debug::env_flag("RASCAR_IAP2_GPGSV", true);
        let location_params = control::start_location_information_params(self.identification.as_ref(), gpgsv);
        info!(
            "iAP2 location: car offers [{}], requesting params {:?} (RASCAR_IAP2_GPGSV={})",
            self.identification.as_ref().map(|i| i.location_sentences().join(" ")).unwrap_or_default(),
            location_params.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            if gpgsv { "on" } else { "off" }
        );
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for (mid, params, what) in [
            // DeviceUUIDUpdate (0x4E0C): uuid CsmString (UTF-8 + NUL).
            (0x4E0Cu16, vec![(0x0000u16, b"3F2A9C71-8E4D-4B0A-9C1E-7A6F2D5B3E80\0".to_vec())], "DeviceUUIDUpdate"),
            // DeviceInformationUpdate (0x4E09): device_name (UTF-8 + NUL).
            (0x4E09, vec![(0x0000, b"iPhone\0".to_vec())], "DeviceInformationUpdate"),
            // DeviceLanguageUpdate (0x4E0A): device_language = "en".
            (0x4E0A, control::device_language_update_params(), "DeviceLanguageUpdate"),
            // DeviceTimeUpdate (0x4E0B): current time as CFAbsoluteTime, UTC offsets.
            (0x4E0B, control::device_time_update_params(unix_secs), "DeviceTimeUpdate"),
            // WirelessCarPlayUpdate (0x4E0D). OFF unless RASCAR_IAP2_WIRELESS_CARPLAY
            // says otherwise — see `control::WirelessCarPlay`. Placed here, between
            // DeviceUUIDUpdate and DeviceTransportIdentifierNotification, because
            // that is the order both the CarlinKit device-side table and the
            // production head-unit log show.
            //
            // `params()` returning None cannot skip an entry in this array, so the
            // message is emitted with an empty parameter list and the send is
            // suppressed below instead.
            (0x4E0D, control::WirelessCarPlay::from_env().params().unwrap_or_default(),
             "WirelessCarPlayUpdate"),
            // DeviceTransportIdentifierNotification (0x4E0E): param 0 = Bluetooth
            // transport id (MAC string), param 1 = USB transport id (string).
            //
            // This tells the accessory WHICH TRANSPORT this iAP2 device is reachable
            // on, and a wired head unit has to bind its CarPlay session to a USB
            // transport. We never sent it. A production head-unit logcat
            // (HaToan/carplay-wifi-extractor iap2.log, a shipping CarPlaySDK) shows a
            // real iPhone pushing it in the post-auth burst:
            //   "Received iAP2 device transport identifier notification:
            //    bluetoothTransportIdentifier=94:89:78:xx:xx:xx,
            //    usbTransportIdentifier=0000815000190xxxxxxxxxxx"
            // Message id and direction are Apple ground truth: spec.rs, generated from
            // `iap2messages-internal.i2mspecarchive` in Xcode 27's CarPlaySimulator —
            // (0x4E0E, "DeviceTransportIdentifierNotification", Source::Device).
            //
            // Sent even though this car does not list 0x4E0E in its
            // MessagesReceivedFromDevice: the same production log shows a head unit
            // answering an undeclared message with "Unrecognized iAP2 message
            // received. Type: 0X4E0A" and carrying on, so an extra notification is
            // logged and ignored, not fatal. There is no dedicated env switch for it; like
            // every message it is subject to the RASCAR_IAP2_GATE declaration check.
            (0x4E0E, control::device_transport_identifier_params(), "DeviceTransportIdentifierNotification"),
            // StartLocationInformation (0xFFFA): one empty flag per sentence type.
            // GGA/RMC/PASCD always; GPGSV only when declared by the car and
            // RASCAR_IAP2_GPGSV is on — see `start_location_information_params`.
            (0xFFFA, location_params, "StartLocationInformation"),
        ] {
            // A post-auth message with no parameters is one we deliberately
            // skipped (currently only 0x4E0D when its knob is off). Every message
            // in this burst legitimately carries at least one parameter, so an
            // empty list is unambiguous.
            if params.is_empty() {
                debug!("iAP2: skipping post-auth {what} (0x{mid:04X}) — disabled");
                continue;
            }
            if let Err(e) = self.send_control(session_id, mid, &params).await {
                warn!("iAP2: failed to send post-auth {what} (0x{mid:04X}): {e}");
            }
        }

        // StopLocationInformation (0xFFFC): only sent with RASCAR_IAP2_KEEP_LOCATION=0.
        //
        // We used to cancel the location subscription in the same breath as opening
        // it, which no phone does, so 2026-09-21 the Stop was removed. Measured at
        // the car afterwards (boot-20260921-191924): the Jaguar then streams
        // `0xFFFB LocationInformation` at us — and those three messages are the LAST
        // thing it ever sends before going silent and dropping the USB device:
        //
        //     3.4027  0xFFFB LocationInformation
        //     3.4080  0xFFFB LocationInformation
        //     3.4134  0xFFFB LocationInformation
        //     (silence; our next packet is never ACKed; device gone)
        //
        // Device lifetime also moved the wrong way, 6293-6630 ms before the change
        // to 5484-6157 ms after it. That is suggestive, not conclusive — the run
        // also changed in other ways — so the Stop is kept as a KNOB. The default
        // leaves the subscription OPEN, because the car's GPS stream is consumed
        // (car GPS over iAP2); RASCAR_IAP2_KEEP_LOCATION=0 sends the Stop again.
        if !crate::debug::env_flag("RASCAR_IAP2_KEEP_LOCATION", true) {
            if let Err(e) = self.send_control(session_id, 0xFFFC, &[]).await {
                warn!("iAP2: failed to send post-auth StopLocationInformation: {e}");
            }
        } else {
            info!(
                "iAP2: leaving the location subscription OPEN (default; RASCAR_IAP2_KEEP_LOCATION=0 stops it) — \
                 the car will stream 0xFFFB LocationInformation at us"
            );
        }

        // Heartbeat interval. Measured at the Jaguar: after auth it subscribes to
        // NowPlaying/Power updates, then if the phone goes silent on the control
        // link it tears the USB session down ~7.9 s from enumeration (during
        // streaming this looks like "sent one keyframe then disconnected"). A real
        // phone keeps pushing updates. `receive_data` blocks up to ~46 s, so poll it
        // with a short timeout and, on each idle window, push a NowPlayingUpdate to
        // keep the control link warm. The reader is cancellation-safe.
        // DEFAULT OFF since 2026-09-20. The comment above describes the theory this
        // heartbeat was built on; it did not hold — with the heartbeat running the
        // car still tore the session down, and the 0x5001 in flight when it died was
        // this heartbeat's. iAP2 has no keepalive message: the link layer's own ACKs
        // are the liveness mechanism, which is why the reference implementation sends
        // nothing on an idle control session and emits `NowPlayingUpdate` only in
        // direct response to `StartNowPlayingUpdates`. Re-pushing an identical media
        // delta every 1.2 s during a screen session is something no phone does, and
        // if the car ever sends `StopNowPlayingUpdates` (0x5002 — which it declares
        // it can send, and which we do not even name) every later beat is an update
        // on a closed subscription. `RASCAR_IAP2_HEARTBEAT=on` restores it for an A/B.
        let heartbeat_enabled = crate::debug::env_flag("RASCAR_IAP2_HEARTBEAT", false);
        info!(
            "iAP2 NowPlaying heartbeat: {} (RASCAR_IAP2_HEARTBEAT)",
            if heartbeat_enabled { "ENABLED" } else { "disabled" }
        );
        const HEARTBEAT: Duration = Duration::from_millis(1200);

        // Real NowPlaying (see `nowplaying`): a poller task reads rascar-app while
        // the car's subscription is open; this loop coalesces, diffs and sends.
        let np_cfg = nowplaying::Config::from_env();
        let file_session_id = self.link.file_transfer_session_id();
        let artwork_enabled = np_cfg.artwork && file_session_id.is_some();
        info!(
            "iAP2 NowPlaying: metadata {} (RASCAR_NOWPLAYING), artwork {} (RASCAR_NOWPLAYING_ARTWORK; \
             file transfer session {})",
            if np_cfg.enabled { "ENABLED" } else { "disabled" },
            if artwork_enabled { "ENABLED" } else { "disabled" },
            file_session_id.map(|s| format!("0x{s:02X}")).unwrap_or_else(|| "not declared".into())
        );
        let (sub_tx, sub_rx) = tokio::sync::watch::channel(false);
        let (text_tx, mut text_rx) = tokio::sync::watch::channel(None::<nowplaying::Observed>);
        let (art_tx, mut art_rx) = tokio::sync::watch::channel(None::<nowplaying::ArtworkReady>);
        let _poller = np_cfg.enabled.then(|| {
            let cfg = nowplaying::Config { artwork: artwork_enabled, ..np_cfg.clone() };
            nowplaying::spawn_poller(cfg, sub_rx, text_tx, art_tx)
        });
        let mut np = nowplaying::Publisher::new(np_cfg.app.clone());
        let mut transfers = nowplaying::FileTransfers::default();
        // Set by StopNowPlayingUpdates (0x5002): from then on no 0x5001 at all,
        // the heartbeat included, until the car subscribes again.
        let mut np_stopped = false;

        loop {
            let now = tokio::time::Instant::now();

            // 1. A NowPlaying update whose rate gate is open.
            if let Some(out) = np.poll(now) {
                match out {
                    nowplaying::Outgoing::Update { params, track_changed, summary } => {
                        if track_changed {
                            info!("iAP2 NowPlaying: track -> {summary}");
                        } else {
                            debug!("iAP2 NowPlaying: playback -> {summary}");
                        }
                        if let Err(e) = self.send_control(session_id, 0x5001, &params).await {
                            warn!("iAP2 NowPlayingUpdate failed: {e}");
                            device_state("Failed", &format!("NowPlayingUpdate: {e}"));
                            return Err(e);
                        }
                    }
                    nowplaying::Outgoing::Artwork { file_id, params, data } => {
                        info!("iAP2 NowPlaying: artwork id 0x{file_id:02X}, {} byte(s)", data.len());
                        if let Err(e) = self.send_control(session_id, 0x5001, &params).await {
                            warn!("iAP2 NowPlayingUpdate (artwork id) failed: {e}");
                            device_state("Failed", &format!("NowPlayingUpdate: {e}"));
                            return Err(e);
                        }
                        transfers.enqueue(file_id, data);
                    }
                }
                continue;
            }

            // 2. Artwork transfer: abandon an unanswered one, else send the next
            //    datagram — but never ahead of car traffic already queued.
            if let Some((id, outcome)) = transfers.on_timeout(now) {
                warn!("iAP2 file transfer 0x{id:02X}: {outcome:?} — no reply from the car, abandoned");
            }
            if let Some(fsid) = file_session_id.filter(|_| !self.link.has_pending()) {
                let chunk = self.link.max_payload().saturating_sub(2);
                if let Some(dg) = transfers.next_datagram(chunk, now) {
                    debug!(
                        "iAP2 file transfer -> session=0x{fsid:02X} id=0x{:02X} op=0x{:02X} {} byte(s)",
                        dg[0],
                        dg[1],
                        dg.len() - 2
                    );
                    if let Err(e) = self.link.send_data(fsid, Bytes::from(dg)).await {
                        warn!("iAP2 file transfer datagram failed: {e}");
                        device_state("Failed", &format!("file transfer: {e}"));
                        return Err(e);
                    }
                    continue;
                }
            }

            // 3. Wait for the car, a new observation, or the next deadline.
            let deadline = [np.next_deadline(), transfers.next_deadline()].into_iter().flatten().min();
            let received = tokio::select! {
                biased;
                r = timeout(HEARTBEAT, self.link.receive_data()) => r,
                Ok(()) = text_rx.changed(), if np_cfg.enabled => {
                    let obs = text_rx.borrow_and_update().clone();
                    if let Some(obs) = obs {
                        np.offer(obs);
                    }
                    continue;
                }
                Ok(()) = art_rx.changed(), if artwork_enabled => {
                    let art = art_rx.borrow_and_update().clone();
                    if let Some(art) = art {
                        np.offer_artwork(art);
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or(now)), if deadline.is_some() => continue,
            };

            let packet = match received {
                Ok(Ok(packet)) => packet,
                Ok(Err(Iap2Error::Timeout)) => {
                    debug!("iAP2 link idle — no accessory traffic in the last window, still serving");
                    continue;
                }
                Ok(Err(e)) => {
                    warn!("iAP2 link service loop stopping: {}", e);
                    device_state("Failed", &format!("service loop: {e}"));
                    return Err(e);
                }
                Err(_elapsed) => {
                    if !heartbeat_enabled {
                        debug!("iAP2 link idle for {HEARTBEAT:?} — not sending anything (heartbeat disabled)");
                        continue;
                    }
                    if np_stopped {
                        debug!("iAP2 heartbeat: car sent StopNowPlayingUpdates — not beating");
                        continue;
                    }
                    // Idle for HEARTBEAT with no accessory traffic — push a
                    // NowPlayingUpdate keepalive so the car does not consider the
                    // control link dead and drop CarPlay mid-stream. Once real
                    // metadata went out, the beat repeats the CURRENT playback
                    // state instead of the "Stopped" reset, which would blank the
                    // car's now-playing display every beat.
                    let beat = match np.heartbeat(tokio::time::Instant::now()) {
                        Some(params) => vec![(0x5001, params, "NowPlayingUpdate(current playback)")],
                        None => control::subscription_replies(0x5000),
                    };
                    for (mid, params, what) in beat {
                        if let Err(e) = self.send_control(session_id, mid, &params).await {
                            warn!("iAP2 heartbeat {what} (0x{mid:04X}) failed: {e}");
                            device_state("Failed", &format!("heartbeat: {e}"));
                            return Err(e);
                        }
                    }
                    continue;
                }
            };

            match packet.session_id {
                Some(sid) if Some(sid) == file_session_id => {
                    // The car's side of a file transfer (Start/Pause/Success/...).
                    let p = &packet.payload;
                    info!(
                        "iAP2 file transfer <- session=0x{sid:02X}: {}",
                        crate::debug::hex_prefix(p, 16)
                    );
                    if let Some((id, outcome)) = transfers.on_datagram(p) {
                        info!("iAP2 file transfer 0x{id:02X}: {outcome:?}");
                    }
                    continue;
                }
                Some(sid) if sid != session_id => {
                    // EA sessions (e.g. mySPIN over iAP2) land here: keep the head of
                    // the payload so a capture shows what the accessory speaks.
                    info!(
                        "Acknowledged {} byte(s) on non-control session 0x{:02X}: {}",
                        packet.payload.len(),
                        sid,
                        crate::debug::hex_prefix(&packet.payload, 64)
                    );
                    continue;
                }
                _ => {}
            }
            assembler.push(&packet.payload);
            loop {
                match assembler.next_message() {
                    Ok(Some(msg)) => {
                        info!(
                            "iAP2 control message 0x{:04X} ({}) from accessory ({} parameter(s)) — acknowledged",
                            msg.msg_id,
                            control::msg_name(msg.msg_id),
                            msg.params.len()
                        );
                        control::trace_received(session_id, &msg);

                        // The car's own GPS, arriving as one NMEA sentence per
                        // 0xFFFB. We used to ACK these and throw them away. Sink
                        // them so the dashboard can use the vehicle's fix — which
                        // is better than a bare receiver, being already fused with
                        // wheel speed, so it survives tunnels and car parks.
                        if msg.msg_id == 0xFFFB {
                            match control::location_nmea(&msg) {
                                Some(s) if control::nmea_checksum_ok(s) => {
                                    // GSV comes in ~3 parts a second and is already
                                    // in car-gps.nmea: at INFO it would triple the
                                    // journal shipped over 4G for no diagnostic gain.
                                    if s.contains("GSV,") {
                                        debug!("car GPS: {s}");
                                    } else {
                                        info!("car GPS: {s}");
                                    }
                                    crate::iap2::sink_car_nmea(s);
                                    crate::gps_clock::on_car_nmea(s);
                                }
                                Some(s) => warn!("car GPS: BAD NMEA checksum, dropped: {s}"),
                                None => {}
                            }
                        }

                        // Answer the car's control subscriptions. Leaving
                        // StartNowPlayingUpdates (0x5000) and StartPowerUpdates
                        // (0xAE00) unanswered is what made the Jaguar tear down USB
                        // ~7.5 s after enumeration (behaviour cross-checked against
                        // the catplay reference transmitter; layouts from its CSM
                        // schema).
                        //
                        // PowerUpdate is flushed exactly ONCE per session. This car
                        // sends both StartPowerUpdates (0xAE00) and PowerSourceUpdate
                        // (0xAE03); answering each with the pair emitted four
                        // PowerUpdates where a phone sends two. `subscription_replies`
                        // no longer answers 0xAE03, so map it onto the 0xAE00 reply
                        // here only while nothing has been flushed — that still covers
                        // a car that leads with 0xAE03.
                        if msg.msg_id == 0xAE03 {
                            // The car's USB power budget for us -> headunit.json.
                            crate::headunit::record_power_source(&msg);
                        }
                        let reply_to = match msg.msg_id {
                            0xAE03 if !power_flushed => 0xAE00,
                            0xAE03 => {
                                debug!("iAP2: PowerSourceUpdate (0xAE03) — PowerUpdate already flushed, not repeating");
                                // 0 has no subscription reply, so nothing is sent.
                                0
                            }
                            other => other,
                        };
                        if reply_to == 0xAE00 {
                            power_flushed = true;
                        }
                        for (mid, params, what) in control::subscription_replies(reply_to) {
                            if let Err(e) = self.send_control(session_id, mid, &params).await {
                                warn!("iAP2: failed to answer 0x{:04X} with {what} (0x{mid:04X}): {e}", msg.msg_id);
                            }
                        }

                        // NowPlaying subscription state. The reset reply above has
                        // just gone out; real updates follow once the rate gate
                        // reopens and the poller has something to say.
                        if msg.msg_id == 0x5000 && np_cfg.enabled {
                            let requested = nowplaying::Requested::from_start_message(&msg);
                            info!(
                                "iAP2 NowPlaying: car subscribed (media {:?}, playback {:?})",
                                requested.media, requested.playback
                            );
                            np.on_subscribe(requested, tokio::time::Instant::now());
                            np_stopped = false;
                            sub_tx.send_replace(true);
                        } else if msg.msg_id == 0x5000 {
                            np_stopped = false;
                        }
                        if msg.msg_id == 0x5002 {
                            info!("iAP2 NowPlaying: car sent StopNowPlayingUpdates — no further 0x5001");
                            np.on_stop();
                            np_stopped = true;
                            sub_tx.send_replace(false);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!(
                            "Control stream out of sync ({}) — discarding {} buffered byte(s)",
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

    /// Send a device→accessory control-session message and log it.
    async fn send_control(&mut self, session_id: u8, msg_id: u16, params: &[(u16, Vec<u8>)]) -> Result<()> {
        // Honour the accessory's `MessagesReceivedFromDevice` declaration. Sending
        // a message the accessory never said it accepts is the textbook iAP2
        // protocol violation a head unit tears the session down on, and we used to
        // send at least one (0x4E0C DeviceUUIDUpdate) that this car does not list.
        // A missing/empty declaration blocks nothing. `RASCAR_IAP2_GATE=off` for an A/B.
        if crate::debug::env_flag("RASCAR_IAP2_GATE", true) {
            if let Some(ident) = &self.identification {
                if !ident.accepts(msg_id) {
                    warn!(
                        "iAP2: NOT sending 0x{msg_id:04X} ({}) — the accessory did not declare it in \
                         MessagesReceivedFromDevice",
                        control::msg_name(msg_id)
                    );
                    return Ok(());
                }
            }
        }
        let borrowed: Vec<(u16, &[u8])> = params.iter().map(|(id, d)| (*id, d.as_slice())).collect();
        control::trace_message("->", session_id, msg_id, &borrowed);
        let bytes = control::encode_control_message(msg_id, &borrowed);
        self.link.send_data(session_id, bytes).await
    }

    /// Consume the driver and recover the transport (Phase 3 hand-off).
    pub fn into_transport(self) -> T {
        self.link.into_transport()
    }
}

#[cfg(test)]
mod tests {
    use super::control::{
        encode_control_message, parse_control_message, ControlMessage,
        MSG_AUTH_CERTIFICATE, MSG_AUTH_RESPONSE, MSG_AUTH_SUCCEEDED,
        MSG_IDENTIFICATION_ACCEPTED, MSG_IDENTIFICATION_INFORMATION,
        MSG_REQUEST_AUTH_CERTIFICATE, MSG_REQUEST_AUTH_CHALLENGE_RESPONSE,
        MSG_START_IDENTIFICATION,
    };
    use super::packet::{ControlByte, Iap2Packet, PacketType};
    use super::*;
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    /// Minimal in-memory *accessory* that mirrors iap2-rs's accessory link
    /// bookkeeping: it originates the SYN, completes the handshake, and then
    /// ACKs / emits control-session DATA packets with the exact sequence-number
    /// convention the Device link expects.
    struct MockAccessory {
        stream: DuplexStream,
        buf: BytesMut,
        /// Next sequence number for accessory-originated DATA / bare ACKs.
        send_seq: u8,
        /// Last device DATA seq we received (used as the ack field we echo).
        last_dev_seq: u8,
    }

    impl MockAccessory {
        fn new(stream: DuplexStream) -> Self {
            MockAccessory {
                stream,
                buf: BytesMut::new(),
                send_seq: 0,
                last_dev_seq: 0,
            }
        }

        fn try_parse(&mut self) -> Option<Iap2Packet> {
            loop {
                if self.buf.len() < 4 {
                    return None;
                }
                if self.buf[0] != 0xFF || self.buf[1] != 0x5A {
                    let _ = self.buf.split_to(1);
                    continue;
                }
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < len {
                    return None;
                }
                let data = self.buf.split_to(len);
                return Some(Iap2Packet::from_bytes(&data).expect("valid packet from device"));
            }
        }

        async fn read_packet(&mut self) -> Iap2Packet {
            loop {
                if let Some(p) = self.try_parse() {
                    return p;
                }
                let mut tmp = [0u8; 1024];
                let n = self.stream.read(&mut tmp).await.expect("read");
                assert!(n > 0, "device closed the transport unexpectedly");
                self.buf.extend_from_slice(&tmp[..n]);
            }
        }

        async fn write_packet(&mut self, pkt: &Iap2Packet) {
            self.stream.write_all(&pkt.to_bytes()).await.expect("write");
            self.stream.flush().await.expect("flush");
        }

        /// Accessory drives link init: SYN → (recv SYN-ACK) → completing ACK.
        async fn handshake(&mut self) {
            self.write_packet(&Iap2Packet::syn(0x9D)).await;

            let syn_ack = self.read_packet().await;
            assert_eq!(syn_ack.control.packet_type, PacketType::SynAck);
            assert_eq!(syn_ack.ack, 0x9D, "SYN-ACK must ack our SYN seq");

            self.send_seq = 0x9E;
            // completing ACK: ack the device's SYN-ACK seq.
            self.write_packet(&Iap2Packet::ack(self.send_seq, syn_ack.seq))
                .await;
        }

        /// Receive one device-originated control message and ACK it.
        async fn recv_control(&mut self) -> ControlMessage {
            let pkt = self.read_packet().await;
            assert!(!pkt.payload.is_empty(), "expected a DATA packet");
            self.last_dev_seq = pkt.seq;
            self.write_packet(&Iap2Packet::ack(self.send_seq, pkt.seq))
                .await;
            parse_control_message(&pkt.payload).expect("parse device control msg")
        }

        /// Send one accessory-originated control message and consume the device's ACK.
        async fn send_control(&mut self, control_session_id: u8, msg_id: u16, params: &[(u16, &[u8])]) {
            let payload = encode_control_message(msg_id, params);
            let pkt = Iap2Packet::data(self.send_seq, self.last_dev_seq, control_session_id, payload);
            self.write_packet(&pkt).await;

            let ack = self.read_packet().await;
            assert_eq!(ack.control.packet_type, PacketType::Ack);
            assert_eq!(ack.ack, self.send_seq, "device must ack the seq we sent");
            self.send_seq = self.send_seq.wrapping_add(1);
        }
    }

    // (a) Link DETECT/SYN/SYN-ACK/ACK negotiation round-trip.
    #[tokio::test]
    async fn link_negotiation_roundtrip() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;
        });

        let mut device = Iap2Device::with_default_config(dev_io);
        device.negotiate().await.expect("negotiation");
        assert!(device.is_ready());
        // Control session id parsed from the canonical SYN session table.
        assert_eq!(device.control_session_id(), 0x0A);

        acc.await.unwrap();
    }

    // (b) Identification: assert we emit StartIdentification then IdentificationAccepted,
    //     and that we parse a fed-in IdentificationInformation.
    #[tokio::test]
    async fn identification_exchange() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;

            let start = acc.recv_control().await;
            assert_eq!(start.msg_id, MSG_START_IDENTIFICATION, "device must send StartIdentification first");

            acc.send_control(
                0x0A,
                MSG_IDENTIFICATION_INFORMATION,
                &[(0x0000, b"RasCar HU\0"), (0x0002, b"OrangeCar\0")],
            )
            .await;

            let accepted = acc.recv_control().await;
            assert_eq!(accepted.msg_id, MSG_IDENTIFICATION_ACCEPTED, "device must accept identification");
        });

        let mut device = Iap2Device::with_default_config(dev_io);
        device.negotiate().await.expect("negotiation");
        let ident = device.identify().await.expect("identification");
        assert_eq!(ident.name.as_deref(), Some("RasCar HU"));
        assert_eq!(ident.manufacturer.as_deref(), Some("OrangeCar"));

        acc.await.unwrap();
    }

    // (c) Authentication: assert we emit RequestAuthCert → RequestChallenge(32B) →
    //     AuthenticationSucceeded, given a canned cert + response (accept-all).
    #[tokio::test]
    async fn authentication_exchange() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);
        let challenge = [0x5Au8; 32];

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;

            let req_cert = acc.recv_control().await;
            assert_eq!(req_cert.msg_id, MSG_REQUEST_AUTH_CERTIFICATE);

            acc.send_control(0x0A, MSG_AUTH_CERTIFICATE, &[(0x0000, b"CANNED-CERT-BYTES")])
                .await;

            let req_challenge = acc.recv_control().await;
            assert_eq!(req_challenge.msg_id, MSG_REQUEST_AUTH_CHALLENGE_RESPONSE);
            let got = req_challenge.param(0x0000).expect("challenge param");
            assert_eq!(got.len(), 32, "challenge must be 32 bytes");
            assert_eq!(&got[..], &[0x5Au8; 32], "device must forward our challenge verbatim");

            acc.send_control(0x0A, MSG_AUTH_RESPONSE, &[(0x0000, b"CANNED-SIGNATURE")])
                .await;

            let succeeded = acc.recv_control().await;
            assert_eq!(
                succeeded.msg_id, MSG_AUTH_SUCCEEDED,
                "device must accept-all and send AuthenticationSucceeded"
            );
        });

        let mut device = Iap2Device::with_default_config(dev_io);
        device.negotiate().await.expect("negotiation");
        let cert = device.authenticate(&challenge).await.expect("authentication");
        assert_eq!(&cert[..], b"CANNED-CERT-BYTES");

        acc.await.unwrap();
    }

    // An EAK must NOT advance our own send sequence. The DATA packet in flight is
    // identified by the seq it was sent with; moving `self.seq` made the awaited
    // `ack.ack == seq` comparison unmatchable, turning a recoverable link event
    // into a hard timeout that aborted the whole CarPlay session.
    #[tokio::test]
    async fn eak_does_not_break_the_pending_ack_match() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;

            // The device's StartIdentification DATA packet.
            let data = acc.read_packet().await;
            assert!(!data.payload.is_empty(), "expected StartIdentification DATA");
            let dev_seq = data.seq;
            acc.last_dev_seq = dev_seq;

            // Interject an EAK before acknowledging anything.
            acc.send_seq = acc.send_seq.wrapping_add(1);
            let eak = Iap2Packet::new(ControlByte::new(PacketType::Eak), acc.send_seq, dev_seq);
            acc.write_packet(&eak).await;

            let eak_ack = acc.read_packet().await;
            assert_eq!(
                eak_ack.control.packet_type,
                PacketType::Ack,
                "device must answer an EAK with an ACK"
            );

            // Only now acknowledge the original DATA packet.
            acc.send_seq = acc.send_seq.wrapping_add(1);
            acc.write_packet(&Iap2Packet::ack(acc.send_seq, dev_seq)).await;

            // Identification then proceeds normally.
            acc.send_seq = acc.send_seq.wrapping_add(1);
            acc.send_control(0x0A, MSG_IDENTIFICATION_INFORMATION, &[(0x0000, b"Car\0")])
                .await;

            // The device's next DATA packet must carry the very next sequence
            // number: an EAK is not one of our transmissions and must not burn a
            // send sequence number.
            let accepted = acc.read_packet().await;
            assert_eq!(
                accepted.seq,
                dev_seq.wrapping_add(1),
                "EAK must not consume one of the device's send sequence numbers"
            );
            assert_eq!(
                parse_control_message(&accepted.payload).unwrap().msg_id,
                MSG_IDENTIFICATION_ACCEPTED
            );
            acc.write_packet(&Iap2Packet::ack(acc.send_seq, accepted.seq))
                .await;
        });

        let mut device = Iap2Device::with_default_config(dev_io);
        device.negotiate().await.expect("negotiation");
        let ident = timeout(Duration::from_secs(5), device.identify())
            .await
            .expect("identify must not hang after an EAK")
            .expect("identification");
        assert_eq!(ident.name.as_deref(), Some("Car"));

        acc.await.unwrap();
    }

    // A missing certificate parameter is a protocol mismatch, not an empty
    // certificate. Substituting a default used to carry a mismatched accessory
    // all the way to "**** SessionReady ****".
    #[tokio::test]
    async fn authentication_fails_when_certificate_parameter_is_absent() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);
        let challenge = [0x5Au8; 32];

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;
            assert_eq!(acc.recv_control().await.msg_id, MSG_REQUEST_AUTH_CERTIFICATE);
            // Correct message id, but the certificate lives under another id.
            acc.send_control(0x0A, MSG_AUTH_CERTIFICATE, &[(0x0007, b"NOT-THE-CERT")])
                .await;
        });

        let mut device = Iap2Device::with_default_config(dev_io);
        device.negotiate().await.expect("negotiation");
        let err = timeout(Duration::from_secs(5), device.authenticate(&challenge))
            .await
            .expect("authenticate must not hang")
            .expect_err("a certificate message without parameter 0x0000 must fail");
        assert!(
            matches!(err, Iap2Error::AuthenticationFailed(_)),
            "expected AuthenticationFailed, got {err:?}"
        );

        acc.await.unwrap();
    }

    // Full driver flow through run_until_ready, exercising the observer hooks.
    #[tokio::test]
    async fn run_until_ready_full_flow() {
        let (dev_io, acc_io) = tokio::io::duplex(4096);
        let challenge = [0x11u8; 32];

        let acc = tokio::spawn(async move {
            let mut acc = MockAccessory::new(acc_io);
            acc.handshake().await;
            // Identification
            assert_eq!(acc.recv_control().await.msg_id, MSG_START_IDENTIFICATION);
            acc.send_control(0x0A, MSG_IDENTIFICATION_INFORMATION, &[(0x0000, b"Car\0")])
                .await;
            assert_eq!(acc.recv_control().await.msg_id, MSG_IDENTIFICATION_ACCEPTED);
            // Authentication
            assert_eq!(acc.recv_control().await.msg_id, MSG_REQUEST_AUTH_CERTIFICATE);
            acc.send_control(0x0A, MSG_AUTH_CERTIFICATE, &[(0x0000, b"CERT")]).await;
            assert_eq!(acc.recv_control().await.msg_id, MSG_REQUEST_AUTH_CHALLENGE_RESPONSE);
            acc.send_control(0x0A, MSG_AUTH_RESPONSE, &[(0x0000, b"SIG")]).await;
            assert_eq!(acc.recv_control().await.msg_id, MSG_AUTH_SUCCEEDED);
        });

        #[derive(Default)]
        struct Hooks {
            link: bool,
            ident: bool,
            cert: bool,
            ready: bool,
        }
        #[async_trait::async_trait]
        impl Iap2Observer for Hooks {
            async fn on_link_established(&mut self, sid: u8) {
                assert_eq!(sid, 0x0A);
                self.link = true;
            }
            async fn on_identification(&mut self, info: &AccessoryIdentification) {
                assert_eq!(info.name.as_deref(), Some("Car"));
                self.ident = true;
            }
            async fn on_certificate(&mut self, cert: &[u8]) {
                assert_eq!(cert, b"CERT");
                self.cert = true;
            }
            async fn on_ready(&mut self, ready: &SessionReady) {
                assert_eq!(ready.control_session_id, 0x0A);
                self.ready = true;
            }
        }

        let mut device = Iap2Device::with_default_config(dev_io);
        let mut hooks = Hooks::default();
        let ready = device
            .run_until_ready(&challenge, &mut hooks)
            .await
            .expect("full bring-up");

        assert_eq!(ready.control_session_id, 0x0A);
        assert_eq!(&ready.certificate[..], b"CERT");
        assert!(hooks.link && hooks.ident && hooks.cert && hooks.ready, "all hooks fired");

        // Transport is recoverable for Phase 3 hand-off.
        let _io = device.into_transport();

        acc.await.unwrap();
    }
}

/// Where the car's NMEA sentences are published for the rest of the system.
///
/// A plain append-only file rather than a socket: `rascar-app` already polls a
/// GPS source, a file survives either process restarting independently, and the
/// same file doubles as a capture of the car's GPS feed. Overridable with
/// `RASCAR_CAR_GPS_PATH`; best-effort, because losing a fix must never disturb
/// the CarPlay session.
pub fn sink_car_nmea(sentence: &str) {
    use std::io::Write;
    let path = std::env::var("RASCAR_CAR_GPS_PATH")
        .unwrap_or_else(|_| "/var/lib/rascar-debug/car-gps.nmea".to_string());
    let opened = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    match opened {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{sentence}") {
                debug!("car GPS: could not write {path}: {e}");
            }
        }
        Err(e) => debug!("car GPS: could not open {path}: {e}"),
    }
}
