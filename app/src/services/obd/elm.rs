//! ELM327 reply parsing shared by the PID, DTC and VIN decoders.
//!
//! The adapter runs with echo off, linefeeds off, spaces on and headers off
//! (`ATE0 ATL0 ATS1 ATH0`), so a reply is one or more `\r`-separated lines:
//!
//! ```text
//! 41 0C 1A F8                    single frame, one ECU
//! 43 00\r43 01 04 20             mode 03 on CAN: one line per answering ECU
//! 014\r0: 49 02 01 53 41 4A\r1: ...   ISO-TP multi-frame: byte count, then
//!                                     "N:"-numbered frames of one message
//! SEARCHING...\r41 00 BE 3F A8 13     protocol search before the first answer
//! ```
//!
//! Without headers the answering ECU is not identified, but every ECU's reply
//! still starts on its own line, which is what the decoders rely on.

/// Bytes of one line of hex, or `None` for status text ("NO DATA",
/// "SEARCHING...", "?", "BUS INIT: ...OK") and for the 3-digit ISO-TP byte
/// count that precedes a multi-frame reply. Tolerates adapters that ignore
/// `ATS1` and print the bytes without spaces.
pub fn hex_line_bytes(line: &str) -> Option<Vec<u8>> {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() || compact.len() % 2 != 0 {
        return None;
    }
    if !compact.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).ok())
        .collect()
}

/// Split a raw reply into its lines (the adapter uses `\r`, a few clones `\n`).
pub fn lines(resp: &str) -> impl Iterator<Item = &str> {
    resp.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty())
}

/// `"1: 07 89 01"` → `Some((1, "07 89 01"))`.
fn frame_index(line: &str) -> Option<(u8, &str)> {
    let (idx, rest) = line.split_once(':')?;
    let idx = idx.trim();
    if idx.len() == 1 {
        let n = u8::from_str_radix(idx, 16).ok()?;
        return Some((n, rest));
    }
    None
}

/// ISO-TP byte count line ("00A", "014"): three hex digits, nothing else.
fn byte_count(line: &str) -> Option<usize> {
    if line.len() == 3 && line.chars().all(|c| c.is_ascii_hexdigit()) {
        usize::from_str_radix(line, 16).ok()
    } else {
        None
    }
}

/// Group a reply into messages: every single-frame line is a message of its
/// own; a byte-count line plus its `N:` frames is one message, truncated to the
/// announced length (the last CAN frame is padded).
pub fn messages(resp: &str) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    // Remaining bytes expected by the multi-frame message being assembled.
    let mut pending: Option<usize> = None;
    for line in lines(resp) {
        if let Some(n) = byte_count(line) {
            out.push(Vec::with_capacity(n));
            pending = Some(n);
            continue;
        }
        if let Some((_, rest)) = frame_index(line) {
            let Some(bytes) = hex_line_bytes(rest) else { continue };
            match (pending, out.last_mut()) {
                (Some(left), Some(msg)) => {
                    let take = bytes.len().min(left);
                    msg.extend_from_slice(&bytes[..take]);
                    pending = if left > take { Some(left - take) } else { None };
                }
                // A frame without a preceding count (count line lost): keep it
                // as a message of its own rather than dropping data.
                _ => out.push(bytes),
            }
            continue;
        }
        if let Some(bytes) = hex_line_bytes(line) {
            pending = None;
            out.push(bytes);
        }
    }
    out.retain(|m| !m.is_empty());
    out
}

/// ELM327 protocol number from an `ATDPN` reply ("A6" = automatic, chose 6).
pub fn protocol_number(resp: &str) -> Option<u8> {
    let line = lines(resp).last()?;
    let digit = line.trim_start_matches(['A', 'a']).chars().next()?;
    digit.to_digit(16).map(|d| d as u8)
}

/// Protocols 6-9 are ISO 15765-4 CAN; A-C are J1939/user CAN.
pub fn protocol_is_can(proto: u8) -> bool {
    (6..=0xC).contains(&proto)
}

/// ISO 15765-4 with 11-bit identifiers (500 or 250 kbaud): the only protocols
/// where the engine ECU has the fixed physical request id 7E0 (reply 7E8).
pub fn protocol_is_can_11bit(proto: u8) -> bool {
    proto == 6 || proto == 8
}

/// True when an AT command was accepted ("OK" line). A clone that does not
/// know the command answers "?".
pub fn is_ok(resp: &str) -> bool {
    lines(resp).any(|l| l.eq_ignore_ascii_case("OK"))
}

pub fn protocol_name(proto: u8) -> &'static str {
    match proto {
        0 => "automatic",
        1 => "SAE J1850 PWM",
        2 => "SAE J1850 VPW",
        3 => "ISO 9141-2",
        4 => "ISO 14230-4 KWP (5 baud init)",
        5 => "ISO 14230-4 KWP (fast init)",
        6 => "ISO 15765-4 CAN (11 bit, 500 kbaud)",
        7 => "ISO 15765-4 CAN (29 bit, 500 kbaud)",
        8 => "ISO 15765-4 CAN (11 bit, 250 kbaud)",
        9 => "ISO 15765-4 CAN (29 bit, 250 kbaud)",
        0xA => "SAE J1939 CAN",
        _ => "user CAN",
    }
}

/// A multi-frame reply as the ELM prints it with `ATH0 ATS1 ATCAF1`: the
/// ISO-TP byte count, then `N:` frames of 6 bytes (first frame) and 7 bytes
/// (consecutive frames), the last one padded. For building realistic test
/// replies.
#[cfg(test)]
pub fn iso_tp(bytes: &[u8]) -> String {
    let first = 6.min(bytes.len());
    let mut frames: Vec<&[u8]> = vec![&bytes[..first]];
    frames.extend(bytes[first..].chunks(7));
    let mut out = format!("{:03X}\r", bytes.len());
    for (i, f) in frames.iter().enumerate() {
        let mut f = f.to_vec();
        f.resize(if i == 0 { 6 } else { 7 }, 0xAA);
        let hex: Vec<String> = f.iter().map(|b| format!("{b:02X}")).collect();
        out.push_str(&format!("{:X}: {} \r", i % 16, hex.join(" ")));
    }
    out.push('\r');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lines_with_and_without_spaces() {
        assert_eq!(hex_line_bytes("41 0C 1A F8"), Some(vec![0x41, 0x0C, 0x1A, 0xF8]));
        assert_eq!(hex_line_bytes("410C1AF8"), Some(vec![0x41, 0x0C, 0x1A, 0xF8]));
        assert_eq!(hex_line_bytes("NO DATA"), None);
        assert_eq!(hex_line_bytes("SEARCHING..."), None);
        assert_eq!(hex_line_bytes("00A"), None);
        assert_eq!(hex_line_bytes("?"), None);
    }

    #[test]
    fn single_frames_one_message_per_line() {
        let m = messages("SEARCHING...\r41 00 BE 3F A8 13\r41 00 98 18 80 11\r\r");
        assert_eq!(m, vec![vec![0x41, 0x00, 0xBE, 0x3F, 0xA8, 0x13], vec![0x41, 0x00, 0x98, 0x18, 0x80, 0x11]]);
    }

    #[test]
    fn multi_frame_reassembled_and_truncated_to_count() {
        // VIN reply on CAN: 0x14 = 20 bytes: 49 02 01 + 17 characters.
        let resp = "014\r0: 49 02 01 53 41 4A\r1: 41 41 32 41 56 33 48\r2: 41 31 32 33 34 35 36";
        let m = messages(resp);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].len(), 20);
        assert_eq!(&m[0][..3], &[0x49, 0x02, 0x01]);
        assert_eq!(*m[0].last().unwrap(), 0x36);
    }

    #[test]
    fn padding_after_announced_length_dropped() {
        let resp = "00A\r0: 43 04 01 23 04 56\r1: 07 89 01 11 00 00 00";
        assert_eq!(messages(resp), vec![vec![0x43, 0x04, 0x01, 0x23, 0x04, 0x56, 0x07, 0x89, 0x01, 0x11]]);
    }

    #[test]
    fn protocol_numbers() {
        assert_eq!(protocol_number("A6"), Some(6));
        assert_eq!(protocol_number("6"), Some(6));
        assert_eq!(protocol_number("A3"), Some(3));
        assert!(protocol_is_can(6));
        assert!(!protocol_is_can(3));
        assert!(protocol_is_can_11bit(protocol_number("A6").unwrap()));
        assert!(protocol_is_can_11bit(8));
        assert!(!protocol_is_can_11bit(7), "29-bit CAN has no 7E0");
        assert!(!protocol_is_can_11bit(3));
    }

    #[test]
    fn at_command_acceptance() {
        assert!(is_ok("OK"));
        assert!(is_ok("OK\r\r"));
        assert!(!is_ok("?"));
        assert!(!is_ok(""));
        assert!(!is_ok("NO DATA"));
    }

    #[test]
    fn multi_frame_with_mixed_line_endings() {
        // Real 0168 reply from the car's raw log: this clone ends the byte
        // count line with \n and the frames with \r.
        let m = messages("009\n0: 41 68 07 47 4A 5B \r1: FF FF FF \r\r");
        assert_eq!(m, vec![vec![0x41, 0x68, 0x07, 0x47, 0x4A, 0x5B, 0xFF, 0xFF, 0xFF]]);
    }
}
