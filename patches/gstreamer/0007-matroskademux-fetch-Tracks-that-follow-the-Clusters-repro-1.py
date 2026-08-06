#!/usr/bin/env python3
# Reproduces matroskademux refusing a playable file when it is fed
# rather than read:
#
#   Could not demultiplex stream. (matroska-demux.c: gst_matroska_demux_parse_id
#   (): File layout does not permit streaming)
#
# Matroska does not require Tracks before the Clusters, and the SeekHead
# at the front of the file says where it is. Pull mode goes and reads it
# (gst_matroska_demux_find_tracks). Streaming gives up on the first
# Cluster, so the same file plays from a file:// URI and fails from a
# pipe.
#
# The fixture also carries a large Void before the first Cluster, which
# is what the affected library files have and what makes the second
# patch necessary: gst_matroska_demux_flush() advanced the read offset
# before it knew the skip had happened, so every offset past a Void
# bigger than the adapter held was wrong by that Void's size — and the
# demuxer then resumed in the middle of a Cluster.
#
#   python3 0007-…-repro-1.py
#   GST_PLUGIN_PATH=/path/to/patched python3 0007-…-repro-1.py
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
from gi.repository import Gst  # noqa: E402

Gst.init(None)

SEEKHEAD, INFO, TRACKS, CUES, CLUSTER, VOID = (
    0x114D9B74, 0x1549A966, 0x1654AE6B, 0x1C53BB6B, 0x1F43B675, 0xEC)
VOID_BYTES = 1 << 16          # bigger than the adapter holds at first sight


def read_id(buf, off):
    first = buf[off]
    for i, mask in enumerate((0x80, 0x40, 0x20, 0x10)):
        if first & mask:
            n = i + 1
            return int.from_bytes(buf[off:off + n], "big"), n
    raise ValueError(f"bad element id at {off}")


def read_size(buf, off):
    first = buf[off]
    for i, mask in enumerate((0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01)):
        if first & mask:
            n = i + 1
            value = first & (mask - 1)
            for byte in buf[off + 1:off + n]:
                value = (value << 8) | byte
            return value, n
    raise ValueError(f"bad element size at {off}")


def children(buf, start, end):
    off = start
    while off < end:
        eid, idn = read_id(buf, off)
        size, szn = read_size(buf, off + idn)
        yield off, eid, idn + szn, size
        off += idn + szn + size


def void_of(total):
    """A Void element occupying exactly `total` bytes."""
    body = total - 1 - 8                      # id byte + 8-byte size field
    return bytes([VOID]) + b"\x01" + struct.pack(">Q", body)[1:] + b"\x00" * body


def build_fixture(src, dst):
    buf = bytearray(open(src, "rb").read())
    seg_off, seg_id, seg_hdr, seg_size = next(
        (o, i, h, s) for o, i, h, s in children(buf, 0, len(buf)) if i == 0x18538067)
    seg_start = seg_off + seg_hdr
    kids = list(children(buf, seg_start, seg_start + seg_size))

    t_off, _, t_hdr, t_size = next(k for k in kids if k[1] == TRACKS)
    t_total = t_hdr + t_size
    tracks = bytes(buf[t_off:t_off + t_total])
    cluster_off = next(k[0] for k in kids if k[1] == CLUSTER)

    # Tracks becomes Void in place — nothing moves — and is appended at
    # the end, where a file that muxed its tracks last would have it.
    buf[t_off:t_off + t_total] = void_of(t_total)
    buf[cluster_off:cluster_off] = void_of(VOID_BYTES)
    buf += tracks

    # The Segment grew by both insertions.
    grew = VOID_BYTES + t_total
    struct.pack_into(">Q", buf, seg_off + 4, (seg_size + grew) | (1 << 56))

    # Every SeekHead position past the inserted Void moves with it, and
    # the Tracks entry now points at the copy on the end.
    sh_off, _, sh_hdr, sh_size = next(k for k in kids if k[1] == SEEKHEAD)
    tracks_pos = len(buf) - t_total - seg_start
    moved = 0
    for s_off, s_id, s_hdr, s_size in children(buf, sh_off + sh_hdr, sh_off + sh_hdr + sh_size):
        if s_id != 0x4DBB:
            continue
        seek_id = seek_pos_at = None
        for i_off, i_id, i_hdr, i_size in children(buf, s_off + s_hdr, s_off + s_hdr + s_size):
            if i_id == 0x53AB:
                seek_id = int.from_bytes(buf[i_off + i_hdr:i_off + i_hdr + i_size], "big")
            elif i_id == 0x53AC:
                seek_pos_at, seek_pos_len = i_off + i_hdr, i_size
        if seek_pos_at is None or seek_pos_len != 8:
            continue
        current = int.from_bytes(buf[seek_pos_at:seek_pos_at + 8], "big")
        new = tracks_pos if seek_id == TRACKS else (
            current + VOID_BYTES if current >= cluster_off - seg_start else current)
        if new != current:
            struct.pack_into(">Q", buf, seek_pos_at, new)
            moved += 1
    open(dst, "wb").write(bytes(buf))
    return tracks_pos, moved


def demux(path, push):
    """Pull mode is filesrc; push mode is an appsrc that answers seeks.

    Seekable-but-pushing is the case that matters: it is what a server
    reading its own storage looks like, and it is the one the demuxer
    refuses even though everything it needs is reachable.
    """
    pipeline = Gst.Pipeline.new(None)
    handle = open(path, "rb")
    if push:
        src = Gst.ElementFactory.make("appsrc")
        src.set_property("stream-type", 1)      # seekable, but pushing
        src.set_property("format", Gst.Format.BYTES)
        src.set_property("size", os.path.getsize(path))

        def need_data(element, length):
            chunk = handle.read(max(length, 4096))
            if chunk:
                element.emit("push-buffer", Gst.Buffer.new_wrapped(chunk))
            else:
                element.emit("end-of-stream")

        def seek_data(_element, offset):
            handle.seek(offset)
            return True

        src.connect("need-data", need_data)
        src.connect("seek-data", seek_data)
    else:
        src = Gst.ElementFactory.make("filesrc")
        src.set_property("location", path)
    demuxer = Gst.ElementFactory.make("matroskademux")
    for el in (src, demuxer):
        pipeline.add(el)
    src.link(demuxer)

    pads = []

    def on_pad(_d, pad):
        pads.append(pad.get_name())
        sink = Gst.ElementFactory.make("fakesink")
        sink.set_property("async", False)
        pipeline.add(sink)
        sink.sync_state_with_parent()
        pad.link(sink.get_static_pad("sink"))

    demuxer.connect("pad-added", on_pad)
    pipeline.set_state(Gst.State.PLAYING)
    msg = pipeline.get_bus().timed_pop_filtered(
        30 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR)
    error = msg.parse_error()[0].message if msg and msg.type == Gst.MessageType.ERROR else None
    pipeline.set_state(Gst.State.NULL)
    return len(pads), error


tmp = tempfile.mkdtemp()
plain = os.path.join(tmp, "plain.mkv")
shaped = os.path.join(tmp, "tracks-last.mkv")
subprocess.run(
    ["gst-launch-1.0", "-q",
     "videotestsrc", "num-buffers=48", "!",
     "video/x-raw,framerate=24/1,width=320,height=180", "!",
     "x264enc", "key-int-max=12", "!", "h264parse", "!",
     "matroskamux", "!", "filesink", "location=" + plain],
    check=True)
pos, moved = build_fixture(plain, shaped)
print(f"fixture: {os.path.getsize(shaped)} bytes, Tracks moved to segment offset {pos}, "
      f"{VOID_BYTES}-byte Void before the first Cluster, {moved} SeekHead entries corrected")

for mode, push in (("pull", False), ("push", True)):
    pads, error = demux(shaped, push)
    print(f"  {mode}: pads={pads}" + (f" — {error}" if error else ""))
    if push:
        verdict = (pads, error)

pads, error = verdict
if error or pads == 0:
    print("REPRODUCED: the same file demuxes in pull mode and is refused in push mode")
    sys.exit(0)
print("not reproduced — Tracks is fetched and the file demuxes (patched)")
sys.exit(1)
