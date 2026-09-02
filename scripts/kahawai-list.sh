#!/usr/bin/env bash
# List kahawai items.
#
#   kahawai-list.sh [-a host:port] [-p|-n] <username> <password> [filter]
#   kahawai-list.sh [-a host:port] -l <library> -r <username> <password> [filter]
#   kahawai-list.sh [-a host:port] -l <library> -A <artist-key> <username> <password> [filter]
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   -p            meaningfully started and unfinished, most recent first
#                 (at least one minute and 1%: the "continue watching" row)
#   -n            what to watch next: one episode per series you are in,
#                 the one after the last you finished ("up next")
#   -l library    library id for artist navigation
#   -r            list the Album Artists in that music library
#   -A artist     list one Album Artist's albums, oldest first
#   password "-"  prompt for it instead of passing on the command line
#   filter        case-insensitive title substring
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
PATH_="/api/v1/items"
MODE="items"
LIBRARY=""
ARTIST=""
QUERY_PARAMS=()

while getopts "a:pnl:rA:h" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        p) QUERY_PARAMS+=(--data-urlencode "in_progress=true") ;;
        # Its own route rather than a flag on the browse: one row per
        # series is not a filter over items.
        n) PATH_="/api/v1/up-next" ;;
        l) LIBRARY="$OPTARG" ;;
        r) MODE="artists"; PATH_="/api/v1/artists" ;;
        A) MODE="albums"; ARTIST="$OPTARG" ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -16; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 2 ] || { echo "usage: $(basename "$0") [-a host:port] <username> <password> [filter]" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 FILTER="${3:-}"

if [ "$MODE" != "items" ] && [ -z "$LIBRARY" ]; then
    echo "artist navigation requires -l <library>" >&2
    exit 2
fi
if [ "$MODE" = "albums" ]; then
    ENCODED_ARTIST=$(python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.argv[1], safe=""))' "$ARTIST")
    PATH_="/api/v1/artists/$ENCODED_ARTIST/albums"
fi
if [ "$MODE" != "items" ]; then
    QUERY_PARAMS+=(--data-urlencode "library=$LIBRARY")
    [ -z "$FILTER" ] || QUERY_PARAMS+=(--data-urlencode "q=$FILTER")
fi

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"client":"api","username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

curl -sfG -H "Authorization: Bearer $TOKEN" "${QUERY_PARAMS[@]}" "http://$API$PATH_" \
    | python3 -c '
import json, sys
needle = sys.argv[1].lower()
mode = sys.argv[2]
answer = json.load(sys.stdin)
if mode == "artists":
    artists = answer["artists"]
    for artist in artists:
        art = "  [art]" if artist.get("art_version") is not None else ""
        print("%s  %s  [%d albums]%s" % (artist["key"], artist["name"], artist["album_count"], art))
    print("-- %d/%d artists" % (len(artists), answer["total"]), file=sys.stderr)
    raise SystemExit
items = answer["albums" if mode == "albums" else "items"]
shown = 0
for i in items:
    title = i["title"]
    # Artist-album filtering is server-side and also matches child tracks; a
    # second title-only filter here would hide exactly those album results.
    if mode == "items" and needle and needle not in title.lower():
        continue
    year = i["year"] or "----"
    n = i["sources"]
    srcs = " [%d sources]" % n if n != 1 else ""
    if i.get("season") is not None and i.get("episode") is not None:
        title = "%s  S%02dE%02d %s" % (i.get("parent_title") or "", i["season"], i["episode"], title)
    mark = ""
    if i.get("played"):
        mark = "  [seen]"
    elif i.get("resume_position_ms"):
        pos = i["resume_position_ms"] // 1000
        mark = "  [resume %d:%02d]" % (pos // 60, pos % 60)
    print("%s  %s (%s)%s%s" % (i["id"], title, year, srcs, mark))
    shown += 1
print("-- %d/%d %s" % (shown, answer.get("total", len(items)), "albums" if mode == "albums" else "items"), file=sys.stderr)
' "$FILTER" "$MODE"
