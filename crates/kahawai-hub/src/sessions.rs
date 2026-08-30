//! Play sessions (HUB-18 minimal): direct play and in-hub remux (AR-10).
//!
//! ponytail: sessions are in-memory (lost on hub restart, clients reopen);
//! idle timeout and per-user concurrency limits land with HUB-18 proper.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::{HubToHost, OpenRead, hub_to_host};
use sqlx::Row;

use crate::leases::{Lease, Leases, LocalAdmission, new_lease_token};
use crate::registry::{LoudnessPreference, Registry};

/// Who a read lease is for. It travels to the mediahost, which serves both
/// identically and schedules its OWN local work — hashes, declarations,
/// probes, extractions — around whether it is serving a viewer.
///
/// The distinction has to be stated because the host cannot infer it: bytes
/// are bytes. Without it, a sweep reading every episode in the library is
/// indistinguishable from somebody watching all day, and the host's queues
/// never drain — measured, hours of intro detection during which not one
/// file was declared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reader {
    /// Somebody is waiting on these bytes.
    Viewer,
    /// The hub's own background work, which can be outrun by anything.
    Sweep,
}

pub enum Mode {
    Direct {
        lease: Lease,
    },
    Remux {
        dir: PathBuf,
        runner: Mutex<RemuxRunner>,
    },
    /// Dispatched to a transcoder module; artifacts proxied on demand.
    /// The module can change: AR-6 reschedules onto a new box.
    Transcode {
        transcoder: Mutex<String>,
    },
}

/// Every other refusal from `start` is about the item: it has no sources, it
/// cannot be played, you already hold too many streams. This one is about the
/// moment — the bytes exist, on a host that is not answering right now — and
/// the same request may well succeed in a minute.
///
/// A type rather than a sentence, because the caller has to ACT on the
/// difference: it becomes 503 at the API edge, and a client that sees 503
/// stands by and tries again instead of giving up. Matching that distinction
/// out of an error message would break the first time someone rewords it.
#[derive(Debug, Clone, Copy)]
pub struct SourceOffline;

impl std::fmt::Display for SourceOffline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no source is currently available (mediahost offline)")
    }
}

impl std::error::Error for SourceOffline {}

/// This account already holds as many playback sessions as it may.
///
/// A type rather than a `bail!` sentence because it is the one refusal from
/// this layer that clears on its own — as soon as any of them ends. Every
/// other one is about the item and will refuse again forever. They arrived at
/// the API as the same 409 with the difference only in the prose, which no
/// client may read: a client playing a LIST rather than one item has to tell
/// "wait" from "give up", and the album queue holds two sessions, so a film
/// playing beside it is enough to reach the limit.
#[derive(Debug)]
pub struct SessionCap {
    pub held: usize,
}

impl std::fmt::Display for SessionCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many concurrent streams ({}); close one first",
            self.held
        )
    }
}

impl std::error::Error for SessionCap {}

/// A satellite that should have answered did not — not connected, or
/// connected and silent past a deadline.
///
/// A type because the API cannot otherwise tell it from absence: collecting a
/// session's logs from a wedged transcoder was answering "no logs for that
/// session", on the one route an operator reaches for when a session is
/// misbehaving.
#[derive(Debug)]
pub struct SatelliteSilent(pub String);

impl std::fmt::Display for SatelliteSilent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SatelliteSilent {}

/// The subtitle track a REQUEST named is not on that item.
///
/// A type because this is the caller's input reaching a layer whose every
/// other failure is about the item. Folded in with those it arrived as 409
/// "this item's playback could not be negotiated" — which under the API's
/// contract means final, so a client asking for track 999 on a perfectly
/// playable film concluded the film was dead.
#[derive(Debug)]
pub struct NoSuchTrack {
    pub item: String,
    pub track: i64,
}

impl std::fmt::Display for NoSuchTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no subtitle track {} on item {}", self.track, self.item)
    }
}

impl std::error::Error for NoSuchTrack {}

/// How a remux/transcode pipeline runs. The hub always spawns the
/// supervised worker (§1.1: a GStreamer crash kills one session, not the
/// process — observed twice on a real library via hlssink3 panics).
/// ponytail: in-process kept for tests, which have no worker binary.
pub enum RemuxRunner {
    InProcess(Arc<kahawai_media::remux::RemuxJob>),
    Worker(Mutex<tokio::process::Child>),
    /// Placeholder while a seek-restart swaps runners.
    Stopped,
}

impl RemuxRunner {
    fn stop(&self) {
        match self {
            RemuxRunner::InProcess(job) => job.stop(),
            RemuxRunner::Worker(child) => {
                let _ = child.lock().unwrap().start_kill();
            }
            RemuxRunner::Stopped => {}
        }
    }

    /// Stop and wait until the pipeline is really gone — a seek-restart
    /// recreates the scratch dir, and a still-dying worker writing into
    /// it corrupts the new run.
    async fn stop_and_wait(&self) {
        self.stop();
        if let RemuxRunner::Worker(child) = self {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if matches!(child.lock().unwrap().try_wait(), Ok(Some(_)) | Err(_)) {
                    return;
                }
                if std::time::Instant::now() > deadline {
                    tracing::warn!("old worker did not exit in time; proceeding");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

/// One file of a (possibly multi-part) source, with its absolute
/// timeline offset. CD1/CD2-era rips: parts play as one continuous
/// timeline; part boundaries are ordinary seek-restarts.
#[derive(Debug, Clone)]
pub struct PartSource {
    pub file_id: i64,
    pub module_id: String,
    pub collection_id: String,
    pub root_token: String,
    pub path_rel: String,
    pub size: u64,
    /// The file's modification time, for callers whose records are about
    /// BYTES: the detector keys its scan rows on the mtime of the rendition
    /// it actually read.
    pub mtime_unix: i64,
    pub base_ms: u64,
    pub duration_ms: u64,
}
fn apply_audio_loudness_measurement(
    plan: &mut kahawai_media::remux::RemuxPlan,
    preference: LoudnessPreference,
    measured: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
) {
    plan.stereo_gain_db = None;
    plan.native_gain_db = None;
    plan.loudness_source_channels = None;
    plan.loudness_gains = [None; kahawai_media::loudness::MAX_LAYOUT_GAINS];
    if preference.enabled()
        && let Some(measured) = measured
    {
        for (slot, measurement) in plan.loudness_gains.iter_mut().zip(&measured.layouts) {
            *slot = Some(kahawai_media::loudness::AudioLayoutGain {
                layout: measurement.layout,
                gain_db: kahawai_media::loudness::gain_db(measurement.loudness),
            });
        }
        plan.loudness_source_channels = Some(measured.source.channels);
        plan.native_gain_db = measured
            .get(measured.source)
            .map(kahawai_media::loudness::gain_db);
        plan.stereo_gain_db = measured
            .get(kahawai_media::loudness::AudioLayout::new(2, 0x3))
            .or_else(|| {
                measured
                    .get(measured.source)
                    .filter(|_| measured.source.channels <= 2)
            })
            .map(kahawai_media::loudness::gain_db);
    }
}

async fn fill_audio_loudness_gains(
    registry: &Registry,
    parts: &[PartSource],
    plan: &mut kahawai_media::remux::RemuxPlan,
    preference: LoudnessPreference,
    known: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
) -> Result<()> {
    let measured = if !preference.enabled()
        || plan.audio != kahawai_media::remux::StreamMode::Encode
        || parts.len() != 1
    {
        None
    } else if preference.force() {
        known
    } else {
        registry
            .audio_loudness(parts[0].file_id, plan.audio_track)
            .await?
    };

    apply_audio_loudness_measurement(plan, preference, measured);
    Ok(())
}
fn same_video_path(
    left: &kahawai_media::remux::RemuxPlan,
    right: &kahawai_media::remux::RemuxPlan,
) -> bool {
    left.video == right.video
        && left.video_track == right.video_track
        && left.video_kbps == right.video_kbps
        && left.max_height == right.max_height
        && left.tone_map == right.tone_map
        && left.deinterlace == right.deinterlace
        && left.burn_subtitle == right.burn_subtitle
        && left.burn_ass == right.burn_ass
        && left.video_codec == right.video_codec
        && left.segment_format == right.segment_format
}

fn replanned_verdict(
    plan: &kahawai_media::remux::RemuxPlan,
    video_verdict: &str,
    audio_verdict: &str,
) -> Result<(String, String)> {
    anyhow::ensure!(plan.playable(), "selected track is not playable");
    Ok((video_verdict.to_owned(), audio_verdict.to_owned()))
}

fn loudness_protocol_feature(
    plan: &kahawai_media::remux::RemuxPlan,
) -> Option<kahawai_proto::ProtocolFeature> {
    plan.loudness_gains
        .iter()
        .any(Option::is_some)
        .then_some(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains)
}

fn wire_scalar_loudness(
    plan: &kahawai_media::remux::RemuxPlan,
) -> (Option<f64>, Option<f64>, Option<u32>) {
    // Presence is authoritative in the protocol-4 baseline. Sentinels retain
    // the worker argv's distinction between absent and an exact 0 dB value.
    (
        Some(plan.stereo_gain_db.unwrap_or(f64::NAN)),
        Some(plan.native_gain_db.unwrap_or(f64::NAN)),
        Some(plan.loudness_source_channels.unwrap_or(0)),
    )
}

fn placement_need(
    plan: &kahawai_media::remux::RemuxPlan,
    info: &kahawai_core::media::MediaInfo,
    parts: &[PartSource],
    burns_ass: bool,
) -> (crate::registry::PlacementNeed, String) {
    use kahawai_media::remux::StreamMode;

    let class = if plan.video == StreamMode::Encode {
        let video = info.video.first();
        crate::pace::work_class(
            video.map_or(0, |video| video.height),
            video.map_or("", |video| video.codec.as_str()),
            plan.video_codec.as_str(),
            plan.tone_map,
        )
    } else {
        String::new()
    };
    let need = crate::registry::PlacementNeed {
        encode_video: plan.video == StreamMode::Encode,
        encode_audio: plan.audio == StreamMode::Encode,
        video_caps: kahawai_media::remux::source_caps_names("video", info),
        audio_caps: kahawai_media::remux::source_caps_names("audio", info),
        needs_tonemap: plan.tone_map,
        needs_ass_burn: burns_ass,
        // Audio-only sessions stay local (`Registry::place`), while a session
        // already bound to a full transcoder remains remote even if a track
        // switch changes video to copy. Preserve the feature for failover.
        required_protocol_feature: loudness_protocol_feature(plan),
        video_codec: if plan.video == StreamMode::Encode {
            plan.video_codec.as_str().to_string()
        } else {
            String::new()
        },
        audio_codec: if plan.audio == StreamMode::Encode {
            plan.audio_codec.as_str().to_string()
        } else {
            String::new()
        },
        work_class: (!class.is_empty()).then(|| class.clone()),
        source_kbps: info
            .duration_ms
            .filter(|duration| *duration > 0)
            .map(|duration| {
                ((parts.iter().map(|part| part.size).sum::<u64>() * 8) / duration) as u32
            }),
    };
    (need, class)
}

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub module_id: String,
    pub size: u64,
    /// All parts in timeline order (len 1 for single-file sources).
    pub parts: Vec<PartSource>,
    pub current_part: std::sync::atomic::AtomicUsize,
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    pub mode: Mode,
    /// Per-kind stream verdict (remux sessions): what happened to video
    /// and audio, for the player's playback-info overlay. LIVE state: a
    /// track switch re-plans and the verdict must say what is playing
    /// NOW, not what played at session start.
    pub verdict: Mutex<Option<(String, String)>>,
    /// Per-subtitle-stream tier verdicts (HUB-32a/b) — additive in the
    /// API response. LIVE state like `verdict`: a track-switch re-plan
    /// must say what is happening NOW.
    pub sub_verdicts: Mutex<Vec<kahawai_media::negotiate::SubtitleVerdict>>,
    /// The effective capability profile (client's or fallback, cap
    /// merged) — re-plans on track switches negotiate against IT.
    profile: kahawai_core::media::CapabilityProfile,
    /// The global loudness preference resolved at session start. A track
    /// switch re-plans against the same user decision as the initial stream.
    loudness: LoudnessPreference,
    /// This session started on a measured force-capable source. Explicit
    /// direct and fallback sessions keep their original-byte contract.
    force_loudness: bool,
    /// What this session's playlist declares as EXT-X-TARGETDURATION.
    ///
    /// Decided ONCE, at session start, and deliberately not a Mutex
    /// like the verdicts beside it: RFC 8216 §6.2.1 forbids the value
    /// changing once published, and a seek re-plan or a track switch
    /// must not move it even if the re-plan would now compute
    /// something else.
    pub target_duration_secs: u32,
    /// HUB-32b: the display sets this session burns, if any. A seek
    /// restarts the pipeline, which must burn the same subtitles. LIVE:
    /// a mid-session subtitle pick swaps them.
    burn_sets: Mutex<Option<std::path::PathBuf>>,
    /// An explicit image-track pick (subtitle unification): seeks and
    /// track-switch re-plans keep forcing the burn it asked for, and a
    /// new pick replaces it.
    burn_pick: Mutex<Option<kahawai_media::negotiate::BurnPick>>,
    /// HUB-32a/d: this user's ASS ladder. Carried on the session
    /// because a seek re-negotiates and must reach the same decision —
    /// against THIS executor's capability, since a seek cannot move
    /// boxes.
    ass: kahawai_media::negotiate::AssPolicy,
    /// HUB-32a: the sidecar script this session burns, if any. Held as
    /// text for the same reason `start_remux` takes it that way — the
    /// session dir is wiped on every restart.
    burn_ass_text: Mutex<Option<String>>,
    /// The HLS sink this session's content actually works on. Some
    /// files crash hlssink3 (TC-6); once the fallback saved a start or
    /// a seek, every later restart uses it directly instead of paying
    /// a crash per restart.
    sink: Mutex<String>,
    /// The negotiated plan (remux/transcode) — reused on seek-restarts;
    /// mutable because audio-track switches re-plan (HUB-27).
    plan: Mutex<Option<kahawai_media::remux::RemuxPlan>>,
    /// Placement requirements — reused when rescheduling (AR-6), and replaced
    /// with the current plan after a track switch.
    needs: Mutex<crate::registry::PlacementNeed>,
    /// HUB-36 work class this session's encode belongs to, or empty
    /// when there is no encode to learn from. Fixed at planning time:
    /// a track switch re-plans the AUDIO, which does not change what
    /// the video costs, and letting it drift would attribute a sample
    /// to work the box never did.
    pub pace_class: String,
    /// Serializes seek-restarts. A scrub fires seeks faster than a
    /// restart completes, and two interleaved restarts wipe each
    /// other's scratch dir mid-bind (observed as intermittent 409s).
    /// Held only by the detached executor task, never by a request.
    seek_lock: tokio::sync::Mutex<()>,
    /// The newest seek intent, COALESCED: a scrub burst collapses to
    /// one restart at the final position. Explicit track choices merge
    /// forward so a scrub cannot silently discard a track switch.
    pending_seek: Mutex<Option<PendingSeek>>,
    seek_gen: std::sync::atomic::AtomicU64,
    /// (generation, outcome) of the last completed restart. Requests
    /// await this instead of executing: superseded seeks return the
    /// winner's result, and an HTTP-cancelled request can no longer
    /// abort a restart midway (the executor task is detached).
    seek_done: tokio::sync::watch::Sender<(u64, Result<u64, String>)>,
    touched: Mutex<std::time::Instant>,
    /// Progress holds a read guard through its watch-state write; teardown
    /// takes the write guard before deciding whether this session earned a
    /// play. The gapless player deliberately sends final progress beside
    /// teardown, so request scheduling must not decide whether the play exists.
    ending: tokio::sync::RwLock<bool>,
    /// Is the playhead past the end threshold? Seeded from where this
    /// watch BEGAN, then moved by every progress report.
    ///
    /// Per SESSION and not read back from `watch_state.played`, so
    /// nothing else that writes that column — a mark by hand, another
    /// device — can put a play in this session's name.
    /// Did this watch take the item past the line ITSELF?
    ///
    /// With the current finished bit it decides `play_count` at teardown, and
    /// the pair is what keeps one sitting from counting twice. A session
    /// that is taken away — a reaped pause, a dead transcoder, a lost
    /// mediahost — is followed by one the client starts AT THE SAME
    /// POSITION (`recovery.ts`), already past the line and so seeded
    /// `finished`; it never sees the crossing, so the play stays with the
    /// watch that did the watching, wherever that one happened to stop.
    /// Asking instead who ended the session cannot work: the answer would
    /// have to be "not the reaper", and a viewer who finishes something
    /// and closes a laptop that never sends its `DELETE` is reaped too.
    ///
    /// The two facts are one atomic state rather than two booleans. Teardown
    /// may race a final progress request; publishing `finished` and
    /// `saw_finish` separately let it observe the first without the second and
    /// silently lose the play.
    watch_finish: std::sync::atomic::AtomicU8,
}

const WATCH_FINISHED: u8 = 1;
const WATCH_SAW_FINISH: u8 = 2;

/// A coalesced seek intent.
#[derive(Debug, Clone, Copy)]
struct PendingSeek {
    generation: u64,
    position_ms: u64,
    audio_track: Option<u32>,
    video_track: Option<u32>,
    /// The burn pick changed (subtitle unification): re-plan even when
    /// the audio/video tracks stayed put.
    replan_subs: bool,
}

impl PendingSeek {
    /// The newer intent wins the position; explicit track choices
    /// survive supersession (None = "keep current").
    fn merge(prev: Option<PendingSeek>, next: PendingSeek) -> PendingSeek {
        match prev {
            Some(p) => PendingSeek {
                audio_track: next.audio_track.or(p.audio_track),
                video_track: next.video_track.or(p.video_track),
                replan_subs: next.replan_subs || p.replan_subs,
                ..next
            },
            None => next,
        }
    }
}

/// The local process's verified encoder codec names — negotiation's
/// target pool when no fleet box would run the encode (HUB-15b),
/// mirroring the tonemap_available() fallback.
/// What the box that would run an encode can do. A PARAMETER rather
/// than part of [`Negotiation`], because the two callers learn it from
/// different places and the difference is load-bearing: a session start
/// probes the whole fleet, while a seek reads it off the executor it is
/// already bound to (a seek cannot move boxes, HUB-15b). Folding these
/// in would quietly make a seek re-plan against a fleet it cannot use.
pub(crate) struct ExecutorFacts {
    /// HUB-15a: the selected full video executor reports tone-map.
    pub tonemap: bool,
    /// HUB-15b: verified video targets of that full executor.
    pub video_targets: Vec<String>,
    /// Its audio targets, used when the whole video-encode pipeline is
    /// dispatched there.
    pub full_audio_targets: Vec<String>,
    /// Audio targets of the hub's lightweight local worker.
    pub local_audio_targets: Vec<String>,
    /// Additive protocol features understood by the selected full executor.
    pub full_protocol: kahawai_proto::ProtocolFeatures,
    /// HUB-32b: this source's display-set timeline is readable where
    /// the encode would run.
    pub burn_capable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceChoiceKey {
    incomplete: bool,
    ordinary_cost: kahawai_media::negotiate::Cost,
    force_missed: bool,
}

/// Cross-rendition choice is based on what each source costs before an
/// optional force preference changes its audio. Force capability breaks ties;
/// it never promotes a source whose ordinary video path is more expensive.
fn source_choice_key(
    ordinary: &kahawai_media::negotiate::SourcePlan,
    force_missed: bool,
) -> SourceChoiceKey {
    SourceChoiceKey {
        incomplete: ordinary.incomplete,
        ordinary_cost: ordinary.cost,
        force_missed,
    }
}

struct SourceChoice {
    plan: kahawai_media::negotiate::SourcePlan,
    index: usize,
    measurement: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
    key: SourceChoiceKey,
}

/// Everything a negotiation needs that does not change between
/// candidates: the caller's identity-derived facts and their picks.
///
/// Extracted from `start_inner`, where it was five closures over the
/// same captures, called at five points — twice to choose a source and
/// three more times to re-plan after a fetch withdrew a tier. Holding
/// it in one place is what lets a read-only caller ask the same
/// question without starting a session.
pub(crate) struct Negotiation<'a> {
    registry: &'a Registry,
    sessions: &'a Sessions,
    /// The client's profile, already tightened by the user's standing
    /// bandwidth cap (HUB-15).
    profile: kahawai_core::media::CapabilityProfile,
    /// Default-on measured loudness normalization policy.
    loudness: LoudnessPreference,
    /// HUB-32a/d: the user's ASS ladder. `pub` because the overlay rung
    /// only becomes real once something has been rasterised, and the
    /// caller flips `overlay_ready` before re-planning.
    pub ass: kahawai_media::negotiate::AssPolicy,
    /// HUB-32c: image tracks that already have OCR text derived from
    /// them, keyed per source.
    ocr_set: std::collections::HashSet<(String, String, String, String, i64)>,
    /// An explicit burn pick, if the caller named one and it is a track
    /// some tier could actually burn.
    burn_row: Option<crate::tracks::Track>,
    audio_track: u32,
    video_track: u32,
    /// The chosen source has a current measurement and force may therefore
    /// turn only its audio copy/direct path into an encode.
    force_audio_encode: bool,
    force_measurement: Option<kahawai_media::loudness::AudioLoudnessMeasurement>,
}

impl<'a> Negotiation<'a> {
    /// Resolve the caller's inputs once. The subtitle pick is validated
    /// here so a bad track id fails before any source work.
    #[allow(clippy::too_many_arguments)] // the caller's request, spelled out
    pub(crate) async fn new(
        sessions: &'a Sessions,
        registry: &'a Registry,
        user_id: &str,
        item_id: &str,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        audio_track: u32,
        video_track: u32,
        subtitle_track: Option<i64>,
    ) -> Result<Self> {
        // ONE path: every session negotiates. The user's standing
        // bandwidth cap tightens whatever the client asked for (HUB-15).
        let mut profile = profile.unwrap_or_default();
        let pref_cap: Option<u32> = sqlx::query_scalar::<_, String>(
            "SELECT value FROM user_prefs
              WHERE user_id = ? AND scope = '' AND key = 'bandwidth_kbps'",
        )
        .bind(user_id)
        .fetch_optional(registry.db())
        .await?
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0);
        profile.max_bandwidth_kbps = match (profile.max_bandwidth_kbps, pref_cap) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let loudness = registry.loudness_normalization(user_id).await?;
        // HUB-32a: burn ASS or flatten it? A fleet fact and this user's
        // standing choice. Fleet-wide rather than per-box on purpose —
        // the tier is decided before placement, which then hard-filters
        // on it (registry::PlacementNeed::needs_ass_burn).
        let local_ass_burn =
            registry.local_video_executor_enabled() && kahawai_media::remux::ass_burn_available();
        let ass = crate::tracks::ass_policy_for_user(
            registry.db(),
            user_id,
            registry.any_transcoder_ass_burn() || local_ass_burn,
        )
        .await;
        // HUB-32c: which embedded image tracks already have an OCR text
        // track derived from them — those prefer text over burn in the
        // negotiation. Fetched once, keyed per source.
        let ocr_set = crate::subtitles::ocr_stream_set(registry.db(), item_id).await;
        // Subtitle unification: an explicit IMAGE track pick forces its
        // burn — overriding both the overlay preference and the
        // OCR-spares-burn rule. Text/ass/downloaded picks have no plan
        // impact (the client fetches them itself). The row binds to a
        // source, so the pick also pins source selection to it: judging
        // other sources would silently drop the burn the user asked for.
        let picked_row = match subtitle_track {
            Some(tid) => {
                let t = crate::tracks::get_for_item(registry.db(), item_id, tid)
                    .await?
                    .ok_or_else(|| NoSuchTrack {
                        item: item_id.to_string(),
                        track: tid,
                    })?;
                Some(t)
            }
            None => None,
        };
        // Image tracks and ASS/SSA both burn (HUB-32b / HUB-32a) and
        // both need a stream to burn FROM, so a hub-stored row (OCR,
        // downloaded) is never a burn pick. Negotiation drops a pick
        // whose tier cannot honour it.
        let burn_row = picked_row
            .filter(|t| {
                crate::tracks::is_image_format(&t.format)
                    || matches!(t.format.as_str(), "ass" | "ssa")
            })
            .filter(|t| t.module_id.is_some() && t.stream_index.is_some());
        Ok(Self {
            registry,
            sessions,
            profile,
            loudness,
            ass,
            ocr_set,
            burn_row,
            audio_track,
            video_track,
            force_audio_encode: false,
            force_measurement: None,
        })
    }

    pub(crate) fn profile(&self) -> &kahawai_core::media::CapabilityProfile {
        &self.profile
    }

    /// The burn pick, but only when it belongs to THESE parts — a pick
    /// naming another source is not a pick for this one.
    pub(crate) fn pick_for(
        &self,
        parts: &[PartSource],
    ) -> Option<kahawai_media::negotiate::BurnPick> {
        let t = self.burn_row.as_ref()?;
        let p = parts.first()?;
        if (
            t.module_id.as_deref(),
            t.collection_id.as_deref(),
            t.root_token.as_deref(),
            t.source_path.as_deref(),
        ) != (
            Some(p.module_id.as_str()),
            Some(p.collection_id.as_str()),
            Some(p.root_token.as_str()),
            Some(p.path_rel.as_str()),
        ) {
            return None;
        }
        let i = t.stream_index? as usize;
        match t.origin.as_str() {
            "embedded" => Some(kahawai_media::negotiate::BurnPick::Embedded(i)),
            "sidecar" => Some(kahawai_media::negotiate::BurnPick::Sidecar(i)),
            _ => None,
        }
    }

    /// Ask the fleet which box would run an encode of this source, and
    /// what it can do. A pure query — `pick_transcoder` is
    /// `choose(need, false)` and reserves nothing.
    pub(crate) fn probe(
        &self,
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        required_protocol_feature: Option<kahawai_proto::ProtocolFeature>,
    ) -> ExecutorFacts {
        // HUB-15a: would the box that runs a video encode of THIS
        // source tone-map? Same placement question the real dispatch
        // asks (ponytail: probed with encode_audio=false — a fleet
        // where that changes the pick diverges cosmetically; the
        // worker-side guard keeps the failure soft).
        let need = crate::registry::PlacementNeed {
            encode_video: true,
            encode_audio: false,
            video_caps: kahawai_media::remux::source_caps_names("video", info),
            audio_caps: vec![],
            needs_tonemap: true,
            // Gain fields are additive on the wire but cannot degrade to an
            // old worker silently ignoring normalization. The ordinary probe
            // stays broad and is repeated with the exact required feature only
            // after the plan proves it needs gain.
            required_protocol_feature,
            // Not yet known here: the probe runs before the plan picks
            // a subtitle tier, and asking for the burn would narrow the
            // pool that DECIDES it.
            needs_ass_burn: false,
            // Codec-agnostic probe: "which box would run an encode of
            // this source at all" — its verified encoder set then
            // becomes negotiation's target pool (HUB-15b), same shape
            // as the tone-map fact.
            video_codec: String::new(),
            audio_codec: String::new(),
            // HUB-36: the probe asks WHICH BOX, not how fast — there is
            // no plan yet to classify, so it carries no prediction
            // inputs and ranks exactly as it did before phase 5.
            work_class: None,
            source_kbps: None,
        };
        let (tonemap, full_targets, full_protocol) = match self.registry.pick_transcoder(&need) {
            Some(tc) => (
                self.registry.transcoder_reports_tonemap(&tc),
                self.registry.transcoder_encoders(&tc),
                self.registry
                    .transcoder_protocol_features(&tc)
                    .unwrap_or_default(),
            ),
            None if self.registry.local_video_executor_enabled() => (
                local_tonemap_available(self.registry),
                local_encoder_names(self.registry),
                kahawai_proto::ProtocolFeatures::current(),
            ),
            None => (false, Vec::new(), Default::default()),
        };
        ExecutorFacts {
            tonemap,
            video_targets: video_encoder_names(&full_targets),
            full_audio_targets: audio_encoder_names(&full_targets),
            full_protocol,
            local_audio_targets: local_audio_encoder_names(),
            burn_capable,
        }
    }

    fn plan_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        facts: &ExecutorFacts,
        force_audio_encode: bool,
    ) -> kahawai_media::negotiate::SourcePlan {
        let est_kbps = info
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| ((parts.iter().map(|p| p.size).sum::<u64>() * 8) / d) as u32);
        let ocr_flags = parts
            .first()
            .map(|p| {
                crate::subtitles::ocr_flags_for(
                    &self.ocr_set,
                    &p.module_id,
                    &p.collection_id,
                    &p.root_token,
                    &p.path_rel,
                    info.subtitles.len(),
                )
            })
            .unwrap_or_default();
        let negotiate = |force| {
            kahawai_media::negotiate::negotiate_for_executors(
                &self.profile,
                info,
                self.audio_track as usize,
                self.video_track as usize,
                parts.len() == 1,
                est_kbps,
                facts.tonemap,
                facts.burn_capable,
                &ocr_flags,
                self.pick_for(parts),
                &self.ass,
                &facts.video_targets,
                &facts.full_audio_targets,
                &facts.local_audio_targets,
                force,
            )
        };
        let normal = negotiate(false);
        if !force_audio_encode {
            return normal;
        }
        negotiate(true)
    }

    fn plans_with_probe(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> (
        kahawai_media::negotiate::SourcePlan,
        kahawai_media::negotiate::SourcePlan,
    ) {
        let ordinary_facts = self.probe(info, burn_capable, None);
        let ordinary = self.plan_with_force(parts, info, &ordinary_facts, false);
        let Some(measurement) = measurement else {
            return (ordinary.clone(), ordinary);
        };
        let forced = self.plan_with_force(parts, info, &ordinary_facts, true);
        let usable_force = |candidate: &kahawai_media::negotiate::SourcePlan| {
            candidate.cost != kahawai_media::negotiate::Cost::Unplayable
                && candidate.plan.audio == kahawai_media::remux::StreamMode::Encode
                && candidate.plan.video == ordinary.plan.video
        };
        let selected = if !usable_force(&forced) {
            ordinary.clone()
        } else {
            let mut measured_plan = forced.plan;
            apply_audio_loudness_measurement(
                &mut measured_plan,
                LoudnessPreference::Force,
                Some(measurement.clone()),
            );
            let required = (measured_plan.video == kahawai_media::remux::StreamMode::Encode)
                .then(|| loudness_protocol_feature(&measured_plan))
                .flatten();
            if required.is_none_or(|feature| ordinary_facts.full_protocol.supports(feature)) {
                forced
            } else {
                let exact_facts = self.probe(info, burn_capable, required);
                let exact = self.plan_with_force(parts, info, &exact_facts, true);
                if usable_force(&exact) {
                    exact
                } else {
                    ordinary.clone()
                }
            }
        };
        (ordinary, selected)
    }

    fn plan_for_protocol(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        required_protocol_feature: Option<kahawai_proto::ProtocolFeature>,
    ) -> kahawai_media::negotiate::SourcePlan {
        let facts = self.probe(info, burn_capable, required_protocol_feature);
        self.plan_with_force(parts, info, &facts, false)
    }

    fn plan_with_probe(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plans_with_probe(parts, info, burn_capable, measurement)
            .1
    }

    /// Probe the fleet, then plan. The session-start form.
    pub(crate) fn plan_probed(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plan_with_probe(parts, info, burn_capable, self.force_measurement.as_ref())
    }

    /// HUB-32b: the display-set timeline comes from the mediahost,
    /// which walks its own disk in milliseconds — the hub cannot, every
    /// read would cross the byte plane at ~4 KB/s. Offer the tier while
    /// that host is reachable; the sets themselves decide later, and a
    /// failure there re-plans.
    fn reads_sets_for(&self, parts: &[PartSource]) -> bool {
        parts.first().is_some_and(|p| {
            self.registry.is_connected(&p.module_id) || self.sessions.reads_locally(&p.module_id)
        })
    }

    fn plan_auto_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plans_auto_with_force(parts, info, measurement).1
    }

    fn plans_auto_with_force(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        measurement: Option<&kahawai_media::loudness::AudioLoudnessMeasurement>,
    ) -> (
        kahawai_media::negotiate::SourcePlan,
        kahawai_media::negotiate::SourcePlan,
    ) {
        self.plans_with_probe(parts, info, self.reads_sets_for(parts), measurement)
    }

    async fn forced_measurement(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
    ) -> Result<Option<kahawai_media::loudness::AudioLoudnessMeasurement>> {
        if !self.loudness.force() || parts.len() != 1 || info.audio.is_empty() {
            return Ok(None);
        }
        let audio_track = (self.audio_track as usize).min(info.audio.len() - 1);
        self.registry
            .audio_loudness(parts[0].file_id, audio_track)
            .await
    }

    /// Which source to play and how, judged across every candidate.
    ///
    /// Returns `Cost::Unplayable` rather than failing when nothing the
    /// client accepts can be produced — a caller asking "what would I
    /// get" deserves that answer, and only a session turns it into an
    /// error. The two failures here are different: they mean there is
    /// no source to negotiate against at all.
    pub(crate) async fn best_source(
        &mut self,
        item_id: &str,
        mode: Option<&str>,
    ) -> Result<(
        Vec<PartSource>,
        kahawai_core::media::MediaInfo,
        kahawai_media::negotiate::SourcePlan,
        String,
    )> {
        match mode {
            // Operator override (scripts, pipeline debugging): explicit direct
            // still means original bytes. An explicit remux may force gain.
            Some(m) => {
                let (parts, info) = self.sessions.source_parts(self.registry, item_id).await?;
                let measurement = if m == "direct" {
                    None
                } else {
                    self.forced_measurement(&parts, &info).await?
                };
                let force = measurement.is_some();
                let sp = self.plan_auto_with_force(&parts, &info, measurement.as_ref());
                self.force_audio_encode = force;
                self.force_measurement = measurement;
                Ok((parts, info, sp, m.to_string()))
            }
            // HUB-14/16: judge every candidate, cheapest sufficient
            // path wins, rank breaks ties.
            None => {
                let mut candidates = self
                    .sessions
                    .candidate_sources(self.registry, item_id)
                    .await?;
                // Nothing at all first. This is the host being away, and it
                // has to be told apart from the burn refusal below: one is a
                // moment (503, stand by), the other is this item (409, give
                // up). Checked in the other order, an offline host reached
                // the burn arm — `retain` on an empty set stays empty — so
                // the one condition stand-by exists for was the one told to
                // give up.
                if candidates.is_empty() {
                    // Only "the rows exist and every host holding them is
                    // away" is a wait. An item with no sources at all is a
                    // permanent refusal, and telling the client to stand by
                    // for it produced an unbounded retry.
                    if self.sessions.has_any_source(self.registry, item_id).await {
                        bail!(SourceOffline);
                    }
                    bail!("no sources for item");
                }
                // A burn pick pins the source it binds to: judging the
                // others would let a cheaper copy win and silently drop
                // the burn the user explicitly selected. Reaching here means
                // sources DO exist and none of them carries the pinned
                // track, which no amount of waiting fixes.
                if self.burn_row.is_some() {
                    candidates.retain(|(parts, _)| self.pick_for(parts).is_some());
                    if candidates.is_empty() {
                        bail!("the picked subtitle track's source is not available");
                    }
                }
                // Completeness and the ORDINARY cost choose the rendition.
                // Force may break that tie, then replace only the winner's
                // audio plan. Ranking the forced plan itself made a measured
                // HEVC encode beat an unmeasured H.264 direct source, which
                // turned an audio-only preference into video transcoding.
                let mut best: Option<SourceChoice> = None;
                for (idx, (parts, info)) in candidates.iter().enumerate() {
                    let measurement = self.forced_measurement(parts, info).await?;
                    let (ordinary, sp) =
                        self.plans_auto_with_force(parts, info, measurement.as_ref());
                    let normalized = measurement.is_some()
                        && sp.plan.audio == kahawai_media::remux::StreamMode::Encode;
                    let force_missed = self.loudness.force() && !normalized;
                    let key = source_choice_key(&ordinary, force_missed);
                    let better = best.as_ref().is_none_or(|current| key < current.key);
                    if better {
                        best = Some(SourceChoice {
                            plan: sp,
                            index: idx,
                            measurement,
                            key,
                        });
                    }
                }
                let SourceChoice {
                    plan: sp,
                    index: idx,
                    measurement,
                    ..
                } = best.unwrap();
                self.force_audio_encode = measurement.is_some();
                self.force_measurement = measurement;
                let mode = if sp.direct { "direct" } else { "remux" };
                let (parts, info) = candidates.into_iter().nth(idx).unwrap();
                Ok((parts, info, sp, mode.to_string()))
            }
        }
    }
}

fn local_encoder_names(registry: &Registry) -> Vec<String> {
    let Some(bench) = registry.local_bench() else {
        return Vec::new();
    };
    kahawai_media::remux::encoder_capabilities()
        .iter()
        .filter(|(_, element, _)| bench.encoder_ready(element))
        .map(|(codec, _, _)| codec.to_string())
        .collect()
}

fn local_tonemap_available(registry: &Registry) -> bool {
    registry
        .local_bench()
        .is_some_and(|bench| bench.tonemap_ready())
        && kahawai_media::remux::tonemap_available()
}

fn local_audio_encoder_names() -> Vec<String> {
    [
        ("aac", kahawai_media::remux::aac_encoder()),
        ("opus", kahawai_media::remux::opus_encoder()),
    ]
    .into_iter()
    .filter_map(|(codec, element)| element.map(|_| codec.to_string()))
    .collect()
}

fn video_encoder_names(targets: &[String]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| matches!(t.as_str(), "h264" | "hevc" | "av1"))
        .cloned()
        .collect()
}

fn audio_encoder_names(targets: &[String]) -> Vec<String> {
    targets
        .iter()
        .filter(|t| matches!(t.as_str(), "aac" | "opus"))
        .cloned()
        .collect()
}

/// The negotiation speaks stream indexes; the API speaks unified track
/// rows. Stamp each verdict with the id of the embedded row bound to
/// the session's source (missing rows read as None — a source scanned
/// before the unification migration ran).
pub(crate) async fn fill_verdict_track_ids(
    registry: &Registry,
    parts: &[PartSource],
    verdicts: &mut [kahawai_media::negotiate::SubtitleVerdict],
) {
    let Some(p) = parts.first() else { return };
    let map: std::collections::HashMap<i64, i64> = sqlx::query_as(
        "SELECT t.stream_index,t.id FROM subtitle_tracks t
         JOIN files f ON f.id=t.source_id JOIN collection_roots r ON r.id=f.root_id
         WHERE t.origin='embedded'
           AND (f.module_id,f.collection_id,r.root_token,f.path_rel)=(?,?,?,?)",
    )
    .bind(&p.module_id)
    .bind(&p.collection_id)
    .bind(&p.root_token)
    .bind(&p.path_rel)
    .fetch_all(registry.db())
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();
    for v in verdicts.iter_mut() {
        v.track_id = map.get(&(v.index as i64)).copied();
    }
}

/// Index of the part containing `abs_ms`.
fn part_index(parts: &[PartSource], abs_ms: u64) -> usize {
    parts.iter().rposition(|p| abs_ms >= p.base_ms).unwrap_or(0)
}

impl Session {
    /// The effective profile this session negotiated with — the one a
    /// capability-masked restart actually applied, where the item QUERY's
    /// listing still reflects the page-load profile.
    pub fn effective_profile(&self) -> &kahawai_core::media::CapabilityProfile {
        &self.profile
    }

    /// The ASS ladder the negotiation used, overlay readiness included.
    pub fn ass_policy(&self) -> &kahawai_media::negotiate::AssPolicy {
        &self.ass
    }

    /// Timeline base of the part currently playing (0 for single-file).
    pub fn part_base_ms(&self) -> u64 {
        self.parts
            .get(self.current_part.load(std::sync::atomic::Ordering::SeqCst))
            .map(|p| p.base_ms)
            .unwrap_or(0)
    }

    /// Aggregate delivery cost of what is playing now. This is derived from
    /// the elementary-stream plan, not [`Mode`]: `Mode::Remux` means the hub
    /// owns an HLS pipeline and `Mode::Transcode` means a satellite owns it;
    /// either pipeline may copy one stream and encode the other.
    pub fn delivery_cost(&self) -> Option<&'static str> {
        if matches!(&self.mode, Mode::Direct { .. }) {
            return Some(kahawai_media::negotiate::Cost::Direct.as_str());
        }
        self.plan.lock().unwrap().map(|plan| {
            use kahawai_media::remux::StreamMode;
            if plan.video == StreamMode::Encode {
                kahawai_media::negotiate::Cost::VideoEncode.as_str()
            } else if plan.audio == StreamMode::Encode {
                kahawai_media::negotiate::Cost::AudioEncode.as_str()
            } else {
                kahawai_media::negotiate::Cost::Copy.as_str()
            }
        })
    }

    /// Any client activity (stream chunks, playlist/segment fetches,
    /// progress pings) keeps the session alive (HUB-18).
    pub fn touch(&self) {
        *self.touched.lock().unwrap() = std::time::Instant::now();
    }

    /// Enter a progress write, or refuse after teardown has won the race.
    pub async fn begin_report(&self) -> Option<tokio::sync::RwLockReadGuard<'_, bool>> {
        let guard = self.ending.read().await;
        if *guard { None } else { Some(guard) }
    }

    /// Record what a progress report said, for the count that lands when
    /// this watch stops. See [`Session::watch_finish`].
    pub fn report(&self, finished: bool) {
        self.watch_finish
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |old| {
                    if finished {
                        let crossed = old & WATCH_FINISHED == 0;
                        Some(old | WATCH_FINISHED | if crossed { WATCH_SAW_FINISH } else { 0 })
                    } else {
                        Some(old & !WATCH_FINISHED)
                    }
                },
            )
            .expect("the finish-state update never refuses");
    }

    /// Is this watch a play? It ended past the line, and it is the one
    /// that got the item there. Read once, at teardown.
    pub fn earned_a_play(&self) -> bool {
        self.watch_finish.load(std::sync::atomic::Ordering::Acquire)
            & (WATCH_FINISHED | WATCH_SAW_FINISH)
            == (WATCH_FINISHED | WATCH_SAW_FINISH)
    }

    pub fn idle_for(&self) -> Duration {
        self.touched.lock().unwrap().elapsed()
    }
}

/// Adapts a mediahost read lease to the remuxer's random-access source
/// trait; runs on the remux feeder thread, bridging into the runtime.
pub(crate) struct LeaseSource {
    pub(crate) lease: Lease,
    pub(crate) size: u64,
    pub(crate) handle: tokio::runtime::Handle,
    /// Reads served, for the log line that says whether a stalled consumer ever
    /// got its first byte.
    pub(crate) reads: u64,
}

impl kahawai_media::remux::RemuxSource for LeaseSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(self.size - offset);
        self.reads += 1;
        let started = std::time::Instant::now();
        if self.reads % 64 == 1 {
            tracing::debug!(offset, len, reads = self.reads, "lease read");
        }
        tracing::trace!(offset, len, reads = self.reads, "lease read: asking");
        let _guard = self.handle.enter();
        let mut stream = self.lease.read_range(offset, len).into_inner();
        let outcome = self.handle.block_on(async {
            let mut filled = 0usize;
            while filled < len as usize {
                match stream.recv().await {
                    Some(Ok(bytes)) => {
                        let n = bytes.len().min(buf.len() - filled);
                        buf[filled..filled + n].copy_from_slice(&bytes[..n]);
                        filled += n;
                    }
                    Some(Err(e)) => return Err(std::io::Error::other(e)),
                    None => break,
                }
            }
            Ok(filled)
        });
        // A read that takes seconds is the byte plane, not the analyzer; a read
        // that never returns does not reach this line at all, which is the
        // distinction worth having in a log.
        tracing::trace!(
            offset,
            ok = outcome.is_ok(),
            seconds = started.elapsed().as_secs_f64(),
            "lease read: answered"
        );
        if started.elapsed() > std::time::Duration::from_secs(5) {
            tracing::warn!(
                offset,
                len,
                seconds = started.elapsed().as_secs_f64(),
                ok = outcome.is_ok(),
                "slow lease read"
            );
        }
        outcome
    }
}

/// Serve RemuxSource reads to a worker over its Unix socket.
/// Wire format: see kahawai_media::worker.
async fn serve_reads(mut conn: tokio::net::UnixStream, lease: Lease, size: u64) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = [0u8; 16];
    loop {
        if conn.read_exact(&mut req).await.is_err() {
            return Ok(()); // worker closed the socket (EOS or teardown)
        }
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
        let len =
            u64::from_le_bytes(req[8..].try_into().unwrap()).min(kahawai_media::worker::MAX_READ);
        let want = if offset >= size {
            0
        } else {
            len.min(size - offset)
        };
        let mut buf = Vec::with_capacity(want as usize);
        if want > 0 {
            let mut stream = lease.read_range(offset, want).into_inner();
            while (buf.len() as u64) < want {
                match stream.recv().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => anyhow::bail!("lease read failed: {e}"),
                    None => break,
                }
            }
            buf.truncate(want as usize);
        }
        conn.write_all(&(buf.len() as u64).to_le_bytes()).await?;
        conn.write_all(&buf).await?;
    }
}

/// A playlist is client-ready with ≥3 segments (~10 s of runway) or an
/// ENDLIST (short source: whatever exists is all there is).
/// What "enough runway" means, measured on real starts (2026-07-31,
/// Firefox): hls.js reveals new segments only on its EVENT-playlist
/// reloads (~every target-duration, 3 s), and production jitters (one
/// heavy GOP took 3.5 s against a ~2 s cadence) — so playback stalls
/// whenever buffered content dips under ~production-gap + reload ≈
/// 6.5 s before the encoder's lead has grown past it. Hand-off with
/// ~4 s of content buffered stalled twice; ~5 s stalled once at 12 s.
/// The gate is therefore CONTENT seconds, not a segment count — three
/// segments can be as little as 4 s when scene-cut keyframes shorten
/// them. ENDLIST (a finished short encode) is always ready.
fn playlist_ready(path: &std::path::Path, target_secs: u32) -> bool {
    // The 6.5 s floor was derived against a declared target of 2: a
    // client reloads about every target duration, so the runway it
    // needs is production-gap PLUS one reload interval. Once the
    // declaration follows the source — 12 s for a 10 s-GOP file — a
    // fixed 6.5 s hands over less than the client's own refresh period
    // and stalls on the first gap. §6.3.3 pushes the same way: a client
    // SHOULD NOT start within three target durations of the end.
    //
    // CAPPED so this gate can never be what times a start out. A 66 s
    // GOP would ask for 204 s of content, which is more than the 30 s
    // start deadline can produce however fast the source reads.
    //
    // Past the cap the server can do nothing useful anyway: a client
    // that honours §6.3.3 waits on its own account whatever we hand
    // it, and one that does not is ready now — so waiting longer here
    // only converts the client's wait into our timeout.
    //
    // Not the cure for extreme sources, and it should not be mistaken
    // for one: a file whose keyframes are 66 s apart cannot close a
    // single segment inside the deadline no matter what this returns,
    // because one segment IS one GOP. Those need `short`, which
    // re-encodes and gives them keyframes of our own choosing.
    const MAX_RUNWAY_SECS: f64 = 30.0;
    let need = (6.5f64).max((3.0 * target_secs as f64).min(MAX_RUNWAY_SECS));
    match std::fs::read_to_string(path) {
        Ok(p) => p.contains("#EXT-X-ENDLIST") || playlist_span_secs(&p) >= need,
        Err(_) => false,
    }
}

/// Total seconds of content a playlist advertises (Σ EXTINF).
fn playlist_span_secs(playlist: &str) -> f64 {
    playlist
        .lines()
        .filter_map(|l| {
            l.strip_prefix("#EXTINF:")?
                .trim_end_matches(',')
                .parse::<f64>()
                .ok()
        })
        .sum()
}

type LocalResolver =
    std::sync::Arc<dyn Fn(&str, &str, &str) -> Result<std::path::PathBuf> + Send + Sync>;
type LocalActivity = std::sync::Arc<dyn Fn(&str) -> Box<dyn Send + Sync> + Send + Sync>;
type LocalBackground = std::sync::Arc<dyn Fn(&str) -> LocalAdmission + Send + Sync>;

/// How long a burn-in session waits for the mediahost to walk its
/// index. Milliseconds on local disk; this is the sanity bound, well
/// inside the client's own patience.
const BURN_SETS_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Fold a worker's session facts (AR-13) into the per-kind verdict, so
/// "dts → aac (transcoded)" becomes "dts → aac (transcoded) · 7.1 → 5.1".
/// Idempotent — a seek-restart re-learns the same facts and must not
/// stutter them — and unknown kinds are logged rather than lost.
fn fold_facts(verdict: &mut Option<(String, String)>, facts: &[kahawai_media::facts::Fact]) {
    let Some((video, audio)) = verdict.as_mut() else {
        return;
    };
    for f in facts {
        let slot = match f.kind.as_str() {
            "audio" => &mut *audio,
            "video" => &mut *video,
            other => {
                tracing::warn!(kind = other, detail = %f.detail, "unroutable session fact");
                continue;
            }
        };
        if !slot.contains(&f.detail) {
            slot.push_str(&format!(" · {}", f.detail));
        }
    }
}

/// The transcoder's answer to a dispatch: ready (with the worker's
/// session facts, AR-13) or an error string.
type ReadyVerdict = Result<Vec<kahawai_media::facts::Fact>, String>;

/// Leases for every part of one session (with sizes), plus the index
/// of the part playback started in.
type PartLeases = (Vec<(Lease, u64)>, usize);

pub struct Sessions {
    pub leases: Leases,
    /// AR-5: the in-process mediahost, if any — (module_id, path
    /// resolver). Its leases are direct file reads, no OpenRead.
    local_source: Mutex<Option<(String, LocalResolver)>>,
    /// Registers an in-process viewer lease as interactive scheduler work.
    local_activity: Mutex<Option<LocalActivity>>,
    /// Scheduler admission for non-interactive all-in-one reads.
    local_background: Mutex<Option<LocalBackground>>,
    /// Scratch space for remux sessions (`<data_dir>/sessions`).
    scratch_root: PathBuf,
    max_per_user: usize,
    idle_timeout: Duration,
    /// The binary to spawn as the per-session pipeline worker (the hub
    /// passes its own executable; the worker is a hidden subcommand).
    /// None → pipelines run in-process (tests only).
    worker_exe: Option<PathBuf>,
    active: Mutex<HashMap<String, Arc<Session>>>,
    /// Session ids admitted but not yet in `active`, keyed to their
    /// user. The per-user cap counts the UNION of this and `active`.
    ///
    /// Without it the cap was unenforceable: it counted `active`, let
    /// the lock go, and the session only landed there ~500 lines and 16
    /// awaits later, so concurrent starts all read the same stale count
    /// (measured: 20 admitted against a cap of 4). A guard that holds
    /// only for requests arriving one at a time is not a guard.
    ///
    /// A placeholder in `active` would not do: `Mode` has no variant
    /// for "not started yet", and a fake one would inflate the metrics
    /// gauge, show as a phantom row in the admin list, be reaped by the
    /// janitor, and stall the subtitle drain loop. Same shape as
    /// `known_sessions`, which already tracks ids `active` cannot
    /// answer for.
    reserved: Mutex<HashMap<String, String>>,
    /// Source leases for dispatched sessions (the transcoder pulls bytes
    /// over its link; lives from dispatch to session end).
    /// Hub-held source leases of dispatched sessions: (lease, size,
    /// part index) — reused across restarts within the same part so
    /// recovery works even when the mediahost link is flapping.
    /// Every part from the session's starting part onward, in timeline
    /// order: the transcoder joins them into one pipeline and asks for
    /// each by index. Second element is the starting part's index, so a
    /// seek that stays inside it can reuse these leases.
    tc_leases: Mutex<HashMap<String, PartLeases>>,
    /// Sessions awaiting the transcoder's ready/error verdict; Ok
    /// carries the worker's session facts (AR-13).
    pending_ready: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ReadyVerdict>>>,
    /// OPS-10: callers awaiting a satellite's log bundle. Same shape as
    /// `pending_ready` — one waiter per session, dropped on timeout.
    pending_logs: Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    /// OPS-10: `(item_id, header)` for sessions `active` cannot answer
    /// for. Two windows need it, and both are the interesting ones:
    ///
    /// * A session that FAILED TO START was never inserted into
    ///   `active` — registration happens after the pipeline is up — so
    ///   without this its diagnostics file under item "unknown".
    /// * A dispatched session's bundle crosses the link well after
    ///   `end()` removed it.
    ///
    /// Populated the moment the id is minted, replaced with the full
    /// header at teardown.
    known_sessions: Mutex<HashMap<String, (String, String)>>,
    /// In-flight artifact fetches, keyed by (session, name).
    artifact_waiting: Mutex<
        HashMap<(String, String), tokio::sync::mpsc::Sender<kahawai_proto::v1::ArtifactData>>,
    >,
    /// Registry handle for teardown messages and watch-state writes from
    /// `end()`.
    /// (set once at startup; None only in tests without dispatch).
    registry_for_teardown: Mutex<Option<Arc<Registry>>>,
}

/// Holds a per-user admission slot for as long as a start is in flight.
///
/// Drop is the only exit every path shares: the thirteen early returns inside
/// `start_inner`, the error paths, and the caller abandoning the request, which
/// drops the whole future and runs no statement after the await.
struct SlotGuard<'a> {
    sessions: &'a Sessions,
    id: String,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.sessions.release(&self.id);
    }
}

impl Sessions {
    pub fn new(scratch_root: PathBuf) -> Self {
        Self::with_limits(scratch_root, 4, Duration::from_secs(90))
    }

    /// Where diagnostics live: the scratch root is `<data_dir>/sessions`,
    /// so its parent is the data dir.
    /// OPS-10: the hub half of a session bundle — what only the hub
    /// knows. Structured state rather than log lines because the hub
    /// cannot read its own log: it writes to stdout, redirected by
    /// whatever launched it, and on macOS launchd discards it entirely.
    ///
    /// Returns `(item_id, header)`; the item id rides into the bundle
    /// FILENAME so the item detail page can find it later.
    pub fn log_header(&self, session_id: &str) -> (String, String) {
        use std::fmt::Write as _;
        let Some(s) = self.get(session_id) else {
            // Torn down already — which is the NORMAL case for a
            // dispatched session's bundle, since it crosses the link
            // after end() has forgotten the session.
            // NOT removed: a failed session emits a crash log AND a
            // bundle, and a mid-session death emits both too — each
            // needs the same header.
            if let Some(kept) = self.known_sessions.lock().unwrap().get(session_id) {
                return kept.clone();
            }
            return (
                "unknown".into(),
                format!("== hub: session {session_id}\n(session already ended; no hub state)\n\n"),
            );
        };
        let mut h = String::new();
        let _ = writeln!(h, "== hub: session {}", s.id);
        let _ = writeln!(h, "item:       {}", s.item_id);
        let _ = writeln!(h, "user:       {}", s.user_id);
        let _ = writeln!(
            h,
            "mode:       {}",
            match &s.mode {
                Mode::Direct { .. } => "direct",
                Mode::Remux { .. } => "remux (hub-local worker)",
                Mode::Transcode { .. } => "transcode (dispatched)",
            }
        );
        if let Mode::Transcode { transcoder } = &s.mode {
            let _ = writeln!(h, "placed on:  {}", transcoder.lock().unwrap());
        }
        if !s.pace_class.is_empty() {
            let _ = writeln!(h, "work class: {}", s.pace_class);
        }
        if let Some((video, audio)) = s.verdict.lock().unwrap().as_ref() {
            let _ = writeln!(h, "verdict:    v: {video}");
            let _ = writeln!(h, "            a: {audio}");
        }
        if let Some(plan) = *s.plan.lock().unwrap() {
            let _ = writeln!(h, "plan:       {plan:?}");
        }
        let _ = writeln!(h, "sink:       {}", s.sink.lock().unwrap());
        let _ = writeln!(h, "idle:       {}s", s.idle_for().as_secs());
        // An error recorded while the session was still live (a
        // mid-session death that AR-6 rescheduled) belongs here too.
        if let Some((_, kept)) = self.known_sessions.lock().unwrap().get(&s.id)
            && let Some(line) = kept.lines().find(|l| l.starts_with("error:"))
        {
            let _ = writeln!(h, "{line}");
        }
        let _ = writeln!(h);
        (s.item_id.clone(), h)
    }

    /// OPS-10: this session's diagnostics, for the download button.
    ///
    /// A LIVE dispatched session is asked over the link; a live local one
    /// is read straight off disk; an ENDED one comes from the bundle
    /// stored at teardown, which is the case that matters — nobody
    /// presses a button on a session they already closed.
    pub async fn collect_logs(
        &self,
        registry: &crate::registry::Registry,
        id: &str,
    ) -> Result<String> {
        let data_dir = self.data_dir().context("no data dir")?.to_path_buf();
        let Some(session) = self.get(id) else {
            // Ended: serve what teardown kept.
            let path = crate::sessionlog::for_session(&data_dir, id)
                .context("no logs kept for that session")?;
            return Ok(std::fs::read_to_string(path)?);
        };
        match &session.mode {
            Mode::Remux { dir, .. } => {
                let (_, header) = self.log_header(id);
                Ok(format!("{header}{}", local_bundle(dir)))
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.pending_logs.lock().unwrap().insert(id.into(), tx);
                let sent = registry
                    .send_to_tc(
                        &tc,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::CollectLogs(
                                kahawai_proto::v1::CollectLogs {
                                    session_id: id.to_string(),
                                },
                            )),
                        },
                    )
                    .await;
                if let Err(e) = sent {
                    self.pending_logs.lock().unwrap().remove(id);
                    // The box is gone; a stored bundle may still exist.
                    return match crate::sessionlog::for_session(&data_dir, id) {
                        Some(p) => Ok(std::fs::read_to_string(p)?),
                        // Typed, like the timeout below: "the transcoder is
                        // not reachable" and "this session kept no logs" are
                        // different answers and the API cannot tell them apart
                        // from a sentence.
                        // The transport failure is the SOURCE and the typed
                        // refusal is the context, not the other way round.
                        // Flattening the chain into the outermost layer is the
                        // exact shape `error.rs` names as a leak vector: it is
                        // harmless only while nothing reads this error's own
                        // `Display`, and it reads backwards in the log.
                        None => Err(e.context(SatelliteSilent(
                            "the transcoder running this session is not reachable".into(),
                        ))),
                    };
                }
                match tokio::time::timeout(Duration::from_secs(10), rx).await {
                    Ok(Ok(body)) => Ok(body),
                    _ => {
                        self.pending_logs.lock().unwrap().remove(id);
                        bail!(SatelliteSilent(
                            "the transcoder running this session did not answer in time".into()
                        ))
                    }
                }
            }
            Mode::Direct { .. } => {
                let (_, header) = self.log_header(id);
                Ok(format!(
                    "{header}(direct play: no pipeline, no worker log)\n"
                ))
            }
        }
    }

    /// OPS-10: remember which item a session id belongs to, from the
    /// moment the id exists. Everything that can go wrong after this
    /// point — a failed start, a mid-session death, a normal teardown —
    /// produces diagnostics that must file under the right item.
    fn note_session(&self, id: &str, item_id: &str) {
        let mut kept = self.known_sessions.lock().unwrap();
        // Bounded: these are small headers, and only the recent ones can
        // still have diagnostics arriving for them.
        if kept.len() > 64 {
            kept.clear();
        }
        kept.insert(
            id.to_string(),
            (
                item_id.to_string(),
                format!("== hub: session {id}\nitem:       {item_id}\n(session did not reach a running state)\n\n"),
            ),
        );
    }

    /// OPS-10: record why a session failed, on the header every later
    /// bundle carries.
    ///
    /// Both the error path and teardown write a bundle for the same
    /// session, and they collide on the filename — so the teardown one,
    /// which is richer but knows nothing about the failure, would erase
    /// the error message. Putting the error on the HEADER means whichever
    /// write lands last still carries it, and the worker log is not
    /// duplicated into two files to achieve that.
    pub fn note_error(&self, session_id: &str, error: &str) {
        let mut kept = self.known_sessions.lock().unwrap();
        if let Some((_, header)) = kept.get_mut(session_id)
            && !header.contains("error:")
        {
            header.push_str(&format!("error:      {error}\n\n"));
        }
    }

    /// A bundle arrived for a caller waiting on it (the download button).
    pub fn deliver_logs(&self, session_id: &str, body: String) {
        if let Some(tx) = self.pending_logs.lock().unwrap().remove(session_id) {
            let _ = tx.send(body);
        }
    }

    pub fn data_dir(&self) -> Option<&std::path::Path> {
        self.scratch_root.parent()
    }

    /// Take a slot for `user_id` under `id`, or refuse. The count is
    /// the union of `reserved` and `active`, taken in ONE critical
    /// section — which is the whole point: the previous check counted
    /// `active` alone and released the lock long before the session
    /// landed there.
    ///
    /// Lock order is `reserved` then `active`, and nothing else takes
    /// both, so this cannot deadlock. Counting the union also makes the
    /// window between the insert into `active` and [`Self::release`]
    /// harmless: a session in both is still one session.
    fn admit(&self, id: &str, user_id: &str) -> Result<()> {
        let mut reserved = self.reserved.lock().unwrap();
        let held = reserved.values().filter(|u| *u == user_id).count()
            + self
                .active
                .lock()
                .unwrap()
                .values()
                .filter(|s| s.user_id == user_id && !reserved.contains_key(&s.id))
                .count();
        if held >= self.max_per_user {
            bail!(SessionCap { held });
        }
        reserved.insert(id.to_string(), user_id.to_string());
        Ok(())
    }

    /// Give the slot back. Called once `start` knows the outcome —
    /// either the session is in `active` and counts itself, or it never
    /// started and must not count at all.
    fn release(&self, id: &str) {
        self.reserved.lock().unwrap().remove(id);
    }

    pub fn with_limits(scratch_root: PathBuf, max_per_user: usize, idle_timeout: Duration) -> Self {
        // Sessions never survive a restart; stale scratch is garbage.
        let _ = std::fs::remove_dir_all(&scratch_root);
        Self {
            leases: Leases::default(),
            local_source: Mutex::new(None),
            local_activity: Mutex::new(None),
            local_background: Mutex::new(None),
            scratch_root,
            max_per_user,
            idle_timeout,
            worker_exe: None,
            active: Mutex::new(HashMap::new()),
            reserved: Mutex::new(HashMap::new()),
            tc_leases: Mutex::new(HashMap::new()),
            pending_ready: Mutex::new(HashMap::new()),
            pending_logs: Mutex::new(HashMap::new()),
            known_sessions: Mutex::new(HashMap::new()),
            artifact_waiting: Mutex::new(HashMap::new()),
            registry_for_teardown: Mutex::new(None),
        }
    }

    /// Give the sync teardown path a registry handle for EndSession
    /// notifications to transcoders.
    pub fn attach_registry(&self, registry: Arc<Registry>) {
        *self.registry_for_teardown.lock().unwrap() = Some(registry);
    }

    /// Run pipelines in a supervised child process (crash isolation).
    pub fn with_worker_exe(mut self, exe: Option<PathBuf>) -> Self {
        self.worker_exe = exe;
        self
    }

    /// Reap idle sessions (HUB-18: no fetch or progress ping → teardown).
    pub fn spawn_janitor(self: &Arc<Self>) {
        let sessions = self.clone();
        // Check at half the timeout (tests use tiny timeouts), capped at 15 s.
        let period =
            (sessions.idle_timeout / 2).clamp(Duration::from_millis(50), Duration::from_secs(15));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let idle: Vec<String> = sessions
                    .active
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|s| s.idle_for() > sessions.idle_timeout)
                    .map(|s| s.id.clone())
                    .collect();
                for id in idle {
                    tracing::info!(session = %id, "ending idle session");
                    sessions.end(&id).await;
                }
            }
        });
    }

    /// Pick the best available source for an item and open a read lease
    /// on its mediahost. Used at session start and on seek-restarts of
    /// local remux sessions (whose lease died with the old worker).
    /// Single-part only (subtitle extraction and friends).
    pub(crate) async fn open_source(
        &self,
        registry: &Registry,
        item_id: &str,
        reader: Reader,
    ) -> Result<(String, String, u64, kahawai_core::media::MediaInfo, Lease)> {
        let (parts, info) = self.source_parts(registry, item_id).await?;
        let p = &parts[0];
        let lease = self
            .open_lease(
                registry,
                &p.module_id,
                &p.collection_id,
                &p.root_token,
                &p.path_rel,
                reader,
            )
            .await?;
        Ok((p.module_id.clone(), p.path_rel.clone(), p.size, info, lease))
    }

    /// All parts of the item's best available source in timeline order:
    /// a complete single-file source wins; otherwise the CD1/CD2-style
    /// part set (with cumulative timeline bases from per-part durations).
    pub(crate) async fn source_parts(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<(Vec<PartSource>, kahawai_core::media::MediaInfo)> {
        let rows = self.playable_rows(registry, item_id).await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let parse_info = |r: &sqlx::sqlite::SqliteRow| -> kahawai_core::media::MediaInfo {
            serde_json::from_str(r.get::<String, _>("streams_json").as_str()).unwrap_or_default()
        };
        let mut any_complete = false;
        for group in rows.chunk_by(|a, b| {
            a.get::<i64, _>("playable_source_id") == b.get::<i64, _>("playable_source_id")
        }) {
            let expected = group[0].get::<i64, _>("expected_parts");
            let ordinals: Vec<i64> = group.iter().map(|r| r.get("part")).collect();
            let complete =
                group.len() as i64 == expected && ordinals.iter().copied().eq(1..=expected);
            if !complete {
                continue;
            }
            any_complete = true;
            if !group
                .iter()
                .all(|r| registry.is_connected(&r.get::<String, _>("module_id")))
            {
                continue;
            }
            let mut parts = Vec::with_capacity(group.len());
            let mut base = 0;
            let mut first_info = None;
            for r in group {
                let info = parse_info(r);
                let duration_ms = if expected > 1 {
                    info.duration_ms
                        .context("multi-part source with unknown part duration")?
                } else {
                    info.duration_ms.unwrap_or(0)
                };
                parts.push(PartSource {
                    file_id: r.get("file_id"),
                    module_id: r.get("module_id"),
                    collection_id: r.get("collection_id"),
                    root_token: r.get("root_token"),
                    path_rel: r.get("source_path"),
                    mtime_unix: r.get("mtime_unix"),
                    size: r.get::<i64, _>("size") as u64,
                    base_ms: base,
                    duration_ms,
                });
                base += duration_ms;
                first_info.get_or_insert(info);
            }
            return Ok((parts, first_info.unwrap()));
        }
        if any_complete {
            bail!(SourceOffline);
        }
        bail!("every playable rendition is incomplete or ambiguous")
    }

    async fn playable_rows(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
        Ok(sqlx::query(
            "SELECT ps.id AS playable_source_id,ps.expected_parts,
                    p.ordinal AS part,f.id AS file_id,f.module_id,f.collection_id,r.root_token,
                    f.path_rel AS source_path,f.size,f.mtime_unix,f.streams_json
             FROM playable_sources ps
             JOIN playable_source_parts p ON p.playable_source_id=ps.id
             JOIN files f ON f.id=p.file_id
             JOIN collection_roots r ON r.id=f.root_id
             WHERE ps.item_id=?
             ORDER BY ps.expected_parts>1,
                      (SELECT MIN(COALESCE(json_extract(f2.streams_json,'$.video[0].height'),0))
                         FROM playable_source_parts p2 JOIN files f2 ON f2.id=p2.file_id
                        WHERE p2.playable_source_id=ps.id) DESC,
                      (SELECT MIN(COALESCE(f2.revision,1))
                         FROM playable_source_parts p2 JOIN files f2 ON f2.id=p2.file_id
                        WHERE p2.playable_source_id=ps.id) DESC,
                      (SELECT SUM(f2.size) FROM playable_source_parts p2
                         JOIN files f2 ON f2.id=p2.file_id
                        WHERE p2.playable_source_id=ps.id) DESC,
                      ps.id,p.ordinal,f.id",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?)
    }

    /// HUB-16: EVERY playable candidate, in the established rank order —
    /// each connected complete file is one candidate, plus at most one
    /// part-set candidate at the end. `source_parts` remains "the best
    /// by rank"; negotiation instead judges each candidate by COST and
    /// only falls back to rank as the tiebreak.
    pub async fn candidate_sources(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<Vec<(Vec<PartSource>, kahawai_core::media::MediaInfo)>> {
        let rows = self.playable_rows(registry, item_id).await?;
        let parse_info = |r: &sqlx::sqlite::SqliteRow| -> kahawai_core::media::MediaInfo {
            serde_json::from_str(r.get::<String, _>("streams_json").as_str()).unwrap_or_default()
        };
        let mut out = Vec::new();
        let mut any_complete = false;
        for group in rows.chunk_by(|a, b| {
            a.get::<i64, _>("playable_source_id") == b.get::<i64, _>("playable_source_id")
        }) {
            let expected = group[0].get::<i64, _>("expected_parts");
            let ordinals: Vec<i64> = group.iter().map(|r| r.get("part")).collect();
            if group.len() as i64 != expected || !ordinals.iter().copied().eq(1..=expected) {
                continue;
            }
            any_complete = true;
            if !group
                .iter()
                .all(|r| registry.is_connected(&r.get::<String, _>("module_id")))
            {
                continue;
            }
            let mut parts = Vec::with_capacity(group.len());
            let mut base_ms = 0;
            let mut first_info = None;
            for r in group {
                let info = parse_info(r);
                let duration_ms = if expected > 1 {
                    info.duration_ms
                        .context("multi-part source with unknown part duration")?
                } else {
                    info.duration_ms.unwrap_or(0)
                };
                parts.push(PartSource {
                    file_id: r.get("file_id"),
                    module_id: r.get("module_id"),
                    collection_id: r.get("collection_id"),
                    root_token: r.get("root_token"),
                    path_rel: r.get("source_path"),
                    mtime_unix: r.get("mtime_unix"),
                    size: r.get::<i64, _>("size") as u64,
                    base_ms,
                    duration_ms,
                });
                base_ms += duration_ms;
                first_info.get_or_insert(info);
            }
            out.push((parts, first_info.unwrap()));
        }
        if out.is_empty() && !any_complete && !rows.is_empty() {
            bail!("every playable rendition is incomplete or ambiguous");
        }
        Ok(out)
    }

    /// Does this item have any source rows at all, connected or not?
    ///
    /// The difference between "wait, the host is away" and "there is nothing
    /// here": `candidate_sources` returning empty cannot tell them apart, and
    /// answering 503 to both had clients retrying a condition that no amount
    /// of waiting fixes.
    pub(crate) async fn has_any_source(&self, registry: &Registry, item_id: &str) -> bool {
        sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM playable_sources WHERE item_id=?)",
        )
        .bind(item_id)
        .fetch_one(registry.db())
        .await
        .unwrap_or(0)
            == 1
    }

    /// Open a read lease on an arbitrary path within a collection (also
    /// used for sidecar subtitle files, which are not `files` rows).
    /// AR-5: register the in-process mediahost — leases for its files
    /// bypass OpenRead entirely and read the disk directly.
    /// Is this module's byte plane short-circuited to local reads
    /// (AR-11, all-in-one)? Burn-in's index walk needs it.
    pub fn reads_locally(&self, module_id: &str) -> bool {
        self.local_source
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(m, _)| m == module_id)
    }

    pub fn set_local_source(
        &self,
        module_id: &str,
        resolve: impl Fn(&str, &str, &str) -> Result<std::path::PathBuf> + Send + Sync + 'static,
    ) {
        *self.local_source.lock().unwrap() =
            Some((module_id.to_string(), std::sync::Arc::new(resolve)));
    }

    pub fn set_local_activity(
        &self,
        enter: impl Fn(&str) -> Box<dyn Send + Sync> + Send + Sync + 'static,
    ) {
        *self.local_activity.lock().unwrap() = Some(std::sync::Arc::new(enter));
    }

    pub fn set_local_background(
        &self,
        admit: impl Fn(&str) -> LocalAdmission + Send + Sync + 'static,
    ) {
        *self.local_background.lock().unwrap() = Some(std::sync::Arc::new(admit));
    }

    pub(crate) async fn open_lease(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        reader: Reader,
    ) -> Result<Lease> {
        // AR-5/AR-11: the in-process mediahost's byte plane is a
        // function call — resolve the path and read the disk directly.
        let local = {
            let guard = self.local_source.lock().unwrap();
            guard
                .as_ref()
                .and_then(|(id, resolve)| (id == module_id).then(|| resolve.clone()))
        };
        if let Some(resolve) = local {
            let activity_guard = (reader == Reader::Viewer)
                .then(|| {
                    self.local_activity
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|enter| enter(root_token))
                })
                .flatten();
            let background_admission = (reader == Reader::Sweep)
                .then(|| {
                    self.local_background
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|admit| admit(root_token))
                })
                .flatten();
            let resolution_permit = match &background_admission {
                Some(admit) => Some(admit().await?),
                None => None,
            };
            let collection_id = collection_id.to_string();
            let root_token_owned = root_token.to_string();
            let path_rel = path_rel.to_string();
            let path = tokio::task::spawn_blocking(move || {
                resolve(&collection_id, &root_token_owned, &path_rel)
            })
            .await
            .context("local media path resolution task failed")?;
            drop(resolution_permit);
            return Ok(Lease::local_guarded(
                path?,
                activity_guard,
                background_admission,
            ));
        }
        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id: collection_id.to_string(),
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: root_token.to_string(),
                    path_rel: path_rel.to_string(),
                }),
                background: reader == Reader::Sweep,
            })),
        };
        // A send failure here means the host went away between being judged
        // present and being asked for bytes — a window no ordering can close,
        // because candidate selection and this call are separated by DB work.
        // Left as a plain error it reached `session_refusal` as 409, "give up
        // on this item", for a source that is merely offline; the recovery
        // contract's answer to an absent host is 503 and stand by.
        self.leases
            .establish(&token, registry.send_to_host(module_id, msg))
            .await
            .map_err(|e| {
                if registry.is_connected(module_id) {
                    e
                } else {
                    anyhow::Error::new(SourceOffline).context(format!("{e:#}"))
                }
            })
    }

    /// Start a session for an item. With an explicit `mode` (scripts,
    /// debugging, old clients) the pre-negotiation behavior applies
    /// verbatim: best-ranked source, mode taken at its word. Without
    /// one, the hub NEGOTIATES (HUB-14): the client's capability
    /// profile — or the conservative fallback — is judged against
    /// every candidate source and the cheapest sufficient path wins
    /// (HUB-16: direct > copy > audio-encode > video-encode), rank
    /// breaking ties.
    #[allow(clippy::too_many_arguments)] // request-shaped plumbing
    /// Mint the session id FIRST, then do the work.
    ///
    /// Everything below can fail — a source that is not currently
    /// available, an unplayable plan, a worker that will not start — and
    /// until an id exists a failure has nothing to attach a log to. That
    /// is why the id is minted here rather than deeper in: "no source is
    /// currently available (mediahost offline)" used to leave a 409 and
    /// no record whatsoever (OPS-10).
    #[allow(clippy::too_many_arguments)] // one call site, spelled out
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        subtitles: &crate::subtitles::Subtitles,
        user_id: &str,
        item_id: &str,
        mode: Option<&str>,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        start_ms: u64,
        audio_track: u32,
        video_track: u32,
        subtitle_track: Option<i64>,
    ) -> Result<Arc<Session>> {
        let id = ulid::Ulid::generate().to_string();
        self.note_session(&id, item_id);
        // The one admission point. Here rather than inside `start_inner`
        // because that function has thirteen early returns, every one of
        // which would otherwise have to remember to give the slot back.
        self.admit(&id, user_id)?;
        let started = {
            // ...and a fourteenth exit a trailing statement cannot cover: the
            // caller going away. `start_session` awaits this inline, so an
            // abandoned request — a closed tab, or a client that gave up on a
            // slow start — drops this future mid-`start_inner` and any code
            // after the await simply never runs. The id stayed in `reserved`
            // for the life of the process and went on counting, so four
            // abandoned starts left an account unable to begin anything until
            // the hub was restarted. Dropping is the one exit every path has.
            let _slot = SlotGuard {
                sessions: self,
                id: id.clone(),
            };
            self.start_inner(
                &id,
                registry,
                subtitles,
                user_id,
                item_id,
                mode,
                profile,
                start_ms,
                audio_track,
                video_track,
                subtitle_track,
            )
            .await
        };
        if let Err(e) = &started
            && let Some(data_dir) = self.scratch_root.parent()
        {
            // A session that never came up still gets a log, filed under
            // the item like any other — the item page is where somebody
            // looks after a report, and it cannot know how far the
            // session got.
            self.note_error(&id, &format!("{e:#}"));
            let (item, header) = self.log_header(&id);
            crate::sessionlog::store(data_dir, &item, &id, &header);
        }
        started
    }

    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    async fn start_inner(
        self: &Arc<Self>,
        id: &str,
        registry: &Registry,
        subtitles: &crate::subtitles::Subtitles,
        user_id: &str,
        item_id: &str,
        mode: Option<&str>,
        profile: Option<kahawai_core::media::CapabilityProfile>,
        start_ms: u64,
        audio_track: u32,
        video_track: u32,
        subtitle_track: Option<i64>,
    ) -> Result<Arc<Session>> {
        let id = id.to_string();
        let mut neg = Negotiation::new(
            self,
            registry,
            user_id,
            item_id,
            profile,
            audio_track,
            video_track,
            subtitle_track,
        )
        .await?;
        let (parts, info, sp, mode) = neg.best_source(item_id, mode).await?;
        for part in &parts {
            registry.hint_discovery(
                &part.module_id,
                "segments",
                &part.collection_id,
                kahawai_proto::v1::SourcePath::new(&part.root_token, &part.path_rel),
                "playback",
                15 * 60,
            );
        }
        // HUB-32b: a burn is only real once the display sets exist. Ask
        // the mediahost, and if they do not arrive, negotiate again
        // with the tier withdrawn — better an honest "unavailable"
        // than an encode that burns nothing.
        let mut burn_sets: Option<std::path::PathBuf> = None;
        let mut sp = sp;
        // Whatever the chosen source was judged with — a later re-plan
        // must not resurrect a tier an earlier one withdrew.
        let mut burn_capable = parts.first().is_some_and(|p| {
            registry.is_connected(&p.module_id) || self.reads_locally(&p.module_id)
        });
        // What to walk: the media file at the embedded index, or — for
        // a sidecar pick — the .idx at its in-idx track.
        let sets_ref = match (sp.plan.burn_subtitle, sp.burn_sidecar) {
            (Some(idx), _) => parts.first().map(|p| (p.path_rel.clone(), idx)),
            (None, Some(i)) => info
                .external_subtitles
                .get(i)
                .filter(|e| e.format == "vobsub")
                .map(|e| (e.path_rel.clone(), e.track.unwrap_or(0) as usize)),
            (None, None) => None,
        };
        if let Some((walk_rel, walk_idx)) = sets_ref
            && let Some(part) = parts.first()
        {
            burn_sets = subtitles
                .image_sets(
                    registry,
                    &part.module_id,
                    &part.collection_id,
                    &part.root_token,
                    &walk_rel,
                    walk_idx,
                    BURN_SETS_WAIT,
                )
                .await;
            if burn_sets.is_none() {
                tracing::warn!(
                    item = item_id,
                    track = walk_idx,
                    "burn-in: no display sets; re-planning without it"
                );
                // burn_capable=false also voids the pick: negotiate
                // ignores a pick it cannot honor.
                burn_capable = false;
                sp = neg.plan_probed(&parts, &info, false);
            }
        }
        // HUB-32d: the overlay rung only exists once a rasterised track
        // does. Asked AFTER the source is chosen and only when the
        // ladder would actually take it — rasterising for a client
        // that would flatten anyway is pure waste — and the answer
        // re-plans, exactly as failing display sets do one tier down.
        if neg.ass.overlay_reachable(neg.profile())
            && let Some(part) = parts.first()
            && subtitles
                .overlay_ready(
                    registry,
                    self,
                    item_id,
                    &part.module_id,
                    &part.collection_id,
                    &part.root_token,
                    &part.path_rel,
                    user_id,
                )
                .await
        {
            neg.ass.overlay_ready = true;
            sp = neg.plan_probed(&parts, &info, burn_capable);
        }
        // HUB-32a: a sidecar ASS burn needs the script itself, the same
        // way an image burn needs its display sets — the worker cannot
        // read the media's neighbourhood. Embedded burns need nothing:
        // they take the demuxer's own pad, which also carries the fonts.
        let mut burn_ass_text: Option<String> = None;
        if let Some(i) = sp.burn_ass_sidecar {
            burn_ass_text = subtitles
                .ass_for_burn(registry, self, item_id, &format!("s{i}"))
                .await;
            if burn_ass_text.is_none() {
                // Same honesty rule as the display sets: re-plan with
                // the tier withdrawn rather than encode video that burns
                // nothing.
                tracing::warn!(
                    item = item_id,
                    track = i,
                    "ASS burn: sidecar script unavailable; re-planning without it"
                );
                sp = neg.plan_probed(&parts, &info, burn_capable);
            }
        }
        // No refusal to raise: the ladder is a permutation and flatten
        // is always possible, so `AssPolicy::choose` is total and a burn
        // is only ever planned when some box can perform it.
        let mut burns_ass = sp.plan.burn_ass.is_some() || sp.burn_ass_sidecar.is_some();
        if sp.cost == kahawai_media::negotiate::Cost::Unplayable && mode != "direct" {
            // The verdict names the actual blocker — a client refusing
            // the encode target reads very differently from a fleet
            // with no transcoder (HUB-14 honesty; found via the mask).
            bail!(
                "no playable streams: {} · {}",
                sp.video_verdict,
                sp.audio_verdict
            );
        }
        let mut negotiated = sp;
        let mode = mode.as_str();
        if parts.len() > 1 && mode == "direct" {
            bail!("multi-part sources play via remux/transcode, not direct");
        }
        let total_ms: u64 = parts.iter().map(|p| p.duration_ms).sum();
        let start_idx = part_index(&parts, start_ms);
        let part = parts[start_idx].clone();
        let local_ms = start_ms.saturating_sub(part.base_ms);
        let (module_id, path_rel, size) =
            (part.module_id.clone(), part.path_rel.clone(), part.size);
        let lease = self
            .open_lease(
                registry,
                &part.module_id,
                &part.collection_id,
                &part.root_token,
                &part.path_rel,
                Reader::Viewer,
            )
            .await?;

        let mut chosen_sink = String::new();
        let mut verdict = None;
        let mut session_plan = None;
        let mut session_needs = crate::registry::PlacementNeed::default();
        // HUB-36: the kind of work this session IS, derived here because
        // this is where the plan and the source metadata are both in
        // scope. Whatever box runs it reports a pace sample against this
        // string, so the two can never describe different things.
        let mut session_class = String::new();
        let session_mode = match mode {
            "direct" => Mode::Direct { lease },
            "remux" => {
                // The muxer stalls on unfed pads, so only claim what the
                // plan will actually feed — the negotiated plan is the
                // single source of truth with the pipeline's link logic.
                let ordinary_negotiated = if neg.loudness.force() {
                    neg.plan_for_protocol(&parts, &info, burn_capable, None)
                } else {
                    negotiated.clone()
                };
                let mut plan = negotiated.plan;
                if !plan.playable() {
                    bail!(
                        "no playable streams: {} · {}",
                        negotiated.video_verdict,
                        negotiated.audio_verdict
                    );
                }
                fill_audio_loudness_gains(
                    registry,
                    &parts,
                    &mut plan,
                    neg.loudness,
                    neg.force_measurement.clone(),
                )
                .await?;
                if !neg.loudness.force()
                    && plan.video == kahawai_media::remux::StreamMode::Encode
                    && let Some(required) = loudness_protocol_feature(&plan)
                {
                    let mut candidate =
                        neg.plan_for_protocol(&parts, &info, burn_capable, Some(required));
                    if !candidate.plan.playable()
                        || candidate.incomplete != ordinary_negotiated.incomplete
                        || candidate.plan.audio != plan.audio
                        || !same_video_path(&candidate.plan, &plan)
                        || candidate.burn_sidecar != ordinary_negotiated.burn_sidecar
                        || candidate.burn_ass_sidecar != ordinary_negotiated.burn_ass_sidecar
                    {
                        // Default normalization is optional and must not alter
                        // the video or subtitle path. If no exact-gain worker
                        // can execute that path, preserve playback without gain.
                        apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                    } else {
                        fill_audio_loudness_gains(
                            registry,
                            &parts,
                            &mut candidate.plan,
                            neg.loudness,
                            None,
                        )
                        .await?;
                        plan = candidate.plan;
                        burns_ass = candidate.plan.burn_ass.is_some()
                            || candidate.burn_ass_sidecar.is_some();
                        negotiated = candidate;
                    }
                }
                verdict = Some((
                    negotiated.video_verdict.clone(),
                    negotiated.audio_verdict.clone(),
                ));
                session_plan = Some(plan);
                (session_needs, session_class) = placement_need(&plan, &info, &parts, burns_ass);
                // Encode work goes to the fleet when one is available
                // (§4.5); pure remux — and encode with no fleet — stays
                // in the local supervised worker.
                // HUB-36 phase 5: the placement now carries what it is
                // expected to sustain, so a session that will crawl says
                // so instead of letting the viewer discover it.
                let place = |need: &crate::registry::PlacementNeed| {
                    if need.encode_video || need.encode_audio {
                        registry.place(need)
                    } else {
                        crate::registry::Placement {
                            target: None,
                            available: true,
                            predicted: None,
                        }
                    }
                };
                let mut placement = place(&session_needs);
                if !placement.available && session_needs.required_protocol_feature.is_some() {
                    // Capacity and hard constraints can change after the
                    // compatible probe. Retry the exact ordinary plan with no
                    // protocol requirement rather than turning that race into
                    // a playback failure or a force-only unity-gain encode.
                    negotiated = ordinary_negotiated;
                    plan = negotiated.plan;
                    apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                    burns_ass = plan.burn_ass.is_some() || negotiated.burn_ass_sidecar.is_some();
                    verdict = Some((
                        negotiated.video_verdict.clone(),
                        negotiated.audio_verdict.clone(),
                    ));
                    session_plan = Some(plan);
                    (session_needs, session_class) =
                        placement_need(&plan, &info, &parts, burns_ass);
                    placement = place(&session_needs);
                }
                anyhow::ensure!(
                    plan.playable(),
                    "no playable streams after loudness protocol fallback: {} · {}",
                    negotiated.video_verdict,
                    negotiated.audio_verdict
                );
                anyhow::ensure!(
                    placement.available,
                    "video transcoding unavailable: no capable external transcoder or enabled all-in-one transcoder"
                );
                let placed = placement.target.clone();
                if let Some(p) = placement.predicted
                    && p < 1.0
                {
                    // AR-13: below realtime is placed anyway — refusing
                    // would leave a slow fleet unusable — but it is
                    // never placed SILENTLY.
                    tracing::warn!(
                        session = %id,
                        box_id = placed.as_deref().unwrap_or("local"),
                        class = session_needs.work_class.as_deref().unwrap_or("-"),
                        predicted = p,
                        "placed below realtime; playback may stall"
                    );
                    fold_facts(
                        &mut verdict,
                        &[kahawai_media::facts::Fact {
                            kind: "video".into(),
                            detail: format!("predicted {p:.1}x realtime — may stall"),
                        }],
                    );
                }
                // Read once: the same bytes serve both dispatch attempts.
                let sets_bytes = match &burn_sets {
                    Some(p) => std::fs::read(p).unwrap_or_default(),
                    None => Vec::new(),
                };
                let ass_bytes = burn_ass_text.clone().unwrap_or_default().into_bytes();
                match placed {
                    Some(tc) => {
                        // `place` reserved this box. Exactly one owner
                        // of that reservation: this branch. It is held
                        // across the sink-fallback retry — which reuses
                        // the same box, so releasing between attempts
                        // would leave a successful retry uncounted —
                        // and returned on every failing path below.
                        let dispatched = self
                            .dispatch_to(
                                registry,
                                &tc,
                                &id,
                                plan,
                                &parts,
                                start_idx,
                                local_ms,
                                &sets_bytes,
                                &ass_bytes,
                            )
                            .await;
                        let (facts, sink) = match dispatched {
                            Ok(v) => v,
                            Err(e) => {
                                registry.tc_session_ended(&tc);
                                return Err(e);
                            }
                        };
                        chosen_sink = sink;
                        fold_facts(&mut verdict, &facts);
                        Mode::Transcode {
                            transcoder: Mutex::new(tc),
                        }
                    }
                    None => {
                        let tail = self.open_part_leases(registry, &parts, start_idx).await?;
                        let (runner, facts) = match self
                            .start_remux(
                                &id,
                                plan,
                                negotiated.target_duration_secs,
                                tail,
                                local_ms,
                                "",
                                burn_sets.as_deref(),
                                burn_ass_text.as_deref(),
                            )
                            .await
                        {
                            Ok(r) => r,
                            Err(first)
                                if plan.segment_format
                                    != kahawai_media::remux::SegmentFormat::Ts =>
                            {
                                return Err(first); // fmp4 has no sink fallback
                            }
                            Err(first) => {
                                tracing::warn!(session = %id, error = format!("{first:#}"),
                                    "start failed; retrying with fallback sink");
                                let tail =
                                    self.open_part_leases(registry, &parts, start_idx).await?;
                                let r = self
                                    .start_remux(
                                        &id,
                                        plan,
                                        negotiated.target_duration_secs,
                                        tail,
                                        local_ms,
                                        "hlssink2",
                                        burn_sets.as_deref(),
                                        burn_ass_text.as_deref(),
                                    )
                                    .await
                                    .with_context(|| format!("first attempt: {first:#}"))?;
                                chosen_sink = "hlssink2".into();
                                r
                            }
                        };
                        fold_facts(&mut verdict, &facts);
                        Mode::Remux {
                            runner: Mutex::new(runner),
                            dir: self.scratch_root.join(&id),
                        }
                    }
                }
            }
            other => bail!("unknown mode {other:?} (direct|remux)"),
        };

        // Fill the unified track ids into the verdicts: the negotiation
        // speaks stream indexes, the API speaks track rows.
        let mut sub_verdicts = negotiated.subtitles;
        fill_verdict_track_ids(registry, &parts, &mut sub_verdicts).await;
        let burn_pick = burn_sets.as_ref().and_then(|_| neg.pick_for(&parts));
        let session_duration = if parts.len() > 1 {
            Some(total_ms)
        } else {
            info.duration_ms
        };
        // Where this watch begins, on the same 90 percent rule progress
        // uses. A session that opens PAST the line is a continuation —
        // recovery restarts at the position that was lost — so its first
        // report must not read as a crossing.
        let started_finished =
            session_duration.is_some_and(|d| d > 0 && start_ms.saturating_mul(10) >= d * 9);
        let session = Arc::new(Session {
            id,
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            module_id,
            size,
            container: info.container.clone(),
            duration_ms: session_duration,
            parts,
            current_part: std::sync::atomic::AtomicUsize::new(start_idx),
            mode: session_mode,
            verdict: Mutex::new(verdict),
            sub_verdicts: Mutex::new(sub_verdicts),
            profile: neg.profile().clone(),
            target_duration_secs: negotiated.target_duration_secs,
            burn_sets: Mutex::new(burn_sets.clone()),
            burn_pick: Mutex::new(burn_pick),
            ass: neg.ass.clone(),
            burn_ass_text: Mutex::new(burn_ass_text),
            loudness: neg.loudness,
            force_loudness: neg.force_audio_encode,
            sink: Mutex::new(chosen_sink),
            seek_lock: tokio::sync::Mutex::new(()),
            pending_seek: Mutex::new(None),
            seek_gen: std::sync::atomic::AtomicU64::new(0),
            seek_done: tokio::sync::watch::channel((0, Ok(0))).0,
            plan: Mutex::new(session_plan),
            needs: Mutex::new(session_needs),
            pace_class: session_class,
            touched: Mutex::new(std::time::Instant::now()),
            ending: tokio::sync::RwLock::new(false),
            watch_finish: std::sync::atomic::AtomicU8::new(if started_finished {
                WATCH_FINISHED
            } else {
                0
            }),
        });
        self.active
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        registry.emit(crate::registry::RegistryEvent::Sessions { kind: "sessions" });
        Ok(session)
    }

    /// Leases for every part from `from` onward, in timeline order.
    ///
    /// A remux pipeline spans the rest of the source, so it needs them
    /// all up front: concat holds each later part blocked until the one
    /// before it ends, but the branch has to exist before that happens.
    /// Costs one lease per remaining part instead of one per session —
    /// paid once, at the start, rather than as a stall at every boundary.
    async fn open_part_leases(
        &self,
        registry: &Registry,
        parts: &[PartSource],
        from: usize,
    ) -> Result<Vec<(Lease, u64)>> {
        let mut out = Vec::with_capacity(parts.len().saturating_sub(from));
        for part in &parts[from..] {
            let lease = self
                .open_lease(
                    registry,
                    &part.module_id,
                    &part.collection_id,
                    &part.root_token,
                    &part.path_rel,
                    Reader::Viewer,
                )
                .await?;
            out.push((lease, part.size));
        }
        Ok(out)
    }

    /// Spin up the remux/transcode pipeline — in a supervised worker
    /// process when configured — feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    #[allow(clippy::too_many_arguments)] // the session's shape, spelled out
    async fn start_remux(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        // What the playlist will DECLARE (not what the sink cuts on):
        // the readiness gate hands over enough runway for a client
        // reloading at that cadence, so it has to know the number.
        target_secs: u32,
        parts: Vec<(Lease, u64)>,
        start_ms: u64,
        sink: &str,
        // HUB-32b: display sets the mediahost walked for us.
        sets: Option<&std::path::Path>,
        // HUB-32a: a sidecar `.ass` script to burn, as TEXT rather than
        // a path — this function wipes and recreates the session dir,
        // so anything written beforehand would not survive. Embedded
        // ASS needs nothing here: it burns from the demuxer's own pad.
        ass: Option<&str>,
    ) -> Result<(RemuxRunner, Vec<kahawai_media::facts::Fact>)> {
        let dir = self.scratch_root.join(session_id);
        // ALWAYS from a clean dir: a crashed first attempt leaves its
        // socket (EADDRINUSE killed the TC-6 fallback) and a stale
        // playlist the readiness check would mistake for output.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        anyhow::ensure!(!parts.is_empty(), "no source parts for the session");
        let ass_path = match ass {
            Some(text) => {
                let p = dir.join("burn.ass");
                std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
                Some(p)
            }
            None => None,
        };

        let runner = match &self.worker_exe {
            Some(exe) => {
                // ponytail: SUN_LEN caps unix socket paths at ~108
                // bytes — a pathologically deep data_dir breaks remux
                // here. Bind under a short tmp dir if it ever bites a
                // real deployment.
                // One socket per part: the worker joins them with concat
                // into a single pipeline, so a CD1->CD2 boundary is not a
                // restart. Part one keeps the historical name and the
                // positional argument; the rest arrive as --part.
                let mut socks = Vec::with_capacity(parts.len());
                for (n, (lease, size)) in parts.iter().enumerate() {
                    let sock = if n == 0 {
                        dir.join("worker.sock")
                    } else {
                        dir.join(format!("worker{n}.sock"))
                    };
                    let listener = tokio::net::UnixListener::bind(&sock)
                        .with_context(|| format!("binding {}", sock.display()))?;
                    // Serve source reads for the session's life; the task
                    // ends when the worker closes its socket.
                    let (lease, size) = (lease.clone(), *size);
                    tokio::spawn(async move {
                        match listener.accept().await {
                            Ok((conn, _)) => {
                                if let Err(e) = serve_reads(conn, lease, size).await {
                                    tracing::debug!(error = %e, "worker read channel closed");
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "worker never connected"),
                        }
                    });
                    socks.push((sock, size));
                }
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let mut cmd = tokio::process::Command::new(exe);
                cmd.arg("remux-worker")
                    .arg(&socks[0].0)
                    .arg(&dir)
                    .arg(socks[0].1.to_string());
                // Who to die with. The worker compares this against its
                // own getppid(); see the guard in run_remux_worker.
                cmd.args(["--supervisor-pid", &std::process::id().to_string()]);
                for (sock, size) in &socks[1..] {
                    cmd.args(["--part", &format!("{}:{size}", sock.display())]);
                }
                for (flag, v) in [
                    ("--video-kbps", plan.video_kbps),
                    ("--max-height", plan.max_height),
                    ("--max-channels", plan.max_channels),
                ] {
                    if let Some(v) = v {
                        cmd.args([flag, &v.to_string()]);
                    }
                }
                if let Some(gain) = plan.stereo_gain_db {
                    cmd.args(["--stereo-gain-db", &gain.to_string()]);
                }
                if let Some(gain) = plan.native_gain_db {
                    cmd.args(["--native-gain-db", &gain.to_string()]);
                }
                if let Some(channels) = plan.loudness_source_channels {
                    cmd.args(["--loudness-source-channels", &channels.to_string()]);
                }
                if plan.loudness_gains.iter().any(Option::is_some) {
                    let gains = plan
                        .loudness_gains
                        .iter()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>();
                    cmd.args(["--loudness-gains", &serde_json::to_string(&gains)?]);
                }
                if plan.deinterlace {
                    cmd.arg("--deinterlace");
                }
                if plan.tone_map {
                    cmd.arg("--tone-map");
                }
                if let Some(n) = plan.burn_subtitle {
                    cmd.args(["--burn-sub", &n.to_string()]);
                }
                if let Some(p) = &sets {
                    cmd.args(["--burn-sets", &p.to_string_lossy()]);
                }
                if let Some(n) = plan.burn_ass {
                    cmd.args(["--burn-ass", &n.to_string()]);
                }
                if let Some(p) = &ass_path {
                    cmd.args(["--burn-ass-file", &p.to_string_lossy()]);
                }
                let child = cmd
                    .args(["--video", kahawai_media::worker::mode_arg(plan.video)])
                    .args(["--audio", kahawai_media::worker::mode_arg(plan.audio)])
                    .args(["--video-codec", plan.video_codec.as_str()])
                    .args(["--audio-codec", plan.audio_codec.as_str()])
                    .args(["--container", plan.segment_format.as_str()])
                    .args(["--audio-track", &plan.audio_track.to_string()])
                    .args(["--video-track", &plan.video_track.to_string()])
                    .args(["--start-ms", &start_ms.to_string()])
                    .args(if sink.is_empty() {
                        vec![]
                    } else {
                        vec!["--sink".into(), sink.to_string()]
                    })
                    // BOTH streams. The dispatched path (transcoder
                    // sessions.rs) learned this the hard way and this
                    // copy never got the fix: `tracing_subscriber` writes
                    // to STDOUT, so capturing stderr alone kept the
                    // GStreamer C-side output and Rust panics — which is
                    // why crash capture looked fine — while every
                    // `tracing::info!` the worker emitted went to the
                    // detached parent's stdout and was discarded. A
                    // locally-remuxed session's worker.log was 0 bytes
                    // for its whole life, and OPS-10 bundled that.
                    .stdout(std::process::Stdio::from(log.try_clone()?))
                    .stderr(std::process::Stdio::from(log))
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning worker {}", exe.display()))?;
                tracing::info!(
                    session = session_id,
                    pid = child.id(),
                    "pipeline worker spawned"
                );
                RemuxRunner::Worker(Mutex::new(child))
            }
            None => {
                // In-process (tests): the pipeline pulls (and seeks —
                // MP4 moov-at-end needs it) from the lease via a blocking
                // adapter on the remux feeder thread.
                let handle = tokio::runtime::Handle::current();
                let sources: Vec<Box<dyn kahawai_media::remux::RemuxSource>> = parts
                    .into_iter()
                    .map(|(lease, size)| {
                        Box::new(LeaseSource {
                            lease,
                            size,
                            handle: handle.clone(),
                            reads: 0,
                        }) as Box<dyn kahawai_media::remux::RemuxSource>
                    })
                    .collect();
                // start_at blocks while prerolling for an offset seek —
                // off the async runtime with it, or the preroll's own
                // lease reads can never be driven (single-thread runtimes
                // deadlock outright).
                let dir2 = dir.clone();
                let sink_owned = (!sink.is_empty()).then(|| sink.to_string());
                let sets_owned = sets.map(|p| p.to_path_buf());
                let ass_owned = ass_path.clone();
                let job = tokio::task::spawn_blocking(move || {
                    kahawai_media::remux::start_parts(
                        &dir2,
                        plan,
                        sources,
                        start_ms,
                        sink_owned.as_deref(),
                        None,
                        sets_owned.as_deref(),
                        ass_owned.as_deref(),
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("worker task panicked: {e}"))??;
                RemuxRunner::InProcess(Arc::new(job))
            }
        };

        // Return once the playlist has enough runway (or ended): a
        // playlist handed over with one ~3 s segment guarantees a stall
        // right after it — hls.js only discovers more segments on its
        // next live reload (~target duration later).
        let playlist = dir.join("master.m3u8");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match &runner {
                RemuxRunner::InProcess(job) => {
                    if let Some(e) = job.failed() {
                        bail!("remux failed to start: {e}");
                    }
                }
                RemuxRunner::Worker(child) => {
                    if let Some(status) = child.lock().unwrap().try_wait()? {
                        // A CLEAN exit is a pipeline that FINISHED: an
                        // all-copy remux of short content completes in
                        // under a second — faster than this poll. The
                        // playlist (with ENDLIST) is the product; fall
                        // through to the ready-check below instead of
                        // declaring death. Only a non-zero exit, or a
                        // clean exit with nothing produced, is failure.
                        if !(status.success() && playlist_ready(&playlist, target_secs)) {
                            let log =
                                std::fs::read_to_string(dir.join("worker.log")).unwrap_or_default();
                            let tail: String =
                                log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                            // Keep the stderr BEFORE the retry wipes this
                            // dir: a panic's message names the file and
                            // line, and the four lines quoted below are
                            // the frames after it, which name nothing.
                            if let Some(data_dir) = self.scratch_root.parent() {
                                // OPS-10: this session is NOT in `active`
                                // — registration happens after start
                                // succeeds — which is why note_session
                                // ran when the id was minted, and why
                                // this bundle is the only trace it ever
                                // existed.
                                let _ = &log;
                                let (item, header) = self.log_header(session_id);
                                let body = format!("{header}{}", local_bundle(&dir));
                                crate::sessionlog::store(data_dir, &item, session_id, &body);
                            }
                            bail!("pipeline worker exited at start ({status}): {tail}");
                        }
                    }
                }
                RemuxRunner::Stopped => unreachable!("start_remux never yields Stopped"),
            }
            if playlist_ready(&playlist, target_secs) {
                return Ok((runner, kahawai_media::facts::read(&dir)));
            }
            if std::time::Instant::now() > deadline {
                runner.stop();
                bail!("remux produced no playlist in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// One dispatch attempt plus TC-6's single sink fallback, as one
    /// fallible unit — so the caller holding the box's reservation has
    /// exactly one place to give it back. Returns the preroll facts and
    /// the sink that actually worked.
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    async fn dispatch_to(
        self: &Arc<Self>,
        registry: &Registry,
        tc: &str,
        id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        parts: &[PartSource],
        start_idx: usize,
        local_ms: u64,
        sets_bytes: &[u8],
        ass_bytes: &[u8],
    ) -> Result<(Vec<kahawai_media::facts::Fact>, String)> {
        let leases = self.open_part_leases(registry, parts, start_idx).await?;
        let first = match self
            .start_transcode(
                registry,
                tc,
                id,
                plan,
                leases,
                start_idx,
                local_ms,
                "",
                sets_bytes.to_vec(),
                ass_bytes.to_vec(),
            )
            .await
        {
            Ok(f) => return Ok((f, String::new())),
            // fmp4 has no sink fallback.
            Err(e) if plan.segment_format != kahawai_media::remux::SegmentFormat::Ts => {
                return Err(e);
            }
            Err(e) => e,
        };
        // TC-6: one retry on the fallback HLS sink — two library files
        // crash hlssink3 but mux fine on hlssink2 (upstream fix pending).
        tracing::warn!(session = %id, error = format!("{first:#}"),
            "start failed; retrying with fallback sink");
        let leases = self.open_part_leases(registry, parts, start_idx).await?;
        let f = self
            .start_transcode(
                registry,
                tc,
                id,
                plan,
                leases,
                start_idx,
                local_ms,
                "hlssink2",
                sets_bytes.to_vec(),
                ass_bytes.to_vec(),
            )
            .await
            .with_context(|| format!("first attempt: {first:#}"))?;
        Ok((f, "hlssink2".into()))
    }

    /// Dispatch a session to a transcoder and wait for its playlist.
    #[allow(clippy::too_many_arguments)] // private plumbing, one call site per mode
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    async fn start_transcode(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        parts: Vec<(Lease, u64)>,
        part_idx: usize,
        start_ms: u64,
        sink: &str,
        // HUB-32b: a dispatched worker can no more walk the source
        // index than the hub can, so the display sets ride along.
        burn_sets: Vec<u8>,
        // HUB-32a: and neither can it read the media's neighbourhood,
        // so a sidecar `.ass` rides along the same way. Empty for an
        // embedded burn, which comes off the demuxer's own pad.
        burn_ass_file: Vec<u8>,
    ) -> Result<Vec<kahawai_media::facts::Fact>> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        anyhow::ensure!(!parts.is_empty(), "no source parts to dispatch");
        let size = parts[0].1;
        let tail_sizes: Vec<u64> = parts[1..].iter().map(|(_, n)| *n).collect();
        self.tc_leases
            .lock()
            .unwrap()
            .insert(session_id.to_string(), (parts, part_idx));
        self.pending_ready
            .lock()
            .unwrap()
            .insert(session_id.to_string(), ready_tx);
        let (stereo_gain_db, native_gain_db, loudness_source_channels) =
            wire_scalar_loudness(&plan);

        let start = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::StartSession(
                kahawai_proto::v1::StartSession {
                    session_id: session_id.to_string(),
                    size,
                    video: kahawai_media::worker::mode_arg(plan.video).into(),
                    audio: kahawai_media::worker::mode_arg(plan.audio).into(),
                    audio_track: plan.audio_track as u32,
                    video_track: plan.video_track as u32,
                    start_ms,
                    sink: sink.into(),
                    tail_sizes,
                    video_kbps: plan.video_kbps.unwrap_or(0),
                    max_height: plan.max_height.unwrap_or(0),
                    max_channels: plan.max_channels.unwrap_or(0),
                    stereo_gain_db,
                    native_gain_db,
                    loudness_source_channels,
                    loudness_gains: plan
                        .loudness_gains
                        .iter()
                        .flatten()
                        .map(|gain| kahawai_proto::v1::AudioLayoutGain {
                            channels: gain.layout.channels,
                            channel_mask: gain.layout.channel_mask,
                            gain_db: gain.gain_db,
                        })
                        .collect(),
                    tone_map: plan.tone_map,
                    deinterlace: plan.deinterlace,
                    // 1-based on the wire: 0 means "burn nothing".
                    burn_subtitle: plan.burn_subtitle.map_or(0, |n| n as u32 + 1),
                    burn_sets: burn_sets.clone(),
                    // 1-based on the wire, same as burn_subtitle.
                    burn_ass: plan.burn_ass.map_or(0, |n| n as u32 + 1),
                    burn_ass_file: burn_ass_file.clone(),
                    video_codec: plan.video_codec.as_str().into(),
                    audio_codec: plan.audio_codec.as_str().into(),
                    container: plan.segment_format.as_str().into(),
                },
            )),
        };
        let cleanup = |sessions: &Self| {
            sessions.tc_leases.lock().unwrap().remove(session_id);
            sessions.pending_ready.lock().unwrap().remove(session_id);
        };
        if let Err(e) = registry
            .send_to_tc_requiring(transcoder, start, loudness_protocol_feature(&plan))
            .await
        {
            cleanup(self);
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(40), ready_rx).await {
            Ok(Ok(Ok(facts))) => {
                // No increment here: the slot has been held since the
                // pick. Counting again would double it.
                tracing::info!(session = session_id, transcoder, "session dispatched");
                Ok(facts)
            }
            Ok(Ok(Err(e))) => {
                cleanup(self);
                bail!("transcoder rejected session: {e}");
            }
            Ok(Err(_)) | Err(_) => {
                cleanup(self);
                let _ = registry
                    .send_to_tc(
                        transcoder,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                kahawai_proto::v1::EndSession {
                                    session_id: session_id.to_string(),
                                },
                            )),
                        },
                    )
                    .await;
                bail!("transcoder produced no playlist in time");
            }
        }
    }

    /// Pacing (§4.6): forward the viewer's position to wherever the
    /// session's worker runs. Fire-and-forget — a missed update only
    /// delays a pause/resume by one ping.
    pub fn viewer_position(self: &Arc<Self>, registry: &Arc<Registry>, id: &str, position_ms: u64) {
        let Some(session) = self.get(id) else { return };
        match &session.mode {
            Mode::Remux { dir, .. } => {
                let _ = std::fs::write(dir.join("viewer.pos"), position_ms.to_string());
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let registry = registry.clone();
                let sid = id.to_string();
                tokio::spawn(async move {
                    let _ = registry
                        .send_to_tc(
                            &tc,
                            kahawai_proto::v1::HubToTc {
                                msg: Some(kahawai_proto::v1::hub_to_tc::Msg::ViewerPosition(
                                    kahawai_proto::v1::ViewerPosition {
                                        session_id: sid,
                                        position_ms,
                                    },
                                )),
                            },
                        )
                        .await;
                });
            }
            Mode::Direct { .. } => {}
        }
    }

    /// Link-facing: the transcoder reported the session ready or failed.
    /// Returns whether a pending start consumed the verdict (false → the
    /// session was already running; the caller may reschedule).
    pub fn transcode_verdict(&self, session_id: &str, result: ReadyVerdict) -> bool {
        if let Some(tx) = self.pending_ready.lock().unwrap().remove(session_id) {
            let _ = tx.send(result);
            true
        } else {
            false
        }
    }

    /// Link-facing: serve one source read for a dispatched session.
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub async fn source_read(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        offset: u64,
        len: u64,
        req: u64,
        part: u32,
    ) {
        let held = self.tc_leases.lock().unwrap().get(session_id).cloned();
        let Some((lease, size)) = held.and_then(|(parts, _)| parts.get(part as usize).cloned())
        else {
            tracing::debug!(
                session = session_id,
                part,
                "source read for unknown session/part"
            );
            return;
        };
        let len = len.min(kahawai_media::worker::MAX_READ);
        let want = if offset >= size {
            0
        } else {
            len.min(size - offset)
        };
        let mut buf = Vec::with_capacity(want as usize);
        if want > 0 {
            let mut stream = lease.read_range(offset, want).into_inner();
            while (buf.len() as u64) < want {
                match stream.recv().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        tracing::warn!(session = session_id, error = %e, "lease read failed");
                        break;
                    }
                    None => break,
                }
            }
            buf.truncate(want as usize);
        }
        let msg = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::SourceData(
                kahawai_proto::v1::SourceData {
                    session_id: session_id.to_string(),
                    offset,
                    data: buf,
                    req,
                    part,
                },
            )),
        };
        if let Err(e) = registry.send_to_tc(transcoder, msg).await {
            tracing::debug!(
                session = session_id,
                error = format!("{e:#}"),
                "source data undeliverable"
            );
        }
    }

    /// Link-facing: a chunk of a requested artifact arrived.
    pub fn artifact_chunk(&self, data: kahawai_proto::v1::ArtifactData) {
        let key = (data.session_id.clone(), data.name.clone());
        let tx = self.artifact_waiting.lock().unwrap().get(&key).cloned();
        if let Some(tx) = tx {
            let _ = tx.try_send(data);
        }
    }

    /// Fetch one artifact (playlist/segment) from the session's
    /// transcoder. ponytail: no cache — playlist polls and one-shot
    /// segment fetches are cheap on a LAN; add LRU when profiling says.
    pub async fn fetch_artifact(
        &self,
        registry: &Registry,
        session: &Session,
        name: &str,
    ) -> Result<Vec<u8>> {
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a transcode session");
        };
        let transcoder = transcoder.lock().unwrap().clone();
        let transcoder = transcoder.as_str();
        let key = (session.id.clone(), name.to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        self.artifact_waiting
            .lock()
            .unwrap()
            .insert(key.clone(), tx);
        let cleanup = || {
            self.artifact_waiting.lock().unwrap().remove(&key);
        };
        let req = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::FetchArtifact(
                kahawai_proto::v1::FetchArtifact {
                    session_id: session.id.clone(),
                    name: name.to_string(),
                },
            )),
        };
        if let Err(e) = registry.send_to_tc(transcoder, req).await {
            cleanup();
            return Err(e);
        }
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(chunk)) => {
                    if !chunk.error.is_empty() {
                        cleanup();
                        bail!("{}", chunk.error);
                    }
                    out.extend_from_slice(&chunk.data);
                    if chunk.eof {
                        cleanup();
                        return Ok(out);
                    }
                }
                Ok(None) | Err(_) => {
                    cleanup();
                    bail!("artifact fetch timed out");
                }
            }
        }
    }

    /// Seek-restart (§6): tear the session's pipeline down and start it
    /// again at `position_ms` (keyframe-snapped by the demuxer). Same
    /// session id, same URLs — the client re-attaches to a playlist that
    /// now begins at the offset.
    /// Returns the timeline base of the part the restart landed in
    /// (players add it to the pipeline's local start.pos).
    #[allow(clippy::too_many_arguments)] // request-shaped plumbing
    pub async fn seek(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        subtitles: &crate::subtitles::Subtitles,
        id: &str,
        position_ms: u64,
        audio_track: Option<u32>,
        video_track: Option<u32>,
        subtitle_track: Option<i64>,
    ) -> Result<u64> {
        let session = self.get(id).context("no such session")?;
        // Subtitle unification: an explicit pick may change what burns.
        // Resolved HERE, in request context (bounded like a session
        // start), so the detached executor only restarts with state
        // already in hand.
        let mut replan_subs = false;
        if let Some(tid) = subtitle_track
            && tid <= 0
        {
            // Sentinel: withdraw an explicit burn ("subtitles off" /
            // a client-rendered track picked after a burn).
            if session.burn_pick.lock().unwrap().take().is_some() {
                *session.burn_sets.lock().unwrap() = None;
                *session.burn_ass_text.lock().unwrap() = None;
                replan_subs = true;
            }
        } else if let Some(tid) = subtitle_track {
            // Typed, as in `Negotiation::new`. This is the path a LIVE viewer
            // takes when they change subtitles, and a stale id — a track
            // deleted from another tab, or one from before a rescan — arrived
            // through `session_refusal` as 409 "this item cannot be played".
            // The player's `switchBurn` then gave up on a film that was
            // playing perfectly well a second earlier.
            let track = crate::tracks::get_for_item(registry.db(), &session.item_id, tid)
                .await?
                .ok_or_else(|| NoSuchTrack {
                    item: session.item_id.clone(),
                    track: tid,
                })?;
            let part = session.parts.first().context("session has no parts")?;
            let is_image = crate::tracks::is_image_format(&track.format);
            // HUB-32a: an ASS pick has to reach negotiation too, but
            // for a different reason than an image one. It never forces
            // a burn — the `ass_fallback` preference is the only way
            // into that tier — it says WHICH track the tier applies to,
            // so switching language mid-film re-burns the new one
            // instead of silently keeping the first.
            let is_ass = matches!(track.format.as_str(), "ass" | "ssa");
            let new_pick = ((is_image || is_ass)
                && track.module_id.as_deref() == Some(part.module_id.as_str())
                && track.collection_id.as_deref() == Some(part.collection_id.as_str())
                && track.root_token.as_deref() == Some(part.root_token.as_str())
                && track.source_path.as_deref() == Some(part.path_rel.as_str()))
            .then(|| {
                let i = track.stream_index.unwrap_or(0) as usize;
                match track.origin.as_str() {
                    "embedded" => Some(kahawai_media::negotiate::BurnPick::Embedded(i)),
                    "sidecar" => Some(kahawai_media::negotiate::BurnPick::Sidecar(i)),
                    _ => None,
                }
            })
            .flatten();
            if (is_image || is_ass) && new_pick.is_none() {
                bail!(
                    "track {tid} is not part of the playing source; restart the session to burn it"
                );
            }
            // No capability check here either: a seek cannot move boxes
            // (HUB-15b), so the re-negotiation below simply walks THIS
            // executor's ladder and lands on the next rung it can serve.
            // Auto-burn already burning this very stream: adopt the
            // pick without touching the sets (no refetch needed).
            let already = matches!(new_pick,
                Some(kahawai_media::negotiate::BurnPick::Embedded(i))
                    if session.plan.lock().unwrap().is_some_and(
                        |pl| pl.burn_subtitle == Some(i) || pl.burn_ass == Some(i)));
            if already {
                *session.burn_pick.lock().unwrap() = new_pick;
            } else if new_pick != *session.burn_pick.lock().unwrap() {
                // A sidecar ASS burns from the FILE, so the script has
                // to be in hand before the restart — the same shape as
                // the display sets below, and for the same reason.
                *session.burn_ass_text.lock().unwrap() = match new_pick {
                    Some(kahawai_media::negotiate::BurnPick::Sidecar(_)) if is_ass => {
                        let text = subtitles
                            .ass_for_burn(registry, self, &session.item_id, &track.internal_key())
                            .await;
                        anyhow::ensure!(text.is_some(), "subtitle track {tid} has no ASS script");
                        text
                    }
                    _ => None,
                };
                let sets = match new_pick {
                    // An ASS pick has no display sets to walk; the
                    // renderer takes the demuxer's pad or the script.
                    Some(_) if is_ass => None,
                    Some(_) => {
                        let (module_id, collection_id, root_token, walk_rel, walk_idx, _) =
                            subtitles.extract_ref(registry, &track).await?;
                        let sets = subtitles
                            .image_sets(
                                registry,
                                &module_id,
                                &collection_id,
                                &root_token,
                                &walk_rel,
                                walk_idx,
                                BURN_SETS_WAIT,
                            )
                            .await;
                        anyhow::ensure!(
                            sets.is_some(),
                            "no display sets for track {tid} (mediahost offline or unindexed)"
                        );
                        sets
                    }
                    // Un-burn: the pick is withdrawn. The re-plan's auto
                    // rules may still burn (no-overlay client, no OCR) —
                    // then the pipeline walks the source itself.
                    None => None,
                };
                *session.burn_sets.lock().unwrap() = sets;
                *session.burn_pick.lock().unwrap() = new_pick;
                replan_subs = true;
            }
        }
        // Register the intent; a burst coalesces to the newest one.
        let my_gen = {
            let generation = session
                .seek_gen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut pending = session.pending_seek.lock().unwrap();
            let next = PendingSeek {
                generation,
                position_ms,
                audio_track,
                video_track,
                replan_subs,
            };
            *pending = Some(PendingSeek::merge(pending.take(), next));
            generation
        };
        let mut done = session.seek_done.subscribe();
        // Detached executor: an HTTP disconnect cancels the REQUEST
        // future, never the restart itself.
        {
            let (this, registry, session) = (self.clone(), registry.clone(), session.clone());
            tokio::spawn(async move {
                let _serialized = session.seek_lock.lock().await;
                // Take whatever intent is newest; None = a prior
                // executor already covered it (we were a burst).
                let Some(todo) = session.pending_seek.lock().unwrap().take() else {
                    return;
                };
                // Inner spawn: a PANIC in the restart still publishes —
                // an unpublished generation would hang every waiter.
                let (this2, registry2, session2) =
                    (this.clone(), registry.clone(), session.clone());
                let outcome = match tokio::spawn(async move {
                    this2.execute_seek(&registry2, &session2, todo).await
                })
                .await
                {
                    Ok(r) => r.map_err(|e| format!("{e:#}")),
                    Err(join) => Err(format!("seek restart panicked: {join}")),
                };
                let _ = session.seek_done.send((todo.generation, outcome));
            });
        }
        // Await the restart that covers this request: ours, or the
        // newer one that superseded it — either way the returned state
        // is what actually plays now. Bounded: a wedged restart turns
        // into an error, never a hung client request.
        let waited = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                {
                    let latest = done.borrow();
                    if latest.0 >= my_gen {
                        return latest.1.clone().map_err(|e| anyhow::anyhow!(e));
                    }
                }
                done.changed().await.context("session torn down mid-seek")?;
            }
        })
        .await;
        match waited {
            Ok(r) => r,
            Err(_) => bail!("seek restart did not settle within 60s"),
        }
    }

    async fn execute_seek(
        self: &Arc<Self>,
        registry: &Registry,
        session: &Arc<Session>,
        todo: PendingSeek,
    ) -> Result<u64> {
        let PendingSeek {
            position_ms,
            audio_track,
            video_track,
            replan_subs,
            ..
        } = todo;
        let mut plan =
            (*session.plan.lock().unwrap()).context("session has no restartable pipeline")?;
        session.touch();
        let want_audio = audio_track.map(|t| t as usize).unwrap_or(plan.audio_track);
        let want_video = video_track.map(|t| t as usize).unwrap_or(plan.video_track);
        if want_audio != plan.audio_track || want_video != plan.video_track || replan_subs {
            // Switching tracks re-plans: the new track's codec decides
            // copy vs encode, not the old one's — and a burn-pick
            // change re-plans even with the same tracks.
            let (_, _, _, _, info) =
                crate::subtitles::source_row(registry, &session.item_id).await?;
            // HUB-15a: the executor is already chosen here — ask IT.
            // Plain hub-local audio work is not a video executor.
            let tonemap = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry.transcoder_reports_tonemap(&tc)
                }
                _ if registry.local_video_executor_enabled() => {
                    kahawai_media::remux::tonemap_available()
                }
                _ => false,
            };
            // Mirrors the start path (connected counts: the mediahost
            // walks its own index); sets already fetched into scratch
            // make the burn unconditionally real — a re-plan must not
            // drop a burn whose data is in hand.
            let burn_capable = session.burn_sets.lock().unwrap().is_some()
                || session.parts.first().is_some_and(|p| {
                    registry.is_connected(&p.module_id) || self.reads_locally(&p.module_id)
                });
            // HUB-15b: re-plans may only pick targets the ALREADY
            // CHOSEN executor encodes — the session does not move boxes
            // on a track switch. A remote video session therefore uses
            // that same box even if the new plan becomes audio-only.
            let (video_targets, full_audio_targets, local_audio_targets) = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    let all = registry.transcoder_encoders(&tc);
                    let video = video_encoder_names(&all);
                    let audio = audio_encoder_names(&all);
                    (video, audio.clone(), audio)
                }
                _ => {
                    let video = if registry.local_video_executor_enabled() {
                        video_encoder_names(&local_encoder_names(registry))
                    } else {
                        Vec::new()
                    };
                    let audio = local_audio_encoder_names();
                    (video, audio.clone(), audio)
                }
            };
            let ocr_set = crate::subtitles::ocr_stream_set(registry.db(), &session.item_id).await;
            let ocr_flags = session
                .parts
                .first()
                .map(|p| {
                    crate::subtitles::ocr_flags_for(
                        &ocr_set,
                        &p.module_id,
                        &p.collection_id,
                        &p.root_token,
                        &p.path_rel,
                        info.subtitles.len(),
                    )
                })
                .unwrap_or_default();
            let executor_protocol = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry
                        .transcoder_protocol_features(&tc)
                        .unwrap_or_default()
                }
                _ => kahawai_proto::ProtocolFeatures::current(),
            };
            let force_measurement = if session.force_loudness && session.parts.len() == 1 {
                registry
                    .audio_loudness(session.parts[0].file_id, want_audio)
                    .await?
            } else {
                None
            };
            let negotiate = |force_audio_encode| {
                kahawai_media::negotiate::negotiate_for_executors(
                    &session.profile,
                    &info,
                    want_audio,
                    want_video,
                    session.parts.len() == 1,
                    None,
                    tonemap,
                    burn_capable,
                    &ocr_flags,
                    // The session's explicit burn keeps forcing across
                    // track switches — its sets are already in hand.
                    *session.burn_pick.lock().unwrap(),
                    // A seek cannot move boxes (HUB-15b), so the question is
                    // whether THIS executor burns ASS — not whether the
                    // fleet does.
                    &kahawai_media::negotiate::AssPolicy {
                        burn_capable: match &session.mode {
                            Mode::Transcode { transcoder } => {
                                let tc = transcoder.lock().unwrap().clone();
                                registry.transcoder_reports_ass_burn(&tc)
                            }
                            _ if registry.local_video_executor_enabled() => {
                                kahawai_media::remux::ass_burn_available()
                            }
                            _ => false,
                        },
                        ..session.ass.clone()
                    },
                    &video_targets,
                    &full_audio_targets,
                    &local_audio_targets,
                    force_audio_encode,
                )
            };
            let mut sp = negotiate(force_measurement.is_some());
            plan = sp.plan;
            fill_audio_loudness_gains(
                registry,
                &session.parts,
                &mut plan,
                session.loudness,
                force_measurement.clone(),
            )
            .await?;
            if loudness_protocol_feature(&plan)
                .is_some_and(|feature| !executor_protocol.supports(feature))
            {
                // A track switch cannot move the session to a newer worker.
                // Drop a force-only encode rather than paying for unity gain;
                // an already-required encode remains playable without the
                // optional normalization fields its worker cannot understand.
                if force_measurement.is_some() {
                    sp = negotiate(false);
                    plan = sp.plan;
                    fill_audio_loudness_gains(
                        registry,
                        &session.parts,
                        &mut plan,
                        session.loudness,
                        force_measurement,
                    )
                    .await?;
                }
                if loudness_protocol_feature(&plan)
                    .is_some_and(|feature| !executor_protocol.supports(feature))
                {
                    apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, None);
                }
            }
            let verdict = replanned_verdict(&plan, &sp.video_verdict, &sp.audio_verdict)?;
            let mut subs = sp.subtitles;
            fill_verdict_track_ids(registry, &session.parts, &mut subs).await;
            *session.verdict.lock().unwrap() = Some(verdict);
            *session.sub_verdicts.lock().unwrap() = subs;
            let burns_ass =
                plan.burn_ass.is_some() || session.burn_ass_text.lock().unwrap().is_some();
            let (needs, _) = placement_need(&plan, &info, &session.parts, burns_ass);
            let mut plan_slot = session.plan.lock().unwrap();
            let mut needs_slot = session.needs.lock().unwrap();
            *plan_slot = Some(plan);
            *needs_slot = needs;
        }
        // Map the absolute position onto the right part (single-part
        // sessions: part 0, local == absolute).
        let idx = part_index(&session.parts, position_ms);
        let part = session
            .parts
            .get(idx)
            .context("session has no parts")?
            .clone();
        let local_ms = position_ms.saturating_sub(part.base_ms);
        session
            .current_part
            .store(idx, std::sync::atomic::Ordering::SeqCst);
        match &session.mode {
            Mode::Remux { dir, runner } => {
                let old = std::mem::replace(&mut *runner.lock().unwrap(), RemuxRunner::Stopped);
                old.stop_and_wait().await;
                let _ = std::fs::remove_dir_all(dir);
                // The old worker's lease died with it; open a fresh one
                // on whichever part the target lands in.
                // A seek restarts in the target part and spans the rest
                // from there: concat cannot serve the seek itself (it
                // accepts one and then plays from zero — measured), so
                // the restart stays, but it only ever happens for a seek
                // now, never for a boundary.
                let sink = session.sink.lock().unwrap().clone();
                let burn_sets = session.burn_sets.lock().unwrap().clone();
                let burn_ass = session.burn_ass_text.lock().unwrap().clone();
                let tail = self.open_part_leases(registry, &session.parts, idx).await?;
                let fresh = match self
                    .start_remux(
                        &session.id,
                        plan,
                        // The session's own value, NOT a fresh
                        // computation: a seek re-plans, but the client
                        // keeps the playlist it already has and §6.2.1
                        // forbids the declaration moving under it.
                        session.target_duration_secs,
                        tail,
                        local_ms,
                        &sink,
                        burn_sets.as_deref(),
                        burn_ass.as_deref(),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(first)
                        if sink != "hlssink2"
                            && plan.segment_format == kahawai_media::remux::SegmentFormat::Ts =>
                    {
                        // The same TC-6 fallback the start path has: some
                        // content crashes hlssink3 on EVERY restart.
                        tracing::warn!(session = %session.id, error = format!("{first:#}"),
                            "seek restart failed; retrying with fallback sink");
                        let tail = self.open_part_leases(registry, &session.parts, idx).await?;
                        let r = self
                            .start_remux(
                                &session.id,
                                plan,
                                session.target_duration_secs,
                                tail,
                                local_ms,
                                "hlssink2",
                                burn_sets.as_deref(),
                                burn_ass.as_deref(),
                            )
                            .await
                            .with_context(|| format!("first attempt: {first:#}"))?;
                        *session.sink.lock().unwrap() = "hlssink2".into();
                        r
                    }
                    Err(e) => return Err(e),
                };
                let (fresh, facts) = fresh;
                fold_facts(&mut session.verdict.lock().unwrap(), &facts);
                *runner.lock().unwrap() = fresh;
                Ok(part.base_ms)
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let _ = registry
                    .send_to_tc(
                        &tc,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                kahawai_proto::v1::EndSession {
                                    session_id: session.id.clone(),
                                },
                            )),
                        },
                    )
                    .await;
                registry.tc_session_ended(&tc);
                // The hub-held lease survives restarts; reuse it while
                // the target stays inside the same part (works even when
                // the mediahost link is flapping). Crossing parts needs
                // a lease on the other file.
                let held = self.tc_leases.lock().unwrap().remove(&session.id);
                let parts = match held {
                    Some((parts, held_idx)) if held_idx == idx => parts,
                    _ => self.open_part_leases(registry, &session.parts, idx).await?,
                };
                let sink = session.sink.lock().unwrap().clone();
                // Read outside the call: a guard held across .await
                // would poison the future's Send-ness.
                let sets_bytes = session
                    .burn_sets
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|p| std::fs::read(p).unwrap_or_default())
                    .unwrap_or_default();
                let ass_bytes = session
                    .burn_ass_text
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
                    .into_bytes();
                if let Err(first) = self
                    .start_transcode(
                        registry,
                        &tc,
                        &session.id,
                        plan,
                        parts,
                        idx,
                        local_ms,
                        &sink,
                        sets_bytes.clone(),
                        ass_bytes.clone(),
                    )
                    .await
                {
                    if sink == "hlssink2"
                        || plan.segment_format != kahawai_media::remux::SegmentFormat::Ts
                    {
                        return Err(first);
                    }
                    tracing::warn!(session = %session.id, error = format!("{first:#}"),
                        "seek restart failed; retrying with fallback sink");
                    let parts = self.open_part_leases(registry, &session.parts, idx).await?;
                    self.start_transcode(
                        registry,
                        &tc,
                        &session.id,
                        plan,
                        parts,
                        idx,
                        local_ms,
                        "hlssink2",
                        sets_bytes,
                        ass_bytes,
                    )
                    .await
                    .with_context(|| format!("first attempt: {first:#}"))?;
                    *session.sink.lock().unwrap() = "hlssink2".into();
                }
                Ok(part.base_ms)
            }
            Mode::Direct { .. } => bail!("direct sessions seek with range requests"),
        }
    }

    /// A transcoder vanished (link loss): reschedule its sessions onto
    /// the remaining fleet at the viewer's last position (AR-6); end the
    /// ones nobody can take.
    pub async fn reschedule_for_transcoder(
        self: &Arc<Self>,
        registry: &Registry,
        transcoder: &str,
    ) -> (usize, usize) {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                matches!(&s.mode, Mode::Transcode { transcoder: t }
                    if *t.lock().unwrap() == transcoder)
            })
            .map(|s| s.id.clone())
            .collect();
        let (mut moved, mut ended) = (0, 0);
        for id in ids {
            match self.reschedule(registry, &id).await {
                Ok(new_tc) => {
                    tracing::info!(session = %id, from = transcoder, to = %new_tc, "session rescheduled");
                    moved += 1;
                }
                Err(e) => {
                    tracing::warn!(session = %id, error = format!("{e:#}"), "reschedule failed; ending");
                    self.end(&id).await;
                    ended += 1;
                }
            }
        }
        (moved, ended)
    }

    /// Re-dispatch one session (its transcoder died or its worker
    /// crashed) at the viewer's last reported position.
    /// The reservation `reserve_transcoder` takes below outlives
    /// several fallible steps — leases, a watch-state read — so the
    /// body is wrapped and the slot returned on any failure. A leaked
    /// one is invisible: the box simply looks busier than it is until
    /// the hub restarts, and two of them retire a `max_sessions = 2`
    /// box from the fleet.
    pub async fn reschedule(self: &Arc<Self>, registry: &Registry, id: &str) -> Result<String> {
        // Holds the reservation until the new box is actually running;
        // cleared on success so only failures give it back.
        let mut reserved: Option<String> = None;
        let out = self.reschedule_inner(registry, id, &mut reserved).await;
        if out.is_err()
            && let Some(tc) = reserved
        {
            registry.tc_session_ended(&tc);
        }
        out
    }

    async fn reschedule_inner(
        self: &Arc<Self>,
        registry: &Registry,
        id: &str,
        reserved: &mut Option<String>,
    ) -> Result<String> {
        let session = self.get(id).context("no such session")?;
        let (plan, needs) = {
            let plan_slot = session.plan.lock().unwrap();
            let plan = (*plan_slot).context("not a pipeline session")?;
            let needs = session.needs.lock().unwrap().clone();
            (plan, needs)
        };
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a dispatched session");
        };
        let old_tc = transcoder.lock().unwrap().clone();
        registry.tc_session_ended(&old_tc);
        let new_tc = registry
            .reserve_transcoder(&needs)
            .context("no capable transcoder left")?;
        *reserved = Some(new_tc.clone());
        // Resume where the viewer was: the player posts progress every
        // 10 s, which is exactly the doc's start_offset for AR-6.
        let position_ms: i64 = sqlx::query_scalar(
            "SELECT position_ms FROM watch_state WHERE user_id = ? AND item_id = ?",
        )
        .bind(&session.user_id)
        .bind(&session.item_id)
        .fetch_optional(registry.db())
        .await?
        .unwrap_or(0);
        let idx = part_index(&session.parts, position_ms.max(0) as u64);
        let part = session
            .parts
            .get(idx)
            .context("session has no parts")?
            .clone();
        let local_ms = (position_ms.max(0) as u64).saturating_sub(part.base_ms);
        session
            .current_part
            .store(idx, std::sync::atomic::Ordering::SeqCst);
        // Reuse the hub-held lease when the position is still in its
        // part — the mediahost may be unreachable during a fleet blip.
        let held = self.tc_leases.lock().unwrap().remove(id);
        let parts = match held {
            Some((parts, held_idx)) if held_idx == idx => parts,
            _ => self.open_part_leases(registry, &session.parts, idx).await?,
        };
        let sets = session
            .burn_sets
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| std::fs::read(p).unwrap_or_default())
            .unwrap_or_default();
        let ass = session
            .burn_ass_text
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default()
            .into_bytes();
        self.start_transcode(
            registry, &new_tc, id, plan, parts, idx, local_ms, "", sets, ass,
        )
        .await?;
        *transcoder.lock().unwrap() = new_tc.clone();
        *reserved = None; // running now; the session owns the slot
        Ok(new_tc)
    }

    /// Active sessions for the admin dashboard (HUB-18).
    pub fn list(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self.active.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// End every session backed by a given mediahost (satellite deletion).
    /// End every session belonging to one user — what deleting an
    /// account has to do before the account is gone, or the sessions
    /// outlive it with no owner to stop them.
    pub async fn end_for_user(&self, user_id: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.user_id == user_id)
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id).await;
        }
        n
    }

    /// End every session that reads from this mediahost.
    ///
    /// Any part, not just the one it started on. `Session::module_id` is
    /// `parts[start_idx].module_id`, and a multi-part source can have a host
    /// per part — a CD1/CD2 item whose discs sit on different mediahosts is
    /// the ordinary case, not a contrived one. Keyed on the starting part
    /// alone, a session playing part two survived part two's host going
    /// away, which is exactly the stall AR-6 exists to prevent.
    ///
    /// Deliberately generous in the other direction: a session that has
    /// moved past a part still counts as reading from its host, because the
    /// viewer can seek back into it. Ending the session is the honest answer
    /// there — the alternative is a seek that fails later with no warning.
    pub async fn end_for_module(&self, module_id: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| reads_from(&s.module_id, &s.parts, module_id))
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id).await;
        }
        n
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.active.lock().unwrap().get(id).cloned()
    }

    /// HUB-36: the kind of work a session is, for attributing a pace
    /// sample. None when the session has ended (its sample arrived on
    /// the next heartbeat and lost the race) or when it never encoded
    /// video and so has nothing to teach.
    pub fn pace_class(&self, id: &str) -> Option<String> {
        let s = self.active.lock().unwrap().get(id).cloned()?;
        (!s.pace_class.is_empty()).then(|| s.pace_class.clone())
    }

    /// Remove a session: direct leases drop (closing the byte channel);
    /// remux pipelines stop and their scratch dir is deleted.
    pub async fn end(&self, id: &str) -> bool {
        // OPS-10: while the session still exists. Everything below this
        // line has already forgotten it.
        let header = self.log_header(id);
        let Some(session) = self.active.lock().unwrap().remove(id) else {
            return false;
        };
        // A progress handler that already found this session finishes its DB
        // write before teardown reads the result. One that arrives after the
        // removal either fails its lookup or sees `ending` and writes nothing.
        let mut ending = session.ending.write().await;
        *ending = true;
        let earned_a_play = session.earned_a_play();
        drop(ending);
        {
            let mut kept = self.known_sessions.lock().unwrap();
            // Bounded like the bundles themselves: a header is only
            // useful until its bundle lands, moments later.
            if kept.len() > 64 {
                kept.clear();
            }
            kept.insert(id.to_string(), header);
        }
        match &session.mode {
            Mode::Remux { dir, runner } => {
                // OPS-10: the hub's OWN worker leaves the same evidence a
                // satellite's does, and this wipe destroys it. Gather
                // first, and store directly — a local session never
                // touches the link. Teardown only: a seek-restart also
                // wipes this dir, but a bundle per scrub is noise.
                if let Some(data_dir) = self.scratch_root.parent() {
                    let (item, header) = self.log_header(id);
                    let body = format!("{header}{}", local_bundle(dir));
                    crate::sessionlog::store(data_dir, &item, id, &body);
                }
                runner.lock().unwrap().stop();
                let _ = std::fs::remove_dir_all(dir);
            }
            Mode::Transcode { transcoder } => {
                let transcoder = transcoder.lock().unwrap().clone();
                self.tc_leases.lock().unwrap().remove(id);
                self.pending_ready.lock().unwrap().remove(id);
                if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
                    registry.tc_session_ended(&transcoder);
                    let sid = id.to_string();
                    tokio::spawn(async move {
                        let _ = registry
                            .send_to_tc(
                                &transcoder,
                                kahawai_proto::v1::HubToTc {
                                    msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                        kahawai_proto::v1::EndSession { session_id: sid },
                                    )),
                                },
                            )
                            .await;
                    });
                }
            }
            Mode::Direct { .. } => {}
        }
        // HUB-10: a play is counted here and nowhere else on the playback
        // path, because stopping is the only moment a watch is over.
        // Counting the 90-percent crossing instead counted a second play
        // for anyone who scrubbed back over the line and forward again,
        // and counted one for a viewer who then abandoned the thing at
        // half way. Whichever session took the item past the line is the
        // one that counts, so a sitting split across two of them by a
        // reaped pause or a dead transcoder is still one play.
        if earned_a_play {
            let registry = { self.registry_for_teardown.lock().unwrap().clone() };
            match registry {
                Some(registry) => {
                    if let Err(e) = sqlx::query(
                        "UPDATE watch_state SET play_count = play_count + 1
                          WHERE user_id = ? AND item_id = ?",
                    )
                    .bind(&session.user_id)
                    .bind(&session.item_id)
                    .execute(registry.db())
                    .await
                    {
                        tracing::warn!(
                            session = id,
                            item = %session.item_id,
                            error = %e,
                            "finished watch not counted"
                        );
                    }
                }
                // Only reachable in an embedding that never called
                // `attach_registry` — `api::router` does. Said out loud
                // rather than dropped, because a play that silently does
                // not count is invisible until someone adds up a year of
                // them.
                None => tracing::warn!(
                    session = id,
                    item = %session.item_id,
                    "no registry attached; finished watch not counted"
                ),
            }
        }
        // The event is emitted only after the watch-state write settles. A
        // client reacting to `sessions` must not refetch the item in the gap
        // and observe the old play count. Callers that are about to archive or
        // delete watch state likewise await this method before proceeding.
        if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
            registry.emit(crate::registry::RegistryEvent::Sessions { kind: "sessions" });
        }
        tracing::info!(session = id, "session ended");
        true
    }
}

/// Does this session read bytes from `module_id`?
///
/// Any part, not just the one it started on. `Session::module_id` is
/// `parts[start_idx].module_id`, and a multi-part source can have a host per
/// part — CD1 and CD2 on different mediahosts is ordinary. Keyed on the
/// starting part alone, a session playing CD2 survived CD2's host leaving.
///
/// True for a part already played, on purpose: the viewer can seek back into
/// it, so the session does still depend on that host.
///
/// A free function so the predicate can be tested without standing up a
/// session: a real multi-part one is remux-only, which means GStreamer and
/// real media to assert one boolean.
fn reads_from(start_host: &str, parts: &[PartSource], module_id: &str) -> bool {
    start_host == module_id || parts.iter().any(|p| p.module_id == module_id)
}

#[cfg(test)]
mod reads_from_tests {
    use super::{PartSource, reads_from};

    fn part(host: &str) -> PartSource {
        PartSource {
            file_id: 0,
            module_id: host.into(),
            collection_id: "movies".into(),
            root_token: "root".into(),
            path_rel: "x.mkv".into(),
            size: 1,
            mtime_unix: 0,
            base_ms: 0,
            duration_ms: 1,
        }
    }

    #[test]
    fn a_session_reads_from_every_host_its_parts_live_on() {
        // CD1 on A, CD2 on B, started on CD1.
        let parts = [part("A"), part("B")];

        assert!(reads_from("A", &parts, "A"), "the host it started on");
        // The one that was missed: B going away used to leave this session
        // alive on a dead lease, which is the stall AR-6 exists to prevent.
        assert!(reads_from("A", &parts, "B"), "a later part's host");
        assert!(!reads_from("A", &parts, "C"), "a host it never reads from");
    }

    #[test]
    fn a_part_already_passed_still_counts() {
        // Deliberate, and the reason the doc no longer claims to have fixed
        // an "over-match": the position is not the whole story, because the
        // viewer can seek back into CD1 whenever they like. Ending the
        // session when its host leaves is the honest answer; the alternative
        // is a seek that fails minutes later with nothing to explain it.
        //
        // Asked from the far side, because `reads_from` has no position to
        // pass: a session STARTED on CD2 still reads from CD1's host. The
        // version of this test that asked `reads_from("A", &parts, "A")` was
        // the first assertion of the test above it word for word, and named a
        // property this signature cannot express — an implementation that
        // scanned only the parts from `start_idx` onwards, which is exactly
        // the "already played does not count" mistake, passed it.
        let parts = [part("A"), part("B")];
        assert!(
            reads_from("B", &parts, "A"),
            "a session playing CD2 still depends on CD1's host: the viewer \
             can seek back into it"
        );
    }

    #[test]
    fn a_single_part_session_is_unchanged() {
        let parts = [part("A")];
        assert!(reads_from("A", &parts, "A"));
        assert!(!reads_from("A", &parts, "B"));
    }
}

#[cfg(test)]
mod lease_purpose_tests {
    use super::{Reader, Sessions};

    /// The mediahost schedules its own local work — hashes, declarations,
    /// probes, extractions — around whether it is serving somebody. It
    /// cannot tell a sweep from a viewer by looking at the bytes, so the
    /// lease has to say, and this is the only place that says it.
    async fn opened_as(reader: Reader) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(db, Default::default());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        registry.register_link(
            "01MH",
            tx,
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );

        let sessions = std::sync::Arc::new(Sessions::new(dir.path().join("sessions")));
        // Nobody answers the OpenRead, so the lease never establishes; the
        // message is on the wire either way, which is the whole subject.
        let opening = tokio::spawn(async move {
            let _ = sessions
                .open_lease(&registry, "01MH", "c", "r", "e.mkv", reader)
                .await;
        });
        let sent = rx.recv().await.expect("an OpenRead reaches the host");
        opening.abort();
        match sent.unwrap().msg {
            Some(kahawai_proto::v1::hub_to_host::Msg::OpenRead(open)) => open.background,
            other => panic!("expected an OpenRead, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sweeps_lease_says_so() {
        assert!(opened_as(Reader::Sweep).await);
    }

    #[tokio::test]
    async fn and_a_viewers_does_not() {
        // The default reading of a missing field, so a hub too old to say
        // is taken as a viewer — the safe way round.
        assert!(!opened_as(Reader::Viewer).await);
    }

    #[tokio::test]
    async fn a_local_lease_holds_mediahost_activity_until_drop() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("episode.mkv");
        std::fs::write(&source, b"bytes").unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(db, Default::default());
        let sessions = Sessions::new(dir.path().join("sessions"));
        sessions.set_local_source("local", move |_, _, _| Ok(source.clone()));
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entered = active.clone();
        sessions.set_local_activity(move |_| {
            entered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(Guard(entered.clone()))
        });

        let lease = sessions
            .open_lease(&registry, "local", "c", "r", "episode.mkv", Reader::Viewer)
            .await
            .unwrap();
        assert_eq!(active.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(lease);
        assert_eq!(active.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_local_sweep_is_admitted_before_path_resolution() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("episode.mkv");
        std::fs::write(&source, b"bytes").unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(db, Default::default());
        let sessions = Sessions::new(dir.path().join("sessions"));
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolving = active.clone();
        sessions.set_local_source("local", move |_, _, _| {
            assert_eq!(resolving.load(std::sync::atomic::Ordering::Relaxed), 1);
            Ok(source.clone())
        });
        let admitted = active.clone();
        sessions.set_local_background(move |_| {
            let admitted = admitted.clone();
            std::sync::Arc::new(move || {
                let admitted = admitted.clone();
                Box::pin(async move {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(Box::new(Guard(admitted)) as Box<dyn Send + Sync>)
                })
            })
        });

        let lease = sessions
            .open_lease(&registry, "local", "c", "r", "episode.mkv", Reader::Sweep)
            .await
            .unwrap();
        drop(lease);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LoudnessPreference, Negotiation, PartSource, Sessions, apply_audio_loudness_measurement,
        fold_facts, local_encoder_names, local_tonemap_available, part_index, replanned_verdict,
        same_video_path, source_choice_key, wire_scalar_loudness,
    };

    /// The per-user cap has to hold when starts ARRIVE TOGETHER, which
    /// is the only time it matters. It used to count `active`, drop the
    /// lock, and let the session land there some five hundred lines and
    /// sixteen awaits later — so concurrent callers all read the same
    /// stale count. Measured against the live hub before this fix: 20
    /// concurrent starts admitted against a cap of 4.
    ///
    /// Admission is tested directly rather than through `start`, which
    /// would need a registry, a mediahost and a real file to reach the
    /// same decision.
    #[test]
    fn the_per_user_cap_holds_when_starts_arrive_together() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = std::sync::Arc::new(Sessions::with_limits(
            dir.path().join("sessions"),
            4,
            std::time::Duration::from_secs(90),
        ));

        // Which ids won is a race; the test must not assume.
        let admitted: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(20));
        let mut threads = Vec::new();
        for i in 0..20 {
            let (sessions, admitted, barrier) =
                (sessions.clone(), admitted.clone(), barrier.clone());
            threads.push(std::thread::spawn(move || {
                let id = format!("s{i}");
                barrier.wait(); // all twenty push on the door at once
                if sessions.admit(&id, "u1").is_ok() {
                    admitted.lock().unwrap().push(id);
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let admitted = admitted.lock().unwrap().clone();
        assert_eq!(admitted.len(), 4, "the cap admitted more than it allows");

        // Another user is unaffected: the cap is per user, not global.
        assert!(sessions.admit("other", "u2").is_ok());

        // Releasing frees exactly one slot, and no more.
        sessions.release(&admitted[0]);
        assert!(sessions.admit("again", "u1").is_ok());
        assert!(sessions.admit("once-more", "u1").is_err());
    }

    #[tokio::test]
    async fn local_serving_capabilities_require_successful_benchmarks() {
        let available = kahawai_media::remux::encoder_capabilities();
        let Some((codec, element, _)) = available.first().copied() else {
            eprintln!("skip: no verified encoder on this test host");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(db, Default::default());
        assert!(
            local_encoder_names(&registry).is_empty(),
            "unmeasured local encoder was offered"
        );

        let cache = dir.path().join("benchmarks.json");
        let mut measured = kahawai_media::bench::BenchResults {
            gst: kahawai_media::bench::gst_version(),
            tonemap: Some(kahawai_media::bench::Speeds {
                s1080: Some(2.0),
                s2160: Some(0.5),
            }),
            ..Default::default()
        };
        measured.encoders.insert(
            element.into(),
            kahawai_media::bench::Speeds {
                s1080: Some(3.0),
                s2160: Some(0.8),
            },
        );
        kahawai_media::bench::store(&cache, &measured);
        registry.set_local_bench(measured);
        assert_eq!(local_encoder_names(&registry), [codec]);
        assert_eq!(
            local_tonemap_available(&registry),
            kahawai_media::remux::tonemap_available()
        );

        kahawai_media::bench::record_crash(
            &cache,
            &kahawai_media::bench::BenchmarkJob::Encoder(element.into()),
        );
        registry.set_local_bench(kahawai_media::bench::load(&cache).unwrap());
        assert!(
            !local_encoder_names(&registry)
                .iter()
                .any(|candidate| candidate == codec),
            "quarantined local encoder remained a serving capability"
        );

        kahawai_media::bench::record_crash(&cache, &kahawai_media::bench::BenchmarkJob::ToneMap);
        registry.set_local_bench(kahawai_media::bench::load(&cache).unwrap());
        assert!(!local_tonemap_available(&registry));
    }

    /// Facts amend the verdict by kind, exactly once — a seek-restart
    /// re-learns the same fold and must not stutter it — and unknown
    /// kinds change nothing.
    #[test]
    fn facts_fold_into_the_verdict_idempotently() {
        let fact = |kind: &str, detail: &str| kahawai_media::facts::Fact {
            kind: kind.into(),
            detail: detail.into(),
        };
        let mut verdict = Some((
            "hevc → h264 (transcoded)".to_string(),
            "dts → aac (transcoded)".to_string(),
        ));
        let facts = vec![
            fact("audio", "7.1 → 5.1"),
            fact("video", "tone-map"),
            fact("weird", "x"),
        ];
        fold_facts(&mut verdict, &facts);
        fold_facts(&mut verdict, &facts); // the seek-restart
        let (video, audio) = verdict.unwrap();
        assert_eq!(audio, "dts → aac (transcoded) · 7.1 → 5.1");
        assert_eq!(video, "hevc → h264 (transcoded) · tone-map");

        // No verdict (direct play) — nothing to amend, no panic.
        let mut none = None;
        fold_facts(&mut none, &facts);
        assert_eq!(none, None);
    }

    #[test]
    fn loudness_opt_out_clears_every_gain() {
        let mut plan = kahawai_media::remux::RemuxPlan::default();
        let measured = kahawai_media::loudness::AudioLoudnessMeasurement {
            source: kahawai_media::loudness::AudioLayout::new(6, 0x3f),
            layouts: vec![
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(6, 0x3f),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -24.0,
                        true_peak_dbtp: -10.0,
                    },
                },
                kahawai_media::loudness::AudioLayoutLoudness {
                    layout: kahawai_media::loudness::AudioLayout::new(2, 0x3),
                    loudness: kahawai_media::loudness::AudioLoudness {
                        integrated_lufs: -26.0,
                        true_peak_dbtp: -8.0,
                    },
                },
            ],
        };
        apply_audio_loudness_measurement(
            &mut plan,
            LoudnessPreference::Encoded,
            Some(measured.clone()),
        );
        assert_eq!(plan.stereo_gain_db, Some(7.0));
        assert_eq!(plan.native_gain_db, Some(6.0));
        assert_eq!(plan.loudness_source_channels, Some(6));
        assert_eq!(
            plan.loudness_gains.iter().flatten().count(),
            2,
            "every exact layout gets a gain"
        );
        apply_audio_loudness_measurement(&mut plan, LoudnessPreference::Off, Some(measured));
        assert_eq!(plan.stereo_gain_db, None);
        assert_eq!(plan.native_gain_db, None);
        assert_eq!(plan.loudness_source_channels, None);
        assert!(plan.loudness_gains.iter().all(Option::is_none));
    }
    #[test]
    fn absent_scalar_gain_is_nonfinite_across_old_worker_decoders() {
        let plan = kahawai_media::remux::RemuxPlan::default();
        let (stereo, native, channels) = wire_scalar_loudness(&plan);
        assert!(stereo.is_some_and(f64::is_nan));
        assert!(native.is_some_and(f64::is_nan));
        assert_eq!(channels, Some(0));
    }

    #[test]
    fn optional_audio_gain_never_changes_the_video_path() {
        let base = kahawai_media::remux::RemuxPlan {
            video: kahawai_media::remux::StreamMode::Encode,
            tone_map: true,
            ..Default::default()
        };
        let mut gain_only = base;
        gain_only.stereo_gain_db = Some(3.0);
        assert!(same_video_path(&base, &gain_only));

        let mut different_codec = base;
        different_codec.video_codec = kahawai_media::remux::VideoTarget::Hevc;
        different_codec.segment_format = kahawai_media::remux::SegmentFormat::Fmp4;
        assert!(!same_video_path(&base, &different_codec));

        let mut missing_tonemap = base;
        missing_tonemap.tone_map = false;
        assert!(!same_video_path(&base, &missing_tonemap));
    }

    #[test]
    fn a_track_replan_refuses_empty_output_and_refreshes_its_verdict() {
        let empty = kahawai_media::remux::RemuxPlan::default();
        assert!(replanned_verdict(&empty, "new video verdict", "new audio verdict").is_err());

        let mut playable = empty;
        playable.audio = kahawai_media::remux::StreamMode::Copy;
        assert_eq!(
            replanned_verdict(&playable, "new video verdict", "new audio verdict").unwrap(),
            ("new video verdict".into(), "new audio verdict".into())
        );
    }

    #[tokio::test]
    async fn forced_video_encode_uses_protocol_four_baseline_layout_gains() {
        use kahawai_core::media::{AudioStream, MediaInfo, VideoStream};
        use kahawai_proto::v1::{CapabilityReport, EncoderCap};

        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry =
            crate::registry::Registry::new(db, Default::default()).with_local_video_executor(false);
        let connect = |id: &str, minor: u32, hardware: bool| {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            std::mem::forget(rx);
            registry.connected(id, "transcoder", id, "fp", "test");
            registry.register_tc_link(id, minor, tx.clone());
            registry.set_transcoder_caps(
                id,
                &CapabilityReport {
                    encoders: vec![
                        EncoderCap {
                            codec: "h264".into(),
                            element: if hardware { "nvh264enc" } else { "x264enc" }.into(),
                            hardware,
                            speed_1080: Some(if hardware { 9.0 } else { 2.0 }),
                            speed_2160: Some(if hardware { 3.0 } else { 0.7 }),
                        },
                        EncoderCap {
                            codec: "aac".into(),
                            element: "avenc_aac".into(),
                            hardware: false,
                            speed_1080: None,
                            speed_2160: None,
                        },
                    ],
                    max_sessions: 2,
                    decode_caps: vec!["video/x-h265".into(), "audio/mpeg".into()],
                    ..Default::default()
                },
            );
            tx
        };
        let baseline_fast = connect("baseline-fast", 0, true);
        let _baseline_slow = connect("baseline-slow", 0, false);

        let sessions = Sessions::new(dir.path().join("sessions"));
        let negotiation = Negotiation::new(
            &sessions,
            &registry,
            "user",
            "item",
            Some(Default::default()),
            0,
            0,
            None,
        )
        .await
        .unwrap();
        let info = MediaInfo {
            container: Some("matroska".into()),
            duration_ms: Some(60_000),
            video: vec![VideoStream {
                codec: "hevc".into(),
                width: 1920,
                height: 1080,
                ..Default::default()
            }],
            audio: vec![AudioStream {
                codec: "aac".into(),
                channels: 8,
                sample_rate: 48_000,
                ..Default::default()
            }],
            ..Default::default()
        };
        let parts = [PartSource {
            file_id: 1,
            module_id: "mediahost".into(),
            collection_id: "movies".into(),
            root_token: "root".into(),
            path_rel: "movie.mkv".into(),
            size: 1_000_000,
            mtime_unix: 1,
            base_ms: 0,
            duration_ms: 60_000,
        }];

        let measurement = kahawai_media::loudness::AudioLoudnessMeasurement {
            source: kahawai_media::loudness::AudioLayout::new(8, 0xc3f),
            layouts: vec![(8, 0xc3f), (6, 0x3f), (2, 0x3)]
                .into_iter()
                .map(
                    |(channels, channel_mask)| kahawai_media::loudness::AudioLayoutLoudness {
                        layout: kahawai_media::loudness::AudioLayout::new(channels, channel_mask),
                        loudness: kahawai_media::loudness::AudioLoudness {
                            integrated_lufs: -24.0,
                            true_peak_dbtp: -8.0,
                        },
                    },
                )
                .collect(),
        };

        assert!(
            negotiation
                .probe(&info, false, None)
                .full_protocol
                .supports(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
            "protocol 4.0 did not expose inherited exact layout gains"
        );
        assert!(
            negotiation
                .probe(
                    &info,
                    false,
                    Some(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
                )
                .full_protocol
                .supports(kahawai_proto::ProtocolFeature::ExactAudioLoudnessGains),
            "the exact probe lost a protocol-4 baseline feature"
        );
        let normal = negotiation.plan_with_probe(&parts, &info, false, None);
        assert_eq!(normal.plan.video, kahawai_media::remux::StreamMode::Encode);
        assert_ne!(normal.plan.audio, kahawai_media::remux::StreamMode::Encode);
        let forced = negotiation.plan_with_probe(&parts, &info, false, Some(&measurement));
        assert_eq!(forced.plan.video, kahawai_media::remux::StreamMode::Encode);
        assert_eq!(
            forced.plan.audio,
            kahawai_media::remux::StreamMode::Encode,
            "force normalization was suppressed at the protocol-4 baseline"
        );

        let mut direct_info = info.clone();
        direct_info.container = Some("mp4".into());
        direct_info.video[0].codec = "h264".into();
        let direct = negotiation.plan_with_probe(&parts, &direct_info, false, None);
        assert_eq!(direct.cost, kahawai_media::negotiate::Cost::Direct);
        assert!(
            source_choice_key(&direct, true) < source_choice_key(&normal, false),
            "measured video transcode outranked unmeasured direct play"
        );
        assert!(
            source_choice_key(&direct, false) < source_choice_key(&direct, true),
            "force capability did not break an ordinary-cost tie"
        );

        assert!(registry.unregister_tc_link_if_current("baseline-fast", &baseline_fast));
        let fallback = negotiation.plan_with_probe(&parts, &info, false, Some(&measurement));
        assert_eq!(
            fallback.plan.audio,
            kahawai_media::remux::StreamMode::Encode,
            "another protocol-4 baseline worker lost exact layout gains"
        );

        let mut stereo_info = info.clone();
        stereo_info.audio[0].channels = 2;
        let stereo = kahawai_media::loudness::AudioLoudnessMeasurement {
            source: kahawai_media::loudness::AudioLayout::new(2, 0x3),
            layouts: vec![kahawai_media::loudness::AudioLayoutLoudness {
                layout: kahawai_media::loudness::AudioLayout::new(2, 0x3),
                loudness: kahawai_media::loudness::AudioLoudness {
                    integrated_lufs: -24.0,
                    true_peak_dbtp: -8.0,
                },
            }],
        };
        let stereo_normal = negotiation.plan_with_probe(&parts, &stereo_info, false, None);
        let baseline = negotiation.plan_with_probe(&parts, &stereo_info, false, Some(&stereo));
        assert_eq!(
            baseline.plan.audio,
            kahawai_media::remux::StreamMode::Encode,
            "protocol 4.0 did not expose exact stereo gain support"
        );
        assert_ne!(baseline.plan.audio, stereo_normal.plan.audio);
    }

    fn part(base_ms: u64, duration_ms: u64) -> PartSource {
        PartSource {
            file_id: 0,
            module_id: "m".into(),
            collection_id: "c".into(),
            root_token: "root".into(),
            path_rel: format!("CD{}.avi", base_ms),
            size: 1,
            mtime_unix: 0,
            base_ms,
            duration_ms,
        }
    }

    /// The CD1→CD2 hand-off is this function and nothing else: when a
    /// part's playlist ends the client seeks to `end + 250 ms`, and which
    /// file that lands in is decided here. Boundaries measured against
    /// the real two-part rip in the library (part 2 based at 3_752_711).
    #[test]
    fn a_timestamp_lands_in_the_part_that_contains_it() {
        let parts = [part(0, 3_752_711), part(3_752_711, 3_758_967)];

        assert_eq!(part_index(&parts, 0), 0, "start of part one");
        assert_eq!(part_index(&parts, 3_752_710), 0, "last ms of part one");
        // base_ms is inclusive: the boundary itself is already part two,
        // which is why the client's `+250` cannot land back in part one.
        assert_eq!(part_index(&parts, 3_752_711), 1, "the boundary");
        assert_eq!(part_index(&parts, 3_752_961), 1, "end-of-part-one + 250");
        assert_eq!(part_index(&parts, 7_511_677), 1, "last ms of the film");
        // Past the end clamps to the final part rather than panicking:
        // a seek beyond the timeline is a UI rounding error, not a crash.
        assert_eq!(part_index(&parts, u64::MAX), 1, "past the end");

        // Single-file sources take the same path with one part.
        assert_eq!(part_index(&[part(0, 1_000)], 999), 0);
        assert_eq!(part_index(&[part(0, 1_000)], 10_000), 0);
        // No parts at all is not reachable today, but must not panic.
        assert_eq!(part_index(&[], 42), 0);
    }
}

#[cfg(test)]
mod seek_merge_tests {
    use super::PendingSeek;

    /// A scrub must not silently discard a queued track switch: the
    /// newest position wins, explicit track choices carry forward.
    #[test]
    fn scrub_keeps_the_queued_track_switch() {
        let switch = PendingSeek {
            replan_subs: false,
            generation: 1,
            position_ms: 1000,
            audio_track: Some(1),
            video_track: None,
        };
        let scrub = PendingSeek {
            replan_subs: false,
            generation: 2,
            position_ms: 9000,
            audio_track: None,
            video_track: None,
        };
        let merged = PendingSeek::merge(Some(switch), scrub);
        assert_eq!(merged.generation, 2);
        assert_eq!(merged.position_ms, 9000, "newest position wins");
        assert_eq!(merged.audio_track, Some(1), "track choice survives");
        // And an explicit newer choice overrides an older one.
        let re_switch = PendingSeek {
            replan_subs: false,
            generation: 3,
            position_ms: 9000,
            audio_track: Some(0),
            video_track: None,
        };
        assert_eq!(
            PendingSeek::merge(Some(merged), re_switch).audio_track,
            Some(0)
        );
    }
}

/// The hub-local worker's half of a session bundle (OPS-10). The
/// satellite's equivalent lives in kahawai-transcoder; this one is
/// deliberately separate rather than shared, because the two read
/// different directory layouts and sharing would mean a crate
/// dependency purely for a string builder.
fn local_bundle(dir: &std::path::Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "== hub-local worker\nrun dir: {}", dir.display());
    let segs = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("segment"))
                .count()
        })
        .unwrap_or(0);
    let _ = writeln!(out, "segments: {segs}");
    for name in ["start.pos", "viewer.pos", "pace.json", "facts.jsonl"] {
        if let Ok(body) = std::fs::read_to_string(dir.join(name)) {
            let _ = writeln!(out, "\n== {name}\n{}", body.trim_end());
        }
    }
    if let Ok(log) = std::fs::read_to_string(dir.join("worker.log")) {
        let _ = writeln!(out, "\n== worker.log\n{log}");
    }
    out
}
