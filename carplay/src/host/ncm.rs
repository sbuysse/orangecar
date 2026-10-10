//! CDC-NCM link bring-up + IPv6 link-local + Bonjour/mDNS discovery.
//!
//! Once the car is a USB host-side device exposing a CDC-NCM interface, the
//! kernel's `cdc_ncm` driver binds it and creates a network interface (typically
//! `usb0` or `enx<MAC>`).  The AirPlay session (Phase 4/5) runs over IPv6
//! link-local on this interface, and the car advertises its CarPlay receiver
//! via Bonjour/mDNS under `_airplay._tcp` (the only type we browse, see
//! [`MDNS_SERVICES`]).
//!
//! This module is deliberately **shell-driven** (`modprobe`, `ip`, `avahi-browse`)
//! rather than raw netlink: it is best-effort plumbing that must be observable in
//! logs on the first real-car run, and the tools are already on the Pi image.
//!
//! # Assumptions
//! - The NCM netif is the one whose sysfs device sits under the USB device the
//!   car enumerated as, or else a USB-backed netif that appeared after our
//!   snapshot (see `find_ncm_interface`); a stale gadget/dongle netif that cannot
//!   be attributed to the car is never used.
//! - Our IPv6 link-local comes from the kernel (no DHCP, no router
//!   advertisements on this link).
//! - `avahi-daemon` must be running for discovery.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

/// Service types the car's CarPlay/AirPlay receiver is expected to advertise.
// Only `_airplay._tcp` — the car advertises CarPlay under it, and browsing the
// empty `_carplay._tcp` too costs ~1 s we cannot spare before the car's ~7.6 s
// USB teardown.
const MDNS_SERVICES: &[&str] = &["_airplay._tcp"];

/// A brought-up NCM network link.
#[derive(Debug, Clone)]
pub struct NcmLink {
    /// Kernel network interface name (e.g. `usb0`, `enxAABBCCDDEEFF`).
    pub ifname: String,
    /// IPv6 link-local address (`fe80::…%ifname`), if SLAAC produced one.
    pub link_local: Option<String>,
}

/// A discovered AirPlay/CarPlay receiver endpoint (from mDNS).
#[derive(Debug, Clone)]
pub struct AirPlayEndpoint {
    pub service_type: String,
    pub name: String,
    pub host: String,
    pub address: String,
    pub port: u16,
    /// Kernel interface the record was seen on (avahi field 1). An IPv6
    /// link-local address is only routable together with a scope, so the RTSP
    /// dial needs this; without it `fe80::…` cannot be connected to at all.
    pub interface: String,
}

/// Ensure the `cdc_ncm` driver is loaded, find the car's NCM netif, bring it up,
/// and confirm an IPv6 link-local address.
///
/// `car_sysfs` is the sysfs path of the USB device the car just enumerated as.
/// It is what lets us prove a candidate netif really belongs to the car rather
/// than to a leftover gadget `usb0` or a plugged-in Ethernet dongle.
pub async fn bringup(
    pre_existing_ifaces: &HashSet<String>,
    car_sysfs: Option<&Path>,
) -> Result<NcmLink> {
    info!("ncm: ensuring cdc_ncm kernel driver is loaded");
    match Command::new("modprobe").arg("cdc_ncm").status().await {
        Ok(s) if s.success() => debug!("ncm: modprobe cdc_ncm OK"),
        Ok(s) => warn!("ncm: modprobe cdc_ncm exited {s} (may be built-in — continuing)"),
        Err(e) => warn!("ncm: could not run modprobe cdc_ncm: {e} (continuing)"),
    }

    // Find the NCM netif: the first USB-backed interface that was NOT present
    // before the role switch (so we don't grab the stale gadget `usb0`).
    let ifname = find_ncm_interface(pre_existing_ifaces, car_sysfs)
        .await
        .context("locating the car's CDC-NCM network interface")?;
    info!("ncm: selected network interface '{ifname}' (t+{} ms)", crate::debug::elapsed_ms());
    info!("ncm: netif appeared: {}", describe_netif(&ifname).await);

    // Bring the link up the way a phone-side CarPlay stack needs it (the sysctl
    // set matches the one catplay-labs/catplay applies to its NCM interface).
    // Two groups, both best-effort (nodes may not exist):
    //
    // (1) DAD off + no router autoconfig. With DAD on, our fe80 stays "tentative"
    //     for ~1 s and dialing the car's link-local AirPlay :7000 fails with
    //     EADDRNOTAVAIL. accept_ra/autoconf/router_solicitations off keeps the link
    //     pure static link-local (the car runs no RA; SLAAC of a global is noise).
    // (2) Neighbour-discovery tuning. Without it a link-local TCP connect to the
    //     car may succeed at once or keep timing out for up to ~20 s while ND
    //     backs off — the "car resolves the service but the dial times out"
    //     failure class. delay_first_probe_time=0 + retrans_time_ms=100 +
    //     mcast_solicit=100 make our ND answer the car's NS immediately and retry
    //     hard instead of backing off past the car's ~7.8 s USB budget.
    for (knob, val) in [
        ("accept_dad", "0"),
        ("dad_transmits", "0"),
        ("optimistic_dad", "1"),
        ("accept_ra", "0"),
        ("autoconf", "0"),
        ("router_solicitations", "0"),
        // NOT addr_gen_mode=1 ("none"): that only suits a stack that assigns
        // its own fixed link-local, and we rely on the kernel's. Until 2026-09-25 systemd-networkd created
        // ours regardless; once the car link became Unmanaged=yes, nothing did
        // and all 6 attempts of one drive stalled waiting for it.
        ("addr_gen_mode", "0"),
    ] {
        let path = format!("/proc/sys/net/ipv6/conf/{ifname}/{knob}");
        if let Err(e) = tokio::fs::write(&path, val).await {
            debug!("ncm: could not set {path}={val}: {e}");
        }
    }
    for (knob, val) in [
        ("delay_first_probe_time", "0"),
        ("retrans_time_ms", "100"),
        ("mcast_solicit", "100"),
    ] {
        let path = format!("/proc/sys/net/ipv6/neigh/{ifname}/{knob}");
        if let Err(e) = tokio::fs::write(&path, val).await {
            debug!("ncm: could not set {path}={val}: {e}");
        }
    }

    // Bring it up.
    run_ip(&["link", "set", &ifname, "up"]).await?;
    info!("ncm: '{ifname}' set up");

    // One identity on the car link, like a real iPhone: systemd-resolved runs
    // LLMNR on every link (it probed "orangepi5-plus" over it on 2026-09-25) even
    // when networkd leaves the link unmanaged. Per-link, best effort.
    for (proto, arg) in [("llmnr", "no"), ("mdns", "no")] {
        match Command::new("resolvectl").args([proto, &ifname, arg]).status().await {
            Ok(st) if st.success() => debug!("ncm: resolvectl {proto} {ifname} {arg}"),
            Ok(st) => debug!("ncm: resolvectl {proto} {ifname} {arg} exited {st}"),
            Err(e) => debug!("ncm: could not run resolvectl: {e}"),
        }
    }

    // Give the link an IPv4. The car runs a DHCP server at 192.168.0.1 on the NCM
    // link and browses `_carplay-ctrl` from that IPv4; without an IPv4 of our own
    // we cannot answer its IPv4 mDNS queries (and the car uses that path). Static-
    // assign the address the car offers (192.168.0.2/24); harmless if it already
    // exists. Without it `_carplay-ctrl` never resolves over IPv4.
    if let Err(e) = run_ip(&["addr", "add", "192.168.0.2/24", "dev", &ifname]).await {
        debug!("ncm: could not add IPv4 192.168.0.2/24 on '{ifname}' (may already exist): {e}");
    } else {
        info!("ncm: assigned IPv4 192.168.0.2/24 on '{ifname}'");
    }

    // Poll for an IPv6 link-local (SLAAC is near-instant but allow slack).
    let link_local = wait_for_link_local(&ifname, Duration::from_secs(10)).await;
    match &link_local {
        Some(ll) => info!("ncm: IPv6 link-local up on '{ifname}': {ll}"),
        None => warn!("ncm: no IPv6 link-local on '{ifname}' after 10s — // TODO(hw): confirm SLAAC / IPv6 enabled on the link"),
    }
    info!("ncm: netif after link-local wait: {}", describe_netif(&ifname).await);

    Ok(NcmLink { ifname, link_local })
}

/// Best-effort one-line description of a netif: MAC, operstate, backing device
/// and every address `ip` reports. Never fails.
async fn describe_netif(ifname: &str) -> String {
    use crate::debug::read_attr;
    let base = format!("/sys/class/net/{ifname}");
    let device = std::fs::read_link(format!("{base}/device"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("<{e}>"));
    let addrs = match Command::new("ip").args(["-o", "addr", "show", "dev", ifname]).output().await {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let list: Vec<String> = text
                .lines()
                .filter_map(|l| {
                    // "3: enx… inet6 fe80::…/64 scope link …" → "inet6 fe80::…/64"
                    let mut it = l.split_whitespace().skip(2);
                    Some(format!("{} {}", it.next()?, it.next()?))
                })
                .collect();
            if list.is_empty() { "<none>".to_string() } else { list.join(", ") }
        }
        Err(e) => format!("<ip failed: {e}>"),
    };
    format!(
        "name={ifname} mac={} operstate={} carrier={} driver_dev={device} addrs=[{addrs}]",
        read_attr(format!("{base}/address")),
        read_attr(format!("{base}/operstate")),
        read_attr(format!("{base}/carrier")),
    )
}

/// Snapshot the set of current network interface names (call BEFORE the role
/// switch so [`bringup`] can tell which one is new).
pub fn snapshot_interfaces() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            if let Ok(name) = e.file_name().into_string() {
                set.insert(name);
            }
        }
    }
    debug!("ncm: pre-existing interfaces: {set:?}");
    set
}

/// True when `<ifname>`'s backing device lives under `car_sysfs`, i.e. the netif
/// is really a child of the USB device the car enumerated as.
fn netif_belongs_to_device(ifname: &str, car_sysfs: &Path) -> bool {
    let dev = std::fs::canonicalize(format!("/sys/class/net/{ifname}/device"));
    let car = std::fs::canonicalize(car_sysfs);
    match (dev, car) {
        (Ok(d), Ok(c)) => d.starts_with(&c),
        _ => false,
    }
}

/// Find a USB-backed netif that appeared after the role switch. Prefers a name
/// that is genuinely new; falls back to the only USB netif ONLY when it can be
/// proven to belong to the car's USB device — a warm fallback onto a leftover
/// gadget `usb0` would let Phase 3 report a network link the car is not on.
async fn find_ncm_interface(
    pre_existing: &HashSet<String>,
    car_sysfs: Option<&Path>,
) -> Result<String> {
    // Re-scan a few times: the netif can lag the USB enumeration slightly.
    for attempt in 1..=10u32 {
        let mut usb_ifaces: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
            for e in rd.flatten() {
                let name = match e.file_name().into_string() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if name == "lo" {
                    continue;
                }
                // A netif that sits under the car's USB device is the answer,
                // whatever udev renamed it to. Measured at the car: cdc_ncm
                // registers `usb0` and udev renames it `enu1i1` ~230 ms later.
                if let Some(car) = car_sysfs {
                    if netif_belongs_to_device(&name, car) {
                        info!("ncm: '{name}' is backed by the car's USB device ({}) — selected", car.display());
                        return Ok(name);
                    }
                }
                // Is it USB-backed? The `device` symlink is RELATIVE
                // (`../../../7-1:1.1`), so resolve it: only the absolute path
                // names the `usb` bus.
                let dev_link = format!("/sys/class/net/{name}/device");
                let is_usb = std::fs::canonicalize(&dev_link)
                    .map(|t| t.to_string_lossy().contains("/usb"))
                    .unwrap_or(false)
                    || name.starts_with("usb")
                    || name.starts_with("enx");
                if is_usb {
                    usb_ifaces.push(name);
                }
            }
        }
        debug!("ncm: scan #{attempt} USB netifs: {usb_ifaces:?}");

        // Prefer one that is new since the snapshot.
        if let Some(fresh) = usb_ifaces.iter().find(|n| !pre_existing.contains(*n)) {
            return Ok(fresh.clone());
        }
        // Otherwise the only USB netif — but only if it is demonstrably the
        // car's. Without that proof this is the stale gadget/dongle trap.
        if usb_ifaces.len() == 1 {
            match car_sysfs {
                Some(car) if netif_belongs_to_device(&usb_ifaces[0], car) => {
                    warn!(
                        "ncm: no *new* USB netif, but '{}' is backed by the car's USB device ({}) — using it",
                        usb_ifaces[0],
                        car.display()
                    );
                    return Ok(usb_ifaces[0].clone());
                }
                Some(car) => debug!(
                    "ncm: only USB netif '{}' is NOT under the car's USB device ({}) — not a fallback candidate",
                    usb_ifaces[0],
                    car.display()
                ),
                None => debug!(
                    "ncm: only USB netif '{}' cannot be attributed to the car (no device path) — not a fallback candidate",
                    usb_ifaces[0]
                ),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "ncm: could not identify the car's NCM network interface — no new USB netif appeared, and          no existing one is backed by the car's USB device. // TODO(hw): check `ip link` and          `ls -l /sys/class/net/*/device` on the board with the car attached."
    )
}

async fn wait_for_link_local(ifname: &str, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(ll) = read_link_local(ifname).await {
            return Some(ll);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        // Poll fast — DAD is disabled on this iface, so the fe80 appears within a
        // few tens of ms; a 500 ms cadence wasted up to half a second of the car's
        // ~7.8 s USB-teardown budget.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Parse `ip -6 addr show dev <if>` for an `fe80::` scope-link address.
async fn read_link_local(ifname: &str) -> Option<String> {
    let out = Command::new("ip")
        .args(["-6", "addr", "show", "dev", ifname, "scope", "link"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inet6 ") {
            if rest.starts_with("fe80") {
                // "fe80::.../64" → strip prefix len, add scope.
                let addr = rest.split_whitespace().next().unwrap_or(rest);
                let addr = addr.split('/').next().unwrap_or(addr);
                return Some(format!("{addr}%{ifname}"));
            }
        }
    }
    None
}

async fn run_ip(args: &[&str]) -> Result<()> {
    let status = Command::new("ip")
        .args(args)
        .status()
        .await
        .with_context(|| format!("running `ip {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`ip {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Browse mDNS/Bonjour for the CarPlay/AirPlay receiver via `avahi-browse`.
///
/// Returns every resolved endpoint found across the known service types within
/// `timeout`.  Non-fatal: an empty result is logged, not an error, because the
/// car may only advertise once the AirPlay/iAP2 handshake is further along.
/// `ifname` scopes the results to the NCM link: `avahi-browse` has no interface
/// selector, but it reports the answering interface in field 1 of each record,
/// so responders on other links (the board's own WiFi/Ethernet) are dropped.
pub async fn discover_airplay(timeout: Duration, ifname: Option<&str>) -> Vec<AirPlayEndpoint> {
    let mut found: Vec<AirPlayEndpoint> = Vec::new();
    for service in MDNS_SERVICES {
        match browse_service(service, timeout, ifname).await {
            Ok(mut eps) => {
                if eps.is_empty() {
                    debug!("mdns: no resolved records for {service}");
                } else {
                    info!("mdns: {} endpoint(s) for {service}", eps.len());
                    found.append(&mut eps);
                }
            }
            Err(e) => warn!("mdns: browsing {service} failed: {e:#} (is avahi-daemon running?)"),
        }
    }
    // Prefer the car's IPv6 link-local, scoped to the NCM interface. Verified at
    // the car: avahi resolves the AirPlay host to its IPv4 (192.168.0.1) but the
    // car only ACCEPTS TCP :7000 on its IPv6 link-local; a dial to the v4 address
    // times out. The car's link-local answers our neighbour discovery, so read it
    // from the neighbour table and rewrite the endpoint to use it (rtsp.rs scopes
    // a link-local address with the interface).
    if let Some(iface) = ifname {
        if let Some(car_ll) = car_link_local(iface).await {
            for ep in &mut found {
                if !ep.address.starts_with("fe80") {
                    debug!("mdns: rewriting {} address {} -> {} (v6 link-local on {iface})", ep.name, ep.address, car_ll);
                    ep.address = car_ll.clone();
                    ep.interface = iface.to_string();
                }
            }
        } else {
            warn!("mdns: no car IPv6 link-local found on {iface} — dialing the advertised address as-is (v4 may not accept)");
        }
    }

    if found.is_empty() {
        warn!("mdns: no AirPlay/CarPlay endpoints discovered — // TODO(hw): confirm avahi-daemon is up and the car advertises on the NCM link");
    } else {
        for ep in &found {
            info!("mdns: discovered {} '{}' at [{}]:{} (host {})", ep.service_type, ep.name, ep.address, ep.port, ep.host);
        }
    }
    found
}

/// The car's IPv6 link-local on the NCM link, from the neighbour table.
///
/// The car answers our neighbour discovery, so its `fe80::…` shows up in
/// `ip -6 neigh show dev <iface>`. Pings it first to populate the table. Returns
/// the first `fe80::` neighbour (the car is the only other node on this link).
pub(crate) async fn car_link_local(iface: &str) -> Option<String> {
    // The car only accepts AirPlay TCP :7000 on its IPv6 link-local, so we MUST
    // learn it — a miss here leaves only the car's v4 address, which refuses the
    // connection and wastes a whole ~7.8 s attempt on a 3 s connect timeout
    // (measured: on a cold boot the neigh table is empty and a single ping did not
    // populate it in time → every other attempt fell back to v4). Nudge with a
    // multicast ping, then poll the neigh table for up to ~1.6 s until a usable
    // fe80 appears. Cheap when it is already cached; decisive when it is not.
    for attempt in 0..8u32 {
        // Re-nudge every couple of tries in case the first solicitation was lost.
        if attempt % 3 == 0 {
            let _ = Command::new("ping")
                .args(["-6", "-c", "1", "-W", "1", "-i", "0.2", "-I", iface, "ff02::1"])
                .output()
                .await;
        }
        if let Ok(out) = Command::new("ip")
            .args(["-6", "neigh", "show", "dev", iface])
            .output()
            .await
        {
            let text = String::from_utf8_lossy(&out.stdout);
            // Prefer a resolved neighbour; skip FAILED entries.
            if let Some(ll) = text
                .lines()
                .find(|l| l.starts_with("fe80") && !l.contains("FAILED"))
                .and_then(|l| l.split_whitespace().next())
            {
                return Some(ll.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Run `avahi-browse -rptk <service>` and parse the resolved (`=`) records.
///
/// avahi terse/parsable format resolved line:
///   `=;<iface>;<proto>;<name>;<type>;<domain>;<host>;<address>;<port>;<txt>`
async fn browse_service(
    service: &str,
    timeout: Duration,
    ifname: Option<&str>,
) -> Result<Vec<AirPlayEndpoint>> {
    let args = ["-r", "-p", "-t", "-k", service];
    info!(
        "mdns: running `avahi-browse {}` (timeout {timeout:?}, iface filter {ifname:?}, t+{} ms)",
        args.join(" "),
        crate::debug::elapsed_ms()
    );
    let started = std::time::Instant::now();
    let child = Command::new("avahi-browse")
        .args(args)
        .stdout(Stdio::piped())
        // Captured (was discarded) so a daemon/permission error lands in the log.
        .stderr(Stdio::piped())
        // Without this, the timeout path below drops the future and leaves the
        // browse running detached — two orphans per discovery attempt.
        .kill_on_drop(true)
        .spawn()
        .with_context(|| "spawning avahi-browse (is avahi-utils installed?)")?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(res) => res.context("waiting for avahi-browse")?,
        Err(_) => {
            warn!("mdns: avahi-browse for {service} exceeded {timeout:?}, treating as no-result (child killed, output lost)");
            return Ok(Vec::new());
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    info!(
        "mdns: avahi-browse {service} exited {} after {} ms ({} stdout line(s), {} resolved '=' record(s), {} stderr byte(s))",
        output.status,
        started.elapsed().as_millis(),
        text.lines().count(),
        text.lines().filter(|l| l.starts_with('=')).count(),
        output.stderr.len()
    );
    debug!(
        "mdns: avahi-browse {service} raw stdout:\n{}",
        crate::debug::truncate_text(&text, 4096)
    );
    if !stderr.trim().is_empty() {
        debug!(
            "mdns: avahi-browse {service} raw stderr:\n{}",
            crate::debug::truncate_text(&stderr, 4096)
        );
    }
    let mut eps = Vec::new();
    for line in text.lines() {
        if !line.starts_with('=') {
            continue; // only fully-resolved records
        }
        let f: Vec<&str> = line.split(';').collect();
        // =;iface;proto;name;type;domain;host;address;port;txt
        if f.len() < 9 {
            debug!("mdns: skipping short avahi record: {line}");
            continue;
        }
        if let Some(want) = ifname {
            if f[1].trim() != want {
                debug!("mdns: ignoring record on interface '{}' (want '{want}'): {line}", f[1].trim());
                continue;
            }
        }
        let port = f[8].trim().parse::<u16>().unwrap_or(0);
        let interface = f[1].trim().to_string();
        eps.push(AirPlayEndpoint {
            interface,
            service_type: service.to_string(),
            name: unescape_avahi(f[3]),
            host: f[6].to_string(),
            address: f[7].to_string(),
            port,
        });
    }
    Ok(eps)
}

/// avahi escapes spaces/specials as `\032` etc. in the parsable name field.
///
/// A truncated escape at the end of the field (`"Car\\03"`) must be passed
/// through literally, never sliced past the end: this parses attacker-adjacent
/// data (anything advertising on the NCM link) and a panic here would take down
/// discovery, and with it the whole handshake.
fn unescape_avahi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Needs a backslash plus exactly three digits still inside the slice.
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            if let Ok(code) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .unwrap_or("")
                .parse::<u8>()
            {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_avahi_decodes_escapes() {
        assert_eq!(unescape_avahi("My\\032Car"), "My Car");
        assert_eq!(unescape_avahi("plain"), "plain");
        assert_eq!(unescape_avahi(""), "");
    }

    /// Regression: a truncated `\NNN` at the end of the field used to slice past
    /// the end of the byte array and panic the discovery task.
    #[test]
    fn unescape_avahi_survives_truncated_escapes() {
        assert_eq!(unescape_avahi("Car\\03"), "Car\\03");
        assert_eq!(unescape_avahi("Car\\0"), "Car\\0");
        assert_eq!(unescape_avahi("Car\\"), "Car\\");
        assert_eq!(unescape_avahi("\\"), "\\");
        // Backslash followed by non-digits stays literal.
        assert_eq!(unescape_avahi("Car\\abc"), "Car\\abc");
        // 256 does not fit in a u8 → left literal rather than wrapped.
        assert_eq!(unescape_avahi("Car\\256"), "Car\\256");
    }
}
