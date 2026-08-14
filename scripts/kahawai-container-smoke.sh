#!/usr/bin/env bash
# Play something through the container image, end to end.
#
#   kahawai-container-smoke.sh [-t TAG] [-b] [-k]
#
#   -t TAG   image to test (default: kahawai)
#   -b       docker build the image first
#   -k       keep the container and its data dir on the way out
#
# Starts all-in-one in the image against a clip the image generates
# itself, creates the admin, waits for the scan, starts a playback
# session and asserts that segments come out of it.
#
# It exists because `doctor` passing is not playback passing. The image
# shipped with a worker guard that read "my parent is pid 1" as "my
# supervisor died" — true on a normal box, and false in a container,
# where the hub IS pid 1 because it is the ENTRYPOINT. Every session
# failed at once with "supervisor already gone; not starting", and
# nothing caught it: the plugins were there, the linkage was right,
# doctor was green. Only a session reaches that code.
set -uo pipefail

TAG=kahawai
BUILD=""
KEEP=""
while getopts "t:bkh" opt; do
    case $opt in
        t) TAG="$OPTARG" ;;
        b) BUILD=1 ;;
        k) KEEP=1 ;;
        h|*) grep '^#' "$0" | sed 's/^# \{0,1\}//' | head -10; exit 0 ;;
    esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)
name="kahawai-smoke-$$"
work=$(mktemp -d -t kahawai-smoke-XXXXXX)
user="$(id -u):$(id -g)"
# Failures print the reason the log already knows, when it knows one.
# The guard this script was written for reports itself in a line the
# session response never carries, so a bare "no session" would send the
# next reader looking in the wrong place.
fail() {
    echo "SMOKE FAIL: $*" >&2
    if docker logs "$name" 2>&1 | grep -q 'supervisor already gone'; then
        echo "  cause: the worker guard refused to start — the hub is pid 1" \
             "in a container, which older builds read as a dead supervisor" >&2
    fi
    exit 1
}

cleanup() {
    if [ -n "$KEEP" ]; then
        echo "kept: container $name, data $work" >&2
        return
    fi
    docker rm -f "$name" >/dev/null 2>&1
    rm -rf "$work"
}
trap cleanup EXIT

if [ -n "$BUILD" ]; then
    echo "==> building $TAG" >&2
    docker build -t "$TAG" "$repo" >/dev/null || fail "docker build"
fi

mkdir -p "$work/media/movies" "$work/state"
cat > "$work/kahawai.toml" <<'TOML'
[hub]
bind = "0.0.0.0:8420"
satellite_bind = "0.0.0.0:8421"
data_dir = "/data/state"
hostnames = ["localhost"]

[mediahost]
hub = "localhost:8421"
name = "smoke"
rescan_minutes = 0

[[mediahost.collections]]
name = "movies"
media_type = "movies"
roots = ["/data/media/movies"]
TOML

# The clip comes out of the image's own GStreamer, so this needs nothing
# installed here and exercises that install on the way past.
echo "==> generating a clip with the image's GStreamer" >&2
docker run --rm --user "$user" -v "$work:/data" --entrypoint sh "$TAG" -c \
    'gst-launch-1.0 -q \
        videotestsrc num-buffers=240 ! video/x-raw,width=320,height=240,framerate=24/1 ! \
        x264enc key-int-max=48 ! h264parse ! matroskamux name=m ! \
        filesink location=/data/media/movies/Smoke.Test.2026.mkv \
        audiotestsrc num-buffers=430 ! audioconvert ! avenc_aac ! m.' \
    >/dev/null 2>&1
[ -s "$work/media/movies/Smoke.Test.2026.mkv" ] || fail "the image could not mux a test clip"

echo "==> starting all-in-one from $TAG" >&2
docker run -d --name "$name" --user "$user" -p 0:8420 -p 127.0.0.1::8422 -v "$work:/data" \
    "$TAG" all-in-one --config /data/kahawai.toml >/dev/null 2>&1 \
    || fail "docker run"
api=$(docker port "$name" 8420/tcp | head -1)
api="localhost:${api##*:}"
setup=$(docker port "$name" 8422/tcp | head -1)
setup="localhost:${setup##*:}"

logs() { docker logs "$name" 2>&1 | sed 's/\x1b\[[0-9;]*m//g'; }
# `d` is the parsed body; the expression prints what it wants from it.
# Errors are NOT silenced: a response that changed shape should say so
# rather than read as "not ready yet" until the loop times out.
py() { python3 -c "import json,sys; d=json.load(sys.stdin); $1"; }
# Items come back as a bare list or under "items", depending on version.
items='it = d if isinstance(d, list) else d["items"];'

for _ in $(seq 30); do
    curl -sf "http://$setup/api/v1/bootstrap" >/dev/null && break
    sleep 2
done

curl -sf -X POST "http://$setup/api/v1/setup" \
    -H "Origin: http://$setup" -H content-type:application/json \
    -d '{"username":"smoke","password":"smoke-password-1"}' >/dev/null \
    || { logs | tail -20 >&2; fail "local setup failed"; }
auth=$(curl -sf -X POST "http://$api/api/v1/auth/token" -H content-type:application/json \
    -d '{"username":"smoke","password":"smoke-password-1"}' \
    | py 'print(d["access_token"])')
[ -n "$auth" ] || fail "login after setup did not return a token"

echo "==> waiting for the scan" >&2
for _ in $(seq 30); do
    body=$(curl -sf -H "Authorization: Bearer $auth" "http://$api/api/v1/items")
    n=$(printf '%s' "$body" | py "$items print(len(it))" 2>/dev/null)
    [ "${n:-0}" -gt 0 ] && break
    sleep 3
done
[ "${n:-0}" -gt 0 ] || fail "the mediahost never announced the clip"
item=$(printf '%s' "$body" | py "$items print(it[0]['id'])")

echo "==> starting a session" >&2
session=$(curl -sf -X POST "http://$api/api/v1/playback/sessions" \
    -H "Authorization: Bearer $auth" -H content-type:application/json \
    -d "{\"item_id\":\"$item\",\"start_ms\":0,\"mode\":\"remux\"}")
url=$(printf '%s' "$session" | py 'print(d["stream_url"])')
[ -n "$url" ] || { logs | tail -20 >&2; fail "no session: $session"; }

# The point of the whole script: segments, produced by a worker the hub
# actually managed to spawn.
for _ in $(seq 30); do
    playlist=$(curl -sf -H "Authorization: Bearer $auth" "http://$api$url")
    segs=$(printf '%s\n' "$playlist" | grep -cE '\.(ts|m4s)$')
    [ "${segs:-0}" -ge 2 ] && break
    sleep 2
done
if [ "${segs:-0}" -lt 2 ]; then
    logs | grep -iE 'worker|supervisor|error' | tail -10 >&2
    fail "session produced ${segs:-0} segments"
fi

first=$(printf '%s\n' "$playlist" | grep -m1 -E '\.(ts|m4s)$')
bytes=$(curl -sf -H "Authorization: Bearer $auth" "http://$api${url%/*}/$first" | wc -c)
[ "${bytes:-0}" -gt 10000 ] || fail "first segment served $bytes bytes"

if logs | grep -q 'supervisor already gone'; then
    fail "the worker guard refused to start inside the container"
fi

echo "SMOKE OK: $TAG played $item — $segs segments, first is $bytes bytes" >&2
