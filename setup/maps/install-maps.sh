#!/bin/bash
# install-maps.sh — offline map data for the dashboard's own navigation map
# (app/frontend/js/navmap.js), served by rascar-app at /maps from $MAPS_DIR.
#
#   region.pmtiles            Protomaps vector tiles (OpenStreetMap), z0-15
#                             (a few GB for a few countries)
#   fonts/<Noto Sans *>/      label glyphs (protomaps/basemaps-assets)
#   sprites/v4/               map icons (protomaps/basemaps-assets)
#
# Runs on the Pi (needs internet) or anywhere, then rsync the directory.
# Re-run to refresh the tiles to the latest daily build.
#
#   sudo BBOX=<west>,<south>,<east>,<north> setup/maps/install-maps.sh
#
# BBOX examples (degrees): Germany 5.8,47.2,15.1,55.1
# · France -5.2,41.3,9.6,51.1 · Bay Area -123.0,36.9,-121.5,38.4. Pick the area
# you drive in: tiles outside it are simply blank.
set -euo pipefail
MAPS_DIR=${MAPS_DIR:-/var/lib/rascar/maps}
BBOX=${BBOX:?set BBOX=<west>,<south>,<east>,<north> (see the header for examples)}
PMTILES_VERSION=${PMTILES_VERSION:-1.31.2}
log() { echo "install-maps: $*"; }

mkdir -p "$MAPS_DIR"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

case "$(uname -m)" in
    aarch64) arch=arm64 ;;
    x86_64) arch=x86_64 ;;
    *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;;
esac
log "pmtiles CLI $PMTILES_VERSION ($arch)"
curl -fsSL "https://github.com/protomaps/go-pmtiles/releases/download/v${PMTILES_VERSION}/go-pmtiles_${PMTILES_VERSION}_Linux_${arch}.tar.gz" \
    | tar xz -C "$work" pmtiles

build=$(curl -fsSL https://build-metadata.protomaps.dev/builds.json \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)[-1]["key"])')
log "tiles: $build, bbox $BBOX (a few GB)"
"$work/pmtiles" extract "https://build.protomaps.com/$build" "$MAPS_DIR/region.pmtiles.tmp" --bbox="$BBOX"
mv "$MAPS_DIR/region.pmtiles.tmp" "$MAPS_DIR/region.pmtiles"

log "fonts + sprites"
git clone -q --depth 1 --filter=blob:none --sparse https://github.com/protomaps/basemaps-assets.git "$work/assets"
git -C "$work/assets" sparse-checkout set "fonts/Noto Sans Regular" "fonts/Noto Sans Medium" "fonts/Noto Sans Italic" sprites/v4
rm -rf "$MAPS_DIR/fonts" "$MAPS_DIR/sprites"
cp -r "$work/assets/fonts" "$MAPS_DIR/fonts"
mkdir -p "$MAPS_DIR/sprites" && cp -r "$work/assets/sprites/v4" "$MAPS_DIR/sprites/v4"
log "done: $(du -sh "$MAPS_DIR" | cut -f1) in $MAPS_DIR"
