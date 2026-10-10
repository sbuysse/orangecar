# Configuration reference

This page lists every file OrangeCar reads or writes on the board, and every environment variable and
command-line option of its programs. `setup/install.sh` creates the files marked *installer*.
Nothing else needs to exist for a first run.

- [Files in /etc/rascar](#files-in-etcrascar)
- [The dashboard user's files (~/.config/rascar)](#the-dashboard-users-files-configrascar)
- [State and evidence (/var/lib)](#state-and-evidence-varlib)
- [System files installed](#system-files-installed)
- [rascar-carplay environment](#rascar-carplay-environment) (`/etc/rascar/carplay.env`)
- [rascar-carplay-run (the runner)](#rascar-carplay-run-the-runner)
- [rascar-app options and environment](#rascar-app-options-and-environment)
- [Helper scripts](#helper-scripts)
- [rascar-agent (/etc/rascar/agent.json)](#rascar-agent-etcrascaragentjson)

Changes take effect when the service restarts:
`sudo systemctl restart rascar-carplay` (or re-plug the car), `sudo systemctl restart rascar-app`.

---

## Files in /etc/rascar

| File | Mode | Written by | Read by | Contents |
|---|---|---|---|---|
| `install.env` | 0644 | installer | `rascar-obd-link`, `install-ota.sh`, installer re-runs | `RASCAR_USER`, `RASCAR_UID`, `RASCAR_HOME` of the dashboard account, plus `RASCAR_WITH_<PART>=1/0`, your answers to the installer's questions |
| `carplay.env` | 0644 | installer (from `setup/carplay.env.example`) | `rascar-carplay.service`, `rascar-carplay-run` | runtime environment of the transmitter, see [below](#rascar-carplay-environment) |
| `rascar-app.env` | 0644 | installer | `rascar-app.service` | one variable, `RASCAR_OPTS`, passed to `rascar-app`'s command line ([options](#rascar-app-options-and-environment)) |
| `hotspot.pass` | 0600 | installer (random, 14 characters) | `install-carplay-debug.sh` → hostapd | Wi-Fi passphrase of the in-car hotspot |
| `hotspot.conf` | 0644 | installer | `rascar-hotspot` | `WLAN=` (AP interface), `ETH=` (wired interface whose carrier means "no AP needed"), `COUNTRY=` (regulatory domain), `SSID=` (default `RasCar-debug`). Empty interface = auto-detect. After editing, re-run `setup/carplay-debug/install-carplay-debug.sh` to regenerate `/etc/hostapd/<WLAN>.conf` |
| `obd-pins` | 0600 | `rascar-obd-link` | bt-agent | PIN answered while pairing the OBD adapter (`* 1234` format); managed by the script |
| `4g.env` | 0644 | installer (4G option) | `rascar-4g-qmi.service` | `APN=` for the QMI modem (empty = the network's default) |
| `google-routes.key` | 0600 | you, optional | `rascar-app` | one-line Google Routes API key for traffic-aware routing. Without it, routing is offline Valhalla only |
| `agent.json` | 0644 | `install-ota.sh` | `rascar-agent` | agent settings, see [below](#rascar-agent-etcrascaragentjson) |
| `update-server.json` | **0600** | you, optional | `rascar-agent` | update server URLs and the car's account ([ota.md](ota.md)). Missing = the agent stays idle |
| `update-server.json.example` | 0644 | `install-ota.sh` | — | template for the above |
| `ota.pub` | 0644 | you (`install-ota.sh --pubkey`) | `rascar-agent` | hex ed25519 public key that releases must be signed with |
| `build-id` | 0644 | optional | `rascar-preflight`, `rascar-bundle` | free-text build label printed in the reports |

## The dashboard user's files (~/.config/rascar)

`~` is the home directory of the account that runs `rascar-app` (the `--user` of the installer).

| File | Written by | Contents |
|---|---|---|
| `tv-channels.json` | you (installer template) | `[{ "name": "...", "url": "https://.../stream.m3u8" }]`. The TV page plays only what is listed |
| `radio-stations.json` | you (installer template) | `[{ "name": "...", "genre": "...", "url": "..." }]`. Also an allowlist: if the file is missing, nothing plays |
| `contacts.json` | you (installer template) | `[{ "name": "...", "phone": "+..." }]`: quick-call tiles and caller names |
| `settings.json` | the dashboard (Settings page), plus a few hand-edited keys such as `nav_favorites` | every key is listed in [app/README.md](../app/README.md). `rascar-obd-link` reads `obd_device` / `obd_pin`; `rascar-agent` reads `cell_data` |
| `trips.db`, `datausage.db` | the dashboard | SQLite: trips and fuel, mobile data usage |
| `calendar-cache.json`, `last_media.json`, `ytmusic_signed_out` | the dashboard | caches and markers |
| `~/.cache/rascar/` | the dashboard | YouTube Music library cache, lyrics cache |

## State and evidence (/var/lib)

| Path | Written by | Contents |
|---|---|---|
| `/var/lib/rascar/usb_serial` | rascar-carplay | the random 40-hex USB serial the board presents. The car remembers a "phone" by it, so keep it |
| `/var/lib/rascar/hap_identity` | rascar-carplay | persistent pairing identity used in the CarPlay session's pair-verify |
| `/var/lib/rascar/headunit.json` | rascar-carplay | what the car reported about itself over iAP2 (name, model, firmware, ...). Shown by the dashboard |
| `/var/lib/rascar/chromium-profile/` | Chromium (started by rascar-carplay) | the car-screen browser's profile: sign-ins, cookies |
| `/var/lib/rascar/maps/` | you ([install.md](install.md#offline-maps-and-routing)) | `region.pmtiles`, `fonts/`, `sprites/`, `geocoder.sqlite` |
| `/var/lib/rascar/routing/` | you | Valhalla graph (`tiles.tar`, `valhalla.pi.json`, ...) |
| `/var/lib/rascar/voice/` | you, optional | Piper voice model for spoken navigation |
| `/var/lib/rascar/obd.mac` | rascar-obd-link | the OBD adapter found by the last scan |
| `/var/lib/rascar-debug/` | rascar-debugcap, rascar-carplay-run, rascar-earlylog, rascar-thermal, rascar-app | per-boot evidence. See [install.md](install.md#10-logs-and-debugging) |
| `/var/lib/rascar-debug/car-gps.nmea` | rascar-carplay | the car's GPS sentences (read by the dashboard) |
| `/var/lib/rascar-agent/` | rascar-agent | OTA state, journal spool, downloads |
| `/usr/local/lib/rascar/modules/<kernel>/cdc_ncm.ko` | `kernel/cdc_ncm/build.sh` | the patched module |
| `/run/rascar/4g.status`, `/run/rascar/obd-link.status` | 4G and OBD helpers | one status line each, shown by the dashboard |

## System files installed

| Path | From | Purpose |
|---|---|---|
| `/opt/orangecar` → `/opt/orangecar-releases/<release>` | installer / OTA | binaries (`target/release/`), `app/`, `setup/` |
| `/etc/systemd/system/rascar-*.service` | `setup/**` | `rascar-app` (generated with your user), `rascar-carplay`, `rascar-debugcap`, `rascar-hotspot`, `rascar-thermal`, `rascar-earlylog`, `rascar-nvme-power`, `rascar-rtc-off`, `rascar-obd-link`, `rascar-4g-qmi`, `rascar-4g-radio`, `rascar-valhalla`, `rascar-agent`, `rascar-ota-boot` |
| `/usr/local/sbin/rascar-*` | `setup/**` | the scripts behind those units, plus `rascar-preflight` and `rascar-bundle` |
| `/etc/systemd/system/rascar-carplay.service.d/20-chromium-clean-stop.conf` | `setup/carplay-debug/` | closes the kiosk browser cleanly before a stop (keeps fresh sign-ins) |
| `/etc/systemd/system/systemd-udevd.service.d/50-rascar-cpuquota.conf` | `setup/boot/` | `CPUQuota=10%` on udev (spreads the boot-time driver probes) |
| `/etc/systemd/journald.conf.d/20-rascar-powercut.conf` | `setup/boot/rascar-journald.conf` | journal on disk, synced every 5 s (power-cut hardening) |
| `/etc/systemd/network/01-rascar-carplay-ncm.{link,network}` | `setup/carplay-debug/` | keep the car's NCM link named `usb0` and unmanaged by networkd |
| `/etc/systemd/network/08-rascar-4g-dongle.network` | `setup/4g/` | DHCP at metric 2048 for RNDIS/CDC-Ether 4G sticks |
| `/etc/modules-load.d/rascar.conf`, `rascar-early-drivers.conf`, `rascar-debug.conf` | `setup/boot/`, `setup/wayland/` | `libcomposite`; GPU (`panthor`) and sound drivers early; `usbmon` |
| `/etc/modprobe.d/rascar-no-hym8563.conf` | `setup/rtc/` | blacklist of the battery-less RTC (stuck interrupt) |
| `/etc/sysctl.d/90-rascar-powercut.conf` | `setup/boot/` | `vm.dirty_expire_centisecs=300`, `vm.dirty_writeback_centisecs=100` |
| `/etc/udev/rules.d/90-rascar-libinput-ignore.rules` | `setup/wayland/` | hide the IR receiver from the kiosk compositor |
| `/etc/udev/rules.d/70-rascar-bt-onboard-off.rules` | `setup/bluetooth/` | switch off the on-board Bluetooth (USB dongle option) |
| `/etc/udev/rules.d/90-rascar-4g.rules` | `setup/4g/` | start the 4G helpers when a modem appears |
| `/etc/chromium/policies/managed/rascar-embedded-apps.json` | `setup/chromium/` | cookies for apps embedded in the dashboard, geolocation allowed, browser sign-in off ([README](../setup/chromium/README.md)) |
| `/etc/sudoers.d/rascar-app` | installer | the root commands `rascar-app` may run with `sudo -n` |
| `/etc/hostapd/<wlan>.conf` | installer | hotspot access point |
| `/boot/armbianEnv.txt` `extraargs` | installer | `cma=256M nvme_core.default_ps_max_latency_us=0 pcie_aspm=off pcie_port_pm=off fsck.repair=yes` |
| `/etc/fstab` root line | installer (power-cut hardening) | `commit=5` |

## rascar-carplay environment

Set in `/etc/rascar/carplay.env`. The shipped example ([setup/carplay.env.example](../setup/carplay.env.example))
contains only the settings a new install needs. Everything else here has a working default.

Flags marked *bool* accept `1/on/true/yes` and `0/off/false/no` (any case). Some flags only accept the
exact value given in their row.

### Settings you may want to change

| Variable | Default | Effect |
|---|---|---|
| `RASCAR_COMPOSITOR` | Xvfb | `wayland`: render the dashboard in `cage` on the GPU (recommended). Otherwise Xvfb with CPU rendering |
| `RASCAR_VIDEO_FPS` | 30 | frame rate of the screen stream. 25 saves ~20% CPU |
| `RASCAR_VIDEO_BITRATE_KBPS` | 3000 | H.264 bitrate |
| `RASCAR_WIRE_TRACE` | off | `1`: hex-dump USB bulk, iAP2 and RTSP traffic into the run logs (recommended while you bring up a new car) |
| `RASCAR_IAP2_HEARTBEAT` | off | bool: re-send Now Playing every 1.2 s on an idle iAP2 link. The car's acknowledgements give the logs a "still talking" timestamp |
| `RASCAR_STAY_DEVICE_REENUM` | off | `1`/`on`: soft-reconnect the gadget when the car sends `40 51 wValue=0`. Tested: this cuts a live session. Keep `0` |
| `RASCAR_CTRL_HOSTLABEL` | `iPhone` | host label in the `_carplay-ctrl` mDNS record. Set it to the board's hostname, the name avahi publishes on the car link |
| `RASCAR_NOWPLAYING` | on | bool: send the dashboard's real Now Playing metadata to the car |
| `RASCAR_NOWPLAYING_ADDR` | `127.0.0.1:8080` | where rascar-app is polled for Now Playing |
| `RASCAR_AUDIO_SOURCE` | monitor of the default output | PulseAudio source captured as the car's audio |
| `RASCAR_CARPLAY_MIC` | on | bool, kill switch: off = never switch the car to telephony (car microphone) |
| `RASCAR_PHONE_RING` | on | bool, kill switch: off = ignore ring requests from the dashboard |
| `RASCAR_SCREEN_HANDBACK` | on | bool, kill switch: off = ignore "give the screen back to the car" from the dashboard |
| `RASCAR_USB_SERIAL` | the persisted random serial | 40 hex digits: present this serial (also disables rotation) |
| `RASCAR_CAR_GPS_PATH` | `/var/lib/rascar-debug/car-gps.nmea` | file the car's NMEA sentences are appended to |
| `NO_COLOR` | unset | `1`: no ANSI colours in the logs |

### Tuning and experiments

These are kept for bringing up other head units and for A/B tests. The defaults are the configuration
that works on the tested car. Change one at a time and compare run logs.

| Variable | Default | Effect |
|---|---|---|
| `RASCAR_ROLE_SWITCH_DELAY_MS` | 10 | delay (≤ 2000) between acknowledging the car's role-switch request and switching the controller to host |
| `RASCAR_ROLE_SWITCH_GRACE_MS` | 45000 | how long to wait for the role-switch request before presenting the gadget again (≥ 500, backs off) |
| `RASCAR_REENUM_TIMEOUT_MS` | 1500 | wait for the car to re-enumerate after authentication (≤ 30000) |
| `RASCAR_NCM_HOLD` | off | `1`: use the patched cdc_ncm's `hold_apple_gadget` until iAP2 auth ([kernel/README.md](../kernel/README.md)). The runner clears it |
| `RASCAR_EARLY_NCM` | off | `1`: bring CDC-NCM up during the iAP2 handshake. Cleared by the runner (raced udev's interface rename) |
| `RASCAR_ROTATE_SERIAL` | off | `1`: new random USB serial before each attempt. Cleared by the runner |
| `RASCAR_DEFER_NCM_BIND` | off | `1`: suppress the cdc_ncm bind with `drivers_autoprobe` (known broken) |
| `RASCAR_POST_AUTH_RESET` | off | `1`: USB reset of the car device right after iAP2 auth |
| `RASCAR_AIRPLAY_FALLBACK` | on | bool: if `_airplay._tcp` discovery stays empty, dial the car at addresses derived from the NCM link |
| `RASCAR_WAIT_FOR_INVITE_MS` | 0 | wait up to N ms for the car's `/ctrl-int/1/connect` invite before dialling |
| `RASCAR_CTRL_VIA_AVAHI` | off | bool: publish `_carplay-ctrl` through `avahi-publish-service` instead of the built-in responder |
| `RASCAR_CTRL_INSTANCE` | `iPhone` | mDNS instance name of `_carplay-ctrl` |
| `RASCAR_CTRL_SRCVERS` | `566.25.21` | `srcvers` TXT value |
| `RASCAR_CTRL_DEVICE_ID` | built-in MAC | `id` TXT value (avahi path only) |
| `RASCAR_IAP2_GATE` | on | bool: never send a control message the car did not list as accepted |
| `RASCAR_IAP2_GPGSV` | on | bool: ask for satellite (GPGSV) sentences if the car offers them |
| `RASCAR_IAP2_KEEP_LOCATION` | on | bool: keep the car's location stream open after auth |
| `RASCAR_IAP2_WIRELESS_CARPLAY` | not sent | `available`/`on`/`1` or `unavailable`/`0`: send the WirelessCarPlay update |
| `RASCAR_IAP2_HID` | off | `1`: run the device-mode iAP2-over-HID responder |
| `RASCAR_HIDE_CARPLAY` | off | `1`: answer the capability request without CarPlay (iAP2-over-HID only) |
| `RASCAR_UAC1` | on | `off`: FunctionFS-only audio descriptors instead of the kernel's f_uac1 |
| `RASCAR_CARPLAY_AUDIO` | follows `RASCAR_MODES_CLAIM_AUDIO` | `on`/`off`: claim MainAudio and open the audio stream |
| `RASCAR_MODES_CLAIM_AUDIO` | on | bool: same claim, used when `RASCAR_CARPLAY_AUDIO` is unset |
| `RASCAR_INITIAL_SCREEN_SYNC` | on | bool: state the car's own screen ownership before taking the screen |
| `RASCAR_KEEPALIVE_SECS` | 1 | AirPlay keepalive interval (1-30) |
| `RASCAR_PHONE_RING_CALLSTATE` | off | bool: announce a phone call while ringing |
| `RASCAR_AUTH_SETUP` | off | `1`: run `/auth-setup` during session setup |
| `RASCAR_ET` | 1 | `et` (encryption type) value in the SETUP request |
| `RASCAR_VIDEO_KEYINT` | 15 | frames between IDR frames |
| `RASCAR_AVCC_TRAILER` | on | bool: append the 4-byte trailer to the avcC record |
| `RASCAR_SCREEN_HUD` | off | bool: ask the receiver to draw its diagnostic overlay |
| `RASCAR_RESPECT_TIMESTAMPS` | off | bool: let the receiver schedule frames by timestamp |
| `RASCAR_TOUCH_PARSER` | off | `experimental`: inject touches from the reverse-channel parser |
| `RASCAR_NOWPLAYING_ARTWORK` | off | bool: send album art over iAP2 file transfer |
| `RASCAR_NOWPLAYING_APP` / `RASCAR_NOWPLAYING_BUNDLE` | `OrangeCar` / `org.orangecar.dashboard` | app name and bundle id in Now Playing |
| `RASCAR_SKIP_PHASE4` | off | bool: stop after iAP2 (never start AirPlay) |
| `RASCAR_ENUM_THEN_IDLE_MS` | unset | enumerate, stay silent N ms, then end the attempt (control experiment) |
| `RASCAR_PERSONA` | iPhone | `android`: run the Android AOA / mySPIN probe instead of the Apple gadget |
| `RASCAR_AOA_HANDSHAKE`, `RASCAR_AOA_PING`, `RASCAR_AOA_SERIAL` | on, on, built-in | AOA probe details |
| `RASCAR_DISPLAY` | `:0` | X display of the Xvfb path |
| `RASCAR_RENDER_NODE` | `/dev/dri/renderD128` | DRM render node for cage (missing = software renderer) |
| `RASCAR_USB_SERIAL_STORE`, `RASCAR_HEADUNIT_PATH`, `RASCAR_HAP_STORE` | `/var/lib/rascar/...` | paths of the state files above |

`RASCAR_BUILD_ID` is read when the binary is **compiled** (the release script sets it). Setting it in
`carplay.env` only changes what the runner prints.

The transmitter's own flags: `--debug` (the runner always passes it), plus bench tools:
`--airplay-endpoint`, `--carplay-endpoint HOST:PORT`, `--mdns-selftest LINKLOCAL%IFACE`, `--audio-probe`.

## rascar-carplay-run (the runner)

`/usr/local/sbin/rascar-carplay-run` wraps each attempt. It reads `carplay.env` too:

| Variable | Default | Effect |
|---|---|---|
| `RASCAR_CDC_NCM_KO` | `/usr/local/lib/rascar/modules/$(uname -r)/cdc_ncm.ko` | patched module to load before the attempt |
| `RASCAR_CAPTURE_SECONDS` | 60 | network + USB capture per attempt: seconds, `0` = none, `full` = whole session |
| `RASCAR_CPU_MAX_KHZ` | 1800000 | cap of the A76 cores during the attempt (`0` = leave alone). `rascar-thermal` lowers it further when hot |
| `RASCAR_FUSB302_I2C` | `6-0022` | I2C device of the fusb302 Type-C controller (Orange Pi 5 Plus), re-bound if a crashed run left it unbound |
| `RASCAR_POST_AUTH_RESET_AB` | 0 | `1`: odd attempts use `RASCAR_POST_AUTH_RESET=1`, even ones the baseline |

Board-specific values used by the scripts: USB device controller `fc000000.usb`, its debugfs mode file
`/sys/kernel/debug/usb/fc000000.usb/mode`, fusb302 at I2C `6-0022`, battery-less RTC at `6-0051`. Another
RK3588 board may use other names.

## rascar-app options and environment

Options go in `RASCAR_OPTS` in `/etc/rascar/rascar-app.env`:

| Option | Default | Meaning |
|---|---|---|
| `--port` | 8080 | HTTP port |
| `--bind` | 0.0.0.0 | listen address. **No authentication**: bind to `127.0.0.1` if the board shares a network you do not trust (the car screen uses localhost) |
| `--lat`, `--lon` | none | optional weather location used until a GPS fix is available (the weather follows the car's GPS) |
| `--ics-uris` | empty | comma-separated iCalendar feed URLs |
| `--obd-port` | `/dev/rfcomm0` | ELM327 serial port |
| `--maps-dir` | `/var/lib/rascar/maps` | offline map data |
| `--trip-retention-days` | 365 | trips older than this are pruned at start (0 = keep all) |

Environment (add `Environment=` lines with `sudo systemctl edit rascar-app`):

| Variable | Default | Effect |
|---|---|---|
| `RASCAR_PARK_POWEROFF` | on | `0`: do not power the board off when the car is in Park with the engine off |
| `RASCAR_PHONE` | off | bool: start the modem phone service at boot (emergency numbers start it on demand anyway) |
| `RASCAR_MIXER_CONTROL` | `Master` | ALSA control used for the volume |
| `RASCAR_VIZ_SOURCE` | monitor of the default sink | PulseAudio source of the audio visualizer |
| `RASCAR_OBD_RAWLOG_DIR` | `/var/lib/rascar-debug/obd` | raw OBD log (16 MiB × 12). Empty or `off` disables it |
| `RASCAR_OBD_TPMS` | off | bool: poll tyre pressures over OBD (vehicle-specific PIDs) |
| `RASCAR_COINGECKO_KEY` | `coingecko_api_key` in settings.json | API key of the crypto ticker |
| `RASCAR_FRONTEND_DIR` | `/opt/orangecar/app/frontend` (found automatically) | static files served |
| `RASCAR_YTMUSIC_HELPER` | `/opt/orangecar/app/scripts/ytmusic_helper.py` | YouTube Music sidecar |
| `RASCAR_CAR_GPS_PATH`, `RASCAR_HEADUNIT_PATH` | as for rascar-carplay | files written by the transmitter |

## Helper scripts

| Script | Variables | Notes |
|---|---|---|
| `rascar-hotspot` | `/etc/rascar/hotspot.conf` | `auto` (the unit: AP only without Ethernet carrier after 45 s), `on`, `off`, `status` |
| `rascar-obd-link` | `OBD_MAC` (pin one adapter), `OBD_CHANNEL` (RFCOMM channel, 1), `RASCAR_SETTINGS` (path of settings.json; default from `RASCAR_HOME` in install.env) | set with `sudo systemctl edit rascar-obd-link` |
| `rascar-4g-qmi` | `APN` (from `/etc/rascar/4g.env`), `WDM` (`/dev/cdc-wdm0`) | default route at metric 2048; USB reset of a wedged stick at most every 120 s |
| `rascar-thermal` | `RASCAR_THERMAL_LOG` (`/var/lib/rascar-debug/thermal.log`), `NVME_OFFSET` (5) | levels at 72/78/83/88 °C, 5 °C hysteresis |
| `rascar-earlylog` | `RASCAR_EARLYLOG_SECS` (90), `RASCAR_EARLYLOG_DIR` (`/var/lib/rascar-debug/earlyboot`) | keeps the last 40 boots |
| `rascar-ota-boot` | `RASCAR_AGENT_STATE`, `RASCAR_INSTALL_LINK`, `RASCAR_MAX_TRIAL_BOOTS` (3), `RASCAR_AGENT_BIN` | the OTA boot guard ([ota.md](ota.md)) |
| `install-maps.sh` | `BBOX` (required), `MAPS_DIR`, `PMTILES_VERSION` | |
| `build-routing.sh` | `VALHALLA_IMAGE` | |
| `kernel/cdc_ncm/build.sh` | `KDIR`; options `--src`, `--no-autoload` | |

## rascar-agent (/etc/rascar/agent.json)

Every key is optional. `{}` means all defaults. Keys are camelCase.

| Key | Default | Meaning |
|---|---|---|
| `serverJson` | `/etc/rascar/update-server.json` | update server settings. Missing file = agent idle |
| `dashboardSettings` | unset (installer sets `~user/.config/rascar/settings.json`) | where the dashboard's "OTA over 4G" / "logs over 4G" switches are read. Unset = both allowed |
| `stateDir` | `/var/lib/rascar-agent` | state, journal spool, downloads |
| `statusIntervalSecs` | 900 | status report period |
| `meteredDrivers` | `qmi_wwan, cdc_mbim, option, cdc_ether, rndis_host, ipheth` | a default route through one of these counts as metered (4G) |
| `journal.enabled`, `.units`, `.kernel`, `.anyUnitMaxPriority`, `.intervalSecs`, `.maxEntriesPerCycle` | on, `rascar-*` and a few system units, on, 3, 10, 20000 | what is copied from the journal into the spool |
| `files[]` (`root`, `glob`, `maxBytesPerFile`, `maxAgeDays`, `metered`) | earlyboot, OBD and CarPlay run logs under `/var/lib/rascar-debug` | extra files uploaded |
| `upload.intervalSecs`, `.meteredIntervalSecs`, `.meteredDailyBudgetBytes`, `.batchBytes`, `.spoolMaxBytes` | 60, 900, 30 MiB, 512 KiB, 512 MiB | upload pacing |
| `ota.enabled` | true | check for releases |
| `ota.checkIntervalSecs` | 3600 | |
| `ota.downloadOnMetered` | true | download releases over 4G too |
| `ota.pubkeyFile` | `/etc/rascar/ota.pub` | |
| `ota.installLink`, `ota.releasesDir` | `/opt/orangecar`, `/opt/orangecar-releases` | |
| `ota.keepReleases`, `ota.maxTrialBoots`, `ota.healthDelaySecs` | 3, 3, 30 | |
| `ota.target` | `orangecar` | device family a manifest must name |
