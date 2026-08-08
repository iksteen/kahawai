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
# Exits 0 when the plugin is fixed, 1 when the bug reproduces.
import os
import ctypes
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


def parser_result(path):
    """Exercise the parser directly when no device decoder is exposed.

    The patched structure is larger than the distro's introspection metadata,
    so using PyGObject here would allocate the old, too-small SliceHdr. Keep the
    NAL ABI explicit and give SliceHdr a deliberately oversized opaque buffer.
    This also makes the reproducer useful on headless CI runners.
    """

    class Nalu(ctypes.Structure):
        _fields_ = [
            ("ref_idc", ctypes.c_uint16),
            ("type", ctypes.c_uint16),
            ("idr_pic_flag", ctypes.c_uint8),
            ("size", ctypes.c_uint32),
            ("offset", ctypes.c_uint32),
            ("sc_offset", ctypes.c_uint32),
            ("valid", ctypes.c_int),
            ("data", ctypes.c_void_p),
            ("header_bytes", ctypes.c_uint8),
            ("extension_type", ctypes.c_uint8),
            ("extension", ctypes.c_uint8 * 6),
        ]

    if ctypes.sizeof(Nalu) != 40:
        raise RuntimeError("unsupported GstH264NalUnit ABI")

    lib = ctypes.CDLL("libgstcodecparsers-1.0.so.0")
    lib.gst_h264_nal_parser_new.restype = ctypes.c_void_p
    lib.gst_h264_parser_identify_nalu.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_uint8),
        ctypes.c_uint,
        ctypes.c_size_t,
        ctypes.POINTER(Nalu),
    ]
    lib.gst_h264_parser_parse_nal.argtypes = [ctypes.c_void_p, ctypes.POINTER(Nalu)]
    lib.gst_h264_parser_parse_slice_hdr.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(Nalu),
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_int,
    ]
    lib.gst_h264_nal_parser_free.argtypes = [ctypes.c_void_p]

    raw = open(path, "rb").read()
    data = (ctypes.c_uint8 * len(raw)).from_buffer_copy(raw)
    parser = lib.gst_h264_nal_parser_new()
    if not parser:
        raise RuntimeError("gst_h264_nal_parser_new() failed")

    offset = 0
    slices = 0
    failures = 0
    try:
        while offset < len(raw):
            nalu = Nalu()
            result = lib.gst_h264_parser_identify_nalu(
                parser, data, offset, len(raw), ctypes.byref(nalu)
            )
            # NO_NAL_END is valid for the final Annex-B NAL in this complete file.
            if result not in (0, 5):
                raise RuntimeError(f"identify_nalu failed at {offset}: {result}")
            if nalu.type in (7, 8):
                result = lib.gst_h264_parser_parse_nal(parser, ctypes.byref(nalu))
                if result != 0:
                    raise RuntimeError(f"parameter-set parse failed: {result}")
            elif nalu.type in (1, 5):
                # Patched GstH264SliceHdr is ~2 KiB on 64-bit platforms. The
                # extra room avoids importing stale struct sizes from the
                # system typelib while retaining canary-safe storage.
                slice_header = (ctypes.c_uint8 * 4096)()
                result = lib.gst_h264_parser_parse_slice_hdr(
                    parser, ctypes.byref(nalu), slice_header, 0, 1
                )
                slices += 1
                failures += result != 0

            next_offset = nalu.offset + nalu.size
            if next_offset <= offset:
                raise RuntimeError("identify_nalu made no progress")
            offset = next_offset
    finally:
        lib.gst_h264_nal_parser_free(parser)

    return slices, failures


tmp = tempfile.mkdtemp()
stream = os.path.join(tmp, "mmco.h264")
make_stream(stream)
print("fixture: %d bytes, %s" % (os.path.getsize(stream), DECODER))

decoder_available = subprocess.run(
    ["gst-inspect-1.0", DECODER], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
).returncode == 0
if not decoder_available:
    slices, failures = parser_result(stream)
    print("  direct parser       : %d slices, %d parse failures" % (slices, failures))
    if slices != FRAMES or failures:
        print("REPRODUCED: the parser rejected a full dec_ref_pic_marking")
        sys.exit(1)
    print("not reproduced — the parser accepts a full dec_ref_pic_marking (patched)")
    sys.exit(0)

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
    sys.exit(1)
print("not reproduced — the parser accepts a full dec_ref_pic_marking (patched)")
sys.exit(0)
