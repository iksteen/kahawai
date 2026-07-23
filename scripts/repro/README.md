# hlssink3 panic reproducers (gst-plugins-rs ≤ 0.15.3)

Both scripts abort the whole process (a Rust panic in an FFI callback
cannot unwind). Found by sweeping a real media library; reduced to
videotestsrc-only pipelines. Run with GStreamer python bindings:

    python3 hlssink3-fragment-pts-panic.py    [outdir]
    python3 hlssink3-running-time-panic.py    [outdir] [-5]

1. **fragment-pts**: `net/hlssink3/src/hlssink3/imp.rs:304` unwraps the
   PTS of each fragment's first buffer. Streams with PTS-less frames
   (e.g. avidemux output for old AVIs) abort. Repro strips PTS from
   keyframes.
2. **running-time**: `net/hlssink3/src/hlsbasesink.rs:660` unwraps the
   running time stored by the imp.rs handler; when the fragment-first
   buffer's PTS maps to no running time (PTS before segment start),
   that value is None. Repro shifts running time negative via a pad
   offset.

Both inputs mux fine with hlssink2. Upstream report drafts in
`docs/upstream-hlssink3-panics.md`.
