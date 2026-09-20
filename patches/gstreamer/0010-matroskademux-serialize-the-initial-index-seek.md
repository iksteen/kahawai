# Initial Matroska index seek races natural index parsing

The ARM64 v0.0.15-rc.1 release failed `remux::tests::starts_at_offset`
with `gst_matroska_demux_parse_id: Failed to seek`. A retry passed. Local
stress reproduced the same error in an optimized build.

In push mode, the first time seek sets `READ_STATE_SEEK`, saves the event,
and then seeks upstream to Cues. The streaming thread can reach that
trailing index naturally between the state change and the flush. It sees
the pending event and seeks to the target cluster while the application
is still starting the index seek. The flushing pad rejects one of the two
byte seeks, producing the fatal error.

Stop upstream and downstream streaming before publishing the pending
seek. Take the sink stream lock, recheck whether the index arrived while
stopping, and issue the appropriate byte seek under that lock. Reopening
upstream event delivery lets the normal byte-seek flush finish the
operation. The stream lock replaces the `building_index` flag.

The companion reproducer uses a debug callback as a rendezvous just
before natural Cues parsing, then releases it at the application's first
flush. It deterministically fails with the original error before the
patch and requires a successful six-second seek and EOS after it. It
does not retry, sleep to hide the race, or require external media.

Kahawai also sends its initial seek through one parsed stream rather than
broadcasting it through both audio and video. That removes a separate
duplicate-delivery race; it does not replace this demuxer fix.
