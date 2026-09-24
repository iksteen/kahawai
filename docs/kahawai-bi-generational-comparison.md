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

## Follow-up: isolate byte delivery from demuxing

Instrumenting the remote file reader during a one-minute run recorded 639
reads. Admission, blocking-thread scheduling and file I/O together took 0.293
seconds; median file I/O was 216 microseconds. Sending chunks spent 124.6
seconds blocked across the two concurrent streams. Thus file I/O and scheduler
admission did not explain that run's stalls waiting for source bytes.

Direct sessions then served ranges without GStreamer or `ReadAhead`. Two
consumers draining independently transferred 512 MiB at 30.9 MiB/s combined.
Alternating one 256 KiB read from each response transferred 256 MiB at only
3.43 MiB/s. This reproduces a delivery penalty without demux track selection.

Diagnostic builds varied HTTP/2 connection ownership and receive credit while
keeping the retained source budget unchanged:

| Alternating 256 MiB transfer | Aggregate throughput |
|---|---:|
| Independent connections, default credit | 3.43 MiB/s |
| Shared connection, 4 MiB connection / 1 MiB stream credit | 20.61 MiB/s |
| Independent connections, 4 MiB connection / 1 MiB stream credit | 19.40 MiB/s |
| Return to independent connections and default credit | 4.66 MiB/s |

The pinned Hyper implementation defaults to 1 MiB at both levels. These are
transport flow-control allowances, distinct from the 32 MiB read-ahead budget.
The larger connection window alone improved the simple alternating benchmark;
connection sharing cannot be credited with that entire improvement. In real
playback, the shared-connection experiment measured 27.60× production, while
the larger-credit independent-connection experiment measured 7.76×. Reducing
the stream window to 256 KiB while retaining 4 MiB connection credit and
independent connections measured 15.00×. The precise
transport feedback causing this workload sensitivity remains unproven; these
results do not establish a production tuning policy.

A separate cost is duplicate prefetch. During the shared-connection playback
experiment, recorded file-read intervals totalled 935,193,000 bytes but covered
only 472,508,680 unique bytes: 49.5% of the read traffic duplicated earlier
ranges. The two continuously advancing windows largely traversed the same file
region. Stable generations therefore do not imply efficient byte delivery.
The old push run did not record comparable interval counters, so this is not
an exact attribution of the entire remaining throughput difference.

Appsrc cannot supply demux track identity with these reads: its callbacks carry
`need-data(length)` and `seek-data(offset)`. The demuxer knows which output track
it is serving, but the upstream pull request carries an offset and length on
its common sink pad. See the [appsrc signal API](https://gstreamer.freedesktop.org/documentation/app/appsrc.html#need-data).
A track label would not itself eliminate overlapping byte transfers or the
transport penalty reproduced by direct range reads.

These were temporary diagnostic builds. Shared connection credit is not a
substitute for proving isolation when more leases or sessions are active.

## Flow control and shared retention

The follow-up uses independent connections with explicit 4 MiB connection /
256 KiB stream receive credit on the satellite listener and TLS clients. The
retained-source allowances remain 32 MiB at the hub and 2 MiB at a transcoder.
Resident bytes take precedence over assigning another producer. Separate recent
demand-position hints allow release behind both cursors while a nearby pair
shares one stream; demand admission can reclaim space even if a hint goes stale.
When two producers converge, selection favors the latest resident supplier so
one stops advancing instead of shadowing the other.

Simply retaining every block until a miss reduced duplication but measured only
5.37× production: the receiver stayed full and admitted prefetch in bursts.
Releasing behind the active positions restored continuous refill. The revised
AVI run produced 900.020 seconds in 18.417 seconds (48.87×), started playback in
1.97 seconds, and completed two browser minutes without underruns or worker
errors, holding about 61 seconds of client buffer. This improves the observed
4–7× two-window results but does not establish parity with the historical 67.23×
push measurement.

NAS positional-read tracing recorded 1,829 reads totalling 478,800,296 bytes,
covering 473,557,256 unique bytes: **1.10% duplicate file-read bytes**, compared with the
earlier 49.5% measurement over a similar unique-byte footprint. Startup probes,
seeks, eviction and already queued speculative data can still cause retransfers;
this is not a guarantee of globally unique reads.

The transport regressions verify exact data while nearby cursors advance beyond
six retention capacities with no overlapping upstream reads, formerly distant
producers converge without sustained duplicate fetching, and reads straddling a
block boundary remain resident. Existing distant-region, full-buffer admission,
seek replacement and lifetime checks remain part of `scripts/kahawai-playback.sh pull`.

The separated-track MP4 check produced 900.025 seconds in 11.218 seconds
(80.23×), started in 2.17 seconds, and completed two browser minutes with no
underruns or worker errors and about 66 seconds of client buffer. Both source
windows continued independently. The earlier independent-window run measured
88.16×; these observations establish continued successful playback, not a
throughput improvement for this workload.

With the final receive-credit policy, the direct alternating-range test transferred
256 MiB in 10.681 seconds (23.97 MiB/s), without GStreamer or read-ahead in the
path. Both responses were HTTP 206 and their combined length was checked. The
original-credit repeats measured 3.43 and 4.66 MiB/s. These runs retain the same
network/cache/background-load limitations described above.

The original local AIO and NAS mediahost binaries were restored after these
checks and verified by SHA-256; both hubs again reported the NAS healthy and no
test sessions remained. The Mac mini executable was unchanged. These results do
not imply fleet rollout.

Final validation passed: the full workspace suite serially with
`KAHAWAI_MEDIA_TEST_STRICT=1`, `cargo clippy --workspace --all-targets`, release
builds for AIO and mediahost, and `cargo fmt --all -- --check`. Real-wire fixtures
use the same receive-credit constructor as the production listener.
