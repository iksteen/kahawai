# codecparsers/h264: ten MMCO commands is below what the standard allows

**Upstream:** not submitted. Branch `h264parse-mmco-limit` on
`gitlab.freedesktop.org/iksteen/gstreamer`. **Observed on:** GStreamer
1.28.5. **Reproducer:** `…-repro-1.py`, builds its own fixture.

```c
GstH264RefPicMarking ref_pic_marking[10];                    /* gsth264parser.h */
if (n_ref_pic_marking >= G_N_ELEMENTS (ref_pic_marking))
    goto error;                                              /* gsth264parser.c */
```

A slice header carrying more than ten memory management control
operations fails to parse, and the *whole header* is discarded with it —
including the reference marking the stream just asked for. The decoder
then keeps pictures the encoder evicted, every following P/B frame
predicts from the wrong references, and nothing resets it: in an open
GOP there is no IDR to recover on, so the picture stays wrong to the end
of the file.

It is silent. The parse failure reaches the pipeline as neither error
nor warning — only as wrong pixels.

The standard sets no such bound (7.3.3.3). What bounds the loop in
practice is the reference set the operations can address: up to 16
short-term and 16 long-term pictures, plus the operations that set the
long-term maximum and mark the current picture. ffmpeg's equivalent
limit is 66.

## Evidence

A fansubbed release (`num_ref_frames=16`, open GOP) evicts **15**
short-term references in one non-IDR slice header when it starts a new
GOP. Through GStreamer's own parser, 750 slices of that file:

    ref_pic_marking[10]   7 parse failures, first at slice 250
    ref_pic_marking[34]   0 failures, 15 operations parsed

and through `nvh264dec`, compared with libavcodec frame by frame:

    before   692 frames, 442 differing, wrong from frame 250 onward
    after    699 frames,   0 differing — bit-exact

The seven frames the decoder dropped are the seven headers it could not
parse. Same GPU and driver, `ffmpeg -c:v h264_cuvid` is bit-exact
throughout, because it hands the bitstream to the driver's own parser.

Ruled out along the way, each by measurement: the NVIDIA driver, a
recent `gst-plugins-bad` package update (1.28.5-2 fails identically),
`num-output-surfaces` and `max-display-delay`, mid-stream SPS/PPS
repetition (removing it changes nothing), and decoder re-initialisation
(`new_sequence` fires once, at startup).

## Scope

The array is in the **shared** codecparser, so every stateless decoder
built on it inherits the ceiling — `vah264dec`, `d3d11h264dec`,
`v4l2slh264dec` and `nvh264dec` alike. NVDEC is simply what this box
uses. `avdec_h264` is unaffected: libavcodec has its own parser.

Not every file trips it. An ordinary encode never evicts more than one
or two references at a time; a comparison title in the same library uses
MMCO just as heavily (301 operations against 298) but never bursts above
2, and decodes bit-exact.

## ABI

Growing the array changes the size of the public
`GstH264DecRefPicMarking`, and of `GstH264SliceHdr`, which embeds it by
value. Every decoder compiled against the old header must be rebuilt, so
this targets `main` (hence `Since: 1.30`) and cannot be backported to a
stable branch as it stands. A backportable fix would have to handle the
overflow without changing the layout — a different, uglier patch.

Anything shipping this patch must rebuild **all** consumers of the
struct together, or the mismatch is worse than the bug.

## Running the reproducer

Exits 0 while the bug is present, 1 once the plugin is fixed:

    python3 0004-…-repro-1.py            # defaults to nvh264dec
    python3 0004-…-repro-1.py vah264dec  # any stateless decoder

It encodes with `x264enc ref=16 open-gop=1`, which evicts a full
reference set in one slice header, then compares the decoder's output
against libavcodec frame by frame. H.264 decoding is bit-exact by spec,
so any difference is wrong output, not a quality trade.
