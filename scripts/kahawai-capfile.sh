#!/usr/bin/env bash
# Render a capability-negotiation torture file (HUB-14/15).
#
#   kahawai-capfile.sh [output-dir]        (default: $HOME)
#
# Produces "Capability Torture (2026).mkv" + a sidecar .ass:
#   - video: HEVC Main-10, 10-bit, 1080p, PQ colorimetry → probes as
#     hdr10; browsers WITHOUT hevc/main-10 get a transcode, those WITH
#     it get a copy plus the HUB-15a "HDR delivered as-is" verdict
#   - audio 0: AAC 5.1 (channel-ceiling test; copies web-side)
#   - audio 1: AC-3 stereo (switching to it re-plans copy → encode)
#   - container: MKV (never direct on a stock browser profile)
#   - sidecar ASS (JASSUB pass-through vs flatten per ass_render)
#
# Drop it into any movies collection root; the scan minds it as its own
# item. Play it per browser and read the playback-info overlay — every
# line of the verdict is a negotiation decision on display.
set -euo pipefail

# The torture file is rendered by the muxers we patch, so render it
# with the patched ones.
. "$(dirname "$0")/kahawai-gst-env.sh"

OUT_DIR="${1:-$HOME}"
NAME="Capability Torture (2026)"
MKV="$OUT_DIR/$NAME.mkv"

for e in x265enc avenc_ac3; do
    gst-inspect-1.0 "$e" >/dev/null 2>&1 || { echo "missing element: $e" >&2; exit 1; }
done
AAC=$(for e in fdkaacenc avenc_aac; do gst-inspect-1.0 "$e" >/dev/null 2>&1 && { echo "$e"; break; }; done)
[ -n "$AAC" ] || { echo "no AAC encoder" >&2; exit 1; }

# 30 s. smpte bars make encode artifacts visible; the ball pattern on
# the second audio's beep keeps A/V sync judgeable by eye.
gst-launch-1.0 -q \
    videotestsrc pattern=smpte num-buffers=750 \
      ! video/x-raw,format=I420_10LE,width=1920,height=1080,framerate=25/1,colorimetry=bt2100-pq \
      ! x265enc bitrate=4000 speed-preset=ultrafast \
      ! h265parse ! mux. \
    audiotestsrc wave=sine freq=440 num-buffers=1450 \
      ! audio/x-raw,channels=6,channel-mask='(bitmask)0x3f',rate=48000 \
      ! audioconvert ! "$AAC" ! aacparse ! mux. \
    audiotestsrc wave=ticks num-buffers=1450 \
      ! audio/x-raw,channels=2,rate=48000 \
      ! audioconvert ! avenc_ac3 ! ac3parse ! mux. \
    matroskamux name=mux ! filesink location="$MKV"

# Sidecar ASS: real styling so JASSUB has something to be faithful TO.
cat > "$OUT_DIR/$NAME.en.ass" << 'ASS'
[Script Info]
Title: Capability Torture
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, Bold, Outline, Alignment
Style: Top,Arial,64,&H0000FFFF,&H00000000,-1,3,8
Style: Bottom,Arial,48,&H00FFFFFF,&H00000000,0,2,2

[Events]
Format: Layer, Start, End, Style, Text
Dialogue: 0,0:00:01.00,0:00:29.00,Top,{\pos(960,80)}HEVC Main-10 · PQ · 5.1 AAC + AC-3
Dialogue: 0,0:00:02.00,0:00:29.00,Bottom,if you can read the styling, ass_render passed through
ASS

echo "rendered: $MKV" >&2
echo "sidecar : $OUT_DIR/$NAME.en.ass" >&2
