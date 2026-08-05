#!/usr/bin/env python3
# Reproduces silent picture corruption in every gst-plugins-bad decoder
# built on the shared H.264 parser (1.28.5 and earlier):
#
#   gsth264parser.h:  GstH264RefPicMarking ref_pic_marking[10];
#   gsth264parser.c:  if (n_ref_pic_marking >= G_N_ELEMENTS (...)) goto error;
#
# A slice header carrying more than ten memory management control
# operations fails to parse. The whole header is discarded, including
# the reference marking the stream just asked for, so the decoder keeps
# pictures the encoder evicted and every following P/B frame predicts
# from the wrong references. Decoding stays wrong until the next IDR —
# in an open GOP, forever. Nothing is reported: no bus error, no
# warning, only wrong pixels.
#
# H.264 sets no such bound (7.3.3.3); a conforming stream may evict its
# whole reference set at once, and x264 does exactly that with
# ref=16 and open-gop=1. ffmpeg's limit is 66, which is why the same
# file decodes correctly there — including through NVDEC.
#
#   python3 0004-…-repro-1.py [decoder, default nvh264dec]
#
# Exits 0 when the bug reproduces, 1 when the plugin is fixed.
import os
import subprocess
import sys
import tempfile

DECODER = sys.argv[1] if len(sys.argv) > 1 else "nvh264dec"
W, H, FRAMES = 320, 240, 400


def make_stream(path):
    """x264 with a full reference set and open GOPs evicts 15 references
    in one non-IDR slice header — more than the parser can hold."""
    subprocess.run(
        ["gst-launch-1.0", "-q",
         "videotestsrc", f"num-buffers={FRAMES}", "pattern=snow", "!",
         f"video/x-raw,format=I420,width={W},height={H},framerate=24/1", "!",
         "x264enc", "ref=16", "bframes=4", "key-int-max=50",
         "option-string=open-gop=1", "!",
         "video/x-h264,stream-format=byte-stream,alignment=au", "!",
         "filesink", "location=" + path],
        check=True)


def framemd5_reference(path):
    out = subprocess.run(
        ["ffmpeg", "-v", "error", "-f", "h264", "-i", path, "-map", "0:v:0",
         "-an", "-pix_fmt", "yuv420p", "-f", "framemd5", "-"],
        capture_output=True, text=True).stdout
    return [l.split()[-1] for l in out.splitlines() if l and not l.startswith("#")]


def framemd5_gst(path, decoder):
    gst = subprocess.Popen(
        ["gst-launch-1.0", "-q", "filesrc", "location=" + path, "!", "h264parse", "!",
         decoder, "!", "videoconvert", "!", "video/x-raw,format=I420", "!", "fdsink", "fd=1"],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    ff = subprocess.run(
        ["ffmpeg", "-v", "error", "-f", "rawvideo", "-pix_fmt", "yuv420p",
         "-s", f"{W}x{H}", "-i", "-", "-f", "framemd5", "-"],
        stdin=gst.stdout, capture_output=True, text=True)
    gst.wait()
    return [l.split()[-1] for l in ff.stdout.splitlines() if l and not l.startswith("#")]


tmp = tempfile.mkdtemp()
stream = os.path.join(tmp, "mmco.h264")
make_stream(stream)
print("fixture: %d bytes, %s" % (os.path.getsize(stream), DECODER))

ref = framemd5_reference(stream)
got = framemd5_gst(stream, DECODER)
n = min(len(ref), len(got))
bad = [i for i in range(n) if ref[i] != got[i]]

print("  libavcodec reference : %d frames" % len(ref))
print("  %-20s : %d frames, %d differing%s"
      % (DECODER, len(got), len(bad),
         ", first at frame %d" % bad[0] if bad else ""))

if bad or len(got) != len(ref):
    print("REPRODUCED: H.264 decoding is bit-exact by spec, so any difference is wrong output")
    sys.exit(0)
print("not reproduced — the parser accepts a full dec_ref_pic_marking (patched)")
sys.exit(1)
