#!/usr/bin/env bash
# Deploy the satellite binaries to silence (the NAS): build
# kahawai-mediahost and kahawai-transcoder on THIS box (both are Arch
# x86_64 — no cross toolchain, no build load on a J5005), ship them,
# restart the orphaned processes, and wait for both links.
#
# No feature flags: each satellite is its own package and has no hub in
# its dependency graph, so it cannot pick up SQLite, axum or Tesseract
# however it is built.
#
# Usage: kahawai-silence.sh [user@host] [--mediahost-only]
set -euo pipefail

HOST="${1:-ingmar@192.168.0.109}"
mediahost_only=false
mode=deploy
for arg in "${@:2}"; do
    case "$arg" in
        --mediahost-only) mediahost_only=true ;;
        --stage) mode=stage ;;
        --activate) mode=activate ;;
        *) echo "usage: $0 [user@host] [--mediahost-only] [--stage|--activate]" >&2; exit 2 ;;
    esac
done
repo=$(cd "$(dirname "$0")/.." && pwd)

if [[ "$mode" != activate ]]; then
echo "==> building lean satellite binaries" >&2
(cd "$repo" && cargo build --release -p kahawai-mediahostd)
if ! $mediahost_only; then
    (cd "$repo" && cargo build --release -p kahawai-transcoderd)
fi

gst_dir="$HOME/.local/lib/kahawai-gst"
if [[ ! -d "$gst_dir/plugins" || ! -d "$gst_dir/lib" ]]; then
    echo "error: staged GStreamer missing at $gst_dir" >&2
    exit 1
fi

ssh "$HOST" 'mkdir -p ~/.kahawai-stage'
echo "==> shipping binaries and staged GStreamer to $HOST" >&2
scp -q "$repo/target/release/kahawai-mediahost" "$HOST:~/.kahawai-stage/"
if ! $mediahost_only; then
    scp -q "$repo/target/release/kahawai-transcoder" "$HOST:~/.kahawai-stage/"
    rsync -a "$gst_dir/" "$HOST:~/.kahawai-stage/gst/"
fi

fi
[[ "$mode" != stage ]] || { echo "staged; services unchanged"; exit 0; }
# Verify both candidates exist before stopping either old process.
ssh "$HOST" bash -s -- "$mediahost_only" <<'REMOTE'
set -euo pipefail
test -x ~/.kahawai-stage/kahawai-mediahost
if ! "$1"; then test -x ~/.kahawai-stage/kahawai-transcoder; test -d ~/.kahawai-stage/gst/plugins; fi
REMOTE

# Stop FIRST: scp into a running executable fails with ETXTBSY.
echo "==> stopping satellites on $HOST" >&2
ssh "$HOST" bash -s -- "$mediahost_only" <<'REMOTE'
set -euo pipefail
mediahost_only=$1
# Match the resolved executable, not argv[0]: a process started as
# ./kahawai-mediahost keeps that relative spelling in cmdline and, after a
# replacement, /proc/PID/exe reads "... (deleted)". Both defeated anchored
# command-line matching and left the old protocol binary connected.
for proc in /proc/[0-9]*; do
    exe=$(readlink "$proc/exe" 2>/dev/null || true)
    exe=${exe% (deleted)}
    if $mediahost_only && [[ "$exe" != /home/ingmar/kahawai-mediahost ]]; then continue; fi
    case "$exe" in
        /home/ingmar/kahawai|/home/ingmar/kahawai-mediahost|/home/ingmar/kahawai-transcoder)
            kill "${proc##*/}" 2>/dev/null || true ;;
    esac
done
sleep 2
left=""
for proc in /proc/[0-9]*; do
    exe=$(readlink "$proc/exe" 2>/dev/null || true)
    exe=${exe% (deleted)}
    if $mediahost_only && [[ "$exe" != /home/ingmar/kahawai-mediahost ]]; then continue; fi
    case "$exe" in
        /home/ingmar/kahawai|/home/ingmar/kahawai-mediahost|/home/ingmar/kahawai-transcoder)
            left="$left ${proc##*/}:$exe" ;;
    esac
done
if [[ -n "$left" ]]; then
    echo "satellite executable still running:$left" >&2
    exit 1
fi
REMOTE

echo "==> activating staged binaries" >&2
ssh "$HOST" bash -s -- "$mediahost_only" <<'REMOTE'
set -euo pipefail
bins=kahawai-mediahost
if ! "$1"; then bins="$bins kahawai-transcoder"; fi
for bin in $bins; do
    cp -p "$HOME/$bin" "$HOME/$bin.previous"
    cp -p "$HOME/.kahawai-stage/$bin" "$HOME/$bin.next"
    mv -f "$HOME/$bin.next" "$HOME/$bin"
done
if ! "$1"; then
    cp -a ~/.local/lib/kahawai-gst "$HOME/.kahawai-stage/gst.previous.$(date +%s)"
    rsync -a ~/.kahawai-stage/gst/ ~/.local/lib/kahawai-gst/
fi
REMOTE

echo "==> starting" >&2
ssh "$HOST" bash -s -- "$mediahost_only" <<'REMOTE'
set -euo pipefail
mediahost_only=$1
gst="$HOME/.local/lib/kahawai-gst"
export GST_PLUGIN_PATH="$gst/plugins"
export GST_PLUGIN_SYSTEM_PATH_1_0="$gst/plugins:/usr/lib/gstreamer-1.0"
export LD_LIBRARY_PATH="$gst/lib"
loaded=$(gst-inspect-1.0 matroskademux | sed -n 's/^  Filename *//p')
[[ "$loaded" == "$gst/plugins/libgstmatroska.so" ]] || {
    echo "staged matroskademux did not load: $loaded" >&2
    exit 1
}
tc_mark=$(wc -l < ~/kahawai-transcoder.log)
nohup ~/kahawai-mediahost >> ~/kahawai-mediahost.log 2>&1 &
mh_pid=$!
tc_pid=""
if ! $mediahost_only; then
    nohup ~/kahawai-transcoder >> ~/kahawai-transcoder.log 2>&1 &
    tc_pid=$!
fi
for attempt in $(seq 1 60); do
    kill -0 "$mh_pid"
    if [[ -n "$tc_pid" ]]; then kill -0 "$tc_pid"; fi
    if $mediahost_only || tail -n +"$((tc_mark + 1))" ~/kahawai-transcoder.log | grep 'link established' >/dev/null; then
        echo "mediahost PID $mh_pid; transcoder PID $tc_pid; transcoder link verified"
        exit 0
    fi
    sleep 1
done
echo "transcoder did not establish a fresh link" >&2
exit 1
REMOTE

# The multi-hub mediahost does not log a successful Hello. Verify the hub's
# current projection of the link instead of waiting for a nonexistent line.
api="${KAHAWAI_API:-http://127.0.0.1:8420}"
[[ "$api" == http*://* ]] || api="http://$api"
python3 - "$api" "${KAHAWAI_MEDIAHOST_NAME:-silence}" "$(git -C "$repo" rev-parse --short HEAD)" <<'PYVERIFY'
import json, sys, time, urllib.request
base, name, build = sys.argv[1:]
for attempt in range(60):
    try:
        with urllib.request.urlopen(base.rstrip('/') + '/health', timeout=2) as response:
            modules = json.load(response)['modules']
        if any(m['kind'] == 'mediahost' and m['name'] == name and m['status'] == 'ok'
               and (m.get('build') or '').startswith(build) for m in modules):
            print(f'mediahost {name}: connected to hub on build {build}')
            break
    except (OSError, ValueError):
        pass
    time.sleep(1)
else:
    sys.exit(f'mediahost {name} did not connect to {base} on build {build}')
PYVERIFY
