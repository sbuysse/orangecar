# Troubleshooting

Every entry below comes from a real failure on the reference build (Orange Pi 5 Plus in a Jaguar F-PACE).
Start with [where the logs are](#where-the-logs-are), then find your symptom.

- [Where the logs are](#where-the-logs-are)
- [Getting into the board in the car: the debug hotspot](#the-debug-hotspot)
- [Boot and system](#boot-and-system)
- [Storage, power and heat](#storage-power-and-heat)
- [CarPlay connection](#carplay-connection)
- [Screen, audio and input](#screen-audio-and-input)
- [4G](#4g)
- [Bluetooth and OBD-II](#bluetooth-and-obd-ii)
- [Habits that save time](#habits-that-save-time)

## Where the logs are

The car cuts power without warning every time it is switched off. Anything held only in RAM is lost, and
a boot that fails is usually power-cycled by hand. So the logs that matter are written to the NVMe and synced.

**First, make sure the journal is persistent.** `setup/install.sh` does this in its power-cut hardening step; if you skipped it, do it by hand. Armbian ships `armbian-ramlog`, which keeps `/var/log` in a 50 MB
zram disk. With it, the journal of every car boot ends a few seconds in, and a boot that failed and was
power-cycled leaves nothing at all. You then only ever read the boots that went well. Turn it off:

```bash
sudo systemctl disable armbian-ramlog          # and set ENABLED=false in /etc/default/armbian-ramlog
sudo mkdir -p /etc/systemd/journald.conf.d /var/log/journal
printf '[Journal]\nStorage=persistent\nSystemMaxUse=200M\nSyncIntervalSec=5s\n' \
  | sudo tee /etc/systemd/journald.conf.d/10-persist.conf
```

Reboot, then check that `findmnt /var/log` prints nothing (no zram) and `journalctl --list-boots` grows.
Even then, journald may lose the last few seconds before a power cut, which is why the files below exist.

| Where | What | Written by |
|---|---|---|
| `/var/lib/rascar-debug/boot-<date>-<time>-<bootid>/` | One directory per boot; `latest` points at the current one. The newest 20 are kept. | `rascar-debugcap` |
| `…/runs/NNN-HHMMSS-carplay.log` | **One file per CarPlay attempt.** Phase markers, every USB control request, role switch, iAP2, RTSP, modes, audio levels. Start here. | `rascar-carplay-run` |
| `…/runs/NNN-HHMMSS-net.pcap0` | Packets on the car's NCM link (first 60 s by default, `RASCAR_CAPTURE_SECONDS`) | `rascar-carplay-run` |
| `…/kernel.log`, `udev.log`, `state.log` | Kernel messages; udev events with kernel and udev timestamps; UDC / Type-C / USB / network state changes | `rascar-debugcap` |
| `…/usbmon.pcapN`, `dwc3-trace.log` | USB bus captures (open in Wireshark) and dwc3 gadget tracepoints | `rascar-debugcap` |
| `…/avahi.log`, `ip-monitor.log`, `preflight.log`, `journal.log` | mDNS seen on the links, address/route changes, readiness checks, journal copy | `rascar-debugcap` |
| `/var/lib/rascar-debug/earlyboot/<date>-<bootid>.{kmsg,journal,progress}` | Kernel log and journal of the first 90 s of each boot, synced every second. A `.progress` file without a final `done: alive at N s` line marks a boot that died. The last 40 boots are kept. | `rascar-earlylog` |
| `/var/lib/rascar-debug/thermal.log` | Throttle level changes, temperatures once a minute | `rascar-thermal` |
| `/var/lib/rascar-debug/car-gps.nmea` | NMEA sentences received from the car | `rascar-carplay` |
| `/var/lib/rascar-debug/obd/` | Every ELM327 command and reply, rotated | `rascar-app` |
| journal | Everything else: `journalctl -b -u rascar-app`, `-u rascar-4g-qmi`, `-u rascar-obd-link`; `journalctl -b -1` for the previous boot | systemd |

Tips:

- **The wall clock is wrong early in a car boot.** There is no RTC battery, so the clock starts from
  `fake-hwclock` and jumps when the car's GPS (or NTP) sets it. Directory names can be misleading. Use the
  boot id and the `t+NNN ms` timestamps (time since the attempt started) in the run log.
- Run logs contain binary hex dumps. Use `grep -a`.
- Useful markers: `Phase <X> START|END`, `FFS event`, `ROLE-SWITCH`, `SessionReady`, `STREAMING`,
  `changeModes`, `requestUI`, `audio: level sent`, `soft reconnect`, `PANIC`.
- `sudo rascar-preflight` checks that the board is ready for a CarPlay run (prints PASS / WARN / FAIL).
- `sudo rascar-bundle [N]` packs the last N boot directories, the current journal and version info into
  one tarball under `/var/lib/rascar-debug/bundles/`.
- Live view over SSH: `journalctl -fu rascar-carplay`.

## The debug hotspot

In the car there is no Ethernet cable, so `rascar-hotspot` can open a Wi-Fi access point for SSH:

1. `setup/install.sh` installs and enables it when you accept the hotspot option (the default with
   `--yes`; `--without-hotspot` skips it). A random passphrase is generated in `/etc/rascar/hotspot.pass`.
   Read it at home: `sudo cat /etc/rascar/hotspot.pass`.
2. At boot it waits up to 45 s for an Ethernet carrier.
   If none shows up, it starts `hostapd` on the Wi-Fi card with SSID **`RasCar-debug`**, the board at
   **10.42.0.1** and a small DHCP pool. At home, with the cable in, nothing changes.
3. In the car, join `RasCar-debug` from a laptop or phone and `ssh <user>@10.42.0.1`.
4. By hand: `sudo rascar-hotspot on | off | status`.

Notes:

- **It delays `multi-user.target` by up to 45 s** when no cable is plugged in: it is a oneshot pulled in by
  `multi-user.target`, which waits for it. `rascar-carplay` and `rascar-app` are not ordered after it, so
  CarPlay is not delayed, but anything ordered after `multi-user.target` is. An early version that also
  waited forever pushed the whole boot past 80 s. Keep it while you debug, then
  `sudo systemctl disable rascar-hotspot` once the install works (`sudo rascar-hotspot on` still works by hand).
- The access point uses the M.2 Wi-Fi card. Without antennas you will need to stand next to the car.
- The access point and the Wi-Fi client share one radio. The script stops the client while the AP runs.

## Boot and system

| Symptom | Cause | Fix |
|---|---|---|
| Boot takes 80-90 s; dashboard and SSH come late | `wpa_supplicant@<wlan>` is ordered before `network.target`, and the Realtek Wi-Fi card only appears ~80 s after power-on (its init queues behind the whole driver probe). Everything after `network.target` waited for it. | Drop-in for `wpa_supplicant@<wlan>.service` with `[Unit]` `Before=` / `Before=shutdown.target` / `Wants=` (empty), so it connects in the background. Check with `systemd-analyze critical-chain`. |
| Boot hangs at ~10 s when there is no network cable (fine at home) | A unit ordered `After=network-online.target` is on the path to `multi-user.target` (on the reference board: `lxc-net`, left over from an Android container). `network-online.target` never completes without a routable link. | Remove or disable that unit; mask `systemd-networkd-wait-online` if nothing needs it. Look for it with `systemd-analyze critical-chain multi-user.target`. |
| Boot slow by up to 45 s in the car only | `rascar-hotspot` waiting for Ethernet | See [the debug hotspot](#the-debug-hotspot). |
| A failed boot left no trace in `journalctl` | Volatile `/var/log` (zram / log2ram). Only clean boots are on disk. | Make the journal persistent (see [above](#where-the-logs-are)) and read `earlyboot/` files. |
| Touch dead for ~60 s after boot; sound, Bluetooth and the 4G netdev also late | udev throttled too hard: every device is published 50-85 s after the kernel saw it. On the reference board a forgotten `systemctl set-property systemd-udevd CPUQuota=5%` from months earlier did this. | Measure first: compare kernel and udev timestamps in `udev.log` (`udevadm monitor`), and check `systemctl show systemd-udevd -p DropInPaths`. The setup uses one cap, `CPUQuota=10%`, in `/etc/systemd/system/systemd-udevd.service.d/50-rascar-cpuquota.conf`. GPU and sound are loaded earlier by `rascar-early-drivers.conf`. |
| Load average ~1.5 on an idle board | The battery-less HYM8563 RTC's interrupt line is stuck: ~4,500 interrupts/s (`/proc/interrupts`, `irq/…-hym8563` in D state) | `rascar-rtc-off.service` (unbind) + `/etc/modprobe.d/rascar-no-hym8563.conf`. |
| Load stays high after that, with nothing busy | Process churn: scripts forking `cat`/`sed`/`lsusb` every second. Check `processes` in `/proc/stat` over time. | Already fixed in `rascar-debugcap` and the media poller; check any script you add. |
| Clock wrong in the car; calendar shows the wrong day | No RTC battery. Time is NTP at home, the car's GPS in the car. | Automatic: `rascar-carplay` steps the clock from `$GPRMC` once a session runs, if NTP is not synchronised (`car clock` in the log). Before the car's GPS flows, `fake-hwclock` time is used. |

## Storage, power and heat

| Symptom | Cause | Fix |
|---|---|---|
| `nvme nvme0: controller is down; will reset: CSTS=0xffffffff`, then I/O errors, read-only root, programs dying with `SIGBUS` | The NVMe dropped off PCIe. Every case on the reference build was under load on a car-powered supply (installs, compiling) or under heat. | A dedicated 5 V supply of at least 4 A ([hardware.md](hardware.md#power)). Never compile or install on the board on car power; cross-compile. Keep the kernel arguments `nvme_core.default_ps_max_latency_us=0 pcie_aspm=off pcie_port_pm=off` and `rascar-nvme-power`. Afterwards run a forced fsck (`sudo tune2fs -E force_fsck /dev/<root>` and reboot): a drop can damage files without ext4 noticing. |
| Board dies ~6-7 s into boot in the car, never at home. On an HDMI screen systemd keeps going but almost every unit fails to start (`tmp.mount`, `systemd-tmpfiles-setup`, `ldconfig`…). No logs survive. | The NVMe drops at the moment all drivers probe together (~6 s with an uncapped udev). The current peak sags a marginal supply. 8 of 22 car boots died this way after the udev cap was removed; 0 of 18 before. | udev `CPUQuota=10%` (installed by the setup). Better supply. Set `verbosity=7` in `/boot/armbianEnv.txt` (kernel `loglevel=7`) so the NVMe/PCIe errors are printed on the console; `rascar-earlylog` cannot see them because the disk is already gone. |
| Root filesystem read-only after heavy use; board very hot | Overheating, then the NVMe dropped. The board has no fan by default, and nothing throttles before 115 °C. | `rascar-thermal` (steps the CPU down from 72 °C). Fit a fan and an NVMe heatsink. Do not run sustained full-CPU work (emulators, builds) on the board; cap experiments with `CPUQuota`. |
| `dpkg: error: reading package info file … Input/output error` long after a crash | Damage from an earlier NVMe drop | Offline fsck as above before trusting the package database. |
| NVMe very warm (controller > 70 °C) with almost no I/O | Drive kept in its maximum power state because APST is disabled | `rascar-nvme-power.service` (operational power state 2). A heatsink. |
| Files reset to defaults after a trip (settings, AirPlay pairing) | Written in place, truncated by the power cut | Fixed in code (temp file + fsync + rename). If you add persistent files, do the same. Mount the root filesystem with `commit=5`; `fsck.repair=yes` is on the kernel command line. |

## CarPlay connection

Read the attempt's run log from the top. The phase markers tell you how far it got.

| Symptom | Cause | Fix |
|---|---|---|
| Nothing at all: no `FFS event ENABLE` in the run log | The car does not see the gadget: wrong board port (use the USB-C **OTG** port), bad cable, a hub in between, the OTG overlay missing, or the car's port does not support CarPlay | Check the port and a direct cable. `sudo rascar-preflight`. `cat /sys/class/udc/*/state` should go to `configured` when plugged in. |
| Car configures the board (`C0 53` answered, `40 40`) but never sends the role switch `40 51`; log says `car configured us … but sent no CarPlay role switch` | Normal for a **cold start**: the head unit decides 22-27 s after it powers up. Also seen when the head unit is still booting (it treats the device as a charger). | Wait. The watchdog re-presents the gadget after 45 s, then every 60 s. Do not shorten `RASCAR_ROLE_SWITCH_GRACE_MS`: re-presenting inside the car's decision window wedged it. |
| The car never sends the role switch again, even across board reboots; it answers every enumeration instantly | The car's CarPlay stack is wedged (seen after re-presentations landed in its decision window) | Only a head-unit reset cleared it. Switch the car off, let it go to sleep, start it again. |
| Run log: the "car" was found within milliseconds, then `No SYN from the accessory` / iAP2 timeout | Another USB device (a 4G stick with a vendor bulk interface) was taken for the car | Fixed: only devices on the OTG controller (`fc000000.usb` in the sysfs path) are considered. If you port to another board or controller, update that check. |
| Session dies 4-6 s after the role switch, around `Authenticat…`, every time (port shows `CONNECTION 0`, power still on) | USB2 Link Power Management between the board and the car's gadget | The code writes the usbcore quirk `8086:9999:ak` (no LPM). Check `usb quirk … 8086:9999:ak` in the run log and `/sys/module/usbcore/parameters/quirks`. A different car will have a different VID:PID: add it. |
| `cdc_ncm … failed to get mac address`, no new network interface after authentication | The car's NCM descriptor has a MAC string the stock driver rejects | Install the patched `cdc_ncm` from `kernel/cdc_ncm/`. When loading it by hand, `modprobe cdc_ether` first (it needs its symbols). The log line then reads `invalid mac address string, using random …`. |
| NCM bring-up stalls; interface renamed late (`usb0` → `enu1i1`) or gets a DHCP client and loses its addresses | udev renaming and the generic `Name=e*` netplan network grabbing the car link | `01-rascar-carplay-ncm.link` (`NamePolicy=kernel`) and `01-rascar-carplay-ncm.network` (`Unmanaged=yes`), both matching only `Driver=cdc_ncm`. Installed by `install-carplay-debug.sh`. |
| A bulk write to the car takes ~1 s right after authentication, session runs out of time | Our `cdc_ncm` switching the NCM data interface during the iAP2 handshake; the car serialises the two | Fixed: NCM comes up after authentication. Keep it that way. |
| Connected, but the car stays on its own UI until you press the CarPlay icon | The car reported in `/info` that it takes the main screen; we announced ownership without a transfer | Fixed by the initial screen sync (screen 2/2 then 1/1). Check `initial sync` in the run log. Kill switch `RASCAR_INITIAL_SCREEN_SYNC=off`. |
| CarPlay lost after pressing a native button (e.g. HOME) and then the CarPlay icon | The car borrowed the screen; its `requestUI` afterwards got no answer and the car reset the link | Fixed: on `requestUI` after a borrow, take the screen back with a keyframe and a `modesChanged`. |
| Car drops USB 1-3 s after a fresh session, the retry holds | Not explained yet. Seen a few times on one morning. | The service retries on its own. Report it with the run log if it is frequent. |
| `usb usbN-port1: disabled by hub (EMI?)`, `error -71` on the car link | Signal problem on the cable | Shorter, better cable; no extension. |

## Screen, audio and input

| Symptom | Cause | Fix |
|---|---|---|
| Blank or frozen dashboard on the car screen while touch still moves things | Happened when the NVMe dropped: the page and its API stopped loading | See [storage](#storage-power-and-heat). Check `kernel.log`. |
| A mouse pointer is painted on the car screen | The board's IR receiver (`gpio_ir_recv`) declares mouse axes; cage draws a cursor for it | `setup/wayland/90-rascar-libinput-ignore.rules`. |
| Blocky video, smeared edges | All-keyframe stream at a fixed bitrate with `ultrafast` (no deblocking) | Fixed defaults: `superfast`, GOP 15, buffer = bitrate. Tune `RASCAR_VIDEO_BITRATE_KBPS`, `RASCAR_VIDEO_KEYINT`, `RASCAR_VIDEO_FPS`. |
| Dashboard animation looks like 2 fps in the car | A page element too expensive for the board (here: a canvas visualiser with a blur per LED segment, 8 fps at home, much less in the car where the encoder also runs) | Measure the page's frame rate over DevTools (`127.0.0.1:9222`, `requestAnimationFrame` counter) at home. The capture runs at 25 fps; anything slower is the page. |
| Video smooth on the wire but the car feels slow on a busy map | The car's decoder side under a high bitrate (moving map, 280+ kB/s) | Try a lower `RASCAR_VIDEO_BITRATE_KBPS` (e.g. 1500) or a longer `RASCAR_VIDEO_KEYINT`. |
| Every touch lands in one corner | Touch scaled against the screen size instead of the HID logical range | Fixed (0..32767). If your car's descriptor differs, check `hid_touch` in the run log. |
| No sound at all for the first minutes after boot | Sound driver loaded late by a throttled udev, or the car answered the audio `SETUP` late | `rascar-early-drivers.conf` loads sound at ~7 s. Late `SETUP` replies are handled (matched by `CSeq`). |
| Audio works, then goes silent for the rest of the session after a car popup | The car borrowed main audio and released it (`changeModes`); without our own `modesChanged` after each accepted change, its audio arbiter stayed muted | Fixed. Check the run log for `changeModes … accepted` followed by `modesChanged(after changeModes …)`. |
| No audio for the whole session; replies in the run log look shifted by one | The car answered `SETUP audio` after our 1.5 s timeout; replies were not matched by `CSeq` | Fixed in `rtsp.rs`. |
| YouTube Music plays in the car, but radio, TV and web pages are silent | Chromium runs as root and had no PulseAudio server, so it played into nothing. The TV `<video>` element was also `muted`. | Fixed: Chromium gets the user session's `PULSE_SERVER`/cookie (log: `render: Chromium audio → unix:…`). |
| Is it us or the car? | | `audio: level sent to the car … rms=` every 10 s. rms 0 means we sent silence (check the source and PulseAudio). rms > 0 and nothing heard means the car is not playing our stream (modes, borrow, its volume). |
| Car microphone never used in calls/meetings | `/info` audio formats are strings on some cars; telephony formats read as missing | Fixed (string and hex numbers accepted). Look for `/info audioFormats … telephony` in the run log. |
| Steering-wheel buttons do nothing | Key reports were parsed as touches and dropped | Fixed (`hidSendReport` key decoding). The run log shows each key. |

## 4G

| Symptom | Cause | Fix |
|---|---|---|
| The board becomes unreachable on the LAN when a 4G stick is plugged in | Armbian's catch-all network (`Name=e*`, route metric 100) also matched the stick's `enx…` interface: two default routes with the same priority | Router-mode sticks: `/etc/systemd/network/08-rascar-4g-dongle.network` (route metric 2048). The SIM7600 script uses metric 2048 as well. |
| Dashboard shows 4G "connected", but there is no data | The script dialed while udev was renaming the interface (`wwan0` → `wwu…`); the link stayed down without a route. The watchdog only checked for an address. | Fixed: it waits for udev to finish with the netdev, and the watchdog requires link up + address + default route. |
| 4G up, down, up…; kernel shows `error -71`, `Cannot enable. Maybe the USB cable is bad?`, `unable to enumerate USB device` | USB signal integrity: a USB extension cable on the modem; errors also hit the Bluetooth dongle on the same hub | Plug the modem directly into a USB 2.0 port. To recover a port the kernel gave up on: `echo 1 > /sys/bus/usb/devices/<hub>:1.0/<hub>-port<N>/disable; sleep 2; echo 0 > …/disable`. |
| Every `qmicli` call times out; `qmi-proxy` at 100 % CPU | The QMI proxy wedged | The script calls `qmicli` directly under a lock. Do not run other QMI clients at the same time. |
| `ps_attached` never becomes true; "waiting for network" forever | QMI channel wedged after a USB error | Re-plug or toggle the port as above. Check registration on the AT port (`AT+CEREG?`). |
| Router-mode stick (DWM-222W) enumerates but never gets service | It boots with its radio off (`AT+CFUN=4`) | `rascar-4g-radio` switches it on. Also turn off its own Wi-Fi access point and change its admin password. |
| Online services (weather, calendar) stay "offline" after 4G came up | Their retry backoff fired before the link was up | Wait for the next retry, or reload the dashboard. |

## Bluetooth and OBD-II

| Symptom | Cause | Fix |
|---|---|---|
| OBD never connects; scans find almost nothing; `l2ping` to the adapter says "Host is down" | The on-board (M.2 card) Bluetooth without antennas is ~40-45 dB weaker than a phone | Use a USB dongle (TP-Link UB500); the setup disables the on-board controller. Or fit the card's antennas. |
| Dongle plugged in, but the controller stays down with address `00:00:00:00:00:00` | `rtl_bt/rtl8761bu_fw.bin` missing (`firmware file … not found` in `dmesg`) | `setup/install.sh` extracts the two RTL8761BU files from Debian's `firmware-realtek`. |
| Tools keep using the weak controller | `bluetoothctl` uses the first registered controller as default | `70-rascar-bt-onboard-off.rules` de-authorises `0489:e123`. `bluetoothctl list` must show only the dongle. |
| Nothing pairs | BLE-only adapter, or a PIN other than 1234/0000 | Use a classic Bluetooth (SPP) ELM327. Put the PIN in `/etc/rascar/obd-pins` (`* <PIN>`) or choose it in Settings > Bluetooth. Status: `cat /run/rascar/obd-link.status`. |
| OBD drops every ~10 min while driving and takes 20-55 s to return | The Bluetooth dongle falls off USB (`disabled by hub (EMI?)`, `USB disconnect`); the adapter then keeps the dead link for its 20 s supervision timeout | `LinkSupervisionTimeout=3200` (2 s) in `/etc/bluetooth/main.conf` (set by the installer). Move the dongle to a USB 2.0 root port, away from the hub and the car's wiring. |
| `OBD connection lost: No such file or directory` forever | `/dev/rfcomm0` was never created: nothing paired and bound | `rascar-obd-link.service` must be enabled and the adapter powered (ignition on). |

## Habits that save time

- **Measure before theorising.** Most wrong turns in this project came from a plausible theory tested
  without a capture. The run log, `usbmon` and the NCM pcap almost always settle it.
- **Assume your own code first.** Hardware got blamed several times for bugs that were ours (a missing
  delay, a wrong timer, a wrong device claimed). Blame the hardware only once the software path has been
  traced on a capture.
- **One change per car test.** Car time is scarce. Two changes in one drive and you learn nothing.
- **Do not trust a single failed sample.** A boot cut short by the ignition proves nothing about "never".
  Measure the car's natural timing over several boots before adding a retry or a reset against it.
- **A clean `systemctl reboot` at your desk is not a car boot.** Car boots are hard power cycles on a
  weaker supply, with no network. Reproduce those conditions or read the logs of the real boot.
- **Never compile or install on the board while it runs on car power.**
