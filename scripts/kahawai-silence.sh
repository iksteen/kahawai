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
# Usage: kahawai-silence.sh [user@host]
set -euo pipefail

HOST="${1:-ingmar@192.168.0.109}"
repo=$(cd "$(dirname "$0")/.." && pwd)

echo "==> building lean satellite binaries" >&2
(cd "$repo" && cargo build --release -p kahawai-mediahostd)
(cd "$repo" && cargo build --release -p kahawai-transcoderd)

# Stop FIRST: scp into a running executable fails with ETXTBSY.
echo "==> stopping satellites on $HOST" >&2
ssh "$HOST" 'bash -s' <<'REMOTE'
set -euo pipefail
# Old single-binary layout: retire it so nothing relaunches it by habit.
pkill -f '^/home/ingmar/kahawai mediahost' 2>/dev/null || true
pkill -f '^/home/ingmar/kahawai transcoder' 2>/dev/null || true
pkill -f '^/home/ingmar/kahawai-mediahost' 2>/dev/null || true
pkill -f '^/home/ingmar/kahawai-transcoder' 2>/dev/null || true
sleep 2
REMOTE

echo "==> shipping to $HOST" >&2
scp -q "$repo/target/release/kahawai-mediahost" \
    "$repo/target/release/kahawai-transcoder" "$HOST:~/"

echo "==> starting" >&2
ssh "$HOST" 'bash -s' <<'REMOTE'
set -euo pipefail
nohup ~/kahawai-mediahost >> ~/kahawai-mediahost.log 2>&1 &
nohup ~/kahawai-transcoder >> ~/kahawai-transcoder.log 2>&1 &
sleep 5
pgrep -af '^/home/ingmar/kahawai-' | head -4
REMOTE

echo "==> links (from the satellite logs)" >&2
ssh "$HOST" "tail -3 ~/kahawai-mediahost.log; tail -2 ~/kahawai-transcoder.log" \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -E 'link established|error' || true
