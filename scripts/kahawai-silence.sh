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
case "${2:-}" in
    "") ;;
    --mediahost-only) mediahost_only=true ;;
    *) echo "usage: $0 [user@host] [--mediahost-only]" >&2; exit 2 ;;
esac
repo=$(cd "$(dirname "$0")/.." && pwd)

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

echo "==> shipping binaries and staged GStreamer to $HOST" >&2
scp -q "$repo/target/release/kahawai-mediahost" "$HOST:~/"
if ! $mediahost_only; then
    scp -q "$repo/target/release/kahawai-transcoder" "$HOST:~/"
    rsync -a "$gst_dir/" "$HOST:~/.local/lib/kahawai-gst/"
fi

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
nohup ~/kahawai-mediahost >> ~/kahawai-mediahost.log 2>&1 &
if ! $mediahost_only; then
    nohup ~/kahawai-transcoder >> ~/kahawai-transcoder.log 2>&1 &
fi
sleep 5
pgrep -af '^/home/ingmar/kahawai-' | head -4
REMOTE

echo "==> links (from the satellite logs)" >&2
ssh "$HOST" "tail -3 ~/kahawai-mediahost.log; tail -2 ~/kahawai-transcoder.log" \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -E 'link established|error' || true
