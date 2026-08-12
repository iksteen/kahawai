#!/usr/bin/env bash
# Serve the web app from vite instead of from inside the hub binary.
#
# Why this exists: `web/dist` is compiled into the hub by rust-embed, so the
# normal loop for a one-line UI change is `cargo build --release` — which
# builds the bundle itself, see the hub's build.rs — at ~40s, plus a restart
# and the wait for the new hash (~15s). A minute a go, for a CSS tweak. Vite serves the same app on :5173 and proxies /api and
# /admin to the hub on :8420, so the loop becomes a page reload — and hot
# module replacement usually not even that.
#
# The hub still has to be running: it owns the database, the sessions and the
# media. This only moves where the HTML and JS come from.
#
# Signing in: the token lives in localStorage, which is per-origin, so :5173
# starts logged out even if :8420 is not — sign in once on the dev origin. The
# `kahawai_token` cookie that authenticates <img> and <video> is set by the
# client on sign-in and cookies ignore the port, so artwork and playback work
# from either origin once you have.
#
# Usage: kahawai-web-dev.sh [--stop]
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
port=5173
hub=8420

listening() { ss -ltn "sport = :$1" 2>/dev/null | grep -q LISTEN; }

if [ "${1:-}" = "--stop" ]; then
    # By port, not by a pgrep pattern: a `-f` pattern for "vite" also matches
    # the shell running this script.
    pid=$(ss -ltnp "sport = :$port" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | head -1 || true)
    [ -n "$pid" ] && kill "$pid" && echo "stopped vite (pid $pid)" || echo "nothing on :$port"
    exit 0
fi

listening "$hub" || {
    echo "!! nothing on :$hub — the API, the database and the media all live there." >&2
    echo "   start it with: ./scripts/kahawai-restart.sh all-in-one" >&2
    exit 1
}

if listening "$port"; then
    echo "==> vite already on :$port"
else
    echo "==> starting vite on :$port" >&2
    # StrictMode stays on. It double-invokes every effect here, which is the
    # point: playback used to die on that (the player released its session in a
    # cleanup, so the second setup inherited a disposed one and everything
    # answered 410), and now it survives. If playback ever breaks under
    # `npm run dev` while working from the hub, that is the same class of bug
    # returning, and it is worth chasing rather than switching off.
    (cd "$repo/web" && npm run dev >"$repo/web/vite-dev.log" 2>&1 &)
    for _ in $(seq 1 60); do
        listening "$port" && break
        sleep 0.5
    done
    listening "$port" || {
        echo "!! vite did not come up — see web/vite-dev.log" >&2
        tail -5 "$repo/web/vite-dev.log" >&2 || true
        exit 1
    }
fi

cat <<EOF
==> http://localhost:$port/app/   (API proxied to :$hub)

    Edits to web/src appear on reload. No npm run build, no cargo build, no
    restart — those are only needed for what the hub actually ships.

    Sign in once on this origin; the dist inside the hub is unchanged and
    :$hub keeps serving the built app as before.

    stop with: $0 --stop
EOF
