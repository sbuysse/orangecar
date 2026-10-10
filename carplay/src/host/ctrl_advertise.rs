//! Device-side `_carplay-ctrl._tcp` Bonjour advertisement + control connect-out
//! listener on the CDC-NCM link.
//!
//! **Root cause (2026-09-19, full-interface pcap).** The Jaguar F-PACE persistently
//! mDNS-browses for `_carplay-ctrl._tcp.local` on the NCM link and, finding no
//! answer it accepts, tears the USB CarPlay session down at ~7 s from enumeration
//! (no NAK, no `requestUI`). A real iPhone advertises this service; the car browses
//! it, resolves host+port and dials `GET /ctrl-int/1/connect`.
//!
//! **Why a raw SOLE responder, not avahi (offline-proven 2026-09-19).** We captured
//! avahi's `_carplay-ctrl` announcement on the wire and it emits NO NSEC record and
//! NO reverse `ip6.arpa` PTR (`avahi-publish-address -R` = `--no-reverse`), TTL
//! ~120 s. A real iPhone sends ONE bundled authoritative packet with SRV + AAAA +
//! reverse-PTR + TXT + service PTRs + device-info + **3 NSEC** (host NSEC =
//! "AAAA-only"), TTL 4500. The host "AAAA-only" NSEC is the authoritative proof
//! "no A exists → dial the AAAA now"; without it the car's Apple mDNSResponder waits
//! its full negative timeout for an A record and never dials. So we emit the
//! iPhone's packet layout ([`crate::host::mdns_ctrl`], tested byte for byte against a
//! real-iPhone announcement) from a raw
//! responder bound to the NCM iface (SO_BINDTODEVICE + SO_REUSEPORT, hop limit 255),
//! and STOP publishing `_carplay-ctrl` via avahi so only one responder answers it
//! (the earlier custom responder failed because it raced avahi on the same name).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::host::mdns_ctrl::CtrlAdvert;

/// TCP port advertised in the SRV record and served for the control connect-out.
pub const CTRL_PORT: u16 = 7100;
/// Service instance name (the `<name>._carplay-ctrl._tcp.local` label).
/// The reference transmitter advertises `iPhone`; overridable for an A/B.
const CTRL_INSTANCE: &str = "iPhone";
/// Dedicated host label the SRV points at (resolves to ONLY the NCM link-local).
const CTRL_HOSTLABEL: &str = "iPhone";
/// srcvers TXT value — the AirPlay SDK generation we claim to be.
///
/// This MUST match the `sourceVersion` we put in the RTSP `SETUP` and the
/// `User-Agent: AirPlay/<v>` we send (`airplay::carplay_session`), because they
/// describe the same device. It did not: we advertised `925.5.1` here (an
/// iOS-26-class phone) while speaking `566.25.21` on the wire. A head unit that
/// resolves our record and then declines to dial — exactly what this car does —
/// may be routing a "modern CarPlay" srcvers down a path we do not implement, or
/// rejecting the mismatch once it sees the real version. The reference
/// transmitter uses one constant everywhere.
const CTRL_SRCVERS: &str = "566.25.21";
/// _device-info model.
const CTRL_MODEL: &str = "D84AP";
/// TXT `id` — the single device MAC (crate::DEVICE_MAC), consistent with the AirPlay identity.
const CTRL_DEVICE_ID: &str = crate::DEVICE_MAC;
/// Apple's TTL for the `_carplay-ctrl` records.
const CTRL_TTL: u32 = 4500;

const MDNS_PORT: u16 = 5353;
const MDNS_MCAST_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_MCAST_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x00fb);

/// Keeps the control TCP listener alive for the process. The mDNS responder runs
/// on its own detached std thread (it must outlive any single call, like the
/// real iPhone's responder does).
pub struct CtrlAdvertise {
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Start the control connect-out listener at PROCESS START. The `_carplay-ctrl`
/// mDNS responder itself needs the NCM iface + link-local, so it is started later
/// by [`publish_ncm_address`] once the link is up. Best-effort.
pub fn spawn() -> CtrlAdvertise {
    let mut tasks = Vec::new();
    // ONE dual-stack [::]:7100 socket serving BOTH families. The car dials our
    // IPv6 link-local for /ctrl-int/1/connect, so the v6 listener is mandatory —
    // the old "bind [::] then 0.0.0.0, tolerate one failing" design silently lost
    // IPv6 whenever a lingering prior instance held [::]:7100 (v6-only), leaving
    // only IPv4 and making the car's dial hit a closed port. A dual-stack socket
    // (IPV6_V6ONLY=false) + SO_REUSEADDR removes both the two-bind race and the
    // TIME_WAIT-from-a-just-exited-instance failure.
    match build_ctrl_listener(CTRL_PORT) {
        Ok(l) => {
            info!("ctrl-advertise: listening on [::]:{CTRL_PORT} (dual-stack, v4+v6) for /ctrl-int/1/connect");
            tasks.push(tokio::spawn(async move { serve_ctrl(l).await }));
        }
        Err(e) => warn!("ctrl-advertise: FATAL — could not bind the dual-stack control listener on :{CTRL_PORT}: {e} (car cannot dial /ctrl-int)"),
    }
    info!("ctrl-advertise: control listener up; _carplay-ctrl responder starts once the NCM link is up");
    CtrlAdvertise { _tasks: tasks }
}

/// Bind a single dual-stack (IPv4 + IPv6) TCP listener on `[::]:port` with
/// SO_REUSEADDR, so the car's IPv6-link-local dial and any v4 dial both land here.
fn build_ctrl_listener(port: u16) -> std::io::Result<TcpListener> {
    let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    sock.set_only_v6(false)?; // dual-stack: also accept IPv4 (v4-mapped)
    sock.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0).into())?;
    sock.listen(1024)?;
    sock.set_nonblocking(true)?;
    TcpListener::from_std(sock.into())
}

/// Start the raw `_carplay-ctrl` mDNS responder once the NCM link is up.
///
/// `link_local` is the NCM IPv6 link-local as `fe80::…%ifname`. We emit an
/// announcement laid out byte for byte like a real iPhone's (with the load-bearing NSEC records) bound
/// to that interface, as the SOLE responder for the service. Detached thread that
/// runs to process exit.
pub fn publish_ncm_address(link_local: &str) {
    let (addr_part, ifname) = match link_local.split_once('%') {
        Some((a, i)) => (a, i.to_string()),
        None => {
            warn!("ctrl-advertise: link-local '{link_local}' has no %iface scope — cannot bind the responder");
            return;
        }
    };
    let addr: Ipv6Addr = match addr_part.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!("ctrl-advertise: cannot parse link-local '{addr_part}': {e}");
            return;
        }
    };
    let addr_v4 = ipv4_of_iface(&ifname);

    // A/B the responder itself (RASCAR_CTRL_VIA_AVAHI=1).
    //
    // Measured at the car (boot-20260921-084125 run 031): the Jaguar sends a burst
    // of four `_carplay-ctrl._tcp` queries at 3.73 s, we answer 51 ms later — and
    // it QUERIES AGAIN at 4.49 s. Re-asking after a prompt answer means it is not
    // accepting ours. We emit a hand-built "byte-exact" packet from a raw socket;
    // avahi is a real, conformant responder. If the car dials when avahi
    // publishes and not when we do, the defect is in our packet, and that is the
    // whole ballgame: the CarlinKit firmware shows the reference implementation
    // simply WAITS to be dialled ("Wait HU connect CarPlayControl server!!!",
    // dec_fakeiOSDevice), so being uninvited is the abnormal state, not a quirk
    // of this head unit.
    if crate::debug::env_flag("RASCAR_CTRL_VIA_AVAHI", false) {
        publish_via_avahi(&ifname, addr_v4);
        return;
    }

    std::thread::Builder::new()
        .name("carplay-ctrl-mdns".into())
        .spawn(move || {
            // The IPv4 can be missing for a moment (systemd-networkd flushed our
            // static 192.168.0.2 once, until its own DHCP lease came
            // back), and the responder keeps what it starts with for its whole
            // life — so wait briefly, here on its own thread, off the async path.
            let mut addr_v4 = addr_v4;
            for _ in 0..10 {
                if addr_v4.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                addr_v4 = ipv4_of_iface(&ifname);
            }
            info!(
                "ctrl-advertise: starting raw _carplay-ctrl responder on {ifname} (AAAA {addr}, A {:?})",
                addr_v4
            );
            run_responder(ifname, addr, addr_v4)
        })
        .map(|_| ())
        .unwrap_or_else(|e| warn!("ctrl-advertise: could not spawn responder thread: {e}"));
}

/// Build the bundled announcement packet for our identity + address.
fn build_packet(addr: Ipv6Addr, addr_v4: Option<Ipv4Addr>) -> Vec<u8> {
    // Each field is overridable from the environment so the car-side A/B does not
    // need a rebuild: attempts are scarce and each one costs head-unit goodwill.
    let instance = std::env::var("RASCAR_CTRL_INSTANCE").unwrap_or_else(|_| CTRL_INSTANCE.to_string());
    let host = std::env::var("RASCAR_CTRL_HOSTLABEL").unwrap_or_else(|_| CTRL_HOSTLABEL.to_string());
    let srcvers = std::env::var("RASCAR_CTRL_SRCVERS").unwrap_or_else(|_| CTRL_SRCVERS.to_string());
    match addr_v4 {
        Some(v4) => info!(
            "ctrl-advertise: announcing instance='{instance}' host='{host}.local.' srcvers={srcvers} \
             port={CTRL_PORT} AAAA={addr} A={v4} (the car refuses to dial a link-local-only host)"
        ),
        None => info!(
            "ctrl-advertise: announcing instance='{instance}' host='{host}.local.' srcvers={srcvers} \
             port={CTRL_PORT} AAAA={addr} (NO IPv4 on the NCM link — the car may not dial us)"
        ),
    }
    CtrlAdvert {
        instance: &instance,
        host: &host,
        port: CTRL_PORT,
        link_local: addr,
        srcvers: &srcvers,
        device_id: CTRL_DEVICE_ID,
        model: CTRL_MODEL,
        ttl: CTRL_TTL,
        ipv4: addr_v4,
    }
    .packet()
}

/// First non-loopback IPv4 configured on `ifname`, via `ip -4 -o addr show`.
///
/// The car is 192.168.0.1 on the NCM link and we are 192.168.0.2; publishing that
/// A record is what lets it dial our `_carplay-ctrl` port at all.
fn ipv4_of_iface(ifname: &str) -> Option<Ipv4Addr> {
    let out = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", ifname])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.split_whitespace().skip_while(|t| *t != "inet").nth(1) {
            let ip = rest.split('/').next()?;
            if let Ok(v4) = ip.parse::<Ipv4Addr>() {
                if !v4.is_loopback() {
                    return Some(v4);
                }
            }
        }
    }
    None
}

fn if_nametoindex(name: &str) -> u32 {
    match std::ffi::CString::new(name) {
        Ok(c) => unsafe { libc::if_nametoindex(c.as_ptr()) },
        Err(_) => 0,
    }
}

/// Build the IPv6 mDNS socket bound to `ifname` (SO_REUSEPORT to coexist with
/// avahi-daemon on :5353, SO_BINDTODEVICE to scope to the NCM link, hop limit 255
/// per RFC 6762 §11, join ff02::fb).
fn build_v6_socket(ifname: &str, ifindex: u32) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_reuse_port(true)?;
    sock.bind_device(Some(ifname.as_bytes()))?;
    sock.set_only_v6(true)?;
    sock.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MDNS_PORT, 0, 0).into())?;
    sock.join_multicast_v6(&MDNS_MCAST_V6, ifindex)?;
    sock.set_multicast_if_v6(ifindex)?;
    sock.set_multicast_loop_v6(false)?;
    sock.set_multicast_hops_v6(255)?;
    sock.set_read_timeout(Some(Duration::from_millis(1000)))?;
    Ok(sock.into())
}

/// Build the IPv4 mDNS socket (best-effort; the control path is IPv6 but the car
/// may browse on v4). Bound to the NCM iface, joins 224.0.0.251, TTL 255.
fn build_v4_socket(ifname: &str, v4_src: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    sock.set_reuse_port(true)?;
    sock.bind_device(Some(ifname.as_bytes()))?;
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT).into())?;
    sock.join_multicast_v4(&MDNS_MCAST_V4, &v4_src)?;
    sock.set_multicast_if_v4(&v4_src)?;
    sock.set_multicast_loop_v4(false)?;
    sock.set_multicast_ttl_v4(255)?;
    Ok(sock.into())
}

/// Our static NCM IPv4 (assigned in ncm.rs) — the v4 source for the v4 responder.
const NCM_V4: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 2);

/// The responder loop: SOLE responder for `_carplay-ctrl` on the NCM link. Sends an
/// initial burst of unsolicited announcements (populates the car's cache), then
/// answers every `_carplay-ctrl` QUERY it sees and re-announces periodically.
fn run_responder(ifname: String, addr: Ipv6Addr, addr_v4: Option<Ipv4Addr>) {
    let ifindex = if_nametoindex(&ifname);
    if ifindex == 0 {
        warn!("ctrl-advertise: if_nametoindex({ifname}) failed — responder cannot start");
        return;
    }
    let packet = build_packet(addr, addr_v4);

    let v6 = match build_v6_socket(&ifname, ifindex) {
        Ok(s) => s,
        Err(e) => {
            warn!("ctrl-advertise: v6 mDNS socket on {ifname} failed: {e} — responder cannot start");
            return;
        }
    };
    // v4 is best-effort; the control record is AAAA-only but the car may browse v4.
    let v4 = build_v4_socket(&ifname, NCM_V4).ok();
    if v4.is_none() {
        info!("ctrl-advertise: no v4 mDNS socket (control path is IPv6 — continuing v6-only)");
    }

    let v6_dst: std::net::SocketAddr = SocketAddrV6::new(MDNS_MCAST_V6, MDNS_PORT, 0, ifindex).into();
    let v4_dst: std::net::SocketAddr = SocketAddrV4::new(MDNS_MCAST_V4, MDNS_PORT).into();
    let send_all = |why: &str| {
        let _ = v6.send_to(&packet, v6_dst);
        if let Some(v4s) = &v4 {
            let _ = v4s.send_to(&packet, v4_dst);
        }
        info!("ctrl-advertise: sent _carplay-ctrl announce ({} B, {why})", packet.len());
    };

    // Initial burst (RFC 6762 §8.3 announce: a few times, ~250 ms apart).
    for _ in 0..3 {
        send_all("initial announce");
        std::thread::sleep(Duration::from_millis(250));
    }

    let mut buf = [0u8; 2048];
    let mut last_announce = Instant::now();
    loop {
        match v6.recv_from(&mut buf) {
            Ok((n, _peer)) => {
                if is_carplay_ctrl_query(&buf[..n]) {
                    send_all("answering car query");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                warn!("ctrl-advertise: responder recv error: {e} — stopping responder");
                return;
            }
        }
        // Re-announce every ~2 s regardless, so a missed query still gets served.
        if last_announce.elapsed() >= Duration::from_secs(2) {
            send_all("periodic re-announce");
            last_announce = Instant::now();
        }
    }
}

/// True if `pkt` is an mDNS QUERY (QR bit 0) that references `_carplay-ctrl` or our
/// dedicated host — i.e. a query we must answer. Cheap substring scan on the wire
/// bytes (labels are length-prefixed ASCII, so the service name appears literally).
fn is_carplay_ctrl_query(pkt: &[u8]) -> bool {
    if pkt.len() < 12 {
        return false;
    }
    // QR is the top bit of the flags word (byte 2). 0 = query, 1 = response.
    let is_query = pkt[2] & 0x80 == 0;
    if !is_query {
        return false;
    }
    contains(pkt, b"_carplay-ctrl") || contains(pkt, CTRL_HOSTLABEL.as_bytes())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Number of valid `/ctrl-int/1/connect` invites the car has sent us.
///
/// The reference transmitter only connects to the car's AirPlay receiver *after*
/// this invite; we connect as soon as mDNS discovery yields an endpoint. Whether
/// the invite ever arrives decides which of those two behaviours is the bug, so
/// it is counted and reported at every AirPlay session start.
static INVITES: AtomicU64 = AtomicU64::new(0);

/// How many valid control-channel invites the car has sent this process.
pub fn invites_received() -> u64 {
    INVITES.load(Ordering::Relaxed)
}

async fn serve_ctrl(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((mut sock, peer)) => {
                info!("ctrl-advertise: car opened the control channel from {peer}");
                let _ = sock.set_nodelay(true);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first = req.lines().next().unwrap_or("");
                    // The invite: `GET /ctrl-int/1/connect` carrying the car's
                    // `AirPlay-Receiver-Device-ID`. Per the reference we 200 ONLY
                    // when that header is present (else 404), then CLOSE the socket
                    // (one-shot invite — do NOT hold it open).
                    let has_devid = req
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("airplay-receiver-device-id:"));
                    let is_connect = first.contains("/ctrl-int/1/connect");
                    for l in req.lines().take(12) {
                        if l.is_empty() {
                            break;
                        }
                        info!("ctrl-advertise:   car> {l}");
                    }
                    let resp: &[u8] = if is_connect && has_devid {
                        let n = INVITES.fetch_add(1, Ordering::Relaxed) + 1;
                        info!("ctrl-advertise: valid /ctrl-int/1/connect invite #{n} → 200 OK, closing (session start)");
                        // Bare status line with NO Server/Content-Length/Connection
                        // headers (the reply the reference implementation expects). A
                        // strict car-side parser can abort on unexpected headers.
                        b"HTTP/1.1 200 OK\r\n\r\n"
                    } else {
                        warn!("ctrl-advertise: /ctrl-int request missing device-id or path → 404");
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    };
                    let _ = sock.write_all(resp).await;
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await; // one invite per connection, then close
                });
            }
            Err(e) => {
                warn!("ctrl-advertise: accept error: {e}");
                break;
            }
        }
    }
}

/// Publish `_carplay-ctrl._tcp` with avahi instead of our raw responder.
///
/// Keys are exactly Apple's Table 23-20 ("CarPlay Control Bonjour Service Keys"):
/// `id` (the device's Bluetooth MAC) and `srcvers`. Nothing else is specified, so
/// nothing else is sent. Best-effort: a failure here just leaves us unadvertised,
/// which is the state we are already in.
fn publish_via_avahi(ifname: &str, addr_v4: Option<Ipv4Addr>) {
    let instance =
        std::env::var("RASCAR_CTRL_INSTANCE").unwrap_or_else(|_| CTRL_INSTANCE.to_string());
    let srcvers =
        std::env::var("RASCAR_CTRL_SRCVERS").unwrap_or_else(|_| CTRL_SRCVERS.to_string());
    let id = std::env::var("RASCAR_CTRL_DEVICE_ID").unwrap_or_else(|_| CTRL_DEVICE_ID.to_string());
    info!(
        "ctrl-advertise: publishing _carplay-ctrl via AVAHI on {ifname} (A {addr_v4:?}) \
         instance='{instance}' id={id} srcvers={srcvers} — raw responder DISABLED for this run"
    );
    match std::process::Command::new("avahi-publish-service")
        .args([
            &instance,
            "_carplay-ctrl._tcp",
            &CTRL_PORT.to_string(),
            &format!("id={id}"),
            &format!("srcvers={srcvers}"),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // Deliberately leaked: avahi-publish-service must live for the whole
        // session — the advertisement is withdrawn the moment it exits.
        Ok(child) => {
            info!("ctrl-advertise: avahi-publish-service running (pid {})", child.id());
            std::mem::forget(child);
        }
        Err(e) => warn!("ctrl-advertise: could not start avahi-publish-service: {e}"),
    }
}
