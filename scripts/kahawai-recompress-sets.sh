#!/usr/bin/env bash
# Recompress pre-existing display-set cache files (KBS1) to zstd-9 in
# place. New files are written compressed by the hub; this migrates the
# ones from before. Readers sniff the magic, so running this under a
# live hub is safe: the swap is an atomic rename and either form reads.
#
# Usage: kahawai-recompress-sets.sh [subtitles-dir]
set -euo pipefail

dir="${1:-$HOME/.local/share/kahawai/subtitles}"
[ -d "$dir" ] || { echo "no such dir: $dir" >&2; exit 2; }

total_before=0 total_after=0 done_n=0 skipped=0
for f in "$dir"/*.sets; do
    [ -e "$f" ] || { echo "nothing to do"; exit 0; }
    magic=$(head -c4 "$f" | od -An -tx1 | tr -d ' \n')
    if [ "$magic" = "28b52ffd" ]; then
        skipped=$((skipped + 1))
        continue
    fi
    [ "$magic" = "4b425331" ] || { echo "skipping non-KBS1 file: $f" >&2; continue; }
    before=$(stat -c%s "$f")
    tmp="$f.zst-tmp"
    zstd -9 -T0 -q -c "$f" > "$tmp"
    mv "$tmp" "$f"
    after=$(stat -c%s "$f")
    total_before=$((total_before + before))
    total_after=$((total_after + after))
    done_n=$((done_n + 1))
done
echo "recompressed $done_n files ($skipped already compressed):"
echo "  $((total_before / 1024 / 1024)) MB -> $((total_after / 1024 / 1024)) MB"
