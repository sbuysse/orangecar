//! The car's HID devices (`/info` → `hidDevices[]`) and decoding of the input
//! reports it sends us as event-channel `hidSendReport` commands.
//!
//! Every report carries the `uuid` of the device it comes from (Apple
//! `AirPlayReceiverSessionSendHIDReport`, AirPlayReceiverSession.c:5419-5422:
//! keys `type`, `uuid`, `hidReport`). The meaning of the bytes is defined by that
//! device's HID report descriptor, so nothing here hard-codes a byte value: a
//! button report on an *array* field is an INDEX into the descriptor's usage
//! list, and that list differs per car. The Jaguar F-PACE's "Consumer keypad"
//! lists `[0, Play, Pause, Scan Next, Scan Previous]`, so its byte `03` is Scan
//! Next; catplay's own media-button descriptor lists `[0, Play, Pause,
//! Play/Pause, Next, Previous, …]`, where `03` is Play/Pause.
//!
//! The Jaguar declares three devices (identical descriptors in all 28 plaintext
//! `/info` replies captured 2026-09-19..29; only the uuids change per session):
//!
//! | name             | application                   | report                    |
//! |------------------|-------------------------------|---------------------------|
//! | Touch Screen     | Digitizer / Touch Screen      | 5 B: tip, X u16, Y u16    |
//! | Telephony keypad | Telephony / Phone             | 1 B array: Hook, Flash, Drop |
//! | Consumer keypad  | Consumer / Consumer Control   | 1 B array: Play, Pause, Next, Prev |
//!
//! It declares no iAP2 HID component (IdentificationInformation carries no
//! param 18/23/29, and MessagesSentByAccessory has no 0x68xx), so this is the
//! only path its buttons take.

use std::collections::HashMap;

/// HID usage pages we name (HID Usage Tables 1.4 §3).
pub const PAGE_GENERIC_DESKTOP: u16 = 0x01;
pub const PAGE_KEYBOARD: u16 = 0x07;
pub const PAGE_TELEPHONY: u16 = 0x0B;
pub const PAGE_CONSUMER: u16 = 0x0C;
pub const PAGE_DIGITIZER: u16 = 0x0D;

/// A usage = page (high 16 bits) + id (low 16 bits), as in HID extended usages.
pub type Usage = u32;

pub const fn usage(page: u16, id: u16) -> Usage {
    ((page as u32) << 16) | id as u32
}

pub fn usage_page(u: Usage) -> u16 {
    (u >> 16) as u16
}

pub fn usage_id(u: Usage) -> u16 {
    (u & 0xFFFF) as u16
}

/// One Input main item of a report descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct InputField {
    pub report_id: u8,
    /// Bit offset in the report, after the report-ID byte if there is one.
    pub bit_offset: usize,
    pub size: u32,
    pub count: u32,
    /// Array field (Input bit 1 clear): each control holds an index into `usages`.
    pub array: bool,
    /// Constant bit (Input bit 0). See `decode` for why it is not padding here.
    pub constant: bool,
    pub usages: Vec<Usage>,
    pub logical_min: i32,
    pub logical_max: i32,
}

/// What a report descriptor says about the device's input reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HidLayout {
    /// Usage of the first Application collection (what kind of device this is).
    pub application: Option<Usage>,
    pub fields: Vec<InputField>,
    pub uses_report_ids: bool,
}

/// Parse the short items of a HID report descriptor (HID 1.11 §6.2.2) into the
/// input fields we need to decode reports. Unknown items are skipped; a
/// truncated descriptor yields the fields parsed so far.
pub fn parse_descriptor(desc: &[u8]) -> HidLayout {
    #[derive(Clone, Default)]
    struct Globals {
        page: u16,
        logical_min: i32,
        logical_max: i32,
        size: u32,
        count: u32,
        report_id: u8,
    }
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut local_usages: Vec<Usage> = Vec::new();
    let mut usage_min: Option<Usage> = None;
    let mut layout = HidLayout::default();
    let mut offsets: HashMap<u8, usize> = HashMap::new();

    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // Long item: [FE][bDataSize][bLongItemTag][data…] — none of ours.
            let n = desc.get(i + 1).copied().unwrap_or(0) as usize;
            i += 3 + n;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            n => n as usize,
        };
        let Some(data) = desc.get(i + 1..i + 1 + size) else { break };
        i += 1 + size;
        let unsigned = data.iter().rev().fold(0u32, |acc, b| (acc << 8) | *b as u32);
        let signed = match size {
            1 => data[0] as i8 as i32,
            2 => i16::from_le_bytes([data[0], data[1]]) as i32,
            4 => unsigned as i32,
            _ => 0,
        };
        // A usage given in 1-2 bytes takes the current page; 4 bytes carry it.
        let page = g.page;
        let full_usage = move |u: u32| if size == 4 { u } else { usage(page, u as u16) };
        let tag = prefix >> 4;
        match (prefix >> 2) & 0x03 {
            // Main items.
            0 => {
                match tag {
                    0x8 => {
                        // Input
                        let bits = unsigned;
                        if let Some(min) = usage_min.take() {
                            local_usages.push(min);
                        }
                        let offset = offsets.entry(g.report_id).or_insert(0);
                        layout.fields.push(InputField {
                            report_id: g.report_id,
                            bit_offset: *offset,
                            size: g.size,
                            count: g.count,
                            array: bits & 0x02 == 0,
                            constant: bits & 0x01 != 0,
                            usages: std::mem::take(&mut local_usages),
                            logical_min: g.logical_min,
                            logical_max: g.logical_max,
                        });
                        *offset += (g.size * g.count) as usize;
                    }
                    0xA => {
                        // Collection: 0x01 = Application.
                        if unsigned == 0x01 && layout.application.is_none() {
                            layout.application = local_usages.first().copied();
                        }
                    }
                    _ => {} // Output, Feature, End Collection
                }
                local_usages.clear();
                usage_min = None;
            }
            // Global items.
            1 => match tag {
                0x0 => g.page = unsigned as u16,
                0x1 => g.logical_min = signed,
                0x2 => {
                    // A max that sign-extends below the min was meant unsigned
                    // (`26 FF 7F` is 32767 either way; `25 FF` with min 0 is 255).
                    g.logical_max = if signed < g.logical_min { unsigned as i32 } else { signed };
                }
                0x7 => g.size = unsigned,
                0x8 => {
                    g.report_id = unsigned as u8;
                    layout.uses_report_ids = true;
                }
                0x9 => g.count = unsigned,
                0xA => stack.push(g.clone()),
                0xB => {
                    if let Some(p) = stack.pop() {
                        g = p;
                    }
                }
                _ => {} // physical range, unit
            },
            // Local items.
            2 => match tag {
                0x0 => local_usages.push(full_usage(unsigned)),
                0x1 => usage_min = Some(full_usage(unsigned)),
                0x2 => {
                    if let Some(min) = usage_min.take() {
                        let max = full_usage(unsigned);
                        // Bounded: a corrupt range must not allocate gigabytes.
                        if max >= min && max - min < 1024 {
                            local_usages.extend(min..=max);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    layout
}

/// A device from `/info` → `hidDevices[]`.
#[derive(Debug, Clone, PartialEq)]
pub struct HidDevice {
    pub uuid: String,
    pub name: String,
    pub layout: HidLayout,
}

impl HidDevice {
    /// A digitizer: its reports are touches, not keys.
    pub fn is_touch(&self) -> bool {
        self.layout.application.map(usage_page) == Some(PAGE_DIGITIZER)
    }
}

/// Read `size` bits at `bit_offset`, LSB first (HID report bit order).
fn read_bits(report: &[u8], bit_offset: usize, size: u32) -> Option<u32> {
    if size == 0 || size > 32 || bit_offset + size as usize > report.len() * 8 {
        return None;
    }
    let mut v: u64 = 0;
    for k in 0..size as usize {
        let bit = bit_offset + k;
        if report[bit / 8] >> (bit % 8) & 1 == 1 {
            v |= 1 << k;
        }
    }
    Some(v as u32)
}

/// The keys held down in one report: every non-null usage selected by an array
/// field, plus every 1-bit variable field that is set. `None` when the report
/// is too short for the descriptor (or carries a report ID we do not know).
pub fn pressed_usages(layout: &HidLayout, report: &[u8]) -> Option<Vec<Usage>> {
    let (id, body) = if layout.uses_report_ids {
        let (&id, rest) = report.split_first()?;
        (id, rest)
    } else {
        (0, report)
    };
    let fields: Vec<&InputField> = layout.fields.iter().filter(|f| f.report_id == id).collect();
    if fields.is_empty() {
        return None;
    }
    let mut pressed = Vec::new();
    for f in fields {
        // The Jaguar flags its key fields Constant (`81 01`) although they carry
        // usages and change with every press (real report `03` then `00`). A
        // field with no usages is padding; one with usages is decoded whatever
        // its constant bit says.
        if f.usages.is_empty() {
            continue;
        }
        for c in 0..f.count as usize {
            let raw = read_bits(body, f.bit_offset + c * f.size as usize, f.size)?;
            if f.array {
                let v = raw as i32;
                if v < f.logical_min || v > f.logical_max {
                    continue; // out of range = no key in this slot
                }
                if let Some(&u) = f.usages.get((v - f.logical_min) as usize) {
                    if usage_id(u) != 0 {
                        pressed.push(u);
                    }
                }
            } else if f.size == 1 && raw != 0 {
                let u = f.usages[c.min(f.usages.len() - 1)];
                pressed.push(u);
            }
        }
    }
    Some(pressed)
}

/// What a car key asks the dashboard to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarKey {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    /// Telephony Hook Switch (0x0B:0x20): a toggle, so its meaning depends on
    /// the line — answer while ringing, hang up during a call or a meeting.
    /// rascar-app decides (it knows the call state); we only relay it.
    Hook,
    /// Telephony Drop (0x0B:0x26): always "end": decline a ringing call, hang
    /// up an active one or the meeting.
    Drop,
}

impl CarKey {
    /// The keys we act on. Consumer usages from HID Usage Tables 1.4 §15.7
    /// (Transport Controls); Telephony Hook Switch and Drop (§14), relayed
    /// separately because Hook is a toggle (answer OR hang up) and Drop only
    /// ends: merging them, as we did when the only call was a meeting, would
    /// make the wheel unable to answer. Flash (0x21) and everything else is
    /// logged, not acted on.
    pub fn from_usage(u: Usage) -> Option<CarKey> {
        if usage_page(u) == PAGE_TELEPHONY {
            return match usage_id(u) {
                0x20 => Some(CarKey::Hook),
                0x26 => Some(CarKey::Drop),
                _ => None,
            };
        }
        if usage_page(u) != PAGE_CONSUMER {
            return None;
        }
        Some(match usage_id(u) {
            0xB0 => CarKey::Play,
            0xB1 => CarKey::Pause,
            0xB5 => CarKey::Next,     // Scan Next Track
            0xB6 => CarKey::Previous, // Scan Previous Track
            0xB7 => CarKey::Stop,
            0xCD => CarKey::PlayPause,
            _ => return None,
        })
    }

    /// The name sent to rascar-app (`key <name>` on 127.0.0.1:7705), which the
    /// dashboard also uses for its media actions.
    pub fn wire_name(self) -> &'static str {
        match self {
            CarKey::Play => "play",
            CarKey::Pause => "pause",
            CarKey::PlayPause => "play-pause",
            CarKey::Next => "next",
            CarKey::Previous => "previous",
            CarKey::Stop => "stop",
            CarKey::Hook => "hook",
            CarKey::Drop => "drop",
        }
    }
}

/// Human name of a usage for the logs (the ones a car is likely to send).
pub fn usage_name(u: Usage) -> String {
    let name = match (usage_page(u), usage_id(u)) {
        (PAGE_CONSUMER, 0xB0) => "Play",
        (PAGE_CONSUMER, 0xB1) => "Pause",
        (PAGE_CONSUMER, 0xB5) => "Scan Next Track",
        (PAGE_CONSUMER, 0xB6) => "Scan Previous Track",
        (PAGE_CONSUMER, 0xB7) => "Stop",
        (PAGE_CONSUMER, 0xCD) => "Play/Pause",
        (PAGE_CONSUMER, 0xE2) => "Mute",
        (PAGE_CONSUMER, 0xE9) => "Volume Increment",
        (PAGE_CONSUMER, 0xEA) => "Volume Decrement",
        (PAGE_CONSUMER, 0x40) => "Menu",
        (PAGE_CONSUMER, 0x223) => "AC Home",
        (PAGE_CONSUMER, 0x224) => "AC Back",
        (PAGE_CONSUMER, 0x29E) => "AC Navigation Guidance",
        (PAGE_TELEPHONY, 0x20) => "Hook Switch",
        (PAGE_TELEPHONY, 0x21) => "Flash",
        (PAGE_TELEPHONY, 0x26) => "Drop",
        (PAGE_TELEPHONY, 0x2F) => "Phone Mute",
        (PAGE_KEYBOARD, 0x2A) => "Keyboard Delete",
        _ => "",
    };
    if name.is_empty() {
        format!("usage {:#06x}:{:#06x}", usage_page(u), usage_id(u))
    } else {
        format!("{name} ({:#04x}:{:#06x})", usage_page(u), usage_id(u))
    }
}

/// A key transition, derived by comparing two consecutive reports of a device.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyEdge {
    Down(Usage),
    /// Released, with how long it was held when we know it.
    Up(Usage, Option<std::time::Duration>),
}

/// Remembers which keys each device holds, to turn level reports into edges.
#[derive(Default)]
pub struct KeyTracker {
    held: HashMap<String, Vec<(Usage, std::time::Instant)>>,
}

impl KeyTracker {
    pub fn update(&mut self, uuid: &str, pressed: &[Usage], now: std::time::Instant) -> Vec<KeyEdge> {
        let held = self.held.entry(uuid.to_string()).or_default();
        let mut edges = Vec::new();
        held.retain(|(u, since)| {
            let still = pressed.contains(u);
            if !still {
                edges.push(KeyEdge::Up(*u, Some(now.saturating_duration_since(*since))));
            }
            still
        });
        for &u in pressed {
            if !held.iter().any(|(h, _)| *h == u) {
                held.push((u, now));
                edges.push(KeyEdge::Down(u));
            }
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Verbatim from the Jaguar F-PACE's `/info` reply, `hidDevices[].hidDescriptor`,
    // pcap captures/boot-20260922-181915-86bcbfba/019-182136-net.pcap0 (and
    // byte-identical in all 28 plaintext /info replies captured 2026-09-19..29).
    const JAGUAR_TOUCH: &str = "050d0904a101050d0922a102050d0933150025013500466400550f651175019501810275079501810105010930150026ff7f7510950181020931150026ff7f751095018102c0c0";
    const JAGUAR_TELEPHONY: &str = "050b0901a1011500250f050b0900092009210926750895018101c0";
    const JAGUAR_CONSUMER: &str = "050c0901a10115002506050c0a00000ab0000ab1000ab5000ab600750895018101c0";

    #[test]
    fn jaguar_consumer_keypad_descriptor() {
        let l = parse_descriptor(&hex(JAGUAR_CONSUMER));
        assert_eq!(l.application, Some(usage(PAGE_CONSUMER, 0x01)));
        assert!(!l.uses_report_ids);
        assert_eq!(l.fields.len(), 1);
        let f = &l.fields[0];
        assert!(f.array);
        assert!(f.constant, "the Jaguar flags it 81 01 — kept as observed");
        assert_eq!((f.size, f.count, f.logical_min, f.logical_max), (8, 1, 0, 6));
        assert_eq!(
            f.usages,
            vec![
                usage(PAGE_CONSUMER, 0x00),
                usage(PAGE_CONSUMER, 0xB0),
                usage(PAGE_CONSUMER, 0xB1),
                usage(PAGE_CONSUMER, 0xB5),
                usage(PAGE_CONSUMER, 0xB6),
            ]
        );
    }

    #[test]
    fn jaguar_telephony_keypad_descriptor() {
        let l = parse_descriptor(&hex(JAGUAR_TELEPHONY));
        assert_eq!(l.application, Some(usage(PAGE_TELEPHONY, 0x01)));
        let f = &l.fields[0];
        assert!(f.array);
        assert_eq!((f.size, f.count, f.logical_min, f.logical_max), (8, 1, 0, 15));
        assert_eq!(
            f.usages,
            vec![
                usage(PAGE_TELEPHONY, 0x00),
                usage(PAGE_TELEPHONY, 0x20),
                usage(PAGE_TELEPHONY, 0x21),
                usage(PAGE_TELEPHONY, 0x26),
            ]
        );
    }

    #[test]
    fn jaguar_touch_descriptor_is_a_digitizer_with_the_known_layout() {
        let l = parse_descriptor(&hex(JAGUAR_TOUCH));
        assert_eq!(l.application, Some(usage(PAGE_DIGITIZER, 0x04)));
        let dev = HidDevice { uuid: "u".into(), name: "Touch Screen".into(), layout: l.clone() };
        assert!(dev.is_touch());
        // tip (1 bit) + 7 bits padding + X (16) + Y (16) = the 5-byte report
        // `parse_hid_touch` reads, with X/Y logical 0..32767 (HID_TOUCH_LOGICAL).
        let shape: Vec<_> = l.fields.iter().map(|f| (f.bit_offset, f.size, f.count, f.usages.clone())).collect();
        assert_eq!(
            shape,
            vec![
                (0, 1, 1, vec![usage(PAGE_DIGITIZER, 0x33)]),
                (1, 7, 1, vec![]),
                (8, 16, 1, vec![usage(PAGE_GENERIC_DESKTOP, 0x30)]),
                (24, 16, 1, vec![usage(PAGE_GENERIC_DESKTOP, 0x31)]),
            ]
        );
        assert_eq!((l.fields[2].logical_min, l.fields[2].logical_max), (0, 32767));
    }

    #[test]
    fn jaguar_scan_next_press_and_release_from_a_real_capture() {
        // Real reports from the Consumer keypad, pcap
        // captures/boot-20260922-181311-f4a15a9d/001-181311-net.pcap0:
        // 16:13:19.160 `03`, 16:13:19.274 `00` (the consumer keypad).
        let l = parse_descriptor(&hex(JAGUAR_CONSUMER));
        let down = pressed_usages(&l, &[0x03]).unwrap();
        assert_eq!(down, vec![usage(PAGE_CONSUMER, 0xB5)]);
        assert_eq!(CarKey::from_usage(down[0]), Some(CarKey::Next));
        assert_eq!(pressed_usages(&l, &[0x00]).unwrap(), Vec::<Usage>::new());

        let mut t = KeyTracker::default();
        let t0 = Instant::now();
        assert_eq!(t.update("consumer-keypad", &down, t0), vec![KeyEdge::Down(usage(PAGE_CONSUMER, 0xB5))]);
        let up = t.update("consumer-keypad", &[], t0 + Duration::from_millis(114));
        assert_eq!(up, vec![KeyEdge::Up(usage(PAGE_CONSUMER, 0xB5), Some(Duration::from_millis(114)))]);
    }

    #[test]
    fn jaguar_consumer_index_maps_through_the_descriptor_not_a_fixed_table() {
        // Index → usage comes from the car's list: 1 Play, 2 Pause, 3 Next, 4 Previous.
        let l = parse_descriptor(&hex(JAGUAR_CONSUMER));
        let key = |b: u8| pressed_usages(&l, &[b]).unwrap().first().and_then(|u| CarKey::from_usage(*u));
        assert_eq!(key(1), Some(CarKey::Play));
        assert_eq!(key(2), Some(CarKey::Pause));
        assert_eq!(key(3), Some(CarKey::Next));
        assert_eq!(key(4), Some(CarKey::Previous));
        // 5 and 6 are inside the logical range but past the usage list: no key.
        assert_eq!(pressed_usages(&l, &[5]).unwrap(), Vec::<Usage>::new());
        assert_eq!(pressed_usages(&l, &[7]).unwrap(), Vec::<Usage>::new());
    }

    #[test]
    fn catplay_media_descriptor_puts_play_pause_at_index_3() {
        // catplay catplay_hid/src/media_buttons.rs `descriptor()`: the same byte
        // `03` means Play/Pause there — why mapping must follow the descriptor.
        let desc = hex("050c0901a10115002506050c0a00000ab0000ab1000acd000ab5000ab6000a9e02750895018100c0");
        let l = parse_descriptor(&desc);
        let u = pressed_usages(&l, &[3]).unwrap();
        assert_eq!(CarKey::from_usage(u[0]), Some(CarKey::PlayPause));
        assert_eq!(usage_name(pressed_usages(&l, &[6]).unwrap()[0]), "AC Navigation Guidance (0x0c:0x029e)");
    }

    #[test]
    fn telephony_hook_and_drop_are_distinct_flash_does_nothing() {
        let l = parse_descriptor(&hex(JAGUAR_TELEPHONY));
        let hook = pressed_usages(&l, &[1]).unwrap();
        assert_eq!(hook, vec![usage(PAGE_TELEPHONY, 0x20)]);
        assert_eq!(CarKey::from_usage(hook[0]), Some(CarKey::Hook));
        assert_eq!(usage_name(hook[0]), "Hook Switch (0x0b:0x0020)");
        let flash = pressed_usages(&l, &[2]).unwrap();
        assert_eq!(flash, vec![usage(PAGE_TELEPHONY, 0x21)]);
        assert_eq!(CarKey::from_usage(flash[0]), None);
        let drop = pressed_usages(&l, &[3]).unwrap();
        assert_eq!(drop, vec![usage(PAGE_TELEPHONY, 0x26)]);
        assert_eq!(CarKey::from_usage(drop[0]), Some(CarKey::Drop));
        assert_eq!(CarKey::Hook.wire_name(), "hook");
        assert_eq!(CarKey::Drop.wire_name(), "drop");
    }

    #[test]
    fn short_or_foreign_reports_are_undecodable() {
        let l = parse_descriptor(&hex(JAGUAR_CONSUMER));
        assert_eq!(pressed_usages(&l, &[]), None);
        let touch = parse_descriptor(&hex(JAGUAR_TOUCH));
        // A 1-byte report cannot hold the touch layout's X/Y.
        assert_eq!(pressed_usages(&touch, &[0x01]), None);
    }

    #[test]
    fn bitmap_buttons_and_report_ids() {
        // Report ID 2, four 1-bit variable buttons (Next, Prev, Play/Pause, Mute) + 4 pad.
        let desc = [
            0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04,
            0x09, 0xB5, 0x09, 0xB6, 0x09, 0xCD, 0x09, 0xE2, 0x81, 0x02, 0x95, 0x04, 0x81, 0x03, 0xC0,
        ];
        let l = parse_descriptor(&desc);
        assert!(l.uses_report_ids);
        assert_eq!(pressed_usages(&l, &[0x02, 0b0000_0110]).unwrap(), vec![usage(PAGE_CONSUMER, 0xB6), usage(PAGE_CONSUMER, 0xCD)]);
        assert_eq!(pressed_usages(&l, &[0x01, 0x01]), None, "unknown report id");
    }

    #[test]
    fn truncated_and_hostile_descriptors_do_not_panic() {
        let full = hex(JAGUAR_CONSUMER);
        for n in 0..full.len() {
            let l = parse_descriptor(&full[..n]);
            let _ = pressed_usages(&l, &[3]);
        }
        // Usage Minimum 0, Usage Maximum 0xFFFF: range ignored, no huge allocation.
        let l = parse_descriptor(&[0x05, 0x0C, 0x19, 0x00, 0x2A, 0xFF, 0xFF, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00]);
        assert!(l.fields[0].usages.len() <= 1);
        let _ = parse_descriptor(&[0xFE, 0xFF]);
    }
}
