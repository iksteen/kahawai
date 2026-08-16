#!/usr/bin/env bash
# Sweep a media directory through discovery + the real remux pipeline and
# report a verdict per file. Run before releases and after codec changes.
#
#   kahawai-sweep.sh <dir> [--full] [--limit N] [--jobs N]
#
# Verdicts: OK / OK(head) / DEGRADED (stream needs a transcoder, dropped) /
# SKIP (nothing TS-muxable) / FAIL. Exit 1 if anything FAILs.
set -euo pipefail
# The verdicts are only about the shipping stack if they were produced by
# it. Without this the sweep grades the system plugins.
. "$(dirname "$0")/kahawai-gst-env.sh"
cd "$(dirname "$0")/.."
exec cargo run --release -q -p kahawai-media --bin kahawai-sweep -- "$@"
