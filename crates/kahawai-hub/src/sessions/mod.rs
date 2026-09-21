//! Play sessions (HUB-18 minimal): direct play and in-hub remux (AR-10).
//!
//! ponytail: sessions are in-memory (lost on hub restart, clients reopen);
//! idle timeout and per-user concurrency limits land with HUB-18 proper.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::bytes::{ByteSources, LeaseSource, Reader};
use crate::leases::Lease;
use crate::registry::{LoudnessPreference, Registry};

pub use crate::bytes::SourceOffline;

mod admission;
mod diagnostics;
mod dispatch;
mod local;
mod negotiation;
mod reschedule;
mod seek;
mod start;
#[cfg(test)]
mod tests;

use admission::*;
pub use local::*;
pub(crate) use negotiation::*;
use seek::*;
use start::*;

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

pub(crate) mod catalogue;
pub use catalogue::{CataloguePlayback, CatalogueSession, FileId};

/// One file of a (possibly multi-part) source, with its absolute
/// timeline offset. CD1/CD2-era rips: parts play as one continuous
/// timeline; part boundaries are ordinary seek-restarts.
#[derive(Debug, Clone)]
pub struct PartSource {
    pub file_id: FileId,
    pub head_xxh3: i64,
    pub tail_xxh3: i64,
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

/// An initial chapter position is source-relative. Saved resume is item-relative;
/// recovery carries the previous physical fingerprint and an absolute position.
#[derive(Default)]
pub struct StartOptions {
    pub ms: u64,
    pub explicit_position: bool,
    pub resume: bool,
    pub source_fingerprint: Option<String>,
    pub catalogue: Option<CataloguePlayback>,
}
impl From<u64> for StartOptions {
    fn from(ms: u64) -> Self {
        Self {
            ms,
            ..Default::default()
        }
    }
}

pub struct Session {
    pub catalogue: CatalogueSession,
    info: kahawai_core::media::MediaInfo,
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub collection_item_id: String,
    pub playable_source_id: i64,
    pub library_item_ids: Vec<String>,
    pub source_fingerprint: String,
    pub replay_gain: Option<kahawai_core::media::ReplayGain>,
    pub effective_start_ms: u64,
    pub last_position_ms: std::sync::atomic::AtomicU64,
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
    /// waits for it before ending the session, so the last progress write
    /// cannot land after teardown has completed.
    ending: tokio::sync::RwLock<bool>,
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

    pub fn idle_for(&self) -> Duration {
        self.touched.lock().unwrap().elapsed()
    }
}

/// The transcoder's answer to a dispatch: ready (with the worker's
/// session facts, AR-13) or an error string.
type ReadyVerdict = Result<Vec<kahawai_media::facts::Fact>, String>;

/// Leases for every part of one session (with sizes), plus the index
/// of the part playback started in.
type PartLeases = (Vec<(Lease, u64)>, usize);

pub struct Sessions {
    /// The byte plane: leases and the all-in-one short-circuit.
    pub bytes: Arc<ByteSources>,
    /// Scratch space for remux sessions (`<data_dir>/sessions`).
    scratch_root: PathBuf,
    max_per_user: usize,
    idle_timeout: Duration,
    /// The binary to spawn as the per-session pipeline worker (the hub
    /// passes its own executable; the worker is a hidden subcommand).
    /// None → pipelines run in-process (tests only).
    worker_exe: Option<PathBuf>,
    active: Mutex<HashMap<String, Arc<Session>>>,
    /// True while `active` is empty. Background work that must not
    /// compete with a viewer (idle OCR) waits on this instead of polling
    /// the session list.
    idle: tokio::sync::watch::Sender<bool>,
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

impl Sessions {
    pub fn new(scratch_root: PathBuf) -> Self {
        Self::with_limits(scratch_root, 4, Duration::from_secs(90))
    }

    pub fn with_limits(scratch_root: PathBuf, max_per_user: usize, idle_timeout: Duration) -> Self {
        // Recover crash-interrupted local runs before deleting their scratch.
        // The durable start header supplies the item identity after memory is lost.
        if let Some(data_dir) = scratch_root.parent()
            && let Ok(entries) = std::fs::read_dir(&scratch_root)
        {
            for entry in entries.flatten() {
                let id = entry.file_name().to_string_lossy().into_owned();
                if let Some(path) = crate::sessionlog::for_session(data_dir, &id)
                    && let Ok(header) = std::fs::read_to_string(path)
                    && let Some(item) = header
                        .lines()
                        .find_map(|line| line.strip_prefix("item:").map(str::trim))
                {
                    crate::sessionlog::store(
                        data_dir,
                        item,
                        &id,
                        &format!(
                            "== recovered after hub restart\n{}",
                            local_bundle(&entry.path())
                        ),
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&scratch_root);
        Self {
            bytes: Arc::new(ByteSources::new()),
            scratch_root,
            max_per_user,
            idle_timeout,
            worker_exe: None,
            active: Mutex::new(HashMap::new()),
            idle: tokio::sync::watch::channel(true).0,
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

    /// Active sessions for the admin dashboard (HUB-18).
    /// Flips to true when the last session ends and to false when one
    /// starts. Idle work that must yield to playback waits on it.
    pub fn idle_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.idle.subscribe()
    }

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
        let (session, idle) = {
            let mut active = self.active.lock().unwrap();
            let Some(session) = active.remove(id) else {
                return false;
            };
            (session, active.is_empty())
        };
        if idle {
            self.idle.send_replace(true);
        }
        // A progress handler that already found this session finishes its DB
        // write before teardown reads the result. One that arrives after the
        // removal either fails its lookup or sees `ending` and writes nothing.
        let mut ending = session.ending.write().await;
        *ending = true;
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
                // touches the link. Seek-restarts preserve their evidence in
                // the same bounded bundle before clearing scratch too.
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
            Mode::Direct { .. } => {
                if let Some(data_dir) = self.data_dir() {
                    let (item, header) = self.log_header(id);
                    crate::sessionlog::store(data_dir, &item, id, &header);
                }
            }
        }
        if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
            registry.emit(crate::registry::RegistryEvent::Sessions { kind: "sessions" });
        }
        tracing::info!(session = id, "session ended");
        true
    }
}
