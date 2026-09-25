# Finding interleaving candidates

Run the standalone Python 3 scanner where the media is stored. It requires
`ffprobe`, reads files without decoding, and never writes media. It operates
outside the hub and does not create playback sessions or background work.

```sh
python3 scripts/kahawai-interleave.py /media/multimedia/movies \
  --limit 100 --stop-after 5 > interleave.jsonl
```

Each completed file emits a JSON record immediately. The last record contains
a ranked shortlist. Progress goes to stderr. `--timeout` bounds each ffprobe
invocation (default 120 seconds); `--limit` and `--stop-after` default to unlimited.
Errors appear as records and make the final exit code nonzero; they are not
silently counted as successful scans. Ctrl-C stops the current probe.

The scanner compares the first non-cover-art video track with each audio track
separately. For each one-second decode-timestamp bucket (presentation timestamp
as fallback), it records the minimum and maximum packet byte positions per
track. The distance between *disjoint* audio/video envelopes is compared with
2 MiB and 16 MiB analysis thresholds. The JSON labels `transcoder` and `hub`
are retained for existing reports; they do not describe current runtime buffer
settings. Overlapping
envelopes count as zero gap, even at high bitrates. Five consecutive affected
seconds flag a candidate (`--min-run` changes this heuristic threshold).
Candidates rank by longest run exceeding 16 MiB, then 2 MiB, then maximum separation. The first three qualifying run starts are
reported for each track pair so manual playback can target them.

This is a layout-risk shortlist, **not a stall prediction or a simulation of
the source implementation**. Distant tracks can play well. GStreamer can choose a
different read order and read size from ffprobe; retained ranges, demux queues,
source bandwidth, latency and the client's buffered time determine whether
playback stalls. The raw backward-step count is in ffprobe's emission order and
is supplementary evidence only. A negative result does not certify a file:
sub-second patterns, alternate video tracks, missing packet positions/timestamps
and formats with overlapping envelopes can hide troublesome layouts. Missing
packet metadata and probe warnings are explicitly reported as incomplete
coverage; no common audio/video buckets is inconclusive.

Cost: full packet inspection reads the file (and can seek), but does not decode
or generate remux output. Memory holds only per-second track envelopes. Files
are processed serially to avoid saturating NAS storage. The practical question
is which files may cause repeated source I/O to rebuild discarded ranges and
latency waiting for those ranges at playback time. This scanner selects examples
for that follow-up; it does not assign a throughput or stall probability.

For manual verification, use the reported audio stream and position through the
hub and inspect segment production and source-delivery diagnostics. ffprobe stream
indices include video and subtitles; the report also includes the zero-based
audio-track ordinal, codec and language for matching the player selection. Repeat with the intended local/remote transcoder
placement. Do not treat an unsupported codec or demux error as interleaving.

Regression checks:

```sh
python3 scripts/test-kahawai-interleave.py
```
