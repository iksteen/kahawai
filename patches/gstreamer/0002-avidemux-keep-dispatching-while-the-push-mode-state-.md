# avidemux: one state per buffer, so a small file never leaves the header

**Upstream:** not submitted. Branch `avidemux-push-state-dispatch` on
`gitlab.freedesktop.org/iksteen/gstreamer`. **Observed on:** GStreamer
1.28.5 (`gst-plugins-good`). **Reproducer:** `…-repro-1.py`, builds its
own fixture.

    gstavidemux.c(895): gst_avi_demux_handle_sink_event ():
    got eos and didn't receive a complete header object

`gst_avi_demux_chain()` handles exactly one state per buffer: START
parses the RIFF header and returns, HEADER waits for the next buffer. A
source that delivers the whole file in a single buffer therefore never
leaves the header, and the file is rejected at EOS as truncated although
every byte of it arrived.

Buffer segmentation is not something a source has to get right for a
demuxer, and nothing else about avidemux depends on it. The patch loops
while the state changes, so a buffer carrying several states' worth of
data is parsed in one go.

## Evidence

Measured with a bare appsrc and no other element involved — the same
1.5 MB AVI, the same bytes:

    pushed as 1 buffer  -> Could not demultiplex stream, 0 pads
    pushed as 2 buffers -> 2 pads, no error

filesrc hides it by operating in pull mode. Any pushing source whose
read block exceeds the file size hits it.

## State in kahawai

kahawai no longer triggers this: `f6366bf` feeds each read to the
pipeline as slices of at most 256 KiB, so a source smaller than the
2 MiB read block is never one buffer. That is a workaround for our
source, not a fix — the commit says so — and it is why this patch still
matters for everyone else.

## Running the reproducer

Exits 0 when the plugin is fixed, 1 when the bug reproduces:

    python3 0002-…-repro-1.py
    GST_PLUGIN_PATH=/path/to/patched python3 0002-…-repro-1.py

It muxes a short AVI, then pushes the identical bytes twice through a
push-only appsrc — once as a single buffer, once as two — so the only
variable is the buffer count.
