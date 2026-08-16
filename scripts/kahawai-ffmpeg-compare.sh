#!/usr/bin/env bash
# The comparison arm the GStreamer assessment doc admits we lack: run
# BOTH pipelines' cheapest honest equivalent over the same sample and
# diff the failure sets. Measures ONE dimension — demux/remux
# robustness on the copy path, ffmpeg's most favorable arm — and the
# two "fail" notions are not symmetric (kahawai-sweep also validates
# segment DTS), so the output is a four-way diff, never a score.
#
#   kahawai-ffmpeg-compare.sh <dir> [--limit N] [--jobs N] [--seed S]
#
# Per file:
#   ours:   kahawai-sweep --one <file>        (head sweep, seconds)
#   ffmpeg: ffmpeg -t 30 -i <file> -map 0:v:0? -map 0:a:0? -c copy
#           -f hls into a tempdir; nonzero exit or stderr = fail
set -euo pipefail

DIR=""
LIMIT=150
JOBS=4
SEED=42
while [ $# -gt 0 ]; do
  case "$1" in
    --limit) LIMIT=$2; shift 2 ;;
    --jobs) JOBS=$2; shift 2 ;;
    --seed) SEED=$2; shift 2 ;;
    *) DIR=$1; shift ;;
  esac
done
[ -n "$DIR" ] || { echo "usage: $0 <dir> [--limit N] [--jobs N] [--seed S]" >&2; exit 2; }

# Our arm has to be the shipping stack, or the comparison measures
# the system plugins against ffmpeg and calls the difference ours.
. "$(dirname "$0")/kahawai-gst-env.sh"
repo=$(cd "$(dirname "$0")/.." && pwd)
SWEEP="$repo/target/release/kahawai-sweep"
[ -x "$SWEEP" ] || (cd "$repo" && cargo build --release -q -p kahawai-media --bin kahawai-sweep)

out=$(mktemp -d /tmp/kahawai-ffcmp.XXXXXX)
echo "==> results under $out" >&2

# Deterministic sample: same seed = same files on a re-run.
# shuf -n instead of |head: head's early close SIGPIPEs the pipeline
# under pipefail and set -e silently kills the whole run on any
# directory big enough to overflow the pipe buffer (found live).
find "$DIR" -type f \( -name '*.mkv' -o -name '*.mp4' -o -name '*.avi' -o -name '*.ts' -o -name '*.m2ts' -o -name '*.wmv' -o -name '*.ogm' \) \
  | sort | shuf --random-source=<(yes "$SEED") -n "$LIMIT" > "$out/files.txt"
total=$(wc -l < "$out/files.txt")
echo "==> $total files sampled from $DIR (seed $SEED)" >&2

one() {
  f=$1
  key=$(printf '%s' "$f" | md5sum | cut -d' ' -f1)
  # ours: head sweep verdict — first tab field of "TAG\tDETAIL"
  # (tags: OK, OK(head), DEGRADED, SKIP, FAIL; a crashed child = CRASH)
  row=$("$SWEEP" --one "$f" 2>/dev/null | head -1 || true)
  ours=$(printf '%s' "$row" | cut -f1)
  ours_detail=$(printf '%s' "$row" | cut -f2- | tr '\t' ' ')
  [ -n "$ours" ] || ours="CRASH"
  # ffmpeg: bounded copy-remux to HLS in a scratch dir
  scratch=$(mktemp -d)
  if err=$(timeout 120 ffmpeg -v error -nostdin -t 30 -i "$f" \
      -map '0:v:0?' -map '0:a:0?' -c copy -muxdelay 0 \
      -f hls -hls_time 3 -hls_list_size 0 "$scratch/out.m3u8" 2>&1); then
    ff="PASS"
    [ -n "$err" ] && ff="WARN"
  else
    ff="FAIL"
  fi
  rm -rf "$scratch"
  # ONE row: flatten stderr, cap the detail columns.
  err_flat=$(printf '%s' "${err:-}" | tr '\n\t' '; ' | head -c 500)
  printf '%s\t%s\t%s\t%s\t%s\n' "$ours" "$ff" "$f" "$ours_detail" "$err_flat" > "$out/$key.row"
}
export -f one
export SWEEP out
xargs -a "$out/files.txt" -d '\n' -P "$JOBS" -I{} bash -c 'one "$@"' _ {}

cat "$out"/*.row > "$out/rows.tsv"
echo
echo "== four-way diff (ours × ffmpeg) =="
awk -F'\t' '{
  o = ($1 ~ /^(OK|SKIP)/) ? "pass" : "fail"   # DEGRADED counts as fail: quality loss
  f = ($2 == "PASS" || $2 == "WARN") ? "pass" : "fail"
  n[o"/"f]++
} END { for (k in n) printf "  ours %s / ffmpeg %s: %d\n", substr(k,1,index(k,"/")-1), substr(k,index(k,"/")+1), n[k] }' "$out/rows.tsv"
echo
echo "== ours-only failures =="
awk -F'\t' '$1 !~ /^(OK|SKIP)/ && ($2=="PASS"||$2=="WARN") {print "  " $1 " :: " $3 " :: " $4}' "$out/rows.tsv" | head -20
echo "== ffmpeg-only failures =="
awk -F'\t' '$1 ~ /^(OK|SKIP)/ && $2=="FAIL" {print "  " $3 " :: " $5}' "$out/rows.tsv" | head -20
echo "== both fail =="
awk -F'\t' '$1 !~ /^(OK|SKIP)/ && $2=="FAIL" {print "  " $1 " :: " $3}' "$out/rows.tsv" | head -20
echo
echo "full rows: $out/rows.tsv"
