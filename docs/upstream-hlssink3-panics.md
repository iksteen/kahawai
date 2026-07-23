# Draft upstream reports — gst-plugins-rs hlssink3 process aborts

File against gstreamer/gst-plugins-rs (GitLab). Version observed:
0.15.3 (Arch `gst-plugin-hlssink3 0.15.3-1`), GStreamer 1.28.5.
Reproducers: `scripts/repro/` (videotestsrc-only, no media files needed).

---

## Issue 1: abort on fragment-first buffer without PTS

**Title:** hlssink3: non-unwinding panic (process abort) when a
fragment's first buffer has no PTS

**Body:**

`format-location-full` handling unwraps the PTS of the fragment's first
buffer:

    thread '<unnamed>' panicked at net/hlssink3/src/hlssink3/imp.rs:304:62:
    called `Option::unwrap()` on a `None` value
    ...
    thread caused non-unwinding panic. aborting.

Because the panic happens inside an FFI callback it cannot unwind and
takes down the whole process (SIGABRT). Real-world trigger: H.264 or
MPEG-4 video demuxed from old AVI files (avidemux emits frames without
PTS). hlssink2 muxes the same input without issue.

Minimal reproducer (videotestsrc only) attached:
`hlssink3-fragment-pts-panic.py` — strips PTS from keyframes; aborts
within a second.

Suggested behavior: treat a missing PTS like the existing
`buffer == None` branch (warning + running_time = None) — though note
that path currently hits Issue 2.

## Issue 2: abort when fragment running time is None

**Title:** hlssink3: non-unwinding panic in hls-segment-added emission
when fragment running time is None

**Body:**

`imp.rs` stores `running_time = None` when the fragment sample has no
buffer, and `segment.to_running_time(pts)` can also return None (PTS
before segment start). The `hls-segment-added` message emission then
unwraps it:

    thread '<unnamed>' panicked at net/hlssink3/src/hlsbasesink.rs:660:53:
    called `Option::unwrap()` on a `None` value
    ...
    thread caused non-unwinding panic. aborting.

Same non-unwinding abort as Issue 1. Real-world trigger: streams with
broken timestamps where the fragment-opening buffer's PTS precedes the
segment start. hlssink2 handles identical input.

Minimal reproducer attached: `hlssink3-running-time-panic.py` — shifts
running time negative via `gst_pad_set_offset()`; aborts within a
second.
