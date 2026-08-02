#!/usr/bin/env bash
# NFR-1: measure what a viewer waits for, against the live fleet.
#
#   kahawai-latency.sh [-n runs] [-c sessions] [-a host:port] <username> <password> [item-id...]
#
#   -n RUNS       repeats per case (default 5)
#   -c SESSIONS   concurrent direct-play sessions for the load case (default 100)
#   -u USERS      spread the load case over this many THROWAWAY accounts,
#                 created and deleted around the run. The per-user cap
#                 ([hub] max_per_user, default 4) is per ACCOUNT, so a
#                 hub-capacity figure needs either enough accounts or a
#                 raised cap — one account cannot show it.
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   item-id...    cases to measure; defaults to the three torture titles
#                 configured below when they resolve on this hub
#
# START LATENCY is timed from the session POST to the first byte a
# PLAYER can consume — the range response's first byte for direct play,
# `segment00000.ts` becoming fetchable for a transcode. Not to the API
# answering: a session id nobody can play yet is not a started session.
#
# The WORST run is the verdict and every run is printed. A best-of-N hid
# a 5x bimodality here for a day (benchmarks lie by omission, not by
# arithmetic).
#
# This loads the real fleet: each transcode case runs a real session on a
# real satellite, and the concurrent case saturates the byte plane
# between hub and mediahost. Run it when nobody is watching anything.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
RUNS=5
CONCURRENT=100
USERS=0

# The acceptance budgets (NFR-1). Exceeding one is the finding, not an
# error: the script always reports every number it took.
BUDGET_DIRECT_MS=2000
BUDGET_TRANSCODE_MS=6000

while getopts "n:c:u:a:h" opt; do
    case $opt in
        n) RUNS="$OPTARG" ;;
        c) CONCURRENT="$OPTARG" ;;
        u) USERS="$OPTARG" ;;
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -28; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 2 ] || { echo "usage: $(basename "$0") [-n runs] [-c sessions] <username> <password> [item-id...]" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2
shift 2

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

json_field() { python3 -c "import json,sys;print(json.load(sys.stdin).get(sys.argv[1],''))" "$1"; }

login() {
    python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' \
        "$USERNAME" "$PASSWORD" \
        | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
        | json_field access_token
}
TOKEN=$(login) || { echo "login failed" >&2; exit 1; }

# The three shapes worth timing, and why each is here:
#   the home-grown torture file — local mediahost, small, the floor
#   Allegiant  — 12 GB, 4K-class scope, DTS: the heaviest decode
#   The Truman Show — HDR10, E-AC-3: the tone-map path
DEFAULT_ITEMS=(
    "01KYR0QZVT4WGQ6H5ZK56BZ956"
    "01KY5X2CMT1BFJT206RWAJ9BPN"
    "01KYYXCAHGEE7QAWR76929MCPS"
)
ITEMS=("$@")
[ ${#ITEMS[@]} -gt 0 ] || ITEMS=("${DEFAULT_ITEMS[@]}")

# A client that accepts everything these files hold, so the hub has no
# reason to encode: this is the direct-play case.
PROFILE_PERMISSIVE='{"containers":["matroska","mp4","webm"],
  "video":[{"codec":"hevc"},{"codec":"h264"},{"codec":"av1"},{"codec":"vp9"}],
  "audio":["dts","eac3","ac3","aac","flac","opus","truehd"],"hdr":true,
  "graphics_overlay":true,"ass_render":true}'
# A client that accepts none of it: forces the full encode, tone-map
# included, which is the six-second budget's real subject.
PROFILE_STRICT='{"containers":["ts"],"video":[{"codec":"h264"}],"audio":["aac"],"hdr":false,
  "graphics_overlay":false,"ass_render":false}'

now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

title_of() {
    curl -sf "http://$API/api/v1/items/$1" -H "Authorization: Bearer $TOKEN" \
        | json_field title 2>/dev/null || echo "$1"
}

end_session() {
    [ -n "${1:-}" ] || return 0
    curl -sf -X DELETE "http://$API/api/v1/playback/sessions/$1" \
        -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
}

# One timed start. Echoes "<mode> <elapsed_ms>", or "<mode> -1" when the
# session never became playable inside the timeout.
start_once() {
    local item=$1 profile=$2 t0 resp sid mode stream elapsed deadline
    t0=$(now_ms)
    resp=$(python3 -c 'import json,sys;print(json.dumps({"item_id":sys.argv[1],"profile":json.loads(sys.argv[2])}))' \
             "$item" "$profile" \
           | curl -sf -X POST "http://$API/api/v1/playback/sessions" \
               -H "Authorization: Bearer $TOKEN" -H content-type:application/json -d @-) \
        || { echo "error -1"; return; }
    sid=$(printf '%s' "$resp" | json_field session_id)
    mode=$(printf '%s' "$resp" | json_field mode)
    stream=$(printf '%s' "$resp" | json_field stream_url)

    if [ "$mode" = "direct" ]; then
        # First byte a player would pull. Range, because that is what a
        # <video> element opens with.
        if curl -sf -o /dev/null -r 0-65535 "http://$API$stream" \
                -H "Authorization: Bearer $TOKEN"; then
            elapsed=$(( $(now_ms) - t0 ))
        else
            elapsed=-1
        fi
    else
        # Playable = the first segment can be fetched. The playlist
        # appearing is not enough; it lists segments before they exist.
        local seg="http://$API/api/v1/playback/sessions/$sid/segment00000.ts"
        elapsed=-1
        deadline=$(( $(now_ms) + 60000 ))
        while [ "$(now_ms)" -lt "$deadline" ]; do
            if curl -sf -o /dev/null "$seg" -H "Authorization: Bearer $TOKEN" 2>/dev/null; then
                elapsed=$(( $(now_ms) - t0 ))
                break
            fi
            sleep 0.05
        done
    fi
    end_session "$sid"
    echo "$mode $elapsed"
}

report() {
    local label=$1 budget=$2; shift 2
    python3 - "$label" "$budget" "$@" <<'PY'
import sys
label, budget, *vals = sys.argv[1:]
nums = [int(v) for v in vals]
bad = [n for n in nums if n < 0]
ok = [n for n in nums if n >= 0]
runs = " ".join(str(n) for n in nums)
if not ok:
    print(f"{label:<44} FAILED every run  [{runs}]")
    sys.exit(0)
worst = max(ok)
verdict = "ok " if worst <= int(budget) and not bad else "OVER"
note = f"  ({len(bad)} never started)" if bad else ""
print(f"{label:<44} worst {worst:6d} ms  budget {budget:>5} ms  {verdict}  [{runs}]{note}")
PY
}

echo "== NFR-1 start latency (worst of $RUNS, ms)"
for item in "${ITEMS[@]}"; do
    title=$(title_of "$item")
    for mode in direct transcode; do
        [ "$mode" = direct ] && profile=$PROFILE_PERMISSIVE || profile=$PROFILE_STRICT
        [ "$mode" = direct ] && budget=$BUDGET_DIRECT_MS || budget=$BUDGET_TRANSCODE_MS
        # ~15 minute tokens, and a full sweep outlives one.
        TOKEN=$(login)
        vals=() got=""
        for _ in $(seq "$RUNS"); do
            out=$(start_once "$item" "$profile")
            got=${out% *}
            vals+=("${out#* }")
        done
        report "$title [$mode → $got]" "$budget" "${vals[@]}"
    done
done

echo
echo "== NFR-1 concurrency: $CONCURRENT direct-play sessions at once"
CONC_ITEM=${ITEMS[0]}
ADMIN=$(login)
TOKEN=$ADMIN
tmp=$(mktemp -d)

# Throwaway accounts, if asked for. Deleted in the EXIT trap so a
# failed run does not leave them behind — the reason `--fix`-style
# cleanup is a trap and not a line at the bottom.
USER_IDS=()
USER_TOKENS=()
cleanup_users() {
    for uid in "${USER_IDS[@]:-}"; do
        [ -n "$uid" ] && curl -sf -X DELETE "http://$API/admin/v1/users/$uid" \
            -H "Authorization: Bearer $ADMIN" >/dev/null 2>&1 || true
    done
}
trap 'rm -rf "$tmp"; cleanup_users' EXIT

if [ "$USERS" -gt 0 ]; then
    pw="loadtest-$$-pw"
    for u in $(seq "$USERS"); do
        name="loadtest-$$-$u"
        uid=$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2],"admin":False}))' "$name" "$pw" \
              | curl -sf -X POST "http://$API/admin/v1/users" \
                  -H "Authorization: Bearer $ADMIN" -H content-type:application/json -d @- \
              | json_field id) || { echo "creating $name failed" >&2; exit 1; }
        USER_IDS+=("$uid")
        USER_TOKENS+=("$(USERNAME=$name PASSWORD=$pw; python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$name" "$pw" \
              | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
              | json_field access_token)")
    done
    echo "   ($USERS throwaway accounts, deleted at exit)"
fi

for i in $(seq "$CONCURRENT"); do
    if [ "$USERS" -gt 0 ]; then
        TOKEN=${USER_TOKENS[$(( (i - 1) % USERS ))]}
    fi
    ( TOKEN=$TOKEN start_once "$CONC_ITEM" "$PROFILE_PERMISSIVE" > "$tmp/$i" 2>/dev/null ) &
done
wait
TOKEN=$ADMIN
mapfile -t conc < <(awk '{print $2}' "$tmp"/* 2>/dev/null)
report "$(title_of "$CONC_ITEM") [x$CONCURRENT direct]" "$BUDGET_DIRECT_MS" "${conc[@]}"
