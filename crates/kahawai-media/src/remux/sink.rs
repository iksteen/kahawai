use super::*;

/// HLS sink elements in preference order: hlssink3 (gst-plugins-rs, better
/// maintained, richer playlist control) over hlssink2 (plugins-bad).
/// Plugin-fallback strategy: pick the best available at runtime, set
/// properties guarded by existence so element/version differences degrade
/// instead of panicking, and let `doctor` recommend the preferred one.
pub const HLS_SINKS: &[&str] = &["hlssink3", "hlssink2"];

/// Set a property only if this element (version) has it.
pub(super) fn set_prop_if_present<V: Into<gst::glib::Value>>(
    el: &gst::Element,
    name: &str,
    value: V,
) {
    use gst::glib::prelude::ObjectExt;
    if el.find_property(name).is_some() {
        el.set_property_from_value(name, &value.into());
    } else {
        tracing::debug!(element = %el.name(), property = name, "property not present; skipped");
    }
}

/// Same, for enum properties set by nick.
pub(super) fn set_prop_str_if_present(el: &gst::Element, name: &str, value: &str) {
    use gst::glib::prelude::ObjectExt;
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, value);
    } else {
        tracing::debug!(element = %el.name(), property = name, "property not present; skipped");
    }
}

/// Best available HLS sink, configured for `out_dir`. Returns the element
/// and its factory name (for logs/tests).
pub(super) fn make_hls_sink(
    out_dir: &Path,
    prefer: Option<&str>,
) -> Result<(gst::Element, &'static str)> {
    // An explicit preference (retry-after-sink-crash, TC-6) wins if the
    // element exists; otherwise the usual best-available order.
    let name = prefer
        .and_then(|p| {
            HLS_SINKS
                .iter()
                .find(|n| **n == p && gst::ElementFactory::find(n).is_some())
        })
        .or_else(|| {
            HLS_SINKS
                .iter()
                .find(|n| gst::ElementFactory::find(n).is_some())
        })
        .context("no HLS sink element (hlssink3/hlssink2) — see `kahawai doctor`")?;
    let sink = gst::ElementFactory::make(name).build()?;
    set_prop_if_present(
        &sink,
        "location",
        out_dir.join("segment%05d.ts").to_str().unwrap(),
    );
    set_prop_if_present(
        &sink,
        "playlist-location",
        out_dir.join("master.m3u8").to_str().unwrap(),
    );
    // Segments cut at the first keyframe AT OR PAST the target, so with
    // the encoders' 2 s GOP every segment runs slightly OVER 2 s — and
    // the spec requires TARGETDURATION >= ceil(max segment duration),
    // which hlssink3 writes verbatim from this property. 3 is therefore
    // the smallest spec-valid value for ~2 s segments (2 produced
    // playlists that violated it).
    // The fragment interval the sink asks keyframes at, and therefore
    // the segment length: ~2 s keeps the session-start gate (three
    // segments) at ~6 s of content.
    set_prop_if_present(&sink, "target-duration", FRAGMENT_TARGET_SECS);
    // ON, and load-bearing: the request is what puts a keyframe at the
    // START of the first fragment. Turned off for one evening to stop
    // the sink racing a frame-based GOP pin, it made segment00000
    // undecodable — the fragment opened before the encoder's first IDR,
    // so the segment carried slices with no SPS/PPS and every player
    // wedged on it at ~1 s while the worker happily produced two minutes
    // more. The double-cut it was meant to fix was cosmetic; this was
    // not. The pin above is now a far-away backstop instead, so the two
    // no longer compete.
    //
    // Copy sessions are unaffected either way: with no encoder upstream
    // there is nothing to honour a keyframe request, so their splits
    // follow the source's own keyframes.
    set_prop_if_present(&sink, "send-keyframe-requests", true);
    // Keep every segment and playlist entry (VOD-style growing playlist).
    set_prop_if_present(&sink, "playlist-length", 0u32);
    set_prop_if_present(&sink, "max-files", 0u32); // hlssink2
    set_prop_if_present(&sink, "max-num-segment-files", 0u32); // hlssink3
    // EVENT: players may seek within already-produced segments while the
    // remux is still running (ENDLIST still lands at EOS). hlssink3 only.
    set_prop_str_if_present(&sink, "playlist-type", "event");
    tracing::info!(sink = name, "HLS sink selected");
    Ok((sink, name))
}

/// Pacing window (§4.6): hold muxer-bound buffers whose media time runs
/// more than `window_ms` past the viewer's position (read from
/// `viewer_file`, absolute ms; absent = `floor_ms`). In-band and
/// deterministic — polling pause/resume loses to pipelines that finish
/// a file between two polls.
///
/// The window alone is not safe on its own, because a client that never
/// reports a position leaves `viewer_file` at the floor: production
/// then stops for good at floor+window and the playlist stops changing
/// with it. A LIVE playlist that stops changing is a client-side error
/// — ExoPlayer's `PlaylistStuckException` fires after 3.5 target
/// durations of no change — so the pacer also releases on playlist AGE
/// (`stale_ms`), which is what keeps a non-reporting viewer playing.
pub struct PaceConfig {
    pub window_ms: u64,
    pub floor_ms: u64,
    pub viewer_file: std::path::PathBuf,
    /// HUB-36: where to drop `pace.json` — the run dir, alongside
    /// `start.pos` and `viewer.pos`.
    pub out_dir: std::path::PathBuf,
    /// Let production through once `master.m3u8` has been untouched for
    /// this long, whatever the viewer says. `None` reads the allowance
    /// from the playlist's own target duration; `Some(0)` turns the
    /// release off and leaves the window alone.
    ///
    /// Self-limiting rather than a widened window: the escape holds only
    /// until the sink closes the next segment and rewrites the playlist,
    /// at which point the file is fresh and the window blocks again. A
    /// stalled session therefore produces about one segment per
    /// allowance — roughly real time, since a segment is about one
    /// target duration of media — instead of freezing or running away.
    pub stale_ms: Option<u64>,
}

/// Staleness allowance before any playlist says otherwise.
pub const PACE_STALE_FALLBACK_MS: u64 = 6_000;

/// A LIVE client's own bound: ExoPlayer's `DefaultHlsPlaylistTracker`
/// errors after this many declared target durations of no change.
pub(super) const CLIENT_STUCK_TARGETS: f64 = 3.5;

/// How much of that bound we are willing to spend. The rest pays for
/// the segment's own production, which is not instant — a heavy GOP has
/// been measured at 3.5 s against a ~2 s cadence.
pub(super) const STALE_SAFETY: f64 = 0.7;

/// How long the playlist may sit unchanged before the pacer lets a
/// segment through, read from the playlist the sink is writing.
///
/// Two numbers, and the smaller wins:
///
/// - **the longest segment** the playlist lists. Each release produces
///   about one segment, so this is what sets the leak rate: allowing one
///   segment's worth of wall time per segment of media paces a
///   non-reporting viewer at about real time, which is the most it can
///   consume anyway. Measured before this was in: a session declaring 2
///   while cutting 5.18 s GOPs released every 2 s and ran at 2.6x,
///   which is most of the pacing window given away.
/// - **the client's tolerance**, `3.5 x EXT-X-TARGETDURATION`, spent at
///   70%. A declaration that undersells the real segment length (the
///   `ignore` profile keeps a constant 2) makes the client the binding
///   constraint, and being right with the client beats pacing.
///
/// Read fresh each time rather than cached: a sink's first playlist can
/// precede both its final target duration and its typical segment.
pub(super) fn playlist_stale_allowance_ms(
    playlist: &std::path::Path,
    configured: Option<u64>,
) -> u64 {
    if let Some(ms) = configured {
        return ms;
    }
    let Ok(text) = std::fs::read_to_string(playlist) else {
        return PACE_STALE_FALLBACK_MS;
    };
    let declared = text.lines().find_map(|l| {
        l.strip_prefix("#EXT-X-TARGETDURATION:")?
            .trim()
            .parse::<f64>()
            .ok()
    });
    let longest = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("#EXTINF:")?
                .trim_end_matches(',')
                .parse::<f64>()
                .ok()
        })
        .fold(f64::NAN, f64::max);

    let client_cap = declared
        .filter(|d| *d > 0.0)
        .map(|d| d * CLIENT_STUCK_TARGETS * STALE_SAFETY);
    let paced = if longest.is_finite() && longest > 0.0 {
        Some(longest)
    } else {
        None
    };
    // Rounded, not truncated: 3.5 x 0.7 lands a millisecond short of
    // the number the comment claims, which reads as a bug forever after.
    match (paced, client_cap) {
        (Some(p), Some(c)) => (p.min(c) * 1000.0).round() as u64,
        (Some(v), None) | (None, Some(v)) => (v * 1000.0).round() as u64,
        (None, None) => PACE_STALE_FALLBACK_MS,
    }
}

/// Whether the playlist has gone unwritten long enough to release the
/// pacer. `allow_ms` of 0 never releases.
///
/// A missing playlist counts as stale. Nothing has been produced yet,
/// so there is no viewer to pace against and no segment for a client to
/// be sitting on — holding production there would stall a start rather
/// than protect anything. In a real session the window is minutes wide
/// and the first segment lands long before it shuts, so this arm is
/// reached only by a restart's empty directory or a degenerate window.
pub(super) fn playlist_stale(playlist: &std::path::Path, allow_ms: u64) -> bool {
    if allow_ms == 0 {
        return false;
    }
    match std::fs::metadata(playlist).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age.as_millis() as u64 >= allow_ms)
            .unwrap_or(false),
        Err(_) => true,
    }
}

/// HUB-36: how fast this box ACTUALLY produced content, measured only
/// while nothing was holding it back.
///
/// The trap this exists to dodge: steady-state production is throttled
/// to viewer+window by the probe below, so a session measured end to
/// end reports ~1.0× **because we paced it** — the number would say
/// "every box is realtime" and rank nothing. Honest pace is visible
/// only while the throttle is asleep: from the first buffer until the
/// window check first fails (or a cap, for a box slow enough never to
/// fill the window). Post-seek catch-up is un-throttled by the same
/// definition, and each seek-restart is a fresh worker, so it gets its
/// own sample.
pub(super) struct PaceMeter {
    pub(super) t0: Option<std::time::Instant>,
    pub(super) first_ms: u64,
    pub(super) done: bool,
}

/// A sample needs this much CONTENT to mean anything — below it the
/// ratio is dominated by preroll burst rather than throughput.
pub(super) const PACE_MIN_CONTENT: u64 = 5_000;
/// …and this much WALL time, only to keep timer jitter out. It is
/// deliberately small: a fast box produces the whole pacing window in
/// under a second, and rejecting that would discard precisely the
/// boxes worth ranking highest (found by test — a local copy-remux
/// cleared a 6 s window in ~0.3 s and reported nothing at all).
pub(super) const PACE_MIN_WALL: u64 = 1_000;
/// A box near 1.0× may never fill the window; stop measuring anyway.
pub(super) const PACE_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// content-ms over wall-ms as a realtime multiple, or None when the
/// sample is too short on either axis to mean anything (the opening
/// seconds are preroll burst, not throughput).
pub(crate) fn pace_multiple(content_ms: u64, wall_ms: u64) -> Option<f64> {
    if wall_ms < PACE_MIN_WALL || content_ms < PACE_MIN_CONTENT {
        return None;
    }
    Some(content_ms as f64 / wall_ms as f64)
}

impl PaceMeter {
    /// Close the measurement and write it out. `produced_ms` is the
    /// position of the buffer that ended it.
    pub(super) fn finish(&mut self, produced_ms: u64, out_dir: &std::path::Path) {
        self.done = true;
        let Some(t0) = self.t0 else { return };
        let wall = t0.elapsed();
        let content = produced_ms.saturating_sub(self.first_ms);
        let Some(multiple) = pace_multiple(content, wall.as_millis() as u64) else {
            return; // too short to mean anything
        };
        let path = out_dir.join("pace.json");
        let body = format!("{{\"multiple\":{multiple:.3}}}");
        if let Err(e) = std::fs::write(&path, body) {
            tracing::debug!(error = %e, "pace sample not written");
            return;
        }
        tracing::info!(
            multiple = format!("{multiple:.2}"),
            content_ms = content,
            wall_ms = wall.as_millis() as u64,
            "un-throttled production measured (HUB-36)"
        );
    }
}

pub(super) fn install_pace_probe(
    pad: &gst::Pad,
    cfg: Arc<PaceConfig>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    meter: Option<Arc<Mutex<PaceMeter>>>,
) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(gst::PadProbeData::Buffer(b)) = &info.data
            && let Some(pts) = b.pts()
        {
            // Raw PTS can carry arbitrary bases (x264's 1000-hour
            // epoch); running time is the honest produced-position —
            // it starts at zero for the run, so add the start offset.
            let Some(rt) = pad
                .sticky_event::<gst::event::Segment>(0)
                .and_then(|e| e.segment().downcast_ref::<gst::ClockTime>().cloned())
                .and_then(|seg| seg.to_running_time(pts))
            else {
                return gst::PadProbeReturn::Ok;
            };
            let produced_ms = cfg.floor_ms + rt.mseconds();
            // HUB-36: the clock starts at the first buffer of the run.
            if let Some(m) = &meter {
                let mut m = m.lock().unwrap();
                if !m.done && m.t0.is_none() {
                    m.t0 = Some(std::time::Instant::now());
                    m.first_ms = produced_ms;
                } else if !m.done && m.t0.is_some_and(|t| t.elapsed() >= PACE_CAP) {
                    // Never filled the window — a ~1.0x box. Its speed
                    // is exactly what the last minute showed.
                    m.finish(produced_ms, &cfg.out_dir);
                }
            }
            loop {
                if stopping.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let viewer = std::fs::read_to_string(&cfg.viewer_file)
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(cfg.floor_ms)
                    .max(cfg.floor_ms);
                if produced_ms <= viewer + cfg.window_ms {
                    break;
                }
                // THE signal: production has outrun the viewer window,
                // so from here on the throttle — not the box — sets the
                // rate. Whatever we measured up to now is the honest
                // number; everything after would read ~1.0x.
                if let Some(m) = &meter {
                    let mut m = m.lock().unwrap();
                    if !m.done {
                        m.finish(produced_ms, &cfg.out_dir);
                    }
                }
                // Release on playlist age as well as viewer position. A
                // viewer that never reports leaves the window shut for
                // good, and a LIVE playlist that never changes is a
                // client error, not a quiet server. Measured after the
                // meter so the throttle still marks the end of the
                // honest measurement.
                let playlist = cfg.out_dir.join("master.m3u8");
                if playlist_stale(
                    &playlist,
                    playlist_stale_allowance_ms(&playlist, cfg.stale_ms),
                ) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        gst::PadProbeReturn::Ok
    });
}
