#!/usr/bin/env bash
# Verify source-aware AAC-in-TS layout selection and the exact loudness
# matrix used for 5.1 side-to-rear conversion. Does not restart services.
# Run on Linux and on the Mac satellite against its patched GStreamer keg.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$(uname -s)" = Darwin ]; then
    keg=/opt/homebrew/Cellar/kahawai-gstreamer/1.28.8
    export PKG_CONFIG_PATH="$keg/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    [ "$(pkg-config --variable=prefix gstreamer-1.0)" = "$keg" ] || {
        echo "GStreamer must come from $keg" >&2
        exit 1
    }
else
    . scripts/kahawai-gst-env.sh
fi
# A machine without the needed codecs must fail rather than silently skip.
export KAHAWAI_MEDIA_TEST_STRICT=1
cargo test -p kahawai-media side_surround -- --nocapture
cargo test -p kahawai-media aac_layout -- --nocapture
if [ "$(uname -s)" != Darwin ]; then
    # These older video fixtures require FDK, which the Mac does not ship.
    # The portable checks above exercise libav, ceilings and 7.1 there.
    cargo test -p kahawai-media seven_one_to_five_one -- --nocapture
    cargo test -p kahawai-media channel_ceiling_downmixes -- --nocapture
    cargo test -p kahawai-media preserved_multichannel_encode -- --nocapture
fi
