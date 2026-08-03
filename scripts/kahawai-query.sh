#!/usr/bin/env bash
# Ask an item what THIS client would be served: QUERY /api/v1/items/{id}
# (RFC 10008). GET answers what the scan found; QUERY answers what
# negotiation would do about it, for the capabilities you declare here.
#
#   kahawai-query.sh [-a host:port] [-c caps] [-m mode] [-j] <username> <password> <item-id>
#
#   -a host:port  API address (default: $KAHAWAI_API or localhost:8420)
#   -c caps       comma-separated capability bits, default "mp4,h264,aac,ass,overlay":
#                   mp4 webm matroska    containers the client DEMUXES
#                     (mkv = matroska; a browser sends mp4/webm only.
#                      This is not the delivery format — HLS is decided
#                      by negotiation, never declared here)
#                   h264 hevc av1 vp9    decodable video
#                   aac ac3 eac3 flac opus mp3 vorbis   decodable audio
#                   hdr                  will display HDR acceptably
#                   ass                  renders ASS itself (JASSUB)
#                   overlay              composites bitmap subtitles
#                 "-" sends no profile at all: the conservative fallback
#                 a session start would use.
#   -m mode       force a mode (direct|remux|transcode) instead of cheapest
#   -t target     what this client needs from EXT-X-TARGETDURATION —
#                 REQUIRED by the API, so the script always sends one:
#                   ignore     don't care (default; what a browser does)
#                   accurate   must be correct, any value
#                   short:N    must be correct AND <= N seconds, which
#                              forces a video encode when the source's
#                              keyframes are further apart than that
#   -j            print the raw JSON response
#   password "-"  prompt for it instead of passing on the command line
#
# Item ids come from kahawai-list.sh. Nothing here changes state: QUERY
# starts no extraction, generates nothing and claims no transcoder, so
# it is safe to run against a live hub in a loop.
set -euo pipefail

API="${KAHAWAI_API:-localhost:8420}"
CAPS="mp4,h264,aac,ass,overlay"
MODE=""
TARGET="ignore"
RAW=0

while getopts "a:c:m:t:jh" opt; do
    case $opt in
        a) API="$OPTARG" ;;
        c) CAPS="$OPTARG" ;;
        m) MODE="$OPTARG" ;;
        t) TARGET="$OPTARG" ;;
        j) RAW=1 ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -31; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

[ $# -ge 3 ] || { echo "usage: $(basename "$0") [-a host:port] [-c caps] [-m mode] [-j] <username> <password> <item-id>" >&2; exit 2; }
USERNAME=$1 PASSWORD=$2 ITEM=$3

if [ "$PASSWORD" = "-" ]; then
    read -rsp "Password for $USERNAME: " PASSWORD; echo >&2
fi

TOKEN=$(python3 -c 'import json,sys;print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$USERNAME" "$PASSWORD" \
    | curl -sf -X POST "http://$API/api/v1/auth/token" -H content-type:application/json -d @- \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["access_token"])') \
    || { echo "login failed" >&2; exit 1; }

BODY=$(python3 - "$CAPS" "$MODE" "$TARGET" <<'PY'
import json, sys

caps, mode, target = sys.argv[1], sys.argv[2], sys.argv[3]
body = {}
# Required by the API: there is no server-side default, because the
# right answer differs per client (see TargetDuration).
if target.startswith("short"):
    _, _, secs = target.partition(":")
    td = {"mode": "short", "max_secs": int(secs or 6)}
elif target in ("ignore", "accurate"):
    td = {"mode": target}
else:
    sys.exit("unknown target duration mode: %s" % target)
if mode:
    body["mode"] = mode

if caps != "-":
    bits = [b.strip() for b in caps.split(",") if b.strip()]
    known_v = {"h264", "hevc", "av1", "vp9"}
    known_a = {"aac", "ac3", "eac3", "flac", "opus", "mp3", "vorbis"}
    # The hub compares these literally against `MediaInfo.container`,
    # which the probe normalizes — so "mkv" would silently match
    # nothing. Accept it as the name people type, send what the hub
    # actually holds.
    aliases = {"mkv": "matroska"}
    bits = [aliases.get(b, b) for b in bits]
    known_c = {"mp4", "webm", "matroska"}
    unknown = [b for b in bits
               if b not in known_v | known_a | known_c | {"hdr", "ass", "overlay"}]
    if unknown:
        sys.exit("unknown capability bits: %s" % ", ".join(unknown))
    body["profile"] = {
        # Family floors, no profile/level: `cap_admits` compares those
        # only when BOTH sides state one, so a bare codec admits every
        # stream of that codec — the same thing the web client sends.
        "containers": [b for b in bits if b in known_c],
        "video": [{"codec": b} for b in bits if b in known_v],
        "audio": [b for b in bits if b in known_a],
        "hdr": "hdr" in bits,
        "ass_render": "ass" in bits,
        "graphics_overlay": "overlay" in bits,
        "target_duration": td,
    }
print(json.dumps(body))
PY
)

RESP=$(curl -sf -X QUERY "http://$API/api/v1/items/$ITEM" \
    -H "Authorization: Bearer $TOKEN" -H content-type:application/json \
    -d "$BODY") || { echo "QUERY failed (is the item id right?)" >&2; exit 1; }

if [ "$RAW" = 1 ]; then
    printf '%s\n' "$RESP" | python3 -m json.tool
    exit 0
fi

printf '%s\n' "$RESP" | python3 -c '
import json, sys

d = json.load(sys.stdin)
title = d["title"]
if d.get("show_title"):
    title = "%s · S%02dE%02d · %s" % (d["show_title"], d.get("season") or 0,
                                      d.get("episode") or 0, title)
print(title)

for s in d.get("sources", []):
    st = s.get("streams") or {}
    v = (st.get("video") or [{}])[0]
    a = (st.get("audio") or [{}])[0]
    print("  %s  %s %s%s %s  %.1f GB%s" % (
        s["path_rel"], st.get("container", "?"), v.get("codec", "?"),
        " %dp" % v["height"] if v.get("height") else "",
        a.get("codec", "?"), s["size"] / 1e9,
        "" if s.get("available") else "  (host offline)"))

n = d.get("negotiated")
if not n:
    print("\nnot negotiable: %s" % d.get("unavailable", "unknown"))
    raise SystemExit(1)

print("\nwould play: %s (%s) from %s" % (n["mode"], n["cost"], n["source"]["path_rel"]))
print("  playlist: EXT-X-TARGETDURATION %s s" % n.get("target_duration_secs", "?"))
print("  video: %s" % n["streams"]["video"])
print("  audio: %s" % n["streams"]["audio"])
for t in n["subtitles"]:
    label = t.get("label") or t.get("language") or "?"
    note = t["note"] or "-"
    print("  sub %-5s %-6s %-10s %-8s %s" % (t["id"], t["format"], label,
                                             t["delivery"], note))
'
