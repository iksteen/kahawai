#!/usr/bin/env python3
"""How much of a non-interleaved MP4 must be read before both streams start?

Measures the cost of the FIRST BUFFER OF EVERY STREAM, pulled and pushed.
That is the moment anything downstream that muxes (an HLS sink, a
transport-stream muxer) can emit its first byte, and it is the thing a
non-interleaved file breaks.

Bytes, not seconds. "How much of the file must be read" is the same
number on any machine; wall clock only shows the problem on storage slow
enough or a file big enough — which is exactly how it hid.

    python3 0008-…-repro-1.py [outdir]

The file is generated here (videotestsrc + audiotestsrc through mp4mux
with interleaving disabled), so this needs no media and no network. The
layout it produces — all video, then all audio, moov last — is a real
one: it came from a 2007 retail MP4 in a personal library, where video
occupied 0 → 1.87 GB and the first audio sample sat at 97.9%.

Reading pushed, qtdemux hands samples out in byte order, so the first
audio sample arrives only after every video sample before it. Reading
pulled, it seeks. The ratio between the two is the pathology; run it
against each GStreamer release to see whether it is still there.
"""

import os
import subprocess
import sys
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

Gst.init(None)

OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp"
CLIP = os.path.join(OUT, "not-interleaved.mp4")
# One chunk per stream: mp4mux only starts a new chunk when an interleave
# limit is hit, so an unreachable limit writes each stream in one run.
NEVER = 2**64 - 1


def generate(path):
    """A ~29 MB MP4 whose audio starts at ~98% of the file."""
    if os.path.exists(path):
        return
    pipeline = (
        f"videotestsrc num-buffers=1800 ! video/x-raw,width=640,height=360,framerate=30/1 ! "
        f"x264enc bitrate=4000 key-int-max=60 ! h264parse ! "
        f"mp4mux name=m interleave-time={NEVER} interleave-bytes={NEVER} ! "
        f"filesink location={path} "
        f"audiotestsrc num-buffers=2900 ! audioconvert ! avenc_aac ! aacparse ! m."
    )
    subprocess.run(["gst-launch-1.0", "-q"] + pipeline.split(), check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def first_sample_offsets(path):
    """(video, audio) byte offset of each stream's first sample."""
    out = []
    for stream in ("v", "a"):
        r = subprocess.run(
            ["ffprobe", "-v", "error", "-select_streams", stream,
             "-read_intervals", "%+#1", "-show_entries", "packet=pos",
             "-of", "default=nw=1:nk=1", path],
            capture_output=True, text=True)
        out.append(int(r.stdout.strip() or -1))
    return tuple(out)


def rchar():
    """Bytes this process has read(), cache hits included — what the
    demuxer ASKED for, which is the property under test."""
    with open("/proc/self/io") as f:
        for line in f:
            if line.startswith("rchar:"):
                return int(line.split()[1])
    return 0


def measure(path, push):
    """Bytes and milliseconds until every pad has delivered one buffer."""
    src = f'filesrc location="{path}"' + (" ! queue" if push else "")
    pipeline = Gst.parse_launch(f"{src} ! qtdemux name=d")
    demux = pipeline.get_by_name("d")
    seen, sinks = set(), []

    def on_pad(_demux, pad):
        sink = Gst.ElementFactory.make("fakesink")
        sink.set_property("sync", False)
        pipeline.add(sink)
        sink.sync_state_with_parent()
        pad.link(sink.get_static_pad("sink"))
        sinks.append(sink)
        pad.add_probe(Gst.PadProbeType.BUFFER,
                      lambda p, _i, name=pad.get_name(): (seen.add(name),
                                                          Gst.PadProbeReturn.OK)[1])

    demux.connect("pad-added", on_pad)
    start_bytes, start = rchar(), time.monotonic()
    pipeline.set_state(Gst.State.PLAYING)

    bus, deadline = pipeline.get_bus(), start + 120
    while time.monotonic() < deadline:
        # Every pad that exists has produced something, and there are at
        # least two of them: the file's streams have all started.
        if len(sinks) >= 2 and len(seen) == len(sinks):
            break
        msg = bus.timed_pop_filtered(10 * Gst.MSECOND,
                                     Gst.MessageType.ERROR | Gst.MessageType.EOS)
        if msg:
            break
    cost = (rchar() - start_bytes, (time.monotonic() - start) * 1000, len(seen))
    pipeline.set_state(Gst.State.NULL)
    return cost


generate(CLIP)
size = os.path.getsize(CLIP)
v, a = first_sample_offsets(CLIP)
print(f"file      {CLIP}  ({size/2**20:.1f} MiB)")
print(f"layout    first video sample at {v:,}, first audio sample at {a:,} "
      f"({a * 100.0 / size:.1f}% into the file)")

results = {}
for mode in ("pull", "push"):
    read, ms, streams = measure(CLIP, push=(mode == "push"))
    results[mode] = read
    print(f"{mode:9s} {read/2**20:8.1f} MiB read, {ms:7.0f} ms, {streams} streams started")

# The verdict is about the FILE, not about pull: a build that fixes this
# still reads more pushed than pulled (it serves a couple of seconds of
# the leading stream before it seeks), and that is fine. What is not fine
# is reading to the far end of the file, because that cost grows with the
# file — 28 MiB here, 1.8 GiB on the title this came from.
share = results["push"] * 100.0 / size
ratio = results["push"] / max(results["pull"], 1)
print()
print(f"pushed, starting both streams cost {share:.0f}% of the file "
      f"({ratio:.0f}x the pulled figure).")
if share >= 50:
    print("AFFECTED: the read runs to the far end, so the wait grows with the "
          "file. Downstream that needs both streams — any muxer — waits it out.")
    sys.exit(1)
print("OK: the read is bounded by the lag threshold, not by the file size.")
