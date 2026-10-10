//! iAP2 over HID in USB *device* mode (the non-CarPlay path).
//!
//! With a serial it has never seen, the Jaguar does not jump to the CarPlay role
//! switch: it first probes classic iAP2 over the HID interface of configuration 2
//! ("iPod USB Interface"), sending the detect sequence `FF 55 02 00 EE 10` as a
//! SET_REPORT every second (seen on a drive: 8 tries, then re-enumeration).
//! A real iPhone echoes it on the HID interrupt IN endpoint and the accessory then
//! opens the iAP2 link. This is the transport InControl (Bosch mySPIN) needs.
//!
//! Wire format (same as `ipod/hid/hid.go`): every report is
//! `[report id][link control][payload, zero-padded to the report size]`, where the
//! report size comes from [`super::ffs::HID_REPORT`]. Link control: 0x00 = whole
//! message, 0x02 = first of several, 0x03 = middle, 0x01 = last.
//!
//! * car -> phone: Output reports arrive as ep0 SET_REPORTs; `ffs.rs` forwards
//!   their data to the sink installed with [`super::ffs::set_hid_iap2_sink`].
//! * phone -> car: Input reports written to the interrupt IN endpoint file.
//!
//! The iAP2 link parser resyncs over garbage, so the zero padding of the last
//! report of a message is simply skipped; the decoder does not need to trim it.

use std::collections::VecDeque;
use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

/// Input (phone -> accessory) reports of [`super::ffs::HID_REPORT`]: (id, report count).
/// The count covers the link-control byte and the payload, not the report id.
pub const INPUT_REPORTS: [(u8, usize); 12] = [
    (0x01, 5),
    (0x02, 9),
    (0x03, 13),
    (0x04, 17),
    (0x05, 25),
    (0x06, 49),
    (0x07, 95),
    (0x08, 193),
    (0x09, 257),
    (0x0a, 385),
    (0x0b, 513),
    (0x0c, 767),
];

/// Interrupt IN wMaxPacketSize (see `ffs::interrupt`). Only reports that fit one
/// packet are used, so each report is exactly one interrupt transfer.
const INTERRUPT_MAX_PACKET: usize = 64;

/// FunctionFS file of configuration 2's only endpoint (the HID interrupt IN).
pub const HID_IN_EP: &str = "/dev/ffs-carplay-c2/ep1";

/// Whether the device-mode iAP2-over-HID responder runs (default off; opt in
/// with `RASCAR_IAP2_HID=1`). Off keeps the CarPlay-only behaviour byte for byte.
pub fn enabled() -> bool {
    enabled_from(std::env::var("RASCAR_IAP2_HID").ok().as_deref())
}

fn enabled_from(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1") | Some("on") | Some("true") | Some("yes"))
}

/// How many soft reconnects one process may do in answer to `40 51 wValue=0`.
const MAX_STAY_DEVICE_RECONNECTS: u32 = 3;
static STAY_DEVICE_RECONNECTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Answer the car's `40 51 wValue=0` ("stay USB device") by dropping off the bus
/// and coming back, as a phone acts on a role-switch request.
///
/// Hypothesis under test (three drives):
/// after `wValue=0` the car stayed silent for minutes both times, never starting
/// iAP2 over HID; it may be waiting for this re-enumeration. Uses the UDC
/// `soft_connect` attribute, so the gadget and every FunctionFS fd stay intact.
/// Runs on its own thread (the ep0 loop must keep answering), at most
/// [`MAX_STAY_DEVICE_RECONNECTS`] times per process.
///
/// REFUTED and now opt-in (`RASCAR_STAY_DEVICE_REENUM=1`): on one drive the
/// car re-enumerated and stayed silent, and on another (C0 53 answered
/// "no CarPlay") the car sent wValue=0 *after* a live iAP2-over-HID session was
/// up, so the reconnect cut a working session.
pub fn soft_reconnect_after_stay_device() {
    if !matches!(std::env::var("RASCAR_STAY_DEVICE_REENUM").ok().as_deref().map(str::trim), Some("1") | Some("on")) {
        info!("stay-device: not reconnecting (opt in with RASCAR_STAY_DEVICE_REENUM=1)");
        return;
    }
    let n = STAY_DEVICE_RECONNECTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    if n > MAX_STAY_DEVICE_RECONNECTS {
        info!("stay-device: {MAX_STAY_DEVICE_RECONNECTS} soft reconnects already done — not reconnecting again");
        return;
    }
    let _ = std::thread::Builder::new().name("stay-device-reenum".into()).spawn(move || {
        let Some(udc) = std::fs::read_dir("/sys/class/udc")
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
        else {
            warn!("stay-device: no UDC under /sys/class/udc — cannot soft reconnect");
            return;
        };
        std::thread::sleep(Duration::from_millis(200));
        info!("stay-device: soft reconnect #{n} — disconnecting from the bus ({udc})");
        match crate::usb::gadget::soft_reconnect(&udc, Duration::from_millis(1000)) {
            Ok(()) => info!("stay-device: soft reconnect #{n} — reconnected, expecting a fresh enumeration"),
            Err(e) => warn!("stay-device: soft reconnect failed: {e:#} — the car may not see us again"),
        }
    });
}

/// Reports usable for one interrupt transfer, smallest first.
fn usable_reports() -> impl Iterator<Item = (u8, usize)> {
    INPUT_REPORTS.into_iter().filter(|(_, count)| 1 + count <= INTERRUPT_MAX_PACKET)
}

/// Split one iAP2 frame into Input reports.
pub fn encode_frame(frame: &[u8]) -> Vec<Vec<u8>> {
    let largest = usable_reports().last().expect("at least one usable report");
    let mut reports = Vec::new();
    let mut offset = 0;
    while offset < frame.len() || reports.is_empty() {
        let left = frame.len() - offset;
        let (id, count) = usable_reports().find(|(_, c)| c - 1 >= left).unwrap_or(largest);
        let take = left.min(count - 1);
        let first = offset == 0;
        let last = offset + take == frame.len();
        let link_control = match (first, last) {
            (true, true) => 0x00,
            (true, false) => 0x02,
            (false, false) => 0x03,
            (false, true) => 0x01,
        };
        let mut report = vec![0u8; 1 + count];
        report[0] = id;
        report[1] = link_control;
        report[2..2 + take].copy_from_slice(&frame[offset..offset + take]);
        reports.push(report);
        offset += take;
    }
    reports
}

/// Payload bytes of one Output report as received in a SET_REPORT data stage
/// (`[id][link control][payload...]`).
pub fn decode_report(report: &[u8]) -> &[u8] {
    report.get(2..).unwrap_or(&[])
}

/// Writer thread: frames arrive whole, leave as Input reports on [`HID_IN_EP`].
/// A write before the car selects configuration 2 fails (ESHUTDOWN); the file is
/// reopened and the report retried once, then the frame is dropped — iAP2
/// retransmits on its own.
pub fn spawn_writer() -> std::sync::mpsc::Sender<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("hid-iap2-tx".into())
        .spawn(move || {
            let mut ep: Option<std::fs::File> = None;
            while let Ok(frame) = rx.recv() {
                let reports = encode_frame(&frame);
                debug!(
                    "hid-iap2 -> car: {} byte frame in {} report(s): {}",
                    frame.len(),
                    reports.len(),
                    crate::debug::hex_prefix(&frame, 32)
                );
                'reports: for report in reports {
                    for attempt in 0..2 {
                        if ep.is_none() {
                            match std::fs::OpenOptions::new().write(true).open(HID_IN_EP) {
                                Ok(f) => ep = Some(f),
                                Err(e) => {
                                    warn!("hid-iap2: opening {HID_IN_EP} failed: {e} — frame dropped");
                                    break 'reports;
                                }
                            }
                        }
                        match ep.as_mut().map(|f| f.write_all(&report)) {
                            Some(Ok(())) => continue 'reports,
                            Some(Err(e)) => {
                                warn!("hid-iap2: report write failed (try {}): {e}", attempt + 1);
                                ep = None;
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            None => unreachable!(),
                        }
                    }
                    warn!("hid-iap2: report dropped after 2 tries — rest of the frame dropped");
                    break;
                }
            }
            info!("hid-iap2: writer thread stopping (all senders gone)");
        })
        .expect("spawning the hid-iap2 writer thread");
    tx
}

/// iAP2 byte transport over the HID interface.
pub struct HidTransport {
    rx: UnboundedReceiver<Vec<u8>>,
    pending: VecDeque<u8>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl HidTransport {
    /// `rx` yields raw SET_REPORT data stages; `tx` feeds [`spawn_writer`].
    pub fn new(rx: UnboundedReceiver<Vec<u8>>, tx: std::sync::mpsc::Sender<Vec<u8>>) -> Self {
        Self { rx, pending: VecDeque::new(), tx }
    }
}

impl AsyncRead for HidTransport {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        while self.pending.is_empty() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(report)) => {
                    let payload = decode_report(&report).to_vec();
                    self.pending.extend(payload);
                }
                // Sink replaced or dropped: end of stream.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = buf.remaining().min(self.pending.len());
        let chunk: Vec<u8> = self.pending.drain(..n).collect();
        buf.put_slice(&chunk);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for HidTransport {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.tx.send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "hid-iap2 writer gone"))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[async_trait::async_trait]
impl crate::iap2::Iap2Transport for HidTransport {}

/// Run the device-mode iAP2 session over HID until the task is aborted.
///
/// Each round installs a fresh sink, brings the link up and, once ready, serves
/// it; any failure (re-enumeration, SYN timeout) starts a new round, so the car's
/// retries after a re-enumeration are answered too.
pub async fn run_device_mode(challenge: [u8; 32]) {
    let writer = spawn_writer();
    let mut round = 0u32;
    loop {
        round += 1;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        super::ffs::set_hid_iap2_sink(Some(tx));
        info!("hid-iap2: round {round} — waiting for the accessory on the HID interface");
        let mut device = crate::iap2::Iap2Device::with_default_config(HidTransport::new(rx, writer.clone()));
        match device.run_until_ready(&challenge, &mut ()).await {
            Ok(ready) => {
                info!(
                    "hid-iap2: **** iAP2 SESSION READY OVER HID **** accessory {:?} / {:?}",
                    ready.identification.name, ready.identification.manufacturer
                );
                // `service_link` only ever returns an error (Result<Infallible>).
                let Err(e) = device.service_link().await;
                warn!("hid-iap2: link service ended: {e:#}");
            }
            Err(e) => warn!("hid-iap2: round {round} bring-up failed: {e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse (report id, count, is_input) out of the HID report descriptor.
    fn parse_reports(desc: &[u8]) -> Vec<(u8, usize, bool)> {
        let (mut i, mut id, mut count) = (0, 0u8, 0usize);
        let mut out = Vec::new();
        while i < desc.len() {
            let prefix = desc[i];
            let size = match prefix & 0x03 { 3 => 4, n => n as usize };
            let data = &desc[i + 1..i + 1 + size];
            let value = data.iter().rev().fold(0usize, |acc, b| (acc << 8) | *b as usize);
            match prefix & 0xFC {
                0x84 => id = value as u8,
                0x94 => count = value,
                0x80 => out.push((id, count, true)),
                0x90 => out.push((id, count, false)),
                _ => {}
            }
            i += 1 + size;
        }
        out
    }

    #[test]
    fn input_table_matches_the_served_descriptor() {
        let inputs: Vec<(u8, usize)> = parse_reports(&crate::usb::ffs::HID_REPORT)
            .into_iter()
            .filter(|r| r.2)
            .map(|r| (r.0, r.1))
            .collect();
        assert_eq!(inputs, INPUT_REPORTS.to_vec());
    }

    #[test]
    fn detect_echo_fits_one_report() {
        let reports = encode_frame(&crate::iap2::link::IAP2_DETECT);
        assert_eq!(reports, vec![vec![0x02, 0x00, 0xFF, 0x55, 0x02, 0x00, 0xEE, 0x10, 0x00, 0x00]]);
    }

    #[test]
    fn long_frame_splits_with_link_control_flags() {
        let frame: Vec<u8> = (0..120u8).collect();
        let reports = encode_frame(&frame);
        let flags: Vec<u8> = reports.iter().map(|r| r[1]).collect();
        assert_eq!(flags, vec![0x02, 0x03, 0x01]);
        assert!(reports.iter().all(|r| r.len() <= INTERRUPT_MAX_PACKET));
        let joined: Vec<u8> = reports.iter().flat_map(|r| decode_report(r).to_vec()).collect();
        assert_eq!(&joined[..frame.len()], &frame[..]);
    }

    #[test]
    fn car_detect_report_decodes_to_the_detect_sequence() {
        let car = [0x0e, 0x00, 0xff, 0x55, 0x02, 0x00, 0xee, 0x10, 0x00];
        assert_eq!(&decode_report(&car)[..6], &crate::iap2::link::IAP2_DETECT);
    }

    #[test]
    fn opt_in_only() {
        assert!(!enabled_from(None));
        assert!(!enabled_from(Some("0")));
        assert!(enabled_from(Some("1")));
        assert!(enabled_from(Some(" on ")));
    }

    #[tokio::test]
    async fn transport_reads_payload_and_writes_reports() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let mut t = HidTransport::new(in_rx, out_tx);
        in_tx.send(vec![0x0e, 0x00, 0xff, 0x55, 0x02, 0x00, 0xee, 0x10, 0x00]).unwrap();
        let mut buf = [0u8; 6];
        t.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, crate::iap2::link::IAP2_DETECT);
        t.write_all(b"abc").await.unwrap();
        assert_eq!(out_rx.recv().unwrap(), b"abc".to_vec());
    }

    /// Real car SYN (captured 2026-09, see iap2::link tests), fed as padded
    /// Output reports exactly like the car does: the link must echo the detect
    /// and answer with a SYN-ACK despite the zero padding between messages.
    #[tokio::test]
    async fn link_negotiates_over_padded_hid_reports() {
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let transport = HidTransport::new(in_rx, out_tx);
        let syn = hex::decode("ff5a001a80180000f50105100001f400b01e0a01000202010116").unwrap();
        in_tx.send(vec![0x0e, 0x00, 0xff, 0x55, 0x02, 0x00, 0xee, 0x10, 0x00]).unwrap();
        // 26-byte SYN in one 0x11-sized Output report (count 25 -> 24 payload) + tail.
        let mut first = vec![0x11, 0x02];
        first.extend_from_slice(&syn[..23]);
        in_tx.send(first).unwrap();
        let mut last = vec![0x0d, 0x01];
        last.extend_from_slice(&syn[23..]);
        last.resize(6, 0);
        in_tx.send(last).unwrap();

        let mut link = crate::iap2::link::Iap2Link::new(transport, Default::default());
        // The mock car never sends the completing ACK, so negotiation is cut off
        // by this timeout; what matters is what went out on the wire before it.
        let _ = tokio::time::timeout(Duration::from_secs(2), link.negotiate_device()).await;
        let frames: Vec<Vec<u8>> = out_rx.try_iter().collect();
        assert_eq!(frames.first().map(|f| f.as_slice()), Some(&crate::iap2::link::IAP2_DETECT[..]), "detect echo first");
        assert!(
            frames.iter().any(|f| f.starts_with(&[0xff, 0x5a]) && f.get(4) == Some(&0xC0)),
            "a SYN-ACK (control 0xC0) must follow: {frames:02x?}"
        );
    }
}
