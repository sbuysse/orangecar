#!/bin/bash
#
# release.sh — build, sign and upload an OrangeCar OTA release to your update server.
#
#   tools/release.sh [--no-upload] [--no-publish] [--allow-dirty] [--skip-tests] [--no-system]
#
# 1. tests (rascar-app, rascar-carplay, rascar-agent)
# 2. cross-build the car binaries like a manual install (docs/install.md):
#    cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36
#    (glibc 2.36 floor; the board runs Debian trixie) with RASCAR_BUILD_ID=<version>
# 3. assemble every sibling the binaries need at runtime (a binary shipped
#    without its frontend/scripts/extension breaks the car):
#      tree/target/release/{rascar-app,rascar-carplay,rascar-agent}
#      tree/app/{frontend,scripts,chromium-ext}   (mirrored; _metadata kept on the car)
#      tree/setup                                  (mirrored copy of setup/)
#      system/...                                  (tools/release-system-files.txt)
# 4. bundle + manifest (sha256 of everything) + ed25519 signature
# 5. upload manifest.json / manifest.sig / bundle.tar.gz to the update server
#    and publish (admin API described in docs/ota.md)
#
# Environment:
#   RASCAR_OTA_KEY        signing key (default ~/.config/rascar/ota-signing.key, 0600)
#   RASCAR_OTA_URL        update server base URL (required unless --no-upload)
#   RASCAR_OTA_USER / RASCAR_OTA_PASSWORD   an ADMIN account (asked if unset)
#   RASCAR_RELEASE_DIR    where releases are assembled
#                         (default ${XDG_DATA_HOME:-~/.local/share}/orangecar/releases)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY=${RASCAR_OTA_KEY:-$HOME/.config/rascar/ota-signing.key}
SERVER_URL=${RASCAR_OTA_URL:-}
OUT_BASE=${RASCAR_RELEASE_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/orangecar/releases}
TRIPLE=aarch64-unknown-linux-gnu
GLIBC=2.36
BINS=(rascar-app rascar-carplay rascar-agent)

UPLOAD=1 PUBLISH=1 DIRTY=0 TESTS=1 SYSTEM=1
for a in "$@"; do
    case $a in
        --no-upload) UPLOAD=0 ;;
        --no-publish) PUBLISH=0 ;;
        --allow-dirty) DIRTY=1 ;;
        --skip-tests) TESTS=0 ;;
        --no-system) SYSTEM=0 ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown option $a" >&2; exit 2 ;;
    esac
done

log() { echo "[release] $*"; }
die() { echo "[release] ERROR: $*" >&2; exit 1; }

cd "$ROOT"
(( ! UPLOAD )) || [[ -n $SERVER_URL ]] || die "set RASCAR_OTA_URL to your update server (or use --no-upload)"
[[ -f $KEY ]] || die "no signing key at $KEY (create it once: cargo run -p rascar-agent --bin rascar-release -- keygen --key $KEY)"
[[ $(stat -c %a "$KEY") == 600 ]] || die "$KEY must be mode 0600"

SHA=$(git rev-parse --short=7 HEAD)
if [[ -n $(git status --porcelain --untracked-files=no) ]]; then
    (( DIRTY )) || die "working tree has uncommitted changes (commit, or --allow-dirty)"
    SHA="$SHA-dirty"
fi
SERIAL=$(date +%s)
VERSION="$(date +%Y%m%d.%H%M)-$SHA"
OUT="$OUT_BASE/$VERSION"
STAGE="$OUT/staging"
mkdir -p "$OUT"
log "version $VERSION (serial $SERIAL) → $OUT"

if (( TESTS )); then
    log "tests"
    cargo test -q -p rascar-app -p rascar-carplay -p rascar-agent
fi

log "host tool"
cargo build -q --release -p rascar-agent --bin rascar-release
TOOL="$ROOT/target/release/rascar-release"

log "cross build ($TRIPLE.$GLIBC)"
# Not piped (set -e sees cargo's own status): a successful run means every
# binary is up to date with this tree. No mtime check: cargo rightly skips
# relinking a crate whose inputs did not change (rascar-app does not read
# RASCAR_BUILD_ID), which an "is it newer than the build start" test rejected.
RASCAR_BUILD_ID="$VERSION" cargo zigbuild --release --target "$TRIPLE.$GLIBC" \
    -p rascar-app -p rascar-carplay -p rascar-agent
for b in "${BINS[@]}"; do
    f="$ROOT/target/$TRIPLE/release/$b"
    [[ -f $f ]] || die "$f missing"
    file "$f" | grep -q 'ARM aarch64' || die "$f is not an aarch64 binary"
done

log "assemble"
rm -rf "$STAGE"
mkdir -p "$STAGE/tree/target/release"
for b in "${BINS[@]}"; do
    install -m 0755 "$ROOT/target/$TRIPLE/release/$b" "$STAGE/tree/target/release/$b"
done
mkdir -p "$STAGE/tree/app"
rsync -a --exclude '__pycache__' "$ROOT/app/frontend/" "$STAGE/tree/app/frontend/"
rsync -a --exclude '__pycache__' "$ROOT/app/scripts/" "$STAGE/tree/app/scripts/"
rsync -a --exclude '_metadata' "$ROOT/app/chromium-ext/" "$STAGE/tree/app/chromium-ext/"
rsync -a --exclude '__pycache__' "$ROOT/setup/" "$STAGE/tree/setup/"
# Symlinks are not shipped (the agent refuses them): materialise or fail here.
if find "$STAGE" -type l | grep -q .; then
    die "symlinks in the release tree: $(find "$STAGE" -type l | head -3)"
fi

if (( SYSTEM )); then
    while read -r src dst mode; do
        [[ -z $src || $src == \#* ]] && continue
        [[ -f $ROOT/$src ]] || die "release-system-files.txt: $src missing"
        install -D -m "$mode" "$ROOT/$src" "$STAGE/system$dst"
    done < "$ROOT/tools/release-system-files.txt"
fi

log "bundle + manifest + signature"
"$TOOL" bundle --dir "$STAGE" --out "$OUT/bundle.tar.gz"
"$TOOL" manifest --dir "$STAGE" --bundle "$OUT/bundle.tar.gz" \
    --version "$VERSION" --serial "$SERIAL" --git-sha "$(git rev-parse HEAD)" \
    --mirror app/frontend,app/scripts,app/chromium-ext,setup \
    --preserve app/chromium-ext/frame-unlock/_metadata \
    --restart rascar-app,rascar-carplay \
    --health-active rascar-app --health-not-failed rascar-carplay \
    --health-http http://127.0.0.1:8080/ --settle-secs 60 \
    --out "$OUT/manifest.json"
"$TOOL" sign --key "$KEY" --manifest "$OUT/manifest.json" --out "$OUT/manifest.sig"
PUB=$("$TOOL" pubkey --key "$KEY")
"$TOOL" verify --pubkey "$PUB" --manifest "$OUT/manifest.json" --sig "$OUT/manifest.sig" --bundle "$OUT/bundle.tar.gz"
log "bundle $(du -h "$OUT/bundle.tar.gz" | cut -f1), signer public key $PUB"

(( UPLOAD )) || { log "not uploaded (--no-upload): $OUT"; exit 0; }

USER_=${RASCAR_OTA_USER:-}
PASS=${RASCAR_OTA_PASSWORD:-}
[[ -n $USER_ ]] || read -r -p "Update server admin user: " USER_
[[ -n $PASS ]] || { read -r -s -p "Password for $USER_: " PASS; echo; }
TOKEN=$(jq -n --arg u "$USER_" --arg p "$PASS" '{username:$u,password:$p}' \
    | curl -fsS --max-time 20 -H 'Content-Type: application/json' -d @- "$SERVER_URL/api/auth/login" \
    | jq -r .token)
[[ -n $TOKEN && $TOKEN != null ]] || die "update server login failed"

for f in manifest.json manifest.sig bundle.tar.gz; do
    log "upload $f"
    curl -fsS --max-time 1800 -X PUT -H "Authorization: Bearer $TOKEN" \
        --data-binary "@$OUT/$f" "$SERVER_URL/api/carops/admin/releases/$VERSION/$f" >/dev/null \
        || die "upload of $f failed"
done

if (( PUBLISH )); then
    curl -fsS --max-time 120 -X POST -H "Authorization: Bearer $TOKEN" \
        "$SERVER_URL/api/carops/admin/releases/$VERSION/publish" || die "publish refused (see the JSON above)"
    echo
    log "published $VERSION: the car stages it at its next check and switches at the next boot"
else
    log "uploaded, not published: POST $SERVER_URL/api/carops/admin/releases/$VERSION/publish"
fi
