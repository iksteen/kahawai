# hlsbasesink: abort when a fragment's running time is None

**Not ours** — the leading `0000` says so, and says it is applied
first. Upstream commit `86d7e33cc` by Piotr Brzeziński, 2026-07-14,
carried here only because releases do not have it yet: the newest
gst-plugins-rs release, `gstreamer-1.28.5` (tagged 2026-07-02),
predates it.

**Released in `gstreamer-1.28.6`** (2026-08-05), verified in the tagged
file rather than the changelog: `hlsbasesink.rs:660` reads
`.field("running-time", running_time)`, no unwrap. Nothing applies it
any more — the image pins 1.28.6 — and it is kept only as the record of
why 0001 needed it first.

It lives on this branch alone, not in `patches/` on master. That tree is
what we wrote and are submitting upstream; this is neither. It exists
because the image builds gst-plugins-rs from a release tag, and that tag
is missing a fix `0001` depends on — a packaging problem, not a bug we
found.

    thread '<unnamed>' panicked at net/hlssink3/src/hlsbasesink.rs:660:53:
    called `Option::unwrap()` on a `None` value
    ...
    thread caused non-unwinding panic. aborting.

`imp.rs` stores `running_time = None` when the fragment sample has no
buffer, and `segment.to_running_time(pts)` can also return None when the
PTS precedes the segment start. The `hls-segment-added` emission then
unwraps it, in an FFI callback that cannot unwind, so the whole process
goes down.

## Why it must be applied before 0001

`0001` fixes the sibling abort by taking exactly this branch — its
message says so: *"Handle it like the existing buffer-less branch: warn
and store no running time (which the hls-segment-added emission already
tolerates since 86d7e33)."*

Applying `0001` to a build without this one therefore trades one abort
for the other. They are a pair, in this order.

Reproducer: `0001-…-repro-2.py`, which shifts running time negative with
`gst_pad_set_offset()`.
