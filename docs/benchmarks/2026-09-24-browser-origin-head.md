# Browser playback comparison: origin/master versus pull-source HEAD

Follow-up: the [wired Mac/NAS comparison](2026-09-25-mac-push-pull.md)
retests the problematic cases on the same two revisions, with the browser and
AIO on the Mac mini. Its results limit attributing the stalls below to source
scheduling alone.

Measured 2026-09-24. Baseline `48fa253217a865569d85a64c477053281b3c37f4`; candidate `59b4ebd1ae57029da18696ef4a2f1438e8978022`. All arrows below are **baseline → candidate**. Sample identifiers are intentionally anonymous.

## Findings

The candidate removes opening underruns on two high-bitrate sources and greatly improves several slow seeks. It is not uniformly faster: several already-fast sources regress in production or latency, and the UHD transcode has a worse seek and post-seek playback. One high-bitrate source still stalls after seeking on both revisions. Two additional sources fail browser playback on both revisions.

The data compares whole revisions, including protocol, buffering and flow-control changes. It does not isolate pull scheduling as the cause of every difference. One run per condition and uncontrolled OS caches limit causal conclusions.

## Conditions and measurement

Fifteen NAS movies were selected from 902 eligible single-part sources longer than 30 minutes. Seed `20260924` was used within format/codec/bitrate strata; the highest-bitrate UHD source was included deliberately as a stress control. This is a diverse stress sample, not a library-weighted random sample.

The local AIO and NAS mediahost ran matching release builds for each revision. Playback was hub-local; the Mac mini was not used. The browser was headless Chrome 154.0.8037.57, driven by Playwright through the actual web player at a 1280×800 viewport. The client code is the same between the tested revisions. The local host has 16 logical CPUs and approximately 62.7 GiB RAM, with GStreamer 1.28.7. Binary hashes and detailed environment information are in the private artifact bundle.

The capability mask removes direct MP4/WebM container support, forcing the pipeline. Supported codecs are copied; unsupported codecs follow the normal transcode path. Thus this is forced pipeline playback, including audio-only and video transcodes, not exclusively codec-copy remux. The chosen video/audio decisions are recorded below and in the CSV. Each request pins the same source rendition. Sources, fingerprints, sizes, durations, stream decisions, seek targets and browser versions were checked across each pair; changing advisory speed predictions were excluded from the stream-decision comparison.

Each run uses a fresh browser context, navigates to the source at zero, waits for a decoded frame, observes **120 wall seconds**, seeks using the player's seekbar to 75% of duration (rounded to a second), waits for a decoded frame at the target, then observes another 120 wall seconds. A start/seek that fails to reach playback within 120 seconds is retained as a failure. Brief playback followed by a decode error is also a failure, regardless of server production. Session recovery time is included in seek latency.

Pair order alternates baseline/candidate and candidate/baseline. OS page caches are not flushed. The local and NAS binaries are switched sequentially, without concurrent builds or benchmark sessions. NAS load and probe-process snapshots are saved around every run. Progress POSTs are intercepted to preserve the user's watch history; both revisions consequently run without live viewer-position updates. This affects normal pacing behavior and limits extrapolation to long viewing sessions. Initial production measurements precede the 900-second pacing boundary.

Unpaced production is the worker's media-time/wall-time rate, measured at about 60 seconds of production or upon reaching 900 seconds of output, whichever comes first. It excludes deliberate pacing and is not an end-to-end download rate. A blank rate means unavailable, not zero. Initial API latency, seek API latency, recovery sessions, HTTP bytes/errors, buffer depth, worker/hub memory and CPU, and candidate source-demand counters are also saved. Source counters include startup/seek preparation; post-seek values subtract the opening snapshot when the same session survives.

"Stall-free" means at least 118 seconds of clock advancement, no waiting event, no worker error and less than one sampled second unexpectedly paused in the 120-second window. It does **not** imply zero dropped frames. Frozen time is a one-second sampling estimate; clock advancement and event counts are the stronger evidence. RSS/HWM are whole-process measurements, not source-buffer allocation. CPU is percent of one logical core over sampled process lifetime. Production from obsolete runs is excluded after a recovered seek.

## Aggregate results

Failures M01 and M13 remain in the cohort but are excluded from successful-playback latency/rate aggregates. The paired ratio is computed per source first; it differs from a ratio of column medians.

| Measure | Opening baseline → candidate | At 75% baseline → candidate |
|---|---:|---:|
| Stall-free windows | 11 → 13 | 11 → 11 |
| Median first-frame latency, s | 1.56 → 2.35 | 10.98 → 5.76 |
| Median production, × real time | 18.32 → 19.40 | 3.86 → 27.26 |
| Total waiting events | 16 → 0 | 26 → 20 |
| Estimated frozen time, s | 36.19 → 0.00 | 97.74 → 80.45 |

Opening: production is faster on the candidate in 8/13 pairs; median paired production ratio 1.01×. First-frame latency is lower in 4/13 pairs; median paired latency ratio 1.43× (lower is better).

After seeking: production is faster on the candidate in 7/13 pairs; median paired production ratio 1.22×. First-frame latency is lower in 5/13 pairs; median paired latency ratio 1.19× (lower is better).

Across the 13 sources that sustain playback, the candidate has 24/26 stall-free windows versus 22/26 on baseline. Startup is slower in 9/13 matched cases and seeking slower in 8/13, despite large improvements in several of the slowest baseline seeks. The median paired production changes are approximately +1% at the opening and +22% after seeking; the much larger ratio of post-seek column medians should not be mistaken for a typical per-source speedup.

## Resource observations

Across successful observation windows, median worker peak HWM is 259 → 233 MiB, with maxima of 1076 → 1067 MiB. Median sampled hub RSS is 741 → 776 MiB, with maxima of 797 → 821 MiB. The workers include decode/encode allocations; hub state persists between some runs. These figures do not isolate the range-buffer memory cost.

Median observed worker CPU is 6.65% → 12.17% of one logical core, and hub CPU 1.19% → 1.94%. Faster production means different amounts of work and pacing within each window, so these are observed loads, not normalized CPU-efficiency comparisons. Full per-window CPU, memory, HTTP-byte, buffer and source-wait measurements are in the CSV.

## Source characteristics

| ID | Container | Video/profile | Dimensions | HDR | First audio | Mbps | Pipeline video |
|---|---|---|---|---|---|---:|---|
| M01 | matroska | av1 / main | 1920×800 | — | opus, 6ch | 7.060 | copy · fmp4 segments |
| M02 | mp4 | h264 / high | 1920×804 | — | ac3, 6ch | 8.502 | h264 copy |
| M03 | matroska | h264 / high | 1920×1080 | — | flac, 2ch | 25.392 | h264 copy · fmp4 segments |
| M04 | matroska | hevc / main-10 | 1920×804 | hdr10 | eac3, 6ch | 8.123 | hevc → h264 (transcoded) · hdr10 → sdr (tone-mapped) |
| M05 | matroska | h264 / high | 1920×1080 | — | dts, 6ch | 27.384 | h264 copy |
| M06 | video/x-msvideo | video/x-msmpeg / — | 720×400 | — | mp3, 2ch | 1.205 | video/x-msmpeg → h264 (transcoded) |
| M07 | mp4 | h264 / high | 1280×720 | — | aac, 2ch | 0.969 | h264 copy |
| M08 | matroska | hevc / main-10 | 3840×2160 | hdr10 | truehd, 8ch | 57.456 | hevc → h264 (transcoded) · hdr10 → sdr (tone-mapped) |
| M09 | matroska | av1 / main | 1920×960 | — | eac3, 6ch | 3.222 | copy · fmp4 segments |
| M10 | video/x-msvideo | mpeg4part2 / advanced-simple | 1280×688 | — | ac3, 6ch | 4.258 | mpeg4part2 → h264 (transcoded) |
| M11 | matroska | h264 / high | 1920×1080 | — | mpeg-audio, 2ch | 8.192 | h264 copy |
| M12 | mp4 | vp9 / 0 | 640×480 | — | aac, 2ch | 0.369 | copy · fmp4 segments |
| M13 | webm | vp9 / 0 | 1920×1080 | — | opus, 2ch | 2.709 | copy · fmp4 segments |
| M14 | video/x-msvideo | mpeg4part2 / — | 544×304 | — | mp3, 2ch | 0.933 | mpeg4part2 → h264 (transcoded) |
| M15 | video/x-msvideo | video/x-divx / simple | 640×480 | — | mp3, 2ch | 1.743 | video/x-divx → h264 (transcoded) |

## Startup, seek and production

Times are seconds; production is a multiple of real time. A fast first frame does not rescue the interrupted M13 run. The CSV carries explicit status and errors.

| ID | Startup s | Seek s | Opening production × | Post-seek production × |
|---|---:|---:|---:|---:|
| M01 | — → — | — → — | — → — | — → — |
| M02 | 2.65 → 2.07 | 19.79 → 5.76 | 2.46 → 17.51 | 1.68 → 16.44 |
| M03 | 4.89 → 1.02 | 53.06 → 4.92 | 0.90 → 3.99 | 0.39 → 4.94 |
| M04 | 2.31 → 3.01 | 9.07 → 10.83 | 18.32 → 8.30 | 3.86 → 14.81 |
| M05 | 4.08 → 1.75 | 20.62 → 20.97 | 1.13 → 3.35 | 0.49 → 0.47 |
| M06 | 1.45 → 3.27 | 2.14 → 3.64 | 34.70 → 66.53 | 80.09 → 55.67 |
| M07 | 1.56 → 2.37 | 1.21 → 2.53 | 85.13 → 84.35 | 99.37 → 81.42 |
| M08 | 4.89 → 5.82 | 10.98 → 51.19 | 1.81 → 1.82 | 1.27 → 1.18 |
| M09 | 1.05 → 1.69 | 1.39 → 2.08 | 44.11 → 19.40 | 24.95 → 30.40 |
| M10 | 1.45 → 2.37 | 56.33 → 3.52 | 5.95 → 28.37 | 2.47 → 27.26 |
| M11 | 7.16 → 1.58 | 17.44 → 9.41 | 2.48 → 17.34 | 1.85 → 16.15 |
| M12 | 1.02 → 1.46 | 2.10 → 3.13 | 430.43 → 334.71 | 393.20 → 256.05 |
| M13 | 0.84 → 1.13 | — → — | 10.16 → 35.32 | — → — |
| M14 | 1.15 → 2.78 | 6.01 → 7.42 | 119.40 → 76.26 | 156.96 → 88.27 |
| M15 | 1.36 → 2.35 | 16.42 → 6.58 | 80.30 → 81.07 | 6.39 → 80.46 |

## Playback progression and stalls

Each advancement column covers 120 wall seconds. A dash means no successful observation window. M13 briefly starts, then pauses following a decode error; its near-zero advancement is not a buffering stall.

| ID | Opening advancement s | Opening waits | Post-seek advancement s | Post-seek waits |
|---|---:|---:|---:|---:|
| M01 | — → — | — → — | — → — | — → — |
| M02 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M03 | 98.1 → 120.0 | 8 → 0 | 57.0 → 119.9 | 11 → 0 |
| M04 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M05 | 91.9 → 120.0 | 8 → 0 | 60.7 → 60.7 | 15 → 15 |
| M06 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M07 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M08 | 120.0 → 120.0 | 0 → 0 | 120.0 → 82.0 | 0 → 5 |
| M09 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M10 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M11 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M12 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M13 | 0.8 → 0.8 | 0 → 0 | — → — | — → — |
| M14 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |
| M15 | 120.0 → 120.0 | 0 → 0 | 120.0 → 120.0 | 0 → 0 |

## Cases requiring attention

- **M01, AV1/Opus Matroska:** no sustained playback on either revision, with startup and seek timeouts. The candidate player reported `mediaSourceRequiresReset`. The first baseline run lacks the later-added pending-state/console diagnostics; its screenshot and session log remain. No general claim about AV1 compatibility follows: M09 plays on both.
- **M03, high-bitrate H.264/FLAC Matroska:** candidate removes opening and post-seek underruns. Baseline needed seek recovery; the candidate's matched seek was much quicker.
- **M05, high-bitrate H.264/DTS Matroska:** candidate fixes opening underruns, but both post-seek windows advance only about 61 seconds and record 15 waits. Post-seek production is 0.49× → 0.47×. The worker uses about 3% of one CPU core while source-demand waits accumulate; CPU saturation is not supported by these observations. Disk, transport and source-window behavior remain possible causes; this benchmark does not identify which.
- **M08, UHD HEVC/HDR with TrueHD:** candidate regression in this run. Both openings are smooth at about 1.8×. The seek grows from 10.98 to 51.19 seconds, with candidate session recovery; post-seek advancement falls from 120 to 82 seconds, with five waits instead of zero. Baseline/candidate post-seek rates are 1.27×/1.18×. This is a video-transcode/tone-map case, not codec-copy remux.
- **M10, MPEG-4 Part 2 AVI:** both play without stalls, but baseline seek recovery takes 56.33 seconds versus 3.52 seconds on the candidate. Production also increases substantially.
- **M12, VP9/AAC MP4:** no buffer starvation, but browser frame drops on both. Opening dropped frames are 240 → 241; post-seek drops are 263 → 263, approximately 7% of frames. High production rates do not fix this symptom.
- **M13, VP9/Opus WebM:** browser `PIPELINE_ERROR_DECODE` on both revisions after roughly 0.84 seconds, with buffered data still available. Deep-seek playback also fails. Server production is measurable but is not usable playback throughput.

The priorities suggested by these measurements are to reproduce M08's seek regression and trace M05's post-seek source waits. Shared browser decode failures should be investigated separately. Repeating those comparisons with reversed order and controlled cache conditions would distinguish persistent revision effects from run-to-run storage/cache variation.

## Artifacts and limitations

The [adjacent CSV](2026-09-24-browser-origin-head.csv) contains all per-phase metrics with units in the column names. The private local bundle is `target/benchmarks/2026-09-24-origin-vs-head/`: `manifest.json` maps sample IDs to sources; `results/` contains browser samples, API responses, screenshots and diagnostics; `report.html` is sortable; `comparison.png`/`.svg` are standalone plots. `browser.mjs`, `run.py`, `summarize.py`, `paired.py`, `plot.py` and `validate.py` preserve the harness and analysis. The deployment controller is environment-specific and should be reviewed before reuse. Neither credentials nor binaries are part of the report bundle. All 30 runs completed; all 15 source pairs and all 60 phase records passed validation. Original local AIO and NAS executable hashes were verified after restoration, both hubs reported the NAS healthy, and no benchmark sessions remained.

These are single-run observations in headless Chrome on one local host and NAS. They do not establish Firefox, Android, direct-play, concurrent-session or long-duration behavior. Failures are not replaced with easier sources. The observed source mix overrepresents uncommon formats by design. Pacing is affected by suppressing progress writes as described above. Memory includes decoding/encoding and GStreamer allocations, so it cannot be interpreted as the cost of the range buffers alone.
