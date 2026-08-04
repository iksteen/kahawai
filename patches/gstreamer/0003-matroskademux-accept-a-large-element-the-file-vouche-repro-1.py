#!/usr/bin/env python3
# Reproduces a fatal demux error in gst-plugins-good matroskademux (1.28.5):
#   matroska-demux.c(5650): gst_matroska_demux_check_read_size ():
#   reading large block of size N not supported; file might be corrupt.
# MAX_BLOCK_SIZE refuses any element over 32 MiB, and in streaming mode
# the refusal is fatal — pull mode skips what it cannot hold, streaming
# cannot. Fansubbed releases attach their subtitle fonts, and a CJK font
# pack passes 32 MiB without being remarkable, so such files are
# undemuxable from any pushing source while filesrc plays them.
#
#   python3 matroskademux-large-attachment.py [attachment MiB, default 33]
#
# Exits 0 when the bug reproduces, 1 when the plugin is fixed.
import os
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

ATTACH_MIB = int(sys.argv[1]) if len(sys.argv) > 1 else 33


def vint(value):
    """EBML unsigned data size, shortest form that fits."""
    for length in range(1, 9):
        if value < (1 << (7 * length)) - 1:
            return (value | (1 << (7 * length))).to_bytes(length, "big")
    raise ValueError(value)


def element(eid, payload):
    return eid + vint(len(payload)) + payload


def make_mkv_with_attachment(path, payload_bytes):
    """Mux a short MKV, then splice an Attachments element before the
    first Cluster and correct the Segment size."""
    subprocess.run(
        ["gst-launch-1.0", "-q",
         "videotestsrc", "num-buffers=50", "!",
         "video/x-raw,format=I420,width=160,height=120,framerate=25/1", "!",
         "x264enc", "!", "h264parse", "!", "matroskamux", "!",
         "filesink", "location=" + path],
        check=True)
    data = bytearray(open(path, "rb").read())

    attached = element(b"\x61\xa7",
                       element(b"\x46\x6e", b"repro.otf")
                       + element(b"\x46\x60", b"application/x-truetype-font")
                       + element(b"\x46\x5c", b"\0" * payload_bytes)
                       + element(b"\x46\xae", b"\x01\x02\x03\x04"))
    attachments = element(b"\x19\x41\xa4\x69", attached)

    cluster = data.find(b"\x1f\x43\xb6\x75")
    assert cluster > 0, "no Cluster found"
    data[cluster:cluster] = attachments

    # Segment size grows by exactly what we spliced in.
    seg = data.find(b"\x18\x53\x80\x67")
    assert seg > 0, "no Segment found"
    size_at = seg + 4
    assert data[size_at] == 0x01, "expected an 8-byte Segment size"
    old = int.from_bytes(data[size_at + 1:size_at + 8], "big")
    data[size_at + 1:size_at + 8] = (old + len(attachments)).to_bytes(7, "big")

    open(path, "wb").write(data)
    return len(attachments)


def demux_push(path):
    """Feed the file to matroskademux through a push-only appsrc."""
    pipe = Gst.Pipeline.new("p")
    src = Gst.ElementFactory.make("appsrc")
    src.set_property("stream-type", 0)          # STREAM: push, never pull
    src.set_property("size", os.path.getsize(path))
    dem = Gst.ElementFactory.make("matroskademux")
    pipe.add(src)
    pipe.add(dem)
    src.link(dem)

    state = {"pads": 0, "attachments": 0, "error": None}

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
        if msg.type == Gst.MessageType.TAG:
            state["attachments"] += msg.parse_tag().get_tag_size("attachment")
        elif msg.type == Gst.MessageType.ERROR:
            state["error"] = msg.parse_error()[0].message
            loop.quit()
        elif msg.type == Gst.MessageType.EOS:
            loop.quit()

    bus.connect("message", on_message)
    pipe.set_state(Gst.State.PLAYING)
    handle = open(path, "rb")

    def feed():
        chunk = handle.read(256 * 1024)
        if not chunk:
            src.emit("end-of-stream")
            return False
        src.emit("push-buffer", Gst.Buffer.new_wrapped(chunk))
        return True

    GLib.idle_add(feed)
    GLib.timeout_add_seconds(60, lambda: (loop.quit(), False)[1])
    loop.run()
    pipe.set_state(Gst.State.NULL)
    return state


Gst.init(None)
tmp = tempfile.mkdtemp()
mkv = os.path.join(tmp, "big-attachment.mkv")
spliced = make_mkv_with_attachment(mkv, ATTACH_MIB * 1024 * 1024)
print("fixture: %d bytes, Attachments element %d bytes (%d MiB payload)"
      % (os.path.getsize(mkv), spliced, ATTACH_MIB))

result = demux_push(mkv)
print("  pads=%d attachments published=%d error=%s"
      % (result["pads"], result["attachments"], result["error"] or "none"))

if result["error"]:
    print("REPRODUCED: an element the file plainly contains is refused in push mode")
    sys.exit(0)
print("not reproduced — the element is accepted (patched)")
sys.exit(1)
