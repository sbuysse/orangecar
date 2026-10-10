#!/bin/bash
#
# OrangeCar ("RasCar") — board setup for an Orange Pi 5 Plus on Armbian (trixie).
#
# Turns a fresh Armbian install into the car unit: packages, boot settings,
# the dashboard (rascar-app), the CarPlay transmitter (rascar-carplay) and its
# debug capture, power/thermal guards, and optional parts (in-car Wi-Fi
# hotspot, OBD-II over Bluetooth, 4G modem, offline maps, OTA updates).
#
# Idempotent: safe to re-run after a git pull. Files under /etc/rascar and
# ~/.config/rascar that already exist are kept. It never starts, stops or
# restarts a running service: everything takes effect at the next boot (or with
# the commands printed at the end).
#
# Usage:
#   sudo setup/install.sh [options]
#
#   --user NAME          account that runs the dashboard (default: the user who
#                        ran sudo; must not be root)
#   --yes                no questions: optional parts take their defaults
#                        (sudoers, cdc-ncm and powercut on, the rest off), or the
#                        answers of the previous run
#   --with-X / --without-X   X = hotspot | powercut | obd | bt-dongle | 4g | maps |
#                        ota | sudoers | cdc-ncm | rtc-battery
#   --native-build       build the Rust binaries on the board if none are found
#                        (slow; never on car power — see docs/install.md)
#
# The binaries are taken from target/aarch64-unknown-linux-gnu/release/ (cross
# build, recommended) or target/release/ (native build) of this checkout.
#
# Full walkthrough: docs/install.md. Every file it writes: docs/configuration.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
SETUP="$SCRIPT_DIR"
INSTALL_LINK=/opt/orangecar
RELEASES=/opt/orangecar-releases
MANUAL_TREE=$RELEASES/manual

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BOLD='\033[1m'; NC='\033[0m'
log()  { echo -e "${GREEN}[install]${NC} $*"; }
warn() { echo -e "${YELLOW}[install]${NC} $*"; }
err()  { echo -e "${RED}[install]${NC} $*" >&2; }
step() { echo; echo -e "${BOLD}== $* ==${NC}"; }
die()  { err "$*"; exit 1; }

# --- Options ------------------------------------------------------------------
ASSUME_YES=0
NATIVE_BUILD=0
RASCAR_USER=${RASCAR_USER:-}
declare -A WANT=()       # part -> 1/0 when forced on the command line
while [[ $# -gt 0 ]]; do
    case $1 in
        --user) RASCAR_USER=${2:?--user needs a name}; shift 2 ;;
        --yes|-y) ASSUME_YES=1; shift ;;
        --native-build) NATIVE_BUILD=1; shift ;;
        --with-*) WANT[${1#--with-}]=1; shift ;;
        --without-*) WANT[${1#--without-}]=0; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
done
[[ -t 0 ]] || ASSUME_YES=1

# Answers of the previous run (saved in install.env) become the defaults.
declare -A PREV=()
if [[ -r /etc/rascar/install.env ]]; then
    while IFS='=' read -r k v; do
        [[ $k == RASCAR_WITH_* ]] || continue
        k=${k#RASCAR_WITH_}; k=${k//_/-}
        PREV[${k,,}]=$v
    done < /etc/rascar/install.env
fi

# ask <part> <default y|n> <question...>  -> sets WANT[part]
ask() {
    local part=$1 def=$2; shift 2
    if [[ -n ${WANT[$part]:-} ]]; then return; fi
    case ${PREV[$part]:-} in 1) def=y ;; 0) def=n ;; esac
    if (( ASSUME_YES )); then
        [[ $def == y ]] && WANT[$part]=1 || WANT[$part]=0
        return
    fi
    local hint="[y/N]" reply
    [[ $def == y ]] && hint="[Y/n]"
    echo
    read -r -p "$(echo -e "${BOLD}?${NC} $* $hint ")" reply
    reply=${reply:-$def}
    [[ $reply =~ ^[Yy] ]] && WANT[$part]=1 || WANT[$part]=0
}
want() { [[ ${WANT[$1]:-0} == 1 ]]; }

# --- Preconditions ------------------------------------------------------------
[[ $EUID -eq 0 ]] || die "run as root: sudo $0"
[[ $(uname -m) == aarch64 ]] || die "this is meant to run on the board (aarch64), not on $(uname -m)"
[[ -f /boot/armbianEnv.txt ]] || die "/boot/armbianEnv.txt not found — this expects Armbian"
. /etc/os-release
[[ ${VERSION_CODENAME:-} == trixie ]] || warn "tested on Armbian trixie; this is ${PRETTY_NAME:-unknown}"
MODEL=$(tr -d '\0' 2>/dev/null < /proc/device-tree/model || echo unknown)
[[ $MODEL == *"Orange Pi 5 Plus"* ]] || warn "tested on an Orange Pi 5 Plus; this board says '$MODEL' (USB controller, I2C and interface names may differ)"

RASCAR_USER=${RASCAR_USER:-${SUDO_USER:-}}
if [[ -z $RASCAR_USER || $RASCAR_USER == root ]]; then
    (( ASSUME_YES )) && die "pass --user <name>: the dashboard must not run as root"
    read -r -p "Account that runs the dashboard (an existing non-root user): " RASCAR_USER
fi
getent passwd "$RASCAR_USER" >/dev/null || die "user '$RASCAR_USER' does not exist (create it first: adduser $RASCAR_USER)"
[[ $RASCAR_USER != root ]] || die "the dashboard must not run as root"
RASCAR_UID=$(id -u "$RASCAR_USER")
RASCAR_HOME=$(getent passwd "$RASCAR_USER" | cut -d: -f6)

log "OrangeCar setup — project $PROJECT_DIR"
log "board: $MODEL, kernel $(uname -r)"
log "dashboard user: $RASCAR_USER (uid $RASCAR_UID, home $RASCAR_HOME)"

# --- Questions (all asked up front, then it runs unattended) -----------------
ask hotspot n "In-car Wi-Fi hotspot for SSH when no Ethernet cable is plugged in (useful for debugging in the car)?"
ask powercut y "Power-cut hardening: journal on disk instead of RAM (armbian-ramlog off), root fs commit=5, journal synced every 5 s?"
ask sudoers y "Let the dashboard user run the few root commands the dashboard needs without a password (poweroff in Park, journal sync, Wi-Fi, 4G restart)?"
ask cdc-ncm y "Build and install the patched cdc_ncm kernel module now (required for CarPlay; needs the kernel headers)?"
ask obd n "OBD-II: pair a Bluetooth ELM327 adapter automatically?"
if want obd; then
    ask bt-dongle n "  Use a USB Bluetooth dongle and switch off the on-board controller (the on-board RTL8852BE card has no antennas fitted)?"
fi
ask 4g n "4G modem support (SIMCom SIM7600 QMI, or an RNDIS/CDC-Ether stick)?"
ask maps n "Offline maps and routing (Valhalla in podman; map data is downloaded separately, several GB)?"
ask ota n "OTA updates + log push agent (only does something once you configure your own update server)?"
ask rtc-battery n "Is a coin-cell battery fitted on the board's RTC connector?"

# --- 1. Packages --------------------------------------------------------------
step "1. System packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
PKGS=(
    ca-certificates curl git rsync jq python3 python3-pip
    avahi-daemon avahi-utils            # mDNS on the car's link
    ffmpeg                              # H.264 encoding of the screen
    chromium cage wlr-randr xvfb        # dashboard kiosk (cage = GPU, Xvfb = fallback)
    pulseaudio pulseaudio-utils alsa-utils playerctl mpv mpv-mpris
    gpsd gpsd-clients                   # optional external GPS receiver
    tcpdump usbutils iw nvme-cli        # debug capture, power settings
    build-essential bc flex bison libssl-dev libelf-dev patch   # kernel module build
)
want hotspot && PKGS+=(hostapd)
want obd && PKGS+=(bluez bluez-tools rfkill)
want 4g && PKGS+=(libqmi-utils)
want maps && PKGS+=(podman)
apt-get install -y -qq --no-install-recommends "${PKGS[@]}"

# Kernel headers for the cdc_ncm build: Armbian names them after the image package.
if want cdc-ncm && [[ ! -d /lib/modules/$(uname -r)/build ]]; then
    img=$(dpkg -S "/boot/vmlinuz-$(uname -r)" 2>/dev/null | cut -d: -f1 || true)
    hdr=${img/linux-image-/linux-headers-}
    if [[ -n $img && $hdr != "$img" ]]; then
        log "installing kernel headers $hdr"
        apt-get install -y -qq "$hdr" || warn "could not install $hdr"
    else
        warn "cannot tell which headers package matches $(uname -r); install it by hand (armbian-config ▸ System ▸ Kernel headers)"
    fi
fi

# YouTube Music sidecar (app/scripts/ytmusic_helper.py). Optional: the
# dashboard shows "not connected" without it. Debian's Python is
# "externally managed" (PEP 668); this single pure-Python package goes system-wide.
if ! python3 -c 'import ytmusicapi' 2>/dev/null; then
    pip3 install -q --break-system-packages ytmusicapi 2>/dev/null \
        || warn "ytmusicapi not installed — YouTube Music features will show 'not connected'"
fi

# --- 2. Binaries + install tree ----------------------------------------------
step "2. Binaries and the /opt/orangecar tree"
BIN_DIR=""
for d in "$PROJECT_DIR/target/aarch64-unknown-linux-gnu/release" "$PROJECT_DIR/target/release"; do
    if [[ -x $d/rascar-app && -x $d/rascar-carplay ]]; then BIN_DIR=$d; break; fi
done
if [[ -z $BIN_DIR ]]; then
    if (( NATIVE_BUILD )); then
        warn "building on the board (10-30 min, high load: do this on a bench supply, never in the car)"
        if ! sudo -u "$RASCAR_USER" -H bash -lc 'command -v cargo' >/dev/null; then
            sudo -u "$RASCAR_USER" -H bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
        fi
        apt-get install -y -qq pkg-config libusb-1.0-0-dev libudev-dev
        sudo -u "$RASCAR_USER" -H bash -lc "cd '$PROJECT_DIR' && cargo build --release -p rascar-app -p rascar-carplay -p rascar-agent"
        BIN_DIR="$PROJECT_DIR/target/release"
    else
        die "no binaries in target/aarch64-unknown-linux-gnu/release/ or target/release/.
       Cross-build them on a PC and copy them here (docs/install.md, 'Build the binaries'),
       or re-run with --native-build."
    fi
fi
log "binaries from $BIN_DIR"

REAL_PROJECT=$(readlink -f "$PROJECT_DIR")
if [[ -d $INSTALL_LINK && ! -L $INSTALL_LINK && $(readlink -f "$INSTALL_LINK") == "$REAL_PROJECT" ]]; then
    # The checkout IS /opt/orangecar: use it in place (OTA then needs install-ota.sh --migrate).
    log "$INSTALL_LINK is this checkout; using it in place"
else
    install -d -m 0755 "$MANUAL_TREE/target/release" "$MANUAL_TREE/app"
    for b in rascar-app rascar-carplay rascar-agent; do
        if [[ -x $BIN_DIR/$b ]]; then install -m 0755 "$BIN_DIR/$b" "$MANUAL_TREE/target/release/$b"; fi
    done
    # rsync writes new files and renames them into place: hard links that an OTA
    # release shares with this tree are never modified in place.
    rsync -a --delete --exclude '__pycache__' "$PROJECT_DIR/app/frontend/" "$MANUAL_TREE/app/frontend/"
    rsync -a --delete --exclude '__pycache__' "$PROJECT_DIR/app/scripts/" "$MANUAL_TREE/app/scripts/"
    rsync -a --delete --exclude '_metadata' "$PROJECT_DIR/app/chromium-ext/" "$MANUAL_TREE/app/chromium-ext/"
    rsync -a --delete --exclude '__pycache__' "$PROJECT_DIR/setup/" "$MANUAL_TREE/setup/"
    if [[ -L $INSTALL_LINK ]]; then
        cur=$(readlink -f "$INSTALL_LINK")
        if [[ $cur != "$(readlink -f "$MANUAL_TREE")" ]]; then
            warn "$INSTALL_LINK points at $cur (an OTA release); updated $MANUAL_TREE but left the link alone."
            warn "  To run this checkout instead: sudo ln -sfn $MANUAL_TREE $INSTALL_LINK"
        fi
    elif [[ -e $INSTALL_LINK ]]; then
        old="$RELEASES/old-$(date +%Y%m%d-%H%M%S)"
        mv "$INSTALL_LINK" "$old"
        ln -s "$MANUAL_TREE" "$INSTALL_LINK"
        warn "moved the previous $INSTALL_LINK directory to $old"
    else
        ln -s "$MANUAL_TREE" "$INSTALL_LINK"
    fi
    log "$INSTALL_LINK -> $(readlink -f "$INSTALL_LINK")"
fi

# --- 3. Configuration ---------------------------------------------------------
step "3. Configuration (/etc/rascar, ~/.config/rascar)"
install -d -m 0755 /etc/rascar /var/lib/rascar /var/lib/rascar-debug /var/lib/rascar/maps
install -d -o "$RASCAR_USER" -g "$RASCAR_USER" -m 0755 /var/lib/rascar-debug/obd

# Who the dashboard runs as, for the root helpers (rascar-obd-link, install-ota.sh).
cat > /etc/rascar/install.env <<EOF
# Written by setup/install.sh — the account that runs rascar-app.
RASCAR_USER=$RASCAR_USER
RASCAR_UID=$RASCAR_UID
RASCAR_HOME=$RASCAR_HOME
EOF
# The answers to the questions, as defaults for the next run.
for part in "${!WANT[@]}"; do
    k=${part//-/_}
    echo "RASCAR_WITH_${k^^}=${WANT[$part]}" >> /etc/rascar/install.env
done

CONF_DIR="$RASCAR_HOME/.config/rascar"
install -d -o "$RASCAR_USER" -g "$RASCAR_USER" -m 0755 "$RASCAR_HOME/.config" "$CONF_DIR"
# A template is only written when the file does not exist.
write_template() {
    local path="$CONF_DIR/$1"
    if [[ -e $path ]]; then log "  $1 present — kept"; return; fi
    cat > "$path"
    chown "$RASCAR_USER:$RASCAR_USER" "$path"
    log "  wrote template $1"
}
# JSON has no comments: each template has one clearly fake entry showing the
# shape. An empty list [] is valid too.
write_template tv-channels.json <<'EOF'
[
  { "name": "EXAMPLE — replace me", "url": "https://example.invalid/stream.m3u8" }
]
EOF
write_template radio-stations.json <<'EOF'
[
  { "name": "EXAMPLE — replace me", "genre": "Example genre", "url": "https://example.invalid/stream.mp3" }
]
EOF
write_template contacts.json <<'EOF'
[
  { "name": "EXAMPLE — replace me", "phone": "+10000000000" }
]
EOF

if [[ ! -f /etc/rascar/rascar-app.env ]]; then
    cat > /etc/rascar/rascar-app.env <<'EOF'
# rascar-app options (passed verbatim on its command line; see docs/configuration.md).
#
#   --lat <deg> --lon <deg>   weather location used until the car's GPS has a fix (optional)
#   --ics-uris <url,url>      iCalendar feeds for the agenda (empty = none)
#   --obd-port <dev>          ELM327 serial device (default /dev/rfcomm0)
#   --maps-dir <dir>          offline map data (default /var/lib/rascar/maps)
#   --port <n> / --bind <ip>  HTTP listener (default 8080 on 0.0.0.0, no auth)
#
# Example:
#   RASCAR_OPTS=--lat 48.85 --lon 2.35 --ics-uris https://example.com/cal.ics
RASCAR_OPTS=
EOF
    log "  wrote /etc/rascar/rascar-app.env"
fi

# --- 4. Boot settings -----------------------------------------------------------
step "4. Boot settings (/boot/armbianEnv.txt, modules, sysctl)"
ENV=/boot/armbianEnv.txt
# Kernel arguments:
#   cma=256M                     contiguous memory for the GPU / video
#   nvme_core.default_ps_max_latency_us=0, pcie_aspm=off, pcie_port_pm=off
#                                no NVMe/PCIe low-power states: the drive dropped
#                                off PCIe on low-power exit, on car power
#   fsck.repair=yes              power is cut at every ignition-off
WANT_ARGS=(cma=256M nvme_core.default_ps_max_latency_us=0 pcie_aspm=off pcie_port_pm=off fsck.repair=yes)
cur_args=$(grep -E '^extraargs=' "$ENV" | head -1 | cut -d= -f2- || true)
new_args=$cur_args
for a in "${WANT_ARGS[@]}"; do
    key=${a%%=*}
    if [[ " $new_args " == *" $key="* ]]; then
        [[ " $new_args " == *" $a "* ]] || warn "  extraargs already sets $key differently ($(echo " $new_args " | grep -oE " $key=[^ ]*" | tr -d ' ')); kept, recommended $a"
    else
        new_args="${new_args:+$new_args }$a"
    fi
done
if [[ $new_args != "$cur_args" ]]; then
    cp -p "$ENV" "$ENV.bak-rascar"
    if grep -qE '^extraargs=' "$ENV"; then
        sed -i "s|^extraargs=.*|extraargs=$new_args|" "$ENV"
    else
        echo "extraargs=$new_args" >> "$ENV"
    fi
    log "  extraargs=$new_args (backup: $ENV.bak-rascar) — needs a reboot"
else
    log "  kernel arguments already set"
fi

# USB-C port (usb@fc000000) must be able to act as a device: on Armbian's
# 6.18 kernel its device tree already says dr_mode=otg, no overlay needed.
dr=$(tr -d '\0' 2>/dev/null < /sys/firmware/devicetree/base/usb@fc000000/dr_mode || echo "")
case $dr in
    otg|peripheral) log "  usb@fc000000 dr_mode=$dr (OK, no overlay needed)" ;;
    "") warn "  no usb@fc000000 in the device tree: not an RK3588, or a different layout — CarPlay needs a dual-role USB-C controller" ;;
    *) warn "  usb@fc000000 dr_mode=$dr: the USB-C port cannot act as a device; it needs an overlay setting dr_mode=\"otg\"" ;;
esac
# Overlays listed in armbianEnv.txt that the running kernel does not ship are
# silently ignored by the bootloader; say so (older guides list some for 5.x/6.1).
prefix=$(grep -E '^overlay_prefix=' "$ENV" | cut -d= -f2 || true)
for o in $(grep -E '^overlays=' "$ENV" | cut -d= -f2-); do
    found=0
    for f in /boot/dtb/*/overlay/"$prefix-$o".dtbo /boot/dtb/*/overlay/"$o".dtbo; do
        if [[ -e $f ]]; then found=1; fi
    done
    (( found )) || warn "  overlay '$o' in $ENV has no .dtbo for this kernel (ignored at boot; safe to remove)"
done

install -m 0644 "$SETUP/boot/rascar-modules.conf" /etc/modules-load.d/rascar.conf
# GPU + sound drivers at the start of boot (see the file for why).
install -m 0644 "$SETUP/wayland/rascar-early-drivers.conf" /etc/modules-load.d/
install -m 0644 "$SETUP/boot/90-rascar-powercut.conf" /etc/sysctl.d/
sysctl -q -p /etc/sysctl.d/90-rascar-powercut.conf || true

# Power-cut hardening. The car cuts power at every ignition-off, without warning.
if want powercut; then
    # Journal on the NVMe, synced every 5 s (Armbian keeps /var/log in a zram
    # disk by default, so the log of a drive was gone at the next boot).
    install -d -m 0755 /etc/systemd/journald.conf.d
    install -m 0644 "$SETUP/boot/rascar-journald.conf" /etc/systemd/journald.conf.d/20-rascar-powercut.conf
    if [[ -f /etc/default/armbian-ramlog ]]; then
        sed -i 's/^ENABLED=.*/ENABLED=false/' /etc/default/armbian-ramlog
    fi
    systemctl disable armbian-ramlog.service 2>/dev/null || true
    # Root ext4 commit interval 5 s (Armbian's fstab sets commit=600 or 120).
    if awk '$2=="/" && $3=="ext4"' /etc/fstab | grep -q .; then
        opts=$(awk '$2=="/" && $3=="ext4" {print $4; exit}' /etc/fstab)
        if [[ ,$opts, != *,commit=5,* ]]; then
            new=$(echo "$opts" | sed -E 's/(^|,)commit=[0-9]+//; s/^,//')
            new="${new:+$new,}commit=5"
            cp -p /etc/fstab /etc/fstab.bak-rascar
            awk -v n="$new" 'BEGIN{OFS="\t"} $2=="/" && $3=="ext4" && !done {$4=n; done=1} {print}' /etc/fstab.bak-rascar > /etc/fstab
            log "  root fs options: $new (backup /etc/fstab.bak-rascar) — needs a reboot"
        fi
    else
        warn "  root is not an ext4 line in /etc/fstab; set a short commit interval by hand"
    fi
    log "  power-cut hardening applied (journal on disk; armbian-ramlog off from the next boot)"
fi

if ! want rtc-battery; then
    install -m 0644 "$SETUP/rtc/rascar-no-hym8563.conf" /etc/modprobe.d/
    install -m 0644 "$SETUP/rtc/rascar-rtc-off.service" /etc/systemd/system/
    RTC_UNIT=rascar-rtc-off.service
else
    rm -f /etc/modprobe.d/rascar-no-hym8563.conf
    systemctl disable rascar-rtc-off.service 2>/dev/null || true
    RTC_UNIT=""
fi

# --- 5. udev, power and thermal guards -----------------------------------------
step "5. udev rules, power and thermal guards"
# The Wayland kiosk takes every libinput device: keep the IR receiver away from it.
install -m 0644 "$SETUP/wayland/90-rascar-libinput-ignore.rules" /etc/udev/rules.d/
# udev is capped at boot: without the cap the driver probe surge dropped the
# NVMe on car power (see the file).
install -d -m 0755 /etc/systemd/system/systemd-udevd.service.d
install -m 0644 "$SETUP/boot/50-rascar-cpuquota.conf" /etc/systemd/system/systemd-udevd.service.d/
for s in thermal/rascar-thermal earlylog/rascar-earlylog; do
    install -m 0755 "$SETUP/$s" /usr/local/sbin/
done
install -m 0644 "$SETUP/thermal/rascar-thermal.service" "$SETUP/earlylog/rascar-earlylog.service" /etc/systemd/system/
NVME_UNIT=""
if [[ -e /dev/nvme0 ]]; then
    # PS2 must be an operational state on THIS drive (it is on the tested one).
    if nvme id-ctrl /dev/nvme0 2>/dev/null | grep -E '^ps +2 ' | grep -q 'operational' \
        && ! nvme id-ctrl /dev/nvme0 2>/dev/null | grep -E '^ps +2 ' | grep -q 'non-operational'; then
        install -m 0644 "$SETUP/nvme/rascar-nvme-power.service" /etc/systemd/system/
        NVME_UNIT=rascar-nvme-power.service
        log "  NVMe: power state 2 is operational — capping the drive there at boot"
    else
        log "  NVMe: power state 2 is not an operational state on this drive — leaving it alone"
    fi
fi
udevadm control --reload-rules || true

# Chromium policies for the car-screen browser (see setup/chromium/README.md).
install -d -m 0755 /etc/chromium/policies/managed
install -m 0644 "$SETUP/chromium/rascar-embedded-apps.json" /etc/chromium/policies/managed/

# --- 6. Dashboard service -----------------------------------------------------
step "6. Dashboard (rascar-app)"
cat > /etc/systemd/system/rascar-app.service <<EOF
# Generated by setup/install.sh (user $RASCAR_USER). Edit options in
# /etc/rascar/rascar-app.env, not here: a re-run of install.sh rewrites this file.
[Unit]
Description=RasCar dashboard (web UI on :8080, shown on the car screen by rascar-carplay)
After=network.target user@$RASCAR_UID.service
# The session bus and PulseAudio of the user (lingering keeps them up without a login).
Wants=user@$RASCAR_UID.service

[Service]
Type=simple
User=$RASCAR_USER
WorkingDirectory=$INSTALL_LINK/app
EnvironmentFile=-/etc/rascar/rascar-app.env
ExecStart=$INSTALL_LINK/target/release/rascar-app \$RASCAR_OPTS
Restart=always
RestartSec=5
Environment=HOME=$RASCAR_HOME
# A system unit with User= inherits neither XDG_RUNTIME_DIR nor the session bus
# address; without both, playerctl (MPRIS) and pactl find nothing.
Environment=XDG_RUNTIME_DIR=/run/user/$RASCAR_UID
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$RASCAR_UID/bus

[Install]
WantedBy=multi-user.target
EOF
rm -f /etc/systemd/system/rascar-app.service.d/dbus.conf
loginctl enable-linger "$RASCAR_USER" || warn "could not enable lingering for $RASCAR_USER"
for g in audio video render input dialout netdev bluetooth; do
    if getent group "$g" >/dev/null; then usermod -aG "$g" "$RASCAR_USER"; fi
done

if want sudoers; then
    tmp=$(mktemp)
    cat > "$tmp" <<EOF
# Written by setup/install.sh: root commands rascar-app runs through "sudo -n".
$RASCAR_USER ALL=(root) NOPASSWD: /usr/bin/systemctl poweroff, /usr/bin/journalctl --sync, \\
    /usr/bin/systemctl start rascar-4g-qmi.service, /usr/bin/systemctl stop rascar-4g-qmi.service, \\
    /usr/sbin/wpa_cli *, /usr/bin/ip link set * down, /usr/sbin/ip link set * down
EOF
    if visudo -cqf "$tmp"; then
        install -m 0440 "$tmp" /etc/sudoers.d/rascar-app
        log "  /etc/sudoers.d/rascar-app installed"
    else
        warn "  sudoers snippet failed validation; not installed"
    fi
    rm -f "$tmp"
fi

# --- 7. Patched cdc_ncm -----------------------------------------------------------
if want cdc-ncm; then
    step "7. Patched cdc_ncm kernel module"
    bash "$PROJECT_DIR/kernel/cdc_ncm/build.sh" \
        || warn "cdc_ncm build failed — CarPlay cannot bring up the car's network link until it is built (kernel/README.md)"
else
    warn "skipped the cdc_ncm module: run 'sudo kernel/cdc_ncm/build.sh' before the first car test"
fi

# --- 8. CarPlay transmitter + debug capture + hotspot --------------------------
step "8. CarPlay transmitter, debug capture$(want hotspot && echo ', hotspot')"
HS_ARGS=()
want hotspot || HS_ARGS+=(--no-hotspot)
bash "$SETUP/carplay-debug/install-carplay-debug.sh" "${HS_ARGS[@]}"
if ! want hotspot; then
    systemctl disable rascar-hotspot.service 2>/dev/null || true
fi

# --- 9. Optional parts --------------------------------------------------------------
if want obd; then
    step "9a. OBD-II over Bluetooth"
    if want bt-dongle; then
        # armbian-firmware lacks rtl8761bu and Conflicts with Debian's
        # firmware-realtek, so take only the two RTL8761BU files out of it.
        if [[ ! -e /usr/lib/firmware/rtl_bt/rtl8761bu_fw.bin ]]; then
            fwtmp=$(mktemp -d); chmod 755 "$fwtmp"   # _apt must be able to write the .deb
            if (cd "$fwtmp" && apt-get download -qq firmware-realtek); then
                dpkg-deb -x "$fwtmp"/firmware-realtek_*.deb "$fwtmp/x"
                install -m 0644 "$fwtmp/x/usr/lib/firmware/rtl_bt/rtl8761bu_fw.bin" \
                                "$fwtmp/x/usr/lib/firmware/rtl_bt/rtl8761bu_config.bin" \
                                /usr/lib/firmware/rtl_bt/ 2>/dev/null \
                    || warn "  rtl8761bu firmware not in firmware-realtek; check your dongle's firmware"
            else
                warn "  could not download firmware-realtek (needs Debian non-free-firmware in sources)"
            fi
            rm -rf "$fwtmp"
        fi
        install -m 0644 "$SETUP/bluetooth/70-rascar-bt-onboard-off.rules" /etc/udev/rules.d/
    fi
    # Link supervision 2 s (3200 x 0.625 ms) instead of 20 s: a dead link to the
    # adapter is noticed at once and the reconnect starts immediately.
    if grep -qE '^#?LinkSupervisionTimeout=' /etc/bluetooth/main.conf 2>/dev/null; then
        sed -i 's/^#\?LinkSupervisionTimeout=.*/LinkSupervisionTimeout=3200/' /etc/bluetooth/main.conf
    fi
    install -m 0755 "$SETUP/obd/rascar-obd-link" /usr/local/sbin/
    install -m 0644 "$SETUP/obd/rascar-obd-link.service" /etc/systemd/system/
    systemctl enable rascar-obd-link.service
    udevadm control --reload-rules || true
fi

if want 4g; then
    step "9b. 4G modem"
    install -m 0755 "$SETUP/4g/rascar-4g-qmi" "$SETUP/4g/rascar-4g-radio" /usr/local/sbin/
    install -m 0644 "$SETUP/4g/rascar-4g-qmi.service" "$SETUP/4g/rascar-4g-radio.service" /etc/systemd/system/
    install -m 0644 "$SETUP/4g/90-rascar-4g.rules" /etc/udev/rules.d/
    install -d -m 0755 /etc/systemd/network
    install -m 0644 "$SETUP/4g/08-rascar-4g-dongle.network" /etc/systemd/network/
    if [[ ! -f /etc/rascar/4g.env ]]; then
        printf '# APN for the QMI modem (empty = the network default). Example: APN=internet\nAPN=\n' > /etc/rascar/4g.env
    fi
    udevadm control --reload-rules || true
fi

if want maps; then
    step "9c. Offline maps and routing"
    install -d -m 0755 /var/lib/rascar/routing
    install -m 0644 "$SETUP/maps/rascar-valhalla.service" /etc/systemd/system/
    systemctl enable rascar-valhalla.service
    log "  map data is not downloaded here (several GB). See docs/install.md 'Offline maps':"
    log "    sudo BBOX=<west>,<south>,<east>,<north> $SETUP/maps/install-maps.sh"
    log "    setup/maps/build-routing.sh on a PC, then copy the output to /var/lib/rascar/routing"
fi

if want ota; then
    step "9d. OTA agent"
    bash "$SETUP/ota/install-ota.sh"
fi

# --- 10. Enable -------------------------------------------------------------------
step "10. Enabling units for the next boot"
systemctl daemon-reload
# shellcheck disable=SC2086
systemctl enable rascar-app.service rascar-thermal.service rascar-earlylog.service $RTC_UNIT $NVME_UNIT
# Left over from older setups (X11 kiosk / Android Auto source): the CarPlay
# service draws the car screen itself now.
for u in rascar.target rascar-kiosk.service rascar-xorg.service; do
    systemctl disable "$u" 2>/dev/null || true
done

echo
log "=============================================="
log "  OrangeCar setup complete (nothing was started)"
log "=============================================="
cat <<EOF

  1. Reboot (kernel arguments, modules, udev cap):   sudo reboot
  2. After the reboot, on the bench:
       sudo rascar-preflight                 # every line should be PASS / WARN
       curl -s localhost:8080 | head -3      # the dashboard answers
  3. Fill in your data (templates contain one fake example each):
       $CONF_DIR/tv-channels.json, radio-stations.json, contacts.json
       /etc/rascar/rascar-app.env            # calendars, fallback weather location
       /etc/rascar/carplay.env               # CarPlay runtime settings
  4. Plug the board's USB-C port into the car's CarPlay USB port and watch:
       journalctl -fu rascar-carplay
       ls /var/lib/rascar-debug/latest/runs/
EOF
want hotspot && echo "     In the car: Wi-Fi hotspot (sudo cat /etc/rascar/hotspot.pass), then ssh $RASCAR_USER@10.42.0.1"
want ota && echo "  OTA: configure /etc/rascar/update-server.json and /etc/rascar/ota.pub (docs/ota.md)."
echo
echo "  Details: docs/install.md · docs/configuration.md"
