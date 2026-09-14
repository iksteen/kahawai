# hlssink3: EXTINF must be the distance to the next fragment

**Upstream:** not reported yet. **Observed on:** gst-plugins-rs
`gstreamer-1.28.6` (the tag `kahawai-gst-plugins.sh` builds), GStreamer
1.28.6. **Reproducer:** `…-repro-1.py`.

hlssink3 writes splitmuxsink's `fragment-duration` straight into `EXTINF`.
That field is not the fragment's length. `update_output_fragment_info` in
`gstsplitmuxsink.c` computes it as

    /* Look for the largest duration across all streams */
    ctx_duration = ctx->out_running_time_end - splitmux->out_fragment_start_runts;
    if (ctx_duration > duration) duration = ctx_duration;

— the LONGEST stream's end minus the fragment start. Fragments begin on
the reference stream's keyframes, so whenever another stream ends later,
that value runs past where the next fragment starts. Audio is that other
stream in every ordinary file: its frames do not align to a video GOP.

`EXTINF` has to be the distance to the next segment. Using a value that
overshoots it makes every segment declare time belonging to its
successor, and the playlist gains that surplus for ever.

## What it costs

A 50 minute h264 + AAC remux, 1504 segments, 23.976 fps with 48-frame
GOPs, cut identically by both sinks:

| playlist | total | error |
| --- | --- | --- |
| source file | 3009.600 s | — |
| hlssink2 | 3009.548 s | −52 ms |
| hlssink3, stock | 3024.629 s | **+15.029 s** |
| hlssink3, patched | 3009.548 s | −52 ms |

Per segment, stock hlssink3 declares 2.005 to 2.018 s where the video
span is a rock-steady 2.002 s (48 × 1001/24000 exactly). hlssink2, from
the same cut points, declares 2.002 every time.

## Why it is worse than a cosmetic error

A player's playhead comes from decoded sample timestamps; its buffered
position comes from the playlist. The gap between them grows by the
surplus on every segment, so the player increasingly believes it holds
media it does not have. When that phantom buffer reaches the player's
minimum-buffer threshold, its load control sees a full buffer and stops
fetching while the renderer has genuinely run out. Nothing recovers: the
phantom never drains, so the threshold is never crossed back.

The stall lands at `minimum buffer / drift rate`. Measured on an ExoPlayer
client configured with a 10 s minimum: 10 / 0.004988 ≈ 2005 s, against
four observed stalls at 1985.75 – 1985.79 s. Media3's stock 15 s puts it
at ~50 minutes, which is why this hides in anything shorter than a film.
Seeking clears it, because a fresh pipeline resets the accumulation.

## The fix

Prefer the reference stream's own running times, which the
`splitmuxsink-fragment-opened` / `-closed` messages already carry, and
keep `fragment-duration` only as the fallback for a fragment whose opened
message had no running time.

Also switches `duration_msec` from `mseconds() as f32 / 1000` to
nanoseconds: truncating to whole milliseconds loses ~35 µs per segment.
That one is genuinely cosmetic at this scale and is included because it
is the same expression.
