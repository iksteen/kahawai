# Intro and credits detection: plan

Replicate what the Jellyfin plugin
[intro-skipper](https://github.com/intro-skipper/intro-skipper) does — find the
shared opening and the end credits of an episode — inside Kahawai, and measure
the result against intro-skipper's *own code*, with no Jellyfin server and no
Jellyfin assemblies anywhere in the loop.

Two halves, and the second one is the point: a port that only agrees with
itself proves nothing (`docs/kahawai-implementation.md`, and the sweep that
scored 0 FAIL for six days because the binary was stale). The reference must be
their code, running.

## What intro-skipper actually does

Read from the `10.11` branch at commit `HEAD` of 2026-08-16. Every number below
is quoted from a named source file, not from recall.

**1. Analysis windows** (`Manager/QueueManager.cs`)

- Intro: `[0, min(duration × 25%, 10 min)]`, and the percentage only applies
  when the episode is at least 5 minutes long.
- Credits: `[duration − min(duration, 450 s), duration]`; 900 s for movies.

**2. Fingerprint** (`FFmpeg/FFmpegService.cs`)

`ffmpeg -ss START -i FILE -to LEN -ac 2 -f chromaprint -fp_format raw -` — raw
Chromaprint points, one `u32` per 4096/11025/3 ≈ 0.123832 s hop
(`Data/ChromaprintConstants.cs`). Identical to `fpcalc -raw`, which is how
their own test vector was generated.

**3. Shared-region search** (`Analyzers/ChromaprintAnalyzer.cs`)

For every unordered pair in a season, until a pair yields a region:

- Invert both fingerprints (point → *last* index it appeared at).
- For every left point, look up right points within ±2 (`InvertedIndexShift`);
  each hit contributes a candidate shift.
- For each shift, XOR the aligned arrays and keep positions whose popcount is
  ≤ 6 (`MaximumFingerprintPointDifferences`).
- Longest run of kept positions with gaps ≤ 3.5 s (`MaximumTimeSkip`) wins,
  provided it is ≥ 15 s (`MinimumIntroDuration`) and ≤ 120 s
  (`MaximumIntroDuration`; for credits, `duration − creditsStart − 1`).
- A region starting within 5 s of zero is snapped to zero.
- Credits ranges get the credits window start added back, because the
  fingerprint's clock starts at the `-ss` seek.

**4. Black-frame credits** (`Analyzers/BlackFrameAnalyzer.cs`)

Binary search backwards from the end of the file: probe a 2 s window at the
midpoint, ask ffmpeg `blackframe=amount=85:threshold=28` whether any frame in
it is black, move the bracket, stop at 4 s of error. The previous episode's
answer seeds the next episode's search.

**5. End refinement** (`Analyzers/TimeAdjustmentHelper.cs`)

Within `[end − 5 s, end + 2 s]`: take the first silence ≥ 0.33 s
(`silencedetect=noise=-50dB`), then snap to the nearest keyframe. An end within
2 s of the file end becomes the file end.

**Not replicated, and why**

- ~~Chapter-name analysis (`ChapterAnalyzer.cs`)~~ — was skipped because
  Kahawai did not index chapter titles. It does now, and the analyzer is
  implemented (`crates/kahawai-intro/src/chapters.rs`): a season whose
  episodes name their own opening and credits is answered from the names,
  and never reaches the fingerprint pass.
- Unnamed-chapter-mark trust (`AdjustIntroBasedOnChapters` and
  `UseChapterMarkersBlackFrame`, both default-on upstream): snapping a
  measured intro edge to the nearest chapter boundary, and trying the last
  chapter in the credits window before the black-frame search. An unnamed
  mark is somebody's scene split; this port only believes chapters that
  NAME what they are. On chaptered files the two implementations disagree
  by design (`season.rs`'s `Config` doc records the same).
- Preview / commercial modes: the same chromaprint search with different
  windows and bounds. (Recap was out of scope here and implemented later, with
  the hub integration — see below.)
- Everything Jellyfin-shaped: the plugin's DB, its scheduled tasks, the skip
  button, the config page.

## What we build

A new crate, `crates/kahawai-intro`, because it carries the one dependency
(`rusty-chromaprint`) that nothing else in the workspace wants:

- `chroma.rs` — the shared-region search. Pure functions over `&[u32]`; this is
  the part that gets compared point-for-point against theirs.
- `fingerprint.rs` — decode a time window through GStreamer into interleaved
  `i16`, feed `rusty_chromaprint::Fingerprinter` (`preset_test2`, which is what
  `fpcalc` and ffmpeg's muxer both default to), return the raw points.
- `blackframe.rs` — decode a window, count luma samples below the threshold per
  frame, run their binary search over that signal. The frames are read in
  whatever planar format the decoder produces, 8- or 10-bit: converting to a
  fixed one also converts colorimetry, and on HDR that rewrites the very luma
  the threshold is a raw value of.
- `silence.rs` — runs where every channel stays below −50 dBFS, sample by
  sample. `silencedetect` in one screenful.
- `season.rs` — windows, the pairwise loop, credits offsetting, end refinement.
- `kahawai intro <dir>` — a subcommand that prints JSON segments, plus
  `scripts/kahawai-intro.sh` per the house rule.

**Since shipped (HUB-37).** The crate is no longer only a command-line tool:
the hub runs it a season at a time over a mediahost lease, stores the
boundaries per episode, carries them on the item QUERY, and the web
player offers a skip button while the playhead is inside one. Recap detection
was added with that work. `docs/kahawai-implementation.md` §4.9 has the
design; this document remains the record of *why the port is faithful*, which
is what makes the comparison below meaningful.

GStreamer, not ffmpeg: it is what Kahawai ships, and it makes the comparison
worth running — two decode stacks, one algorithm, and any disagreement that
comes from decoding is a finding rather than a coincidence.

## How it gets measured

Three levels, because a single end-to-end number cannot say *which* half is
wrong (`docs/kahawai-implementation.md` on measuring what distinguishes).

**L1 — fingerprint parity.** Our points versus the reference rig's ffmpeg
chromaprint muxer on the same file. ~~Exact equality is the bar~~ — as built,
the rig reports bit-distance statistics and the share of points within the
search's own 6-bit tolerance, because two decode stacks land a point apart at
window edges without that mattering to the search
(`docs/intro-detection-results.md` has the measured numbers).

**L2 — algorithm parity.** Feed *identical* `u32` arrays to both
implementations and compare the segments they return. This isolates the search
from the decoder: any difference here is a porting bug, full stop.

**L3 — end-to-end.** Both implementations run from the media file on the same
episodes. Report per-episode start/end deltas, IoU, agreement rate, and
wall-clock.

**The reference rig.** `scripts/kahawai-intro-ref.sh` clones intro-skipper at a
pinned commit into a cache directory, drops a small MIT-licensed harness
(`scripts/introref/`: a `Program.cs` and shims for the handful of Jellyfin
types the analyzers touch) beside it, and builds the analyzer sources
*unmodified* with `dotnet` from `~/.dotnet`. No Jellyfin packages: the analyzers
reach Jellyfin only through `Plugin.Instance`, chapter lists, and a logger, all
of which the shim supplies.

Their GPL-3.0 sources are never vendored into this MIT repository — the rig
fetches them, builds locally, and the harness binary is never distributed.

**Datasets.** Two, and both are needed:

- Synthetic seasons built by `scripts/kahawai-intro-dataset.sh`: a shared
  intro, distinct bodies, shared credits over black. Ground truth is exact and
  reproducible on any machine, so accuracy — not just agreement — is
  measurable, for them as well as for us.
- Real episodes, whatever directory is passed on the command line. Agreement
  and timing only; nobody hand-labels 26 episodes.

Results land in `docs/intro-detection-results.md`, re-measured in the commit
that claims them.
