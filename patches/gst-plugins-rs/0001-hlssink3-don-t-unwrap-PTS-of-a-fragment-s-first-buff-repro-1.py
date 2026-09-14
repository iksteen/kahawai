#!/usr/bin/env python3
# Reproduces a process abort in gst-plugins-rs hlssink3 (<= 0.15.3):
#   thread panicked at net/hlssink3/src/hlssink3/imp.rs:304:
#   called `Option::unwrap()` on a `None` value
# The format-location-full handler unwraps the PTS of the fragment's
# first buffer. Real-world AVI streams (avidemux) emit frames without
# PTS; this script mimics that by stripping PTS from keyframes.
# The panic occurs in an FFI callback and cannot unwind -> SIGABRT.
import sys
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst, GLib

Gst.init(None)
out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/hlssink3-repro"
import os; os.makedirs(out, exist_ok=True)

p = Gst.parse_launch(
    "videotestsrc num-buffers=300 ! video/x-raw,width=320,height=240,framerate=30/1 "
    "! x264enc key-int-max=30 tune=zerolatency ! h264parse ! identity name=strip "
    f"! hlssink3 location={out}/seg%05d.ts playlist-location={out}/list.m3u8 "
    "target-duration=1")

stripped = 0

def strip_pts(pad, info):
    global stripped
    buf = info.get_buffer()
    if not buf.has_flags(Gst.BufferFlags.DELTA_UNIT):  # keyframes only
        # A probe does not own its buffer, and PyGObject 3.58 enforces
        # that: assigning to .pts raises NotWritableMiniObject, where
        # 3.56 let it through. 3.58 also binds make_writable as an
        # in-place call returning a bool, not the buffer — and .copy()
        # does NOT help, the copy is marked immutable too. 3.56 has no
        # make_writable at all and needs none.
        if hasattr(buf, "make_writable"):
            buf.make_writable()
        buf.pts = Gst.CLOCK_TIME_NONE
        info.set_buffer(buf)
        stripped += 1
    return Gst.PadProbeReturn.OK

strip = p.get_by_name("strip")
strip.get_static_pad("src").add_probe(Gst.PadProbeType.BUFFER, strip_pts)

p.set_state(Gst.State.PLAYING)
bus = p.get_bus()
msg = bus.timed_pop_filtered(30 * Gst.SECOND,
                             Gst.MessageType.EOS | Gst.MessageType.ERROR)
print("finished without crash:", msg.type if msg else "timeout")
p.set_state(Gst.State.NULL)

# The fixture is the whole test: if no PTS was actually removed, this
# reproduced nothing and "no crash" means only that nothing was asked of
# the sink. Say so rather than pass.
print("keyframe PTS stripped:", stripped)
if stripped == 0:
    print("FIXTURE FAILED: no buffer was modified, so nothing was tested")
    raise SystemExit(2)
