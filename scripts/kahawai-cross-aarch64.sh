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
set -euo pipefail

SYSROOT="${SYSROOT:-$HOME/sysroots/pi3-aarch64}"
TARGET=aarch64-unknown-linux-gnu
[ -d "$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig" ] || {
    echo "no sysroot at $SYSROOT (see header for the rsync recipe)" >&2; exit 1; }

export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SYSROOT"
export PKG_CONFIG_LIBDIR="$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig:$SYSROOT/usr/share/pkgconfig"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$SYSROOT -C link-arg=-Wl,-rpath-link,$SYSROOT/usr/lib/aarch64-linux-gnu"
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export CFLAGS_aarch64_unknown_linux_gnu="--sysroot=$SYSROOT"

cargo build --release --target $TARGET
BIN="target/$TARGET/release/kahawai"
file "$BIN" | sed 's/, BuildID.*//'

if [ "${1:-}" = "--ship" ]; then
    scp "$BIN" "$2:kahawai.new"
    ssh "$2" 'mv kahawai.new kahawai && chmod +x kahawai && ./kahawai doctor | tail -5'
fi
