//! Pure helpers around seeking a multi-part timeline (HUB-17).
//!
//! CD1/CD2-era rips play as one continuous timeline; a part boundary is an
//! ordinary seek-restart into the next file. These functions decide which
//! part a timestamp lives in and which hosts a session depends on, and
//! coalesce a burst of seek intents into the one that should run.

/// One part of a session's timeline, as far as seeking is concerned.
pub trait TimelinePart {
    /// Absolute offset of this part's first millisecond.
    fn base_ms(&self) -> u64;
    /// The mediahost holding this part's bytes.
    fn module_id(&self) -> &str;
}

/// Index of the part containing `abs_ms`. `base_ms` is inclusive: the
/// boundary itself is already the next part. Past the end clamps to the
/// last part rather than panicking — a seek beyond the timeline is a UI
/// rounding error, not a crash.
pub fn part_index<P: TimelinePart>(parts: &[P], abs_ms: u64) -> usize {
    parts
        .iter()
        .rposition(|part| abs_ms >= part.base_ms())
        .unwrap_or(0)
}

/// Does a session started on `start_host` read from `module_id`? Every
/// part counts, played or not: the viewer can seek back into CD1 whenever
/// they like, so a host that holds ANY part going away ends the session
/// (AR-6) rather than leaving it alive on a lease that will fail minutes
/// later with nothing to explain it.
pub fn reads_from<P: TimelinePart>(start_host: &str, parts: &[P], module_id: &str) -> bool {
    start_host == module_id || parts.iter().any(|part| part.module_id() == module_id)
}

/// A coalesced seek intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSeek {
    pub generation: u64,
    pub position_ms: u64,
    pub audio_track: Option<u32>,
    pub video_track: Option<u32>,
    /// The burn pick changed (subtitle unification): re-plan even when
    /// the audio/video tracks stayed put.
    pub replan_subs: bool,
}

impl PendingSeek {
    /// The newer intent wins the position; explicit track choices
    /// survive supersession (`None` = "keep current").
    pub fn merge(prev: Option<PendingSeek>, next: PendingSeek) -> PendingSeek {
        match prev {
            Some(prev) => PendingSeek {
                audio_track: next.audio_track.or(prev.audio_track),
                video_track: next.video_track.or(prev.video_track),
                replan_subs: next.replan_subs || prev.replan_subs,
                ..next
            },
            None => next,
        }
    }
}
