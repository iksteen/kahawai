# matroskademux: an element over 32 MiB is refused, however plainly it is there

**Upstream:** not submitted. Branch `matroskademux-accept-large-element`
on `gitlab.freedesktop.org/iksteen/gstreamer`. **Observed on:** GStreamer
1.28.5 (`gst-plugins-good`). **Reproducer:** `…-repro-1.py`, builds its
own fixture.

    matroska-demux.c(5650): gst_matroska_demux_check_read_size ():
    reading large block of size 35821506 not supported; file might be corrupt.

`MAX_BLOCK_SIZE` refuses any element over 32 MiB, and in streaming mode
the refusal is fatal — the whole difference is six lines in
`gst_matroska_demux_take()`:

```c
if (!demux->streaming) {
  /* in pull mode, we can skip */
  ...
} else {
  /* otherwise fatal */
  ret = GST_FLOW_ERROR;
}
```

Size alone is not evidence of corruption. Fansubbed releases attach
their subtitle fonts, and a CJK font pack passes 32 MiB without being
remarkable. What *does* distinguish a corrupt size is whether the
element could be there at all — and the demuxer already knows the file
length, since it consults it for SeekHead validation. The patch accepts
an element that ends inside the file, bounded by a ceiling so the
adapter is still bounded, and refuses everything else exactly as before.

## Evidence

One ordinary collection, 1108 Matroska files: 163 carry attachments — 12
over the limit (one release group's entire run, every one unplayable),
and 28 between 8 and 32 MiB. Releases approach the boundary routinely
rather than exceptionally, so the population above it grows as groups
add fonts.

Verified both directions, through a push-only appsrc so pull mode cannot
be reached:

- the affected file demuxes and the demuxer publishes **all 45
  attachments**, instead of dying;
- a file in the same library claiming a 128041197-byte element inside
  88.8 MB **still fails** — that one is genuinely damaged, and ffmpeg
  independently reports "exceeds containing master element".

## Scope: length, not seekability

The guard depends on upstream answering a BYTES duration query, not on
being seekable. Measured with `stream-type=0` (no pull, no seeks):

    length known (size set)      pads=3 attachments=45  no error
    length UNKNOWN (no size)     pads=3 attachments=0   Could not demultiplex

So a non-seekable source with a known length is covered — an HTTP server
sending Content-Length, or our appsrc, which always sets `size`. A live
stream of unknown length still refuses, unchanged: the patch is never
worse than current behaviour, it just does not reach that case. Covering
it too would need progressive skipping, which plays the file at the cost
of the attachments; that is a larger change and is not written.

kahawai has no local mitigation. Rewriting the oversized element as a
chain of sub-32 MiB `Void` elements in the byte source does work (tested:
0 segments → 24), but it is container-specific surgery that silently
discards the fonts, so it was not kept.

## Running the reproducer

Exits 0 when the bug reproduces, 1 when the plugin is fixed:

    python3 0003-…-repro-1.py [attachment MiB, default 33]
    GST_PLUGIN_PATH=/path/to/patched python3 0003-…-repro-1.py

It muxes a short MKV, splices an Attachments element in before the first
Cluster (correcting the Segment size), and pushes it through a push-only
appsrc, reporting how many attachments the demuxer published.
