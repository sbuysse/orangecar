#!/usr/bin/env bash
# Install the Android Auto head-unit identity for rascar-aa into /etc/rascar/aa.
#
# The phone only accepts a head unit whose TLS client certificate is signed by
# Google's "Google Automotive Link" CA. Every open-source head unit uses the
# JVC Kenwood certificate + key published by opencardev/aasdk (cert/), valid
# until 2045. It is NOT committed to this repo: this script fetches it at a
# pinned commit and checks it by SHA-256. Personal, non-commercial use only.
#
# Usage: sudo ./aa-headunit-cert.sh [path/to/aasdk/cert]   (default: download)
set -euo pipefail

COMMIT=9bf6adf933665dee26532201719fac14a047ccf1
BASE="https://raw.githubusercontent.com/opencardev/aasdk/$COMMIT/cert"
CRT_SHA=85b5043a09b1ba9464f745e6917bdbaa2bc582fe48cb727a7787c1a832a773e4
KEY_SHA=9e837a172a1eef5b05cda9df9d086753639648a17f76c2c939bcbd094aa972f2
DEST=/etc/rascar/aa
OWNER=${RASCAR_USER:-${SUDO_USER:-$(id -un)}}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [[ $# -ge 1 ]]; then
    cp "$1/headunit.crt" "$1/headunit.key" "$tmp/"
else
    curl -fsSL "$BASE/headunit.crt" -o "$tmp/headunit.crt"
    curl -fsSL "$BASE/headunit.key" -o "$tmp/headunit.key"
fi

echo "$CRT_SHA  $tmp/headunit.crt" | sha256sum -c - || { echo "certificate hash mismatch" >&2; exit 1; }
echo "$KEY_SHA  $tmp/headunit.key" | sha256sum -c - || { echo "key hash mismatch" >&2; exit 1; }
if [[ "$(openssl x509 -in "$tmp/headunit.crt" -noout -modulus)" != "$(openssl rsa -in "$tmp/headunit.key" -noout -modulus 2>/dev/null)" ]]; then
    echo "key does not match certificate" >&2
    exit 1
fi

install -d -m 0750 -o root -g "$OWNER" "$DEST"
install -m 0644 -o root -g root "$tmp/headunit.crt" "$DEST/headunit.crt"
install -m 0640 -o root -g "$OWNER" "$tmp/headunit.key" "$DEST/headunit.key"
openssl x509 -in "$DEST/headunit.crt" -noout -subject -enddate
echo "installed in $DEST"
