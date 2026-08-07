# avidemux: a zero-sized JUNK before the header is fatal in push mode

**Upstream:** not submitted. Branch `avidemux-skip-leading-junk` on
`gitlab.freedesktop.org/iksteen/gstreamer`. **Observed on:** GStreamer
1.28.5 (`gst-plugins-good`). **Reproducer:** `…-repro-1.py`, builds its
own fixture.

    gstavidemux.c(5932): gst_avi_demux_chain (): unhandled buffer size

`gst_riff_read_chunk()` skips JUNK/JUNQ chunks automatically, so an AVI
that pads before the `hdrl` LIST demuxes normally while avidemux is
pulling. Push mode parses the headers off the adapter instead, through
`gst_avi_demux_peek_chunk()`, which has no such skip and additionally
refuses a zero-sized chunk by setting `abort_buffering`. The
header-state caller has no escape hatch, so the file dies before a
single byte of media is parsed — and only when the source pushes:

    filesrc location=junk0.avi ! queue ! avidemux ! fakesink   -> error
    filesrc location=junk0.avi !         avidemux ! fakesink   -> plays

Zero-sized JUNK chunks *inside* the movi list are already tolerated
explicitly ("accept 0 size buffer here", `bb2b02c5b7`); the patch
extends the same tolerance to padding that precedes the header. Chunks
of 1 GiB or more are still left for `gst_avi_demux_peek_chunk()` to
reject, rather than waiting for the adapter to hold one.

## Evidence

4 of 1872 AVI files in one ordinary library start this way — all
unplayable, all fine through filesrc. The affected files produce 42, 24
and 41 segments respectively through kahawai's worker once patched.

kahawai has no local mitigation for this one: the byte plane always
pushes, so the patch (or an updated package) is the only fix.

## Running the reproducer

Exits 0 when the plugin is fixed, 1 when the bug reproduces, so it works
as a before/after check:

    python3 0001-…-repro-1.py
    GST_PLUGIN_PATH=/path/to/patched python3 0001-…-repro-1.py

It muxes a short AVI with avimux, splices `JUNK` + a zero size field in
after the `AVI ` form type (adding 8 to the RIFF size field; idx1 offsets
are relative to the movi list and stay valid), then feeds it through a
push-only appsrc — `stream-type=0`, so pull mode cannot be reached.
