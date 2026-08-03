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

use crate::leases::{Lease, Leases, new_lease_token};
use crate::registry::Registry;

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
    pub module_id: String,
    pub collection_id: String,
    pub path_rel: String,
    pub size: u64,
    pub base_ms: u64,
    pub duration_ms: u64,
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
    /// Placement requirements — reused when rescheduling (AR-6).
    needs: crate::registry::PlacementNeed,
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
}

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
    /// HUB-15a: that box reports the GL tone-map segment.
    pub tonemap: bool,
    /// HUB-15b: its verified encoder codec names — negotiation's target
    /// pool, intersected with what the client accepts.
    pub targets: Vec<String>,
    /// HUB-32b: this source's display-set timeline is readable where
    /// the encode would run.
    pub burn_capable: bool,
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
    /// HUB-32a/d: the user's ASS ladder. `pub` because the overlay rung
    /// only becomes real once something has been rasterised, and the
    /// caller flips `overlay_ready` before re-planning.
    pub ass: kahawai_media::negotiate::AssPolicy,
    /// HUB-32c: image tracks that already have OCR text derived from
    /// them, keyed per source.
    ocr_set: std::collections::HashSet<(String, String, String, i64)>,
    /// An explicit burn pick, if the caller named one and it is a track
    /// some tier could actually burn.
    burn_row: Option<crate::tracks::Track>,
    audio_track: u32,
    video_track: u32,
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
        // HUB-32a: burn ASS or flatten it? A fleet fact and this user's
        // standing choice. Fleet-wide rather than per-box on purpose —
        // the tier is decided before placement, which then hard-filters
        // on it (registry::PlacementNeed::needs_ass_burn).
        let ass = crate::tracks::ass_policy_for_user(
            registry.db(),
            user_id,
            registry.any_transcoder_ass_burn() || kahawai_media::remux::ass_burn_available(),
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
                let t = crate::tracks::get(registry.db(), tid)
                    .await?
                    .with_context(|| format!("no subtitle track {tid}"))?;
                anyhow::ensure!(
                    t.item_id == item_id,
                    "subtitle track {tid} belongs to another item"
                );
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
            ass,
            ocr_set,
            burn_row,
            audio_track,
            video_track,
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
            t.path_rel.as_deref(),
        ) != (
            Some(p.module_id.as_str()),
            Some(p.collection_id.as_str()),
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
        let (tonemap, targets) = match self.registry.pick_transcoder(&need) {
            Some(tc) => (
                self.registry.transcoder_reports_tonemap(&tc),
                self.registry.transcoder_encoders(&tc),
            ),
            None => (
                kahawai_media::remux::tonemap_available(),
                local_encoder_names(),
            ),
        };
        ExecutorFacts {
            tonemap,
            targets,
            burn_capable,
        }
    }

    /// The plan for one source against one executor's capabilities.
    pub(crate) fn plan(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        facts: &ExecutorFacts,
    ) -> kahawai_media::negotiate::SourcePlan {
        let est_kbps = info
            .duration_ms
            .filter(|d| *d > 0)
            .map(|d| ((parts.iter().map(|p| p.size).sum::<u64>() * 8) / d) as u32);
        kahawai_media::negotiate::negotiate(
            &self.profile,
            info,
            self.audio_track as usize,
            self.video_track as usize,
            parts.len() == 1,
            est_kbps,
            facts.tonemap,
            facts.burn_capable,
            &parts
                .first()
                .map(|p| {
                    crate::subtitles::ocr_flags_for(
                        &self.ocr_set,
                        &p.module_id,
                        &p.collection_id,
                        &p.path_rel,
                        info.subtitles.len(),
                    )
                })
                .unwrap_or_default(),
            self.pick_for(parts),
            &self.ass,
            &facts.targets,
        )
    }

    /// Probe the fleet, then plan. The session-start form.
    pub(crate) fn plan_probed(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
        burn_capable: bool,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plan(parts, info, &self.probe(info, burn_capable))
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

    /// Plan against the source's own reachability — the form used while
    /// choosing between candidates.
    pub(crate) fn plan_auto(
        &self,
        parts: &[PartSource],
        info: &kahawai_core::media::MediaInfo,
    ) -> kahawai_media::negotiate::SourcePlan {
        self.plan_probed(parts, info, self.reads_sets_for(parts))
    }

    /// Which source to play and how, judged across every candidate.
    ///
    /// Returns `Cost::Unplayable` rather than failing when nothing the
    /// client accepts can be produced — a caller asking "what would I
    /// get" deserves that answer, and only a session turns it into an
    /// error. The two failures here are different: they mean there is
    /// no source to negotiate against at all.
    pub(crate) async fn best_source(
        &self,
        item_id: &str,
        mode: Option<&str>,
    ) -> Result<(
        Vec<PartSource>,
        kahawai_core::media::MediaInfo,
        kahawai_media::negotiate::SourcePlan,
        String,
    )> {
        match mode {
            // Operator override (scripts, pipeline debugging): the mode
            // is forced on the rank-best source; the PLAN still comes
            // from negotiation.
            Some(m) => {
                let (parts, info) = self.sessions.source_parts(self.registry, item_id).await?;
                let sp = self.plan_auto(&parts, &info);
                Ok((parts, info, sp, m.to_string()))
            }
            // HUB-14/16: judge every candidate, cheapest sufficient
            // path wins, rank breaks ties.
            None => {
                let mut candidates = self
                    .sessions
                    .candidate_sources(self.registry, item_id)
                    .await?;
                // A burn pick pins the source it binds to: judging the
                // others would let a cheaper copy win and silently drop
                // the burn the user explicitly selected.
                if self.burn_row.is_some() {
                    candidates.retain(|(parts, _)| self.pick_for(parts).is_some());
                    if candidates.is_empty() {
                        bail!("the picked subtitle track's source is not available");
                    }
                }
                if candidates.is_empty() {
                    bail!("no source is currently available (mediahost offline)");
                }
                let mut best: Option<(kahawai_media::negotiate::SourcePlan, usize)> = None;
                for (idx, (parts, info)) in candidates.iter().enumerate() {
                    let sp = self.plan_auto(parts, info);
                    if best.as_ref().is_none_or(|(cur, _)| sp.cost < cur.cost) {
                        best = Some((sp, idx));
                    }
                }
                let (sp, idx) = best.unwrap();
                let mode = if sp.direct { "direct" } else { "remux" };
                let (parts, info) = candidates.into_iter().nth(idx).unwrap();
                Ok((parts, info, sp, mode.to_string()))
            }
        }
    }
}

fn local_encoder_names() -> Vec<String> {
    kahawai_media::remux::encoder_capabilities()
        .iter()
        .map(|(c, _, _)| c.to_string())
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
        "SELECT stream_index, id FROM subtitle_tracks
         WHERE origin = 'embedded'
           AND (module_id, collection_id, path_rel) = (?, ?, ?)",
    )
    .bind(&p.module_id)
    .bind(&p.collection_id)
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
    /// Timeline base of the part currently playing (0 for single-file).
    pub fn part_base_ms(&self) -> u64 {
        self.parts
            .get(self.current_part.load(std::sync::atomic::Ordering::SeqCst))
            .map(|p| p.base_ms)
            .unwrap_or(0)
    }

    /// Any client activity (stream chunks, playlist/segment fetches,
    /// progress pings) keeps the session alive (HUB-18).
    pub fn touch(&self) {
        *self.touched.lock().unwrap() = std::time::Instant::now();
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
        let _guard = self.handle.enter();
        let mut stream = self.lease.read_range(offset, len).into_inner();
        self.handle.block_on(async {
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
        })
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
fn playlist_ready(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(p) => p.contains("#EXT-X-ENDLIST") || playlist_span_secs(&p) >= 6.5,
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

type LocalResolver = std::sync::Arc<dyn Fn(&str, &str) -> Result<std::path::PathBuf> + Send + Sync>;

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
    /// Registry handle for teardown messages from the sync `end()` path
    /// (set once at startup; None only in tests without dispatch).
    registry_for_teardown: Mutex<Option<Arc<Registry>>>,
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
                        None => Err(e),
                    };
                }
                match tokio::time::timeout(Duration::from_secs(10), rx).await {
                    Ok(Ok(body)) => Ok(body),
                    _ => {
                        self.pending_logs.lock().unwrap().remove(id);
                        bail!("transcoder did not answer with logs in time")
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
            bail!("too many concurrent streams ({held}); close one first");
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
                    sessions.end(&id);
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
    ) -> Result<(String, String, u64, kahawai_core::media::MediaInfo, Lease)> {
        let (parts, info) = self.source_parts(registry, item_id).await?;
        let p = &parts[0];
        let lease = self
            .open_lease(registry, &p.module_id, &p.collection_id, &p.path_rel)
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
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, s.part, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ?
             -- HUB-3 ranking. Resolution tier first: that is a deliberate
             -- quality choice. Within a tier the CORRECTED release wins
             -- (revision — a v2/REPACK is often smaller than the broken
             -- encode it replaces, so size cannot decide this), then size.
             ORDER BY s.part IS NOT NULL,
                      COALESCE(json_extract(f.streams_json, '$.video[0].height'), 0) DESC,
                      COALESCE(f.revision, 1) DESC,
                      f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let parse_info = |r: &sqlx::sqlite::SqliteRow| -> kahawai_core::media::MediaInfo {
            serde_json::from_str(r.get::<String, _>("streams_json").as_str()).unwrap_or_default()
        };
        // Complete single file first (query put part-NULL rows up front).
        if let Some(r) = rows.iter().find(|r| {
            r.get::<Option<i64>, _>("part").is_none()
                && registry.is_connected(&r.get::<String, _>("module_id"))
        }) {
            let info = parse_info(r);
            return Ok((
                vec![PartSource {
                    module_id: r.get("module_id"),
                    collection_id: r.get("collection_id"),
                    path_rel: r.get("path_rel"),
                    size: r.get::<i64, _>("size") as u64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                }],
                info,
            ));
        }
        // Part set: connected parts, ordered, deduped by part number.
        let mut by_part: std::collections::BTreeMap<i64, &sqlx::sqlite::SqliteRow> =
            Default::default();
        for r in &rows {
            if let Some(part) = r.get::<Option<i64>, _>("part")
                && registry.is_connected(&r.get::<String, _>("module_id"))
            {
                by_part.entry(part).or_insert(r);
            }
        }
        if by_part.is_empty() {
            bail!("no source is currently available (mediahost offline)");
        }
        let mut parts = Vec::new();
        let mut base = 0u64;
        let mut first_info = None;
        for (_, r) in by_part {
            let info = parse_info(r);
            let dur = info
                .duration_ms
                .context("multi-part source with unknown part duration")?;
            parts.push(PartSource {
                module_id: r.get("module_id"),
                collection_id: r.get("collection_id"),
                path_rel: r.get("path_rel"),
                size: r.get::<i64, _>("size") as u64,
                base_ms: base,
                duration_ms: dur,
            });
            base += dur;
            first_info.get_or_insert(info);
        }
        Ok((parts, first_info.unwrap()))
    }

    /// HUB-16: EVERY playable candidate, in the established rank order —
    /// each connected complete file is one candidate, plus at most one
    /// part-set candidate at the end. `source_parts` remains "the best
    /// by rank"; negotiation instead judges each candidate by COST and
    /// only falls back to rank as the tiebreak.
    pub(crate) async fn candidate_sources(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<Vec<(Vec<PartSource>, kahawai_core::media::MediaInfo)>> {
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, s.part, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ?
             ORDER BY s.part IS NOT NULL,
                      COALESCE(json_extract(f.streams_json, '$.video[0].height'), 0) DESC,
                      COALESCE(f.revision, 1) DESC,
                      f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        let parse_info = |r: &sqlx::sqlite::SqliteRow| -> kahawai_core::media::MediaInfo {
            serde_json::from_str(r.get::<String, _>("streams_json").as_str()).unwrap_or_default()
        };
        let mut out = Vec::new();
        for r in rows.iter().filter(|r| {
            r.get::<Option<i64>, _>("part").is_none()
                && registry.is_connected(&r.get::<String, _>("module_id"))
        }) {
            let info = parse_info(r);
            out.push((
                vec![PartSource {
                    module_id: r.get("module_id"),
                    collection_id: r.get("collection_id"),
                    path_rel: r.get("path_rel"),
                    size: r.get::<i64, _>("size") as u64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                }],
                info,
            ));
        }
        // The part set (if any) as one trailing candidate, via the
        // existing assembly which already handles ordering and dedup.
        if rows
            .iter()
            .any(|r| r.get::<Option<i64>, _>("part").is_some())
            && let Ok(ps) = self.source_parts(registry, item_id).await
            && ps.0.len() > 1
        {
            out.push(ps);
        }
        Ok(out)
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
        resolve: impl Fn(&str, &str) -> Result<std::path::PathBuf> + Send + Sync + 'static,
    ) {
        *self.local_source.lock().unwrap() =
            Some((module_id.to_string(), std::sync::Arc::new(resolve)));
    }

    pub(crate) async fn open_lease(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
    ) -> Result<Lease> {
        // AR-5/AR-11: the in-process mediahost's byte plane is a
        // function call — resolve the path and read the disk directly.
        let local = {
            let guard = self.local_source.lock().unwrap();
            guard.as_ref().and_then(|(id, resolve)| {
                (id == module_id).then(|| resolve(collection_id, path_rel))
            })
        };
        if let Some(path) = local {
            return Ok(Lease::local(path?));
        }
        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id: collection_id.to_string(),
                path_rel: path_rel.to_string(),
            })),
        };
        self.leases
            .establish(&token, registry.send_to_host(module_id, msg))
            .await
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
        let started = self
            .start_inner(
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
            .await;
        self.release(&id);
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
        let burns_ass = sp.plan.burn_ass.is_some() || sp.burn_ass_sidecar.is_some();
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
        let negotiated = sp;
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
                &part.path_rel,
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
                let plan = negotiated.plan;
                if !plan.playable() {
                    bail!(
                        "no playable streams: {} · {}",
                        negotiated.video_verdict,
                        negotiated.audio_verdict
                    );
                }
                verdict = Some((
                    negotiated.video_verdict.clone(),
                    negotiated.audio_verdict.clone(),
                ));
                session_plan = Some(plan);
                use kahawai_media::remux::StreamMode;
                session_needs = crate::registry::PlacementNeed {
                    encode_video: plan.video == StreamMode::Encode,
                    encode_audio: plan.audio == StreamMode::Encode,
                    video_caps: kahawai_media::remux::source_caps_names("video", &info),
                    audio_caps: kahawai_media::remux::source_caps_names("audio", &info),
                    needs_tonemap: plan.tone_map,
                    // HUB-32a: a HARD filter — see PlacementNeed. Both
                    // arms count: an embedded index in the plan, and a
                    // sidecar script shipped with the session.
                    needs_ass_burn: burns_ass,
                    // HUB-15b: the chosen TARGETS are hard placement
                    // filters (empty when the stream doesn't encode).
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
                    // Filled in just below, once the class is derived.
                    work_class: None,
                    // Average over the whole source: the link term only
                    // needs the order of magnitude, and a per-scene peak
                    // would condemn a box for one busy minute.
                    source_kbps: info
                        .duration_ms
                        .filter(|d| *d > 0)
                        .map(|d| ((parts.iter().map(|p| p.size).sum::<u64>() * 8) / d) as u32),
                };
                // Only an ENCODE has a pace worth learning: a copy runs
                // at whatever the link allows and says nothing about
                // this box's compute.
                if plan.video == StreamMode::Encode {
                    let v = info.video.first();
                    session_class = crate::pace::work_class(
                        v.map_or(0, |v| v.height),
                        v.map_or("", |v| v.codec.as_str()),
                        plan.video_codec.as_str(),
                        plan.tone_map,
                    );
                    session_needs.work_class = Some(session_class.clone());
                }
                // Encode work goes to the fleet when one is available
                // (§4.5); pure remux — and encode with no fleet — stays
                // in the local supervised worker.
                // HUB-36 phase 5: the placement now carries what it is
                // expected to sustain, so a session that will crawl says
                // so instead of letting the viewer discover it.
                let placement = if session_needs.encode_video || session_needs.encode_audio {
                    registry.place(&session_needs)
                } else {
                    crate::registry::Placement {
                        target: None,
                        predicted: None,
                    }
                };
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
        let session = Arc::new(Session {
            id,
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            module_id,
            size,
            container: info.container.clone(),
            duration_ms: if parts.len() > 1 {
                Some(total_ms)
            } else {
                info.duration_ms
            },
            parts,
            current_part: std::sync::atomic::AtomicUsize::new(start_idx),
            mode: session_mode,
            verdict: Mutex::new(verdict),
            sub_verdicts: Mutex::new(sub_verdicts),
            burn_sets: Mutex::new(burn_sets.clone()),
            burn_pick: Mutex::new(burn_pick),
            ass: neg.ass.clone(),
            burn_ass_text: Mutex::new(burn_ass_text),
            profile: neg.profile().clone(),
            sink: Mutex::new(chosen_sink),
            seek_lock: tokio::sync::Mutex::new(()),
            pending_seek: Mutex::new(None),
            seek_gen: std::sync::atomic::AtomicU64::new(0),
            seek_done: tokio::sync::watch::channel((0, Ok(0))).0,
            plan: Mutex::new(session_plan),
            needs: session_needs,
            pace_class: session_class,
            touched: Mutex::new(std::time::Instant::now()),
        });
        self.active
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        registry.emit(serde_json::json!({ "kind": "sessions" }));
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
                    &part.path_rel,
                )
                .await?;
            out.push((lease, part.size));
        }
        Ok(out)
    }

    /// Spin up the remux/transcode pipeline — in a supervised worker
    /// process when configured — feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    async fn start_remux(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
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
                        if !(status.success() && playlist_ready(&playlist)) {
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
            if playlist_ready(&playlist) {
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
                    tone_map: plan.tone_map,
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
        if let Err(e) = registry.send_to_tc(transcoder, start).await {
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
            let track = crate::tracks::get(registry.db(), tid)
                .await?
                .with_context(|| format!("no subtitle track {tid}"))?;
            anyhow::ensure!(
                track.item_id == session.item_id,
                "subtitle track {tid} belongs to another item"
            );
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
                && track.path_rel.as_deref() == Some(part.path_rel.as_str()))
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
                        let (module_id, collection_id, walk_rel, walk_idx, _) =
                            subtitles.extract_ref(registry, &track).await?;
                        let sets = subtitles
                            .image_sets(
                                registry,
                                &module_id,
                                &collection_id,
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
            let (_, _, _, info) = crate::subtitles::source_row(registry, &session.item_id).await?;
            // HUB-15a: the executor is already chosen here — ask IT.
            let tonemap = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry.transcoder_reports_tonemap(&tc)
                }
                _ => kahawai_media::remux::tonemap_available(),
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
            // on a track switch.
            let targets = match &session.mode {
                Mode::Transcode { transcoder } => {
                    let tc = transcoder.lock().unwrap().clone();
                    registry.transcoder_encoders(&tc)
                }
                _ => local_encoder_names(),
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
                        &p.path_rel,
                        info.subtitles.len(),
                    )
                })
                .unwrap_or_default();
            let sp = kahawai_media::negotiate::negotiate(
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
                        _ => kahawai_media::remux::ass_burn_available(),
                    },
                    ..session.ass.clone()
                },
                &targets,
            );
            plan = sp.plan;
            anyhow::ensure!(plan.playable(), "selected track is not playable");
            *session.verdict.lock().unwrap() =
                Some((sp.video_verdict.clone(), sp.audio_verdict.clone()));
            let mut subs = sp.subtitles;
            fill_verdict_track_ids(registry, &session.parts, &mut subs).await;
            *session.sub_verdicts.lock().unwrap() = subs;
            *session.plan.lock().unwrap() = Some(plan);
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
                    self.end(&id);
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
        let plan = (*session.plan.lock().unwrap()).context("not a pipeline session")?;
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a dispatched session");
        };
        let old_tc = transcoder.lock().unwrap().clone();
        registry.tc_session_ended(&old_tc);
        let new_tc = registry
            .reserve_transcoder(&session.needs)
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
    pub fn end_for_user(&self, user_id: &str) -> usize {
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
            self.end(&id);
        }
        n
    }

    pub fn end_for_module(&self, module_id: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.module_id == module_id)
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id);
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
    pub fn end(&self, id: &str) -> bool {
        // OPS-10: while the session still exists. Everything below this
        // line has already forgotten it.
        let header = self.log_header(id);
        let Some(session) = self.active.lock().unwrap().remove(id) else {
            return false;
        };
        {
            let mut kept = self.known_sessions.lock().unwrap();
            // Bounded like the bundles themselves: a header is only
            // useful until its bundle lands, moments later.
            if kept.len() > 64 {
                kept.clear();
            }
            kept.insert(id.to_string(), header);
        }
        if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
            registry.emit(serde_json::json!({ "kind": "sessions" }));
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
        tracing::info!(session = id, "session ended");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{PartSource, Sessions, fold_facts, part_index};

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

    fn part(base_ms: u64, duration_ms: u64) -> PartSource {
        PartSource {
            module_id: "m".into(),
            collection_id: "c".into(),
            path_rel: format!("CD{}.avi", base_ms),
            size: 1,
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
