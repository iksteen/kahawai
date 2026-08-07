# matroskademux: Tracks after the Clusters is refused, not fetched

**Upstream:** not submitted. Branch `matroskademux-tracks-after-clusters`
on `gitlab.freedesktop.org/iksteen/gstreamer` (second of two — needs
0006). **Observed on:** GStreamer 1.28.5 (`gst-plugins-good`).
**Reproducer:** `…-repro-1.py`, builds its own fixture.

    Could not demultiplex stream.
    File layout does not permit streaming

Matroska does not require Tracks before the Clusters, and a file that
puts it last says so in the SeekHead at the front. Pull mode acts on
that — `gst_matroska_demux_find_tracks()` goes and reads it. Streaming
gives up on the first Cluster:

```c
case GST_MATROSKA_ID_CLUSTER:
  if (G_UNLIKELY (demux->tracks_ebml_offset == G_MAXUINT64)) {
    if (demux->streaming) {
      GST_DEBUG_OBJECT (demux, "Cluster before Track");
      goto not_streamable;
```

So the same file plays from a `file://` URI and fails from a pipe, which
is how it presents: not as an unusual layout, but as corruption.

The fix follows a detour this element already makes. It remembers where
the SeekHead said the **Cues** are and, when a seek needs them, goes and
reads them and comes back. This does the same for Tracks — remember the
location, and on meeting a Cluster with nothing to put it in, seek there
and resume. Only when upstream is seekable, and only once: if the seek
lands somewhere without Tracks, the file really cannot be read this way
and the old error stands.

## Evidence

One affected file, top level:

    EBML @0
    Segment @40
      SeekHead @52        -> Info 161, Chapters 2950, Cues 1684885225,
                             Tags 1684924532, Tracks 1684925972
      Void @143
      Info @213
      Void @360
      Chapters @3002
      Void @3188
      Cluster @4458       <- streaming stops here
      …

Tracks is at the very end of a 1.68 GB file, and the SeekHead at byte 52
says so.

**6 files of ~37 000** in one library sweep failed this way, every one
playable with ffmpeg and through this element in pull mode. With both
patches, all six demux and sweep `OK`.

The seekability requirement is what makes this safe: a genuinely
non-seekable stream — a pipe, a live feed — still gets the old error,
because nothing can reach the Tracks in that case either.

## Why 0006 is required

The resume offset is `demux->common.offset`, and without 0006 that value
is wrong on exactly the files this patch targets: a large Void ahead of
the first Cluster makes the demuxer skip it twice in its accounting. The
detour then resumes 1270 bytes inside a Cluster and resyncs through the
rest of the file. Measured before 0006: `have Tracks, resuming at 5728`
for a Cluster that starts at 4458.

## Running the reproducer

Exits 0 when the plugin is fixed, 1 when the bug reproduces:

    python3 0007-…-repro-1.py
    GST_PLUGIN_PATH=/path/to/patched python3 0007-…-repro-1.py

It muxes an ordinary Matroska file, replaces the Tracks element with a
Void of the same size, appends Tracks at the end, corrects the Segment
size and the SeekHead positions, and inserts a 64 KiB Void before the
first Cluster — so the fixture needs both patches, like the real files.
It then demuxes twice: from `filesrc`, and through an `appsrc` that
answers seeks, which is what a server reading its own storage looks
like.
