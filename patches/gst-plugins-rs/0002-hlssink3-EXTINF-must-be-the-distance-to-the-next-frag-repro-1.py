#!/usr/bin/env python3
# Reproduces hlssink3 declaring EXTINF values longer than the fragments
# actually are. It writes splitmuxsink's `fragment-duration`, which is the
# LONGEST stream's end minus the fragment start, not the distance to the
# next fragment. Audio frames do not align to a video GOP, so every
# segment overshoots and a playlist gains the surplus for ever.
#
# Video alone cannot show it: with one stream the largest end IS the
# reference end. The audio branch is the whole point.
#
# Affected builds declare a total noticeably longer than the media; fixed
# ones land within a few milliseconds of it.
import os
import sys

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst

Gst.init(None)
out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/hlssink3-extinf-repro"
os.makedirs(out, exist_ok=True)

# 24 s of media: 30 fps with 60-frame GOPs = 2 s fragments on the video,
# while AAC's 1024-sample frames at 44.1 kHz land elsewhere every time.
SECONDS = 24
p = Gst.parse_launch(
    f"videotestsrc num-buffers={30 * SECONDS} is-live=false "
    "! video/x-raw,width=320,height=240,framerate=30/1 "
    "! x264enc key-int-max=60 tune=zerolatency ! h264parse ! mux.video "
    f"audiotestsrc num-buffers={43 * SECONDS} samplesperbuffer=1024 "
    "! audio/x-raw,rate=44100,channels=2 ! avenc_aac ! aacparse ! mux.audio "
    f"hlssink3 name=mux location={out}/seg%05d.ts "
    f"playlist-location={out}/list.m3u8 target-duration=2 playlist-length=0 "
    "max-files=0"
)

p.set_state(Gst.State.PLAYING)
bus = p.get_bus()
msg = bus.timed_pop_filtered(
    120 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR
)
p.set_state(Gst.State.NULL)

if msg is None or msg.type == Gst.MessageType.ERROR:
    print("pipeline did not finish:", msg and msg.parse_error()[0])
    raise SystemExit(2)

declared = 0.0
with open(f"{out}/list.m3u8") as fh:
    for line in fh:
        if line.startswith("#EXTINF:"):
            declared += float(line[len("#EXTINF:"):].rstrip(",\n"))

drift = declared - SECONDS
print(f"media produced : {SECONDS:.3f} s")
print(f"playlist claims: {declared:.3f} s")
print(f"drift          : {drift * 1000:+.0f} ms over {SECONDS} s")
# A fragment's worth of surplus over 24 s is far outside rounding; a
# correct sink lands within a few ms either way.
if abs(drift) > 0.05:
    print("AFFECTED: EXTINF is not the distance to the next fragment")
    raise SystemExit(1)
print("ok: playlist duration matches the media")
