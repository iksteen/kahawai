#!/usr/bin/env python3
# Reproduces a fatal demux error in gst-plugins-good avidemux (1.28.5):
#   gstavidemux.c(5932): gst_avi_demux_chain (): unhandled buffer size
# An AVI whose first top-level chunk is a zero-sized JUNK is refused in
# PUSH mode: gst_avi_demux_peek_chunk() treats size 0 as unparseable and
# sets abort_buffering, and the header-state caller has no escape. Pull
# mode skips it (gst_riff_read_chunk skips JUNK/JUNQ automatically), so
# the same file plays from filesrc and fails from any pushing source.
#
#   python3 avidemux-leading-junk.py
#
# Exits 0 when the bug reproduces, 1 when the plugin is fixed.
import os
import struct
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402


def make_avi_with_leading_junk(path):
    """Mux a short AVI, then splice a zero-sized JUNK in front of hdrl."""
    subprocess.run(
        ["gst-launch-1.0", "-q",
         "videotestsrc", "num-buffers=50", "!",
         "video/x-raw,format=I420,width=160,height=120,framerate=25/1", "!",
         "jpegenc", "!", "avimux", "!", "filesink", "location=" + path],
        check=True)
    data = bytearray(open(path, "rb").read())
    assert data[:4] == b"RIFF" and data[8:12] == b"AVI ", "not an AVI"
    data[12:12] = b"JUNK" + struct.pack("<I", 0)
    struct.pack_into("<I", data, 4, struct.unpack_from("<I", data, 4)[0] + 8)
    open(path, "wb").write(data)


def demux_push(path):
    """Feed the file to avidemux through a push-only appsrc."""
    Gst.init(None)
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
    handle = open(path, "rb")

    def feed():
        chunk = handle.read(64 * 1024)
        if not chunk:
            src.emit("end-of-stream")
            return False
        src.emit("push-buffer", Gst.Buffer.new_wrapped(chunk))
        return True

    GLib.idle_add(feed)
    GLib.timeout_add_seconds(20, lambda: (loop.quit(), False)[1])
    loop.run()
    pipe.set_state(Gst.State.NULL)
    return state


tmp = tempfile.mkdtemp()
avi = os.path.join(tmp, "leading-junk.avi")
make_avi_with_leading_junk(avi)
result = demux_push(avi)
print("pads exposed: %d, error: %s" % (result["pads"], result["error"] or "none"))
if result["error"]:
    print("REPRODUCED: a zero-sized JUNK before the header kills push-mode demux")
    sys.exit(0)
print("not reproduced — avidemux skips the padding (patched)")
sys.exit(1)
