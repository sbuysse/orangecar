#!/bin/bash
#
# install-carplay-debug.sh — install the CarPlay transmitter service and its
# always-on evidence capture (called by setup/install.sh, can be re-run alone).
#
# Installs the transmitter runner, the persistent debug capture, the preflight
# check, the bundle collector and, optionally, the in-car Wi-Fi hotspot, and
# enables them for the next boot. Idempotent: existing /etc/rascar files are
# kept.
#
# It never starts, stops or restarts a running service: the change takes effect
# at the next boot, or immediately with the commands printed at the end.
#
# Usage: sudo ./install-carplay-debug.sh [--no-hotspot]
#   Hotspot settings come from the environment when set (install.sh passes them):
#   RASCAR_HOTSPOT_WLAN, RASCAR_HOTSPOT_ETH, RASCAR_HOTSPOT_COUNTRY, RASCAR_HOTSPOT_SSID

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SETUP="$(dirname "$HERE")"
log() { echo "[carplay] $*"; }

HOTSPOT=1
for a in "$@"; do
    case $a in
        --no-hotspot) HOTSPOT=0 ;;
        *) echo "unknown option $a" >&2; exit 2 ;;
    esac
done

[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }

log "installing scripts to /usr/local/sbin"
for s in rascar-preflight rascar-debugcap rascar-carplay-run rascar-bundle rascar-hotspot rascar-chromium-stop; do
    install -m 0755 "$HERE/$s" /usr/local/sbin/$s
done

log "installing systemd units"
for u in rascar-carplay.service rascar-debugcap.service rascar-hotspot.service; do
    install -m 0644 "$HERE/$u" /etc/systemd/system/$u
done

# Clean browser stop before the service is signalled (cookies flushed).
install -d -m 0755 /etc/systemd/system/rascar-carplay.service.d
install -m 0644 "$HERE/20-chromium-clean-stop.conf" /etc/systemd/system/rascar-carplay.service.d/

# The car's CDC-NCM link: keep the kernel name and keep networkd off it.
install -d -m 0755 /etc/systemd/network
install -m 0644 "$HERE/01-rascar-carplay-ncm.link" "$HERE/01-rascar-carplay-ncm.network" /etc/systemd/network/

install -d -m 0755 /etc/rascar /var/lib/rascar /var/lib/rascar-debug

if [[ ! -f /etc/rascar/carplay.env ]]; then
    # The ctrl host label should be this board's hostname (what avahi publishes).
    sed "s/^RASCAR_CTRL_HOSTLABEL=.*/RASCAR_CTRL_HOSTLABEL=$(hostname -s)/" "$SETUP/carplay.env.example" > /etc/rascar/carplay.env
    chmod 0644 /etc/rascar/carplay.env
    log "wrote /etc/rascar/carplay.env (from setup/carplay.env.example)"
else
    log "/etc/rascar/carplay.env already present — keeping it"
fi

log "usbmon autoload"
echo usbmon > /etc/modules-load.d/rascar-debug.conf

systemctl enable rascar-debugcap.service rascar-carplay.service avahi-daemon.service

if (( HOTSPOT )); then
    log "hotspot config"
    if [[ $(wc -c < /etc/rascar/hotspot.pass 2>/dev/null || echo 0) -lt 8 ]]; then
        # Bounded input: `tr < /dev/urandom | head` dies of SIGPIPE under pipefail.
        head -c 256 /dev/urandom | tr -dc 'a-km-np-zA-HJ-NP-Z2-9' | cut -c1-14 > /etc/rascar/hotspot.pass
        log "generated a hotspot passphrase in /etc/rascar/hotspot.pass"
    fi
    chmod 0600 /etc/rascar/hotspot.pass
    if [[ ! -f /etc/rascar/hotspot.conf ]]; then
        cat > /etc/rascar/hotspot.conf <<EOF
# rascar-hotspot settings (see the header of /usr/local/sbin/rascar-hotspot).
# Empty interface = auto-detect. COUNTRY = your regulatory domain (e.g. DE, FR, US).
WLAN=${RASCAR_HOTSPOT_WLAN:-}
ETH=${RASCAR_HOTSPOT_ETH:-}
COUNTRY=${RASCAR_HOTSPOT_COUNTRY:-}
SSID=${RASCAR_HOTSPOT_SSID:-RasCar-debug}
EOF
        log "wrote /etc/rascar/hotspot.conf"
    fi
    # Resolve the interface the same way rascar-hotspot does.
    WLAN="" COUNTRY="" SSID=RasCar-debug
    # shellcheck disable=SC1091
    . /etc/rascar/hotspot.conf
    if [[ -z $WLAN ]]; then
        for d in /sys/class/net/*; do
            [[ -d $d/wireless || -d $d/phy80211 ]] && { WLAN=$(basename "$d"); break; }
        done
    fi
    if [[ -z $WLAN ]]; then
        log "WARNING: no Wi-Fi interface found — hotspot not configured"
    else
        # Debian's hostapd@.service BindsTo the net device named by the instance,
        # so the instance (and therefore the config file name) is the interface.
        install -d -m 0755 /etc/hostapd
        {
            echo "interface=$WLAN"
            echo "driver=nl80211"
            echo "ssid=$SSID"
            [[ -n $COUNTRY ]] && echo "country_code=$COUNTRY"
            cat <<EOF
hw_mode=g
channel=6
ieee80211n=1
wmm_enabled=1
auth_algs=1
wpa=2
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
wpa_passphrase=$(cat /etc/rascar/hotspot.pass)
EOF
        } > "/etc/hostapd/$WLAN.conf"
        chmod 0600 "/etc/hostapd/$WLAN.conf"
        systemctl unmask "hostapd@$WLAN.service" >/dev/null 2>&1 || true
        systemctl enable rascar-hotspot.service
        log "hotspot: SSID $SSID on $WLAN (passphrase: sudo cat /etc/rascar/hotspot.pass)"
    fi
fi

systemctl daemon-reload

# The Android Auto source experiment (experimental/bridge) owns the same USB
# controller; never boot both.
systemctl disable rascar-bridge.service 2>/dev/null || true

cat <<EOF

[carplay] installed. Nothing running was touched.

  Takes effect at next boot. To activate now instead:
    sudo systemctl start rascar-debugcap rascar-carplay

  Check readiness:     sudo rascar-preflight
  Watch live:          journalctl -fu rascar-carplay
  Evidence on disk:    /var/lib/rascar-debug/latest/   (runs/NNN-*.log, state.log, kernel.log, usbmon.pcap ...)
  Pack for analysis:   sudo rascar-bundle
EOF
if (( HOTSPOT )); then
    echo "  In the car:          Wi-Fi \"${SSID:-RasCar-debug}\" (pass: sudo cat /etc/rascar/hotspot.pass) -> ssh ${SUDO_USER:-<user>}@10.42.0.1"
fi
