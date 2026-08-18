#!/usr/bin/env bash
# What chapters a file declares, checked against ffprobe.
#
#   kahawai-chapters.sh <file...>
#
# Prints one row per file: our sparse container read, what the GStreamer
# discoverer's TOC saw, and ffprobe's answer. Exits non-zero if the sparse
# read disagrees with ffprobe about the start times — that is the whole
# point of the tool. A discoverer that sees fewer is expected: measured,
# matroskademux posts no TOC at all for some files ffprobe reads fine.
set -euo pipefail

case "${1:-}" in
    -h|--help|'') grep '^#' "$0" | tail -n +2 | sed 's/^# \{0,1\}//' | head -9; exit 0 ;;
esac

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
. "$HERE/kahawai-gst-env.sh"

cargo build --release --manifest-path "$HERE/../Cargo.toml" -p kahawai-media \
    --example chapter_probe >&2
target=$(cargo metadata --format-version 1 --no-deps --manifest-path "$HERE/../Cargo.toml" |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

probed=$("$target/release/examples/chapter_probe" "$@")
rc=0
for f in "$@"; do
    ours=$(awk -F'\t' -v f="$f" '$1==f && $2=="sparse" {print $3}' <<<"$probed")
    toc=$(awk -F'\t' -v f="$f" '$1==f && $2=="discover" {print $3}' <<<"$probed")
    theirs=$(ffprobe -v quiet -show_entries chapter=start_time -of csv=p=0 "$f" |
        awk 'NF {printf "%s%d", sep, $1 * 1000 + 0.5; sep = ","}')
    starts() { tr ',' '\n' <<<"${1:-}" | cut -d: -f1 | grep . || true; }
    # A millisecond apart is the two roundings of one nanosecond value,
    # not a disagreement: we truncate, ffprobe rounds.
    verdict=$(paste <(starts "$ours") <(starts "$theirs") |
        awk 'BEGIN {v="OK"} {d=$1-$2; if (d<0) d=-d; if (NF!=2 || d>1) v="DIFF"} END {print v}')
    [ "$verdict" = OK ] || rc=1
    printf '%s\n  ours     %s\n  ffprobe  %s\n  toc      %s\n  %s\n' \
        "$f" "${ours:-(none)}" "${theirs:-(none)}" "${toc:-(none)}" "$verdict"
done
exit $rc
