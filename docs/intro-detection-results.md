# Intro and credits detection: what the comparison said

Measured on 2026-08-17 against
[intro-skipper](https://github.com/intro-skipper/intro-skipper) at commit
`577981ff7fe8b4745ab02040d525f315194732f8` (branch `10.11`), built from their
sources by `scripts/kahawai-intro-ref.sh` and run with jellyfin-ffmpeg 7.1.4-3.
No Jellyfin server, no Jellyfin assemblies. The design is in
`docs/intro-detection-plan.md`; every number below is reproducible with

```sh
scripts/kahawai-intro-dataset.sh ~/media/introtest/synthetic
scripts/kahawai-intro-compare.py l1 EPISODE --window 360
scripts/kahawai-intro-compare.py l2 EPISODE OTHER --window 360
scripts/kahawai-intro-compare.py l3 SEASON_DIR [--anime]
```

The reference harness runs their Introduction, Recap and Credits modes; the
chapter analyzer is not exercised on either side, because neither has chapter
titles to read.

## The short version

On three seasons — one synthetic with known boundaries, one anime, one
live-action 4K HDR — the two implementations found the **same 36 of 36**
openings and credits, at a median IoU of **1.0**. Two differences survive, both
understood and both traced to a specific mechanism, not to the search: one is
their keyframe scan reading past its own window, the other is a fingerprint
that differs by a fraction of a bit near a gap threshold.

**Recaps**, added later and measured on the synthetic season where the truth is
known: ours finds 4 of 6, theirs 2 of 6, and where both find one they agree
within 0.36 s. The gap is not one mechanism but a handful of deliberate departures
from their recap flow, kept because the simpler shape measured better here:
our black-frame filter uses the fixed threshold rather than their adaptive
normalisation, the stored recap end is the raw last-black-frame (no silence
pull-back or keyframe snap — a recap ends on a hard cut, not a fade), and the
pairwise search takes the first episode pair that yields a card where theirs
keeps trying later pairs. `season.rs::recaps` documents these and one more (a boundary scan that reads two seconds past the intro start and clamps back). Counting all three kinds against ground truth, ours hits 16 of
18 boundaries and theirs 14, at a median IoU of 0.978 against 0.974.

We are **3–8× slower** per season. Both are fast enough for a background pass.

## L1 — fingerprints

Ours (GStreamer decode, `rusty-chromaprint`) against theirs (ffmpeg's
`chromaprint` muxer), point by point over the same window.

| clip | window | identical | mean bits | max bits | within their 6-bit tolerance |
|---|---|---|---|---|---|
| Ao no Exorcist E01 (FLAC 48 kHz) | 360 s | 1887/2886 | 0.47 | 6 | **100%** |
| Andor S01E01 (E-AC-3) | 588 s | 2579/4726 | 1.21 | 16 | 95.4% |
| big_buck_bunny_intro.mp3 | 25 s | 58/180 | 1.14 | 5 | **100%** |

Not bit-identical, and it does not need to be: their search calls two points
equal when at most 6 bits differ. The reference has the same jitter against
itself — ffmpeg's chromaprint muxer versus `fpcalc -raw` on the same file is
179/180 identical, max 1 bit — and feeding ffmpeg's own PCM to our
fingerprinter halves the distance (101/180 identical, mean 0.54), so roughly
half the difference is the two decoders and half is the two Chromaprint
implementations.

The E-AC-3 row is the loosest, with 4.6% of points outside the tolerance. It
did not change any boundary in L3, but it is the case to watch.

## L2 — the search

Identical fingerprints into both implementations: theirs, so the input is not
in doubt.

| points from | ours | theirs |
|---|---|---|
| big_buck_bunny intro/clip, 60 s | 0 – 17.213726379440665 / 0 – 22.167316704459562 | identical |
| Ao no Exorcist E01/E02 intro window | 99.1956462585034 – 188.6079516250945 | identical |
| Ao no Exorcist E01/E02 credits window | 344.15068783068784 – 432.8199546485261 | identical |

Identical to the last bit of the double, including on their own unit test's
expected values (`TestIntroDetection`: 17.214 and 22.167). The port of the
search is not approximately right; it is the same function.

## L3 — end to end

Both implementations run from the media file. Ours through
`scripts/kahawai-intro.sh`, theirs through `scripts/kahawai-intro-ref.sh`.

| season | segments found by both | only ours | only theirs | median IoU | within 1 s | ours | theirs |
|---|---|---|---|---|---|---|---|
| Synthetic (6 × 10 min, with recaps) | 14 | 2 | 0 | 1.0 | 14/14 | 8.3 s | 2.2 s |
| Ao no Exorcist (6 × 24 min, anime) | 12/12 | 0 | 0 | 1.0 | 10/12 | 28.5 s | 3.7 s |
| Andor S01 (6 × 40 min, 4K HDR10) | 12/12 | 0 | 0 | 1.0 | 10/12 | 125.8 s | 39.6 s |

**Against ground truth**, on the synthetic season where every boundary is known
by construction: ours hits 16 of 18, theirs 14 of 18, at median IoUs of 0.978
and 0.974. The two the pair miss are the same two, and they trace to the
dataset rather than to either implementation: synthetic melodies partly match
each other, so the opening's *start* lands up to three seconds early there, and
the recap's bounding black frame then falls outside the window. Real seasons do
not show it — the anime and live-action openings agree with theirs to within
0.37 s.

### The two differences that remain

**Andor E01 and E06, intro end, 7.1 s and 7.5 s apart.** Not the search: with
refinement off (`--no-refine`) the raw matches agree to 0.12 s. It is the
keyframe snap. For E01 the raw end is 27.74 s, so both look for a keyframe in
`[22.74, 29.74]`:

```
$ scripts/kahawai-intro-ref.sh keyframes E01.mkv 22.74 29.74   →  30.020, 40.020
$ target/release/examples/keyframe_probe E01.mkv 22.74 29.74   →  (none)
```

Both keyframes their scan reports are *outside* the window it asked for:
`ffmpeg -to` does not bound a `-skip_frame nokey` scan, a leak their own source
comments on elsewhere. Their `SelectNearest` then snaps the end to 30.02 —
6.7 s past the silence they themselves picked. We keep the window and, finding
no keyframe in it, leave the end where the silence put it. Both are defensible;
only one of them is what the code says it does.

The silence detectors agree closely, incidentally: on the same window, theirs
reports 23.284–25.353 and ours 22.916–25.477.

**Ao no Exorcist E05 and E06, credits start, 3.34 s apart.** Fingerprint
jitter meeting a threshold: their contiguity rule allows gaps of up to 3.5 s,
so a single point that one side matches and the other does not extends the run
by just under the limit. L2 on those two episodes' credits windows returns
identical ranges, so the search agrees when the input does.

## What the comparison found in our port

Five bugs, none of which any test we could have written would have caught: the
first three by disagreeing with the reference on real media, the last two by
running the detector inside the hub against a library on another machine.

1. **Every black-frame probe was measuring the decoder.** Seeking into the
   middle of a GOP and reading pixels immediately gives frames the decoder has
   not got the references for yet, and they are dark: 92% "black" where the
   file's own frame is 4% black, on exactly the signal the credits search hunts
   for. Fixed by decoding two seconds of lead-in and dropping it
   (`decode::LEAD_IN`). Andor's credits went from up to 86 s off to exact.
2. **The black-frame search start was not carried across episodes.** Theirs
   seeds each episode's binary search with where the previous episode's credits
   began; computing it fresh each time converges on a different black run.
   Worth up to 25 s per episode here.
3. **`gst::Caps` before `gst::init()` panics**, which the probe tools hid
   behind a redirected stderr and which read exactly like "no silence found".
   The rig's own tools needed the same scrutiny as the thing they measure.

4. **Unwanted pads were parked on default fakesinks**, which honour the clock.
   The video branch then drained at playback speed, the demuxer stopped feeding
   the audio branch about 28 seconds in, and the analysis of a 22-minute
   episode hung — indistinguishable, from the outside, from a hung byte plane.
   `sync=false` on those sinks; the earlier local tests never showed it because
   their files are small enough to be read before backpressure matters.
5. **The recap search re-fingerprinted a window the opening search had already
   read.** Cheap on a local file, a quarter of the episode dragged across the
   LAN twice on a remote mediahost. The two searches now share one pass.

And one in the rig rather than the port: holding a single `ChromaprintAnalyzer`
across analysis modes silently searches the credits with the intro's inverted
index — their cache is keyed by episode id alone. Their plugin builds one
analyzer per mode, so this is a trap for anyone driving their classes directly,
not a bug in the plugin. Symptom: credits found by the black-frame fallback, or
not at all.

## Cost

Per episode, on the dev box, release build, cold cache on both sides:

| season | ours | theirs |
|---|---|---|
| Synthetic 10 min | 1.4 s | 0.4 s |
| Anime 24 min | 4.8 s | 0.6 s |
| Andor 40 min 4K HDR | 21.0 s | 6.6 s |

Inside the hub, reading a remote mediahost over the byte plane, the same work
costs about a minute an episode: roughly 500 MB crosses the LAN per episode,
and that — not the search — is what the time is spent on.

The gap is decode plus per-probe pipeline setup: we build and preroll a
GStreamer pipeline per probe where they hand one ffmpeg process a filter
graph, and `rusty-chromaprint` is a pure-Rust port of a C library. Nothing here
suggests an algorithmic difference, and neither number matters much for a pass
that runs once per episode ever.

## What is not replicated

Since these measurements were taken, two of the original gaps closed: Kahawai
now indexes chapter titles (the sparse container reader) and runs the
chapter-name analyzer — a season whose files name their own boundaries is
answered without reading a byte — and recap detection was implemented and is
measured above. Still not replicated: the preview / commercial modes (the same
search with different windows), and everything Jellyfin-shaped — the plugin's
database, its scheduled tasks, its skip button.
