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
[mediahost]
hub = "localhost:8421"
name = "smoke"
rescan_minutes = 0

[[mediahost.collections]]
name = "movies"
media_type = "movies"
roots = ["/data/media/movies"]

[hub]
bind = "0.0.0.0:8420"
satellite_bind = "0.0.0.0:8421"
data_dir = "/data/state"
hostnames = ["localhost"]
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
# Docker may assign a different ephemeral host port on `docker restart`.
# Reserve one explicitly so the canonical Origin remains true across both
# exact-container restarts exercised below.
host_port=$(python3 -c \
    'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
docker run -d --name "$name" --user "$user" -p "127.0.0.1:$host_port:8420" \
    -v "$work:/data" "$TAG" all-in-one --config /data/kahawai.toml >/dev/null 2>&1 \
    || fail "docker run"
api=$(docker port "$name" 8420/tcp | head -1)
api="localhost:${api##*:}"

logs() { docker logs "$name" 2>&1 | sed 's/\x1b\[[0-9;]*m//g'; }

# Discover the random published port before choosing the canonical browser
# origin, then restart this exact container so the bind-mounted config is read.
printf 'public_url = "http://localhost:%s"\n' "${api##*:}" >> "$work/kahawai.toml"
container_id=$(docker inspect -f '{{.Id}}' "$name")
before_pid=$(docker inspect -f '{{.State.Pid}}' "$name")
docker restart "$name" >/dev/null || fail "first exact-container restart"
[ "$(docker inspect -f '{{.Id}}' "$name")" = "$container_id" ] \
    || fail "first restart replaced the container"
[ "$(docker inspect -f '{{.State.Pid}}' "$name")" != "$before_pid" ] \
    || fail "first restart did not replace the hub process"
# Every wait that spans a hub start gets a full minute. Loading the config
# now initializes GStreamer and probes decoder ranks before the hub logs
# anything about its own configuration, so this waits out a START, not a log
# flush -- seconds on an idle box, and this runs on a shared runner.
for _ in $(seq 120); do
    logs 2>/dev/null | grep -q \
        'browser authentication cookies and tokens cross the network in cleartext' && break
    sleep 0.5
done
logs | grep -q 'browser authentication cookies and tokens cross the network in cleartext' \
    || fail "configured HTTP public_url warning was not logged"
for _ in $(seq 120); do
    curl -sf "http://$api/health" >/dev/null && break
    sleep 0.5
done
curl -sf "http://$api/health" >/dev/null || fail "public API did not return after first restart"
# `d` is the parsed body; the expression prints what it wants from it.
# Errors are NOT silenced: a response that changed shape should say so
# rather than read as "not ready yet" until the loop times out.
py() { python3 -c "import json,sys; d=json.load(sys.stdin); $1"; }
# Items come back as a bare list or under "items", depending on version.
items='it = d if isinstance(d, list) else d["items"];'

# `setup_bind` is loopback inside the container. Publishing 8422 would DNAT
# to the container's bridge address and cannot reach a listener on its own
# 127.0.0.1, so make this request from inside the network namespace. The
# runtime image has bash; /dev/tcp avoids adding curl solely for this check.
setup_ready=""
for _ in $(seq 30); do
    setup_probe=$(docker exec "$name" bash -c '
        exec 3<>/dev/tcp/127.0.0.1/8422
        printf "GET /api/v1/bootstrap HTTP/1.1\r\nHost: localhost:8422\r\nConnection: close\r\n\r\n" >&3
        cat <&3
    ' 2>/dev/null || true)
    case "$setup_probe" in HTTP/1.1\ 200*) setup_ready=1; break ;; esac
    sleep 2
done
[ -n "$setup_ready" ] \
    || { logs | tail -20 >&2; fail "the local setup control plane never opened"; }
setup_modes=$(docker exec "$name" sh -c '
    stat -c "%a %n" "$KAHAWAI_HUB__DATA_DIR/control" \
        "$KAHAWAI_HUB__DATA_DIR/control/bootstrap.sock"
') || fail "could not inspect setup control permissions"
printf '%s\n' "$setup_modes" | grep -q '^700 .*/control$' \
    || fail "setup control directory is not mode 0700: $setup_modes"
printf '%s\n' "$setup_modes" | grep -q '^600 .*/control/bootstrap.sock$' \
    || fail "bootstrap socket is not mode 0600: $setup_modes"
# Exercise the operator path exactly as documented. `init-admin` deliberately
# reads passwords from a terminal, so give docker exec a real PTY and answer
# each prompt only after it appears (feeding all input early can echo secrets).
setup_output=$(python3 - "$name" <<'PY'
import errno
import os
import pty
import select
import signal
import sys
import time

container = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(
        "docker",
        ["docker", "exec", "-it", container, "kahawai", "hub", "init-admin"],
    )

answers = [
    (b"Admin username: ", b"smoke\n"),
    (b"Admin password: ", b"smoke-password-1\n"),
    (b"Confirm password: ", b"smoke-password-1\n"),
]
output = bytearray()
next_answer = 0
deadline = time.monotonic() + 30
status = None
while status is None:
    if time.monotonic() >= deadline:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        sys.stderr.write("init-admin timed out\n")
        sys.exit(1)
    readable, _, _ = select.select([fd], [], [], 0.1)
    if readable:
        try:
            chunk = os.read(fd, 4096)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            chunk = b""
        output.extend(chunk)
        if next_answer < len(answers) and answers[next_answer][0] in output:
            os.write(fd, answers[next_answer][1])
            next_answer += 1
    waited, wait_status = os.waitpid(pid, os.WNOHANG)
    if waited == pid:
        status = wait_status

sys.stdout.buffer.write(output)
sys.exit(os.waitstatus_to_exitcode(status))
PY
) || { printf '%s\n' "$setup_output" >&2; logs | tail -20 >&2; fail "init-admin failed"; }
printf '%s' "$setup_output" | grep -q 'initial administrator created' \
    || { printf '%s\n' "$setup_output" >&2; fail "init-admin did not report success"; }
case "$setup_output" in
    *smoke-password-1*) fail "init-admin echoed the password" ;;
esac
for _ in $(seq 30); do
    docker exec "$name" sh -c 'test ! -e "$KAHAWAI_HUB__DATA_DIR/control"' \
        >/dev/null 2>&1 && break
    sleep 0.1
done
docker exec "$name" sh -c 'test ! -e "$KAHAWAI_HUB__DATA_DIR/control"' \
    >/dev/null 2>&1 || fail "setup control directory remained after init-admin"
auth=$(curl -sf -X POST "http://$api/api/v1/auth/token" -H content-type:application/json \
    -d '{"client":"api","username":"smoke","password":"smoke-password-1"}' \
    | py 'print(d["access_token"])')
[ -n "$auth" ] || fail "login after setup did not return a token"

echo "==> exercising API and browser authentication" >&2
"$repo/scripts/kahawai-auth-cycle.sh" -a "$api" smoke smoke-password-1 \
    || fail "expanded authentication cycle"
browser_jar="$work/browser.cookies"
browser_login=$(curl -sf -c "$browser_jar" -X POST \
    "http://$api/api/v1/auth/token" -H content-type:application/json \
    -H "Origin: http://$api" \
    -d '{"client":"browser","username":"smoke","password":"smoke-password-1"}')
printf '%s' "$browser_login" \
    | py 'assert set(d) == {"access_token", "expires_in"}; print(d["access_token"])' \
    >/dev/null || fail "browser login exposed the wrong response shape"

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
    playlist=$(curl -sf -b "$browser_jar" "http://$api$url")
    segs=$(printf '%s\n' "$playlist" | grep -cE '\.(ts|m4s)$')
    [ "${segs:-0}" -ge 2 ] && break
    sleep 2
done
if [ "${segs:-0}" -lt 2 ]; then
    logs | grep -iE 'worker|supervisor|error' | tail -10 >&2
    fail "session produced ${segs:-0} segments"
fi

first=$(printf '%s\n' "$playlist" | grep -m1 -E '\.(ts|m4s)$')
bytes=$(curl -sf -b "$browser_jar" "http://$api${url%/*}/$first" | wc -c)
[ "${bytes:-0}" -gt 10000 ] || fail "first segment served $bytes bytes"

# The refresh family and its HttpOnly cookie survive a real hub restart.
before_pid=$(docker inspect -f '{{.State.Pid}}' "$name")
docker restart "$name" >/dev/null || fail "second exact-container restart"
[ "$(docker inspect -f '{{.Id}}' "$name")" = "$container_id" ] \
    || fail "second restart replaced the container"
[ "$(docker inspect -f '{{.State.Pid}}' "$name")" != "$before_pid" ] \
    || fail "second restart did not replace the hub process"
for _ in $(seq 120); do
    curl -sf "http://$api/health" >/dev/null && break
    sleep 0.5
done
curl -sf "http://$api/health" >/dev/null || fail "hub did not return after second restart"
browser_refresh=$(curl -sf -b "$browser_jar" -c "$browser_jar" -X POST \
    "http://$api/api/v1/auth/refresh" \
    -H content-type:application/json -H "Origin: http://$api" \
    -d '{"client":"browser"}') || fail "browser refresh cookie did not survive restart"
printf '%s' "$browser_refresh" \
    | py 'assert set(d) == {"access_token", "expires_in"}; print(d["access_token"])' \
    >/dev/null || fail "post-restart browser refresh returned the wrong shape"

python3 - "$work/kahawai/hub.db" "$repo/crates/kahawai-hub/migrations" <<'PYDB' \
    || fail "live authentication database postconditions"
import pathlib
import sqlite3
import sys

db_path, migrations = sys.argv[1:3]
db = sqlite3.connect(db_path)
applied = db.execute(
    "SELECT max(version) FROM _sqlx_migrations WHERE success = 1"
).fetchone()[0]
expected = max(int(path.name.split("_", 1)[0]) for path in pathlib.Path(migrations).glob("*.sql"))
assert applied == expected, (applied, expected)
families, active, revoked = db.execute(
    "SELECT count(*), count(*) FILTER (WHERE revoked_at IS NULL), "
    "count(*) FILTER (WHERE revoked_at IS NOT NULL) FROM refresh_families"
).fetchone()
assert (families, active, revoked) == (6, 2, 4), (families, active, revoked)
assert db.execute(
    "SELECT count(*) FROM users WHERE password_hash NOT LIKE '$argon2%'"
).fetchone()[0] == 0
assert db.execute(
    "SELECT count(*) FROM refresh_families "
    "WHERE length(current_token_hash) != 64 "
    "OR current_token_hash GLOB '*[^0-9a-f]*' "
    "OR current_token_hash LIKE 'v1.%'"
).fetchone()[0] == 0
print("live auth database is current, bounded and hashed")
PYDB

if logs | grep -q 'supervisor already gone'; then
    fail "the worker guard refused to start inside the container"
fi

echo "SMOKE OK: $TAG played $item — $segs segments, first is $bytes bytes" >&2
