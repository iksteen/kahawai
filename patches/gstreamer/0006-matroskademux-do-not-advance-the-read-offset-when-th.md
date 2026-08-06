# matroskademux: the read offset moves even when the skip does not

**Upstream:** not submitted. Branch `matroskademux-tracks-after-clusters`
on `gitlab.freedesktop.org/iksteen/gstreamer` (first of two).
**Observed on:** GStreamer 1.28.5 (`gst-plugins-good`). **Reproducer:**
shared with 0007 — `0007-…-repro-1.py`, which only passes with both.

```c
demux->common.offset += flush;              /* moved first … */
if (demux->streaming) {
  ...
  if (flush <= gst_adapter_available (demux->common.adapter))
    gst_adapter_flush (demux->common.adapter, flush);
  else
    return GST_FLOW_EOS;                    /* … and nothing was dropped */
}
```

`gst_matroska_demux_flush()` advances the read offset before it knows
whether the bytes can go. When the element to skip is larger than the
adapter currently holds, it returns `GST_FLOW_EOS`, the caller waits for
more data and reads **the same element again** — with the offset already
past it.

Every offset after such an element is then wrong by that element's size.

## Evidence

A file with a 1261-byte Void before its first Cluster, traced through
the demuxer's own chain function:

    offset=3188  id=0xec        size=1261     Void
    offset=4458  id=0xec        size=1261     the SAME Void, read again
    offset=5728  id=0x1f43b675  size=1704617  Cluster

The Cluster is at 4458. The demuxer thinks it is at 5728 — exactly 1270
bytes (9 header + 1261 body) too far.

Nothing complains, because in streaming mode the offset is mostly
bookkeeping. It matters the moment something seeks by it: the seek lands
inside a Cluster and the parser resyncs through the rest of the file.
That is how it surfaced — the Tracks detour in 0007 resumed at 5728 and
spent the rest of the file looking for a cluster boundary.

## The fix

Advance the offset once the adapter has actually given up the bytes.
Pull mode never enters that branch and is unchanged.

## Testing

`gst-plugins-good`'s `elements_matroskademux`, `elements_matroskaparse`
and `elements_matroskamux` suites pass. (The
`pipelines_simple_launch_lines` failure in the same run is
`test_rtp_payloaders` and `test_videomixer` — plugins an
`auto_features=disabled` build does not contain.)
