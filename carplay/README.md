# rascar-carplay — the CarPlay transmitter

`rascar-carplay` makes an Orange Pi 5 Plus (RK3588) behave like an iPhone on a
car's wired CarPlay USB port. It enumerates as an Apple device, answers the
car's USB role-switch request, becomes USB host, speaks iAP2 to the head unit,
brings up the CDC-NCM link, finds the car's CarPlay receiver over mDNS and then
runs a CarPlay/AirPlay session that streams the OrangeCar dashboard (rendered by
`rascar-app` in a Chromium kiosk) to the car's screen, with audio, touch,
steering-wheel keys, Now Playing and the car's GPS coming back.

No MFi authentication coprocessor is involved: the car is the iAP2
*accessory*, the board is the *device*, and the device side is the verifier.

Tested on a Jaguar F-PACE (Harman InControl head unit, legacy CarPlay receiver
`srcvers 280.33.12.1`). Other head units may need different knobs, which is
why almost every protocol decision below can be toggled through an environment
variable without rebuilding.

The protocol findings behind the code are written up in
[`docs/protocol.md`](../docs/protocol.md). Many code comments also record the
measurements (dates, capture names) that led to each decision.

## Modules

| Path | What it does |
|------|--------------|
| `src/main.rs` | Orchestrator: CLI, phase sequencing, retries, re-enumeration handling, bench entry points. |
| `src/usb/` | **Phase 1, USB device side.** `gadget.rs` builds the configfs gadget (Apple VID/PID, four iPhone-shaped configurations, persisted USB serial); `ffs.rs` runs the FunctionFS ep0 loop and answers the vendor requests up to the CarPlay role switch (`40 51 wValue=1`); `hid_iap2.rs` is an optional iAP2-over-HID responder; `aoa_probe.rs` is an Android Open Accessory / mySPIN persona probe (`RASCAR_PERSONA=android`). |
| `src/iap2/` | **Phase 2, iAP2 device stack** (transport-generic). `packet.rs`/`link.rs` = link layer (SYN/ACK negotiation, framing, retransmit); `control.rs` = control session (identification, accept-all authentication, post-auth notifications, location); `nowplaying.rs` = Now Playing updates fed from `rascar-app`'s `/api/media`. |
| `src/host/` | **Phase 3, USB host side.** `role_switch.rs` flips the DWC3 controller to host; `usb_host.rs` finds the car and opens the iAP2 bulk pipe; `ncm.rs` brings up the CDC-NCM interface (IPv6 link-local + the car's IPv4 /24) and discovers `_airplay._tcp` / `_carplay._tcp`; `ctrl_advertise.rs` + `mdns_ctrl.rs` answer the car's `_carplay-ctrl._tcp` browse with a byte-exact iPhone-style mDNS record and accept its control connection. |
| `src/airplay/` | **Phase 4, CarPlay/AirPlay control.** `rtsp.rs` (RTSP + binary plist), `pairing.rs` (HomeKit pair-setup SRP-6a / pair-verify, persisted identity), `fairplay.rs` + `fairplay_data.rs` (FairPlay SAP), `carplay_session.rs` (SETUP, `/info`, RECORD, `modesChanged`, resources, keepalive, screen/audio stream setup, HID routing), `hid.rs` (HID report descriptors and key decoding). |
| `src/stream/` | **Phase 5, media.** `render.rs` starts the dashboard (Xvfb + Chromium, or a Wayland kiosk); `video.rs` captures and H.264-encodes it into AirPlay screen-stream packets; `crypto.rs` encrypts frames (AES-128-CTR / ChaCha20-Poly1305); `wlcapture.rs` is the Wayland screencopy capture; `audio.rs` captures PulseAudio output for the car; `call.rs` handles the car microphone and the ringtone for phone/meeting calls; `input.rs` maps car touch onto the dashboard; `carkeys.rs` forwards steering-wheel media keys to `rascar-app`; `screen.rs` hands the screen back to the car's native UI and takes it again. |
| `src/headunit.rs` | Publishes the head unit's self-description (iAP2 identification + USB power offer) as JSON for the other services. |
| `src/gps_clock.rs` | Sets the system clock from the car's `$GPRMC` when NTP is not available (the board has no RTC). |
| `src/debug.rs` | Phase timers, wire tracing, the `env_flag` helper used by most knobs. |

## Run phases

1. **Apple enumeration.** Create the `carplay` configfs gadget, mount one
   FunctionFS instance per configuration, bind the UDC and answer ep0 until the
   car sends the CarPlay role-switch request.
2. **Role switch.** Acknowledge it, wait `RASCAR_ROLE_SWITCH_DELAY_MS`, remove
   the gadget and switch the USB-C port to host mode.
3. **iAP2 + NCM.** Enumerate the car as a USB device, open its iAP2 bulk
   interface and run the device side of iAP2: link negotiation,
   identification, accept-all authentication, post-auth notifications,
   location and Now Playing subscriptions. The CDC-NCM interface comes up in
   parallel, and the car re-enumerates once after authentication, which is
   normal.
4. **CarPlay control.** Find the car's receiver (mDNS, or a direct dial on the
   link if no record shows up), pair, run `SETUP`, `GET /info`, `RECORD` and
   the first `modesChanged`.
5. **Streams.** Screen stream (H.264, encrypted), main audio / alert /
   telephony audio streams, touch and HID back from the car, keepalive, screen
   hand-back on request.

Requirements: root (configfs, FunctionFS, usbfs, raw sockets), the RK3588 DWC3
peripheral overlay and the patched `cdc_ncm` module from `kernel/cdc_ncm/`.
See `setup/` for the systemd unit and the board setup.

### Command line

```bash
sudo rascar-carplay [--debug]                       # full run against a car
rascar-carplay --carplay-endpoint HOST:PORT         # bench: CarPlay session (phases 4-5) against a receiver
rascar-carplay --airplay-endpoint HOST:PORT         # bench: AppleTV-style AirPlay/FairPlay path
rascar-carplay --mdns-selftest 'fe80::1%eth0'       # bench: run only the _carplay-ctrl responder ~30 s
rascar-carplay --audio-probe                        # bench: 3 s of car-audio capture, print the level
```

`--debug` raises the log level; `RASCAR_WIRE_TRACE=1` adds hex dumps of USB,
iAP2 and RTSP traffic. Log lines carry grep-able markers such as
`Phase <X> START`, `ROLE-SWITCH RESULT`, `iAP2 LINK STATE`.

## Environment variables

Booleans read through `env_flag` accept `1/on/true/yes` and `0/off/false/no`.
Variables marked *opt-in* only react to `1/on/true/yes`; *opt-out* ones only
to `0/off`. Unless stated otherwise, an unset or unparsable value means the
default.

### USB device side (phase 1)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_USB_SERIAL` | unset | Pin the USB serial (exactly 40 hex characters). Unset: use the persisted random serial. |
| `RASCAR_USB_SERIAL_STORE` | `/var/lib/rascar/usb_serial` | File holding the persisted random serial. Delete it to get a new identity. |
| `RASCAR_ROTATE_SERIAL` | off (opt-in) | Generate a new serial after a failed attempt, so the head unit sees a new iPhone. Ignored when the serial is pinned. |
| `RASCAR_UAC1` | on (`off` disables) | Let the kernel's `f_uac1` provide the audio interfaces of configuration 2 with proper class descriptors. |
| `RASCAR_HIDE_CARPLAY` | off (opt-in) | Answer `C0 53` with "no role-switch capability" (present as an iPhone without CarPlay). |
| `RASCAR_IAP2_HID` | off (opt-in) | Run the device-mode iAP2-over-HID responder on configuration 2. |
| `RASCAR_STAY_DEVICE_REENUM` | off (opt-in) | Soft-reconnect the gadget after the car's "stay USB device" request (`40 51 wValue=0`). |
| `RASCAR_ROLE_SWITCH_GRACE_MS` | `45000` (min 500) | How long to wait for the role-switch request before re-presenting the gadget. |
| `RASCAR_PERSONA` | unset | `android` runs the Android Open Accessory / mySPIN probe instead of CarPlay. |
| `RASCAR_AOA_HANDSHAKE` | on (opt-out) | AOA probe: answer the car's opening handshake (off = only log). |
| `RASCAR_AOA_PING` | on (opt-out) | AOA probe: send the mySPIN ping once the accessory pipe is open. |
| `RASCAR_AOA_SERIAL` | `RASCAR0AOA0PROBE1` | AOA probe: USB serial string. |

### USB host side, NCM and mDNS (phase 3)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_ROLE_SWITCH_DELAY_MS` | `10` (max 2000) | Delay between acknowledging the role switch and switching to host. |
| `RASCAR_EARLY_NCM` | off (opt-in) | Bring the NCM link up during the iAP2 handshake instead of after authentication. |
| `RASCAR_NCM_HOLD` | off (opt-in) | Use the patched module's `hold_apple_gadget` parameter to keep `cdc_ncm` off the car until iAP2 authentication is done. |
| `RASCAR_DEFER_NCM_BIND` | off (opt-in) | Older, known-broken way of deferring the `cdc_ncm` bind (bench only). |
| `RASCAR_POST_AUTH_RESET` | off (opt-in) | Port-reset the car's USB device right after iAP2 authentication. |
| `RASCAR_REENUM_TIMEOUT_MS` | `1500` (max 30000) | How long to wait for the car to re-enumerate after authentication. |
| `RASCAR_ENUM_THEN_IDLE_MS` | unset | Control experiment: enumerate the car, then do nothing for this many ms and exit. |
| `RASCAR_CTRL_VIA_AVAHI` | off | Publish `_carplay-ctrl._tcp` through avahi instead of the built-in raw responder. |
| `RASCAR_CTRL_INSTANCE` | `iPhone` | `_carplay-ctrl` service instance name. |
| `RASCAR_CTRL_HOSTLABEL` | `iPhone` | Host label used in the `_carplay-ctrl` record. |
| `RASCAR_CTRL_SRCVERS` | `566.25.21` | `srcvers` TXT value of the `_carplay-ctrl` record. |
| `RASCAR_CTRL_DEVICE_ID` | built-in device MAC | `id` TXT value, avahi path only (the raw responder always uses the built-in MAC). |

### iAP2 (phase 3)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_IAP2_GATE` | on | Never send a message the accessory did not list in `MessagesReceivedFromDevice`. |
| `RASCAR_IAP2_GPGSV` | on | Also request `GPGSV` sentences when the car offers them. |
| `RASCAR_IAP2_KEEP_LOCATION` | on | Keep the location subscription open (off sends `StopLocationInformation` right after starting it). |
| `RASCAR_IAP2_HEARTBEAT` | off | Re-send the Now Playing state every 1.2 s during a session. |
| `RASCAR_IAP2_WIRELESS_CARPLAY` | unset (message omitted) | `available` / `unavailable`: send `WirelessCarPlayUpdate` (0x4E0D) with that value. |
| `RASCAR_NOWPLAYING` | on | Send Now Playing updates built from `rascar-app`'s media state. |
| `RASCAR_NOWPLAYING_ARTWORK` | off | Also send album artwork (needs Now Playing on). |
| `RASCAR_NOWPLAYING_ADDR` | `127.0.0.1:8080` | Where `rascar-app` listens (`/api/media`). |
| `RASCAR_NOWPLAYING_APP` | empty in the first update, `OrangeCar` afterwards | App name reported in Now Playing. |
| `RASCAR_NOWPLAYING_BUNDLE` | empty in the first update, `org.orangecar.dashboard` afterwards | App bundle id reported in Now Playing. |
| `RASCAR_CAR_GPS_PATH` | `/var/lib/rascar-debug/car-gps.nmea` | Append-only file receiving the car's NMEA sentences (read by `rascar-app`). |
| `RASCAR_HEADUNIT_PATH` | `/var/lib/rascar/headunit.json` | Where the head unit description is written. |

### CarPlay session (phase 4)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_SKIP_PHASE4` | off | Split experiment: keep iAP2 serviced but never start AirPlay. |
| `RASCAR_AIRPLAY_FALLBACK` | on | Dial the car directly on the NCM link when no AirPlay mDNS record appears. |
| `RASCAR_WAIT_FOR_INVITE_MS` | `0` | Wait up to this long for the car's `/ctrl-int/1/connect` invite before dialling. |
| `RASCAR_HAP_STORE` | `/var/lib/rascar/hap_identity` | Persisted pairing identity and learned accessory key (pair-setup once, pair-verify afterwards). |
| `RASCAR_AUTH_SETUP` | off (only `1` enables) | Perform `/auth-setup` (MFiSAP) for receivers that require it. |
| `RASCAR_ET` | `1` | `et` (encryption type) value sent in SETUP; 1 = none. |
| `RASCAR_CARPLAY_AUDIO` | unset | `on`/`off`: claim MainAudio and open the audio stream, overriding `RASCAR_MODES_CLAIM_AUDIO`. |
| `RASCAR_MODES_CLAIM_AUDIO` | on | Claim MainAudio in `modesChanged` (and therefore open the audio stream). |
| `RASCAR_INITIAL_SCREEN_SYNC` | on | First restate the car's initial screen ownership before taking the screen, when the car's take constraint allows it. |
| `RASCAR_KEEPALIVE_SECS` | `1` (clamped 1..30) | Interval of the keepalive ping to the receiver's `keepAlivePort`. |
| `RASCAR_PHONE_RING_CALLSTATE` | off | Assert `phoneCall` in `modesChanged` while ringing, not only during a call. |

### Streams (phase 5)

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_DISPLAY` | `:0` | X display the dashboard is rendered on and captured from. |
| `RASCAR_COMPOSITOR` | unset (Xvfb) | `wayland` runs the dashboard in a headless Wayland kiosk on the GPU instead of Xvfb. |
| `RASCAR_RENDER_NODE` | `/dev/dri/renderD128` | GPU render node for the Wayland kiosk. |
| `RASCAR_VIDEO_FPS` | `30` | Screen stream frame rate. |
| `RASCAR_VIDEO_BITRATE_KBPS` | `3000` | H.264 bitrate. |
| `RASCAR_VIDEO_KEYINT` | `15` | Keyframe interval in frames. |
| `RASCAR_AVCC_TRAILER` | on | Append the 4-byte trailer to the AVCC configuration record. |
| `RASCAR_SCREEN_HUD` | off | Ask the receiver to draw its own diagnostic HUD over the stream. |
| `RASCAR_RESPECT_TIMESTAMPS` | off | Ask the receiver to schedule frames on their timestamps instead of displaying on arrival. |
| `RASCAR_SCREEN_HANDBACK` | on | Honour requests to hand the screen back to the car's native UI. |
| `RASCAR_AUDIO_SOURCE` | unset (default output monitor) | PulseAudio source captured for the car. |
| `RASCAR_CARPLAY_MIC` | on | Allow switching the car to telephony audio to use its microphone. |
| `RASCAR_PHONE_RING` | on | Play the ringtone on the car for incoming calls. |
| `RASCAR_TOUCH_PARSER` | unset | `experimental` enables the unverified reverse-channel touch parser in `stream/input.rs`. |

### Debug and build

| Variable | Default | Meaning |
|----------|---------|---------|
| `RASCAR_WIRE_TRACE` | off (only `1` enables) | Hex-dump USB bulk transfers, iAP2 headers/messages and RTSP bodies. |
| `RASCAR_BUILD_ID` | `dev` | **Compile-time** (`option_env!`): build identifier printed at start-up. |

## Building and testing

```bash
cargo check -p rascar-carplay
cargo test  -p rascar-carplay
```

The unit tests run on any Linux host; they use captured byte sequences from the
tested car (identifiers such as the head unit's serial number are replaced by
same-length placeholders). Cross-compile for `aarch64-unknown-linux-gnu` to run
on the board.

## Licensing

This crate is **GPL-3.0** (see [`LICENSE`](../LICENSE)).

- The iAP2 stack in `src/iap2/` is derived from
  [usenocturne/iap2-rs](https://github.com/usenocturne/iap2-rs) (GPL-3.0):
  `packet.rs` nearly verbatim, `link.rs` framing verbatim with negotiation
  inverted to the device role, `control.rs` framing kept with identification
  and authentication inverted. Vendoring that code makes the whole crate
  GPL-3.0. Details in the header of `src/iap2/mod.rs`.
- The AirPlay sender in `src/airplay/` (`rtsp`, `pairing`, `fairplay`,
  `fairplay_data`) and the frame encryption in `src/stream/crypto.rs` are
  ported from [omarroth/doubletake](https://github.com/omarroth/doubletake)
  (LGPL-3.0), which is compatible with GPL-3.0. The FairPlay code embeds
  Apple-derived constants and tables reproduced byte for byte; reproducing them
  is a legal gray area. Details in the header of `src/airplay/mod.rs`.
- [catplay-labs/catplay](https://github.com/catplay-labs/catplay) was used as
  a reference for the wire format: InitialSetup fields, stream descriptions,
  modes/resources, iAP2 post-auth messages and Now Playing encoding, the
  `_carplay-ctrl` mDNS record, and some test vectors (the 399-byte mDNS
  announcement, an SRP-6a proof vector, a media-button HID descriptor). No
  catplay code is included; the implementation here is written independently.

Apple, CarPlay, AirPlay, iPhone and FairPlay are trademarks of Apple Inc. This
project is not affiliated with or endorsed by Apple.
