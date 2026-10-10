//! Wire encoder for the `_carplay-ctrl._tcp` mDNS announcement.
//!
//! A real iPhone on a CarPlay USB link answers the car's `_carplay-ctrl` browse
//! with ONE unsolicited authoritative response (ID 0, flags 0x8400, no
//! questions) that carries everything the car needs to dial it:
//!
//! | section    | record                                   | cache-flush |
//! |------------|------------------------------------------|-------------|
//! | answer     | SRV  `<inst>._carplay-ctrl._tcp` → host  | yes         |
//! | answer     | AAAA host → link-local                   | yes         |
//! | answer     | PTR  `<reversed>.ip6.arpa` → host        | yes         |
//! | answer     | TXT  instance: `id=<mac>`, `srcvers=<v>` | yes         |
//! | answer     | PTR  `_carplay-ctrl._tcp` → instance     | no          |
//! | answer     | PTR  `_services._dns-sd._udp` → service  | no          |
//! | answer     | TXT  `<inst>._device-info._tcp`: model   | no          |
//! | additional | NSEC instance {TXT, SRV}                 | yes         |
//! | additional | NSEC host {AAAA}                         | yes         |
//! | additional | NSEC reverse name {PTR}                  | yes         |
//!
//! All TTLs are 4500 s. Names are compressed (RFC 1035 §4.1.4) against every
//! suffix already present in the packet, the reverse name uses upper-case hex
//! nibbles, and NSEC bitmaps use window 0 trimmed to the last non-zero byte.
//!
//! **Why the NSEC records matter.** avahi's announcement for the same service
//! carries no NSEC and no reverse PTR (and a ~120 s TTL). The host NSEC listing
//! only AAAA is the authoritative statement "this host has no A record"; without
//! it the car's mDNSResponder waits for its negative-cache timeout on the A
//! query and never dials, and the USB session is dropped at ~7 s.
//!
//! The layout follows the iPhone announcement reproduced by the 399-byte test
//! vector of catplay-labs/catplay (used as a reference for the wire format);
//! `announce_matches_reference_vector` pins our encoder to it byte for byte. The
//! encoder itself is our own.
//!
//! One deliberate extension: when the NCM link also has an IPv4 address we add
//! an A record after the AAAA and list A in the host NSEC (see [`CtrlAdvert::ipv4`]).

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// DNS resource-record types used in the announcement (IANA values).
mod rrtype {
    pub const A: u16 = 1;
    pub const PTR: u16 = 12;
    pub const TXT: u16 = 16;
    pub const AAAA: u16 = 28;
    pub const SRV: u16 = 33;
    pub const NSEC: u16 = 47;
}

/// Class IN with the mDNS cache-flush bit (RFC 6762 §10.2) set or clear.
fn class_field(unique: bool) -> u16 {
    const IN: u16 = 1;
    const FLUSH: u16 = 1 << 15;
    if unique { IN | FLUSH } else { IN }
}

/// Parameters of the `_carplay-ctrl` advertisement.
#[derive(Debug, Clone)]
pub struct CtrlAdvert<'a> {
    /// Service instance label, e.g. "iPhone".
    pub instance: &'a str,
    /// Host label the SRV points at (without `.local`).
    pub host: &'a str,
    /// TCP control port (7100).
    pub port: u16,
    /// NCM IPv6 link-local, without scope.
    pub link_local: Ipv6Addr,
    /// NCM IPv4, when the link has one.
    ///
    /// Measured on the Jaguar F-PACE: the car browses `_carplay-ctrl._tcp` and we
    /// answer within ~50 ms, but it never dials port 7100 while the only address
    /// published for the SRV target is an IPv6 link-local (Apple's CarPlay spec,
    /// "CarPlay Control" step 3, says accessories should not use link-local
    /// addresses). The car runs IPv4 on the NCM link (it is 192.168.0.1, we are
    /// 192.168.0.2), so publishing an A record gives it an address it dials.
    ///
    /// `None` produces the AAAA-only iPhone packet unchanged.
    pub ipv4: Option<Ipv4Addr>,
    /// AirPlay source version advertised in TXT `srcvers=`.
    pub srcvers: &'a str,
    /// Colon-separated MAC advertised in TXT `id=`.
    pub device_id: &'a str,
    /// Hardware model advertised in the `_device-info` TXT.
    pub model: &'a str,
    /// TTL of every record (an iPhone uses 4500).
    pub ttl: u32,
}

impl CtrlAdvert<'_> {
    /// Serialise the announcement as one mDNS response packet.
    pub fn packet(&self) -> Vec<u8> {
        let service = "_carplay-ctrl._tcp.local.".to_string();
        let instance = format!("{}.{service}", self.instance);
        let host = format!("{}.local.", self.host);
        let reverse = reverse_name(&self.link_local);
        let device_info = format!("{}._device-info._tcp.local.", self.instance);

        let mut answers = vec![
            Record::unique(&instance, Rdata::Srv { port: self.port, target: &host }),
            Record::unique(&host, Rdata::Aaaa(self.link_local.octets())),
        ];
        if let Some(v4) = self.ipv4 {
            answers.push(Record::unique(&host, Rdata::A(v4.octets())));
        }
        answers.extend([
            Record::unique(&reverse, Rdata::Ptr(&host)),
            Record::unique(
                &instance,
                Rdata::Txt(vec![format!("id={}", self.device_id), format!("srcvers={}", self.srcvers)]),
            ),
            Record::shared(&service, Rdata::Ptr(&instance)),
            Record::shared("_services._dns-sd._udp.local.", Rdata::Ptr(&service)),
            Record::shared(&device_info, Rdata::Txt(vec![format!("model={}", self.model)])),
        ]);

        // With an A record published, the host NSEC must not deny it.
        let host_types: &[u16] =
            if self.ipv4.is_some() { &[rrtype::A, rrtype::AAAA] } else { &[rrtype::AAAA] };
        let additionals = [
            Record::unique(&instance, Rdata::Nsec { types: &[rrtype::TXT, rrtype::SRV] }),
            Record::unique(&host, Rdata::Nsec { types: host_types }),
            Record::unique(&reverse, Rdata::Nsec { types: &[rrtype::PTR] }),
        ];

        let mut out = PacketWriter::default();
        out.header(answers.len(), additionals.len());
        for rec in answers.iter().chain(additionals.iter()) {
            out.record(rec, self.ttl);
        }
        out.finish()
    }
}

/// `fe80::1` → `1.0.0.0.…8.E.F.ip6.arpa.`: the 32 nibbles of the address in
/// reverse order, one label each, upper-case hex (as an iPhone writes them).
fn reverse_name(addr: &Ipv6Addr) -> String {
    let mut name: String = addr
        .octets()
        .iter()
        .rev()
        .flat_map(|byte| [byte & 0x0f, byte >> 4])
        .map(|nibble| format!("{nibble:X}."))
        .collect();
    name.push_str("ip6.arpa.");
    name
}

/// Record payloads. Names are absolute (trailing dot).
enum Rdata<'a> {
    A([u8; 4]),
    Aaaa([u8; 16]),
    Ptr(&'a str),
    Srv { port: u16, target: &'a str },
    Txt(Vec<String>),
    /// NSEC whose "next domain" is the owner itself (the mDNS convention).
    Nsec { types: &'a [u16] },
}

impl Rdata<'_> {
    fn rrtype(&self) -> u16 {
        match self {
            Rdata::A(_) => rrtype::A,
            Rdata::Aaaa(_) => rrtype::AAAA,
            Rdata::Ptr(_) => rrtype::PTR,
            Rdata::Srv { .. } => rrtype::SRV,
            Rdata::Txt(_) => rrtype::TXT,
            Rdata::Nsec { .. } => rrtype::NSEC,
        }
    }
}

struct Record<'a> {
    owner: &'a str,
    /// mDNS "unique" record: sent with the cache-flush bit.
    unique: bool,
    data: Rdata<'a>,
}

impl<'a> Record<'a> {
    fn unique(owner: &'a str, data: Rdata<'a>) -> Self {
        Self { owner, unique: true, data }
    }
    fn shared(owner: &'a str, data: Rdata<'a>) -> Self {
        Self { owner, unique: false, data }
    }
}

/// Builds the packet in a single buffer so compression pointers are plain
/// buffer offsets (the header is written first).
#[derive(Default)]
struct PacketWriter {
    buf: Vec<u8>,
    /// Absolute offset of every name suffix written so far, keyed by its
    /// dotted text (exact, case-sensitive match).
    suffix_at: HashMap<String, u16>,
}

impl PacketWriter {
    fn put16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn header(&mut self, answers: usize, additionals: usize) {
        let fields: [u16; 6] = [
            0,      // ID: always 0 in mDNS
            0x8400, // QR=1 (response), AA=1
            0,      // no questions: unsolicited
            answers as u16,
            0, // authority: unused in mDNS responses
            additionals as u16,
        ];
        for f in fields {
            self.put16(f);
        }
    }

    /// Write `name`, replacing its longest already-written suffix by a pointer.
    fn name(&mut self, name: &str) {
        let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
        let mut i = 0;
        while i < labels.len() {
            let suffix = labels[i..].join(".");
            if let Some(&at) = self.suffix_at.get(&suffix) {
                self.put16(0xc000 | at);
                return;
            }
            let here = self.buf.len();
            if here < 0x4000 {
                self.suffix_at.entry(suffix).or_insert(here as u16);
            }
            self.buf.push(labels[i].len() as u8);
            self.buf.extend_from_slice(labels[i].as_bytes());
            i += 1;
        }
        self.buf.push(0);
    }

    fn record(&mut self, rec: &Record, ttl: u32) {
        self.name(rec.owner);
        self.put16(rec.data.rrtype());
        self.put16(class_field(rec.unique));
        self.buf.extend_from_slice(&ttl.to_be_bytes());

        // RDLENGTH is only known once the (possibly compressed) RDATA is out.
        let len_at = self.buf.len();
        self.put16(0);
        match &rec.data {
            Rdata::A(ip) => self.buf.extend_from_slice(ip),
            Rdata::Aaaa(ip) => self.buf.extend_from_slice(ip),
            Rdata::Ptr(target) => self.name(target),
            Rdata::Srv { port, target } => {
                self.put16(0); // priority
                self.put16(0); // weight
                self.put16(*port);
                self.name(target);
            }
            Rdata::Txt(strings) => {
                for s in strings {
                    self.buf.push(s.len() as u8);
                    self.buf.extend_from_slice(s.as_bytes());
                }
            }
            Rdata::Nsec { types } => {
                self.name(rec.owner);
                let bitmap = type_bitmap(types);
                self.buf.push(0); // window block 0 (types 0..=255)
                self.buf.push(bitmap.len() as u8);
                self.buf.extend_from_slice(&bitmap);
            }
        }
        let rdlength = (self.buf.len() - len_at - 2) as u16;
        self.buf[len_at..len_at + 2].copy_from_slice(&rdlength.to_be_bytes());
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// NSEC window-0 type bitmap (RFC 4034 §4.1.2): bit 0 of byte 0 is the MSB,
/// type `t` lives in byte `t / 8`, and the map stops at the highest type.
fn type_bitmap(types: &[u16]) -> Vec<u8> {
    let Some(&highest) = types.iter().max() else { return Vec::new() };
    let mut map = vec![0u8; highest as usize / 8 + 1];
    for &t in types {
        map[t as usize / 8] |= 0x80 >> (t % 8);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Byte-for-byte equality with the iPhone-style `_carplay-ctrl` announcement
    /// of the catplay-labs/catplay test vector (399 bytes), with the MAC in the
    /// TXT record replaced by a locally administered one.
    #[test]
    fn announce_matches_reference_vector() {
        let payload = CtrlAdvert {
            instance: "iPhone SIM",
            host: "iPhone-SIM",
            port: 50013,
            link_local: "fe80::7c20:b2f1:fecc:d351".parse::<Ipv6Addr>().unwrap(),
            srcvers: "925.5.1",
            model: "D84AP",
            device_id: "02:00:00:00:00:01",
            ttl: 4500,
            ipv4: None,
        }
        .packet();
        assert_eq!(payload.len(), 399);
        assert_eq!(
            payload,
            vec![
                0, 0, 132, 0, 0, 0, 0, 7, 0, 0, 0, 3, 10, 105, 80, 104, 111, 110, 101, 32, 83, 73, 77, 13, 95, 99, 97, 114, 112, 108, 97, 121,
                45, 99, 116, 114, 108, 4, 95, 116, 99, 112, 5, 108, 111, 99, 97, 108, 0, 0, 33, 128, 1, 0, 0, 17, 148, 0, 19, 0, 0, 0, 0, 195,
                93, 10, 105, 80, 104, 111, 110, 101, 45, 83, 73, 77, 192, 42, 192, 65, 0, 28, 128, 1, 0, 0, 17, 148, 0, 16, 254, 128, 0, 0, 0,
                0, 0, 0, 124, 32, 178, 241, 254, 204, 211, 81, 1, 49, 1, 53, 1, 51, 1, 68, 1, 67, 1, 67, 1, 69, 1, 70, 1, 49, 1, 70, 1, 50, 1,
                66, 1, 48, 1, 50, 1, 67, 1, 55, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1, 48, 1,
                56, 1, 69, 1, 70, 3, 105, 112, 54, 4, 97, 114, 112, 97, 0, 0, 12, 128, 1, 0, 0, 17, 148, 0, 2, 192, 65, 192, 12, 0, 16, 128, 1,
                0, 0, 17, 148, 0, 37, 20, 105, 100, 61, 48, 50, 58, 48, 48, 58, 48, 48, 58, 48, 48, 58, 48, 48, 58, 48, 49, 15, 115, 114, 99,
                118, 101, 114, 115, 61, 57, 50, 53, 46, 53, 46, 49, 192, 23, 0, 12, 0, 1, 0, 0, 17, 148, 0, 2, 192, 12, 9, 95, 115, 101, 114,
                118, 105, 99, 101, 115, 7, 95, 100, 110, 115, 45, 115, 100, 4, 95, 117, 100, 112, 192, 42, 0, 12, 0, 1, 0, 0, 17, 148, 0, 2,
                192, 23, 10, 105, 80, 104, 111, 110, 101, 32, 83, 73, 77, 12, 95, 100, 101, 118, 105, 99, 101, 45, 105, 110, 102, 111, 192, 37,
                0, 16, 0, 1, 0, 0, 17, 148, 0, 12, 11, 109, 111, 100, 101, 108, 61, 68, 56, 52, 65, 80, 192, 12, 0, 47, 128, 1, 0, 0, 17, 148,
                0, 9, 192, 12, 0, 5, 0, 0, 128, 0, 64, 192, 65, 0, 47, 128, 1, 0, 0, 17, 148, 0, 8, 192, 65, 0, 4, 0, 0, 0, 8, 192, 106, 0, 47,
                128, 1, 0, 0, 17, 148, 0, 6, 192, 106, 0, 2, 0, 8
            ]
        );
    }

    /// Packets produced by the previous encoder for the parameter sets we use
    /// on the car (with and without the NCM IPv4), pinned before the encoder
    /// was rewritten so the bytes on the wire cannot drift.
    #[test]
    fn car_parameter_sets_are_byte_stable() {
        let with_v4 = CtrlAdvert {
            instance: "iPhone",
            host: "iPhone",
            port: 7100,
            link_local: "fe80::211:22ff:fe33:4455".parse().unwrap(),
            ipv4: Some(Ipv4Addr::new(192, 168, 0, 2)),
            srcvers: "280.33.12.1",
            device_id: "02:11:22:33:44:55",
            model: "iPhone14,2",
            ttl: 4500,
        }
        .packet();
        assert_eq!(with_v4, unhex("000084000000000800000003066950686f6e650d5f636172706c61792d6374726c045f746370056c6f63616c000021800100001194000f000000001bbc066950686f6e65c026c03d001c8001000011940010fe80000000000000021122fffe334455c03d00018001000011940004c0a800020135013501340134013301330145014601460146013201320131013101320130013001300130013001300130013001300130013001300130013001380145014603697036046172706100000c8001000011940002c03dc00c001080010000119400291469643d30323a31313a32323a33333a34343a353513737263766572733d3238302e33332e31322e31c013000c0001000011940002c00c095f7365727669636573075f646e732d7364045f756470c026000c0001000011940002c013066950686f6e650c5f6465766963652d696e666fc02100100001000011940011106d6f64656c3d6950686f6e6531342c32c00c002f8001000011940009c00c00050000800040c03d002f8001000011940008c03d000440000008c072002f8001000011940006c07200020008"));

        let v6_only = CtrlAdvert {
            instance: "RasCar",
            host: "rascar-ctrl",
            port: 7100,
            link_local: "fe80::dead:beef:1234:5678".parse().unwrap(),
            ipv4: None,
            srcvers: "925.5.1",
            device_id: "a1:b2:c3:d4:e5:f6",
            model: "D84AP",
            ttl: 120,
        }
        .packet();
        assert_eq!(v6_only, unhex("000084000000000700000003065261734361720d5f636172706c61792d6374726c045f746370056c6f63616c0000218001000000780014000000001bbc0b7261736361722d6374726cc026c03d001c8001000000780010fe80000000000000deadbeef123456780138013701360135013401330132013101460145014501420144014101450144013001300130013001300130013001300130013001300130013001380145014603697036046172706100000c8001000000780002c03dc00c001080010000007800251469643d61313a62323a63333a64343a65353a66360f737263766572733d3932352e352e31c013000c0001000000780002c00c095f7365727669636573075f646e732d7364045f756470c026000c0001000000780002c013065261734361720c5f6465766963652d696e666fc0210010000100000078000c0b6d6f64656c3d4438344150c00c002f8001000000780009c00c00050000800040c03d002f8001000000780008c03d000400000008c067002f8001000000780006c06700020008"));
    }

    #[test]
    fn reverse_name_is_nibble_reversed_upper_case() {
        let n = reverse_name(&"fe80::ab".parse().unwrap());
        assert!(n.starts_with("B.A.0.0."), "{n}");
        assert!(n.ends_with("0.8.E.F.ip6.arpa."), "{n}");
        assert_eq!(n.matches('.').count(), 34);
    }

    #[test]
    fn type_bitmap_matches_rfc4034_example_bits() {
        assert_eq!(type_bitmap(&[rrtype::AAAA]), vec![0, 0, 0, 0x08]);
        assert_eq!(type_bitmap(&[rrtype::PTR]), vec![0, 0x08]);
        assert_eq!(type_bitmap(&[rrtype::TXT, rrtype::SRV]), vec![0, 0, 0x80, 0, 0x40]);
        assert_eq!(type_bitmap(&[rrtype::A, rrtype::AAAA]), vec![0x40, 0, 0, 0x08]);
        assert!(type_bitmap(&[]).is_empty());
    }

    /// Our real parameters must still produce a well-formed packet with the NSEC
    /// records present (the negative proofs avahi omitted).
    #[test]
    fn rascar_params_carry_nsec_and_aaaa_only() {
        let p = CtrlAdvert {
            instance: "RasCar",
            host: "rascar-ctrl",
            port: 7100,
            link_local: "fe80::dead:beef:1234:5678".parse::<Ipv6Addr>().unwrap(),
            srcvers: "925.5.1",
            model: "D84AP",
            device_id: "a1:b2:c3:d4:e5:f6",
            ttl: 4500,
            ipv4: None,
        }
        .packet();
        // 7 answers + 3 NSEC additionals, QDCOUNT 0 (unsolicited), flags 0x8400.
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 0x8400, "flags");
        assert_eq!(u16::from_be_bytes([p[4], p[5]]), 0, "QDCOUNT");
        assert_eq!(u16::from_be_bytes([p[6], p[7]]), 7, "ANCOUNT");
        assert_eq!(u16::from_be_bytes([p[10], p[11]]), 3, "ARCOUNT (NSEC)");
    }
}

#[cfg(test)]
mod a_record_tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn params(v4: Option<Ipv4Addr>) -> CtrlAdvert<'static> {
        CtrlAdvert {
            instance: "iPhone",
            host: "iPhone",
            port: 7100,
            link_local: "fe80::211:22ff:fe33:4455".parse().unwrap(),
            srcvers: "566.25.21",
            device_id: "d0:81:7a:c0:ff:ee",
            model: "D84AP",
            ttl: 4500,
            ipv4: v4,
        }
    }

    fn ancount(p: &[u8]) -> u16 {
        u16::from_be_bytes([p[6], p[7]])
    }

    /// No IPv4 known -> the packet must be EXACTLY what we shipped before, so the
    /// change cannot regress a link where only IPv6 exists.
    #[test]
    fn without_ipv4_the_packet_is_unchanged() {
        let p = params(None).packet();
        assert_eq!(ancount(&p), 7, "the original packet carries 7 answers");
        // Host NSEC must still assert AAAA-only (type bit 28, window 0).
        assert!(!contains_a_record(&p), "must not emit an A record when none is known");
    }

    /// With an IPv4 the car can actually dial, the A record must be present AND
    /// the host NSEC must stop claiming "no A exists" — otherwise we would publish
    /// the address and simultaneously prove it absent.
    #[test]
    fn with_ipv4_the_a_record_is_published_and_not_denied() {
        let v4 = Ipv4Addr::new(192, 168, 0, 2);
        let p = params(Some(v4)).packet();
        assert_eq!(ancount(&p), 8, "one more answer than the AAAA-only packet");
        assert!(contains_a_record(&p), "A record missing — the car will not dial us");
        assert!(
            find(&p, &v4.octets()).is_some(),
            "the NCM IPv4 must appear in the packet"
        );
        // NSEC type bitmap for the host must now list type 1 (A) as well as 28.
        // Window 0, bitmap byte 0 bit for type 1 is 0x40.
        assert!(
            nsec_host_bitmap_has_a(&p),
            "host NSEC still says AAAA-only, which denies the A record we just sent"
        );
    }

    fn contains_a_record(p: &[u8]) -> bool {
        // TYPE_A(0x0001) + CLASS cache-flush IN(0x8001) + ttl + rdlength 4
        find(p, &[0x00, 0x01, 0x80, 0x01]).is_some()
    }

    fn nsec_host_bitmap_has_a(p: &[u8]) -> bool {
        // Find an NSEC rr (type 47) whose bitmap window-0 byte has both bit 1 (A)
        // and bit 28 (AAAA) set: byte0 & 0x40 != 0 and byte3 & 0x08 != 0.
        let mut i = 0usize;
        while let Some(pos) = find(&p[i..], &[0x00, 0x2f]) {
            let at = i + pos;
            // skip type(2) class(2) ttl(4) rdlength(2)
            let rd = at + 10;
            if rd + 4 < p.len() {
                // rdata = next-name (compressed ptr, 2 bytes) + window + len + bitmap
                let bm = rd + 2 + 2;
                if bm + 4 <= p.len() && p[bm] & 0x40 != 0 && p[bm + 3] & 0x08 != 0 {
                    return true;
                }
            }
            i = at + 2;
        }
        false
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
}
