#!/usr/bin/env python3
# Reproduces a fatal demux error in gst-plugins-good avidemux (1.28.5):
#   gstavidemux.c(895): gst_avi_demux_handle_sink_event ():
#   got eos and didn't receive a complete header object
# gst_avi_demux_chain() advances exactly ONE state per buffer: START
# parses the RIFF header and returns, HEADER waits for the next buffer.
# A source that hands over the whole file in a single buffer therefore
# never leaves the header, and every byte having arrived makes no
# difference. filesrc hides this by operating in pull mode; any pushing
# source whose read block exceeds the file size hits it.
#
#   python3 avidemux-single-buffer-header.py
#
# Exits 0 when the bug reproduces, 1 when the plugin is fixed.
import os
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402


def make_small_avi(path):
    subprocess.run(
        ["gst-launch-1.0", "-q",
         "videotestsrc", "num-buffers=50", "!",
         "video/x-raw,format=I420,width=160,height=120,framerate=25/1", "!",
         "jpegenc", "!", "avimux", "!", "filesink", "location=" + path],
        check=True)


def demux_push(path, chunks):
    """Push the whole file as `chunks` buffers; report the outcome."""
    pipe = Gst.Pipeline.new("p")
    src = Gst.ElementFactory.make("appsrc")
    src.set_property("stream-type", 0)          # STREAM: push, never pull
    src.set_property("size", os.path.getsize(path))
    dem = Gst.ElementFactory.make("avidemux")
    pipe.add(src)
    pipe.add(dem)
    src.link(dem)

    state = {"pads": 0, "error": None}

    def on_pad(_element, pad):
        state["pads"] += 1
        sink = Gst.ElementFactory.make("fakesink")
        sink.set_property("sync", False)
        sink.set_property("async", False)
        pipe.add(sink)
        sink.sync_state_with_parent()
        pad.link(sink.get_static_pad("sink"))

    dem.connect("pad-added", on_pad)
    loop = GLib.MainLoop()
    bus = pipe.get_bus()
    bus.add_signal_watch()

    def on_message(_bus, msg):
        if msg.type == Gst.MessageType.ERROR:
            state["error"] = msg.parse_error()[0].message
            loop.quit()
        elif msg.type == Gst.MessageType.EOS:
            loop.quit()

    bus.connect("message", on_message)
    pipe.set_state(Gst.State.PLAYING)

    data = open(path, "rb").read()
    step = len(data) // chunks + 1
    for i in range(0, len(data), step):
        src.emit("push-buffer", Gst.Buffer.new_wrapped(data[i:i + step]))
    src.emit("end-of-stream")
    GLib.timeout_add_seconds(20, lambda: (loop.quit(), False)[1])
    loop.run()
    pipe.set_state(Gst.State.NULL)
    return state


Gst.init(None)
tmp = tempfile.mkdtemp()
avi = os.path.join(tmp, "small.avi")
make_small_avi(avi)
print("fixture: %d bytes" % os.path.getsize(avi))

one = demux_push(avi, 1)
two = demux_push(avi, 2)
print("  as 1 buffer : pads=%d error=%s" % (one["pads"], one["error"] or "none"))
print("  as 2 buffers: pads=%d error=%s" % (two["pads"], two["error"] or "none"))

if one["error"] and not two["error"]:
    print("REPRODUCED: identical bytes, and only the buffer COUNT decides it")
    sys.exit(0)
print("not reproduced — chain() keeps dispatching while the state advances (patched)")
sys.exit(1)
