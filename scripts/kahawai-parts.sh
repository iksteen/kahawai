#!/usr/bin/env bash
# Verify the multi-part (CD1/CD2) hand-off against a live hub.
#
#   kahawai-parts.sh [-a host:port] <username> <password> <item-id>
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it instead of passing on the command line
#   item-id       a movie whose source is split across parts
#
# When a part's playlist ends the player seeks to `end + 250 ms` and the
# hub restarts the pipeline in the next file. That decision is server
# side, so it can be checked without a browser: seek to the last
# millisecond of part one and to the hand-off target, and see which part
# the hub answers with. Exits non-zero if the hand-off would not happen.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"

while getopts "a:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -8; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { echo "usage: $(basename "$0") [-a host:port] <username> <password> <item-id>" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 ITEM=$3

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"client":"api","username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

auth=(-H "Authorization: Bearer $TOKEN" -H content-type:application/json)

# Multi-part sources are refused in direct mode, so ask for remux.
session=$(curl -sf "${auth[@]}" -X POST "http://$API/api/v1/playback/sessions" \
    -d "{\"item_id\":\"$ITEM\",\"mode\":\"remux\"}")
read -r SID PARTS DURATION <<<"$(python3 -c '
import json, sys
s = json.load(sys.stdin)
print(s["session_id"], s.get("parts", 1), s.get("duration_ms") or 0)
' <<<"$session")"

cleanup() { curl -sf -X DELETE "${auth[@]}" "http://$API/api/v1/playback/sessions/$SID" >/dev/null || true; }
trap cleanup EXIT

echo "session $SID: $PARTS part(s), duration ${DURATION}ms"
[ "$PARTS" -gt 1 ] || { echo "FAIL: $ITEM is not a multi-part source" >&2; exit 1; }
# The player gates the hand-off on a known duration; without one it never
# fires, however correct the seek would have been.
[ "$DURATION" -gt 0 ] || { echo "FAIL: no duration_ms — the player cannot hand off" >&2; exit 1; }

seek() {  # position_ms -> part_base_ms
    curl -sf "${auth[@]}" -X POST "http://$API/api/v1/playback/sessions/$SID/seek" \
        -d "{\"position_ms\":$1}" \
        | python3 -c 'import json,sys;print(json.load(sys.stdin)["part_base_ms"])'
}

# Find where part two begins by asking the hub about the far half.
BASE2=$(seek $((DURATION * 3 / 4)))
[ "$BASE2" -gt 0 ] || { echo "FAIL: no part boundary found in the second half" >&2; exit 1; }
echo "part two begins at ${BASE2}ms"

LAST1=$(seek $((BASE2 - 1)))
HANDOFF=$(seek $((BASE2 + 250)))
echo "  last ms of part one -> part_base $LAST1"
echo "  hand-off (+250ms)   -> part_base $HANDOFF"

rc=0
[ "$LAST1" -eq 0 ] || { echo "FAIL: the ms before the boundary is not in part one" >&2; rc=1; }
[ "$HANDOFF" -eq "$BASE2" ] || { echo "FAIL: the hand-off target is not in part two" >&2; rc=1; }
[ $rc -eq 0 ] && echo "OK: CD1->CD2 hand-off lands in part two"
exit $rc
