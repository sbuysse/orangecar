#!/bin/bash
# build-routing.sh — Valhalla routing graph for the dashboard's offline navigation
# (app/frontend/js/navmap.js → rascar-app /api/route → valhalla_service on the Pi).
#
# Builds on any machine with podman or docker (the graph is architecture
# independent), with the SAME Valhalla image the Pi runs, then rsync the output
# directory to the Pi's /var/lib/rascar/routing.
#
#   setup/maps/build-routing.sh <dir-with-osm.pbf-files> <out-dir>
#
# Region = the offline map's (see install-maps.sh). Extracts per country or
# region: download.geofabrik.de or download.openstreetmap.fr.
set -euo pipefail
SRC=$(readlink -f "${1:?osm dir}")
OUT=$(readlink -f "${2:?out dir}")
IMAGE=${VALHALLA_IMAGE:-ghcr.io/valhalla/valhalla:latest}
RUN=$(command -v podman || command -v docker)
mkdir -p "$OUT"
log() { echo "build-routing: $*"; }

# One input file: Valhalla warns that several extracts break the ways that cross
# their borders (valhalla#3925), i.e. exactly the cross-border roads. Merge them first (osmium, from a Debian container).
PBFS=("$SRC"/*.osm.pbf)
if [ "${#PBFS[@]}" -gt 1 ]; then
    log "merging ${#PBFS[@]} extracts with osmium"
    mkdir -p "$OUT/merged"
    "$RUN" run --rm -v "$SRC:/osm:ro,Z" -v "$OUT/merged:/m:Z" docker.io/library/debian:trixie bash -euc \
        'apt-get update -qq >/dev/null && apt-get install -y -qq osmium-tool >/dev/null && osmium merge /osm/*.osm.pbf -o /m/region.osm.pbf --overwrite'
    INPUT="$OUT/merged"
else
    INPUT="$SRC"
fi

log "image $IMAGE ($("$RUN" run --rm "$IMAGE" valhalla_service --version 2>&1 | head -1))"
"$RUN" run --rm -v "$INPUT:/osm:ro,Z" -v "$OUT:/out:Z" "$IMAGE" bash -euc '
  cd /out
  valhalla_build_config \
      --mjolnir-tile-dir /out/tiles \
      --mjolnir-tile-extract /out/tiles.tar \
      --mjolnir-timezone /out/timezones.sqlite \
      --mjolnir-admin /out/admins.sqlite \
      --mjolnir-concurrency "$(nproc)" > /out/valhalla.json
  valhalla_build_timezones > /out/timezones.sqlite
  valhalla_build_admins -c /out/valhalla.json /osm/*.osm.pbf
  valhalla_build_tiles -c /out/valhalla.json /osm/*.osm.pbf
  valhalla_build_extract -c /out/valhalla.json -v
  rm -rf /out/tiles
'
rm -rf "$OUT/merged"
# The config the Pi's container uses: same paths, mounted at /data there.
# Loopback only on the Pi: rascar-app is the one client (never the hotspot/LAN).
sed -e 's#/out/#/data/#g' -e 's#tcp://\*:8002#tcp://127.0.0.1:8002#' "$OUT/valhalla.json" > "$OUT/valhalla.pi.json"
log "done: $(du -sh "$OUT" | cut -f1) in $OUT"
