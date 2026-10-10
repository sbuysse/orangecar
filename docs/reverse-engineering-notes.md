# Reverse-engineering notes

This page explains how the facts in [protocol.md](protocol.md) were found, and how to debug your own car
with the same tools. Nothing here was given to us: no Apple documentation of the phone side, no head-unit
logs. Everything came from capturing what the car does, comparing it with other implementations, and
changing one thing at a time.

- [1. Principles that saved the most time](#1-principles-that-saved-the-most-time)
- [2. Capturing the device phase: dwc3 tracepoints](#2-capturing-the-device-phase-dwc3-tracepoints)
- [3. Capturing the host phase: usbmon and the network pcap](#3-capturing-the-host-phase-usbmon-and-the-network-pcap)
- [4. Inside the transmitter: wire trace and phase timers](#4-inside-the-transmitter-wire-trace-and-phase-timers)
- [5. The per-attempt wrapper and the capture archive](#5-the-per-attempt-wrapper-and-the-capture-archive)
- [6. Python probes: fast experiments before writing Rust](#6-python-probes-fast-experiments-before-writing-rust)
- [7. Offline analysis: finding the car's clocks](#7-offline-analysis-finding-the-cars-clocks)
- [8. Reference implementations and firmware](#8-reference-implementations-and-firmware)
- [9. Paths that did not work](#9-paths-that-did-not-work-android-auto-myspin-in-band-iap2)
- [10. Debugging your own car: a checklist](#10-debugging-your-own-car-a-checklist)
- [11. Timeline](#11-timeline)

---

## 1. Principles that saved the most time

- **The car is the ground truth.** Other implementations, Apple's documents and our own earlier notes were
  all wrong somewhere. When a reference says one thing and the car does another, the car wins.
- **Car attempts are scarce and they degrade the head unit.** Repeated failures fill its paired-controller
  list, and the CarPlay stack can wedge (see the [role-switch latch](protocol.md#73-the-role-switch-latch-car-stops-sending-40-51)).
  So one binary carries every experiment behind a `RASCAR_*` environment switch. A test drive runs A/B
  arms without a rebuild.
- **Every A/B keeps one arm equal to the last known-good configuration.** One drive had both arms broken,
  and it was impossible to tell which change was at fault.
- **Never quote a timing from one run.** Profile every run, then look for the event whose interval to the
  failure has the lowest variance. That is how the car's clocks were found (section 7).
- **Measure against the kernel's USB disconnect**, not a proxy. The car's 1 Hz GPS burst looked like a
  good "car is alive" signal. It is quantised to 1 s and produced a false "10-sigma improvement".
- **Write down refuted hypotheses.** Half of [protocol.md](protocol.md)'s value is in its "what does NOT
  matter" lists.

---

## 2. Capturing the device phase: dwc3 tracepoints

Before the role switch, the board is a USB *device*, and usbmon sees nothing. FunctionFS only forwards the
requests the kernel does not answer itself. So a FunctionFS log will never show the car asking for a
configuration descriptor index you do not have.

The dwc3 controller's tracepoints show **every** SETUP packet, including the standard ones the kernel
answers on its own:

```bash
mount -t debugfs none /sys/kernel/debug 2>/dev/null
cd /sys/kernel/debug/tracing
echo 8192 > buffer_size_kb
for ev in dwc3_ctrl_req dwc3_event dwc3_gadget_ep_cmd; do
  echo 1 > events/dwc3/$ev/enable
done
echo 1 > tracing_on
cat trace_pipe >> /var/lib/rascar-debug/dwc3-trace.log
```

This is how we learned that the first gadget (single configuration, full speed) was stuck in *addressed*,
while the car tried to read configuration index 4. A real iPhone has four configurations, and the fix
followed directly.

Useful state files while in device mode:

| File | What it tells you |
|---|---|
| `/sys/class/udc/fc000000.usb/state` | `not attached`, `default`, `addressed`, `configured`, … |
| `/sys/class/udc/fc000000.usb/current_speed` | must be `high-speed` |
| `/sys/kernel/debug/usb/fc000000.usb/mode` | `device` / `host` (the role-switch knob) |
| `/sys/kernel/debug/usb/fc000000.usb/link_state` | U0/U3 etc. |
| `/sys/class/usb_role/*/role`, `/sys/class/typec/*` | what tcpm thinks the role is |

`setup/carplay-debug/rascar-debugcap` runs all of this from boot and logs a one-line fingerprint of these
files every second (a snapshot is written only on change, plus a heartbeat every 60 s).

---

## 3. Capturing the host phase: usbmon and the network pcap

After the role switch the car is a USB device on the board's host controller, and the normal tools work.

```bash
modprobe usbmon
# every bus, including buses created after the capture starts (the role switch creates a new one):
tcpdump -Z root -i usbmon0 -s 0 -U -w usb.pcap -C 100 -W 4 &
# the NCM interface appears mid-run, so capture on "any":
tcpdump -Z root -i any -s 0 -U -w net.pcap -C 100 -W 4 &
```

What these captures revealed:

- The car's device descriptor `8086:9999` "Gadget MFi v0.1", its single configuration, and the CDC-NCM
  `iMACAddress` pointing at a non-MAC string.
- The 64-byte string descriptor sent without a zero-length packet. That explained a 4 s hang, then `-71`,
  in cdc_ncm.
- That the 6 s cutoff was **not** a power event: `PORT_POWER=1`, `OVER_CURRENT=0`, and a clean
  `CONNECTION 1→0`.
- Bulk-OUT URBs that took up to 1.4 s to complete, each right after a cdc_ncm `SET_INTERFACE`.
- The car's mDNS browse for `_carplay-ctrl`, and that it never opens TCP 7100.
- That the car's TCP 7000 answers only on its IPv6 link-local.

Tips:

- In Wireshark, match usbmon *submit* and *complete* records by URB id to get per-transfer latency. A bulk
  OUT that takes more than 100 ms means the car stopped draining its endpoint.
- `sll.pkttype == 0` in tshark keeps only frames addressed to us on an `any` capture.
- **Do not run `lsusb -v` against the car.** It reads string descriptors with a 255-byte request from
  userspace and wedges the car for about 10 s
  ([why](protocol.md#16-never-read-the-cars-strings-with-libusb)).

---

## 4. Inside the transmitter: wire trace and phase timers

`carplay/src/debug.rs` provides the instrumentation used by the whole transmitter:

| Facility | How to enable | What you get |
|---|---|---|
| Wire trace | `RASCAR_WIRE_TRACE=1` | byte-level dumps of USB bulk IN/OUT (`host/usb_host.rs`), every iAP2 link packet with decoded control bits, seq and ack (`iap2/link.rs`, `iap2/packet.rs` `describe_raw`), every iAP2 control message (`iap2/control.rs`), and RTSP bodies (`airplay/rtsp.rs`) |
| Phase timers | always on | `Phase X START` / `END ok\|err in N ms` lines |
| Elapsed stamps | always on | `t+<ms>` from process start, monotonic, so logs line up even when the wall clock is wrong |
| Sampling | always on | rate-limits hot-path traces (video frames) |

Wire-trace lines look like this:

```
usb bulk IN ep 0x81 len=26:
  0000  ff 5a 00 1a 80 18 00 00 f5 01 05 10 00 01 f4 00  |.Z..............|
  0010  b0 1e 0a 01 00 02 02 01 01 16                    |..........|
iap2 link -> sop=ok len=26 ctrl=0xc0 [SYN|ACK] port_type=SynAck seq=0x00 ack=0x18 session=0x00 payload_len=16 header_ck=ok payload_ck=ok
```

The output goes to the normal tracing log: the journal, plus the per-attempt run log (section 5). Put
switches in `/etc/rascar/carplay.env`; the run wrapper sources it, so manual runs and service runs are
instrumented the same way.

Other useful switches:

- `RUST_LOG=debug` for more detail.
- `--mdns-selftest <fe80::…%if>` runs only the `_carplay-ctrl` responder for 30 s.
- `--carplay-endpoint HOST:PORT` runs the AirPlay session against a bench receiver, with no USB at all.

---

## 5. The per-attempt wrapper and the capture archive

### Why the capture must live on the NVMe

On Armbian, `/var/log` and the journal live in zram, and the car cuts power at ignition-off without
warning. Everything in RAM is lost, typically the very attempt you care about. All evidence therefore
goes to `/var/lib/rascar-debug/` on the NVMe.

### `rascar-debugcap` (one directory per boot)

`setup/carplay-debug/rascar-debugcap.service` starts at boot and writes
`/var/lib/rascar-debug/boot-<date>-<time>-<bootid>/`. A `latest` symlink points at the newest; the last 20
boots are kept.

| File | Content |
|---|---|
| `kernel.log` | `dmesg --time-format iso --follow` (ISO for sub-second ordering) |
| `udev.log` | `udevadm monitor --kernel --udev --property` |
| `ip-monitor.log` | `ip -timestamp monitor all` |
| `avahi.log` | `avahi-browse --all --resolve --parsable` |
| `journal.log` | `journalctl -f -o short-precise` for the CarPlay, app and avahi units |
| `usbmon.pcap` | ring buffer of all USB buses |
| `dwc3-trace.log` | the tracepoints from section 2 |
| `state.log` | the per-second state fingerprint |
| `runs/` | one evidence set per attempt (below) |

The boot-directory date comes from a stale `fake-hwclock` until the car's GPS sets the clock
([protocol.md 3.5.2](protocol.md#352-car-gps-and-gear-0xfffb)). Correlate by boot id and by the
monotonic `t+` stamps, not by wall time.

### `rascar-carplay-run` (one evidence set per attempt)

systemd re-runs the transmitter after every exit (`Restart=always`, `RestartSec=20`: re-role-switching the car every 5 s wore out its CarPlay daemon). The wrapper gives each
attempt a number and writes:

- `runs/NNN-HHMMSS-carplay.log`. Its header records the binary's sha256, the build id, the wire-trace flag
  and a `VARIANT:` line listing every A/B switch, so a run can always be traced back to its configuration.
- `runs/NNN-HHMMSS-net.pcap`, from `tcpdump -i any`.
- `runs/NNN-HHMMSS-usb.pcap`, from `tcpdump -i usbmon0`, time-limited by `RASCAR_CAPTURE_SECONDS`
  (default 60). Writing more than 1 MB/s of pcap on car power is not free.

Before each attempt it also resets the board to a known state:

- dwc3 back to `device`, fusb302 re-bound;
- the patched `cdc_ncm.ko` loaded, with a version check;
- IPv6 DAD off and `drivers_autoprobe=1`;
- a CPU cap applied (`RASCAR_CPU_MAX_KHZ`).

The header block expands every variable as `${x:-unset}`. A `set -u` failure once silently removed the
`VARIANT:` line from every run of a drive.

### Other helpers in `setup/carplay-debug/`

| Script | Purpose |
|---|---|
| `rascar-preflight` | PASS/WARN/FAIL checks: UDC present, no leftover gadget or FunctionFS mount, no conflicting service on the UDC, dwc3 mode, binary and build id |
| `rascar-bundle [N]` | tars the last N boot directories + journal + preflight + versions into one file to copy off the board |
| `rascar-hotspot` | starts an access point if no Ethernet appears within 45 s, so you can SSH into the board in the car |

`/sys/class/udc/fc000000.usb/soft_connect` (`disconnect` / `connect`) forces the car to re-enumerate
without touching the cable. Use it sparingly: see the latch in protocol.md 7.3.

---

## 6. Python probes: fast experiments before writing Rust

Each new layer was first poked with a throw-away Python script, then implemented in Rust once the car's
behaviour was known. The scripts are not in this repository; what they did is easy to reproduce.

- **Multi-configuration FunctionFS gadget.** One FunctionFS instance per configuration, FS+HS
  descriptors, and an `ep0` event loop that logs every SETUP. Three runs found the minimum set of things the
  car needs before it sends the role switch:
  1. The car read configurations 0–3, chose configuration 2 and asked for the HID report descriptor.
  2. With an iPod HID report descriptor served, the car sent **`40 51 wValue=1`** 1.29 s later.
  3. tcpm reverted the role; unbinding fusb302 fixed it.
- **pyusb iAP2 probe.** After the role switch, claim the car's interface `FF/F0/00` and just read. The car
  sent the detect bytes and then a SYN without any prompting, which settled who opens the link.
- **AirPlay control probe.** Register `_carplay-ctrl`, serve `/ctrl-int`, dial the car's port 7000.
  - IPv4 timed out.
  - IPv6 link-local failed with `EADDRNOTAVAIL` until DAD was disabled.
  - Then `GET /info` answered 200 with `Server: AirTunes/280.33.12.1`, and no MFi exchange was needed.
- **Bench receiver simulator.** A small RTSP/bplist receiver modelled on the car's `/info`, with options to
  reproduce the car's traps (for example the audio-ownership contradiction, or a delayed audio SETUP
  reply). It became the regression gate for the session layer without driving to the car.

FunctionFS layout details that are easy to get wrong:

```text
descriptors (write to ep0 first):
  u32 LE magic = 3 (FUNCTIONFS_DESCRIPTORS_MAGIC_V2)
  u32 LE total length
  u32 LE flags  = HAS_FS_DESC(1) | HAS_HS_DESC(2) | ALL_CTRL_RECIP(64) | CONFIG0_SETUP(128)
  u32 LE fs_count, u32 LE hs_count
  FS descriptors…, HS descriptors…
strings (write next):
  u32 LE magic = 2, u32 LE length, u32 LE str_count, u32 LE lang_count
  u16 LE lang (0x0409), then each string NUL-terminated
events (read from ep0): 12 bytes = 8-byte SETUP packet + u8 type + 3 padding
```

Without `ALL_CTRL_RECIP`, the device-recipient vendor requests (`C0 53`, `40 51`) never reach you.

---

## 7. Offline analysis: finding the car's clocks

After each drive, every run was profiled automatically into one table row:

| Source | Fields |
|---|---|
| run log | build, variant, HTTP 466 seen, frames sent, outcome |
| run log + kernel log | intervals from SYN, authentication, first frame and enumeration to the USB disconnect |
| net pcap | last car ACK, peak unacknowledged bytes, zero-window count, did the car dial us, mDNS browse and reply times |
| usbmon | bulk-OUT completions over 100 ms, longest one, USB errors |

The method that located the 6-second cutoff:

1. For each candidate start event (enumeration, SYN, authentication, first NCM packet, RECORD, first
   frame), compute the interval to the disconnect across all runs.
2. The event with the **lowest standard deviation** is the car's real anchor. "Auth to death" had sd 0.29 s
   where "first frame to death" had several times more.
3. Regress the remaining time against the length of the pre-auth stall. A slope near −1 means the clock
   started earlier.
4. Fit the decision model `D = max(enumeration + 3.82 s, auth + 2.14 s)` across 24 runs; 17 of them fit
   within ±70 ms.
5. With the decision point pinned, check what is on the wire at that instant on every plane. Nothing in
   iAP2 or AirPlay changed, which pointed back at the USB link itself. Disabling USB2 LPM fixed it
   ([protocol.md 7.1](protocol.md#71-the-6-second-cutoff)).

---

## 8. Reference implementations and firmware

| Source | What it gave us |
|---|---|
| [usenocturne/iap2-rs](https://github.com/usenocturne/iap2-rs) | A working iAP2 link layer in Rust (accessory side). Its packet and link code is the base of `carplay/src/iap2/`, inverted to the device side. |
| [omarroth/doubletake](https://github.com/omarroth/doubletake) | AirPlay sender building blocks: RTSP, binary plist, HomeKit pairing, stream ciphers, FairPlay (unused with this car). |
| [catplay-labs/catplay](https://github.com/catplay-labs/catplay) | An open CarPlay transmitter. Its source gave the legacy session order (no pairing, no FairPlay), the 128-byte screen frame, modes and resources, the RTCP timing reply and the ACK-only seq rule. |
| Commercial wireless CarPlay dongle (live USB capture + decrypted firmware) | The phone-side recipe used by a shipping product (detailed below). |
| An Android CarPlay box ROM | The same four-configuration recipe. It imports Apple's reference iAP2 link symbols, which confirmed the ACK-only sequence-number rule. |
| Public captures and logs | A dongle-to-head-unit USB capture (the SYN\|ACK counter-proposal); a production head-unit log (the post-auth burst of a real iPhone); [lvalen91/carplayd](https://github.com/lvalen91/carplayd) and [shilapi/xcertplay](https://github.com/shilapi/xcertplay) (iAP2 message ids and directions). |
| Published research | A university thesis showing that a dongle still works as a phone with its MFi chip removed, and academic work on CarPlay's one-directional trust (the accessory proves itself to the phone). Together these showed early on that no chip is needed on the phone side. |

What the commercial dongle showed:

- **Gadget:** `05AC:12A8` with four configurations, and the 208-byte HID report descriptor.
- **Role switch:** done by OTG HNP after `hnp_interval_ms` = 10 ms.
- **NCM:** a patched cdc_ncm with a 220 ms bind delay, NTB 12288/15224 and a MAC bypass. It loads
  cdc_ncm only once CarPlay starts.
- **Recovery:** it gives itself 6.000 s for the NCM link. It has a special case for Jaguar / Land Rover
  whose only action is to reboot the dongle when it wedges, and it retries iAP2 every 10 ms without
  reloading the gadget.

**Running catplay itself on the board** was also instructive:

- Its kernel-side role switch failed with `-EINVAL`. The dwc3 debugfs `mode` file implements only
  `->write`, so `kernel_write()` cannot use it.
- Once patched, it received the **same truncated `ep0` sequence** from a latched car. That proved the latch
  is head-unit behaviour.
- It logged "legacy head unit without CARPLAY_CONTROL flag, inviting self". This confirmed that this car
  will never dial the phone, which ended a long hunt for the missing invitation.

**Lesson:** each reference was right about something and wrong about something else. catplay sent
`et` = None correctly, while our port had sent 0. The dongle's NCM timing worked; its exact descriptors
were not required. Verify each borrowed detail on the car.

---

## 9. Paths that did not work: Android Auto, mySPIN, in-band iAP2

**Android Auto, as the phone.** The project started as an Android Auto *source*: the board posed as an
Android phone over AOA2 (`experimental/bridge/`). The head unit authenticates the *phone* with a
Google-signed certificate, and the session stops at `AuthComplete status -3`. There is no way around that
without Google's key.

CarPlay is the opposite. The accessory authenticates itself to the phone, so the phone side needs no
secret. That asymmetry is why the project switched to CarPlay. FairPlay was expected to be the next wall;
this legacy receiver never asks for it.

**Android persona at the Jaguar.** `carplay/src/usb/aoa_probe.rs` (`RASCAR_PERSONA=android`) presents a
Google-VID phone, answers AOA `GET_PROTOCOL` with 2 and logs what the car sends. The car identifies as
"Android Auto", not as Bosch mySPIN. On this firmware the USB Android path is Android Auto only, so it hits
the same certificate wall.

**mySPIN / InControl Apps.**

- Decompiling the Android apps showed a wire protocol with no authentication or encryption.
- It also has no turn-by-turn message (only "navigate to" for the car's own navigation) and no second video
  surface, so there is no path to the HUD or the cluster either.
- On iOS it would ride on an iAP2 External Accessory session. This car declares no EA protocol in either
  CarPlay or iPod mode
  ([protocol.md 3.6](protocol.md#36-iap2-over-hid-the-device-mode-variant)).

**iAP2 over HID (iPod mode).** Answering the car's capability request with "no CarPlay" gives a complete
iAP2 session over HID reports, and the car never detaches. But there is no CarPlay and no EA protocol, so
nothing to project.

**Conclusion:** on this car, wired CarPlay is the only projection path. The car's `/info` lists a single
display and the car declares no route-guidance iAP2 messages, so CarPlay cannot reach the HUD either.

---

## 10. Debugging your own car: a checklist

Walk the phases in order and stop at the first one that fails.

1. **Does the car configure us?** Check that `/sys/class/udc/*/state` reaches `configured` and
   `current_speed` is `high-speed`. If it stays `addressed`, compare the configuration indices the car
   requests (dwc3 trace) with the ones you offer.
2. **Does `C0 53` arrive, and then `40 51`?** If `40 51` never comes, wait at least 30 s on a cold start
   before doing anything. Do not re-enumerate; see the latch. A `40 51 wValue=0` means the user chose the
   car's own phone-app mode instead of CarPlay.
3. **Does the role switch hold?** Check that `/sys/kernel/debug/usb/<udc>/mode` reads `host` 10 s later. If
   it flips back, something (tcpm, the Type-C driver) is reacting to the car's VBUS cycle.
4. **Does the car enumerate?** Look in `kernel.log` for its VID:PID, and note it: it may not be `8086:9999`.
   Add your car's VID:PID to the usbcore quirk (`<vid>:<pid>:ak`), because the LPM and string-fetch
   problems belong to the head unit's gadget stack, not to Jaguar.
5. **iAP2:** with `RASCAR_WIRE_TRACE=1`, look for the detect, the car's SYN and its parameters, then our
   SYN|ACK and the car's ACK. Copy the car's `0x1D01` lists (params `0x0006`/`0x0007`) into a bug report.
   They say exactly what your car will send and expect.
6. **NCM:** check that a netif appears under the car's sysfs node, has an `fe80::` address, and that
   `ip -6 neigh` shows the car.
7. **AirPlay:** look for the car's `_airplay._tcp` TXT `features`. Bit 37 set means your car *does* dial
   `_carplay-ctrl`. Check the `SETUP /setup` status (466 means `et` is wrong) and the `GET /info` dump.
8. **When the session dies:** compare the disconnect time with enumeration, SYN, auth and the first NCM
   packet over several runs, and look for the lowest-variance anchor.

When you report a new car, include the run log, both pcaps and the `kernel.log` of the attempt. See
[tested-vehicles.md](tested-vehicles.md) for the template.

---

## 11. Timeline

| Date (2026) | Milestone |
|---|---|
| June–July | Android Auto phone impersonation blocked by the certificate wall (`AuthComplete -3`) |
| 19 July | Feasibility study for a CarPlay transmitter (no chip needed as the phone); first full implementation written blind, never run against a car |
| 15 Sept | First day at the car: dwc3 trace shows the car wants four configurations → **first `40 51` role switch** → dwc3 host via debugfs + fusb302 unbind → **car enumerates as `8086:9999`** → `STRING_FETCH_255` quirk → **first iAP2 link**, car identifies as "Jaguar F-PACE" → CarPlay logo on screen → **first NCM link** with the patched cdc_ncm → `GET /info` over IPv6 link-local |
| 16 Sept | **First iAP2 SessionReady**; RTSP through screen SETUP and the data channel |
| 17 Sept | First continuous H.264 to the car; the session dies after a few seconds |
| 18 Sept | Role-switch latch broken with a fresh USB serial; dashboard briefly visible |
| 19–20 Sept | Commercial dongle capture and firmware analysis; catplay run on the board; NCM enumeration fix; audio-ownership contradiction fixed |
| 21 Sept | cdc_ncm bind settled before iAP2; negotiated retransmit timer; ACK-only seq fix; **car GPS over iAP2** |
| 22 Sept | **`et=1`**: no more 466 or pairing; dashboard on screen 2–3 s per attempt; capture archive and run profiling |
| 23 Sept | Touch scaling fixed; NCM/iAP2 collision analysis; mySPIN and iAP2-over-HID investigated |
| 25 Sept | Cutoff model measured across three planes; clean NCM link config; **first sustained CarPlay session after disabling USB2 LPM for the car** |
| 27 Sept | Cold-start handling, NVMe power state, power-cut hardening, clock from car GPS |
| 28 Sept | **Media audio** in the car; GOP video instead of all-IDR |
| 29 Sept | Audio after car borrows; 4G stick no longer mistaken for the car; screen hand-back to the native UI |
| 30 Sept | GPU (Wayland) capture path |
| 1–5 Oct | Daily-drive fixes: late RTSP replies by CSeq, `requestUI` after a borrow, initial screen sync, boot reliability |
| 6 Oct | Steering-wheel keys; signed over-the-air updates |
| 7 Oct | Cold-start role-switch analysis: 45 s re-presentation grace |
| 9 Oct | Full-drive data analysis: 16 of 16 sessions ended only at ignition-off |
