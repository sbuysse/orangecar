//! iAP2 link-layer packet framing.
//!
//! Lifted **nearly verbatim** from usenocturne/iap2-rs (`src/packet.rs`, GPL-3).
//! The only change from upstream is the error import path (`super::error`
//! instead of `crate::error`). The wire format — `FF 5A` SOP, big-endian packet
//! length, control byte, seq/ack, session id, the two 8-bit two's-complement
//! checksums (header over bytes 0..8, payload trailer) — is byte-for-byte
//! identical, so it interoperates with any conformant iAP2 peer regardless of
//! which side we play.
//!
//! See `mod.rs` for the full derivation / GPL-3 note.

use bytes::{BufMut, Bytes, BytesMut};
use std::fmt;
use tracing::error;

use super::error::{Iap2Error, Result};

const SYNC_BYTE: u8 = 0xFF;
const SOP_BYTE: u8 = 0x5A;
/// SOP(2) + length(2) + control(1) + seq(1) + ack(1) + session(1) + checksum(1).
pub const PACKET_HEADER_SIZE: usize = 9;
/// A packet carrying a payload appends a single trailing payload checksum byte.
pub const PAYLOAD_CHECKSUM_SIZE: usize = 1;

/// Largest payload that can be described by the 16-bit on-the-wire length field.
/// The link layer enforces the (smaller) negotiated maximum — this is only the
/// absolute encoding ceiling.
pub const MAX_ENCODABLE_PAYLOAD: usize =
    u16::MAX as usize - PACKET_HEADER_SIZE - PAYLOAD_CHECKSUM_SIZE;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PacketType {
    Syn = 0x80,
    SynAck = 0xC0,
    Ack = 0x40,
    Data = 0x00,
    Eak = 0x60,
    Rst = 0xA0,
    Suspend = 0xE0,
    SuspendAck = 0xF0,
    Detect = 0xEE,
}

impl TryFrom<u8> for PacketType {
    type Error = Iap2Error;

    fn try_from(value: u8) -> Result<Self> {
        if value == 0xEE {
            return Ok(PacketType::Detect);
        }

        match value & 0xF0 {
            0x80 => Ok(PacketType::Syn),
            0xC0 => Ok(PacketType::SynAck),
            0x40 => Ok(PacketType::Ack),
            0x00 => Ok(PacketType::Data),
            0x60 => Ok(PacketType::Eak),
            0xA0 => Ok(PacketType::Rst),
            0xE0 => Ok(PacketType::Suspend),
            0xF0 => Ok(PacketType::SuspendAck),
            _ => Err(Iap2Error::InvalidPacket(format!(
                "Unknown packet type: 0x{:02X}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlByte {
    pub packet_type: PacketType,
    pub has_session: bool,
}

impl ControlByte {
    pub fn new(packet_type: PacketType) -> Self {
        ControlByte {
            packet_type,
            has_session: false,
        }
    }

    pub fn to_byte(&self) -> u8 {
        (self.packet_type as u8) | (if self.has_session { 0x08 } else { 0x00 })
    }

    pub fn from_byte(byte: u8) -> Result<Self> {
        let packet_type = PacketType::try_from(byte)?;
        Ok(ControlByte {
            packet_type,
            has_session: (byte & 0x08) != 0,
        })
    }
}

#[derive(Clone)]
pub struct Iap2Packet {
    pub control: ControlByte,
    pub seq: u8,
    pub ack: u8,
    pub session_id: Option<u8>,
    pub payload: Bytes,
}

impl Iap2Packet {
    pub fn new(control: ControlByte, seq: u8, ack: u8) -> Self {
        Iap2Packet {
            control,
            seq,
            ack,
            session_id: None,
            payload: Bytes::new(),
        }
    }

    pub fn detect() -> Self {
        let control = ControlByte::new(PacketType::Detect);
        Iap2Packet::new(control, 0x10, 0x00)
    }

    /// The canonical iAP2 link-synchronization payload used by iap2-rs. It
    /// declares link version 1, 5 outstanding packets, 4096-byte max packet, the
    /// retransmit/cumulative-ack timers, and the session table (the control
    /// session is id `0x0A`, session type `0x00`). On the Device side we do not
    /// originate the SYN (the accessory does — see `link.rs`), but we reuse this
    /// builder to echo an accepted config back in the SYN-ACK and for symmetric
    /// tests.
    pub fn syn(seq: u8) -> Self {
        let control = ControlByte::new(PacketType::Syn);
        let mut packet = Iap2Packet::new(control, seq, 0x00);

        let mut payload = BytesMut::new();
        payload.put_u8(0x01);
        payload.put_u8(0x05);
        payload.put_u8(0x10);
        payload.put_u8(0x00);
        payload.put_u8(0x04);
        payload.put_u8(0x0B);
        payload.put_u8(0x00);
        payload.put_u8(0x17);
        payload.put_u8(0x03);
        payload.put_u8(0x03);
        payload.put_u8(0x01);
        payload.put_u8(0x01);
        payload.put_u8(0x02);
        payload.put_u8(0x0A);
        payload.put_u8(0x00);
        payload.put_u8(0x01);
        payload.put_u8(0x0B);
        payload.put_u8(0x02);
        payload.put_u8(0x01);

        packet.payload = payload.freeze();
        packet
    }

    pub fn ack(seq: u8, ack: u8) -> Self {
        let control = ControlByte::new(PacketType::Ack);
        Iap2Packet::new(control, seq, ack)
    }

    pub fn ack_with_session(seq: u8, ack: u8, session_id: u8) -> Self {
        // ACK (0x40) with the session id in header byte 7 — no SLP/0x08 bit (see
        // `data()`); matches catplay's `new_ack` and the car's own bare ACKs.
        let mut p = Self::ack(seq, ack);
        p.session_id = Some(session_id);
        p
    }

    /// Build a SYN-ACK carrying an explicit payload (used by the Device side to
    /// echo the accessory's accepted link parameters).
    pub fn syn_ack(seq: u8, ack: u8, payload: Bytes) -> Self {
        let control = ControlByte::new(PacketType::SynAck);
        let mut packet = Iap2Packet::new(control, seq, ack);
        packet.payload = payload;
        packet
    }

    pub fn data(seq: u8, ack: u8, session_id: u8, payload: Bytes) -> Self {
        // Control byte = ACK (0x40) only. The `0x08` bit is NOT a "session present"
        // marker (the session id is always in header byte 7, set via `session_id`
        // below) — it is the link SLP/Device-Sleep flag (catplay `header.rs` SLP =
        // 0b0000_1000). Asserting it on every DATA packet told the head unit "device
        // going to sleep" on each frame; catplay and the real car both send 0x40.
        let control = ControlByte::new(PacketType::Ack);
        let mut packet = Iap2Packet::new(control, seq, ack);
        packet.session_id = Some(session_id);
        packet.payload = payload;
        packet
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(PACKET_HEADER_SIZE + self.payload.len());

        buf.put_u8(SYNC_BYTE);
        buf.put_u8(SOP_BYTE);

        if self.control.packet_type == PacketType::Detect {
            buf.put_u16(0x0006);
            buf.put_u8(self.control.to_byte());
            buf.put_u8(self.seq);
            return buf.freeze();
        }

        let has_payload = !self.payload.is_empty();
        // Compute in usize: `PACKET_HEADER_SIZE as u16 + payload.len() as u16`
        // wraps silently once the payload passes ~64 KiB, emitting a packet whose
        // length field contradicts its contents. The link layer rejects anything
        // above the negotiated maximum long before this, so reaching the ceiling
        // here means a caller bypassed it — say so instead of truncating.
        let total_len = PACKET_HEADER_SIZE
            + self.payload.len()
            + if has_payload { PAYLOAD_CHECKSUM_SIZE } else { 0 };
        if total_len > u16::MAX as usize {
            error!(
                "iAP2 packet of {} bytes cannot be encoded (16-bit length field);                  emitting a truncated-length packet the peer will reject.                  Payload must be capped at {} bytes by the caller.",
                total_len, MAX_ENCODABLE_PAYLOAD
            );
        }
        let packet_len = total_len as u16;

        buf.put_u16(packet_len);

        buf.put_u8(self.control.to_byte());
        buf.put_u8(self.seq);
        buf.put_u8(self.ack);

        if let Some(session) = self.session_id {
            buf.put_u8(session);
        } else {
            buf.put_u8(0x00);
        }

        let header_checksum = Self::calculate_header_checksum(&buf[0..8]);
        buf.put_u8(header_checksum);

        if has_payload {
            buf.put_slice(&self.payload);

            let payload_checksum = Self::calculate_payload_checksum(&self.payload);
            buf.put_u8(payload_checksum);
        }

        buf.freeze()
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 6 {
            return Err(Iap2Error::InvalidPacket(format!(
                "Packet too short: {} bytes",
                data.len()
            )));
        }

        if data[0] != SYNC_BYTE || data[1] != SOP_BYTE {
            return Err(Iap2Error::InvalidPacket(
                "Invalid sync/SOP bytes".to_string(),
            ));
        }

        let packet_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        if data.len() < packet_len {
            return Err(Iap2Error::InvalidPacket(format!(
                "Incomplete packet: expected {} bytes, got {}",
                packet_len,
                data.len()
            )));
        }

        let control = ControlByte::from_byte(data[4])?;

        if control.packet_type == PacketType::Detect && packet_len == 6 {
            return Ok(Iap2Packet {
                control,
                seq: data[5],
                ack: 0,
                session_id: None,
                payload: Bytes::new(),
            });
        }

        if data.len() < PACKET_HEADER_SIZE {
            return Err(Iap2Error::InvalidPacket(format!(
                "Packet too short for type {:?}: {} bytes",
                control.packet_type,
                data.len()
            )));
        }

        let seq = data[5];
        let ack = data[6];
        let session_id = if data[7] != 0 { Some(data[7]) } else { None };

        let mut sum: u8 = 0;
        for &byte in data.iter().take(9) {
            sum = sum.wrapping_add(byte);
        }
        if sum != 0 {
            return Err(Iap2Error::InvalidPacket(format!(
                "Header checksum verification failed: sum=0x{:02X} (expected 0)",
                sum
            )));
        }

        let payload_start = PACKET_HEADER_SIZE;
        let payload = if packet_len > payload_start {
            let payload_data = &data[payload_start..packet_len];

            if !payload_data.is_empty() {
                let mut payload_sum: u8 = 0;
                for &byte in payload_data {
                    payload_sum = payload_sum.wrapping_add(byte);
                }
                if payload_sum != 0 {
                    return Err(Iap2Error::InvalidPacket(format!(
                        "Payload checksum verification failed: sum=0x{:02X} (expected 0)",
                        payload_sum
                    )));
                }
                Bytes::copy_from_slice(&payload_data[..payload_data.len() - 1])
            } else {
                Bytes::new()
            }
        } else {
            Bytes::new()
        };

        Ok(Iap2Packet {
            control,
            seq,
            ack,
            session_id,
            payload,
        })
    }

    fn calculate_header_checksum(data: &[u8]) -> u8 {
        let mut sum: u8 = 0;
        for &byte in data {
            sum = sum.wrapping_add(byte);
        }
        (!sum).wrapping_add(1)
    }

    fn calculate_payload_checksum(data: &[u8]) -> u8 {
        let mut sum: u8 = 0;
        for &byte in data {
            sum = sum.wrapping_add(byte);
        }
        (!sum).wrapping_add(1)
    }
}

/// Decode a raw link packet header for wire tracing, WITHOUT validating it.
///
/// Control bits are printed under the iAP2 link-spec names (SYN 0x80, ACK 0x40,
/// EAK 0x20, RST 0x10, SUS 0x08) alongside the [`PacketType`] this port derives
/// from the same byte, so any disagreement between the two readings is visible
/// in a capture. Checksums are recomputed and reported ok/bad.
pub fn describe_raw(data: &[u8]) -> String {
    if data.len() < 6 {
        return format!("<{} bytes: too short for a header>", data.len());
    }
    let sop_ok = data[0] == SYNC_BYTE && data[1] == SOP_BYTE;
    let declared_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let ctrl = data[4];
    let mut bits: Vec<&str> = Vec::new();
    for (mask, name) in [(0x80, "SYN"), (0x40, "ACK"), (0x20, "EAK"), (0x10, "RST"), (0x08, "SUS")] {
        if ctrl & mask != 0 {
            bits.push(name);
        }
    }
    let port_type = PacketType::try_from(ctrl)
        .map(|t| format!("{t:?}"))
        .unwrap_or_else(|_| "invalid".to_string());
    if data.len() < PACKET_HEADER_SIZE {
        return format!(
            "sop={} len={declared_len} ctrl=0x{ctrl:02x} [{}] port_type={port_type} seq=0x{:02x} (short header, {} bytes)",
            if sop_ok { "ok" } else { "BAD" },
            bits.join("|"),
            data[5],
            data.len()
        );
    }
    let header_ok = data[..PACKET_HEADER_SIZE]
        .iter()
        .fold(0u8, |s, &b| s.wrapping_add(b))
        == 0;
    let end = declared_len.min(data.len());
    let (payload_len, payload_ck) = if end > PACKET_HEADER_SIZE {
        let region = &data[PACKET_HEADER_SIZE..end];
        let ok = region.iter().fold(0u8, |s, &b| s.wrapping_add(b)) == 0;
        (region.len().saturating_sub(PAYLOAD_CHECKSUM_SIZE), if ok { "ok" } else { "BAD" })
    } else {
        (0, "n/a")
    };
    format!(
        "sop={} len={declared_len} ctrl=0x{ctrl:02x} [{}] port_type={port_type} seq=0x{:02x} ack=0x{:02x} session=0x{:02x} payload_len={payload_len} header_ck={} payload_ck={payload_ck}",
        if sop_ok { "ok" } else { "BAD" },
        bits.join("|"),
        data[5],
        data[6],
        data[7],
        if header_ok { "ok" } else { "BAD" },
    )
}

impl fmt::Debug for Iap2Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Iap2Packet {{ type: {:?}, seq: 0x{:02X}, ack: 0x{:02X}, session: {:?}, payload_len: {} }}",
            self.control.packet_type,
            self.seq,
            self.ack,
            self.session_id,
            self.payload.len()
        )
    }
}
