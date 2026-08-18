#!/usr/bin/env bash
# Measure audio/video sync in a transcoded session's own segments.
#
#   kahawai-avsync.sh [-a host:port] [-n segments] <username> <password> <item-id>
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   -n N          how many segments to measure (default 20)
#   -c            measure the COPY path: offer every passthrough codec, so
#                 the session remuxes instead of transcoding (a laced-Opus
#                 title shipped one packet in eight, ratio 0.125, and only
#                 this mode's segments carry that failure)
#   password "-"  prompt for it instead of passing on the command line
#
# Starts a transcoded session, waits for segments, and compares — per
# stream — how much CONTENT the packets carry against how much TIMELINE
# they span. Both ratios must be ~1.0. Anything else means the muxed
# stream itself is wrong, whatever the player then does with it.
#
# This exists because a DD+ (E-AC-3) title shipped 60 s of audio stamped
# across 30 s of timeline — sound racing the picture 2:1 — and every
# test in the workspace stayed green. ac3parse split each container
# block into core + extension substream and interpolated a timestamp
# between the halves; nothing downstream could tell. A ratio is the
# smallest thing that fails when that happens again.
#
# Exit codes: 0 both streams within tolerance, 1 out of tolerance.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
SEGMENTS=20

COPY=""
while getopts "a:n:ch" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        n) SEGMENTS="$OPTARG" ;;
        c) COPY=1 ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -12; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { echo "usage: $(basename "$0") [-a host:port] [-n segments] <username> <password> <item-id>" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 ITEM=$3

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

command -v ffprobe >/dev/null || { echo "ffprobe not found" >&2; exit 2; }

TOKEN=$(curl -fsS -X POST "http://$API/api/v1/auth/token" \
    -H 'content-type: application/json' \
    -d "{\"client\":\"api\",\"username\":\"$USERNAME\",\"password\":\"$PASSWORD\"}" |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')

# Default: a profile that forces a transcode (no passthrough codecs
# offered). -c: the opposite — offer everything, so copy wins and the
# measurement covers the remux path instead.
if [ -n "$COPY" ]; then
    PROFILE='{"containers":["mp4"],"video":[{"codec":"av1"},{"codec":"hevc"},{"codec":"h264"},{"codec":"vp9"}],"audio":["opus","aac","ac3","eac3","flac","mp3"],"hdr":true,"graphics_overlay":false,"ass_render":false,"target_duration":{"mode":"ignore"}}'
else
    PROFILE='{"containers":["mp4"],"video":[{"codec":"h264"}],"audio":["aac"],"hdr":false,"graphics_overlay":false,"ass_render":false,"target_duration":{"mode":"ignore"}}'
fi
SESSION=$(curl -fsS -X POST "http://$API/api/v1/playback/sessions" \
    -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d "{\"item_id\":\"$ITEM\",\"profile\":$PROFILE}")
SID=$(echo "$SESSION" | python3 -c 'import json,sys; print(json.load(sys.stdin)["session_id"])')
echo "session $SID"

cleanup() {
    curl -fsS -X DELETE -H "Authorization: Bearer $TOKEN" \
        "http://$API/api/v1/playback/sessions/$SID" -o /dev/null 2>/dev/null || true
}
trap cleanup EXIT

# Segments land in the executing box's session dir. Only a session run
# by THIS machine is measurable here; a dispatched one is not.
DIR=""
for _ in $(seq 60); do
    # `|| true`: until the run dir exists find exits non-zero, and with
    # pipefail that status would propagate into the assignment and kill
    # the script under `set -e` — silently, before anything is printed.
    DIR=$(find "$HOME/.local/share"/kahawai*/sessions/"$SID" \
          \( -name 'segment00000.ts' -o -name 'segment00000.m4s' \) \
          -exec dirname {} \; 2>/dev/null | head -1 || true)
    # if/then, not an && chain: a failing AND-list as the last command
    # of a loop body trips `set -e` and exits 1 with nothing printed.
    if [ -n "$DIR" ] && [ "$(find "$DIR" -name 'segment*.ts' -o -name 'segment*.m4s' | wc -l)" -ge "$SEGMENTS" ]; then
        break
    fi
    sleep 2
done
[ -n "$DIR" ] || { echo "no segments appeared (dispatched to another box?)" >&2; exit 2; }

# One joined file: TS segments concatenate by design, and fMP4 segments
# only probe at all with their init in front. Unit-independent seconds
# from ffprobe, so the same arithmetic serves both.
JOINED=$(mktemp --suffix=.probe)
trap 'rm -f "$JOINED"; cleanup' EXIT
if compgen -G "$DIR/segment*.m4s" >/dev/null; then
    cat "$DIR/init.mp4" $(ls "$DIR"/segment*.m4s | sort | head -n "$SEGMENTS") > "$JOINED"
else
    cat $(ls "$DIR"/segment*.ts | sort | head -n "$SEGMENTS") > "$JOINED"
fi

python3 - "$JOINED" <<'PY'
import subprocess, sys

f = sys.argv[1]

def packets(stream):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", stream,
         "-show_entries", "packet=pts_time,duration_time", "-of", "csv=p=0", f],
        capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        cells = [c for c in line.split(',') if c]
        if len(cells) == 2 and 'N/A' not in cells:
            rows.append((float(cells[0]), float(cells[1])))
    return rows

v = packets("v:0")
a = packets("a:0")
if not v or not a:
    print("missing a stream — nothing to compare"); sys.exit(2)

bad = False
for name, rows in (("video", v), ("audio", a)):
    # CONTENT is what the packets carry; TIMELINE is what they span.
    # Both from ffprobe's own seconds, so any codec and either segment
    # format measures the same way — the E-AC-3 2:1 race and the laced
    # Opus one-in-eight both fail this without knowing why.
    content = sum(d for _, d in rows)
    span = max(t for t, _ in rows) - min(t for t, _ in rows)
    ratio = span / content if content else 0.0
    flag = ""
    # Span omits the final packet's own duration, so short measurements
    # sit a little under 1.0 by construction; 10% is well clear of that
    # and of any real cadence error, which halves or doubles.
    if not 0.90 <= ratio <= 1.10:
        flag, bad = "  <-- OUT OF TOLERANCE", True
    print(f"{name:6s}: {len(rows):5d} packets, content {content:7.2f}s, "
          f"timeline {span:7.2f}s, ratio {ratio:.3f}{flag}")

drift = (max(t for t, _ in a) - min(t for t, _ in a)) - (max(t for t, _ in v) - min(t for t, _ in v))
print(f"\naudio-vs-video timeline drift over the measured span: {drift*1000:+.0f} ms")
sys.exit(1 if bad else 0)
PY
