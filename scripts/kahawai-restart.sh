#!/usr/bin/env bash
# Restart a local kahawai service the safe way: stop, VERIFY it died,
# start, VERIFY it came back. Exists so nobody hand-writes a pgrep
# pattern for the routine case.
#
# Two traps this closes, both hit repeatedly in practice:
#
#   1. Self-match. `pkill -f 'kahawai hub'` also matches the wrapper
#      shell whose command line contains that string, so it kills its
#      own invocation: exit 144, target still alive, and the next
#      command shows a running service — indistinguishable from a
#      successful restart. The bracket form `[k]ahawai` cannot match
#      itself, because the pattern text and the process text differ.
#   2. Wrong flavor. An anchor on `target/debug/` silently misses a
#      service running from `target/release/`, and a kill that matches
#      nothing looks exactly like a kill that worked. So: match either
#      flavor, and verify the pid actually changed.
#
# Usage: kahawai-restart.sh <hub|all-in-one|transcoder|mediahost> [--build|--stop-only]
set -euo pipefail

svc="${1:-}"
case "$svc" in
  hub | all-in-one | transcoder | mediahost) ;;
  *)
    echo "usage: $0 <hub|all-in-one|transcoder|mediahost> [--build]" >&2
    exit 2
    ;;
esac
build=false
stop_only=false
case "${2:-}" in
  "") ;;
  --build) build=true ;;
  --stop-only) stop_only=true ;;
  *) echo "usage: $0 <hub|all-in-one|transcoder|mediahost> [--build|--stop-only]" >&2; exit 2 ;;
esac

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"

# Bracketed first character: this pattern can never match the shell
# that runs it, whatever the rest of the command line says.
# ERE, not BRE — pgrep -f takes extended regex, so alternation is
# (a|b). The first draft wrote \(a\|b\) and matched NOTHING, which the
# verify steps below turned into a loud failure instead of a silent
# "restart worked". That is exactly why they are there.
pat="[.]/target/(debug|release)/kahawai $svc\$"
pids() { pgrep -f "$pat" || true; }

before=$(pids | tr '\n' ' ')
echo "==> running before: ${before:-none}" >&2

if $build; then
  echo "==> cargo build --release" >&2
  cargo build --release
fi

# Prefer the release binary when both exist (that is what deploys run).
exe=""
for f in "$repo/target/release/kahawai" "$repo/target/debug/kahawai"; do
  [ -x "$f" ] && exe="$f" && break
done
[ -n "$exe" ] || { echo "no kahawai binary built" >&2; exit 1; }

if [ -n "${before// /}" ]; then
  for p in $before; do kill "$p" 2>/dev/null || true; done
  for _ in $(seq 1 20); do
    [ -z "$(pids)" ] && break
    sleep 0.5
  done
  if [ -n "$(pids)" ]; then
    echo "!! still alive after SIGTERM: $(pids | tr '\n' ' ') — escalating" >&2
    for p in $(pids); do kill -9 "$p" 2>/dev/null || true; done
    sleep 1
  fi
fi
[ -z "$(pids)" ] || { echo "FAILED to stop $svc" >&2; exit 1; }
echo "==> stopped" >&2
$stop_only && exit 0

# A locally staged codec stack, if one is present: patches/gstreamer/
# 0004 changes the size of a public H.264 struct, so the plugins holding
# one must come from the same build as the library. Kahawai-only on
# purpose — a plugin here is invisible to every other GStreamer program
# on the box, so nothing else can end up mixing the two ABIs. The
# plugins carry an RPATH to the matching library, and this exports the
# path anyway so a hand-run gst-launch against the same directory
# behaves like the service does.
gst_local="$HOME/.local/lib/kahawai-gst"
if [ -d "$gst_local/plugins" ]; then
  export GST_PLUGIN_PATH="$gst_local/plugins${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
  export LD_LIBRARY_PATH="$gst_local/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  echo "==> staged plugins: $gst_local/plugins" >&2
fi

log="${KAHAWAI_LOG_DIR:-$HOME/.local/share/kahawai}/${svc}.log"
mkdir -p "$(dirname "$log")"
# Relative exec path so the anchored/bracketed pattern keeps matching.
rel="./${exe#"$repo"/}"
# setsid + </dev/null: the hub READS STDIN (enrollment codes), so an
# inherited descriptor keeps this script — and anything piping its
# output — alive forever. Fully detach.
#
# --fork, or the detaching is a coin toss. Without it setsid EXECS in place
# when it can, and bash had already optimised away the subshell's own fork —
# so the service stayed this script's child and bash sat in do_wait on it.
# The restart still worked and still reported "started", then hung until the
# service exited: ten minutes of a finished deploy looking like a slow build,
# repeatedly, before anyone thought to look at the process tree. Forcing the
# fork means the service is init's child by the time we check for it.
( cd "$repo" && setsid --fork "$rel" "$svc" >>"$log" 2>&1 </dev/null & ) || true

for _ in $(seq 1 20); do
  now=$(pids | tr '\n' ' ')
  if [ -n "${now// /}" ] && [ "$now" != "$before" ]; then
    echo "==> started: $now (was: ${before:-none})" >&2
    echo "==> log: $log" >&2
    exit 0
  fi
  sleep 0.5
done
echo "FAILED to start $svc — see $log" >&2
tail -5 "$log" >&2 || true
exit 1
