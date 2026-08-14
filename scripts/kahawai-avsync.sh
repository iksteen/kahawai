#!/usr/bin/env bash
# Measure audio/video sync in a transcoded session's own segments.
#
#   kahawai-avsync.sh [-a host:port] [-n segments] <username> <password> <item-id>
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   -n N          how many segments to measure (default 20)
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

while getopts "a:n:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        n) SEGMENTS="$OPTARG" ;;
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

# A profile that forces a transcode: no passthrough codecs offered.
SESSION=$(curl -fsS -X POST "http://$API/api/v1/playback/sessions" \
    -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d "{\"item_id\":\"$ITEM\",\"profile\":{\"containers\":[\"mp4\"],
         \"video\":[{\"codec\":\"h264\"}],\"audio\":[\"aac\"],\"hdr\":false}}")
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
    DIR=$(find "$HOME/.local/share"/kahawai*/sessions/"$SID" -name 'segment00000.ts' \
          -exec dirname {} \; 2>/dev/null | head -1 || true)
    # if/then, not an && chain: a failing AND-list as the last command
    # of a loop body trips `set -e` and exits 1 with nothing printed.
    if [ -n "$DIR" ] && [ "$(find "$DIR" -name 'segment*.ts' | wc -l)" -ge "$SEGMENTS" ]; then
        break
    fi
    sleep 2
done
[ -n "$DIR" ] || { echo "no segments appeared (dispatched to another box?)" >&2; exit 2; }

python3 - "$DIR" "$SEGMENTS" <<'PY'
import subprocess, sys, glob, os

d, want = sys.argv[1], int(sys.argv[2])
segs = sorted(glob.glob(os.path.join(d, "segment*.ts")))[:want]

def packets(f, stream, fields="pts"):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", stream,
         "-show_entries", f"packet={fields}", "-of", "csv=p=0", f],
        capture_output=True, text=True).stdout.split()
    return [x.rstrip(',') for x in out if x.rstrip(',')]

def rate(f, stream, entry):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", stream,
         "-show_entries", f"stream={entry}", "-of", "csv=p=0", f],
        capture_output=True, text=True).stdout.strip().rstrip(',')
    return out.split(',')[0]

vpts, apts = [], []
for s in segs:
    vpts += [int(x) for x in packets(s, "v:0")]
    apts += [int(x) for x in packets(s, "a:0")]

if not vpts or not apts:
    print("missing a stream — nothing to compare"); sys.exit(2)

fps = rate(segs[0], "v:0", "r_frame_rate")
num, den = (int(x) for x in fps.split('/'))
srate = int(rate(segs[0], "a:0", "sample_rate"))
# AAC-LC: 1024 samples per frame. The muxed cadence is what we check.
vcontent = len(vpts) * den / num
acontent = len(apts) * 1024 / srate
vspan = (max(vpts) - min(vpts)) / 90000
aspan = (max(apts) - min(apts)) / 90000

bad = False
for name, n, content, span in (("video", len(vpts), vcontent, vspan),
                               ("audio", len(apts), acontent, aspan)):
    ratio = span / content if content else 0.0
    flag = ""
    # Span omits the final packet's own duration, so short measurements
    # sit a little under 1.0 by construction; 10% is well clear of that
    # and of any real cadence error, which halves or doubles.
    if not 0.90 <= ratio <= 1.10:
        flag, bad = "  <-- OUT OF TOLERANCE", True
    print(f"{name:6s}: {n:5d} packets, content {content:7.2f}s, "
          f"timeline {span:7.2f}s, ratio {ratio:.3f}{flag}")

drift = aspan - vspan
print(f"\naudio-vs-video timeline drift over the measured span: {drift*1000:+.0f} ms")
sys.exit(1 if bad else 0)
PY
