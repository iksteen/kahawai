#!/usr/bin/env bash
# Find the intro and end credits of a season.
#
#   kahawai-intro.sh [--json] [--anime] [--no-refine] <season-dir|file...>
#   kahawai-intro.sh --fingerprint --window 0:60 <file>
#
# Builds first, then runs against the staged GStreamer — the plugins that
# ship, not the system ones (see kahawai-gst-env.sh).
set -euo pipefail

case "${1:-}" in
    -h|--help) grep '^#' "$0" | tail -n +2 | sed 's/^# \{0,1\}//' | head -6; exit 0 ;;
esac

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
. "$HERE/kahawai-gst-env.sh"

cargo build --release --manifest-path "$HERE/../Cargo.toml" -p kahawai --bin kahawai >&2
target=$(cargo metadata --format-version 1 --no-deps --manifest-path "$HERE/../Cargo.toml" |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

exec "$target/release/kahawai" intro "$@"
