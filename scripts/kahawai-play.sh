#!/usr/bin/env bash
# Play a kahawai item in mpv.
#
#   kahawai-play.sh [-r] [-a host:port] <username> <password> <item-id> [-- mpv args...]
#
#   -r            remux to HLS in the hub (default: direct play)
#   -s SECONDS    start at this offset (remux: pipeline starts there, §6)
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it instead of passing on the command line
#
# The play session is deleted when mpv exits.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
MODE="direct"

while getopts "rs:a:h" opt; do
    case $opt in
        r) MODE="remux" ;;
        s) START_MS=$((OPTARG * 1000)) ;;
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -9; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { echo "usage: $(basename "$0") [-r] [-a host:port] <username> <password> <item-id> [-- mpv args...]" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 ITEM=$3
shift 3
[ "${1:-}" = "--" ] && shift

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

json_field() { python3 -c "import json,sys;print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | json_field access_token) || { echo "login failed" >&2; exit 1; }

SESSION=$(curl -sf -X POST "http://$API/api/v1/playback/sessions" \
    -H "Authorization: Bearer $TOKEN" -H content-type:application/json \
    -d "{\"item_id\":\"$ITEM\",\"mode\":\"$MODE\",\"start_ms\":${START_MS:-0}}") \
    || { echo "session failed (bad item id, source offline, or codecs need a transcoder?)" >&2; exit 1; }

SESSION_ID=$(printf '%s' "$SESSION" | json_field session_id)
STREAM_URL=$(printf '%s' "$SESSION" | json_field stream_url)
echo "session $SESSION_ID ($MODE) → $STREAM_URL" >&2

cleanup() {
    curl -sf -X DELETE "http://$API/api/v1/playback/sessions/$SESSION_ID" \
        -H "Authorization: Bearer $TOKEN" >/dev/null || true
}
trap cleanup EXIT

mpv --http-header-fields="Authorization: Bearer $TOKEN" "$@" "http://$API$STREAM_URL"
