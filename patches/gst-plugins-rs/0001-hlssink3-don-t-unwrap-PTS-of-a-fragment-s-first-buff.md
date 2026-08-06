# hlssink3: process abort when a fragment's first buffer has no PTS

**Upstream:** merged as `80bfd7064` via gst-plugins-rs MR 3189, and
**released in `gstreamer-1.28.6`** (2026-08-05) — verified in the tagged
`net/hlssink3/src/hlssink3/imp.rs`, which carries the warning this patch
adds. Nothing applies it any more; the image pins 1.28.6 and the file is
kept as the record, with a reproducer that still answers "is your build
affected?".
**Observed on:** 0.15.3 (Arch `gst-plugin-hlssink3 0.15.3-1`), GStreamer
1.28.5. **Reproducer:** `…-repro-1.py` (videotestsrc only).

    thread '<unnamed>' panicked at net/hlssink3/src/hlssink3/imp.rs:304:62:
    called `Option::unwrap()` on a `None` value
    ...
    thread caused non-unwinding panic. aborting.

The `format-location-full` handler unwraps the PTS of the fragment's
first buffer. Because the panic happens inside an FFI callback it cannot
unwind and takes the whole process down (SIGABRT). Real-world trigger:
H.264 or MPEG-4 video demuxed from old AVI files, where avidemux emits
frames without PTS. hlssink2 muxes the same input without complaint.

The fix treats a missing PTS like the existing `buffer == None` branch:
warn, and store no running time.

## Sibling issue — carried here as 0000, and applied first

    thread '<unnamed>' panicked at net/hlssink3/src/hlsbasesink.rs:660:53:
    called `Option::unwrap()` on a `None` value

`imp.rs` stores `running_time = None` when the fragment sample has no
buffer, and `segment.to_running_time(pts)` can also return None (PTS
before segment start); the `hls-segment-added` emission then unwraps it.
Fixed upstream by `86d7e33cc` "hlsbasesink: Don't unwrap() running_time
when a segment is added" (Piotr Brzezinski, 2026-07-14), which is what
makes the branch above safe to take — so this patch is only correct on
top of it. No release carries it yet, so it is vendored beside this one
as `0000-hlsbasesink-…`: not ours, and applied first.

`…-repro-2.py` covers it, shifting running time negative with
`gst_pad_set_offset()`.

## State on this box

Neither fix is in the installed plugin: `0.15.3-6302bea23` predates both
(verified by ancestry). The aborts stay reachable here until the package
moves, so a 0.15 backport is still worth asking for.

kahawai no longer reaches either abort in any case. Both are triggered
by buffers with no PTS, and `a4c6990` stamps a missing PTS from the DTS
on the way INTO the parser chain, so nothing timestamp-less now reaches
the HLS sink — which is why the AVI that first produced these aborts
plays on a stock GStreamer.

## Running the reproducers

Both abort the whole process on an unpatched plugin, within a second:

    python3 0001-…-repro-1.py    [outdir]
    python3 0001-…-repro-2.py    [outdir] [-5]

Same failure family in other elements:
`patches/gstreamer/` — demuxers that work when pulled and fail when
pushed.
