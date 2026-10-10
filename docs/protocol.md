# Wired CarPlay from the phone side: a byte-level guide

This page explains how to be the **iPhone** in a wired CarPlay connection to a real head unit. It goes
step by step, in the order the car expects things, from the first USB descriptor read to a running video,
audio and touch session. It also names the file in `carplay/src` that implements each step.

Everything here was measured on one car: a **Jaguar F-PACE** with an InControl Touch Pro head unit made by
Harman. Its receiver is a *legacy* wired CarPlay receiver: AirPlay `srcvers` 280.33.12.1, features
`0x44440380,0x41`, display 1200x512. See [tested-vehicles.md](tested-vehicles.md) for its full identity.
The phone side runs on an Orange Pi 5 Plus (RK3588, Linux 6.18).

Conventions used on this page:

- **Roles.** "We", "phone" and "device" mean the board pretending to be an iPhone. "Car", "accessory" and
  "head unit" mean the Jaguar. The roles at the USB level change halfway through (see [step 2](#2-role-switch-the-phone-becomes-usb-host)).
- **Byte order.** Hex bytes are written as they appear on the wire. Multi-byte integers are big-endian
  unless the text says LE.
- **Source of truth.** The code is authoritative. Where an earlier finding was disproved, it appears in a
  short "what does NOT matter" list. Those lists are worth reading before you spend days on the same idea.
- **Unverified.** Anything not proven on the car is marked **(unverified)**.
- **File references** point to `carplay/src/...` unless the path says otherwise. Functions are named
  rather than line numbers, because line numbers drift.

## Contents

0. [The whole sequence on one page](#0-the-whole-sequence-on-one-page)
1. [USB device phase: looking like an iPhone](#1-usb-device-phase-looking-like-an-iphone)
2. [Role switch: the phone becomes USB host](#2-role-switch-the-phone-becomes-usb-host)
3. [iAP2: link layer, identification, authentication, services](#3-iap2-link-layer-identification-authentication-services)
4. [The CDC-NCM tunnel and IPv6 link-local](#4-the-cdc-ncm-tunnel-and-ipv6-link-local)
5. [mDNS / Bonjour](#5-mdns--bonjour)
6. [The CarPlay (AirPlay) session](#6-the-carplay-airplay-session)
7. [Failure modes and the car's clocks](#7-failure-modes-and-the-cars-clocks)
8. [Credits](#8-credits)

---

## 0. The whole sequence on one page

```
 t (typical, car already awake)        PHONE (board)                         CAR (head unit)
 ─────────────────────────────────────────────────────────────────────────────────────────────────
 0.0   USB device, VID 05AC, 4 configs  ◄── GET_DESCRIPTOR x N, SET_CONFIGURATION 2
       HID report descriptor (208 B)    ◄── 21 0A SET_IDLE, 81 06 GET_DESCRIPTOR(HID report)
                                        ◄── C0 53  "can you switch roles?"   → 01 00 00 00
                                        ◄── 40 40  charging 500/1000 mA      → ACK
                                        ◄── 40 51 wValue=1  "become host"     → ACK
 +0.01 remove gadget, unbind fusb302, dwc3 debugfs mode := host
 +0.6…1.1                               car enumerates as USB device 8086:9999
       claim iAP2 iface (FF/F0/00)      ◄── FF 55 02 00 EE 10 (iAP2 detect)   → echo
                                        ◄── SYN (link params, sessions)       → SYN|ACK (counter-proposal)
                                        ◄── ACK
       0x1D00 StartIdentification       ──►
                                        ◄── 0x1D01 IdentificationInformation
       0x1D02 IdentificationAccepted    ──►
       0xAA00 RequestAuthCertificate    ──►   ◄── 0xAA01 certificate (908 B)
       0xAA02 Challenge (32 B random)   ──►   ◄── 0xAA03 response
       0xAA05 AuthenticationSucceeded   ──►   (we accept blindly, no MFi chip)
       post-auth burst 4E09 4E0A 4E0B FFFA ──►  ◄── 4E01 AE00 AE03 5000 4C00 4E03 (answered)
 +~1.5 CDC-NCM up (patched cdc_ncm), IPv6 link-local, car's fe80 from neighbour table
       mDNS _carplay-ctrl announce      ──►   (car browses, never dials it)
       TCP connect [car fe80]:7000      ──►   AirPlay receiver
       SETUP /setup (et=1)              ──►   ◄── eventPort, keepAlivePort
       GET /info                        ──►   ◄── displays, hidDevices, audioFormats, modes
       TCP connect eventPort            ──►   (car sends POST /command to us here)
       RECORD                           ──►
       POST /command modesChanged       ──►   (we own MainScreen + MainAudio)
       SETUP {type 110 screen}          ──►   ◄── dataPort
       TCP dataPort: VideoConfig, IDR, P, P …  (AES-128-CTR)
       SETUP {type 100 audio}           ──►   ◄── dataPort; RTP PCM over UDP
       /feedback every 1 s, data-channel keepalive every 1 s, UDP keepalive every 1 s
                                        ◄── 0xFFFB NMEA at 1 Hz; hidSendReport (touch, keys)
```

---

## 1. USB device phase: looking like an iPhone

Implemented in `usb/gadget.rs` (configfs) and `usb/ffs.rs` (FunctionFS descriptors and the ep0 loop).

### 1.1 Gadget identity (configfs)

The gadget lives at `/sys/kernel/config/usb_gadget/carplay`. The code loads `libcomposite` and `usb_f_fs`,
and uses the first UDC under `/sys/class/udc`, which is `fc000000.usb` on the RK3588. The USB-C port must
be able to start in peripheral mode. On Armbian with the 6.18 kernel the controller node `usb@fc000000`
already has `dr_mode = "otg"`, so no device-tree overlay is needed (older guides mention
`rk3588-dwc3-peripheral`; that overlay does not exist for 6.18).

| Attribute | Value | Notes |
|---|---|---|
| `idVendor` | `0x05AC` | Apple |
| `idProduct` | `0x12A8` | iPhone |
| `bcdDevice` | `0x1001` | Matches a commercial CarPlay dongle in iPhone mode, and published iPhone dumps |
| `bcdUSB` | `0x0200` | The wire showed `0x0210` plus a BOS descriptor (composite/LPM default). The car reads the BOS. |
| `bDeviceClass/SubClass/Protocol` | `00/00/00` | Per-interface classes |
| `bMaxPacketSize0` | 64 | |
| `max_speed` | **not written** | High speed is mandatory: with `full-speed` the car stays in *addressed* and never sends SET_CONFIGURATION |
| Manufacturer / Product | `Apple Inc.` / `iPhone` | |
| Serial | 40 hex characters | Random once, persisted in `/var/lib/rascar/usb_serial` (`RASCAR_USB_SERIAL_STORE`); `RASCAR_USB_SERIAL` pins one. A real iPhone uses a 40-hex UDID; the dongle used 17 characters. The car does not seem to care **(unverified)**. |
| `MaxPower`, `bmAttributes` | 500, `0xC0` | Per configuration |

An extra string group (`strings/0x409/pad/s = "Apple Mobile Device"`) pushes the string of configuration 1
to index 5, as on a real iPhone **(index placement unverified)**.

### 1.2 Four configurations, like a real iPhone

The car reads all four configurations and then selects **configuration 2**. A single-configuration
gadget, or one with a single vendor `FF/FF/00` "iAP Interface", never received the role-switch request.

| cfg | `iConfiguration` | Interfaces (class/subclass/protocol, endpoints as declared to FunctionFS) |
|---|---|---|
| 1 | `PTP` | IF0 `06/01/01` still image: bulk OUT, bulk IN, interrupt IN |
| 2 | `iPod USB Interface` | IF0/IF1 USB audio (from the kernel's `f_uac1`), **IF2 `03/00/00` HID** + interrupt IN (mps 64) |
| 3 | `PTP + Apple Mobile Device` | PTP + IF1 `FF/FE/02` "Apple USB Multiplexor": bulk OUT, bulk IN |
| 4 | `PTP + Apple Mobile Device + Apple USB Ethernet` | PTP + AMD + IF2 `FF/FD/01` alt 0 empty, alt 1 bulk IN/OUT |

Endpoints are bulk 64 (FS) / 512 (HS). Interrupt endpoints are mps 64 with bInterval 8 (FS) / 7 (HS).

Configuration 2's audio function comes from `f_uac1` (`c_chmask=3`, `c_ssize=2`, rates 8000…48000,
`p_chmask=0`). FunctionFS rejects class-specific audio descriptors (types `0x24/0x25`), so they cannot be
declared through it. `RASCAR_UAC1=off` falls back to a minimal hand-written AC/AS layout. Whether the
car's behaviour depends on the audio interface was **never isolated**.

### 1.3 The iPod HID report descriptor

The HID class descriptor is `09 21 11 01 00 01 22 D0 00` (HID 1.11, one report descriptor of 208 bytes).
The report descriptor itself was copied byte for byte from a commercial dongle in iPhone mode
(`HID_REPORT` in `usb/ffs.rs`):

```
06 00 ff 09 01 a1 01 75 08 26 80 00 15 00
09 01 85 01 95 05 82 02 01   09 01 85 02 95 09 82 02 01   09 01 85 03 95 0d 82 02 01
09 01 85 04 95 11 82 02 01   09 01 85 05 95 19 82 02 01   09 01 85 06 95 31 82 02 01
09 01 85 07 95 5f 82 02 01   09 01 85 08 95 c1 82 02 01   09 01 85 09 96 01 01 82 02 01
09 01 85 0a 96 81 01 82 02 01  09 01 85 0b 96 01 02 82 02 01  09 01 85 0c 96 ff 02 82 02 01
09 01 85 0d 95 05 92 02 01   09 01 85 0e 95 09 92 02 01   09 01 85 0f 95 0d 92 02 01
09 01 85 10 95 11 92 02 01   09 01 85 11 95 19 92 02 01   09 01 85 12 95 31 92 02 01
09 01 85 13 95 5f 92 02 01   09 01 85 14 95 c1 92 02 01   09 01 85 15 95 ff 92 02 01
c0
```

The descriptor declares a vendor page `0xFF00` with 8-bit fields:

- 12 **Input** reports, ids `0x01…0x0C`, with 5, 9, 13, 17, 25, 49, 95, 193, 257, 385, 513 and 767 bytes;
- 9 **Output** reports, ids `0x0D…0x15`, with 5, 9, 13, 17, 25, 49, 95, 193 and 255 bytes.

These are the iAP2-over-HID transport reports (see [3.6](#36-iap2-over-hid-the-device-mode-variant)).

An older 96-byte descriptor, with 4 Input and 5 Output reports, was already enough to receive the role
switch. The 208-byte one is used now because it is what a known-good dongle sends.

### 1.4 FunctionFS specifics

- Each configuration has its own FunctionFS instance, `functions/ffs.cN`, mounted at
  `/dev/ffs-carplay-cN`.
- The descriptor bundle is v2 (magic 3) with flags `HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP |
  CONFIG0_SETUP` = `0xC3`.
- **`ALL_CTRL_RECIP` is required.** CarPlay's vendor requests are addressed to the *device* recipient.
  Without the flag, FunctionFS never sees them.
- The strings bundle contains one string, `"Apple USB Multiplexor"`. FunctionFS needs at least one string to
  reach ACTIVE.
- Descriptors and strings are written to every `ep0` **before** the UDC is bound. All four `ep0` file
  descriptors must stay open: closing one unbinds the whole gadget.
- One thread `poll()`s the four `ep0`s with a 200 ms timeout. Events are 12 bytes long (BIND=0, UNBIND=1,
  ENABLE=2, DISABLE=3, SETUP=4, SUSPEND=5, RESUME=6).
- Each data stage takes exactly one `read(2)`. To stall an IN request, `read(0)`; it returns `EL2HLT`.
- There are no MS OS descriptors.

### 1.5 The car's control requests (device phase)

This sequence was measured on configuration 2 and is handled in `usb/ffs.rs`:

| bmRequestType | bRequest | wValue | wIndex | wLength | Meaning | Our answer |
|---|---|---|---|---|---|---|
| `21` | `0A` | 0 | 2 | 0 | HID SET_IDLE | ACK |
| `81` | `06` | `0x2200` | 2 | 96 / 34 / 512 | GET_DESCRIPTOR (HID report) | the 208-byte report, truncated to wLength |
| **`C0`** | **`53`** | 0 | 0 | 4 | **Apple: "CarPlay / role-switch capability?"** | **`01 00 00 00`** |
| `40` | `40` | `0x01F4` (500) | `0x03E8` (1000) | 0 | Apple charging request: 500 mA / 1000 mA | ACK |
| **`40`** | **`51`** | **1** | 0 | 0 | **Apple: "switch roles, become USB host" (CarPlay)** | **ACK, then go to step 2** |
| `40` | `51` | **0** | 0 | 0 | "stay a USB device". Sent when the driver picks InControl, not CarPlay, on the head unit's chooser | ACK, log |
| `A1` | `01` | — | — | n | HID GET_REPORT | n zero bytes |
| `21` | `09` | — | — | n | HID SET_REPORT | read the data stage; it carries iAP2 when the car uses iAP2 over HID |
| `C0` | `45` | — | — | 1 | usbmuxd GET_MODE | `01` **(inferred; the car never sent it)** |

Notes on this table:

- Answering `C0 53` with `00 00 00 00` hides CarPlay (`RASCAR_HIDE_CARPLAY=1`). The car then runs iAP2
  over HID with the board still in device mode (see 3.6).
- The ACK to `40 51` goes out **before** anything else happens. The ep0 loop then returns
  `RoleSwitchRequested` and all `ep0` file descriptors are closed.

**When the role switch comes.** If the car is already running, `40 51` arrives about 0.7–1.3 s after
`C0 53`. On a cold start the car configures the gadget about 2.5 s after power-on but sends `40 51`
**on that same enumeration** only 22–27 s later, with no traffic in between. Do not "help" it by
re-enumerating; see the [role-switch latch](#73-the-role-switch-latch-car-stops-sending-40-51).

**Minimum needed to get `40 51`:**

- four iPhone-style configurations at high speed;
- a HID interface in configuration 2 whose report descriptor is served;
- `C0 53` answered with `01 00 00 00`;
- `40 40` ACKed.

No iAP2 traffic over HID is needed first.

### 1.6 Never read the car's strings with libusb

This applies after the role switch, when the car is a USB device.

- The car's string descriptor 3 is exactly 64 bytes, and the car sends **no zero-length packet** after it.
- Linux's 255-byte string fetch therefore hangs for about 4 s and ends in `-71` (EPROTO). After that every
  request fails and the car drops off the bus.
- The kernel fix is the usbcore quirk flag `a` (`USB_QUIRK_STRING_FETCH_255`); see [2.3](#23-the-usbcore-quirk-80869999ak).
- **`lsusb -v` against the car** does the same 255-byte read from userspace, where the quirk does not apply.
  It wedges the car for about 10 s.

---

## 2. Role switch: the phone becomes USB host

Implemented in `host/mod.rs` (`run`), `host/role_switch.rs` (`switch_to_host`) and `host/usb_host.rs`
(finding and claiming the car).

### 2.1 Sequence

1. Take a snapshot of the network interfaces, so a new USB netif can be recognised later.
2. **Remove the gadget completely**: unbind the UDC, remove the configs, unmount FunctionFS. This order is
   the hardware-proven one.
3. Wait **10 ms** after the `40 51` ACK. This is the commercial dongle's `hnp_interval_ms` default;
   override with `RASCAR_ROLE_SWITCH_DELAY_MS`.
4. Write the usbcore quirk `8086:9999:ak` (see 2.3).
5. **Unbind the Type-C controller driver.** Unbind every device bound to
   `/sys/bus/i2c/drivers/typec_fusb302`; on this board that is one device. The reason is below.
6. Switch the dwc3 controller. The code tries several knobs in order and logs each attempt:

   | Knob | Result on RK3588 / 6.18 |
   |---|---|
   | `/sys/class/usb_role/fc000000.usb-role-switch/role` | the switch exists but has **no `role` attribute** |
   | **`/sys/kernel/debug/usb/fc000000.usb/mode` ← `host`** | **works** |
   | `/sys/bus/platform/devices/fc000000.usb[/dwc3.*]/role` | fallback |
   | `…syscon…/usb2-phy@…/otg_mode` (Rockchip legacy) | fallback |

7. Wait **300 ms**, re-read `mode` and warn if it is not `host`. One `ROLE-SWITCH RESULT:` line
   summarises the switch.
8. Wait up to **20 s** for the car to enumerate, polling every 25 ms. It takes 0.55 or 1.1 s
   (bimodal). Then set `power/control=on` on the car device, so it never autosuspends.

On any error in this phase, `RoleRollback` writes `device`/`peripheral` back and re-binds fusb302. The run
wrapper `setup/carplay-debug/rascar-carplay-run` also checks before every attempt that `mode` is
`device` and that fusb302 is bound.

**Why fusb302 must be unbound.** The car cycles VBUS for about 220 ms during its own role flip. tcpm
(through the fusb302) sees SNK_READY → SNK_UNATTACHED and pushes the USB role back to device, so dwc3
drops host mode about 5 s in. With fusb302 unbound, `mode=host` holds for the whole session. The device
tree has `dr_mode="otg"` plus `usb-role-switch`.

> Kernel-module implementers: the debugfs `mode` file implements only `->write`. An in-kernel
> `kernel_write()` on it returns `-EINVAL`. A userspace `write(2)` works.

### 2.2 What the car looks like as a USB device

| Field | Value |
|---|---|
| VID:PID | **`8086:9999`** |
| Manufacturer / product | `Wind River Systems` / `Gadget MFi v0.1` |
| Speed / bcdUSB / bcdDevice | high / `0x0210` / `0x0099` |
| Configurations | one: `Gadget MFI (iap2/eap/ncm/audio)`, self-powered |

The car's interfaces are:

| IF | Class | Endpoints | Role |
|---|---|---|---|
| 0 | `FF/F0/00` | bulk IN `0x81`, bulk OUT `0x02`, mps 512, alt 0 | **iAP2** |
| 1 (IAD 1–2) | `02/0D/00` | interrupt IN `0x85`, mps 16 | **CDC-NCM control**: Header, Union 1→2, Ethernet descriptor with `iMACAddress=3`, NCM 1.0 |
| 2 | `0A/00/01` | alt 0 none; alt 1 bulk IN `0x83` / OUT `0x04` | CDC-NCM data |

How the code finds the car (`usb_host.rs`, `scan_once` and `inspect_device`):

- **Only devices under the role-switched controller count.** The sysfs path must contain the component
  `fc000000.usb` (`on_role_switched_controller`).
  - Without this check, a USB 4G stick on another controller, with a vendor interface and a bulk pair, was
    claimed as "the car" 2–7 ms after the switch. The real car arrived 250 ms later and was ignored.
- The first `0xFF` interface with a bulk IN+OUT pair becomes iAP2.
- A `02/0D` interface is recorded as NCM.
- The VID:PID is **not** used to identify the car. It appears in the code only in the quirk string.
- Claiming uses `detach_and_claim_interface` and 16 KiB bulk transfers. On `-71`/EPIPE, `recover()` cancels
  the transfers, clears the halt on both endpoints and rebuilds them.

### 2.3 The usbcore quirk `8086:9999:ak`

`role_switch.rs` (`apply_car_usb_quirk`) appends `8086:9999:ak` to `/sys/module/usbcore/parameters/quirks`
on every attempt, before the switch. It upgrades an existing `8086:9999:a`.

| Flag | Kernel name | Why |
|---|---|---|
| `a` | `USB_QUIRK_STRING_FETCH_255` | The car's 64-byte string 3 without a ZLP wedges the 255-byte fetch (see 1.6) |
| `k` | `USB_QUIRK_NO_LPM` | The car advertises USB2 LPM + BESL. With hardware LPM enabled, **every session died about 4.3 s after authentication** (see [7.1](#71-the-6-second-cutoff)). Disabling LPM gave the first session that outlived that deadline; sessions now last as long as the drive. |

The causality of `k` is strong but not formally isolated. It was the last change before the first
sustained session, and nobody has re-run with only `a` since. **(No A/B with `k` removed.)**

### 2.4 Re-acquiring the car

After authentication the car may detach and re-present its device 0.3–0.5 s later. This matches the
"second enumeration" in Apple's CarPlay flow, though this car does not need one.

- `host::reacquire_car` repeats only the wait-and-claim step (1.5 s timeout), without touching the role.
  Up to 3 such cycles are followed.
- iAP2 bring-up retries twice on a disconnect (`IAP2_BRINGUP_RETRIES=2` in `main.rs`).

### 2.5 What does NOT matter (USB / role switch)

- The `usb_role` class `role` attribute: absent on this board. debugfs `mode` is the knob.
- **`drivers_autoprobe=0`** to keep cdc_ncm away from the car: harmful.
  - Before the switch it stops `hub` binding the new root hub, so nothing enumerates.
  - After it, the generic `usb` driver never sets a configuration.
  - It is off by default (`RASCAR_DEFER_NCM_BIND`).
- Serial rotation between attempts: implemented (`RASCAR_ROTATE_SERIAL=1`) but never shown to help. It is
  off.
- A `USBDEVFS_RESET` of the car right after authentication, to force a "second enumeration": the car
  never came back (`can't restore configuration (error=-71)`). Refuted; left as opt-in
  `RASCAR_POST_AUTH_RESET=1`.
- Endpoint collisions between iAP2 and NCM: none (`0x02/0x81` vs `0x04/0x83`, `0x85`).
- Bus suspend or autosuspend: never seen, and `power/control=on` is set anyway.

---

## 3. iAP2: link layer, identification, authentication, services

Implemented in `iap2/packet.rs`, `iap2/link.rs`, `iap2/control.rs`, `iap2/nowplaying.rs` and
`iap2/mod.rs`, with orchestration in `main.rs`. The packet and link code is derived from
[usenocturne/iap2-rs](https://github.com/usenocturne/iap2-rs), which is an *accessory*-side stack. The
negotiation and the control session are inverted here to the **device** side.

### 3.1 Detect

Right after we claim interface 0, the car sends `FF 55 02 00 EE 10` unprompted. **We echo the same six
bytes.**

- This is not an `FF 5A` packet. The echo is done inside the frame reader in every link state, so a
  re-sent detect is always answered (`read_packet`).
- If the car sees no SYN|ACK (below), it re-sends its SYN after 1 s. It drops off USB about **4 s after
  enumerating** if it never gets one, which is why the enumeration poll is 25 ms.

### 3.2 Packet format

| Byte | Field |
|---|---|
| 0–1 | `FF 5A` |
| 2–3 | total packet length, BE, including the header and the payload checksum |
| 4 | control: SYN `0x80`, ACK `0x40`, EAK `0x20`, RST `0x10`, SLP `0x08` |
| 5 | sequence number |
| 6 | acknowledgement number |
| 7 | session id (0 = link-level) |
| 8 | header checksum |
| 9… | payload, then 1 payload checksum byte (only when the payload is not empty) |

Both checksums are 8-bit two's complement of the byte sum: `(!sum) + 1`. The sum over the covered bytes
plus the checksum is 0.

### 3.3 Link negotiation: the car sends SYN

**The accessory originates SYN; the device answers SYN|ACK.** This is the reverse of what an
accessory-side stack does (`negotiate_device` in `link.rs`).

The Jaguar's SYN, captured:

```
ff 5a 00 1a 80 18 00 00 f5 | 01 05 1000 01f4 00b0 1e 0a | 01 00 02 | 02 01 01 | 16
                             │  │  │    │    │    │  │    └ session 1: type 0 (Control) v2
                             │  │  │    │    │    │  │      session 2: type 1 (File Transfer) v1
                             │  │  │    │    │    │  └ max cumulative acks 10
                             │  │  │    │    │    └ max retransmissions 30
                             │  │  │    │    └ cumulative ack timeout 176 ms
                             │  │  │    └ retransmit timeout 500 ms
                             │  │  └ max receive packet length 4096
                             │  └ max outstanding packets 5
                             └ link version 1
```

There is **no External Accessory session** (type 2) on this car.

**Our SYN|ACK** copies the car's payload and changes only two fields (`counter_propose`):

- max outstanding → **`0x7F`** (127);
- max receive length → **`0xFFFF`**.

These are the values a working commercial dongle sends to another head unit. The timing fields and the
session table are echoed untouched.

```
ff 5a 00 1a c0 00 18 00 b5 | 01 7f ffff 01f4 00b0 1e 0a 01 00 02 02 01 01 | ae
                 │  └ ack = car's SYN seq
                 └ our seq = 0x00
```

The car completes the handshake with a plain ACK (`0x40`, `ack` = our SYN|ACK seq). Our first DATA packet
then uses seq `0x01`.

From the SYN we adopt:

| Setting | Source | Notes |
|---|---|---|
| Control session id | type 0 entry | **`0x01` on the Jaguar**; the iap2-rs default is `0x0A` |
| File-transfer session id | type 1 entry | `0x02` |
| Max packet size | SYN | |
| **Retransmit timeout** | SYN bytes 4–5 | 500 ms here |

Before the retransmit timeout was adopted, we used a fixed 1500 ms. The SYN|ACK → auth phase then took
1.29 s on average instead of about 0.4 s.

### 3.4 Steady state: sequence and acknowledgement rules

- **DATA packets** use control `0x40`: ACK always set, with the current ack piggy-backed. Do **not** set
  `0x08`: it is SLP, and it tells the head unit on every frame that the device is going to sleep. The car
  itself sends `0x40`.
- **Sending.** Send, then wait up to the negotiated retransmit timeout for an ACK of that seq.
  Retransmit the identical packet up to 30 times. `seq` advances only when the packet is ACKed. In
  practice one packet is outstanding at a time.
- **Receiving.**
  - In order (`ack+1`): ACK at once and deliver. We do not batch cumulative ACKs.
  - Ahead of order: buffer and re-ACK the last in-order seq.
  - Behind: drop and re-ACK.
- **EAK** (payload byte 0 = highest seq the car received): retransmit the missing packet from a 16-entry
  TX history, then send an ACK-only. An EAK does not advance our seq.
- **RST** closes the link. **SUSPEND** is not implemented. The car never sent EAK, RST or SLP in any
  capture.
- **Transport errors.** On a cold handshake the car can NAK our bulk OUT and the URB completes with `-71`.
  `write_all_resilient` calls `recover()` (clear halt), waits 200 ms and retries up to 4 times. Resending
  is safe because iAP2 de-duplicates by seq.

**The ACK-only sequence number.** An ACK-only packet (no payload) must carry `seq − 1`, the last DATA seq
we sent, not the next one (`ack_only_seq`). Getting this wrong puts an ACK-only and the next DATA packet
on the same seq:

```
OUT  9 B  ctrl 40 seq 05 ack fc        ← ACK-only  (wrong: should be seq 04)
OUT 16 B  ctrl 40 seq 05 ack fc …      ← DATA
```

QNX/Harman stacks treat an unexpected sequence as fatal, and Apple's own link code uses `seq+1` only for
data. Fixing this did **not** cure the 6-second cutoff, but it is required for correctness.

**There is no iAP2 keepalive.** Link ACKs are the liveness signal. A self-invented "heartbeat" (a 0x5001
every 1.2 s) did nothing useful and is off (`RASCAR_IAP2_HEARTBEAT`).

### 3.5 Control session

**Message framing** (`control.rs`):

```
40 40 | LL LL (total length, counted from the 40 40) | MM MM (message id) | params…
param: LL LL (length, including this 4-byte header) | II II (param id) | data
```

- Groups nest the same TLVs.
- Strings are UTF-8 with a terminating NUL, so an empty string is the single byte `00`.
- Messages longer than one link packet are reassembled (`ControlAssembler`); the 908-byte certificate
  spans several.

#### Identification and authentication

| # | Direction | Message | Content |
|---|---|---|---|
| 1 | phone → car | `0x1D00` StartIdentification | no params |
| 2 | car → phone | `0x1D01` IdentificationInformation | see below |
| 3 | phone → car | `0x1D02` IdentificationAccepted | no params (`0x1D03` Rejected is never used) |
| 4 | phone → car | `0xAA00` RequestAuthenticationCertificate | — |
| 5 | car → phone | `0xAA01` AuthenticationCertificate | param `0x0000`: 908-byte MFi certificate |
| 6 | phone → car | `0xAA02` RequestAuthenticationChallengeResponse | param `0x0000`: 32 random bytes |
| 7 | car → phone | `0xAA03` AuthenticationResponse | param `0x0000`: signature |
| 8 | phone → car | `0xAA05` AuthenticationSucceeded | — |

The time budgets are 5 s for identification and 15 s for authentication (`iap2/mod.rs`).

**Why no MFi chip is needed.** In iAP2 the *accessory* proves to the *Apple device* that it is genuine.
The car holds the MFi coprocessor; it sends its certificate and signs our challenge. As the phone we are
the **verifier**. We need no Apple secret, and we accept the certificate and signature without checking
them.

Projects that implement the car or dongle side need an MFi chip; the phone side does not. A teardown of
a commercial dongle with its MFi chip removed reached the same conclusion: it still worked as a phone
towards a car.

#### What the car declares in `0x1D01`, and the `0x0007` gate

| Param | Meaning | Jaguar |
|---|---|---|
| `0x0000/01/02` | name / model / manufacturer | `Jaguar F-PACE` / `Jaguar F-PACE` / `Jaguar` |
| `0x0004/05` | firmware / hardware version | `S22A_22.32.4-…` / `IxC.1.0` |
| **`0x0006`** | messages sent by the accessory | `4C00 4C02 4C03 4C05 5000 5002 FFFB 4157 4159 4154 4156 4E01 4E03 4E05 AE00 AE02 AE03` |
| **`0x0007`** | messages received from the device | `4C01 4C04 5001 FFFA FFFC 4158 4155 4E09 4E0A 4E0B 4E04 AE01` |
| `0x0008` | power-providing capability | `0x02` |
| `0x000C/0D` | current / supported language | `en` |
| `0x0010` | USB host transport component | name `iAP2`; supports iAP2 and CarPlay; CarPlay interface 1 |
| `0x0011` | Bluetooth transport component | id 1 |
| `0x0014` | vehicle information component | |
| `0x0016` | location information component | sentences GPGGA, GPRMC, GPGSV, PASCD |

There is no `0x000A` (External Accessory protocol). So on this car nothing can ride on iAP2 External
Accessory sessions, in either mode.

**Only send what the car listed in `0x0007`.** `send_control` drops any device → accessory message that
is not in the car's list (`AccessoryIdentification::accepts`). A missing or empty list blocks nothing. The
gate is on by default; `RASCAR_IAP2_GATE=off` disables it.

On the Jaguar the gate suppresses:

- `0x4E0C` DeviceUUIDUpdate;
- `0x4E0E` DeviceTransportIdentifierNotification;
- `0x4E0D` WirelessCarPlayUpdate, which is not sent by default anyway.

#### After authentication

Our post-auth burst runs in this order, each message only if the gate allows it (`iap2/mod.rs`):

| ID | Name | Params we send |
|---|---|---|
| `0x4E0C` | DeviceUUIDUpdate | `0x0000` UUID string (gated off here) |
| `0x4E09` | DeviceInformationUpdate | `0x0000` `"iPhone\0"` |
| `0x4E0A` | DeviceLanguageUpdate | `0x0000` `"en\0"`; wire `40 40 00 0D 4E 0A 00 07 00 00 65 6E 00` |
| `0x4E0B` | DeviceTimeUpdate | `0x0000` **u64 Unix seconds**; `0x0001` u16 TZ offset = 0; `0x0002` **u8** DST = 0 |
| `0x4E0D` | WirelessCarPlayUpdate | `0x0000` u8: 0 unavailable / 1 available. Omitted by default (`RASCAR_IAP2_WIRELESS_CARPLAY`). |
| `0x4E0E` | DeviceTransportIdentifierNotification | BT and USB transport ids (gated off here) |
| `0xFFFA` | StartLocationInformation | empty flag params: `0x0001` GGA, `0x0002` RMC, `0x0003` GSV (only if the car declares it), `0x0004` PASCD |

On the Jaguar only `4E09 4E0A 4E0B FFFA` reach the wire. The burst is done about 0.3 s after
`0xAA05`; a real iPhone takes about 0.5 s.

The car's subscriptions arrive within about 20 ms of authentication, the same six in every run. All of
them are answered:

| Car → phone | Meaning | Our answer (phone → car) |
|---|---|---|
| `0x4E01` BluetoothComponentInformation | BT component enabled | none needed |
| `0xAE00` StartPowerUpdates | wants `0x0000`, `0x0001`, `0x0002` | **two** `0xAE01` PowerUpdate: `00 06 00 00 03 E8` (max current 1000 mA), then `00 05 00 01 01` (will charge). `0x0002` AccessoryPowerMode is not sent. |
| `0xAE03` PowerSourceUpdate | offers 2100 mA, charging on | logged (`headunit.rs`) |
| `0x5000` StartNowPlayingUpdates | wants media item ids 1, 4, 6, 12, 18, 26 and playback ids 0–7, 12–16 | `0x5001` "reset": status Stopped, app name `""`, bundle id `""`; then real updates (3.5.1) |
| `0x4C00` StartMediaLibraryInformation | — | `0x4C01`: group{`0x0000` name `"iPhone"`, `0x0001` UID `"1"`, `0x0002` type u8 0 = local device} |
| `0x4E03` StartBluetoothConnectionUpdates | component id 1 | `0x4E04`: `0x0000` u16 1, `0x0001` empty group (no profiles connected) |

Other messages:

- Declared by the car but never seen: `0x4154` StartCallStateUpdates (we would answer `0x4155` with state
  Disconnected), `0x4157` Communications, `0x5002` StopNowPlaying, `0xAE02`.
- `0x4C03` StartMediaLibraryUpdates (we answer `0x4C04` with UID `"1"`, revision `"1"`, no items) is seen
  only in HID mode.
- **`0x4300`/`0x4301` (CarPlayAvailability / CarPlayStartSession) are not declared by this car and not
  implemented.** On this car, CarPlay start is not signalled over iAP2 at all: it happens on the network
  (step 4 onwards).

#### 3.5.1 Now Playing (`0x5001`)

Implemented in `iap2/nowplaying.rs`. The update has two top-level groups: `0x0000` media item attributes
and `0x0001` playback attributes.

| Group | Id | Field | Type |
|---|---|---|---|
| media | `0x0001` | title | string |
| media | `0x0004` | duration (ms) | u32 |
| media | `0x0006` | album | string |
| media | `0x000C` | artist | string |
| media | `0x0012` | composer | string (empty) |
| media | `0x001A` | artwork file-transfer id | u8 |
| playback | `0x0000` | status: 0 stopped, 1 playing, 2 paused | u8 |
| playback | `0x0001` | elapsed (ms) | u32 |
| playback | `0x0002/03/04` | queue index / count / chapter | u32 |
| playback | `0x0005/06` | shuffle / repeat | u8 |
| playback | `0x0007` | app name | string |
| playback | `0x000C` | speed (100 = playing) | u16 |
| playback | `0x000D/0E` | set-elapsed / queue-list available | u8 |
| playback | `0x0010` | app bundle id | string |

The reset reply is `40 40 00 19 50 01 00 13 00 01 | 00 05 00 00 00 | 00 05 00 07 00 | 00 05 00 10 00`.

How updates are sent:

- Only changed fields that the car asked for are sent, at most one update every 500 ms.
- The source is the dashboard's `/api/media` endpoint, polled every 1 s.
- Artwork goes over the File Transfer session. It is off by default (`RASCAR_NOWPLAYING_ARTWORK`), and the
  layout of its setup payload is **unverified**.

#### 3.5.2 Car GPS and gear (`0xFFFB`)

After `0xFFFA`, the car sends `0xFFFB` LocationInformation. Each message carries one NMEA sentence in param
`0x0000` (ASCII, ending in `\n`). The first one arrives 1.3–5 s after the SYN; then one burst per second:

- `$GPGGA`: fix quality always 6 (dead-reckoning / fused);
- `$GPRMC`: always `A`, mode `E`;
- `$PASCD,<seconds of day>,C,<gear>,1,1,0.00,<speed m/s>*CS`: gear `P`, `R`, `N`, `D`, or `U` (unknown, at
  start). The speed matches OBD PID `0x0D` with no lag; RMC lags it by about 1 s.
- `$GPGSV`, about 3 parts per second, if requested.

What the code does with them:

- `iap2/mod.rs` validates the `*HH` checksum and appends each sentence to
  `/var/lib/rascar-debug/car-gps.nmea`.
- `gps_clock.rs` steps the system clock from a valid RMC when it is more than 2 s off and NTP is not
  synchronised. It checks at most once a minute. The board has no network time in the car, so this
  matters.
- The dashboard parses the gear (`app/src/services/gps.rs`, `parse_pascd_gear`). A gear reading goes stale
  after 5 s.

`0xFFFC` StopLocationInformation is **not** sent by default (`RASCAR_IAP2_KEEP_LOCATION`), so the stream
stays open for the whole session.

### 3.6 iAP2 over HID (the device-mode variant)

Implemented in `usb/hid_iap2.rs`. It is opt-in: `RASCAR_IAP2_HID=1`.

The car runs iAP2 over the configuration-2 HID interface while the board is still a USB device:

- always, if `C0 53` is answered "no CarPlay";
- once (not reproducible), with a never-seen USB serial.

How the transport works:

- **Framing.** Each report is `[report id][link control][payload, zero-padded]`. Link control is `00`
  for a whole message, `02` first, `03` middle, `01` last.
- **Car → phone:** HID Output reports, delivered as ep0 SET_REPORT data stages, with report ids `0x0E`,
  `0x12`, `0x14`, `0x15`. The detect, for example, is `0e 00 ff 55 02 00 ee 10 00`.
- **Phone → car:** Input reports on the interrupt IN endpoint (FunctionFS `ep1`, mps 64). Only ids
  `0x01…0x06` fit in one 64-byte packet, so we use the smallest that fits. Our detect echo is
  `02 00 ff 55 02 00 ee 10 00 00`.

What a session over HID looks like:

- Identification and authentication succeed. The car's `0x1D01` then lists a USB host-transport component
  and an iAP2 HID component.
- The car subscribes to `0x6800` StartHID and `0xDA00` StartUSBDeviceModeAudio, in addition to the usual
  subscriptions.
- The car never detaches in this mode; sessions lasted 3–62 s.
- There is no CarPlay here, and **no External Accessory protocol**, so InControl Apps or mySPIN cannot use
  it.

### 3.7 What does NOT matter (iAP2)

None of these moved the 6-second cutoff. The fixes that are correct are kept.

- Answering every car subscription. All of them are answered and link-ACKed now, and the cutoff did not
  move.
- The ACK-only seq defect.
- The SLP bit on DATA packets.
- The order or presence of the `0x4E0x` identity updates.
- The NowPlaying heartbeat.
- `0xFFFC` vs keeping location open.
- An apparent NAK of IdentificationAccepted. It was a cold-start `-71`, fixed by transport recovery.

The car **never closes the session in-band**. In no capture did it send EAK, RST, SLP or `0x1D03`. When it
gives up, its iAP2 stack simply goes silent.

---

## 4. The CDC-NCM tunnel and IPv6 link-local

Implemented in `host/ncm.rs` and `host/mod.rs`, plus the kernel patch in `kernel/cdc_ncm/` and the network
config in `setup/carplay-debug/01-rascar-carplay-ncm.{link,network}`.

### 4.1 Why stock `cdc_ncm` fails on this car

1. **`iMACAddress` is not a MAC.** The Ethernet functional descriptor's `iMACAddress=3` points at the
   configuration name, not at 12 hex digits. `usbnet_get_ethernet_addr` fails, bind fails, and no netif
   appears. The same string is the ZLP-less 64-byte string from 1.6.
2. **The alt-setting switch comes too soon.** Upstream waits 10–20 ms before `usb_set_interface(data, 1)`.
   On this gadget, a SET_INTERFACE about 12–40 ms after SET_CONFIGURATION gets `-71` and the device falls
   off the bus.
3. **NTB sizes.** Upstream's defaults are larger than what a shipping CarPlay dongle negotiates.

### 4.2 The patch (`kernel/cdc_ncm/cdc_ncm-rascar.patch`)

The patch is built out of tree against your kernel's own `cdc_ncm.c`. Version stamp: `MODULE_VERSION("rascar-3")`.

| Change | Detail |
|---|---|
| Random-MAC fallback | When `usbnet_get_ethernet_addr` fails, call `eth_hw_addr_random()` and warn `invalid mac address string, using random …` instead of failing the bind |
| Bind delay | `msleep(220)` before `usb_set_interface`. This is the commercial dongle's "delay load NCM" value. |
| NTB ceilings | RX 12288, TX 15224, matching the dongle |
| `hold_apple_gadget` (bool, 0644, default N) | When set, `cdc_ncm_bind()` returns `-ENODEV` before touching the device, so there is no bus traffic at all |

`hold_apple_gadget` was built to keep cdc_ncm off the car until iAP2 authentication finished. That is what
the commercial dongle does: it `insmod`s cdc_ncm only when CarPlay starts.

- `host/mod.rs` sets the parameter before the switch and releases it after SessionReady, then binds the
  interface through `/sys/bus/usb/drivers/cdc_ncm/bind`.
- It works mechanically but did not help: 0 of 3 held attempts authenticated. It is off by default
  (`RASCAR_NCM_HOLD`).
- Earlier module builds became `[permanent]`: the `struct module` layout did not match because
  `CONFIG_DEBUG_INFO_BTF_MODULES` was missing from the headers. If `modprobe -r cdc_ncm` returns EBUSY,
  check this.

### 4.3 The bulk-OUT stall was our own CDC-NCM

- **Symptom.** In 10 of 11 streaming runs, one iAP2 bulk-OUT URB, usually our 16-byte
  `IdentificationAccepted`, took 378–1359 ms (mean 752 ms). No other USB traffic happened during the stall.
- **Cause.** cdc_ncm's SET_INTERFACE and descriptor reads landed 10–57 ms before the stall. The car's
  gadget serialises its NCM bring-up against iAP2 bulk traffic.
- **Fix.** After claiming iAP2, `wait_for_ncm_bind` (`host/mod.rs`) waits up to 1.5 s, polling every
  10 ms, for the cdc_ncm netdev to appear under the car's sysfs node. cdc_ncm registers the netdev last,
  after the alt switch, so its presence proves the alt switch is done. Only then does iAP2 start.

### 4.4 systemd-networkd: keep everything else off the link

`01-rascar-carplay-ncm.link`:

```ini
[Match]
Driver=cdc_ncm

[Link]
NamePolicy=kernel
MACAddressPolicy=none
```

`01-rascar-carplay-ncm.network`:

```ini
[Match]
Driver=cdc_ncm

[Link]
Unmanaged=yes
```

Why:

- **`NamePolicy=kernel`.** udev used to rename `usb0` → `enu1i1` up to 4.8 s after the netif appeared, while
  it was already UP and configured. Bring-up then waited on a name that no longer existed.
- **`Unmanaged=yes`.** The distribution's catch-all netplan config ran a DHCP client, IPv6 privacy
  addresses and LLMNR on the car link, and sometimes flushed our static IPv4.
- **`MACAddressPolicy=none`** keeps the kernel's random MAC from the patch. That this is required is
  **inferred, not tested**.

Only cdc_ncm devices match, so Ethernet and Wi-Fi are untouched.

### 4.5 Bring-up (`host/ncm.rs`, `bring_up_ncm`)

1. **Pick the netif** whose `/sys/class/net/<if>/device` sits under the car's USB device. Rescan up to 10
   times, 500 ms apart. Otherwise use a new USB netif that was not in the pre-switch snapshot.
2. **Per-interface sysctls**, under `/proc/sys/net/ipv6/conf/<if>/`:

   | Knob | Value | Why |
   |---|---|---|
   | `accept_dad`, `dad_transmits` | 0 | With DAD, our fe80 stays *tentative* for about 1 s, and connecting to :7000 fails with `EADDRNOTAVAIL` |
   | `optimistic_dad` | 1 | |
   | `accept_ra`, `autoconf`, `router_solicitations` | 0 | |
   | **`addr_gen_mode`** | **0** | The kernel builds an EUI-64 link-local from the MAC. With `Unmanaged=yes`, nothing else creates one; `1` (no link-local) stalled every attempt. |

   Neighbour sysctls, under `/proc/sys/net/ipv6/neigh/<if>/`: `delay_first_probe_time=0`,
   `retrans_time_ms=100`, `mcast_solicit=100`. Without them, link-local connects can stall for up to about
   20 s.
3. `ip link set <if> up`; `resolvectl llmnr <if> no`; `resolvectl mdns <if> no`. This keeps one identity on
   the link.
4. Add a static IPv4 `.2/24` in the car's private subnet. The car runs a DHCP server at `.1` and also
   browses over IPv4. CarPlay itself uses IPv6.
5. Wait for our `fe80::` address: poll every 100 ms, for up to 10 s.
6. **Find the car's link-local** (`car_link_local`). Ping `ff02::1` on the interface, then read
   `ip -6 neigh show dev <if>`. The car is the only other node on the link. Up to 8 polls, 200 ms apart.

**Scope ids matter.** Every `fe80::` address is carried as `fe80::…%<ifname>`. `airplay/rtsp.rs`
(`resolve_endpoint`) refuses to dial a bare `fe80` with no interface, because `scope_id=0` gives
`EINVAL`/`ENETUNREACH`.

### 4.6 Early NCM (off)

The default path brings NCM up **after** iAP2 SessionReady.

`RASCAR_EARLY_NCM=1` instead runs the bring-up concurrently with the iAP2 handshake, so the
`_carplay-ctrl` announcement goes out before the car's one-shot browse (5.3).

- **Never join that task ahead of the iAP2 service loop.** Doing so once starved the loop, and the car
  dropped us 1.2 s after SessionReady.
- It is off because of the rename race. `NamePolicy=kernel` has since fixed that race, but the default was
  never flipped back.

### 4.7 What does NOT matter (NCM)

- cdc_ncm causing the hub's "disabled by hub (EMI?)" port disable: refuted. It still happened with
  cdc_ncm held off the device.
- TCP back-pressure: zero-window was 0 in every run.
- Global autoprobe tricks: these break enumeration (2.5).

---

## 5. mDNS / Bonjour

Implemented in `host/mdns_ctrl.rs` (packet builder), `host/ctrl_advertise.rs` (responder and control
listener) and `host/ncm.rs` (`discover_airplay`).

### 5.1 What the car advertises

The car advertises `_airplay._tcp`, instance `CarPlay`, port **7000**, on host `localhost.local`. Its TXT
record:

| Key | Value |
|---|---|
| `srcvers` | `280.33.12.1` |
| `model` | `Jaguar F-PACE` |
| `flags` | `0x4` |
| `features` | `0x44440380,0x41`: bits 7, 8, 9, 18, 22, 26, 30, 32, 38 |
| `pi` | a UUID |
| `fv` | firmware version |

Feature bits that matter:

- **Bit 37 (CarPlayControl) is clear.** This car does not dial the phone (see 5.3).
- Bit 38 (HKPairingAndEncrypt) is set. That does **not** mean pairing is mandatory (see 6.10).

`_carplay._tcp` is empty on this car.

**The car's TCP 7000 accepts connections only on its IPv6 link-local.** A dial to its advertised IPv4 gets
a SYN and nothing back.

So the default path skips mDNS discovery entirely. It dials `[car fe80]:7000` straight from the neighbour
table, which saves about 0.8 s. The fallback path runs `avahi-browse -r -p -t -k _airplay._tcp` (8 s
window) and rewrites any non-fe80 answer to the car's link-local.

`connect_best` (`airplay/carplay_session.rs`) races TCP connects, 3 s each, to:

- the advertised address;
- `.1` of our IPv4 /24;
- every fe80 neighbour.

**We dial the car. The car never connects to us.**

### 5.2 What we advertise: `_carplay-ctrl._tcp`

The car browses `_carplay-ctrl._tcp.local` as soon as NCM traffic starts: 6–7 queries within 1 ms, QU and
QM, over v4 and v6. We answer like an iPhone would. The packet is built by `mdns_ctrl.rs` and is
byte-for-byte equal to a real iPhone's 399-byte announcement, which a golden test pins.

The header is ID 0, flags `0x8400` (response, authoritative), no questions.

Answer section (cache-flush is set on all records except the last three):

1. SRV `iPhone._carplay-ctrl._tcp.local.`: priority 0, weight 0, port **7100**, target `iPhone.local.`
2. AAAA `iPhone.local.`: our NCM link-local.
3. A `iPhone.local.`: our `.2` IPv4, only if present.
4. PTR `<nibbles>.ip6.arpa.` → `iPhone.local.`
5. TXT for the instance: `id=<device id>`, `srcvers=566.25.21`.
6. PTR `_carplay-ctrl._tcp.local.` → instance.
7. PTR `_services._dns-sd._udp.local.` → service.
8. TXT `iPhone._device-info._tcp.local.`: `model=D84AP`.

Additional section: NSEC records for the instance ({TXT, SRV}), the host ({AAAA} or {A, AAAA}) and the
reverse name ({PTR}).

Other details:

- TTL is 4500 s.
- `srcvers` **must equal** the RTSP `sourceVersion` and `User-Agent: AirPlay/566.25.21`.
- Env overrides: `RASCAR_CTRL_INSTANCE`, `RASCAR_CTRL_HOSTLABEL`, `RASCAR_CTRL_SRCVERS`.

**Why a raw responder rather than avahi.** Avahi's announcement had no NSEC records, no reverse PTR, and a
TTL of about 120 s. Our responder:

- binds `[::]:5353` (and `0.0.0.0:5353`) with `SO_REUSEADDR` + **`SO_REUSEPORT`**, so it coexists with
  avahi-daemon;
- uses `SO_BINDTODEVICE` to the NCM interface, joins `ff02::fb` / `224.0.0.251`, hop limit 255;
- is **authoritative without probing**: three sends 250 ms apart, an answer to every query mentioning
  `_carplay-ctrl` or our host label, and a re-announce every 2 s.

The **host label** defaults to `iPhone`. `RASCAR_CTRL_HOSTLABEL` can set it to the board's own avahi host
name, so that one address has one name on the link.

We also listen on TCP **7100** (dual stack). `GET /ctrl-int/1/connect` with an `AirPlay-Receiver-Device-ID`
header gets a bare `200 OK`.

### 5.3 What the car does and does not dial

- The car browses `_carplay-ctrl` **once**, about 3.6 s after the role switch in the early captures. Our
  first answers were 50–200 ms late, so this looked like the blocker. Later the car re-queried with known
  answers (TTL about 4487), which showed it had cached our record.
- **Across every archived run, the car has never opened TCP 7100**: raw responder or avahi, early or late,
  with or without an A record. That matches feature bit 37 being clear.
- The advertisement is kept because a real iPhone sends it. It is not on the critical path.
- Waiting for the invite (`RASCAR_WAIT_FOR_INVITE_MS`, default 0) only delays the session.

---

## 6. The CarPlay (AirPlay) session

Implemented in `airplay/carplay_session.rs` (`run_carplay`), `airplay/rtsp.rs` (RTSP client),
`airplay/hid.rs` (HID descriptor parser), `stream/crypto.rs`, `stream/video.rs`, `stream/audio.rs`,
`stream/call.rs`, `stream/screen.rs` and `stream/carkeys.rs`.

This is the **legacy receiver path**: plaintext RTSP, no pairing, no `fp-setup`, no FairPlay.
`airplay/fairplay.rs` and the pairing code exist, but the car path calls FairPlay **never** and pairing
only as a fallback (6.10).

### 6.1 RTSP conventions

- Request line: `METHOD /path RTSP/1.0`, with bare paths such as `/setup`, `/info`, `/` and `/command`,
  never `rtsp://…`.
- Headers, in order:
  - `CSeq` (starting at 1);
  - `User-Agent: AirPlay/566.25.21`;
  - `Content-Type: application/x-apple-binary-plist` and `Content-Length`, **only when there is a body**.
    A bodyless RECORD or `/feedback` carries neither.
- No `X-Apple-*` headers on the legacy path.
- Response timeout 10 s; audio SETUP, `modesChanged` and audio TEARDOWN use 1.5 s.
- A reply with an older CSeq than expected is kept aside (at most 8) and picked up later. Without this,
  `/feedback` once consumed a late audio-SETUP reply.

### 6.2 The ordered sequence

**Step 0: timing responder.** Bind a UDP socket on `[::]:0` and start answering timing requests (6.8)
**before** SETUP. Screen SETUP hangs if the car cannot sync time.

**Step 1: `SETUP /setup`** (`build_initial_setup`), with a binary-plist body:

| Key | Value |
|---|---|
| `deviceID`, `macAddress` | our device id (MAC-format string) |
| `model` / `name` | `iPhone8.1` / `iPhone` |
| `osName` / `osVersion` / `osBuildVersion` | `iPhone OS` / `15.0.2` / `19A404` |
| `sourceVersion` | `566.25.21` |
| `sessionUUID`, `sessionCorrelationUUID` | fresh random uppercase UUIDs |
| `timingPort` | our UDP timing port |
| `statsCollectionEnabled`, `updateSessionRequest` | false |
| `keepAliveLowPower` | true |
| **`et`** | **1** (`kAirPlayEncryptionType_None = 1 << 0`). Omitted on a post-pairing SETUP. |
| `isScreenMirroringSession` | false |
| `features` | empty array |

The reply gives us `eventPort` (required), `keepAlivePort` (optional) and `timingPort` (ignored, because
the car pings us). The UDP keepalive starts now (6.9). A 4xx reply sends us to the pairing fallback (6.10).

> **`et` must be 1, not 0.** `0` is `kAirPlayEncryptionType_Undefined`. The receiver treats any
> `et != None` as "encrypted, fetch a key". It answers **466 Key Management Error**, which looks exactly
> like "this car requires pairing". After the fix, 0 of 25 runs got a 466.

**Step 2: `GET /info`**, with an empty-dictionary bplist body. The reply is about 97 KB with embedded
icons.

| Key | Jaguar | Used for |
|---|---|---|
| `displays[0].widthPixels/heightPixels` | 1200 / 512 | capture and encode size |
| `displays[0].uuid` | per session | screen stream uuid |
| `displays[0].maxFPS` | 30 | not read; fps is our setting |
| `hidDevices[]` | touch screen, telephony keypad, consumer keypad | input decoding (6.6) |
| `audioFormats[]` | see 6.5 | choosing audio formats |
| `modes` | MainScreen Take (`transferType 1`, `takeConstraint 100`) | initial screen sync (6.3) |
| `keepAliveSendStatsAsBody` | **(value not recorded)** | data-channel keepalive form (6.4) |

> **The Jaguar sends numbers as strings** (`"100"`, `"0x8000"`). Parse integers, decimal strings and
> `0x` hex.

**Step 3: event channel.** **We** open TCP to `car:eventPort`. On this socket the roles are reversed: the
car sends requests (`POST /command RTSP/1.0` with a bplist body) and we answer
`RTSP/1.0 200 OK\r\nCSeq: n\r\n\r\n`.

- Commands seen from this car: `changeModes`, `requestUI`, `forceKeyFrame` and `hidSendReport`. Anything
  else is answered 200 and logged.
- We wait 150 ms for a `requestUI`, then carry on regardless. `requestUI` means "the user pressed the
  CarPlay button"; it is **not** a start signal.

**Step 4: `RECORD /`**, with no body.

**Step 5: `POST /command` `modesChanged`.** This declares that we own the resources (6.3). A non-2xx reply
is fatal; the Jaguar answers `200 {}` in about 4 ms.

**Step 6: `SETUP /` for the screen stream**, with this body:

```
{ streams: [ { type: 110, streamConnectionID: <random i64 in 1..2^63-1>,
               latencyMs: 25, uuid: <displays[0].uuid> } ] }
```

The reply's `streams[type==110].dataPort` is the TCP port for video. Connect to it (`TCP_NODELAY`). The car
never sends anything useful back on it; input comes over the event channel.

**Step 7: video.** Send VideoConfig, then the first IDR, then frames (6.4).

**Step 8: `SETUP /` for audio** (type 100), on its own task once video runs, so it can never stall video
(6.5).

**Steady state:**

| What | How often |
|---|---|
| `POST /feedback` | every 1 s |
| data-channel KeepAlive | every 1 s |
| UDP keepalive | every 1 s |
| timing replies | on demand |
| `modesChanged` | after every accepted `changeModes` |

### 6.3 Modes and resources

**`modesChanged` body** (`modes_params`):

```
{ type: "modesChanged",
  params: {
    resources: [ { resourceID: 1 /*MainScreen*/, entity: E, permanentEntity: P },
                 { resourceID: 2 /*MainAudio*/,  entity: E, permanentEntity: P } ],
    appStates: [ { appStateID: 2 /*phoneCall*/,  entity: 0|1 },
                 { appStateID: 3 /*turnByTurn*/, entity: 0 },
                 { appStateID: 1 /*speech*/,     entity: 0, speechMode: -1 } ] } }
```

Entities are 0 = none, **1 = controller (phone)**, 2 = accessory (car). By default we own both
resources (1/1). `phoneCall` is 1 while a telephony stream is wanted.

> **Never tell the car it owns MainAudio and then SETUP audio.** An earlier build gave MainAudio to the
> car and still opened an audio stream. The Jaguar never answered that SETUP. It stopped reading video
> 1.1 s later and pulled USB 2.3 s after that. `RASCAR_CARPLAY_AUDIO=off` hands MainAudio to the car
> **and** skips the audio SETUP; both follow from one setting.

**The car's `changeModes`** looks like this:

```
{ type: "changeModes",
  params: { reasonStr, resources: [ { resourceID, transferType, transferPriority, …Constraint } ] } }
```

| `transferType` | Meaning | Our answer |
|---|---|---|
| 1 | Take | refused, `{status: 1}`. Not modelled (catplay accepts it and tears down). |
| 2 | Untake | `{status: 0, params: <modes>}` |
| 3 | Borrow | accepted; `entity` becomes 2 while `permanentEntity` stays 1 |
| 4 | Unborrow | accepted; back to 1/1 |

Examples from the Jaguar:

- `"Request Main Audio"`: `{resourceID 2, transferType 3, transferPriority 500, unborrowConstraint 500}`,
  later `"Release Main Audio Request"` (Unborrow);
- `"Request Screen"` and `"Release screen"`, for example around the reverse camera;
- a borrow at every switch-off.

**After accepting a `changeModes`, reply first, then send a fresh `modesChanged`** with the resulting state
(MainAudio 2/1 during a borrow, 1/1 after). Without it, audio stayed silent after the first borrow for the
rest of a 70-minute drive. Audio SETUP is held back while the car borrows MainAudio, for at most 60 s.

**Initial screen sync.** If `/info.modes` shows MainScreen as Take with `takeConstraint 100`, first send a
`modesChanged` with MainScreen **2/2** (echoing the car's state), then the normal 1/1. Apple's spec says
the accessory reads the initial state as a transfer to itself, so the car then sees a transfer back to the
phone. This is meant to make CarPlay take the screen at start-up. It is built and passes bench tests;
**on the car it is unverified**. Kill switch: `RASCAR_INITIAL_SCREEN_SYNC=off`.

**`requestUI`** has four cases:

| Situation | What we do |
|---|---|
| We handed the screen to the car | take it back (below) |
| The car only borrowed the screen | clear the borrow, force a keyframe, re-announce modes |
| After any accepted `changeModes` | force a keyframe, re-announce modes. This fixed the car staying minutes on its own UI after a borrow and release. |
| Otherwise | log only |

**Handing the screen back to the native UI** (`stream/screen.rs`; trigger `screen car` / `screen phone` on
UDP `127.0.0.1:7704`):

- **Give away:** stop the encoder and drop the data socket, then
  `TEARDOWN / {streams:[{type:110, uuid}]}`, then `modesChanged` with MainScreen 2/2. Touches are ignored
  while the car owns the screen.
- **Take back:** `modesChanged` 1/1, then a **new** `SETUP` type 110 with a new `streamConnectionID` and
  therefore a new key, then restart the encoder and send VideoConfig and an IDR.

Built and bench-tested. Whether the car keeps a session alive indefinitely with no screen stream is
**unverified**.

### 6.4 Screen stream: frame header, encryption, H.264

**128-byte frame header, little-endian:**

```
offset  size  field
0       4     body_size  (u32 LE, payload length)
4       1     opcode
5       3     small_param[3]      (small_param[1] = byte 6 = flags)
8       120   params[15]          (u64 LE each; params[i] at 8 + 8*i)
```

| Opcode | Name | Header contents | Payload |
|---|---|---|---|
| **1** | VideoConfig | byte 6 `|= 0x04`; width as f32 LE at [16..20], height as f32 LE at [20..24] (the encoded size) | `avcC`, **plaintext** |
| **0** | VideoFrame | `params[0]` (bytes 8..16) = timestamp, NTP 32.32 since stream start | one access unit in AVCC form (4-byte BE length per NAL), **encrypted** |
| **2** | KeepAlive | `body_size` 0 | none |
| **5** | KeepAlive with stats | `body_size` 42; f32 42.0 at [124..128] | 42-byte bplist of an empty dictionary |

Details:

- Only flag `0x04` is set. `RespectTimestamps` (`0x02`) and `ShowHUD` (`0x01`) are not, so the car shows
  frames on arrival.
- `avcC` = `01 sps[1] sps[2] sps[3] FF E1 <len16> SPS 01 <len16> PPS`, with no trailer.
- Ordering: VideoConfig first, then an IDR access unit carrying SPS and PPS; P-frames before it are
  dropped. Each access unit is assembled from the Annex-B stream and sent as **one** VideoFrame (AUD NALs
  dropped).
- **Keepalive:** opcode 5 if `/info.keepAliveSendStatsAsBody` is true, else opcode 2, every 1 s. An
  analysis of a Harman receiver suggests a zero-body frame does not refresh its activity timer **(car
  value of this key not recorded)**.
- A write blocked for more than 2 s ends the session ("car stalled").

**Encryption, legacy `et=1` session: AES-128-CTR** (`stream/crypto.rs`). Let `id` be the
`streamConnectionID` **we sent in SETUP**, formatted as an unsigned decimal string. Then:

```
key = SHA-512( "AirPlayStreamKey" || id || 16 zero bytes )[0..16]
iv  = SHA-512( "AirPlayStreamIV"  || id || 16 zero bytes )[0..16]
```

The cipher is AES-128 in CTR mode with a big-endian 128-bit counter starting at `iv`.

- Only VideoFrame payloads are encrypted. VideoConfig and keepalives do not consume keystream.
- The keystream is **continuous across frames**. The leftover bytes of a partial block are used by the
  next frame, never skipped. `body_size` equals the plaintext length (no tag).
- Golden vector, `id = "0"`: key `51376cf5cf85231963cec9ba9f4806b6`, iv `025641101bed4a53c861a33a234074e2`.
- A classic bug is keying with id 0 while advertising a random id. Every frame then decodes as garbage.

**Paired sessions** (only after the 6.10 fallback) use **ChaCha20-Poly1305** instead:

- key = HKDF-SHA512(pair-verify shared secret, salt `"DataStream-Salt" || id`, info
  `"DataStream-Output-Encryption-Key"`), 32 bytes;
- nonce = 4 zero bytes || LE64 counter;
- AAD = the full 128-byte header, with `body_size` = payload + 16;
- output = ciphertext || 16-byte tag.

**H.264** (`stream/video.rs`) uses ffmpeg + libx264 in software; the RK3588 has no mainline H.264 encoder.

```
-pix_fmt yuv420p -profile:v baseline            (no -level: libx264 picks one; 3.1 is illegal above 720p)
-preset superfast -tune zerolatency              (ultrafast disables deblocking → blocky)
-b:v/-maxrate/-bufsize <kbps>                    (default 3000 kbps)
-g K -keyint_min K -sc_threshold 0 -bf 0         (K = 15 → an IDR every 0.5 s at 30 fps)
-x264-params repeat-headers=1 -f h264            (SPS/PPS with every IDR)
```

- fps defaults to 30 (`RASCAR_VIDEO_FPS`; 20–25 is used in the car to save power); bitrate is set with
  `RASCAR_VIDEO_BITRATE_KBPS`, keyint with `RASCAR_VIDEO_KEYINT`.
- The encoder captures the dashboard via `x11grab` or Wayland screencopy (`stream/wlcapture.rs`), sized
  to the car's display.
- **Every frame IDR is not needed.** All-IDR was used early on and made no difference to session life, and
  a GOP gives far better quality (PSNR 35.6 → 56.4 dB measured). libopenh264 (`constrained_baseline`) is the
  fallback encoder. HEVC is **untested**.
- On `forceKeyFrame` from the car we resend VideoConfig with the next IDR, which is at most one GOP away.

### 6.5 Audio streams

**SETUP body** (`build_audio_setup`):

```
{ streams: [ { type: 100, audioType: "media" | "alert" | "telephony",
               audioFormat: <bit>, input: <bool>,
               streamConnectionID: 0,           (random > 0 only on a paired session)
               dataPort: <our port>,            (telephony only: where the car sends the mic)
               audioLatencyMs: 100, supportsRTPPacketRedundancy: false,
               supportsHighAccuracyTimestamps: true, controlPort: <our RTCP port> } ] }
```

`audioType` is a **string**. There is no `uuid` and no per-stream key. The reply's `streams[type==100]`
gives the car's `dataPort` (and optionally its `controlPort`).

**Formats** are chosen from the car's `audioFormats` list. The bits come from Apple's `AirPlayCommon.h`.

| audioType | Jaguar lists | We use |
|---|---|---|
| media | `0x8000` | bit 15: PCM 48 kHz / 16-bit / stereo (the only media format offered) |
| telephony | in/out `0x54` | first of bit 6 (24 kHz), bit 4 (16 kHz), bit 8 (32 kHz), bit 14 (48 kHz), mono → **24 kHz** |
| alert | `0x800` | first of bit 11 (44.1 kHz stereo), bit 15, bit 10, bit 14 → **44.1 kHz stereo** |
| speechRecognition | `0x40` | not used |
| compatibility (types 100/101) | `0x54` / `0x854`, `0x8000` | not used |

**RTP over UDP** to `car:dataPort`:

- 12-byte header: `80`, PT **100**, seq BE, timestamp BE, **SSRC 0**.
- 352 frames per packet. At 48 kHz stereo that is 1408 bytes, one packet every 7.33 ms.
- **PCM is 16-bit big-endian.** Little-endian plays as noise.
- When the source is silent for more than 200 ms we send zeros, so the stream never stops.
- On `et=1` audio is plaintext. On a paired session it is ChaCha20-Poly1305: packet =
  `RTP header || ciphertext || tag(16) || nonce(8 = LE64 counter)`, AAD = BE64 `(ts << 32) | ssrc`.

**Telephony and the car microphone.** The telephony SETUP has `input: true` and our `dataPort`. The car
then sends microphone RTP with the same framing. We feed it into a PulseAudio source.

- Order: `modesChanged(phoneCall=1)`, then the stream.
- To switch: TEARDOWN the media stream, then `modesChanged`, then SETUP telephony. The reverse order
  applies when the call ends.
- **Alert (ringtone):** a separate stream, set up only if the car lists the format.
- Whether the car accepts telephony and alert streams has only been bench-tested **(unverified on the
  car)**.

**Audio TEARDOWN:** `TEARDOWN / {streams:[{type:100}]}`.

The Jaguar answers audio SETUP in 340–410 ms and plays about 250 ms behind what we send.

### 6.6 Input: touch and steering-wheel keys

**We register no HID devices.** The car declares its own in `/info.hidDevices[]` (`uuid`, `name`,
`hidDescriptor`). It then sends reports over the event channel:

```
POST /command  { type: "hidSendReport", uuid: <device uuid>, hidReport: <data> }
```

`airplay/hid.rs` parses each descriptor. **Never hard-code report bytes**: usages are resolved from the
descriptor.

| Jaguar device | Usage | Report |
|---|---|---|
| Touch Screen | Digitizer `0x0D:0x04` | 5 bytes: bit 0 tip switch, X u16 LE, Y u16 LE, logical **0…32767**, no report id |
| Telephony keypad | Telephony `0x0B:0x01` | 1-byte array: 1 Hook Switch `0x20`, 2 Flash `0x21`, 3 Drop `0x26` |
| Consumer keypad | Consumer `0x0C:0x01` | 1-byte array: 1 Play `0xB0`, 2 Pause `0xB1`, 3 Scan Next `0xB5`, 4 Scan Previous `0xB6` |

How reports are handled:

- Key fields are declared `Input(Constant, Array)` even though they change. The parser ignores the
  constant bit when usages are present.
- Level reports are turned into press/release edges.
- **Touch** coordinates are scaled from the 32768×32768 logical range to the capture size. An earlier build
  scaled from the display size, so every touch landed in the bottom-right corner.
- **Keys** go to the dashboard as UDP datagrams (`key next`, `key hook`, …) on `127.0.0.1:7705`
  (`stream/carkeys.rs`).

The Jaguar has no rotary knob and declares no iAP2 HID component, so the event channel is its only input
path.

### 6.7 Feedback and keepalives

| Channel | What | Interval |
|---|---|---|
| RTSP control | `POST /feedback`, no body | first one immediately, then every 1 s |
| UDP `keepAlivePort` | 4 zero bytes (`AirPlayIdleKeepAlivePacket`; version bits must be 0) | every 1 s (`RASCAR_KEEPALIVE_SECS`) |
| Screen data channel | opcode 2 or 5 | every 1 s |

Notes:

- `/feedback` replies contain per-stream timing. `sampleTime` 0 twice in a row is normal, and an audio `sr`
  of about 47999 shows the car consuming audio.
- Per Apple's receiver source, the UDP idle keepalive matters only when no stream is open.

### 6.8 Timing (AirPlay NTP over RTCP)

The car sends 32-byte requests to our `timingPort`: version 2, PT `0xD2`, length 7, then four NTP-format
stamps. The origin is in the **last** one (t3). We reply with:

```
80 D3 00 07 | rtp_ts = 0 | t1 = request.t3 | t2 = now | t3 = now
```

"Now" is NTP 32.32: a monotonic clock plus 2 208 988 800 s. The car sends a burst of 5 requests, then one
every 2–3 s.

### 6.9 Teardown

At the end of a session: stop all tasks and the encoder, drop the data socket, then send
`TEARDOWN / {streams:[{type:110, uuid}]}`.

On this car the session normally ends with the ignition. The car never sends a FIN or RST: it goes quiet
and the USB link disappears.

### 6.10 Pairing fallback (for receivers that need it)

This runs only if `SETUP /setup` returns 4xx (`airplay/pairing.rs`). The Jaguar no longer goes down this
path since `et=1`.

1. Open a **new** TCP connection. A receiver that has sent a 466 stops servicing that socket.
2. Load the persistent controller identity (`RASCAR_HAP_STORE`, default `/var/lib/rascar/hap_identity`).
   - If the accessory's key is known, run **pair-verify only**. Re-running pair-setup every time fills the
     head unit's controller list until it refuses to connect.
   - If pair-verify fails, forget the key and run pair-setup.
3. **pair-setup:**
   - `POST /pair-setup`, `Content-Type: application/pairing+tlv8`, header `X-Apple-HKP: 2`.
   - SRP-6a with the 3072-bit RFC 5054 group, g = 5, SHA-512, user `Pair-Setup`, PIN `3939`.
   - M5/M6 are encrypted with HKDF(`Pair-Setup-Encrypt-Salt`/`-Info`), nonces `PS-Msg05`/`PS-Msg06`.
4. **pair-verify:**
   - `POST /pair-verify` with `X-Apple-HKP: 2` and `X-Apple-PD: 1`; X25519 + Ed25519, nonces
     `PV-Msg02`/`PV-Msg03`.
   - Control keys: HKDF(shared, `Control-Salt`, `Control-Write-Encryption-Key` / `Control-Read-Encryption-Key`).
5. **RTSP is then framed** as blocks of at most 1024 bytes: `[u16 LE len][ciphertext][16-byte tag]`, with
   AAD = the length bytes and nonce = 4 zero bytes || LE64 counter (separate TX and RX counters).
6. Send `SETUP /setup` again **without `et`**. Streams are then ChaCha20-Poly1305 (6.4, 6.5).
7. The event channel uses `Events-Salt`. Its keys are swapped because we are the server on that socket.

`/auth-setup` (MFi-SAP) is sent only with `RASCAR_AUTH_SETUP=1`; the Jaguar answers 403. This path is
bench-tested against an open receiver, not used on the car.

### 6.11 What does NOT matter (AirPlay)

- **"466 means pairing is required"** and **"features bit 38 means pairing is mandatory"**: wrong. The 466
  came from `et=0`.
- **The UDP keepalive interval.** Keepalive → silence: mean 1.28 s, sd 0.70, no relation. The 1 s interval
  is harmless.
- **All-IDR video**, `/feedback` cadence, timing, the event channel, encryption: none of them moved the
  cutoff.
- **Waiting for `/ctrl-int/1/connect`** or for `requestUI` before starting.
- **Bluetooth or HFP state** for a wired session: not required.
- **`/auth-setup`**: not required, and refused.

---

## 7. Failure modes and the car's clocks

The head unit keeps several timers that are not documented anywhere. This section lists what we measured
and what fixed each problem, as **symptom → cause → fix**.

**How to measure.** Always measure against the kernel's `USB disconnect` timestamp. Never use the car's
1 Hz GPS burst as a proxy, because it is quantised to 1 s. Never trust a timing from a single run.
[reverse-engineering-notes.md](reverse-engineering-notes.md) describes the capture pipeline.

### 7.1 The 6-second cutoff

**Symptom.** Everything works: iAP2 authenticates, NCM comes up, RTSP succeeds and video frames flow.
Then, about 6 s after the car enumerated, all car endpoints go silent at the same instant. About 2 s later
the car disconnects from USB. There is no iAP2 RST, no TCP FIN and no error message. At best the dashboard
was visible for 2–4 s.

**What was measured** over 24 runs, with usbmon, the network pcap and iAP2 traces aligned:

- The car **decides** at `D = max(enumeration + 3.82 s, AuthenticationSucceeded + 2.14 s)`. 17 of 24 runs
  fall within ±70 ms. On the network plane, `D` = first NCM traffic + 2.10 s (sd 0.11).
- At `D`, every car IN endpoint stops at once: iAP2 `0x81`, NCM `0x83`, and NCM OUT `0x04` is no longer
  accepted.
- The car **detaches exactly 2.09 s later** (sd 0.05). This is a clean `CONNECTION 1→0`. Port power stays
  on and there is no over-current, so the car's own software is shutting the link down.
- Nothing at the iAP2 or AirPlay layer moves `D`. Only reaching authentication or NCM later moves it.

**Cause and fix.** The car's device advertises **USB2 Link Power Management** (LPM + BESL), and Linux
enables hardware LPM by default. Disabling it for the car with the usbcore quirk flag `k`
(`8086:9999:ak`, `host/role_switch.rs`) gave the **first session that outlived `D`**, the same evening.
Every drive since has had sessions lasting the whole trip: 16–17 minutes, 70+ minutes, and one 78-minute
session.

**Status.** Fixed in practice. The causal link is before/after evidence, not a controlled A/B: nobody has
re-run with only `a` since, because nobody wants to break a working car.

**What does NOT matter for the cutoff.** All of these were tested; the correct ones are kept as fixes.

| Hypothesis | Verdict |
|---|---|
| Unanswered iAP2 subscriptions, identity updates, the ACK-only seq bug, the SLP bit, the NowPlaying heartbeat | fixed, no effect on `D` |
| AirPlay: `/feedback`, keepalive interval, timing, the event channel, pairing, `et`, all-IDR video | no effect |
| Missing "second enumeration" after auth (Apple's flow) | refuted: a forced reset after auth killed the car's device for good |
| A Harman-style power reset of the phone after the role switch | refuted: `POWER=1`, `OC=0`, and an orderly 2.09 s shutdown |
| A 6 s OTG/charger timer in the RK3588 USB2 PHY driver | never confirmed, and incompatible with `D` moving when protocol progress moves **(unverified, superseded)** |
| The late `_carplay-ctrl` answer to the car's one-shot browse | refuted: the car never dials it anyway |
| tcpm flipping the role back | refuted while fusb302 is unbound |
| Bus suspend or autosuspend | never happens |

### 7.2 The car's other clocks

| Clock | Value | Consequence |
|---|---|---|
| Enumeration → iAP2 SYN\|ACK | the car drops USB about **4 s** after enumerating without one | claim the interface fast (25 ms poll) and answer the SYN at once |
| Car SYN retransmit | 1 s | the SYN\|ACK must answer whichever SYN is current |
| iAP2 retransmit timeout | **500 ms** (from the SYN) | use the negotiated value. A fixed 1500 ms wasted about 0.9 s per lost packet before auth. |
| Pre-auth stall vs session | before the LPM fix, each second of pre-auth stall cost about 0.5 s of post-auth window | keep the handshake tight. The NCM-bind wait (4.3) removed a 0.4–1.4 s stall. |
| Cold start: configuration → `40 51` | **22–27 s** after power-on, on the same enumeration | do not re-enumerate in that window (7.3) |
| Car awake: `C0 53` → `40 51` | about 0.7 s | |
| Role switch → car enumerates | 0.55 or 1.1 s | wait up to 20 s anyway |
| Post-auth re-presentation | 0.3–0.5 s, sometimes | re-acquire without touching the role (2.4) |
| Car timing sync | burst of 5, then every 2–3 s | the timing responder must run before screen SETUP |
| Audio SETUP answer | 340–410 ms, once 3.2 s | match replies by CSeq (6.1) |
| Device mode (iAP2 over HID) | the car never detaches | |

### 7.3 The role-switch latch (car stops sending `40 51`)

**Symptom.** The car enumerates us and replays the usual requests byte for byte (`21 0A`, HID descriptor,
`C0 53` answered `01 00 00 00`, `40 40`). Then it never sends `40 51`. A replug, "forget device" on the head
unit and an ignition cycle did not always clear it.

**Causes found:**

1. **A constant USB serial after failed attempts.** A fresh serial broke the latch the first time it was
   tried. The serial is now random and persisted, and stays the same while things work.
2. **Re-enumerating during the car's cold-boot decision window.** An early watchdog re-presented the gadget
   (UDC `soft_connect` off/on) 3, 6 and 12 s after `configured`. On a cold start the car sends `40 51` only
   22–27 s after power-on, so a re-presentation landing in that window **wedged the car's CarPlay stack**.
   The car then answered every enumeration but never sent `40 51` again: through 10 re-presentations and
   two board reboots, until the head unit reset its own bus.
3. A different open-source transmitter's gadget got the identical truncated sequence from this car. So
   the latch itself is head-unit behaviour; we can only avoid provoking it.

**Fix** (`usb/ffs.rs`): wait **45 s** (`ROLE_SWITCH_GRACE`) after `configured` before the first
re-presentation, then double the wait up to 60 s, with 1 s off the bus each time. Override with
`RASCAR_ROLE_SWITCH_GRACE_MS`.

**Status.** Mitigated. That the 45 s grace avoids the wedge on cold starts is **pending verification**
across enough cold starts. The only proven way to clear an already wedged car is for the head unit to
reset its bus.

### 7.4 Car still booting

**Symptom.** On a cold start the first attempt "fails": we are configured, `C0 53` and `40 40` arrive, then
nothing.

**Cause.** The head unit's CarPlay service is not up yet. Meanwhile it treats us as an iPod or charger.

**Fix.** Wait (see 7.3); `40 51` arrives on the same enumeration.

### 7.5 `et=0` is Undefined, not None

**Symptom.** Every `SETUP /setup` got **466**. We fell into PIN pairing on every attempt. After enough
attempts the head unit showed its own "Connection impossible" message, because its controller list had
filled up.

**Cause.** `kAirPlayEncryptionType_Undefined = 0`, `None = 1`. The receiver requests a key whenever
`et != None`, and the only exit from that branch is 466.

**Fix.** Send `et = 1` (`encryption_type_none()` in `airplay/carplay_session.rs`). That gave 0 of 25 runs
with a 466 and no pairing.

### 7.6 Audio contradiction and silent audio after a borrow

- **Symptom:** the audio SETUP is never answered; the car stops reading video about 1 s later.
  **Cause:** `modesChanged` gave MainAudio to the car while we SETUP a MainAudio stream.
  **Fix:** one setting decides both (`claims_main_audio`).
- **Symptom:** audio is silent for the rest of the drive after the car borrows and returns MainAudio.
  **Cause:** no `modesChanged` was sent after accepting `changeModes`.
  **Fix:** `send_state_modes_changed` (6.3).
- **Symptom:** every RTSP request receives the previous request's reply.
  **Cause:** a late audio-SETUP reply (3.2 s), read without checking CSeq.
  **Fix:** match replies by CSeq (`read_response_for`, `take_late_reply`).

### 7.7 USB bring-up problems on the host side

| Symptom | Cause | Fix |
|---|---|---|
| `can't set config #1, error -71`; the car drops off | upstream cdc_ncm switches the NCM alt setting 10–20 ms after SET_CONFIGURATION | patched cdc_ncm: `msleep(220)` (4.2) |
| cdc_ncm "failed to get mac address", no netif | `iMACAddress` is not a MAC | random-MAC fallback (4.2) |
| 4 s hang then `-71` on a string read | 64-byte string with no ZLP | quirk `a` (2.3); never `lsusb -v` the car |
| `IdentificationAccepted` never completes; car leaves at about 5.3 s | the cdc_ncm alt switch lands during the iAP2 handshake | `wait_for_ncm_bind` (4.3) |
| Bulk OUT `-71` during a cold handshake | the car NAKs while busy | `recover()` + resend (3.4); re-acquire the car (2.4) |
| dwc3 drops host mode about 5 s in | tcpm reacts to the car's VBUS cycle | unbind `typec_fusb302` (2.1) |
| `No SYN from the accessory after 46.5 s` | a 4G stick on another controller claimed as "the car" | only devices under `fc000000.usb` (2.2) |
| The netif vanishes mid bring-up | udev renames `usb0` → `enu1i1` while it is UP | `.link` with `NamePolicy=kernel` (4.4) |
| Our static IPv4 flushed; DHCP and privacy addresses on the link | the distribution's catch-all netplan | `.network` with `Unmanaged=yes` (4.4) |
| No IPv6 link-local at all | `Unmanaged=yes` + `addr_gen_mode=1` | `addr_gen_mode=0` (4.5) |
| `EADDRNOTAVAIL` dialling `:7000` | our fe80 still *tentative* (DAD) | DAD off (4.5) |
| Later runs do not enumerate at all | a global `drivers_autoprobe=0` survived a crashed process | the run wrapper forces `drivers_autoprobe=1` before every attempt |

### 7.8 Power, storage and heat (the board in a car)

These are not protocol issues, but they ended more sessions than any protocol bug once the cutoff was
fixed.

**NVMe brownouts.**

- Symptom: `nvme nvme0: controller is down; will reset: CSTS=0xffffffff`, then ext4 aborts and the root
  becomes read-only. The log stops in the middle of a live session.
- Cause: the board and NVMe sag on the car's USB-C power under load. Software H.264 costs about 0.66 of a
  core at 30 fps.
- Mitigations:
  - `nvme_core.default_ps_max_latency_us=0 pcie_aspm=off pcie_port_pm=off`;
  - a dedicated 12 V → 5 V supply;
  - lower fps (`RASCAR_VIDEO_FPS`), a CPU cap during attempts (`RASCAR_CPU_MAX_KHZ`) and time-limited
    pcap capture (`RASCAR_CAPTURE_SECONDS`).

**Dead boots.**

- Symptom: the journal stops 6–7 s into boot, and every unit that executes from the root filesystem fails.
- Cause: removing the udev CPU cap made udev coldplug every device at once. 8 of 22 boots then failed,
  against 0 of 18 with the cap. The power-sag explanation is a hypothesis.
- Fix: `setup/boot/50-rascar-cpuquota.conf` (`CPUQuota=10%`).

**NVMe heat.** With APST disabled the drive stays in PS0 (6.2 W).
`setup/nvme/rascar-nvme-power.service` sets operational PS2 (2.1 W): 75 → 69 °C.

**Board heat.** `setup/thermal/` steps CPU frequencies down at 72/78/83/88 °C. Fit a fan.

**Instant power cuts at ignition-off.**

- There is never a clean shutdown.
- Mitigations: ext4 `commit=5`, `fsck.repair=yes`, journald `SyncIntervalSec=5s`, and atomic
  temp+fsync+rename writes for state files.
- With no working RTC, the clock is set from the car's `$GPRMC` (3.5.2).

See [hardware.md](hardware.md) for the power and cooling recommendations.

---

## 8. Credits

- [usenocturne/iap2-rs](https://github.com/usenocturne/iap2-rs) (GPL-3.0): the iAP2 packet, link and
  control code, adapted here to the device side.
- [omarroth/doubletake](https://github.com/omarroth/doubletake) (LGPL-3.0): the RTSP client, binary plist,
  HomeKit pairing, FairPlay and stream cipher building blocks in `airplay/` and `stream/crypto.rs`.
- [catplay-labs/catplay](https://github.com/catplay-labs/catplay): a reference for nearly every
  session-level detail on this page, including:
  - session order, the 128-byte screen frame, modes and resources, stream descriptions;
  - the ACK-only seq fix, iAP2 subscription-reply layouts and Now Playing value types;
  - the `_carplay-ctrl` golden packet.
- [lvalen91/carplayd](https://github.com/lvalen91/carplayd) and
  [shilapi/xcertplay](https://github.com/shilapi/xcertplay): iAP2 message identifiers and directions.
- A public USB capture of a commercial dongle against an aftermarket head unit, from the Cynthion
  issue tracker: the SYN|ACK counter-proposal values.
- Commercial CarPlay dongle firmware: the NCM bind delay, the NTB sizes and the HID report descriptor.

See [THIRD_PARTY.md](../THIRD_PARTY.md) for licences, and
[reverse-engineering-notes.md](reverse-engineering-notes.md) for how all this was found.
