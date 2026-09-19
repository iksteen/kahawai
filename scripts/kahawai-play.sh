#!/usr/bin/env bash
# Play a kahawai item in mpv.
#
#   kahawai-play.sh -l library-id [-r|-D] [-P profile.json] [-a host:port] <username> <password> <item-id> [-- mpv args...]
#
#   default       the hub NEGOTIATES the mode (HUB-14)
#   -l LIBRARY    mediadb library containing the item (required)
#   -e ENTRY      pin a stable mediadb rendition ID
#   -r            force remux to HLS in the hub
#   -D            force direct play
#   -P FILE       send this CapabilityProfile JSON with the request
#   -s SECONDS    start at this offset (remux: pipeline starts there, §6)
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it instead of passing on the command line
#
# The play session is deleted when mpv exits.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
MODE=""
PROFILE_FILE=""
LIBRARY=""
ENTRY=""

while getopts "l:e:rDP:s:a:h" opt; do
    case $opt in
        l) LIBRARY="$OPTARG" ;;
        e) ENTRY="$OPTARG" ;;
        r) MODE="remux" ;;
        D) MODE="direct" ;;
        P) PROFILE_FILE="$OPTARG" ;;
        s) START_MS=$((OPTARG * 1000)) ;;
        a) API="$OPTARG" ;;
        h|*) sed -n '2,/^set /{ /^#/s/^# \{0,1\}//p; }' "$0"; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { echo "usage: $(basename "$0") -l library [-r] [-a host:port] <username> <password> <item-id> [-- mpv args...]" >&2; exit 2; }
[ -n "$LIBRARY" ] || { echo "-l library-id is required" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 ITEM=$3
shift 3
[ "${1:-}" = "--" ] && shift

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

json_field() { python3 -c "import json,sys;print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"client":"api","username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | json_field access_token) || { echo "login failed" >&2; exit 1; }

BODY=$(python3 - "$ITEM" "${START_MS:-}" "$MODE" "$PROFILE_FILE" "$LIBRARY" "$ENTRY" <<'PYBODY'
import json, sys
item, start_ms, mode, profile_file, library, entry = sys.argv[1:7]
body = {"item_id": item,"library_id": library,"resume": not bool(start_ms)}
if entry:
    body["media_entry_id"] = entry
if start_ms:
    body["start_ms"] = int(start_ms)
if mode:
    body["mode"] = mode
if profile_file:
    body["profile"] = json.load(open(profile_file))
print(json.dumps(body))
PYBODY
)
SESSION=$(curl -sf -X POST "http://$API/api/v1/playback/sessions" \
    -H "Authorization: Bearer $TOKEN" -H content-type:application/json \
    -d "$BODY") \
    || { echo "session failed (bad item id, source offline, or codecs need a transcoder?)" >&2; exit 1; }

SESSION_ID=$(printf '%s' "$SESSION" | json_field session_id)
STREAM_URL=$(printf '%s' "$SESSION" | json_field stream_url)
GOT_MODE=$(printf '%s' "$SESSION" | json_field mode)
echo "session $SESSION_ID ($GOT_MODE) → $STREAM_URL" >&2

cleanup() {
    curl -sf -X DELETE "http://$API/api/v1/playback/sessions/$SESSION_ID" \
        -H "Authorization: Bearer $TOKEN" >/dev/null || true
}
trap cleanup EXIT

MPV_START=()
if [ "$GOT_MODE" = direct ]; then
    OFFSET=$(printf '%s' "$SESSION" | python3 -c 'import json,sys;s=json.load(sys.stdin);print(max(0,s["effective_start_ms"]-s["part_base_ms"])/1000)')
    MPV_START=("--start=$OFFSET")
fi
mpv "${MPV_START[@]}" --http-header-fields="Authorization: Bearer $TOKEN" "$@" "http://$API$STREAM_URL"
