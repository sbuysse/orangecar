//! Diagnostic trouble codes: wire decoding (modes 03 / 07 / 0A) and
//! descriptions (SAE J2012).

use super::dtc_table::DTC_TABLE;
use super::elm;

/// Which list a code came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DtcKind {
    /// Mode 03: confirmed, MIL-relevant.
    Stored,
    /// Mode 07: detected during the current/last drive cycle, not yet confirmed.
    Pending,
    /// Mode 0A: permanent, only the ECU itself can erase it (mode 04 cannot).
    Permanent,
}

impl DtcKind {
    pub fn request(self) -> &'static str {
        match self {
            DtcKind::Stored => "03",
            DtcKind::Pending => "07",
            DtcKind::Permanent => "0A",
        }
    }
    /// Positive-response mode byte (request + 0x40).
    pub fn echo(self) -> u8 {
        match self {
            DtcKind::Stored => 0x43,
            DtcKind::Pending => 0x47,
            DtcKind::Permanent => 0x4A,
        }
    }
}

/// One 2-byte DTC → "P0420". Bits 7-6 of the first byte select the system
/// (00 P, 01 C, 10 B, 11 U), bits 5-4 the first digit (0-3), then three nibbles.
pub fn decode_dtc(a: u8, b: u8) -> String {
    let letter = match a >> 6 {
        0b00 => 'P',
        0b01 => 'C',
        0b10 => 'B',
        _ => 'U',
    };
    format!("{letter}{}{:X}{:X}{:X}", (a >> 4) & 0x03, a & 0x0F, b >> 4, b & 0x0F)
}

/// Decode a mode 03/07/0A reply into codes, de-duplicated, in reply order.
///
/// On ISO 15765-4 (CAN) every emission ECU answers with `<echo> <count>
/// <pairs...>`; on the older K-line/J1850 protocols there is no count byte and
/// each line carries three pairs padded with `00 00`. `can = None` (protocol
/// not known yet) falls back to the length: after the echo byte a CAN message
/// has 1 + 2n bytes (odd), a legacy one 6 (even).
///
/// The previous decoder concatenated all lines and paired bytes from the first
/// echo without the count byte. On a CAN car with two ECUs that both have no
/// fault (`43 00\r43 00`) it produced the phantom code P0043 (`00 43`), and a
/// real code was shifted by one byte (`43 01 24 63` → "P0124").
pub fn parse_dtcs(resp: &str, kind: DtcKind, can: Option<bool>) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for msg in elm::messages(resp) {
        if msg.first() != Some(&kind.echo()) {
            continue;
        }
        let body = &msg[1..];
        let is_can = can.unwrap_or(body.len() % 2 == 1);
        let pairs: &[u8] = if is_can {
            let Some((&count, rest)) = body.split_first() else { continue };
            let n = (count as usize).min(rest.len() / 2);
            &rest[..n * 2]
        } else {
            body
        };
        for p in pairs.chunks_exact(2) {
            if p[0] == 0 && p[1] == 0 {
                continue; // padding / no code
            }
            let code = decode_dtc(p[0], p[1]);
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes
}

/// Whether a reply is a positive answer to the DTC request (even with zero
/// codes). "NO DATA" to mode 07/0A just means the ECU does not support it.
pub fn is_positive(resp: &str, kind: DtcKind) -> bool {
    elm::messages(resp).iter().any(|m| m.first() == Some(&kind.echo()))
}

/// Description of a code and whether it is a known title (vs. a structural
/// description built from the J2012 numbering).
pub fn describe(code: &str) -> (String, bool) {
    if let Ok(i) = DTC_TABLE.binary_search_by(|(c, _)| (*c).cmp(code)) {
        return (DTC_TABLE[i].1.to_string(), true);
    }
    (structural_description(code), false)
}

/// What the J2012 numbering alone tells us about a code that is not in the
/// table: who defines it and which subsystem it belongs to.
fn structural_description(code: &str) -> String {
    let mut ch = code.chars();
    let (Some(sys), Some(d1), Some(d2)) = (ch.next(), ch.next(), ch.next()) else {
        return "Not a valid fault code".to_string();
    };
    let system = match sys {
        'P' => "powertrain",
        'C' => "chassis",
        'B' => "body",
        'U' => "network",
        _ => return "Not a valid fault code".to_string(),
    };
    let manufacturer = match (sys, d1) {
        ('P', '1') => true,
        ('P', '3') => matches!(d2, '0'..='3'),
        (_, '1') | (_, '2') => sys != 'P',
        _ => false,
    };
    let scope = if manufacturer {
        "Manufacturer-specific (Jaguar Land Rover)"
    } else {
        "Generic SAE"
    };
    let area = match (sys, d2) {
        ('P', '0') => Some("fuel and air metering, auxiliary emission controls"),
        ('P', '1') | ('P', '2') => Some("fuel and air metering"),
        ('P', '3') => Some("ignition system or misfire"),
        ('P', '4') => Some("auxiliary emission controls"),
        ('P', '5') => Some("vehicle speed, idle control and auxiliary inputs"),
        ('P', '6') => Some("computer and auxiliary outputs"),
        ('P', '7' | '8' | '9') => Some("transmission"),
        ('P', 'A' | 'B' | 'C') => Some("hybrid propulsion"),
        ('U', '0') => Some("network electrical"),
        ('U', '1' | '2') => Some("lost communication with a module"),
        ('U', '3') => Some("software incompatibility"),
        ('U', '4') => Some("invalid data received"),
        _ => None,
    };
    match area {
        Some(a) => format!("{scope} {system} code — {a}; no description in the table"),
        None => format!("{scope} {system} code; no description in the table"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_all_four_systems() {
        assert_eq!(decode_dtc(0x04, 0x20), "P0420");
        assert_eq!(decode_dtc(0x01, 0x33), "P0133");
        assert_eq!(decode_dtc(0x24, 0x63), "P2463");
        assert_eq!(decode_dtc(0x10, 0x00), "P1000");
        assert_eq!(decode_dtc(0x3F, 0xFF), "P3FFF");
        assert_eq!(decode_dtc(0x41, 0x23), "C0123");
        assert_eq!(decode_dtc(0x9A, 0x01), "B1A01");
        assert_eq!(decode_dtc(0xC1, 0x00), "U0100");
        assert_eq!(decode_dtc(0xE0, 0x01), "U2001");
    }

    #[test]
    fn can_two_ecus_without_faults_is_empty() {
        // The reply that the old decoder turned into the phantom "P0043".
        assert!(parse_dtcs("43 00\r43 00", DtcKind::Stored, None).is_empty());
        assert!(parse_dtcs("43 00\r43 00", DtcKind::Stored, Some(true)).is_empty());
        assert!(is_positive("43 00\r43 00", DtcKind::Stored));
    }

    #[test]
    fn can_count_byte_skipped() {
        assert_eq!(parse_dtcs("43 01 24 63", DtcKind::Stored, Some(true)), vec!["P2463"]);
        assert_eq!(parse_dtcs("43 01 24 63", DtcKind::Stored, None), vec!["P2463"]);
        assert_eq!(parse_dtcs("43 00\r43 01 04 20", DtcKind::Stored, None), vec!["P0420"]);
    }

    #[test]
    fn can_multi_frame_many_codes() {
        let resp = "00A\r0: 43 04 01 33 04 20\r1: C1 00 24 63 00 00 00";
        assert_eq!(
            parse_dtcs(resp, DtcKind::Stored, Some(true)),
            vec!["P0133", "P0420", "U0100", "P2463"]
        );
    }

    #[test]
    fn legacy_k_line_three_pairs_per_line() {
        let resp = "43 01 33 04 20 00 00\r43 00 00 00 00 00 00";
        assert_eq!(parse_dtcs(resp, DtcKind::Stored, Some(false)), vec!["P0133", "P0420"]);
        assert_eq!(parse_dtcs(resp, DtcKind::Stored, None), vec!["P0133", "P0420"]);
    }

    #[test]
    fn pending_and_permanent_echoes() {
        assert_eq!(parse_dtcs("47 01 01 71", DtcKind::Pending, Some(true)), vec!["P0171"]);
        assert_eq!(parse_dtcs("4A 01 01 71", DtcKind::Permanent, Some(true)), vec!["P0171"]);
        // A mode-03 echo in a mode-07 reply is not a pending code.
        assert!(parse_dtcs("43 01 01 71", DtcKind::Pending, Some(true)).is_empty());
        assert!(!is_positive("NO DATA", DtcKind::Permanent));
    }

    #[test]
    fn status_strings_produce_no_codes() {
        for r in ["NO DATA", "SEARCHING...\rUNABLE TO CONNECT", "?", "CAN ERROR", ""] {
            assert!(parse_dtcs(r, DtcKind::Stored, None).is_empty(), "{r}");
        }
    }

    #[test]
    fn duplicate_codes_from_two_ecus_listed_once() {
        assert_eq!(parse_dtcs("43 01 04 20\r43 01 04 20", DtcKind::Stored, Some(true)), vec!["P0420"]);
    }

    #[test]
    fn table_sorted_unique_and_well_formed() {
        assert!(DTC_TABLE.len() > 2000);
        for w in DTC_TABLE.windows(2) {
            assert!(w[0].0 < w[1].0, "{} !< {}", w[0].0, w[1].0);
        }
        for (c, d) in DTC_TABLE {
            assert_eq!(c.len(), 5, "{c}");
            assert!(!d.is_empty(), "{c}");
        }
    }

    #[test]
    fn describe_known_and_fallbacks() {
        let (d, known) = describe("P0420");
        assert!(known);
        assert!(d.contains("Catalyst"), "{d}");
        let (d, known) = describe("P0043");
        assert!(known);
        assert!(d.contains("Bank 1 Sensor 3"), "{d}");
        let (d, known) = describe("P1D9C");
        assert!(!known);
        assert!(d.starts_with("Manufacturer-specific"), "{d}");
        let (d, known) = describe("U3001");
        assert!(!known);
        assert!(d.contains("network"), "{d}");
        let (d, _) = describe("P3011");
        assert!(d.starts_with("Manufacturer-specific"), "{d}");
        let (d, _) = describe("P34FF");
        assert!(d.starts_with("Generic SAE"), "{d}");
        let (d, _) = describe("B1234");
        assert!(d.starts_with("Manufacturer-specific"), "{d}");
    }
}
