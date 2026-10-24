#!/usr/bin/env bash
# Cross-compile the kahawai binary for aarch64-linux (NFR-5), linking
# against a target sysroot rsync'd from the device — so glibc and
# GStreamer versions match the device exactly.
#
#   SYSROOT=~/sysroots/pi3-aarch64 scripts/kahawai-cross-aarch64.sh [--ship user@host]
#
# Sysroot refresh (run when the device's packages change):
#   rsync -az --copy-unsafe-links user@host:/usr/include  $SYSROOT/usr/
#   rsync -az --copy-unsafe-links user@host:/usr/lib/aarch64-linux-gnu $SYSROOT/usr/lib/
#   rsync -az --copy-unsafe-links user@host:/usr/share/pkgconfig $SYSROOT/usr/share/
#   rsync -az --copy-unsafe-links user@host:/opt/kahawai-gstreamer $SYSROOT/opt/
# Install the patched kahawai-gstreamer stack on the target first. Headers
# and runtime plugins must share its ABI; stock GStreamer is not compatible.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
SYSROOT="${SYSROOT:-$HOME/sysroots/pi3-aarch64}"
TARGET=aarch64-unknown-linux-gnu
[ -d "$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig" ] || {
    echo "no sysroot at $SYSROOT (see header for the rsync recipe)" >&2; exit 1; }
SYSROOT=$(cd "$SYSROOT" && pwd)
gst="$SYSROOT/opt/kahawai-gstreamer"
[ -f "$gst/lib/pkgconfig/gstreamer-1.0.pc" ] || {
    echo "no patched GStreamer in $gst (see header for the rsync recipe)" >&2; exit 1; }

export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_PATH=""
export PKG_CONFIG_LIBDIR="$gst/lib/pkgconfig:$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig:$SYSROOT/usr/share/pkgconfig"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$SYSROOT -C link-arg=-Wl,-rpath-link,$SYSROOT/usr/lib/aarch64-linux-gnu -C link-arg=-Wl,-rpath-link,$gst/lib -C link-arg=-Wl,--disable-new-dtags,-rpath,/opt/kahawai-gstreamer/lib"
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export CFLAGS_aarch64_unknown_linux_gnu="--sysroot=$SYSROOT"

npm ci --prefix web
npm run --prefix web build
KAHAWAI_SKIP_WEB_BUILD=1 KAHAWAI_REQUIRE_WEB=1 cargo build --locked --release --target "$TARGET"
BIN="target/$TARGET/release/kahawai"
file "$BIN" | sed 's/, BuildID.*//'

if [ "${1:-}" = "--ship" ]; then
    scp "$BIN" "$2:kahawai.new"
    ssh "$2" 'mv kahawai.new kahawai && chmod +x kahawai && ./kahawai doctor | tail -5'
fi
