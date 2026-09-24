# Bi-generational source comparison — 2026-09-24

The regression workload is an AVI with audio/video demux demands alternating
between advancing file regions. The 32 MiB single-window pull source stalled
under this pattern, while the old push appsrc played it successfully. A second
workload is an MP4 with audio and video in widely separated file regions.
Physical interleave distance alone did not predict whether either source
implementation would stall.

## Method

NAS mediahost → Linux AIO → headless Chrome, driven by Playwright.
Each run starts at zero and observes two minutes after playback passes its first
second. Progress writes are intercepted to preserve the user's watch position.
The AVI workload transcodes MPEG-4 video with `nvh264enc` and AC3 with `fdkaacenc`, using
the same measured −5.03 dB loudness adjustment. The MP4 workload copies H264/AAC to
HLS/TS; the temporary browser profile masks MP4 support to prevent direct play.
The executor runs on the Linux hub; these tests do not exercise a remote transcoder.

Old push is commit `48fa253217a865569d85a64c477053281b3c37f4` (protocol 4.5).
Single-window pull is the deployed code subsequently committed as `eba4d17`
(protocol 5; actual binary stamped `82f74ba+dirty`). Bi-generational pull uses
protocol 6, two 16 MiB hub windows and two separate NAS byte connections. The
remote-transcoder allowance is two 1 MiB windows; these real-file comparisons
use hub-local workers. The same patched GStreamer stack is used throughout.

Startup below means navigation to the first browser `playing` event, not just
session creation. Production rate is the worker's initial unpaced measurement:
media duration produced divided by wall time, sampled after 60 seconds or when
it reaches the 900-second pacing limit. Client buffer is measured independently.
Tests also inspect worker errors and source diagnostics: uninterrupted browser
playback alone can hide a producer that has already failed.

These are real playback observations, not controlled throughput benchmarks.
The push/single-window runs were on September 23 and the final bi-generational
runs on September 24. NAS filesystem caches were not reset and its external
interleave probe remained active. Do not interpret a rate difference between
successful runs as a precise speedup attributable only to the source design.

## Implementation checks

Each window reserves half the existing total allowance, releases blocks before
its own read's starting block, and backpressures independently. Nearby forward
misses stay on that window; a skip smaller than a transport chunk waits for
in-flight data. A genuinely new region replaces the least recently demanded
window. Replacing one generation neither clears nor cancels the other.

Two pitfalls found during live validation have regression coverage or use the
production path in the integration fixtures:

- A consecutive audio read just beyond its prefetch frontier initially replaced
  the older video window. The transport regression reproduces this demand
  sequence and requires both original generations to survive.
- Two gRPC streams on one mediahost connection were not independent. The initial
  MP4 attempt produced only about three minutes before a source-read
  failure, although its two-minute browser test saw no stall. Production lease
  construction now opens a fresh connection, and the real mTLS remux/dispatched
  playback fixtures use that production constructor.

Runnable contract checks: `scripts/kahawai-playback.sh pull`. They cover exact
reads, retained prefixes, full-window admission, alternating streams, third-region
replacement, grant ownership/revocation, retries, and real pipeline execution.

## Results

| Workload | Source | Startup | Underruns in two minutes | Initial production rate |
|---|---|---:|---:|---:|
| Alternating-region AVI transcode | Old push | 1.34 s | 0 | 67.23× |
| Alternating-region AVI transcode | Single-window pull, 32 MiB | 11.52 s | 5 | 0.76× |
| Alternating-region AVI transcode | Two-window pull, 32 MiB total | 3.05 s | 0 | 6.66× |
| Alternating-region AVI transcode | Two-window repeat | 2.05 s | 0 | 3.93× |
| Separated-track MP4 remux | Old push | 2.16 s | 0 | 15.07× |
| Separated-track MP4 remux | Single-window pull, 32 MiB | 1.33 s | 0 | 108.20× |
| Separated-track MP4 remux | Two-window pull, 32 MiB total | 2.47 s | 0 | 88.16× |

Both final AVI runs held about 61 seconds of client buffer and finished without
worker errors. The two windows remained at generations 5 and 2 after startup,
compared with 70 generations in the single-window run. The first final AVI run
overlapped local test-suite execution; the repeat ran after tests finished.
The observed throughput gap to old push remains unresolved: these results
establish removal of the observed underruns, not performance parity.

The final MP4 run held about 66 seconds of client buffer and produced 900.025
seconds of output in 10.209 seconds before pacing, without worker errors.
No throughput improvement over the single-window source is claimed for this
workload. More than two independent access regions can still cause replacement;
overlapping windows can duplicate prefetch, and each stream has half the total
reservation rather than access to the whole budget.

## Verification and rollout

The full workspace suite passed during development. After the final connection
ownership change, the complete hub and mediahost suites passed serially with
`KAHAWAI_MEDIA_TEST_STRICT=1`; workspace clippy and formatting checks passed.
Transport tests cover the final forward-skip selection behavior. Live playback
uses the real NAS and the release build, not the integration fixtures.

Protocol 6 requires coordinated hub/mediahost/transcoder activation; see
[kahawai-deployment.md](kahawai-deployment.md#protocol-6-fleet-cutover). The
comparison temporarily upgraded the test hub and mediahost, then restored their
original protocol-5 binaries. This report does not imply fleet deployment.
