#!/bin/bash
#
# install-ota.sh — install the (optional) log-push + OTA agent and its boot guard.
#
# The agent does nothing until an update server is configured in
# /etc/rascar/update-server.json (see docs/ota.md); installing it is harmless.
#
#   sudo setup/ota/install-ota.sh [--pubkey <hex>] [--migrate]
#
#   --pubkey   write /etc/rascar/ota.pub (the release signer's PUBLIC key,
#              printed by `rascar-release keygen` on the dev machine)
#   --migrate  one-time: turn a plain /opt/orangecar directory into a symlink
#              to /opt/orangecar-releases/base-<date> (a rename, nothing copied).
#              Required before the first OTA release can be applied. Not needed
#              when setup/install.sh created /opt/orangecar (already a symlink).
#
# Like install-carplay-debug.sh it never starts, stops or restarts a running
# service: units are enabled for the next boot, and the commands to start
# them now are printed at the end.
#
# The agent binary itself is shipped like the others, in
# /opt/orangecar/target/release/rascar-agent (installed by setup/install.sh the
# first time, then every OTA release carries it).

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
LINK=/opt/orangecar
RELEASES=/opt/orangecar-releases
STATE=/var/lib/rascar-agent
log() { echo "[ota] $*"; }

PUBKEY=""
MIGRATE=0
while [[ $# -gt 0 ]]; do
    case $1 in
        --pubkey) PUBKEY=${2:?--pubkey needs a value}; shift 2 ;;
        --migrate) MIGRATE=1; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }

# The dashboard user (its settings.json holds the "OTA / logs over 4G" choices).
# shellcheck disable=SC1091
[[ -r /etc/rascar/install.env ]] && . /etc/rascar/install.env
RASCAR_USER=${RASCAR_USER:-${SUDO_USER:-}}
RASCAR_HOME=${RASCAR_HOME:-}
if [[ -z $RASCAR_HOME && -n $RASCAR_USER ]]; then
    RASCAR_HOME=$(getent passwd "$RASCAR_USER" | cut -d: -f6)
fi

# Copy everything this script installs BEFORE a migration renames the tree
# it is running from.
log "boot guard + units"
install -m 0755 "$HERE/rascar-ota-boot" /usr/local/sbin/rascar-ota-boot
install -m 0644 "$HERE/rascar-ota-boot.service" /etc/systemd/system/rascar-ota-boot.service
install -m 0644 "$HERE/rascar-agent.service" /etc/systemd/system/rascar-agent.service
install -d -m 0700 "$STATE"
install -d -m 0755 /etc/rascar

if [[ ! -f /etc/rascar/agent.json ]]; then
    # Defaults are in agent/src/config.rs; only what differs per board goes here.
    if [[ -n $RASCAR_HOME ]]; then
        printf '{\n  "dashboardSettings": "%s/.config/rascar/settings.json"\n}\n' "$RASCAR_HOME" > /etc/rascar/agent.json
    else
        echo '{}' > /etc/rascar/agent.json
    fi
    chmod 0644 /etc/rascar/agent.json
    log "wrote /etc/rascar/agent.json"
fi

# Template for the server settings; the agent only reads update-server.json.
cat > /etc/rascar/update-server.json.example <<'JSON'
{
  "lanUrl": "http://updates.lan",
  "remoteUrl": "https://updates.example.org",
  "certSha256": null,
  "username": "car",
  "password": "change-me"
}
JSON
chmod 0644 /etc/rascar/update-server.json.example
if [[ -f /etc/rascar/update-server.json ]]; then
    chmod 0600 /etc/rascar/update-server.json
    log "update server configured (/etc/rascar/update-server.json)"
else
    log "no /etc/rascar/update-server.json: the agent stays idle (see docs/ota.md)"
fi

if [[ -n $PUBKEY ]]; then
    [[ $PUBKEY =~ ^[0-9a-fA-F]{64}$ ]] || { echo "--pubkey must be 64 hex digits" >&2; exit 1; }
    echo "$PUBKEY" > /etc/rascar/ota.pub
    chmod 0644 /etc/rascar/ota.pub
    log "wrote /etc/rascar/ota.pub"
elif [[ ! -f /etc/rascar/ota.pub ]]; then
    log "WARNING: no /etc/rascar/ota.pub yet — the agent will refuse every release until it exists"
fi

if (( MIGRATE )); then
    if [[ -L $LINK ]]; then
        log "$LINK is already a symlink -> $(readlink -f "$LINK")"
    elif [[ -d $LINK ]]; then
        install -d -m 0755 "$RELEASES"
        # A rename only works within one filesystem; refuse rather than copy.
        if [[ $(stat -c %d "$LINK") != $(stat -c %d "$RELEASES") ]]; then
            echo "$LINK and $RELEASES are on different filesystems; aborting" >&2
            exit 1
        fi
        base="$RELEASES/base-$(date +%Y%m%d-%H%M%S)"
        # Running processes keep their open files and cwd across the rename;
        # the symlink appears a few ms later, before any service restarts.
        mv "$LINK" "$base"
        ln -s "$base" "$LINK"
        sync
        log "migrated: $LINK -> $base"
    else
        echo "$LINK does not exist" >&2
        exit 1
    fi
fi

systemctl daemon-reload
systemctl enable rascar-ota-boot.service rascar-agent.service

cat <<EOF

[ota] installed. Nothing running was touched.

  Start now:        sudo systemctl start rascar-agent
  Watch:            journalctl -fu rascar-agent
  Local state:      sudo $LINK/target/release/rascar-agent status
  Force a check:    sudo $LINK/target/release/rascar-agent check-now
  Apply on bench:   sudo $LINK/target/release/rascar-agent apply-now   (restarts rascar units)
  Otherwise a staged release is applied at the next boot by rascar-ota-boot.
EOF
