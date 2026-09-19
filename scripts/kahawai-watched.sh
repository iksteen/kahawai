#!/usr/bin/env bash
# Mark a kahawai item watched or unwatched, without playing it.
#
#   kahawai-watched.sh [-a host:port] [-u] -l <library-id> <username> <password> <item-id>...
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   -u            mark UNwatched instead
#   -l library-id catalogue library containing the items
#   password "-"  prompt for it instead of passing on the command line
#   item-id       stable mediadb movie/episode/track IDs
#
# Either direction clears the resume position. There is no seen counter.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
PLAYED=true
LIBRARY=""

while getopts "a:l:uh" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        u) PLAYED=false ;;
        l) LIBRARY="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -12; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] && [ -n "$LIBRARY" ] || { echo "usage: $(basename "$0") [-a host:port] [-u] -l <library-id> <username> <password> <item-id>..." >&2; exit 2; }
USERNAME=$1 PASSWORD=$2
shift 2

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"client":"api","username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

# One request per id, and a failing id fails the script: a partial sweep
# that exits 0 is the kind of success nobody checks. Checked before the
# pipe, so a refusal reads as a refusal instead of as a JSON parse error
# on the empty body curl leaves behind.
for ID in "$@"; do
    if ! OUT=$(curl -sS -f -X PUT "http://$API/api/v1/catalogue/libraries/$LIBRARY/items/$ID/watched" \
        -H "Authorization: Bearer $TOKEN" \
        -H content-type:application/json \
        -d "{\"played\":$PLAYED}" 2>/dev/null); then
        echo "$ID: refused — no such item, or not yours to see" >&2
        exit 1
    fi
    printf '%s' "$OUT" | python3 -c '
import json, sys
r = json.load(sys.stdin)["updated"][0]
print("%s  played=%s" % (sys.argv[1], str(r["played"]).lower()))
' "$ID"
done
