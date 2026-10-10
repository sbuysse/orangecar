#!/usr/bin/env bash
set -euo pipefail

# RasCar head unit installer — prepares the developer laptop to run
# `rascar-headunit`, the AA protocol conformance prober.
# Run once. Requires sudo for package installation and udev rules.
#
# Usage:
#   ./install.sh                  # deps + udev + build rascar-headunit
#   ./install.sh --with-openauto  # additionally build f1xpl aasdk + openauto
#
# OpenAuto is NOT built by default: nothing in headunit/ links against aasdk or
# openauto. It is only useful as an independent visual cross-check, and building
# it pulls unpinned upstream master plus a large C++ toolchain.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/.build"
AASDK_COMMIT="master"     # TODO: pin to a known-good commit after first successful build
OPENAUTO_COMMIT="master"  # TODO: pin to a known-good commit after first successful build

WITH_OPENAUTO=0
for arg in "$@"; do
    case "$arg" in
        --with-openauto) WITH_OPENAUTO=1 ;;
        -h|--help)
            sed -n '4,14p' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        *)
            echo "ERROR: unknown option '$arg' (try --help)" >&2
            exit 1
            ;;
    esac
done

detect_pkg_mgr() {
    if command -v apt-get &>/dev/null; then echo "apt"
    elif command -v dnf &>/dev/null; then echo "dnf"
    else echo "unknown"; fi
}

# Runtime dependencies of rascar-headunit itself:
#   ffmpeg   — display.rs spawns it to decode the H.264 stream
#   paplay   — main.rs spawns it to play the PCM audio stream
#   X11 / Wayland client libs — minifb opens the video window through them
#   libusb   — nusb talks to the gadget through it
install_deps() {
    local pm
    pm=$(detect_pkg_mgr)
    echo "==> Installing runtime dependencies (pkg manager: $pm)"
    case $pm in
        apt)
            sudo apt-get update -q
            sudo apt-get install -y \
                ffmpeg \
                pulseaudio-utils \
                libusb-1.0-0 \
                libx11-6 libxkbcommon0 \
                libwayland-client0 libwayland-cursor0
            ;;
        dnf)
            # NOTE: on Fedora the full `ffmpeg` lives in RPM Fusion, not the base
            # repos. Either enable RPM Fusion (https://rpmfusion.org/Configuration)
            # so `ffmpeg` resolves, or install the base-repo `ffmpeg-free` build.
            sudo dnf install -y \
                ffmpeg \
                pulseaudio-utils \
                libusb1 \
                libX11 libxkbcommon \
                libwayland-client libwayland-cursor
            ;;
        *)
            echo "ERROR: unsupported package manager. Install dependencies manually."
            echo "Required: ffmpeg, paplay (pulseaudio-utils or pipewire-pulse), libusb,"
            echo "          libX11 + libxkbcommon and/or wayland client+cursor libs"
            exit 1
            ;;
    esac
}

install_openauto_deps() {
    local pm
    pm=$(detect_pkg_mgr)
    echo "==> Installing OpenAuto build dependencies (pkg manager: $pm)"
    case $pm in
        apt)
            sudo apt-get install -y \
                cmake build-essential \
                libusb-1.0-0-dev libssl-dev \
                libboost-all-dev \
                libprotobuf-dev protobuf-compiler \
                libsdl2-dev libsdl2-ttf-dev \
                libgstreamer1.0-dev \
                gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
                gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly \
                gstreamer1.0-libav
            ;;
        dnf)
            sudo dnf install -y \
                cmake gcc-c++ \
                libusb1-devel openssl-devel \
                boost-devel \
                protobuf-devel \
                SDL2-devel SDL2_ttf-devel \
                gstreamer1-devel \
                gstreamer1-plugins-base gstreamer1-plugins-good \
                gstreamer1-plugins-bad-free
            ;;
        *)
            echo "ERROR: unsupported package manager for --with-openauto."
            exit 1
            ;;
    esac
}

clone_and_build_openauto() {
    mkdir -p "$BUILD_DIR"
    cd "$BUILD_DIR"

    # aasdk
    if [ ! -d aasdk ]; then
        git clone https://github.com/f1xpl/aasdk.git
    fi
    cd aasdk
    git fetch origin
    # Build the fetched tip, not a stale local checkout that never fast-forwards.
    # For a branch ref (e.g. "master") reset to origin/<ref>; a pinned SHA has no
    # origin/<sha>, so fall back to checking out the SHA directly.
    git checkout "$AASDK_COMMIT"
    git reset --hard "origin/$AASDK_COMMIT" 2>/dev/null || true
    cmake -DCMAKE_BUILD_TYPE=Release -B build .
    cmake --build build -j"$(nproc)"
    sudo cmake --install build
    cd ..

    # openauto
    if [ ! -d openauto ]; then
        git clone https://github.com/f1xpl/openauto.git
    fi
    cd openauto
    git fetch origin
    git checkout "$OPENAUTO_COMMIT"
    git reset --hard "origin/$OPENAUTO_COMMIT" 2>/dev/null || true
    cmake -DCMAKE_BUILD_TYPE=Release -B build .
    cmake --build build -j"$(nproc)"
    sudo cmake --install build
    cd ..

    echo "==> OpenAuto installed"
}

build_headunit() {
    echo "==> Building rascar-headunit"
    if ! command -v cargo &>/dev/null; then
        echo "ERROR: cargo not found. Install Rust (https://rustup.rs) and re-run." >&2
        exit 1
    fi
    # Build only this crate: the sibling bridge/app crates target the board and
    # are not needed to run the prober.
    (cd "$REPO_ROOT" && cargo build --release -p rascar-headunit)
    echo "==> Built $REPO_ROOT/target/release/rascar-headunit"
}

install_udev_rules() {
    echo "==> Installing udev rules for RasCar USB gadget"
    sudo tee /etc/udev/rules.d/50-rascar-headunit.rules > /dev/null <<'EOF'
# Pre-accessory mode (AOA2 init — Pi starts here)
SUBSYSTEM=="usb", ATTRS{idVendor}=="18d1", ATTRS{idProduct}=="4ee1", GROUP="plugdev", MODE="0660"
# Accessory mode (after START_ACCESSORY — bulk I/O starts here)
SUBSYSTEM=="usb", ATTRS{idVendor}=="18d1", ATTRS{idProduct}=="2d00", GROUP="plugdev", MODE="0660"
EOF
    sudo udevadm control --reload-rules
    sudo udevadm trigger
    echo "==> udev rules installed"
}

add_to_plugdev() {
    local user
    user=$(logname 2>/dev/null || echo "$USER")
    # Ensure the plugdev group exists (Fedora and some distros don't create it
    # by default) before adding the user — usermod -aG would fail otherwise.
    if ! getent group plugdev >/dev/null; then
        sudo groupadd plugdev
        echo "==> Created plugdev group"
    fi
    # Exact-match the group name (substring grep would also match e.g. "plugdevx").
    if ! id -nG "$user" | tr ' ' '\n' | grep -qx plugdev; then
        sudo usermod -aG plugdev "$user"
        echo "==> Added $user to plugdev group (re-login required for effect)"
    else
        echo "==> $user already in plugdev"
    fi
}

echo "=== RasCar Head Unit Installer ==="
install_deps
if [ "$WITH_OPENAUTO" -eq 1 ]; then
    install_openauto_deps
    clone_and_build_openauto
fi
build_headunit
install_udev_rules
add_to_plugdev

echo ""
echo "=== Done ==="
echo "Usage:"
echo "  1. Connect Pi to laptop via USB"
echo "  2. On Pi (SSH): sudo cargo run --bin rascar-bridge"
echo "  3. On laptop: ./target/release/rascar-headunit"
if [ "$WITH_OPENAUTO" -eq 1 ]; then
    echo "  4. On laptop (independent visual cross-check): openauto"
fi
