//! iAP2 link layer — **Device side**, transport-generic.
//!
//! Derived from usenocturne/iap2-rs (`src/link.rs`, GPL-3). See `mod.rs` for the
//! full licensing note.
//!
//! # What was lifted verbatim vs changed
//!
//! - **Lifted almost verbatim:** [`Iap2Link::send_data`], [`Iap2Link::receive_data`],
//!   the cancellation-safe reader (`fill_buf` / `try_parse_frame` / `read_packet`),
//!   sequencing / cumulative-ACK / EAK-retransmit / out-of-order buffering logic,
//!   and the `is_seq_after` helper. These are symmetric between the two roles, so
//!   they work unchanged for the Device.
//! - **Rewritten / inverted:** the transport is now a bound generic
//!   `T: AsyncRead + AsyncWrite + Unpin` owned by the link (upstream hard-coded
//!   `bluer::rfcomm::Stream` passed as a parameter — the `bluer` dependency is
//!   gone). `negotiate()` is replaced by [`Iap2Link::negotiate_device`]: upstream
//!   is the *accessory* and **sends** DETECT+SYN; we are the *Device* and instead
//!   **receive** the accessory's SYN, reply with a SYN-ACK, and receive the final
//!   ACK. The iAP1 probe and DETECT-origination were dropped.
//!
//! # ⚠ Link-direction inference (HARDWARE-GATED)
//!
//! The iAP2 *Link Layer* chapter is **not** in the reference PDF we hold (only
//! ch.12 Auth + ch.13 Identification). We infer that the **accessory (car) sends
//! the SYN** and the Device answers with SYN-ACK, from two facts:
//!   1. iap2-rs (a working *accessory*) is the SYN originator, and its SYN payload
//!      defines the session table — the SYN sender declares the sessions.
//!   2. Per ch.12/13, the Device is the one that *initiates* the control protocol
//!      (it sends `StartIdentification` / `RequestAuthenticationCertificate` first,
//!      confirmed against the PDF). The link's SYN *responder* is therefore the
//!      control *initiator* = the Device = us.
//!
//! The exact seq/ack bookkeeping across SYN→SYN-ACK→ACK, and whether the SYN-ACK
//! must echo the accepted link parameters, are best-effort and **must be verified
//! against a real head unit** in Phase 3.

use bytes::{Buf, Bytes, BytesMut};
use std::collections::{BTreeMap, VecDeque};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{sleep, timeout, Duration};
use tracing::{debug, info, warn};

use super::error::{Iap2Error, Result};
use super::packet::{Iap2Packet, PacketType, PACKET_HEADER_SIZE, PAYLOAD_CHECKSUM_SIZE};

/// The byte transport the link drives.
///
/// It is any tokio async byte stream, plus a [`recover`](Iap2Transport::recover)
/// hook a real transport can override to un-wedge itself after a transient I/O
/// error (on USB, clearing a halted bulk endpoint). The default is a no-op, so
/// the in-memory `DuplexStream` used by the unit tests needs no change.
#[async_trait::async_trait]
pub trait Iap2Transport: AsyncRead + AsyncWrite + Unpin + Send {
    /// Recover the transport after a transient I/O error, so the identical bytes
    /// can be resent. iAP2 de-dupes by sequence number, so a resend after a
    /// transport hiccup is protocol-safe. The default does nothing.
    async fn recover(&mut self) {}
}

/// The in-memory duplex stream the unit tests use has nothing to recover.
#[async_trait::async_trait]
impl Iap2Transport for tokio::io::DuplexStream {}

/// How many times a transport read/write is retried — each preceded by a
/// transport [`recover`](Iap2Transport::recover) and a short backoff — after a
/// transient I/O error before the error is finally surfaced. The car NAKs our
/// bulk OUT during a cold handshake (URB status `-71`/EPROTO), and the link
/// layer is built for retransmission, so a bounded resend rides through it.
const IO_RECOVER_ATTEMPTS: u32 = 4;

/// Backoff between a transport I/O error and the recovering retry, giving the
/// endpoint time to settle after `clear_halt`.
const IO_RECOVER_BACKOFF: Duration = Duration::from_millis(200);

/// Default control session id, used if the accessory's SYN session table cannot
/// be parsed. Matches the value iap2-rs falls back to.
pub const DEFAULT_CONTROL_SESSION_ID: u8 = 0x0A;

/// Maximum link packet assumed until the accessory's SYN says otherwise — the
/// value the canonical iAP2 link-sync payload advertises.
pub const DEFAULT_MAX_PACKET: u16 = 4096;

/// The iAP2 detect sequence. It is NOT an `FF 5A` link packet, so the packet
/// parser used to discard it as garbage. Measured on the car: it sends these six
/// bytes first, unprompted, right after the interface is claimed, and the Device
/// answers by echoing the same bytes back.
pub const IAP2_DETECT: [u8; 6] = [0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10];

/// One unit decoded from the byte stream.
enum Frame {
    Packet(Iap2Packet),
    /// A complete [`IAP2_DETECT`] sequence.
    Detect,
}

/// Upper bound on the unacknowledged DATA packets kept for retransmission. The
/// link parameters advertise 5 outstanding packets; this keeps slack while
/// making it impossible for a peer that never ACKs to grow the map without end.
const MAX_TX_HISTORY: usize = 16;

/// Link-layer timing / sizing configuration (subset of iap2-rs `LinkConfig`, with
/// the accessory-only iAP1-probe/detect knobs removed).
#[derive(Debug, Clone)]
pub struct LinkConfig {
    pub max_retries: u32,
    pub timeout_ms: u64,
    pub initial_seq: u8,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            max_retries: 30,
            timeout_ms: 1500,
            initial_seq: 0x00,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LinkState {
    Idle,
    Established,
}

pub struct Iap2Link<T> {
    transport: T,
    seq: u8,
    ack: u8,
    state: LinkState,
    config: LinkConfig,
    control_session_id: u8,
    /// File Transfer session id (session type `0x01`) from the accessory's SYN
    /// session table, if it declared one. The Jaguar declares `(2, 1, v1)`.
    file_transfer_session_id: Option<u8>,
    /// Maximum link packet the accessory advertised in its SYN.
    max_packet: u16,
    pending: VecDeque<Iap2Packet>,
    rx_buffer: BTreeMap<u8, Iap2Packet>,
    /// DATA packets we sent and have not seen acknowledged, keyed by their seq.
    /// An EAK names a sequence number the accessory never received; without the
    /// original packet there is nothing to retransmit.
    tx_history: BTreeMap<u8, Iap2Packet>,
    read_buf: BytesMut,
    /// A DETECT was consumed from `read_buf` but its echo has not been fully
    /// written yet. Kept as state (not a local) so a read cancelled by its
    /// timeout mid-echo still sends the echo on the next read.
    detect_echo_pending: bool,
    /// Retransmission timeout actually negotiated with the accessory, in ms.
    ///
    /// The accessory names this in its SYN payload and `counter_propose` leaves
    /// the field untouched, so we AGREE to it -- then we must honour it. We used
    /// a fixed 1500 ms while the Jaguar F-PACE asks for 500 ms, so whenever our
    /// DATA packet was dropped the car's own retransmit timer fired first and we
    /// spent 0.7-1.9 s of its session budget waiting. Measured over 25 in-car
    /// runs, every second of that stall costs ~0.52 s of the post-auth window
    /// (r = -0.80), and the run with the smallest stall streamed the most frames.
    retransmit_timeout_ms: u64,
}

impl<T> Iap2Link<T>
where
    T: Iap2Transport,
{
    pub fn new(transport: T, config: LinkConfig) -> Self {
        Iap2Link {
            retransmit_timeout_ms: config.timeout_ms,
            transport,
            seq: 0x00,
            ack: 0x00,
            state: LinkState::Idle,
            config,
            control_session_id: DEFAULT_CONTROL_SESSION_ID,
            file_transfer_session_id: None,
            max_packet: DEFAULT_MAX_PACKET,
            pending: VecDeque::new(),
            rx_buffer: BTreeMap::new(),
            tx_history: BTreeMap::new(),
            read_buf: BytesMut::with_capacity(8192),
            detect_echo_pending: false,
        }
    }

    pub fn is_established(&self) -> bool {
        self.state == LinkState::Established
    }

    /// Change the link state, logging every transition.
    fn set_state(&mut self, new: LinkState, reason: &str) {
        if self.state != new {
            info!(
                "iAP2 LINK STATE: {:?} -> {:?} ({reason}; seq=0x{:02X} ack=0x{:02X}; t+{} ms)",
                self.state,
                new,
                self.seq,
                self.ack,
                crate::debug::elapsed_ms()
            );
        }
        self.state = new;
    }

    /// The control session id negotiated during link establishment (parsed from
    /// the accessory's SYN session table, or [`DEFAULT_CONTROL_SESSION_ID`]).
    pub fn control_session_id(&self) -> u8 {
        self.control_session_id
    }

    /// The File Transfer session id the accessory declared in its SYN (session
    /// type `0x01`), or `None` when it declared none — then nothing may be sent
    /// on a file transfer session at all.
    pub fn file_transfer_session_id(&self) -> Option<u8> {
        self.file_transfer_session_id
    }

    /// True when a DATA packet that arrived while we were waiting for an ACK is
    /// queued and the next [`receive_data`](Self::receive_data) returns it
    /// without touching the transport.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Consume the link and get the transport back (Phase 3 hands the byte stream
    /// off to the NCM / AirPlay layers once the session is established).
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Device-side link negotiation.
    ///
    /// Waits for the accessory's SYN (a leading iAP2 DETECT is echoed back by the
    /// packet reader as soon as it arrives, see [`IAP2_DETECT`]), replies at once
    /// with a SYN-ACK that echoes the accepted link parameters, then waits
    /// for the accessory's completing ACK. See the module-level ⚠ note: the SYN
    /// direction is inferred and hardware-gated.
    pub async fn negotiate_device(&mut self) -> Result<()> {
        info!("Starting iAP2 link negotiation (Device side)");

        // 1. Wait for the accessory's SYN (ignore any DETECT that precedes it).
        //
        // The head unit opens its iAP2 link on its own schedule and can take
        // several seconds after the interface is claimed, so a single
        // `timeout_ms` window is far too short to declare failure on: spend the
        // whole `max_retries` budget here, exactly as the completing-ACK wait
        // below does.
        let syn_budget_ms = self.config.timeout_ms * u64::from(self.config.max_retries + 1);
        info!(
            "Waiting for the accessory's SYN (up to {} attempts x {} ms = {:.1} s)",
            self.config.max_retries + 1,
            self.config.timeout_ms,
            syn_budget_ms as f64 / 1000.0
        );
        let mut syn_attempt: u32 = 0;
        let syn = loop {
            let packet = match self.read_packet_with_timeout().await {
                Ok(packet) => packet,
                Err(Iap2Error::Timeout) => {
                    syn_attempt += 1;
                    if syn_attempt > self.config.max_retries {
                        warn!(
                            "No SYN from the accessory after {:.1} s — giving up on link negotiation",
                            syn_budget_ms as f64 / 1000.0
                        );
                        return Err(Iap2Error::Timeout);
                    }
                    debug!(
                        "Still waiting for the accessory's SYN (attempt {}/{})",
                        syn_attempt, self.config.max_retries
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            match packet.control.packet_type {
                PacketType::Syn => break packet,
                PacketType::Detect => {
                    // Only an `FF 5A`-framed 0xEE packet lands here; the real
                    // `FF 55` detect sequence is answered inside `read_packet`.
                    debug!("Received FF 5A-framed DETECT packet from accessory, awaiting SYN");
                    continue;
                }
                PacketType::Rst => {
                    warn!("Accessory sent RST during negotiation — it reset the link");
                    return Err(Iap2Error::ConnectionClosed);
                }
                other => {
                    warn!("Expected SYN/DETECT during negotiation, got {:?}", other);
                    continue;
                }
            }
        };

        // Learn the control session id and the accepted maximum packet size from
        // the SYN link-sync payload.
        self.control_session_id =
            Self::parse_control_session_id(&syn.payload).unwrap_or(DEFAULT_CONTROL_SESSION_ID);
        self.file_transfer_session_id = Self::parse_session_id_of_type(&syn.payload, 0x01);
        self.max_packet = match Self::parse_max_packet(&syn.payload) {
            Some(max) => max,
            None => {
                warn!(
                    "SYN payload ({} bytes) does not carry a usable maximum packet size — assuming {}",
                    syn.payload.len(),
                    DEFAULT_MAX_PACKET
                );
                DEFAULT_MAX_PACKET
            }
        };
        self.ack = syn.seq;
        self.retransmit_timeout_ms = negotiated_retransmit_ms(&syn.payload, self.config.timeout_ms);
        info!(
            "Received SYN (seq=0x{:02X}); control session id = 0x{:02X}; max packet = {} bytes; \
             retransmit timeout = {} ms",
            syn.seq, self.control_session_id, self.max_packet, self.retransmit_timeout_ms
        );

        // 2. Reply with SYN-ACK. We do NOT echo the accessory's payload verbatim:
        //    the two capability fields describe what WE can receive, so echoing
        //    them understates us and caps what the accessory may send.
        self.seq = self.config.initial_seq;
        let syn_ack = Iap2Packet::syn_ack(self.seq, self.ack, counter_propose(&syn.payload));
        self.write_packet(&syn_ack).await?;
        info!(
            "Sent SYN-ACK (seq=0x{:02X}, ack=0x{:02X}) at t+{} ms",
            self.seq,
            self.ack,
            crate::debug::elapsed_ms()
        );

        // 3. Wait for the accessory's completing ACK (retry-tolerant: some
        //    accessories re-send SYN if our SYN-ACK is missed).
        let mut attempt = 0;
        loop {
            match self.wait_for_ack().await {
                Ok(()) => break,
                Err(e) => {
                    attempt += 1;
                    if attempt > self.config.max_retries {
                        return Err(e);
                    }
                    warn!(
                        "No completing ACK, resending SYN-ACK (attempt {}/{})",
                        attempt, self.config.max_retries
                    );
                    self.write_packet(&syn_ack).await?;
                }
            }
        }

        // Bump our seq so the first DATA packet does not reuse the SYN-ACK seq
        // (mirrors iap2-rs's accessory bookkeeping).
        self.seq = self.next_seq();
        self.set_state(LinkState::Established, "SYN / SYN-ACK / completing ACK handshake done");
        info!("iAP2 link established (Device side)");
        Ok(())
    }

    async fn wait_for_ack(&mut self) -> Result<()> {
        let packet = self.read_packet_with_timeout().await?;
        if packet.control.packet_type != PacketType::Ack {
            return Err(Iap2Error::Protocol(format!(
                "Expected completing ACK, got {:?}",
                packet.control.packet_type
            )));
        }
        if packet.ack != self.seq {
            return Err(Iap2Error::Protocol(format!(
                "Completing ACK has wrong ack: expected 0x{:02X}, got 0x{:02X}",
                self.seq, packet.ack
            )));
        }
        self.ack = packet.seq;
        debug!(
            "Received completing ACK (seq=0x{:02X}, ack=0x{:02X})",
            packet.seq, packet.ack
        );
        Ok(())
    }

    /// Parse the control session id (session type `0x00`) from a SYN link-sync
    /// payload. Layout: a 10-byte link header followed by 3-byte session triples
    /// `(session_id, session_type, version)`. Returns `None` if the payload is
    /// too short or no control-type session is present.
    fn parse_control_session_id(payload: &[u8]) -> Option<u8> {
        Self::parse_session_id_of_type(payload, 0x00)
    }

    /// First session id of `wanted_type` in a SYN link-sync payload (`0x00`
    /// control, `0x01` file transfer, `0x02` external accessory).
    fn parse_session_id_of_type(payload: &[u8], wanted_type: u8) -> Option<u8> {
        const LINK_HEADER_LEN: usize = 10;
        if payload.len() <= LINK_HEADER_LEN {
            return None;
        }
        let sessions = &payload[LINK_HEADER_LEN..];
        for triple in sessions.chunks_exact(3) {
            let (id, session_type) = (triple[0], triple[1]);
            if session_type == wanted_type {
                return Some(id);
            }
        }
        None
    }

    /// Parse the maximum link packet size from a SYN link-sync payload
    /// (big-endian `u16` at offset 2, right after version and max-outstanding).
    /// Returns `None` when the payload is too short or advertises a size that
    /// cannot even hold a header — the caller then falls back to the default.
    fn parse_max_packet(payload: &[u8]) -> Option<u16> {
        if payload.len() < 4 {
            return None;
        }
        let max = u16::from_be_bytes([payload[2], payload[3]]);
        let minimum = (PACKET_HEADER_SIZE + PAYLOAD_CHECKSUM_SIZE + 1) as u16;
        (max >= minimum).then_some(max)
    }

    /// Largest control payload that fits in one link packet at the negotiated
    /// maximum packet size.
    pub fn max_payload(&self) -> usize {
        self.max_packet as usize - PACKET_HEADER_SIZE - PAYLOAD_CHECKSUM_SIZE
    }

    /// Remember a DATA packet so an EAK can be answered with the real message.
    fn remember_sent(&mut self, packet: &Iap2Packet) {
        self.tx_history.insert(packet.seq, packet.clone());
        if self.tx_history.len() > MAX_TX_HISTORY {
            // Evict whichever entry sits furthest behind the packet we just sent:
            // it has fallen out of the link's retransmission window anyway.
            if let Some(&oldest) = self
                .tx_history
                .keys()
                .max_by_key(|&&s| packet.seq.wrapping_sub(s))
            {
                warn!(
                    "TX history full ({} packets): dropping unacked packet 0x{:02X}, it can no longer be retransmitted",
                    MAX_TX_HISTORY, oldest
                );
                self.tx_history.remove(&oldest);
            }
        }
    }

    /// Drop every remembered packet the accessory's cumulative ACK covers.
    fn prune_tx_history(&mut self, acked_seq: u8) {
        let delivered: Vec<u8> = self
            .tx_history
            .keys()
            .copied()
            .filter(|&s| s == acked_seq || self.is_seq_after(acked_seq, s))
            .collect();
        for seq in delivered {
            self.tx_history.remove(&seq);
        }
    }

    /// Receive one application/control DATA packet, ACKing it (and handling
    /// out-of-order buffering). Lifted verbatim from iap2-rs (generic transport).
    pub async fn receive_data(&mut self) -> Result<Iap2Packet> {
        if let Some(pkt) = self.pending.pop_front() {
            debug!(
                "Delivering pending control message with seq=0x{:02X}",
                pkt.seq
            );
            return Ok(pkt);
        }

        // `read_packet` has no deadline of its own, so without this budget the
        // public entry points (`identify`, `authenticate`, the post-bring-up
        // service loop) can block forever on a silent accessory and only ever
        // unwind through a caller-supplied timeout.
        let idle_budget_ms = self.config.timeout_ms * u64::from(self.config.max_retries + 1);
        let mut idle_attempt: u32 = 0;

        loop {
            if !self.rx_buffer.is_empty() {
                let expected_seq = self.ack.wrapping_add(1);
                if let Some(buffered) = self.rx_buffer.remove(&expected_seq) {
                    debug!(
                        "Delivering buffered packet with seq=0x{:02X} (was out-of-order)",
                        expected_seq
                    );
                    self.ack = buffered.seq;
                    let ack = if let Some(s) = buffered.session_id {
                        Iap2Packet::ack_with_session(self.ack_only_seq(), self.ack, s)
                    } else {
                        Iap2Packet::ack(self.ack_only_seq(), self.ack)
                    };
                    self.write_packet(&ack).await?;
                    return Ok(buffered);
                }
            }

            let packet = match self.read_packet_with_timeout().await {
                Ok(packet) => {
                    idle_attempt = 0;
                    packet
                }
                Err(Iap2Error::Timeout) => {
                    idle_attempt += 1;
                    if idle_attempt > self.config.max_retries {
                        warn!(
                            "receive_data: accessory silent for {:.1} s, giving up",
                            idle_budget_ms as f64 / 1000.0
                        );
                        return Err(Iap2Error::Timeout);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            let has_payload = !packet.payload.is_empty();

            match packet.control.packet_type {
                PacketType::Data if has_payload => {
                    let expected_seq = self.ack.wrapping_add(1);

                    if packet.seq == expected_seq {
                        self.ack = packet.seq;
                        let ack = Iap2Packet::ack(self.ack_only_seq(), self.ack);
                        self.write_packet(&ack).await?;
                        return Ok(packet);
                    } else if self.is_seq_after(packet.seq, expected_seq) {
                        warn!(
                            "Out-of-order packet: expected seq=0x{:02X}, got seq=0x{:02X}, buffering",
                            expected_seq, packet.seq
                        );
                        self.rx_buffer.insert(packet.seq, packet.clone());

                        let ack = Iap2Packet::ack(self.ack_only_seq(), self.ack);
                        self.write_packet(&ack).await?;
                        continue;
                    } else {
                        warn!(
                            "Duplicate/old packet: expected seq=0x{:02X}, got seq=0x{:02X}, dropping",
                            expected_seq, packet.seq
                        );
                        let ack = Iap2Packet::ack(self.ack_only_seq(), self.ack);
                        self.write_packet(&ack).await?;
                        continue;
                    }
                }
                PacketType::Ack if has_payload => {
                    let expected_seq = self.ack.wrapping_add(1);

                    if packet.seq == expected_seq || packet.seq == self.ack {
                        self.ack = packet.seq;
                        let ack = if let Some(s) = packet.session_id {
                            Iap2Packet::ack_with_session(self.ack_only_seq(), self.ack, s)
                        } else {
                            Iap2Packet::ack(self.ack_only_seq(), self.ack)
                        };
                        self.write_packet(&ack).await?;
                        return Ok(packet);
                    } else if self.is_seq_after(packet.seq, expected_seq) {
                        warn!(
                            "Out-of-order ACK packet: expected seq=0x{:02X}, got seq=0x{:02X}, buffering",
                            expected_seq, packet.seq
                        );
                        self.rx_buffer.insert(packet.seq, packet.clone());
                        continue;
                    } else {
                        debug!(
                            "Duplicate/old ACK packet with seq=0x{:02X}, dropping",
                            packet.seq
                        );
                        continue;
                    }
                }
                PacketType::Ack => {
                    debug!("Received ACK packet without payload");
                    continue;
                }
                PacketType::Rst => {
                    // The accessory tore the link down. Reporting this as a
                    // generic timeout several seconds later hides the cause, so
                    // surface it immediately and mark the link unusable.
                    warn!("Accessory sent RST — the link was reset by the peer");
                    self.set_state(LinkState::Idle, "accessory sent RST while receiving");
                    return Err(Iap2Error::ConnectionClosed);
                }
                PacketType::Suspend | PacketType::SuspendAck => {
                    // Link suspend/resume is not implemented; treating a SUSPEND
                    // as noise would leave us reading from a link the accessory
                    // has stopped servicing.
                    warn!(
                        "Accessory sent {:?} — link suspend is not implemented",
                        packet.control.packet_type
                    );
                    self.set_state(LinkState::Idle, "accessory sent SUSPEND");
                    return Err(Iap2Error::Protocol(format!(
                        "Accessory suspended the link ({:?}); suspend/resume is not implemented",
                        packet.control.packet_type
                    )));
                }
                _ => {
                    warn!("Unexpected packet type: {:?}", packet.control.packet_type);
                    continue;
                }
            }
        }
    }

    /// Send an application/control DATA packet and wait for its ACK (with EAK
    /// retransmit handling). Lifted verbatim from iap2-rs (generic transport).
    pub async fn send_data(&mut self, session_id: u8, payload: Bytes) -> Result<()> {
        if self.state != LinkState::Established {
            return Err(Iap2Error::Protocol("Link not established".to_string()));
        }

        // The on-the-wire length field is 16 bits and the accessory advertised a
        // maximum link packet in its SYN. Reject an oversized payload rather than
        // emitting a packet whose declared length contradicts its contents; there
        // is no fragmentation on this path.
        if payload.len() > self.max_payload() {
            return Err(Iap2Error::Protocol(format!(
                "Payload of {} bytes exceeds the negotiated maximum link packet ({} bytes, {} usable) — refusing to send",
                payload.len(),
                self.max_packet,
                self.max_payload()
            )));
        }

        // The seq this DATA packet carries. It must NOT move while we wait: the
        // loop below exits on `ack.ack == inflight_seq`, and mutating `self.seq`
        // (as the EAK path used to) makes that comparison unmatchable forever.
        let inflight_seq = self.seq;

        debug!(
            "Preparing to send data with seq=0x{:02X}, ack=0x{:02X}",
            inflight_seq, self.ack
        );
        let packet = Iap2Packet::data(inflight_seq, self.ack, session_id, payload);

        self.write_packet(&packet).await?;
        self.remember_sent(&packet);

        let mut attempt: u32 = 0;
        loop {
            let ack = match self.read_packet_within(self.retransmit_timeout_ms).await {
                Ok(p) => p,
                Err(Iap2Error::Timeout) => {
                    // A dropped DATA packet is the common case here; the SYN wait
                    // and the completing-ACK wait both retry, and so must this.
                    attempt += 1;
                    if attempt > self.config.max_retries {
                        warn!(
                            "No ACK for seq=0x{:02X} after {} attempts — giving up",
                            inflight_seq, self.config.max_retries
                        );
                        return Err(Iap2Error::Timeout);
                    }
                    warn!(
                        "No ACK for seq=0x{:02X} within {} ms, retransmitting (attempt {}/{})",
                        inflight_seq, self.retransmit_timeout_ms, attempt, self.config.max_retries
                    );
                    self.write_packet(&packet).await?;
                    continue;
                }
                Err(e) => return Err(e),
            };

            match ack.control.packet_type {
                PacketType::Ack => {
                    let has_payload = !ack.payload.is_empty();
                    if has_payload {
                        if !self.pending.iter().any(|p| p.seq == ack.seq) {
                            self.pending.push_back(ack.clone());
                            debug!(
                                "Queued pending control message from ACK packet (seq=0x{:02X}, queue size: {})",
                                ack.seq,
                                self.pending.len()
                            );
                        } else {
                            debug!(
                                "Ignoring duplicate control message in ACK (seq=0x{:02X} already queued)",
                                ack.seq
                            );
                        }
                    }
                    self.ack = ack.seq;
                    self.prune_tx_history(ack.ack);

                    if ack.ack == inflight_seq {
                        debug!("Received ACK for our message (seq=0x{:02X})", inflight_seq);

                        self.seq = self.next_seq();
                        debug!("Incremented sequence to 0x{:02X} for next packet", self.seq);

                        break;
                    } else {
                        if has_payload {
                            let response_ack = Iap2Packet::ack(self.ack_only_seq(), self.ack);
                            self.write_packet(&response_ack).await?;
                            debug!(
                                "Sent ACK for accessory data while waiting: seq=0x{:02X}, ack=0x{:02X}",
                                self.seq, self.ack
                            );
                        }
                        debug!(
                            "Still waiting for ACK: expected 0x{:02X}, got 0x{:02X}",
                            inflight_seq, ack.ack
                        );
                    }
                }
                PacketType::Eak => {
                    debug!("Received EAK with payload: {}", hex::encode(&ack.payload));

                    if !ack.payload.is_empty() {
                        let highest_received = ack.payload[0];
                        let in_order_ack = ack.ack;

                        // The accessory has everything up to `in_order_ack` plus
                        // `highest_received`; the gap starts at in_order_ack + 1.
                        // All comparisons must wrap — `self.seq - 1` panics in
                        // debug once seq rolls over 0x00.
                        let previous_seq = inflight_seq.wrapping_sub(1);
                        if highest_received == inflight_seq
                            && self.is_seq_after(previous_seq, in_order_ack)
                        {
                            let missing_seq = in_order_ack.wrapping_add(1);
                            match self.tx_history.get(&missing_seq).cloned() {
                                Some(missing) => {
                                    warn!(
                                        "Accessory is missing packet 0x{:02X}, retransmitting the original DATA packet",
                                        missing_seq
                                    );
                                    self.write_packet(&missing).await?;
                                    info!(
                                        "Retransmitted packet 0x{:02X} ({} payload bytes)",
                                        missing_seq,
                                        missing.payload.len()
                                    );
                                }
                                None => {
                                    // A bare ACK carries no payload, so sending
                                    // one here (as the old code did) could never
                                    // deliver the message the accessory is
                                    // waiting for — it would just stall until the
                                    // authentication timeout with a log line
                                    // claiming success.
                                    return Err(Iap2Error::Protocol(format!(
                                        "Accessory EAK asks for packet 0x{:02X}, which is not in our TX history ({} packet(s) held) — cannot retransmit",
                                        missing_seq,
                                        self.tx_history.len()
                                    )));
                                }
                            }
                        }
                    }

                    self.ack = ack.seq;

                    // NOTE: our own send sequence must NOT advance here. The DATA
                    // packet above is still unacknowledged and is identified by
                    // `inflight_seq`.
                    let eak_ack = Iap2Packet::ack(self.ack_only_seq(), self.ack);
                    self.write_packet(&eak_ack).await?;
                    debug!(
                        "Sent ACK for EAK: seq=0x{:02X}, ack=0x{:02X}",
                        self.seq, self.ack
                    );
                }
                PacketType::Rst => {
                    warn!("Accessory sent RST while awaiting our ACK — the link was reset");
                    self.set_state(LinkState::Idle, "accessory sent RST while awaiting our ACK");
                    return Err(Iap2Error::ConnectionClosed);
                }
                _ => {
                    return Err(Iap2Error::Protocol(format!(
                        "Expected ACK or EAK, got {:?}",
                        ack.control.packet_type
                    )));
                }
            }
        }

        self.tx_history.remove(&inflight_seq);
        Ok(())
    }

    async fn write_packet(&mut self, packet: &Iap2Packet) -> Result<()> {
        let bytes = packet.to_bytes();
        debug!("Sending raw packet: {}", hex::encode(&bytes));
        if crate::debug::wire_trace() {
            debug!("iap2 link -> {}", super::packet::describe_raw(&bytes));
        }
        debug!("Sent packet: {:?}", packet);
        self.write_all_resilient(&bytes).await
    }

    /// Write the whole buffer to the transport and flush it, riding through a
    /// transient transport error (the car NAKing our bulk OUT during a cold
    /// bring-up, URB status `-71`) by recovering the transport and resending the
    /// identical bytes, up to [`IO_RECOVER_ATTEMPTS`] times. Resending is safe:
    /// the accessory de-dupes by sequence number.
    async fn write_all_resilient(&mut self, bytes: &[u8]) -> Result<()> {
        let mut attempt: u32 = 0;
        loop {
            let outcome: std::io::Result<()> = async {
                self.transport.write_all(bytes).await?;
                self.transport.flush().await?;
                Ok(())
            }
            .await;

            match outcome {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt > IO_RECOVER_ATTEMPTS {
                        warn!(
                            "iAP2 OUT write failed after {} recovery attempt(s): {} — giving up (t+{} ms)",
                            IO_RECOVER_ATTEMPTS,
                            e,
                            crate::debug::elapsed_ms()
                        );
                        return Err(Iap2Error::Io(e));
                    }
                    warn!(
                        "iAP2 OUT write error (attempt {}/{}): {} — recovering the transport and resending (iAP2 seq de-dup makes this safe)",
                        attempt, IO_RECOVER_ATTEMPTS, e
                    );
                    self.transport.recover().await;
                    sleep(IO_RECOVER_BACKOFF).await;
                }
            }
        }
    }

    async fn read_packet_with_timeout(&mut self) -> Result<Iap2Packet> {
        self.read_packet_within(self.config.timeout_ms).await
    }

    async fn read_packet_within(&mut self, ms: u64) -> Result<Iap2Packet> {
        match timeout(Duration::from_millis(ms), self.read_packet()).await {
            Ok(result) => result,
            Err(_) => Err(Iap2Error::Timeout),
        }
    }

    /// Read available bytes from the transport into our persistent buffer.
    ///
    /// A transient transport read error (a bulk IN that faults with `-71` mid
    /// bring-up) is ridden through the same way the OUT path is: recover the
    /// transport and retry, up to [`IO_RECOVER_ATTEMPTS`] times. A clean EOF
    /// (`n == 0`) is a real close and is never retried.
    async fn fill_buf(&mut self) -> Result<()> {
        let mut tmp = [0u8; 4096];
        let mut attempt: u32 = 0;
        let n = loop {
            match self.transport.read(&mut tmp).await {
                Ok(n) => break n,
                Err(e) => {
                    attempt += 1;
                    if attempt > IO_RECOVER_ATTEMPTS {
                        warn!(
                            "iAP2 IN read failed after {} recovery attempt(s): {} — giving up",
                            IO_RECOVER_ATTEMPTS, e
                        );
                        return Err(Iap2Error::Io(e));
                    }
                    warn!(
                        "iAP2 IN read error (attempt {}/{}): {} — recovering the transport and retrying",
                        attempt, IO_RECOVER_ATTEMPTS, e
                    );
                    self.transport.recover().await;
                    sleep(IO_RECOVER_BACKOFF).await;
                }
            }
        };
        if n == 0 {
            return Err(Iap2Error::ConnectionClosed);
        }
        self.read_buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    /// Try to parse a complete iAP2 packet or DETECT sequence from the read
    /// buffer. Returns `None` if not enough data is available yet. Resyncs on
    /// garbage.
    fn try_parse_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            if self.read_buf.len() < 2 {
                return Ok(None);
            }

            if self.read_buf[0] == 0xFF && self.read_buf[1] == 0x55 {
                let n = self.read_buf.len().min(IAP2_DETECT.len());
                if self.read_buf[..n] == IAP2_DETECT[..n] {
                    if n < IAP2_DETECT.len() {
                        return Ok(None); // detect split across transfers
                    }
                    self.read_buf.advance(IAP2_DETECT.len());
                    return Ok(Some(Frame::Detect));
                }
                debug!("iap2 link: FF 55 not followed by the detect sequence, skipping it");
                self.read_buf.advance(1);
                continue;
            }

            if self.read_buf[0] != 0xFF || self.read_buf[1] != 0x5A {
                // Skip the whole run up to the next candidate start (FF 5A packet
                // or FF 55 detect) at once so it can be logged as one event; keep
                // only the last byte if none is found (it may start one). Index 0
                // is not a candidate here, so the run is never empty.
                let run = self
                    .read_buf
                    .windows(2)
                    .position(|w| w[0] == 0xFF && (w[1] == 0x5A || w[1] == 0x55))
                    .unwrap_or(self.read_buf.len() - 1);
                debug!("iap2 link: discarding {run} byte(s) before the next FF 5A sync");
                if crate::debug::wire_trace() {
                    debug!(
                        "iap2 link: discarded bytes:{}",
                        crate::debug::hex_dump(&self.read_buf[..run], 64)
                    );
                }
                self.read_buf.advance(run);
                continue;
            }

            if self.read_buf.len() < 4 {
                return Ok(None);
            }

            let packet_len = u16::from_be_bytes([self.read_buf[2], self.read_buf[3]]) as usize;
            if !(6..=8192).contains(&packet_len) {
                warn!(
                    "Invalid packet length {} at sync position, resyncing",
                    packet_len
                );
                self.read_buf.advance(2);
                continue;
            }

            if self.read_buf.len() < packet_len {
                return Ok(None);
            }

            let packet_data = self.read_buf.split_to(packet_len);
            debug!("Received raw packet: {}", hex::encode(&packet_data));
            if crate::debug::wire_trace() {
                debug!("iap2 link <- {}", super::packet::describe_raw(&packet_data));
            }

            match Iap2Packet::from_bytes(&packet_data) {
                Ok(packet) => {
                    debug!("Parsed packet: {:?}", packet);
                    return Ok(Some(Frame::Packet(packet)));
                }
                Err(e) => {
                    warn!("Invalid packet ({}), skipping and resyncing", e);
                    continue;
                }
            }
        }
    }

    /// Read the next link packet. A DETECT sequence met on the way is echoed back
    /// immediately and never reaches the caller, so every state (SYN wait, ACK
    /// wait, established session) answers it the same way.
    async fn read_packet(&mut self) -> Result<Iap2Packet> {
        loop {
            if self.detect_echo_pending {
                self.write_all_resilient(&IAP2_DETECT).await?;
                self.detect_echo_pending = false;
                info!(
                    "iAP2 DETECT echoed back to the accessory ({}) at t+{} ms",
                    hex::encode(IAP2_DETECT),
                    crate::debug::elapsed_ms()
                );
            }
            match self.try_parse_frame()? {
                Some(Frame::Packet(packet)) => return Ok(packet),
                Some(Frame::Detect) => {
                    info!(
                        "iAP2 DETECT received from the accessory at t+{} ms — echoing it",
                        crate::debug::elapsed_ms()
                    );
                    self.detect_echo_pending = true;
                }
                None => self.fill_buf().await?,
            }
        }
    }

    /// Next send sequence number. Sequence numbers are 8-bit and wrap: plain
    /// `+ 1` panics in debug builds after 255 packets and silently misbehaves in
    /// release.

    /// Sequence number to put on a ZERO-PAYLOAD (ACK-only) packet.
    ///
    /// An ACK-only packet consumes no sequence number, so it must carry the LAST
    /// CONSUMED one — never `self.seq`, which is the next number a data packet
    /// will use. Putting `self.seq` on an ACK and then sending data with the same
    /// `self.seq` puts a DUPLICATE sequence number on the wire; measured in our own
    /// capture (boot-20260921-081439 run 127):
    ///
    ///     2.8072 OUT   9 B  ctrl 0x40  seq 05  ack fc   <- ACK-only
    ///     2.8075 OUT  16 B  ctrl 0x40  seq 05  ack fc   <- DATA, SAME seq
    ///
    /// It was inconsistent rather than always wrong, because `self.seq` only
    /// advances once the peer ACKs, so an ACK-only sent while a data packet was
    /// still in flight happened to carry the right value.
    ///
    /// This matters on QNX/Harman head units specifically. The catplay author
    /// disassembled that stack and confirmed it mis-handles sequence numbers on
    /// ACK-only packets, and — critically — that its
    /// "Packet seq received not expected range" is a HARD STOP, not a warning:
    /// https://github.com/catplay-labs/catplay/issues/12 (fix commit 52ec19f5).
    /// A hard stop there means the head unit's CarPlay manager wedges, its
    /// supervisor watchdog bites, and the port is reset — which is exactly the
    /// silent, fixed-delay USB teardown we see.
    fn ack_only_seq(&self) -> u8 {
        self.seq.wrapping_sub(1)
    }

    fn next_seq(&self) -> u8 {
        self.seq.wrapping_add(1)
    }

    fn is_seq_after(&self, seq_a: u8, seq_b: u8) -> bool {
        let diff = seq_a.wrapping_sub(seq_b);
        diff > 0 && diff < 128
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_packet(frame: Option<Frame>) -> Iap2Packet {
        match frame {
            Some(Frame::Packet(p)) => p,
            Some(Frame::Detect) => panic!("expected a packet, got DETECT"),
            None => panic!("expected a packet, got nothing"),
        }
    }

    /// Resync skips garbage in one step; the `FF 55` detect sequence is NOT
    /// garbage (it used to be discarded here) and is surfaced as a DETECT, and
    /// a stray `FF 55` that is not a detect is skipped. Pure garbage keeps only
    /// its last byte.
    #[test]
    fn try_parse_frame_resyncs_over_garbage_and_keeps_detect() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());
        link.read_buf
            .extend_from_slice(&[0x00, 0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10, 0xFF, 0x55, 0x07]);
        link.read_buf
            .extend_from_slice(&Iap2Packet::ack(0x01, 0x02).to_bytes());
        assert!(matches!(link.try_parse_frame().unwrap(), Some(Frame::Detect)));
        let pkt = expect_packet(link.try_parse_frame().unwrap());
        assert_eq!(pkt.control.packet_type, PacketType::Ack);
        assert_eq!((pkt.seq, pkt.ack), (0x01, 0x02));

        link.read_buf.extend_from_slice(&[0x01, 0x02, 0xFF]);
        assert!(link.try_parse_frame().unwrap().is_none());
        assert_eq!(&link.read_buf[..], &[0xFF]);
    }

    /// A detect split across two bulk transfers waits for the rest.
    #[test]
    fn detect_split_across_reads_is_reassembled() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());
        link.read_buf.extend_from_slice(&IAP2_DETECT[..3]);
        assert!(link.try_parse_frame().unwrap().is_none());
        assert_eq!(&link.read_buf[..], &IAP2_DETECT[..3]);
        link.read_buf.extend_from_slice(&IAP2_DETECT[3..]);
        assert!(matches!(link.try_parse_frame().unwrap(), Some(Frame::Detect)));
        assert!(link.read_buf.is_empty());
    }

    // ── Real car bytes (usbmon capture, 2026-09-15) ──────────────────────────
    /// The car's detect, sent first and unprompted after the interface claim.
    const CAR_DETECT: [u8; 6] = [0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10];
    /// The car's link SYN: seq 0x18, link version 1, max outstanding 5, max
    /// packet 0x1000, retransmit 0x01f4, cumulative-ack 0x00b0, max retransmit
    /// 0x1e, max cumulative acks 0x0a, sessions (1, control, v2) (2, type 1, v1).
    const CAR_SYN: &str = "ff5a001a80180000f50105100001f400b01e0a01000202010116";
    /// Our expected SYN|ACK, computed independently of this crate (Python): seq 0,
    /// ack 0x18, header checksum 0xb5, the car's parameters echoed, payload
    /// checksum 0x16.
    /// The SYN|ACK we must put on the wire in answer to the real Jaguar SYN
    /// captured in `CAR_SYN`.
    ///
    /// Updated 2026-09-21: the payload is no longer the car's own bytes echoed
    /// back. We now counter-propose our receive capability — `01 7f ff ff` in
    /// place of `01 05 10 00` — exactly as a working dongle does on a Pioneer
    /// head unit (see `counter_propose` and `syn_ack_tests`). The car's timing
    /// fields (`01f4 00b0 1e 0a`) and session descriptors are still echoed
    /// untouched, and the payload checksum changes from 0x16 to 0xae as a
    /// consequence.
    const EXPECTED_SYN_ACK: &str =
        "ff5a001ac0001800b5017fffff01f400b01e0a010002020101ae";
    /// Well under the car's 1 s SYN retransmit interval.
    const PROMPT: Duration = Duration::from_millis(250);

    fn checksum_ok(bytes: &[u8]) -> bool {
        bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b)) == 0
    }

    async fn read_n(car: &mut tokio::io::DuplexStream, n: usize, what: &str) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        timeout(PROMPT, car.read_exact(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("{what} not written within {PROMPT:?}"))
            .unwrap_or_else(|e| panic!("reading {what}: {e}"));
        buf
    }

    fn assert_syn_ack(bytes: &[u8]) -> Iap2Packet {
        assert_eq!(hex::encode(bytes), EXPECTED_SYN_ACK);
        assert!(checksum_ok(&bytes[..PACKET_HEADER_SIZE]), "header checksum");
        assert!(checksum_ok(&bytes[PACKET_HEADER_SIZE..]), "payload checksum");
        let parsed = Iap2Packet::from_bytes(bytes).expect("SYN|ACK must parse (checksums verified)");
        assert_eq!(parsed.control.packet_type, PacketType::SynAck);
        assert_eq!(parsed.ack, 0x18);
        parsed
    }

    /// Detect first, then the SYN, exactly as the car sends them: the detect is
    /// echoed before the SYN exists, the SYN|ACK follows the SYN promptly, and
    /// the link establishes on the car's completing ACK.
    #[tokio::test]
    async fn negotiate_answers_real_car_detect_then_syn_promptly() {
        let (link_io, mut car) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut link = Iap2Link::new(link_io, LinkConfig::default());
            let result = link.negotiate_device().await;
            (link, result)
        });

        car.write_all(&CAR_DETECT).await.unwrap();
        assert_eq!(read_n(&mut car, 6, "detect echo").await, CAR_DETECT);

        let syn = hex::decode(CAR_SYN).unwrap();
        let sent = std::time::Instant::now();
        car.write_all(&syn).await.unwrap();
        let syn_ack = read_n(&mut car, syn.len(), "SYN|ACK").await;
        assert!(sent.elapsed() < PROMPT, "SYN|ACK took {:?}", sent.elapsed());
        let parsed = assert_syn_ack(&syn_ack);

        car.write_all(&Iap2Packet::ack(0x18, parsed.seq).to_bytes()).await.unwrap();
        let (link, result) = timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        result.expect("negotiation must complete on the car's ACK");
        assert!(link.is_established());
        assert_eq!(link.control_session_id(), 0x01);
        assert_eq!(link.max_packet, 0x1000);
    }

    /// The car re-sends the same SYN after 1 s when it has not seen our SYN|ACK:
    /// we answer the retransmission with the same valid SYN|ACK, promptly.
    #[tokio::test]
    async fn negotiate_answers_retransmitted_car_syn() {
        let (link_io, mut car) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut link = Iap2Link::new(link_io, LinkConfig::default());
            let result = link.negotiate_device().await;
            (link, result)
        });

        let syn = hex::decode(CAR_SYN).unwrap();
        car.write_all(&CAR_DETECT).await.unwrap();
        assert_eq!(read_n(&mut car, 6, "detect echo").await, CAR_DETECT);
        car.write_all(&syn).await.unwrap();
        assert_syn_ack(&read_n(&mut car, syn.len(), "first SYN|ACK").await);

        car.write_all(&syn).await.unwrap();
        let parsed = assert_syn_ack(&read_n(&mut car, syn.len(), "SYN|ACK for the retransmitted SYN").await);

        car.write_all(&Iap2Packet::ack(0x18, parsed.seq).to_bytes()).await.unwrap();
        let (link, result) = timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        result.expect("negotiation must complete after the retransmitted SYN");
        assert!(link.is_established());
    }

    #[test]
    fn parse_control_session_id_from_canonical_syn() {
        // The canonical iap2-rs SYN payload: control session is id 0x0A, type 0x00.
        let syn = Iap2Packet::syn(0x9D);
        assert_eq!(
            Iap2Link::<tokio::io::DuplexStream>::parse_control_session_id(&syn.payload),
            Some(0x0A)
        );
    }

    #[test]
    fn parse_file_transfer_session_id_from_the_jaguar_syn() {
        // The Jaguar's SYN payload (sessions (1, control, v2) (2, file transfer, v1)).
        let jaguar = [
            0x01, 0x05, 0x10, 0x00, 0x01, 0xf4, 0x00, 0xb0, 0x1e, 0x0a,
            0x01, 0x00, 0x02, 0x02, 0x01, 0x01,
        ];
        type L = Iap2Link<tokio::io::DuplexStream>;
        assert_eq!(L::parse_control_session_id(&jaguar), Some(0x01));
        assert_eq!(L::parse_session_id_of_type(&jaguar, 0x01), Some(0x02));
        // The canonical iap2-rs SYN lists (1, file transfer) BEFORE (0x0A, control):
        // the lookup is by type, never by position.
        assert_eq!(L::parse_session_id_of_type(&Iap2Packet::syn(0x9D).payload, 0x01), Some(0x01));
        assert_eq!(L::parse_session_id_of_type(&jaguar, 0x02), None);
    }

    #[test]
    fn parse_control_session_id_none_when_too_short() {
        assert_eq!(
            Iap2Link::<tokio::io::DuplexStream>::parse_control_session_id(&[0u8; 4]),
            None
        );
    }

    #[test]
    fn parse_max_packet_from_canonical_syn() {
        let syn = Iap2Packet::syn(0x9D);
        assert_eq!(
            Iap2Link::<tokio::io::DuplexStream>::parse_max_packet(&syn.payload),
            Some(4096)
        );
        // Too short, and a size that could not even hold a header, are rejected.
        assert_eq!(
            Iap2Link::<tokio::io::DuplexStream>::parse_max_packet(&[0x01, 0x05, 0x10]),
            None
        );
        assert_eq!(
            Iap2Link::<tokio::io::DuplexStream>::parse_max_packet(&[0x01, 0x05, 0x00, 0x02]),
            None
        );
    }

    /// Sequence numbers are 8-bit and wrap; `self.seq + 1` panicked in debug
    /// builds after 255 packets, killing the transmitter mid-session.
    #[test]
    fn next_seq_wraps_past_0xff() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());
        link.seq = 0xFE;
        assert_eq!(link.next_seq(), 0xFF);
        link.seq = 0xFF;
        assert_eq!(link.next_seq(), 0x00);
    }

    /// An oversized payload must be refused, not encoded with a length field
    /// that contradicts its contents.
    #[tokio::test]
    async fn send_data_rejects_payload_above_the_negotiated_maximum() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());
        link.state = LinkState::Established;
        link.max_packet = 64;

        let too_big = Bytes::from(vec![0u8; 64]);
        let err = link
            .send_data(0x0A, too_big)
            .await
            .expect_err("payload larger than the negotiated max packet must be refused");
        assert!(
            matches!(err, Iap2Error::Protocol(_)),
            "expected a protocol error, got {err:?}"
        );

        // One byte under the limit is still accepted (write goes to the duplex).
        assert_eq!(link.max_payload(), 64 - 9 - 1);
    }

    /// TX history must hold the real DATA packet so an EAK can be answered with
    /// the missing message instead of an empty ACK, and cumulative ACKs prune it.
    #[test]
    fn tx_history_remembers_and_prunes() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());

        for seq in 0..5u8 {
            let pkt = Iap2Packet::data(seq, 0x00, 0x0A, Bytes::from(vec![seq; 4]));
            link.remember_sent(&pkt);
        }
        assert_eq!(link.tx_history.len(), 5);
        assert_eq!(&link.tx_history[&3].payload[..], &[3u8; 4]);

        // A cumulative ACK for 0x02 clears 0x00..=0x02 only.
        link.prune_tx_history(0x02);
        assert_eq!(
            link.tx_history.keys().copied().collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    /// The history is bounded: a peer that never ACKs cannot grow it without end.
    #[test]
    fn tx_history_is_bounded() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());

        for seq in 0..=200u8 {
            let pkt = Iap2Packet::data(seq, 0x00, 0x0A, Bytes::from_static(b"x"));
            link.remember_sent(&pkt);
        }
        assert!(
            link.tx_history.len() <= MAX_TX_HISTORY,
            "history grew to {}",
            link.tx_history.len()
        );
        // The most recent packet is always retained.
        assert!(link.tx_history.contains_key(&200));
    }
}

#[cfg(test)]
mod ack_only_seq_tests {
    use super::*;

    /// A zero-payload ACK must carry the LAST CONSUMED sequence number, never the
    /// next one a data packet will use.
    ///
    /// Regression for a duplicate sequence number we put on the wire. From our own
    /// usbmon capture (boot-20260921-081439 run 127), OUT direction:
    ///
    ///     2.8072   9 B  ctrl 0x40  seq 05  ack fc   <- ACK-only
    ///     2.8075  16 B  ctrl 0x40  seq 05  ack fc   <- DATA with the SAME seq
    ///
    /// QNX/Harman head units treat "Packet seq received not expected range" as a
    /// hard stop (catplay issue #12, confirmed by disassembly of the car's stack),
    /// which wedges their CarPlay manager until a watchdog resets the USB port.
    #[test]
    fn ack_only_carries_the_last_consumed_sequence() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());

        // `seq` is the NEXT number a data packet will use.
        link.seq = 0x05;
        assert_eq!(
            link.ack_only_seq(),
            0x04,
            "ACK-only must not reuse the number the next DATA packet will take"
        );

        // It must wrap like every other sequence number on this link.
        link.seq = 0x00;
        assert_eq!(link.ack_only_seq(), 0xFF, "must wrap, not underflow");
    }

    /// The bug was invisible whenever a data packet was still unacknowledged,
    /// because `seq` only advances once the peer ACKs — which is why it showed up
    /// intermittently rather than on every packet.
    #[test]
    fn ack_only_is_one_below_the_next_data_packet_for_every_value() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut link = Iap2Link::new(io, LinkConfig::default());
        for next in 0u8..=255 {
            link.seq = next;
            assert_eq!(link.ack_only_seq(), next.wrapping_sub(1));
            assert_ne!(
                link.ack_only_seq(),
                link.seq,
                "an ACK-only and the following DATA packet must never share a seq"
            );
        }
    }
}

/// Our SYN-ACK link parameters, derived from the accessory's SYN.
///
/// The SYN payload is:
///   [0]    LinkVersion
///   [1]    MaxOutstandingPackets     <- OUR receive capability
///   [2..4] MaxRecvPacketLength (BE)  <- OUR receive capability
///   [4..6] RetransmitTimeout (BE, ms)
///   [6..8] CumulativeAckTimeout (BE, ms)
///   [8]    MaxRetransmissions
///   [9]    MaxCumulativeAcks
///   [10..] session descriptors, 3 bytes each (id, type, version)
///
/// We used to send the accessory's payload back unchanged. A working dongle does
/// not. From a wire capture of a SUCCESSFUL wired CarPlay bring-up on a Pioneer
/// DMH-1700NEX (Cynthion/Packetry, greatscottgadgets/cynthion issue 260,
/// `Speed_High_CarPlay_USB_RoleSwap.pcapng`, frames at t~0.29 s):
///
///   head unit SYN : 01 08 1000 2710 07d0 1e 07 | 01 00 02  02 01 01
///   dongle SYN|ACK: 01 7f ffff 2710 07d0 1e 07 | 01 00 02  02 01 01
///                      ^^ ^^^^
///
/// i.e. it RAISES MaxOutstandingPackets to 127 and MaxRecvPacketLength to 65535
/// and leaves the timing fields and session descriptors exactly as proposed.
///
/// This matters on our Jaguar, which proposes only 5 outstanding packets: echoing
/// that tells the car it may have just five packets in flight to us before it has
/// to stop and wait for an ACK — with a 176 ms cumulative-ACK timeout, far tighter
/// than the Pioneer's 2000 ms. Whether it explains the teardown is unproven, but
/// understating our own receive capability is wrong regardless.
///
/// Timing fields are left alone deliberately: they govern the ACCESSORY's
/// retransmission behaviour, and a known-good implementation does not touch them.
/// Retransmission timeout the accessory named in its SYN payload (bytes 4..6,
/// big-endian ms). `counter_propose` deliberately leaves this field alone, so
/// whatever the accessory asked for is what we agreed to and what we must use.
///
/// Falls back to `default_ms` when the payload is too short, and clamps to the
/// spec's 20..=65535 ms range so a malformed or zero value cannot turn the
/// retransmit loop into a busy spin.
fn negotiated_retransmit_ms(syn_payload: &[u8], default_ms: u64) -> u64 {
    const MIN_MS: u64 = 20;
    const MAX_MS: u64 = 65_535;
    if syn_payload.len() < 6 {
        return default_ms;
    }
    let proposed = u64::from(u16::from_be_bytes([syn_payload[4], syn_payload[5]]));
    if proposed == 0 {
        return default_ms;
    }
    proposed.clamp(MIN_MS, MAX_MS)
}

fn counter_propose(syn_payload: &[u8]) -> bytes::Bytes {
    /// What we can actually receive. `link.rs` reassembles from a growing buffer
    /// and `ControlAssembler` handles fragmentation, so neither is a real limit.
    const OUR_MAX_OUTSTANDING: u8 = 127;
    const OUR_MAX_RECV_LEN: u16 = u16::MAX;

    let mut p = syn_payload.to_vec();
    if p.len() < 10 {
        // Too short to be a link-parameter payload — echo rather than corrupt it.
        return bytes::Bytes::from(p);
    }
    p[1] = OUR_MAX_OUTSTANDING;
    p[2..4].copy_from_slice(&OUR_MAX_RECV_LEN.to_be_bytes());
    bytes::Bytes::from(p)
}

#[cfg(test)]
mod syn_ack_tests {
    use super::*;

    /// Byte-exact payloads from the Pioneer capture referenced above.
    const HU_SYN: [u8; 16] = [
        0x01, 0x08, 0x10, 0x00, 0x27, 0x10, 0x07, 0xd0, 0x1e, 0x07,
        0x01, 0x00, 0x02, 0x02, 0x01, 0x01,
    ];
    const DONGLE_SYN_ACK: [u8; 16] = [
        0x01, 0x7f, 0xff, 0xff, 0x27, 0x10, 0x07, 0xd0, 0x1e, 0x07,
        0x01, 0x00, 0x02, 0x02, 0x01, 0x01,
    ];

    #[test]
    fn reproduces_the_working_dongles_syn_ack_byte_for_byte() {
        assert_eq!(&counter_propose(&HU_SYN)[..], &DONGLE_SYN_ACK[..]);
    }

    #[test]
    fn timing_fields_and_sessions_are_left_exactly_as_proposed() {
        // Our Jaguar's SYN: 5 outstanding, 4096 bytes, 500 ms retransmit,
        // 176 ms cumulative ACK, 30 retransmissions, 10 cumulative ACKs.
        let jaguar = [
            0x01, 0x05, 0x10, 0x00, 0x01, 0xf4, 0x00, 0xb0, 0x1e, 0x0a,
            0x01, 0x00, 0x02, 0x02, 0x01, 0x01,
        ];
        let out = counter_propose(&jaguar);
        assert_eq!(out[1], 127, "must raise MaxOutstandingPackets");
        assert_eq!(&out[2..4], &[0xff, 0xff], "must raise MaxRecvPacketLength");
        assert_eq!(&out[4..], &jaguar[4..], "timing and sessions must be untouched");
        assert_eq!(out[0], jaguar[0], "LinkVersion must be echoed, not invented");
        assert_eq!(out.len(), jaguar.len(), "length must not change");
    }

    #[test]
    fn a_short_payload_is_echoed_rather_than_corrupted() {
        // Never index past the end just because a peer sent something odd.
        for n in 0..10usize {
            let short = vec![0xAAu8; n];
            assert_eq!(&counter_propose(&short)[..], &short[..], "len {n}");
        }
    }
}

#[cfg(test)]
mod syn_ack_semantics_tests {
    use super::*;

    /// Guard the PROPERTY, not just the byte string: whatever the peer proposes,
    /// our SYN|ACK must advertise our real receive capability and must not
    /// silently adopt the peer's timing as if it were ours.
    #[test]
    fn syn_ack_never_understates_our_receive_capability() {
        for outstanding in [1u8, 5, 8, 64] {
            for maxlen in [64u16, 1024, 4096] {
                let mut syn = vec![0x01, outstanding];
                syn.extend_from_slice(&maxlen.to_be_bytes());
                syn.extend_from_slice(&[0x01, 0xf4, 0x00, 0xb0, 0x1e, 0x0a]);
                syn.extend_from_slice(&[0x01, 0x00, 0x02]);
                let out = counter_propose(&syn);
                assert!(
                    out[1] >= outstanding,
                    "we must never advertise FEWER outstanding packets than proposed"
                );
                assert!(
                    u16::from_be_bytes([out[2], out[3]]) >= maxlen,
                    "we must never advertise a SMALLER receive packet length"
                );
            }
        }
    }
}

#[cfg(test)]
mod negotiated_retransmit_tests {
    use super::*;

    /// The real Jaguar F-PACE SYN payload. It asks for 500 ms; we used 1500 ms,
    /// so on a dropped DATA packet the car's timer fired first and we burned
    /// 0.7-1.9 s of its budget waiting for an ACK that was never coming.
    const JAGUAR_SYN: &[u8] = &[0x01, 0x05, 0x10, 0x00, 0x01, 0xf4, 0x00, 0xb0, 0x1e, 0x0a];

    #[test]
    fn adopts_the_accessorys_retransmit_timeout() {
        assert_eq!(negotiated_retransmit_ms(JAGUAR_SYN, 1500), 500);
    }

    /// We must not counter-propose this field, or "what we agreed to" and "what
    /// we use" drift apart again -- which is the bug this fixes.
    #[test]
    fn counter_propose_leaves_the_timing_fields_untouched() {
        let out = counter_propose(JAGUAR_SYN);
        assert_eq!(&out[4..10], &JAGUAR_SYN[4..10], "timing fields must be echoed");
        assert_eq!(
            negotiated_retransmit_ms(&out, 1500),
            negotiated_retransmit_ms(JAGUAR_SYN, 1500),
            "the value we send back must be the value we then honour"
        );
    }

    #[test]
    fn falls_back_and_clamps_rather_than_spinning() {
        assert_eq!(negotiated_retransmit_ms(&[0x01, 0x05, 0x10], 1500), 1500, "short payload");
        let zero = [0x01, 0x05, 0x10, 0x00, 0x00, 0x00, 0x00, 0xb0, 0x1e, 0x0a];
        assert_eq!(negotiated_retransmit_ms(&zero, 1500), 1500, "zero is not a timeout");
        let tiny = [0x01, 0x05, 0x10, 0x00, 0x00, 0x05, 0x00, 0xb0, 0x1e, 0x0a];
        assert_eq!(negotiated_retransmit_ms(&tiny, 1500), 20, "clamped to the spec floor");
    }

    /// A fresh link must not claim a negotiated value before any SYN is seen.
    #[test]
    fn defaults_to_config_before_negotiation() {
        let cfg = LinkConfig { timeout_ms: 1234, ..Default::default() };
        assert_eq!(negotiated_retransmit_ms(&[], cfg.timeout_ms), 1234);
    }
}
