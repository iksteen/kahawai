#!/usr/bin/env bash
# NFR-2: stand up N mediahosts against this hub and check it holds them.
#
#   kahawai-fanout.sh [-n hosts] [-a host:port] <admin-user> <admin-password>
#
#   -n HOSTS      how many to enroll (default 10, the NFR-2 target)
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#
# WHAT THIS PROVES, and it is worth being exact. Ten processes on one
# box share a disk, a kernel and a page cache, so this says nothing
# about ten real hosts' I/O — that is not claimed and cannot be claimed
# here (NFR-2 amendment, 2026-08-02). What it does exercise is every
# per-module thing on the HUB: ten identities in the mTLS allowlist,
# ten control streams with heartbeats, ten announced collections
# reconciling independently, and whether any of it degrades or caps as
# the count grows. That is the whole of "without architectural change",
# and it is the part that can break silently.
#
# Each host gets its own state dir — the enrolled identity lives there,
# and sharing one would make every process the same satellite — and its
# own tiny generated collection, so the hub sees distinct content
# rather than one file deduplicated ten ways.
#
# Everything is torn down on exit: processes killed, satellites deleted
# through the admin API (which cascades their files and items), scratch
# removed. A failed run cleans up too, which is why it is a trap.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
HOSTS=10
ROOT="${TMPDIR:-/tmp}/kahawai-fanout.$$"

while getopts "n:a:h" opt; do
    case $opt in
        n) HOSTS="$OPTARG" ;;
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -24; exit 0 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -ge 2 ] || { echo "usage: $(basename "$0") [-n hosts] <admin-user> <admin-password>" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2

# The mediahosts this stands up probe media in-process, same as the
# service does, so they need the same plugins the service gets.
. "$(dirname "$0")/kahawai-gst-env.sh"

BIN=$(cd "$(dirname "$0")/.." && pwd)/target/release/kahawai
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }

json_field() { python3 -c "import json,sys;print(json.load(sys.stdin).get(sys.argv[1],''))" "$1"; }
login() {
    python3 -c 'import json,sys;print(json.dumps({"client":"api","username":sys.argv[1],"password":sys.argv[2]}))' \
        "$USERNAME" "$PASSWORD" \
        | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
        | json_field access_token
}
TOKEN=$(login) || { echo "login failed" >&2; exit 1; }

PIDS=() ; MODULE_IDS=()
# Bracketed on purpose: an unbracketed -f pattern also matches the shell
# running it, which then kills itself and reports success (house rule,
# hook-enforced).
FANOUT_PAT="[k]ahawai --config $ROOT"
alive() { pgrep -cf "$FANOUT_PAT" 2>/dev/null || true; }

cleanup() {
    echo "== teardown"
    # VERIFY they died. A plain kill left two of ten running once, and a
    # survivor is not harmless: its satellite row is about to be deleted,
    # so it sits re-enrolling against a hub that no longer knows it.
    # Same rule as kahawai-restart.sh — never assume a kill worked.
    for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
    for _ in $(seq 20); do
        [ "$(alive)" -eq 0 ] && break
        sleep 0.5
    done
    if [ "$(alive)" -ne 0 ]; then
        echo "   $(alive) ignored SIGTERM; sending KILL" >&2
        pkill -KILL -f "$FANOUT_PAT" || true
        sleep 1
    fi
    [ "$(alive)" -eq 0 ] || echo "   WARNING: $(alive) fanout processes still alive" >&2
    TOKEN=$(login) || true
    for mid in "${MODULE_IDS[@]:-}"; do
        [ -n "$mid" ] && curl -sf -X DELETE "http://$API/admin/v1/satellites/$mid" \
            -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
    done
    rm -rf "$ROOT"
    echo "   $HOSTS hosts stopped, satellites deleted, scratch removed"
}
trap cleanup EXIT

echo "== standing up $HOSTS mediahosts"
mkdir -p "$ROOT"
for i in $(seq "$HOSTS"); do
    media="$ROOT/h$i/media"; state="$ROOT/h$i/state"
    mkdir -p "$media" "$state"
    # Distinct content per host: the frame count varies, so size and
    # hash differ and the hub sees ten collections rather than one file
    # deduplicated ten ways (HUB-3 would be right to merge those).
    for f in 1 2; do
        gst-launch-1.0 -q videotestsrc num-buffers=$((10 + i * 3 + f)) \
            ! video/x-raw,width=320,height=240 ! x264enc ! matroskamux \
            ! filesink location="$media/Fanout $i-$f (2026).mkv" >/dev/null 2>&1 || true
    done
    cat > "$ROOT/h$i/kahawai.toml" <<EOF
[mediahost]
hub = "${API%:*}:8421"
name = "fanout-$i"
state_dir = "$state"
rescan_minutes = 0

[[mediahost.collections]]
name = "fanout-$i"
media_type = "movies"
roots = ["$media"]
EOF
    "$BIN" --config "$ROOT/h$i/kahawai.toml" mediahost > "$ROOT/h$i/log" 2>&1 &
    PIDS+=("$!")
done

echo "== approving enrollments"
for i in $(seq "$HOSTS"); do
    code=""
    for _ in $(seq 60); do
        code=$(grep -oE '[A-Z0-9]{4}-[A-Z0-9]{4}' "$ROOT/h$i/log" 2>/dev/null | head -1 || true)
        [ -n "$code" ] && break
        sleep 1
    done
    [ -n "$code" ] || { echo "host $i never printed an enrollment code" >&2; exit 1; }
    TOKEN=$(login)
    python3 -c 'import json,sys;print(json.dumps({"code":sys.argv[1]}))' "$code" \
        | curl -sf -X POST "http://$API/admin/v1/enrollments/approve" \
            -H "Authorization: Bearer $TOKEN" -H content-type:application/json -d @- >/dev/null \
        || { echo "approving host $i ($code) failed" >&2; exit 1; }
done

echo "== waiting for links"
for _ in $(seq 90); do
    TOKEN=$(login)
    n=$(curl -sf "http://$API/admin/v1/satellites" -H "Authorization: Bearer $TOKEN" \
        | python3 -c 'import json,sys
d=json.load(sys.stdin).get("satellites",[])
print(sum(1 for s in d if s["module_type"]=="mediahost" and s["connected"] and s["name"].startswith("fanout-")))')
    [ "$n" -ge "$HOSTS" ] && break
    sleep 2
done
echo "   connected: $n/$HOSTS"

# Record the ids so teardown removes exactly these and nothing else.
TOKEN=$(login)
mapfile -t MODULE_IDS < <(curl -sf "http://$API/admin/v1/satellites" -H "Authorization: Bearer $TOKEN" \
    | python3 -c 'import json,sys
for s in json.load(sys.stdin).get("satellites",[]):
    if s["name"].startswith("fanout-"): print(s["module_id"])')

echo "== what the hub sees"
TOKEN=$(login)
curl -sf "http://$API/admin/v1/satellites" -H "Authorization: Bearer $TOKEN" > "$ROOT/sats.json"
python3 "$(dirname "$0")/kahawai-fanout-report.py" "$HOSTS" "$ROOT/sats.json"

# Browse must stay answerable with every link up: this is where a
# per-module cost the hub pays on every request would show.
TOKEN=$(login)
start=$(date +%s%N)
items=$(curl -sf "http://$API/api/v1/items?limit=1" -H "Authorization: Bearer $TOKEN" | json_field total)
end=$(date +%s%N)
echo "   browse with $HOSTS hosts up  : $(( (end - start) / 1000000 )) ms (items visible: ${items:-?})"
