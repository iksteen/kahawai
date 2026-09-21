//! When a run's playlist is ready to hand to a client.
//!
//! What "enough runway" means, measured on real starts (2026-07-31,
//! Firefox): hls.js reveals new segments only on its EVENT-playlist
//! reloads (about every target duration), and production jitters (one
//! heavy GOP took 3.5 s against a ~2 s cadence) — so playback stalls
//! whenever buffered content dips under production-gap + reload ≈ 6.5 s
//! before the encoder's lead has grown past it. Hand-off with ~4 s of
//! content buffered stalled twice; ~5 s stalled once at 12 s. The gate is
//! therefore CONTENT seconds, not a segment count — three segments can be
//! as little as 4 s when scene-cut keyframes shorten them. ENDLIST (a
//! finished short encode) is always ready.

use std::path::Path;

/// The runway is capped so this gate can never be what times a start out.
/// A 66 s GOP would ask for 204 s of content, which is more than the 30 s
/// start deadline can produce however fast the source reads. Past the cap
/// the server can do nothing useful anyway: a client that honours RFC 8216
/// §6.3.3 waits on its own account whatever we hand it, and one that does
/// not is ready now — so waiting longer here only converts the client's
/// wait into our timeout.
pub const MAX_RUNWAY_SECS: f64 = 30.0;

/// The runway derived against a declared target of 2, which is what every
/// run used before the declaration followed the source.
pub const FLOOR_RUNWAY_SECS: f64 = 6.5;

/// Seconds of content a playlist must advertise before it is handed over.
///
/// The 6.5 s floor was derived against a declared target of 2: a client
/// reloads about every target duration, so the runway it needs is
/// production-gap PLUS one reload interval. Once the declaration follows
/// the source — 12 s for a 10 s-GOP file — a fixed 6.5 s hands over less
/// than the client's own refresh period and stalls on the first gap.
/// §6.3.3 pushes the same way: a client SHOULD NOT start within three
/// target durations of the end.
///
/// Not the cure for extreme sources, and it should not be mistaken for
/// one: a file whose keyframes are 66 s apart cannot close a single
/// segment inside the deadline no matter what this returns, because one
/// segment IS one GOP. Those need `short`, which re-encodes and gives them
/// keyframes of our own choosing.
///
/// `None` is a run whose declaration is not known here — a transcoder fed
/// by a hub too old to say — and gets the floor, exactly as before.
pub fn runway_secs(target_duration_secs: Option<u32>) -> f64 {
    match target_duration_secs {
        Some(target) if target > 0 => {
            FLOOR_RUNWAY_SECS.max((3.0 * target as f64).min(MAX_RUNWAY_SECS))
        }
        _ => FLOOR_RUNWAY_SECS,
    }
}

/// Is the playlist at `path` ready: ENDLIST, or the runway's worth of
/// content advertised?
pub fn playlist_ready(path: &Path, target_duration_secs: Option<u32>) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => playlist_text_ready(&text, target_duration_secs),
        Err(_) => false,
    }
}

/// [`playlist_ready`] on text already read.
pub fn playlist_text_ready(playlist: &str, target_duration_secs: Option<u32>) -> bool {
    playlist.contains("#EXT-X-ENDLIST")
        || playlist_span_secs(playlist) >= runway_secs(target_duration_secs)
}

/// Total seconds of content a playlist advertises (Σ EXTINF).
pub fn playlist_span_secs(playlist: &str) -> f64 {
    playlist
        .lines()
        .filter_map(|line| {
            line.strip_prefix("#EXTINF:")?
                .trim_end_matches(',')
                .parse::<f64>()
                .ok()
        })
        .sum()
}

/// Has the run finished, as far as the playlist says?
pub fn playlist_finished(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.contains("#EXT-X-ENDLIST"))
        .unwrap_or(false)
}
