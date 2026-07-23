#!/usr/bin/env python3
# Reproduces a second process abort in gst-plugins-rs hlssink3 (<= 0.15.3):
#   thread panicked at net/hlssink3/src/hlsbasesink.rs:660:
#   called `Option::unwrap()` on a `None` value  (running_time.unwrap())
# When the fragment-opening buffer's PTS converts to no running time
# (PTS before the segment start — real-world streams with broken
# timestamps get there easily), imp.rs stores running_time = None and the
# hls-segment-added emission unwraps it. Non-unwinding panic -> SIGABRT.
import sys, os
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst

Gst.init(None)
out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/hlssink3-repro2"
os.makedirs(out, exist_ok=True)
offset = int(sys.argv[2]) if len(sys.argv) > 2 else -5

p = Gst.parse_launch(
    "videotestsrc num-buffers=300 ! video/x-raw,width=320,height=240,framerate=30/1 "
    "! x264enc key-int-max=30 tune=zerolatency ! h264parse ! identity name=shift "
    f"! hlssink3 location={out}/seg%05d.ts playlist-location={out}/list.m3u8 "
    "target-duration=1")

# Shift running time negative: to_running_time() yields None for the
# fragment-first buffer.
shift = p.get_by_name("shift")
shift.get_static_pad("src").set_offset(offset * Gst.SECOND)

p.set_state(Gst.State.PLAYING)
bus = p.get_bus()
msg = bus.timed_pop_filtered(30 * Gst.SECOND,
                             Gst.MessageType.EOS | Gst.MessageType.ERROR)
print("finished without crash:", msg.type if msg else "timeout")
p.set_state(Gst.State.NULL)
