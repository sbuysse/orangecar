#!/bin/bash
#
# build.sh — build and install the patched cdc_ncm module for the running kernel.
#
#   sudo kernel/cdc_ncm/build.sh [--no-autoload] [--src <cdc_ncm.c>]
#
# 1. gets the stock drivers/net/usb/cdc_ncm.c matching `uname -r`
#    (a linux-source tarball in /usr/src if one matches, else kernel.org);
# 2. applies cdc_ncm-rascar.patch (see kernel/README.md for what it changes);
# 3. builds it out of tree against /lib/modules/$(uname -r)/build, preparing
#    Armbian's headers package first if it ships without its host tools;
# 4. installs the module where the CarPlay runner loads it from:
#      /usr/local/lib/rascar/modules/$(uname -r)/cdc_ncm.ko
#    and, unless --no-autoload, also in /lib/modules/$(uname -r)/updates/ so the
#    module the kernel autoloads is the patched one (the hold parameter defaults
#    to off, so this is harmless for any other cdc_ncm device).
#
# Run it again after every kernel upgrade: a module only loads into the exact
# kernel it was built for. It never unloads the module currently in use; the
# runner (rascar-carplay-run) swaps it before the next CarPlay attempt.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PATCH="$HERE/cdc_ncm-rascar.patch"
KREL=$(uname -r)
KDIR=${KDIR:-/lib/modules/$KREL/build}
DEST=/usr/local/lib/rascar/modules/$KREL
AUTOLOAD=1
SRC=""

log() { echo "[cdc_ncm] $*"; }
die() { echo "[cdc_ncm] ERROR: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
    case $1 in
        --no-autoload) AUTOLOAD=0; shift ;;
        --src) SRC=$(readlink -f "${2:?--src needs a file}"); shift 2 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run as root (it may prepare the kernel headers and installs into /lib/modules)"
[[ -f $PATCH ]] || die "$PATCH missing"
[[ -d $KDIR ]] || die "no kernel build tree at $KDIR — install the headers for $KREL (Armbian: apt install linux-headers-current-<family>, e.g. linux-headers-current-rockchip64)"
for t in make gcc patch curl; do
    command -v "$t" >/dev/null || die "$t missing (apt install build-essential patch curl)"
done

# "6.18.20-current-rockchip64" -> base "6.18.20", localversion "-current-rockchip64".
BASE=$(echo "$KREL" | grep -oE '^[0-9]+\.[0-9]+(\.[0-9]+)?')
LOCALVER=${KREL#"$BASE"}
# Stable tags drop a ".0": 6.18.0 is tagged v6.18.
TAG="v${BASE%.0}"
log "kernel $KREL (upstream $TAG), build tree $KDIR"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/drivers/net/usb"
C="$WORK/drivers/net/usb/cdc_ncm.c"

# --- 1. stock source --------------------------------------------------------
if [[ -n $SRC ]]; then
    cp "$SRC" "$C"
    log "source: $SRC"
else
    for tarball in /usr/src/linux-source-"${BASE%.*}".tar.* /usr/src/linux-source-"$BASE".tar.*; do
        [[ -f $tarball ]] || continue
        member=$(tar -tf "$tarball" 2>/dev/null | grep -m1 'drivers/net/usb/cdc_ncm.c$' || true)
        if [[ -n $member ]]; then
            tar -xOf "$tarball" "$member" > "$C"
            log "source: $member from $tarball (check that it matches $KREL)"
            break
        fi
    done
    if [[ ! -s $C ]]; then
        for url in \
            "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/drivers/net/usb/cdc_ncm.c?h=$TAG" \
            "https://raw.githubusercontent.com/gregkh/linux/$TAG/drivers/net/usb/cdc_ncm.c"; do
            if curl -fsSL --max-time 60 "$url" -o "$C" && grep -q 'cdc_ncm_bind' "$C"; then
                log "source: $url"
                break
            fi
            rm -f "$C"
        done
    fi
    [[ -s $C ]] || die "could not fetch cdc_ncm.c for $TAG; pass one with --src"
fi

# --- 2. patch ---------------------------------------------------------------
if ! patch -d "$WORK" -p1 --forward --no-backup-if-mismatch < "$PATCH"; then
    die "the patch does not apply to $TAG's cdc_ncm.c (it was made against 6.18.20); port the hunks by hand"
fi
cp "$HERE/Makefile" "$WORK/drivers/net/usb/Makefile"

# --- 3. build tree ----------------------------------------------------------
# Armbian's headers are built on x86 and can lack the arm64 host tools
# (scripts/basic/fixdep, scripts/mod/modpost). `make modules_prepare` builds
# them, but it regenerates kernel.release from the Makefile alone: without a
# localversion file the result is "6.18.20", the module's vermagic no longer
# matches the kernel and insmod says "Invalid module format".
if [[ ! -x $KDIR/scripts/basic/fixdep || ! -x $KDIR/scripts/mod/modpost ]]; then
    log "preparing the headers tree (host tools missing)"
    if [[ -n $LOCALVER && ! -f $KDIR/localversion ]]; then
        echo "$LOCALVER" > "$KDIR/localversion"
        log "wrote $KDIR/localversion ($LOCALVER)"
    fi
    make -C "$KDIR" -j"$(nproc)" modules_prepare
fi
if [[ -f $KDIR/include/config/kernel.release ]]; then
    have=$(cat "$KDIR/include/config/kernel.release")
    [[ $have == "$KREL" ]] || die "headers tree says kernel.release=$have, running $KREL: the module would not load"
fi

# --- 4. build + install -----------------------------------------------------
make -C "$KDIR" M="$WORK/drivers/net/usb" -j"$(nproc)" modules
KO="$WORK/drivers/net/usb/cdc_ncm.ko"
[[ -f $KO ]] || die "build produced no cdc_ncm.ko"
ver=$(modinfo -F version "$KO")
magic=$(modinfo -F vermagic "$KO" | awk '{print $1}')
[[ $magic == "$KREL" ]] || die "built module has vermagic $magic, expected $KREL"

install -D -m 0644 "$KO" "$DEST/cdc_ncm.ko"
log "installed $DEST/cdc_ncm.ko (version $ver)"
if (( AUTOLOAD )); then
    install -D -m 0644 "$KO" "/lib/modules/$KREL/updates/cdc_ncm.ko"
    depmod -a "$KREL"
    log "installed /lib/modules/$KREL/updates/cdc_ncm.ko + depmod (autoloaded from the next load on)"
fi
log "resident now: $(cat /sys/module/cdc_ncm/version 2>/dev/null || echo 'stock or not loaded'); the CarPlay runner swaps it before its next attempt"
