# HLS VOD delivery — plan, cost model, and why it is not scheduled

**Status: not scheduled.** This records what a VOD playlist would take,
what it would buy, and what it would cost, so the decision can be made
on evidence rather than re-derived. Nothing here is committed work.
Written 2026-08-05, after the ExoPlayer liveness claim in
`kahawai-implementation.md` §4.6 was corrected.

## What we deliver today

The playlist is EVENT and grows as the pipeline produces:
`fmp4sink.rs` writes it, `remux.rs` sets `playlist-type=event` on
hlssink3, `EXT-X-ENDLIST` lands only at EOS. Three properties follow:

- **A seek is a server-side restart.** `Sessions::execute_seek` removes
  the output directory and re-rolls the pipeline from the keyframe at
  or before the target. The playlist's t=0 is that seek point, so the
  client carries the mapping itself — `offset` in `Picture.vue`, with
  subtitle cues shifted by `-offset` and the overlay's `timeOffset`.
- **A start is gated on runway.** `playlist_ready` waits for 6.5–30 s
  of content, because an EVENT client discovers segments only by
  reloading the playlist.
- **Production is paced to `viewer + 120 s`** (`worker.rs`), so once
  the pipeline runs ahead, the playlist stops changing.

### The liveness problem, stated correctly

ExoPlayer raises `PlaylistStuckException` after 3.5 × targetDuration of
no change, and the exception surfaces through the *loading* path — a
player sitting on a full buffer never asks for it, so **pausing does
not trip it** (source-validated 2026-08-05; an earlier draft of §4.6
claimed otherwise and that error is what made VOD look mandatory).

What remains is a bounded race: a *playing* client whose playlist
stops changing for longer than the threshold. The window is bounded by
how fast the viewer drains the 120 s pacing window — but the two
measurements we have, 42 s (copy) and 47 s (transcode), sit right on
the 42 s a declared target of 12 buys. Both were taken **with no
progress pings**, which is the shape of the failure: a client that
never reports its position leaves `viewer.pos` at the floor, so the
pacer freezes production and the playlist with it.

That race has a cheaper fix than VOD, and it is being done separately:
release the pacer on playlist age as well as viewer position.

## What VOD actually requires

Four properties, in order of difficulty:

1. **The complete segment list before the first byte is served.** Exact
   duration, the segmentation rule, and — for a copy — the real
   keyframe times. We store only `max_keyframe_interval_ms`
   (`core::media`), derived from a container index the scan already
   reads, so the full list is an extension of an existing read for
   MKV/MP4/AVI. Containers without an index cannot be listed without a
   scan and must stay EVENT.
2. **Any segment producible on demand, out of order.** The requirement
   that gets skipped. A VOD playlist *promises* random access; if
   segment 500 answers 404 because the sequential pipeline is at 40,
   clients do not tolerate it. VOD is per-segment production, not a tag
   change.
3. **Immutability and exact boundaries.** Segment *k* must always be
   the same bytes, and the boundary the planner predicted must be the
   one the sink produced. One frame of drift and `EXTINF` stops
   matching, which corrupts seek accuracy for the rest of the file.
4. **One init segment for all of them.** Identical parameter sets; for
   encodes, a forced IDR at every boundary plus audio priming, since
   AAC's encoder delay is per-run.

### Two different projects

**Copy/remux.** Boundaries are the source's own keyframes. A segment
job is the existing pipeline with a start (`start_ms`, already
supported) and an end bound. No encoder state, so rebuilding a segment
is a source seek plus a GOP of demux.

**Transcode.** Encoders treat keyframe requests as advisory — we have a
`vtenc` run that skipped one — and scene-cut keyframes shorten segments
unpredictably. Boundaries cannot be predicted, only enforced, so
per-segment encode jobs and all of (4) become mandatory.

## What it buys

- **Seeks become client-side.** No restart, no output-directory wipe,
  no discarded encode work, and `playlist_ready`'s whole rationale
  disappears.
- **The player collapses.** `offsetRef`, the cue shifting,
  `timeOffset` and the `trackEpoch` dance exist only because player
  time ≠ media time.
- **Segments become reusable** across sessions and viewers. Today every
  session re-encodes from scratch; a second viewer pays full price.
- **Per-segment failover.** AR-6 becomes retry-the-segment instead of
  re-issuing a session, and HUB-17's quality switching becomes ordinary
  variant selection.
- **The liveness race goes away by construction** — idleness is legal
  in VOD. A side benefit, not the reason: see the correction above.

## What it costs

- **The prediction has to be exactly right.** The planner predicts
  boundaries, the sink produces them, and nothing checks that the two
  agree. That is the real risk on the copy path, and it belongs in the
  sweep, not in a unit test.
- **Per-segment overhead on encodes.** Encoder init per job,
  rate-control reset at every boundary, a GL/tone-map context per job,
  and hardware session limits under a player that prefetches 3–5
  segments. Steady-state CPU gets *worse* than one linear encode; only
  scrubbing and reuse get better. Longer segments amortise it and fight
  seek granularity.
- **Cache growth.** Transcoded segments become durable artifacts keyed
  by item × profile × tracks × burn choice. OPS-6 ("caches are not
  evicted") was decided for artwork and subtitle extractions — small,
  expensive to rebuild. A transcode cache is library-sized and cheap-ish
  to rebuild: the opposite quadrant. The rule as written does not cover
  it and should not be stretched to.
- **Session-shaped mechanisms lose their anchor.** HUB-18's per-user
  concurrency, the 90 s idle teardown and the mediahost read lease all
  assume a live session; VOD delivery is closer to stateless.
- **HUB-17 names the mechanism** — "session restart at seek point".
  Changing it is a requirements edit with the status checklist in the
  same commit, not an implementation detail.

## The cost model

Two axes, per segment: cost to rebuild, and latency at the moment it is
needed.

| | rebuild a segment | seek → first frame |
|---|---|---|
| today (copy) | n/a, sequential | pipeline restart + runway gate: seconds |
| VOD (copy) | source seek + one GOP demux — cheap, repeatable | one segment fetch: sub-second |
| VOD (transcode) | decode + encode + init — expensive | segment production, unless already cached |

Copies win on both axes, so they need no cache at all: recompute is
cheap enough that segments can be ephemeral. Transcodes trade a worse
steady-state cost for a much better seek latency, and only pay off if
segments are cached — which is the OPS-6 question above, and must not
be answered by accident.

## If it is ever built

1. **Copies first, properly.** Keyframe list stored at scan; the
   playlist synthesized by the hub at serve time (`declare_target_duration`
   in `hub::api` is already where the hub rewrites playlists, and
   synthesizing there sidesteps hlssink2/hlssink3 differences);
   per-segment production on demand; and a sweep check that predicted
   boundaries match produced ones.
2. **Transcodes stay EVENT** until per-segment encoding exists, and
   that decision rests on the cache question rather than on symmetry
   with the copy path.

**Open question to settle before any code:** whether a VOD playlist for
a copy must predict exactly how the *sink* would cut, or whether
segmentation is taken away from the sink and copies are cut from the
index directly. The second is more work and much easier to verify.

## Why it is not scheduled

With the paused-player claim withdrawn, no observed failure requires
VOD. What is left is three improvements — seeking without a restart,
segment reuse across viewers, and deleting the client-side offset
bookkeeping — against an architecture change that touches the session
model, the lease model, the cache policy and one requirement. That is a
different budget than a bug fix, and it should be spent deliberately.
