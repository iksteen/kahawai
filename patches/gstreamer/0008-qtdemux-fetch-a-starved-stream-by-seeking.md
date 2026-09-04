# qtdemux: a non-interleaved MP4 delivers nothing until the whole file is read

**Upstream:** not submitted yet. Branch `qtdemux-starved-stream-seek` on
the fork, based on `main`; the patch here is the same commit against
1.28.5, which is what the measurements below ran on.
**Observed on:** GStreamer 1.28.5, and unchanged in `main` as of
2026-08-06. **Reproducer:** `…-repro-1.py` (videotestsrc + audiotestsrc,
no media needed).

    session start: remux produced no playlist in time
    sweep:         [hang] no output for 120s

A retail *Transformers (2007).mp4*, 1.79 GiB, plays in a browser and
fails in every path that runs a GStreamer pipeline — copy and encode
alike. The file is not interleaved:

    video packets start at offset           32   (mdat #1, 0 → 1.87 GB)
    audio packets start at offset 1,875,896,971   (mdat #2, at 97.9%)

All the video, then all the audio, `moov` last.

## Why it stalls

Pushed, `next_entry_size()` serves the stream whose next sample has the
smallest byte offset, so the read walks the file front to back. With
this layout that means every video sample is delivered before the first
audio sample exists. Anything downstream that muxes — an HLS sink, a
transport-stream muxer — holds its first output until every pad has
data, so it holds it for 1.87 GB.

Pulled, the same file starts immediately: each stream is read where it
lives. Measured on the title above, delivering 2000 video frames:

    pull    44 MiB read,   2.9 s
    push  1746 MiB read,  99.1 s

That 40x is not a slow code path, it is 40x the bytes. The demuxer is
doing exactly what it was told to do.

**What it is not.** Three plausible causes were measured and are
innocent. There is no seek storm: qtdemux issues three seeks on this
file, and they are the right ones (mdat #2's header, `moov`, then back
to 24). It is not the trailing `moov` by itself: a 1.51 GiB faststart
file with the same tooling demuxes 2000 frames in 1.1 s pushed. And it
is not a gap qtdemux could skip — it wants every byte between here and
there, so the existing "jump over the atom with a seek" logic has
nothing to skip.

## The fix

Push mode already knows how to seek — it does it to find headers, gated
on `upstream_seekable`. Use the same ability during playback: notice
the stream that has fallen behind in decode time, and go and get it.

    QTDEMUX_PUSH_SEEK_LAG       2 s     larger than any sane interleave
                                        (mp4mux defaults to 250 ms), so
                                        a well-muxed file never reaches it
    QTDEMUX_PUSH_SEEK_DISTANCE  4 MiB   data closer than this arrives on
                                        its own; the same guard the
                                        header-hunting seek uses

Three parts, and the third is the one that cost an evening:

1. `qtdemux_push_starved_stream()` returns the offset of the stream
   furthest behind, or FALSE — which is the answer for every interleaved
   file, i.e. nearly all of them.
2. `next_entry_size()` passes over a stream whose next sample lies
   *behind* the read position instead of selecting it, finding no entry
   at the current offset, and ending the file early. Gated on the seek
   being available, so a non-seekable upstream keeps today's behaviour.
3. The segment that answers our own seek must not be treated as "start
   again from here". That path calls `gst_qtdemux_find_sample(…,
   set=TRUE)`, which repositions every stream to the new byte offset and
   marks as EOS any stream with no samples there — precisely the stream
   just seeked away from. Without this the demuxer switches to audio
   once, EOSes video, and streams 190,974 audio packets into a single
   72 MB segment. The seek's seqnum identifies its own segment.

## Numbers

Reproducer, 27.5 MiB synthetic file, audio starting at 97.9%, cost of
starting *both* streams:

    stock    27.6 MiB read — 100% of the file   AFFECTED (exit 1)
    patched  6-7 MiB read  —  23-26% over runs   OK      (exit 0)
    pulled    0.1 MiB read

The remaining quarter is the lag threshold: two seconds of the leading
stream before the first switch. It does not grow with the file, which is
the whole point — the 1.79 GiB title went from **no output in 120 s** to
**3057 segments in 105 s**, each carrying both streams (69 video / 70
audio packets in the first, 250/244 by the tenth).

Regression: 90 series files swept with the patch produce the same
verdicts as without it — one FAIL, the same corrupt file (89% zero
bytes) that failed before.

## Running the reproducer

    python3 0008-…-repro-1.py [outdir]

It mixes its own file, prints what starting both streams costs pulled
and pushed, and exits non-zero when the pushed read runs to the far end of the
file. That is the check to re-run on each GStreamer release.

Exits 0 when the plugin is fixed, 1 when the bug reproduces — the same
way as every other reproducer here. It used to be the other way round,
alone among the nine, which made a checker that ran them all report this
patch as missing precisely when it was working.

## Amendment 2026-09-04: the flush wiped a seeked run's timeline

The fetch seek carries `GST_SEEK_FLAG_FLUSH`, and qtdemux's `FLUSH_STOP`
handler answers any flush with `gst_qtdemux_reset (demux, FALSE)`, which
re-inits `demux->segment` and keeps only its duration. That is written
for the header-hunting seek, which happens before a timeline exists.

Sent mid-playback on a run that was STARTED at an offset, it discards the
segment start. The branch above then recognises the answering segment as
our own and deliberately declines to publish a new one — so nothing ever
restores what the flush erased, and the next pending-segment check
republishes every stream at zero. Buffer timestamps do not change, only
the segment converting them to running time, so running time silently
becomes absolute media time and every clock derived from it moves by
exactly the offset the run resumed at.

Measured against a hub session resumed 18 minutes into a 98-minute MP4
with an embedded VobSub track — sparse enough to make the fetch seek fire
repeatedly. The produced HLS timeline stepped by 1082 s, the resume
offset, eleven segments in; a client's position, its subtitles and the
progress it saved were all that far ahead, with no visible skip in the
picture because the content itself stayed continuous.

The fix keeps the whole segment across the reset when the flush answers
our own fetch seek, rather than only the duration. There is no new
timeline to describe: the seek moved the read position, and its flush is
already dropped before it reaches downstream, so downstream's segment is
still the right one.

    before: 13 segments, PTS 3600.000s -> 4693.134s (step +1082.123s)
    after: 388 segments, PTS 3600.000s -> 3987.387s, continuous

An earlier attempt at this, `0010-qtdemux-preserve-timeline-across-
unmapped-byte-segments`, has been dropped. It guarded the byte-to-time
mapping further down the same handler, which is a real weakness — a byte
offset that maps to an early sample, or to none, still re-bases the
timeline there — but instrumentation showed that path is never reached
for these seeks, because the branch above exits first. It fixed its own
synthetic reproducer and did nothing for the case that prompted it.

## Also seen, not diagnosed

`filesrc ! queue ! qtdemux ! two fakesinks` hangs on this layout — with
and without this patch, so it is not caused by it and is not fixed by
it. It reproduces with the file the reproducer generates.
