#!/usr/bin/env python3
# Reproduces matroskademux killing a file over damage INSIDE an element,
# where the very same demuxer walks through it in pull mode:
#
#   Could not demultiplex stream. (matroska-demux.c: gst_matroska_demux_parse_id
#   (): Failed to parse Element 0xe7)
#
# Push mode already resyncs — it scans on for the next Cluster when an
# element's ID/length header will not read at all. It never gets there
# when the header reads FINE and the contents do not: a corrupt region
# can present a plausible id and a plausible length, and the failure then
# lands on a fatal path instead of the recovering one.
#
# This fixture writes a valid file and then overwrites one Cluster
# Timestamp's body with a length that no unsigned integer may have (EBML
# allows at most 8 bytes), leaving the id and the length field readable.
# That is what a hole in a download looks like from the parser's side,
# and it is what the affected library file does at every 1 MiB boundary.
#
#   python3 0009-…-repro-1.py
#   GST_PLUGIN_PATH=/path/to/patched python3 0009-…-repro-1.py
#
# Exits 0 when the plugin is fixed, 1 when the bug reproduces. Builds its
# own fixture — no media needed.
import os
import struct
import subprocess
import sys
import tempfile

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

Gst.init(None)

SEGMENT, CLUSTER, TIMESTAMP = 0x18538067, 0x1F43B675, 0xE7
SECONDS = 4


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


def mux(path):
    """A plain Matroska file with several Clusters."""
    pipeline = (
        f"videotestsrc num-buffers={SECONDS * 25} ! video/x-raw,framerate=25/1 "
        f"! x264enc key-int-max=25 speed-preset=ultrafast ! queue "
        f"! matroskamux name=m max-cluster-duration=500000000 "
        f"! filesink location={path} "
        f"audiotestsrc num-buffers={SECONDS * 50} ! audioconvert ! avenc_aac ! queue ! m."
    )
    subprocess.run(["gst-launch-1.0", "-q"] + pipeline.split(),
                   check=True, capture_output=True)


def damage(path):
    """Give the SECOND cluster's Timestamp a body no uint may have.

    The id and the length field stay readable on purpose: this is the
    case the header-level resync cannot see, because nothing about the
    header is wrong.
    """
    buf = bytearray(open(path, "rb").read())
    seg = next(k for k in children(buf, 0, len(buf)) if k[1] == SEGMENT)
    kids = list(children(buf, seg[0] + seg[2], seg[0] + seg[2] + seg[3]))
    clusters = [k for k in kids if k[1] == CLUSTER]
    if len(clusters) < 2:
        sys.exit("fixture has too few clusters; raise SECONDS")

    c_off, _, c_hdr, c_size = clusters[1]
    ts = next(k for k in children(buf, c_off + c_hdr, c_off + c_hdr + c_size)
              if k[1] == TIMESTAMP)
    ts_off = ts[0]
    # The id and the one-byte length field stay perfectly readable; the
    # length they announce is 125, which no unsigned integer may be —
    # EBML allows at most 8. Nothing is inserted: the element simply
    # claims the bytes that follow it, which is what a parser sees when a
    # hole has eaten the real header. The bytes it swallows are the
    # cluster's own, so the file stays exactly as long as it was.
    body = 125
    if ts_off + 2 + body > len(buf):
        sys.exit("fixture too small to damage; raise SECONDS")
    buf[ts_off:ts_off + 2] = bytes([TIMESTAMP, 0x80 | body])
    open(path, "wb").write(buf)
    return body


def demux_pushed(path):
    """Feed the file through appsrc — push mode, seekable.

    Pads are linked as they appear: matroskademux's are sometimes-pads,
    so naming them in a parse_launch string links nothing and the file is
    never demuxed at all — which reads exactly like a pass.
    """
    pipeline = Gst.Pipeline.new(None)
    handle = open(path, "rb")
    src = Gst.ElementFactory.make("appsrc")
    src.set_property("stream-type", 1)          # seekable, but pushing
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
    err = None
    if msg and msg.type == Gst.MessageType.ERROR:
        gerror, dbg = msg.parse_error()
        err = f"{gerror.message} ({dbg.splitlines()[-1].strip() if dbg else ''})"
    pipeline.set_state(Gst.State.NULL)
    return len(pads), err


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="mkv-resync-")
    os.makedirs(out, exist_ok=True)
    path = os.path.join(out, "damaged.mkv")

    mux(path)
    body = damage(path)
    print(f"fixture: {path} ({os.path.getsize(path)} bytes), "
          f"cluster 2 Timestamp body forced to {body} bytes")

    pads, err = demux_pushed(path)
    # No pads means nothing was demuxed, so neither verdict is evidence.
    if pads == 0:
        sys.exit("harness broken: no pads appeared, the file was never demuxed")
    if err is None:
        print(f"OK       pushed to EOS on {pads} pads — resynced past the damage")
        return 0
    print(f"AFFECTED pushed run failed on {pads} pads: {err}")
    print("         (pull mode reaches search_cluster() and survives the same file)")
    return 1


if __name__ == "__main__":
    sys.exit(main())
