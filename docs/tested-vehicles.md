# Tested vehicles

OrangeCar has been tested end to end in **one car**. Every timer and quirk the code works around was
measured on it. Other cars will differ, which is why reports are the most useful contribution right now
(see [the template](#report-template) at the end of this page).

| Car | Head unit | CarPlay receiver | Status |
|---|---|---|---|
| Jaguar F-PACE | InControl Touch Pro (Harman) | Legacy wired CarPlay, `srcvers` 280.33.12.1 | **Works**: daily driving, sessions of 70+ minutes, video, media audio, touch, steering-wheel keys, car GPS, Now Playing, telephony audio |

## Jaguar F-PACE

### Identity

What the car reports about itself (values that identify a single car, such as serial numbers and Bluetooth
or MAC addresses, are left out):

| Source | Field | Value |
|---|---|---|
| iAP2 `IdentificationInformation` | Name / model / manufacturer | `Jaguar F-PACE` / `Jaguar F-PACE` / `Jaguar` |
| | Firmware / hardware | `S22A_22.32.4-…` / `IxC.1.0` |
| | Transports | USB host transport (iAP2 + CarPlay, CarPlay on interface 1), Bluetooth transport |
| | Components | Vehicle information (engine type gasoline), location (GPGGA, GPRMC, GPGSV, PASCD) |
| | Absent | Vehicle status (range, temperatures), route guidance display (no HUD or cluster guidance), HID components, wireless CarPlay |
| mDNS `_airplay._tcp` TXT | `srcvers` | `280.33.12.1` |
| | `features` | `0x44440380,0x41` |
| | `flags` | `0x4` |
| | `model` | `Jaguar F-PACE` |
| AirPlay `GET /info` | `OSInfo` | `Linux 2.2R2 x86` |
| | Display | **1200x512 px**, 225x96 mm (~135 dpi), `maxFPS` 30, features 4 (low-fidelity touch), primary input device = touch |
| | `nightMode` | always `1` (see quirks) |
| | Audio formats | media `0x8000` (48 kHz / 16-bit / stereo, the only media format); telephony in/out `0x54` (8/16/24 kHz mono); speech recognition `0x40`; alert `0x800` |
| | Audio latencies | 65-95 ms depending on the stream type |
| | HID devices | Touch screen, telephony keypad, consumer keypad (see below) |
| USB after role switch | Car's device | `8086:9999` "Wind River Systems" / "Gadget MFi v0.1", high speed, iAP2 bulk + CDC-NCM |
| iAP2 `PowerSourceUpdate` | Current offered to the phone | 2100 mA |

HID devices declared in `/info`:

| Device | Report | Meaning |
|---|---|---|
| Touch screen | 5 bytes: tip bit, 7 padding bits, X u16, Y u16, logical range 0..32767 | Single touch |
| Telephony keypad | 1-byte array | 1 hook switch, 2 flash, 3 drop |
| Consumer keypad | 1-byte array | 1 play, 2 pause, 3 scan next, 4 scan previous |

There is no play/pause toggle, no volume (the car's amplifier handles it) and no rotary knob. Build the key
mapping from the descriptor; other head units put these keys at different indexes.

### Measured timeline of a working session

From the reference build's logs. `t` is time since `rascar-carplay` started; the service starts about 9.5 s
after power-on.

| Step | Typical |
|---|---|
| Gadget (fake iPhone) bound | t+2.0 s |
| Car configures it (configuration 2 "iPod USB Interface"), sends `C0 53` then `40 40` (charging request) | t+2.4 s |
| Car sends the role switch `40 51 wValue=1` | ~170 ms after configuration if the car was already running; **22-27 s after car power-on on a cold start** |
| Board in host mode, car re-enumerates as `8086:9999` | +0.4 s |
| iAP2 `SessionReady` (identification + authentication) | +1 to 4 s |
| AirPlay `SETUP screen` | +0.5 to 0.7 s |
| First video frame (`STREAMING dashboard to car`, AES-128-CTR) | +0.2 to 0.7 s |

So with the car already running, the dashboard appears 2-6 s after the role switch. From a cold start the
car's own boot dominates: about half a minute.

### Quirks and what the code does about them

| Quirk | Symptom without the workaround | Workaround |
|---|---|---|
| **USB2 LPM.** The car's gadget advertises USB 2.0 Link Power Management + BESL. | The car stops servicing its endpoints and detaches a few seconds after iAP2 authentication (the "6-second cutoff"). | `rascar-carplay` writes the usbcore quirk `8086:9999:ak` (`k` = no LPM) before the car re-enumerates. This was the last change before the first sustained session. Several other fixes were also needed (below). |
| **Non-hex MAC in the NCM descriptor.** | Stock `cdc_ncm` logs `failed to get mac address` and creates no network interface. | Patched `cdc_ncm` (`kernel/cdc_ncm/`) falls back to a random MAC. |
| **The car serialises its NCM bring-up against iAP2.** | Our `cdc_ncm`'s `SET_INTERFACE` landing during the iAP2 handshake stalled a bulk-OUT write for 0.4-1.4 s, eating the session budget. | NCM is brought up after authentication; the iAP2 service loop starts the moment the session is ready. |
| **Cold-boot decision window.** On a cold start the head unit configures the "iPhone" early but only sends the role switch 22-27 s after power-on. | Re-presenting the gadget inside that window (an early version did so after 3 s) wedged the car's CarPlay stack: it answered every enumeration but never switched again, across reboots of the board, until the head unit reset its USB bus. | Wait 45 s before the first re-presentation, then every 60 s (`RASCAR_ROLE_SWITCH_GRACE_MS`). |
| **Car still booting.** Plugged in during the head unit's boot, it may treat the device as a charger only (`40 40`, no `40 51`). | No CarPlay until replug. | The same re-presentation watchdog gives the car a fresh enumeration later. |
| **Screen ownership at start.** `/info` says the car *takes* the main screen with take constraint "anytime". | CarPlay is connected but the car stays on its native UI until the driver presses the CarPlay icon. | Announce the car's state first (`modesChanged` screen 2/2), then take it (1/1), as a transfer. Kill switch `RASCAR_INITIAL_SCREEN_SYNC=off`. When the car starts on its own start-up screen (borrow), it releases the screen and sends `requestUI` by itself. |
| **Borrows.** The car borrows the screen or main audio (`changeModes`, transfer type 3) for its own popups, then releases them. | Refusing the borrow, or not re-announcing the resulting state, left audio silent for the rest of the session; pressing the CarPlay icon after a borrow reset the USB link. | Accept borrows and unborrows, send our own `modesChanged` after each accepted `changeModes`, and on `requestUI` after a borrow take the screen back with a fresh keyframe. |
| **`/info` numbers are strings** (`"type":"100"`, `"0x8000"`). | Telephony formats were read as missing: the car microphone was never used. | The plist parser accepts decimal and `0x` hex strings. |
| **Late RTSP replies.** The car sometimes answers the audio `SETUP` 3 s late, bundled with the next reply. | Audio disabled for the whole session, and every later request read its predecessor's reply. | Replies are matched by `CSeq`; a late audio reply is still picked up. |
| **Buttons only via AirPlay HID.** | Steering-wheel keys ignored. | `hidSendReport` key reports are decoded from the car's descriptors and relayed to the dashboard. |
| **Touch in logical units.** | Every touch landed in one corner when scaled against 1200x512. | Scale from the HID logical range (0..32767). |
| **`nightMode` is constant 1**, and the car never sends `setNightMode` or `limitedUI`. | Day/night theming cannot follow the car. | The dashboard uses its own theme logic. `$PASCD` gear is a better "parked" signal than anything in CarPlay. |
| **The car never dials `_carplay-ctrl`.** | Waiting for the car's connect invite stalls the session. | The board dials the car's AirPlay receiver directly over IPv6 link-local. |
| **Abrupt power cut.** The car cuts USB power at switch-off with no teardown and no warning. | Data written in the last seconds is lost; journals stop early. | See power-cut handling in [hardware.md](hardware.md#storage-nvme). |
| **Native media integration.** The head unit looks for a UPnP/DLNA media server on the USB network (SSDP `M-SEARCH`, Cinemo). | None. | Not used. Could be an alternative audio path; untested. |

The car's iAP2 identification lists exactly which messages it sends and accepts. Sending a message that is
not on those lists (vehicle status, route guidance) is a protocol violation; this car tore sessions down for
less. Check the lists before adding a feature.

## Report template

Copy this into an issue titled "Vehicle report: <make> <model> <year>". Remove anything that identifies
your car or you: VIN, serial numbers, Bluetooth and MAC addresses, the `pi` / `deviceID` values from mDNS
and `/info`, and GPS sentences.

```markdown
### Vehicle
- Make / model / year:
- Head unit (name, supplier if known):
- Head unit firmware (iAP2 firmware field, serial removed):
- CarPlay receiver `srcvers` and `features` (from `avahi.log` or the run log):
- Display from `/info` (pixels, mm, maxFPS):
- Car's USB device after role switch (VID:PID, product string):
- Which USB port did you use (CarPlay port marked with a phone/CarPlay icon?):

### Board
- Board + RAM:
- OS image and kernel:
- Power supply used for the board:
- Cable (type, length, direct or through anything):
- Commit / release of OrangeCar:

### How far did it get?
Tick the last step that worked (grep markers from `/var/lib/rascar-debug/latest/runs/*-carplay.log`):
- [ ] Car enumerated the board (`FFS event ENABLE`)
- [ ] Car asked for role-switch support (`C0 53`)
- [ ] Car sent the role switch (`CARPLAY ROLE-SWITCH REQUEST`) after ___ s
- [ ] Board became host, car re-enumerated (`ROLE-SWITCH RESULT ... mode_after_300ms=host`)
- [ ] iAP2 session ready (`SessionReady`)
- [ ] NCM link up, car found over mDNS (`_airplay._tcp` in avahi.log)
- [ ] Screen streaming (`STREAMING dashboard to car`)
- [ ] Dashboard visible on the car screen
- [ ] Touch works
- [ ] Media audio heard (`audio: level sent to the car ... rms=` non-zero)
- [ ] Steering-wheel keys work
- [ ] Session survives > 5 min
- [ ] Takes the screen by itself at car start

### Quirks seen
- Cold start: seconds from car power-on to role switch:
- Did the car ever stop answering role switches? What reset it?
- Anything in `changeModes` / `requestUI` that behaves differently:
- Car GPS sentences received (types only):

### Logs
Attach a trimmed run log (`runs/NNN-HHMMSS-carplay.log`) and `kernel.log` from the boot directory,
or a bundle from `sudo rascar-bundle 1`, **after removing personal data**.
```
