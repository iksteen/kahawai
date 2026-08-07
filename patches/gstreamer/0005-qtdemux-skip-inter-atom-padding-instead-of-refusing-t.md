# qtdemux: eight bytes of nothing condemn the file, but only when it is fed

**Upstream:** not submitted. Branch `qtdemux-skip-inter-atom-padding` on
`gitlab.freedesktop.org/iksteen/gstreamer`. **Observed on:** GStreamer
1.28.5 (`gst-plugins-good`). **Reproducer:** `…-repro-1.py`, builds its
own fixture.

    This file is invalid and cannot be played.
    atom .... has bogus size 18446744073709551615

Some muxers align the atom that follows by leaving eight zero bytes that
no atom covers. ISO base media has no way to describe a gap belonging to
nobody, so the walk reads a size field of 0 — which means "to the end of
the file", and `extract_initial_length_and_fourcc` turns into
`G_MAXUINT64` — together with a fourcc of 0, and the size check rejects
the whole file.

The fix is to recognise it: no atom has a zero type, so eight zero bytes
cannot be confused with content. Step over them.

## Why it looks like corruption

**Only the streaming path meets it.** Pull mode stops walking once it
has the moov and reads by the sample tables, so it never visits the
padding. Push mode walks every byte. The same file therefore plays from
a `file://` URI and fails from a pipe — which is exactly how it presents
in a player that reads files directly and a server that streams them.

Measured on one affected file, with nothing else changed:

    filesrc ! qtdemux            demuxes
    filesrc ! queue ! qtdemux    atom .... has bogus size …

## Evidence

Sweeping a library through a push-only source: **25 files of ~37 000**
failed this way, every one of them from a single release group, every
one playable with ffmpeg and through qtdemux in pull mode. The layout of
one, read off the file:

    ftyp @0          32
    moov @32         2742700
    free @2742732    3257300      ends at 6000032
    ????? @6000032   0            eight zero bytes
    mdat @6000040    475895943

The `free` atom's size is eight bytes short of the space it occupies, so
`mdat` begins eight bytes after the walk expects it. ffmpeg scans
forward and recovers; qtdemux in pull mode never looks.

With the patch, all 25 sweep `OK`, and an mp4 that already passed is
unaffected.

## Testing

`gst-plugins-good`'s own `elements_qtdemux` suite passes with the patch.
(The `pipelines_simple_launch_lines` failure in the same run is
`test_rtp_payloaders` and `test_videomixer` — plugins an
`auto_features=disabled` build does not contain.)

## Running the reproducer

Exits 0 when the plugin is fixed, 1 when the bug reproduces:

    python3 0005-…-repro-1.py
    GST_PLUGIN_PATH=/path/to/patched python3 0005-…-repro-1.py

It muxes an MP4 with `faststart`, inserts eight zero bytes before `mdat`
and adds 8 to every chunk offset in `stco` — so the file is correct in
every respect except the orphan padding — then demuxes it twice, once
from `filesrc` and once through a push-only `appsrc`.
