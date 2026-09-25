# Wired Mac/NAS push-versus-pull playback comparison

Measured 2026-09-25. Push baseline `48fa253217a865569d85a64c477053281b3c37f4`;
pull candidate `59b4ebd1ae57029da18696ef4a2f1438e8978022`. These are the same
whole revisions as the [desktop comparison](2026-09-24-browser-origin-head.md),
not an isolated appsrc-mode toggle. All arrows below mean **push → pull**.

## Findings

The sustained starvation and long seek recoveries seen in the desktop runs did
not recur in these wired Mac tests. M03, M05 and M10 all sustain playback on both
revisions. The original interleaving stress samples also play without waits.
M08's HEVC-copy path has brief waiting events on both revisions but loses very
little playback time. M01 still fails to decode on both; M13 plays on both here.

The pull revision is not uniformly lower-latency. It provides more production
headroom in several remux cases; I02 has the largest measured gain. The two
AVI cases are approximately tied in production. These are single paired runs,
not a statistical estimate of small performance differences. In the forced UHD
transcode, seek latency is 7.52 → 6.34 seconds and post-seek production is
1.63× → 1.88×, with no waits on either revision. All eight successful workload
pairs have equal or higher measured production on pull; only two have lower
startup latency and three lower seek latency in this run.

## Method and controls

The Mac mini ran the AIO and the browser; silence served the same pinned NAS
files. Their path is Ethernet through a switch, as confirmed by the maintainer.
The desktop only orchestrated SSH and collected small diagnostic responses.
The [synthetic wired control](2026-09-25-wired-tcp-control.md) preceded this test.

The Mac is model Mac16,10, with 10 logical CPUs and 16 GiB RAM, running macOS
26.6.2. Both release binaries were built against the patched GStreamer keg at
`/opt/homebrew/Cellar/kahawai-gstreamer/1.28.7`; dynamic-link checks rejected the
stock Homebrew GStreamer. Both embedded the same web bundle. NAS mediahost
binaries matched each revision's protocol. No builds ran during playback.

Playwright drove Chromium 151.0.7922.34 on the Mac, using a fresh context per run.
The container capability mask forced pipeline playback, with normal codec
negotiation. Each successful start was observed for 120 wall seconds, followed
by a seek through the player's control to 75% and another 120-second observation.
Startup and seek each had a 120-second timeout. A first frame followed by a
paused decoder is a failure, not successful playback.

The initial eight pairs alternated revision order. Caches were not flushed.
Two adjustments were necessary and are retained in the evidence:

- M05 hit a Mac hardware-decoder error on the initial pull run. The paired M05
  results below use `--disable-accelerated-video-decode` on **both** revisions.
  The original hardware-decoder run is archived separately, with a short push
  hardware-decoder control. Server codec decisions were unchanged.
- The Mac browser accepts HEVC, unlike the earlier desktop browser. M08 therefore
  copies HEVC video and encodes TrueHD audio to AAC. **M08T** is the same source
  with HEVC masked out, restoring the earlier H.264 video-transcode and HDR-to-SDR
  workload. This additional pair ran pull then push.

M05's matched pull rerun followed the main sequence. No test writes playback
progress, preserving watch history but suppressing live viewer-position updates,
just as in the desktop benchmark. Production rates are the worker's unpaced
media-time/wall-time ratio, measured at 900 seconds of output or 60 seconds of
production. They are not network throughput. A dagger marks completion rate when
all remaining media finishes before that measurement; it includes worker startup.
Process memory and CPU were not sampled on macOS.

## Startup, seek and production

Latencies are seconds. Production is a multiple of real time. M01's missing
latencies/rates reflect failed playback, not zero performance.

| Sample | Startup s | Seek s | Opening production × | Post-seek production × |
|---|---:|---:|---:|---:|
| M01 | — → — | — → — | — → — | — → — |
| M03 | 0.56 → 0.58 | 0.97 → 1.07 | 29.08 → 34.61 | 29.73 → 34.80 |
| M05 | 0.64 → 0.72 | 0.86 → 1.01 | 24.00 → 30.72 | 26.41 → 30.41 |
| M08 | 1.10 → 1.20 | 1.47 → 1.59 | 13.62 → 15.72 | 13.29 → 15.62 |
| M08T | 6.64 → 4.87 | 7.52 → 6.34 | 1.97 → 2.68 | 1.63 → 1.88 |
| M10 | 1.17 → 1.26 | 1.32 → 1.10 | 21.72 → 21.83 | 21.69 → 21.77 |
| M13 | 0.44 → 0.76 | 3.57 → 3.68 | 285.54 → 331.26 | 260.57† → 271.05† |
| I01 | 1.27 → 0.96 | 0.81 → 1.00 | 61.26 → 61.50 | 61.20 → 61.33 |
| I02 | 0.96 → 1.27 | 3.10 → 2.28 | 47.96 → 347.77 | 47.86 → 244.71 |

## Playback progression

Each advancement figure covers a 120-second observation. Waiting-event counts
are reported separately from clock advancement: a brief event is not equivalent
to seconds of buffer starvation.

| Sample | Opening advancement s | Opening waits | Post-seek advancement s | Post-seek waits |
|---|---:|---:|---:|---:|
| M01 | — → — | — → — | — → — | — → — |
| M03 | 119.95 → 119.96 | 0 → 0 | 119.96 → 119.96 | 0 → 0 |
| M05 | 119.96 → 119.96 | 0 → 0 | 119.96 → 119.96 | 0 → 0 |
| M08 | 119.84 → 119.92 | 10 → 10 | 119.90 → 119.89 | 4 → 4 |
| M08T | 119.96 → 119.96 | 0 → 0 | 119.96 → 119.96 | 0 → 0 |
| M10 | 119.96 → 119.96 | 0 → 0 | 119.96 → 119.96 | 0 → 0 |
| M13 | 119.95 → 119.96 | 0 → 0 | 120.00 → 120.01 | 0 → 0 |
| I01 | 119.96 → 119.96 | 0 → 0 | 119.96 → 119.96 | 0 → 0 |
| I02 | 119.95 → 119.96 | 0 → 0 | 120.01 → 120.00 | 0 → 0 |

## Source and pipeline coverage

| Sample | Source | Pipeline |
|---|---|---|
| M01 | AV1/Opus Matroska, 1080p | AV1 and Opus copy; startup and seek fail on both |
| M03 | H.264/FLAC Matroska, 1080p, 25.4 Mbps | Video/audio copy |
| M05 | H.264/DTS Matroska, 1080p, 27.4 Mbps | H.264 copy, DTS-to-AAC; software browser video decoding for the matched pair |
| M08 | HEVC/HDR10/TrueHD Matroska, UHD, 57.5 Mbps | HEVC copy, TrueHD-to-AAC audio |
| M08T | Same source as M08 | H.264 transcode, HDR-to-SDR tone mapping, AAC audio |
| M10 | MPEG-4 Part 2/AC3 AVI | H.264 transcode |
| M13 | VP9/Opus WebM, 1080p | Video/audio copy |
| I01 | Original AVI interleaving stress sample | H.264 transcode |
| I02 | Original MP4 interleaving stress sample | H.264 copy |

## Interpretation and limits

The earlier desktop M03 and M05 underruns and M10's long seek are absent here.
Together with the plain-TCP reproducer, these observations undermine attributing
those earlier results solely to push or pull source mechanics. However, moving
to the Mac changes the receiver OS, CPU, GPU and browser as well as the network
path. Cross-host improvements, especially transcode speed and decoder behavior,
cannot all be assigned to Ethernet.

Within this Mac comparison, source renditions, browser settings and codec
choices match across each pair. Actual startup and seek latency can still be
slower on pull even where production is faster. No conclusion is made about
concurrent sessions, memory efficiency or longer viewing sessions. Original
binaries are restored after the tests; this report does not deploy a new revision.

## Evidence and verification

The [CSV](2026-09-25-mac-push-pull.csv) includes per-phase measurements, frame
drops, status and pipeline decisions. Private evidence is retained under
`target/benchmarks/2026-09-25-mac-push-pull/`: exact source manifest, browser/API
records, screenshots, worker diagnostics, NAS TCP snapshots, build/link checks,
binary hashes, harness and analysis scripts. No media identities are included in
this report or CSV.

Mac worker logs include ANSI color sequences; the harness strips them before
rate parsing. The first M03 push run's rates were recovered from its archived
session diagnostics using the same measurement fields. The initial pull
hardware-decoder run and short push control are diagnostics,
not members of the software-decoded pair. Both report
`VTDecompressionOutputCallback`, OSStatus `-12909`, at roughly 0.29 seconds
from the opening and roughly 0.04 seconds after the seek. The push control
observed ten seconds per window; the original pull diagnostic observed 120.
Both have buffered media when decoding fails. This error occurs on the push
baseline too; the software-decoded pair plays normally.

Across 1,100 NAS TCP RTT samples from the main and follow-up runs, RTT ranges
from 0.29 to 5.49 ms, with median 0.83 ms. These snapshots include control and
idle source connections, so this is not an active-data-only latency statistic.
The previously observed hundreds-of-milliseconds TCP state did not appear.

Both controllers exited successfully. All nine paired workloads passed checks
for exact source rendition, fingerprint, size and duration, matching stream
decisions and browser settings,
75% seek targets and full observation windows where playback succeeded. No
completed playback phase dropped more than 1% of frames. The Mac AIO and NAS
mediahost hashes match their original backups after restoration. Both Mac and
desktop hubs report the NAS healthy, no test sessions remain, and the browser
runner has exited. The desktop AIO and separate transcoder binaries were not
changed.
