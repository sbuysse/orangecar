# Installation: from a blank NVMe to CarPlay in the car

This guide starts with the hardware described in [hardware.md](hardware.md): an Orange Pi 5 Plus, an
NVMe SSD, a solid 5 V supply and a USB-C cable to the car. It ends with the dashboard on the car's
screen. Plan on an evening for the bench part and a short drive for the first car test.

Contents:

1. [Flash Armbian](#1-flash-armbian)
2. [First boot](#2-first-boot)
3. [Build the binaries](#3-build-the-binaries) (on a PC, recommended)
4. [Copy the tree to the board](#4-copy-the-tree-to-the-board)
5. [Run the installer](#5-run-the-installer)
6. [The patched cdc_ncm module](#6-the-patched-cdc_ncm-module)
7. [Reboot and check on the bench](#7-reboot-and-check-on-the-bench)
8. [Connect to the car](#8-connect-to-the-car)
9. [Optional parts](#9-optional-parts): hotspot, OBD-II, 4G, offline maps, OTA
10. [Logs and debugging](#10-logs-and-debugging)
11. [Updating](#11-updating)

> **Golden rule: never compile, run `apt upgrade` or do anything heavy on the board while it is
> powered by the car.** The car's supply sags under sustained load and the NVMe drops off PCIe, which
> leaves the root filesystem read-only (see [hardware.md](hardware.md#storage-nvme)). Do every step in
> this guide on a bench supply.

---


> **How this guide was tested.** On the reference Orange Pi 5 Plus (Armbian 25.11 trixie, kernel
> 6.18.20), `kernel/cdc_ncm/build.sh` fetched the stock source, patched and built a module whose
> `srcversion` is identical to the one running in the reference car. `setup/install.sh --yes` with every
> optional part was run end to end on a blank Debian trixie arm64 system: it completed, a second run
> changed nothing, and the installed dashboard served its pages and API. On a blank system with a working
> systemd, after a reboot, every unit came up by itself (`rascar-app` as the dashboard user with
> lingering, the agent idle, the guards running), and only `rascar-carplay` waited for USB hardware.
> The released `rascar-carplay` passes the five scenarios of [tools/bench](../tools/bench/README.md)
> exactly like the build that runs in the reference car. This installer has not yet been run on a freshly flashed board
> followed by a drive (the reference car was set up step by step before it existed). Please report how
> it goes for you.

## 1. Flash Armbian

The reference build uses **Armbian 25.11 "trixie" (Debian 13), `current` kernel 6.18.x** (the
`6.18.20-current-rockchip64` image) for the **Orange Pi 5 Plus**, the minimal/CLI flavour. A desktop
image also works, but the extra services cost CPU you will want later. The `vendor` (6.1) and `legacy`
(5.10) kernels are **not** supported: the kiosk needs the mainline `panthor` GPU driver, and the
kernel patch targets 6.18.

The root filesystem must end up on the NVMe. The board boots from its SPI flash, which then loads the
system from the NVMe:

1. Write the Armbian image to a microSD card (balenaEtcher, or
   `xzcat Armbian_*.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync status=progress`).
2. Fit the NVMe, boot from the microSD and complete the first-boot questions (next section).
3. Run `sudo armbian-install` and choose **"Boot from SPI — system on NVMe"** (the exact label
   depends on the Armbian release). It writes the bootloader to the SPI flash and copies the system
   to the NVMe.
4. Power off, remove the microSD and power on again. `findmnt /` should show `/dev/nvme0n1p1`.

## 2. First boot

On the first boot (on HDMI or over SSH as `root` / `1234` on the Ethernet port), Armbian asks for a
root password and creates a normal user. **That user runs the dashboard.** The installer asks for it and
defaults to the account that runs `sudo`. Any name works.

Then:

```sh
sudo apt update && sudo apt full-upgrade     # on the bench, not in the car
sudo armbian-config                          # optional: hostname, timezone, locale, Wi-Fi
uname -r                                     # e.g. 6.18.20-current-rockchip64
```

If `full-upgrade` installs a new kernel, reboot before going further: the cdc_ncm module is built for the
*running* kernel.

## 3. Build the binaries

The car runs four Rust programs from this workspace: `rascar-app` (the dashboard), `rascar-carplay` (the
CarPlay transmitter), `rascar-agent` (optional updates and log upload) and `rascar-release` (a PC-side
tool). **Cross-compiling on a PC is recommended.** It takes a couple of minutes there, against 10-30
minutes of full CPU load on the board.

### Cross-compile on a PC (recommended)

Any Linux or macOS machine works. Use [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild),
which links against an exact glibc version (2.36 here, so the binaries run on Debian 12 and 13):

```sh
# once
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add aarch64-unknown-linux-gnu
pip install ziglang           # or install zig from your distribution / ziglang.org
cargo install --locked cargo-zigbuild

# each build, from the repository root
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36 \
    -p rascar-app -p rascar-carplay -p rascar-agent
ls target/aarch64-unknown-linux-gnu/release/rascar-{app,carplay,agent}
```

No system libraries are needed for the target. SQLite is bundled, TLS is rustls, and USB goes through
`nusb` without libusb.

### Native build on the board (alternative)

`sudo setup/install.sh --native-build` installs rustup for the dashboard user and runs
`cargo build --release -p rascar-app -p rascar-carplay -p rascar-agent` when it finds no binaries.
Only do this on a bench supply, with a heatsink or a fan.

## 4. Copy the tree to the board

The installer runs from a checkout on the board and copies what it needs into `/opt/orangecar`. Copy the
repository without the PC's `target/` directory, then add the three binaries:

```sh
BOARD=<user>@<board-ip>
rsync -a --exclude target/ --exclude .git/ ./ $BOARD:orangecar/
ssh $BOARD mkdir -p orangecar/target/aarch64-unknown-linux-gnu/release
rsync -a target/aarch64-unknown-linux-gnu/release/rascar-{app,carplay,agent} \
    $BOARD:orangecar/target/aarch64-unknown-linux-gnu/release/
```

You can also `git clone` on the board, then copy only the binaries.

## 5. Run the installer

```sh
cd ~/orangecar
sudo setup/install.sh
```

It asks all of its questions first, then runs without stopping. Re-running it later (for example after
`git pull`) is safe: existing configuration files are kept.

| Question | Default | What it installs |
|---|---|---|
| In-car Wi-Fi hotspot | no | `rascar-hotspot`: an access point with a **passphrase generated at install time**, started only when no Ethernet cable is plugged in at boot (recommended while you debug a new car) |
| Power-cut hardening | yes | journal on the NVMe instead of RAM (`armbian-ramlog` off), synced every 5 s; root ext4 `commit=5` (details below) |
| Passwordless sudo for the dashboard | yes | `/etc/sudoers.d/rascar-app`, limited to the commands the dashboard runs (poweroff in Park, `journalctl --sync`, `wpa_cli`, restarting the 4G link) |
| Build the patched cdc_ncm now | yes | runs `kernel/cdc_ncm/build.sh` (section 6) |
| OBD-II over Bluetooth | no | `rascar-obd-link` (finds, pairs and binds an ELM327 to `/dev/rfcomm0`), plus a follow-up question about a USB Bluetooth dongle |
| 4G modem | no | QMI dialer for SIMCom SIM7600, radio switch for D-Link DWM-222W, a network profile for RNDIS/CDC-Ether sticks |
| Offline maps | no | `podman` and the Valhalla routing unit. The map data is downloaded separately (section 9) |
| OTA agent | no | `rascar-agent` and its boot guard. It does nothing until you set up your own update server ([ota.md](ota.md)) |
| RTC coin cell fitted | no | Without a battery, the HYM8563 RTC driver is blacklisted: its interrupt line is stuck and costs ~1.5 load average |

Your answers are saved in `/etc/rascar/install.env` and become the defaults of the next run.
Non-interactive use: `sudo setup/install.sh --yes --user alice --with-obd --without-hotspot`.
The options are `--with-X` / `--without-X` for `hotspot`, `sudoers`, `cdc-ncm`, `obd`, `bt-dongle`, `4g`,
`maps`, `ota`, `rtc-battery`, `powercut`.

What it does, in order:

1. **Packages**: avahi, ffmpeg, Chromium, cage, wlr-randr, Xvfb, PulseAudio, mpv, playerctl, tcpdump,
   nvme-cli, the build tools for the kernel module, and the kernel headers for the running kernel.
2. **Install tree**: binaries, `app/frontend`, `app/scripts`, `app/chromium-ext` and `setup/` go to
   `/opt/orangecar-releases/manual`. `/opt/orangecar` becomes a symlink to it. Every unit uses
   `/opt/orangecar`, and the OTA agent switches releases by moving that symlink.
3. **Configuration**: `/etc/rascar/install.env` (which user runs the dashboard),
   `/etc/rascar/rascar-app.env`, `/etc/rascar/carplay.env` (from `setup/carplay.env.example`), and
   templates in `~/.config/rascar/` (TV channels, radio stations, contacts).
4. **Boot settings** in `/boot/armbianEnv.txt`. It adds
   `extraargs=cma=256M nvme_core.default_ps_max_latency_us=0 pcie_aspm=off pcie_port_pm=off fsck.repair=yes`,
   keeping any arguments already there (backup in `armbianEnv.txt.bak-rascar`). **No device-tree
   overlay is needed** on the 6.18 kernel: the USB-C controller `usb@fc000000` already has
   `dr_mode = "otg"`. The installer checks this and warns if the overlays in `armbianEnv.txt` have no
   `.dtbo` for the running kernel. That is the case for the old `rk3588-dwc3-peripheral` and
   `rk3588-usbc0-peripheral` names: they are ignored at boot and can be removed.
5. **Modules, sysctl, udev**: `libcomposite`, GPU and sound drivers loaded early, short writeback timers
   for power cuts, a `CPUQuota=10%` cap on `systemd-udevd` at boot (the probe surge otherwise sagged the
   car supply), the IR receiver hidden from the Wayland kiosk, and Chromium policies.
6. **Power-cut hardening** (if chosen). The car cuts power without warning at every
   ignition-off. Armbian keeps `/var/log` and the journal in a zram disk (`armbian-ramlog`), so a
   drive's log was lost and Armbian's `commit=600` kept up to ten minutes of writes in RAM. The
   installer disables `armbian-ramlog` (`ENABLED=false` in `/etc/default/armbian-ramlog`),
   installs `/etc/systemd/journald.conf.d/20-rascar-powercut.conf` (`Storage=persistent`,
   `SystemMaxUse=200M`, `SyncIntervalSec=5s`), sets `commit=5` on the root ext4 line of `/etc/fstab`
   (backup in `fstab.bak-rascar`) and, in every case, short writeback timers
   (`vm.dirty_expire_centisecs=300`, `vm.dirty_writeback_centisecs=100`, together with `fsck.repair=yes`
   on the kernel command line). The last ~3-5 s of writes can still be lost.
7. **Guards**: `rascar-thermal` (steps the CPU clock down as the board heats, since the board has no active cooling by default), `rascar-earlylog` (fsyncs the first 90 s of every boot to disk), and
   `rascar-nvme-power` (NVMe power state 2, enabled only if your drive reports that state as operational).
8. **Services**: `rascar-app` (as your user, with lingering so its PulseAudio and session bus exist
   without a login), `rascar-carplay`, `rascar-debugcap` and the optional ones. They are enabled for the
   next boot. Nothing is started.

## 6. The patched cdc_ncm module

The stock `cdc_ncm` driver cannot bring up the car's network function. [kernel/README.md](../kernel/README.md)
explains why. If you skipped it in the installer, build it now:

```sh
sudo kernel/cdc_ncm/build.sh
cat /sys/module/cdc_ncm/version 2>/dev/null   # "rascar-3" once loaded
```

To build it without installing anything (for example to inspect it first):
`sudo kernel/cdc_ncm/build.sh --dest /tmp/cdc_ncm-test`, then `modinfo /tmp/cdc_ncm-test/cdc_ncm.ko`.

It needs the headers for the running kernel (the installer installs them). On Armbian the package is
`linux-headers-current-rockchip64`. The module goes to
`/usr/local/lib/rascar/modules/$(uname -r)/cdc_ncm.ko`, and `rascar-carplay-run` loads it before every
attempt. **Re-run the script after every kernel upgrade.**

## 7. Reboot and check on the bench

```sh
sudo reboot
# after the reboot
sudo rascar-preflight
```

`rascar-preflight` ends with `SUMMARY: PASS=.. WARN=.. FAIL=..`. On the bench, with no car, expect:

- `PASS UDC fc000000.usb present` and `PASS dr_mode=otg`,
- `PASS dashboard http://127.0.0.1:8080 answers 200`,
- `PASS cdc_ncm module available`, `PASS avahi-daemon active`,
- a `WARN` about the clock not being NTP-synchronised is normal in the car.

Look at the dashboard from a PC on the same network: `http://<board-ip>:8080`.

`rascar-carplay` is already running and waiting for a car. Its log shows the gadget presented and
nothing connecting:

```sh
journalctl -fu rascar-carplay
```

Before the first drive, fill in your own data:

- `~/.config/rascar/tv-channels.json`, `radio-stations.json`, `contacts.json`: each template has one fake
  entry showing the format. An empty list `[]` is valid. The radio and TV lists double as allowlists,
  so the dashboard plays nothing that is not listed.
- `/etc/rascar/rascar-app.env`: optionally `--ics-uris` for calendars, and `--lat`/`--lon` as a weather
  location until the car's GPS has a fix.
- Then `sudo systemctl restart rascar-app`.

## 8. Connect to the car

1. Power the board from its own 5 V supply. See [hardware.md](hardware.md#power): the car's USB port
   is not enough.
2. Connect the board's **USB-C OTG/data port** (the USB-C port that is *not* the power input,
   `usb@fc000000`; see [hardware.md](hardware.md)) to the car's **CarPlay USB port** (the one an
   iPhone uses for CarPlay). Use a data-capable cable.
3. Start the car. The board presents itself as an iPhone. The head unit asks it to switch roles,
   authenticates it over iAP2, and opens the CarPlay session. The dashboard appears on the car's screen
   after 10-20 s. Touch, the rotary controller and the steering-wheel keys work through CarPlay.

If the dashboard does not appear, `rascar-carplay` starts another attempt about 20 s after each failed
one. Every attempt is logged. Go to [section 10](#10-logs-and-debugging), and see
[troubleshooting.md](troubleshooting.md).

## 9. Optional parts

### In-car hotspot

Off by default. Enable it with `sudo setup/install.sh --with-hotspot` (or answer yes to the question).

In the car there is no Ethernet cable, so at boot `rascar-hotspot` waits 45 s for a carrier on the wired
interface. If none appears, it starts an access point:

- SSID `RasCar-debug` (`/etc/rascar/hotspot.conf`), WPA2, passphrase in `/etc/rascar/hotspot.pass`,
  generated at install time (`sudo cat /etc/rascar/hotspot.pass`).
- The board is `10.42.0.1`. Clients get an address from `10.42.0.10` upwards, with no route or DNS, so
  your phone keeps its own mobile data.
- `ssh <user>@10.42.0.1`.

Interfaces on the Orange Pi 5 Plus: `wlP2p33s0` (Wi-Fi, M.2 RTL8852BE card) and
`enP3p49s0` / `enP4p65s0` (the two 2.5 GbE ports). They are auto-detected. Pin them and set your
regulatory country in `/etc/rascar/hotspot.conf`, then re-run
`sudo setup/carplay-debug/install-carplay-debug.sh` to regenerate the hostapd file.

If the hotspot does not come up:

```sh
sudo rascar-hotspot status
journalctl -b -u rascar-hotspot -u 'hostapd@*'
iw dev                                   # is the interface name the one in hotspot.conf?
sudo rascar-hotspot on                   # force it, even with Ethernet plugged in
```

Common causes: the Wi-Fi interface has a different name (set `WLAN=`), `wpa_supplicant` still owns the
radio (the script stops `wpa_supplicant@<wlan>`, but NetworkManager setups may need
`nmcli radio wifi off` or an unmanaged-device rule), or the country code is not allowed on channel 6.

### OBD-II (Bluetooth ELM327)

`rascar-obd-link` runs as root. It finds a Classic Bluetooth adapter whose name looks like OBD
(`OBD|ELM|V-LINK|VGATE|...`), pairs it (PIN from the dashboard, then 1234, then 0000), and binds it to
`/dev/rfcomm0`, which `rascar-app` reads. Pick a specific adapter or PIN in the dashboard under
Settings ▸ Bluetooth. Status: `cat /run/rascar/obd-link.status`. BLE-only adapters are not supported.

The on-board RTL8852BE Bluetooth has no antennas fitted on the Orange Pi 5 Plus and in tests never
found the car's adapter. A USB dongle (tested: TP-Link UB500, RTL8761BU) works. With the dongle option,
the installer adds its firmware and de-authorizes the on-board controller (`0489:e123`).

### 4G modem

- **SIMCom SIM7600E-H** (QMI, `1e0e:9001`): `rascar-4g-qmi` dials when `/dev/cdc-wdm0` appears, keeps the
  link up and resets the stick when its QMI channel wedges. Set the APN in `/etc/rascar/4g.env`
  (`APN=...`; empty uses the network default). The default route gets metric 2048, so home Ethernet and
  Wi-Fi stay preferred.
- **RNDIS / CDC-Ether sticks** (HiLink style): plain DHCP through
  `/etc/systemd/network/08-rascar-4g-dongle.network`, also at metric 2048.
- **D-Link DWM-222W**: boots with its radio off. `rascar-4g-radio` sends `AT+CFUN=1` when it appears.

### Offline maps and routing

The navigation page uses Protomaps vector tiles and an offline geocoder, served by `rascar-app` from
`/var/lib/rascar/maps`, plus a Valhalla routing graph in `/var/lib/rascar/routing`. Build the data on a
PC with internet access and copy it over:

```sh
# Tiles, fonts and sprites for your region (west,south,east,north in degrees)
sudo BBOX=5.8,47.2,15.1,55.1 MAPS_DIR=/tmp/maps setup/maps/install-maps.sh
# Search index and routing graph from OpenStreetMap extracts (*.osm.pbf, e.g. from download.geofabrik.de)
pip install osmium && setup/maps/build-geocoder.py /tmp/maps/geocoder.sqlite osm/*.osm.pbf
setup/maps/build-routing.sh osm/ /tmp/routing       # needs podman or docker
rsync -a /tmp/maps/ <user>@<board>:/tmp/maps/ && rsync -a /tmp/routing/ <user>@<board>:/tmp/routing/
# on the board
sudo rsync -a /tmp/maps/ /var/lib/rascar/maps/ && sudo rsync -a /tmp/routing/ /var/lib/rascar/routing/
sudo podman pull ghcr.io/valhalla/valhalla:latest && sudo systemctl start rascar-valhalla
```

`rascar-valhalla` only starts once `/var/lib/rascar/routing/tiles.tar` exists, and listens on loopback
only.

### OTA updates and log upload

Optional, and only useful with your own server. See [ota.md](ota.md).

## 10. Logs and debugging

The car cuts power without warning. With power-cut hardening the journal survives that (on the NVMe,
synced every 5 s: `journalctl -b -1` shows the previous drive). Without it, `/var/log` is Armbian's RAM
disk and the journal of a drive is gone by the next boot. Either way, all CarPlay evidence is written to
the NVMe, under
**`/var/lib/rascar-debug/`**, in one directory per boot (the newest 20 are kept):

| Path | Contents |
|---|---|
| `latest/` | symlink to the current boot's directory |
| `boot-*/runs/NNN-HHMMSS-carplay.log` | one file per CarPlay attempt: environment, cdc_ncm version, the transmitter's full log (with `RASCAR_WIRE_TRACE=1`, hex dumps of USB/iAP2/RTSP), exit code |
| `boot-*/runs/NNN-*-net.pcap`, `-usb.pcap`, `-kernel.log` | network and USB captures of that attempt (first 60 s by default), kernel log tail |
| `boot-*/kernel.log`, `udev.log`, `state.log`, `ip-monitor.log`, `avahi.log`, `dwc3-trace.log`, `usbmon.pcap` | `rascar-debugcap`: everything the board saw during that boot |
| `boot-*/journal.log` | journal of rascar-carplay, rascar-app, avahi and 4G |
| `earlyboot/` | first 90 s of every boot, fsynced every second (for boots that die early) |
| `thermal.log`, `obd/`, `car-gps.nmea` | throttling steps, raw OBD traffic, the car's GPS sentences |

Useful commands:

```sh
journalctl -fu rascar-carplay                 # live, over SSH or the hotspot
less /var/lib/rascar-debug/latest/runs/*-carplay.log
sudo rascar-preflight                         # readiness check
sudo rascar-bundle 3                          # pack the last 3 boots into one tarball
scp <user>@<board>:/var/lib/rascar-debug/bundles/rascar-debug-*.tar.gz .
```

Attach a bundle when you report a problem with a new car. It holds no secrets, but it does hold the
car's identifiers (VIN-like serials, Bluetooth address), so review it before posting it publicly.

## 11. Updating

```sh
# on the PC
git pull
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36 -p rascar-app -p rascar-carplay -p rascar-agent
# copy as in section 4, then on the board (bench supply)
cd ~/orangecar && sudo setup/install.sh --yes
sudo systemctl restart rascar-app rascar-carplay
```

After a kernel upgrade, also run `sudo kernel/cdc_ncm/build.sh` and reboot. With an update server
configured, [OTA releases](ota.md) do the same without SSH.
