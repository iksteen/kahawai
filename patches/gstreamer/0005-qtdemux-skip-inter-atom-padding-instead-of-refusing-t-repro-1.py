#!/usr/bin/env python3
# Reproduces qtdemux refusing a playable MP4 when it is fed rather than
# read:
#
#   This file is invalid and cannot be played.
#   atom .... has bogus size 18446744073709551615
#
# Some muxers align the atom that follows by leaving eight zero bytes
# that no atom covers. ISO base media has no way to describe a gap
# belonging to nobody, so the walk reads a size field of 0 — which means
# "to the end of the file", and becomes G_MAXUINT64 — with a fourcc of
# 0, and the size check rejects the file.
#
# Only the streaming path meets it. Pull mode stops walking once it has
# the moov and reads by the sample tables, so it never visits the
# padding; push mode walks every byte. The same file plays from a
# file:// URI and fails from a pipe.
#
#   python3 0005-…-repro-1.py
#   GST_PLUGIN_PATH=/path/to/patched python3 0005-…-repro-1.py
#
# Exits 0 when the bug reproduces, 1 when the plugin is fixed. Builds
# its own fixture — no media needed.
import os
import struct
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst  # noqa: E402

Gst.init(None)


def mux_fixture(path):
    """An ordinary MP4 with the moov first, as any progressive file has."""
    subprocess.run(
        ["gst-launch-1.0", "-q",
         "videotestsrc", "num-buffers=48", "!",
         "video/x-raw,framerate=24/1,width=320,height=180", "!",
         "x264enc", "key-int-max=12", "!", "h264parse", "!",
         "qtmux", "faststart=true", "!",
         "filesink", "location=" + path],
        check=True)


def boxes(buf, start, end, depth=0):
    off = start
    while off + 8 <= end:
        size = struct.unpack(">I", buf[off:off + 4])[0]
        kind = buf[off + 4:off + 8].decode("latin1", "replace")
        if size == 1:
            size = struct.unpack(">Q", buf[off + 8:off + 16])[0]
        elif size == 0:
            size = end - off
        yield off, kind, size, depth
        if kind in ("moov", "trak", "mdia", "minf", "stbl"):
            yield from boxes(buf, off + 8, off + size, depth + 1)
        if size <= 0:
            return
        off += size


def add_padding(src, dst):
    """Insert eight orphan bytes before mdat and keep the file correct.

    Every chunk offset moves with the data, so the only thing wrong with
    the result is the padding itself — exactly what the affected library
    files look like, where a free atom's size is eight bytes short of
    the space it occupies.
    """
    data = bytearray(open(src, "rb").read())
    mdat = next(o for o, k, _, d in boxes(data, 0, len(data)) if k == "mdat" and d == 0)
    shifted = 0
    for off, kind, _size, _d in list(boxes(data, 0, len(data))):
        if kind != "stco":
            continue
        count = struct.unpack(">I", data[off + 12:off + 16])[0]
        for i in range(count):
            at = off + 16 + 4 * i
            struct.pack_into(">I", data, at, struct.unpack(">I", data[at:at + 4])[0] + 8)
            shifted += 1
    open(dst, "wb").write(bytes(data[:mdat]) + b"\x00" * 8 + bytes(data[mdat:]))
    return mdat, shifted


def demux(path, push):
    """Return (pads, error) after feeding the file to qtdemux."""
    pipeline = Gst.Pipeline.new(None)
    if push:
        src = Gst.ElementFactory.make("appsrc")
        src.set_property("stream-type", 0)          # no pull, no seeking
        src.set_property("size", os.path.getsize(path))
    else:
        src = Gst.ElementFactory.make("filesrc")
        src.set_property("location", path)
    demuxer = Gst.ElementFactory.make("qtdemux")
    for el in (src, demuxer):
        pipeline.add(el)
    src.link(demuxer)

    pads = []

    def on_pad(_demuxer, pad):
        pads.append(pad.get_name())
        sink = Gst.ElementFactory.make("fakesink")
        sink.set_property("async", False)
        pipeline.add(sink)
        sink.sync_state_with_parent()
        pad.link(sink.get_static_pad("sink"))

    demuxer.connect("pad-added", on_pad)
    pipeline.set_state(Gst.State.PLAYING)

    error = None
    if push:
        data = open(path, "rb").read()
        for at in range(0, len(data), 4096):
            buf = Gst.Buffer.new_wrapped(data[at:at + 4096])
            if src.emit("push-buffer", buf) != Gst.FlowReturn.OK:
                break
        src.emit("end-of-stream")
    msg = pipeline.get_bus().timed_pop_filtered(
        20 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR)
    if msg and msg.type == Gst.MessageType.ERROR:
        error = msg.parse_error()[0].message
    pipeline.set_state(Gst.State.NULL)
    return len(pads), error


tmp = tempfile.mkdtemp()
plain = os.path.join(tmp, "plain.mp4")
padded = os.path.join(tmp, "padded.mp4")
mux_fixture(plain)
at, shifted = add_padding(plain, padded)
print(f"fixture: {os.path.getsize(padded)} bytes, "
      f"8 orphan bytes before mdat@{at}, {shifted} chunk offsets corrected")

for label, path in (("clean", plain), ("padded", padded)):
    for mode, push in (("pull", False), ("push", True)):
        pads, error = demux(path, push)
        print(f"  {label:<7} {mode}: pads={pads}"
              + (f" — {error}" if error else ""))
        if label == "padded" and mode == "push":
            verdict = (pads, error)

pads, error = verdict
if error or pads == 0:
    print("REPRODUCED: the same file demuxes in pull mode and is refused in push mode")
    sys.exit(0)
print("not reproduced — the padding is skipped and the file demuxes (patched)")
sys.exit(1)
