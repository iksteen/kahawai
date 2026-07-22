#!/usr/bin/env bash
# List kahawai items.
#
#   kahawai-list.sh [-a host:port] <username> <password> [filter]
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   password "-"  prompt for it instead of passing on the command line
#   filter        case-insensitive title substring
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"

while getopts "a:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -7; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 2 ] || { echo "usage: $(basename "$0") [-a host:port] <username> <password> [filter]" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 FILTER="${3:-}"

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

curl -sf -H "Authorization: Bearer $TOKEN" "http://$API/api/v1/items" \
    | python3 -c '
import json, sys
needle = sys.argv[1].lower()
items = json.load(sys.stdin)["items"]
shown = 0
for i in items:
    title = i["title"]
    if needle and needle not in title.lower():
        continue
    year = i["year"] or "----"
    n = i["sources"]
    srcs = " [%d sources]" % n if n != 1 else ""
    mark = ""
    if i.get("played"):
        mark = "  [seen]"
    elif i.get("resume_position_ms"):
        pos = i["resume_position_ms"] // 1000
        mark = "  [resume %d:%02d]" % (pos // 60, pos % 60)
    print("%s  %s (%s)%s%s" % (i["id"], title, year, srcs, mark))
    shown += 1
print("-- %d/%d items" % (shown, len(items)), file=sys.stderr)
' "$FILTER"
