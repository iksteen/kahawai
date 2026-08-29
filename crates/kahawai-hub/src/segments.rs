//! Media segments: where the recap, the opening and the end credits are, so a
//! client can offer to skip them.
//!
//! The hub chooses and persists work; the owning mediahost runs
//! `kahawai-intro` against exact local sources and returns boundaries. A season
//! is the unit because an opening is found by comparing episodes; one episode
//! on its own has nothing to match.
//!
//! ## `media_segments` schema (authority for migration 0062)
//!
//! - `item_id` + `kind` — one segment of each kind per episode, replaced
//!   wholesale when a re-analysis produces a new answer. `kind` is `recap`,
//!   `intro` or `credits`; nothing else is stored, so a client can treat an
//!   unknown kind as a bug rather than a feature it has yet to learn.
//! - `start_ms` / `end_ms` — milliseconds from the start of the item's
//!   timeline, the same clock as `watch_state.position_ms`, so the player
//!   compares them directly against the current position.
//! - `source` — which analyzer answered: `chapter` when the file named the
//!   boundary itself, `chromaprint` or `blackframe` when it was inferred.
//!   Kept because they fail differently: a chromaprint credits segment starts
//!   where the music starts, a black-frame one where the picture goes dark,
//!   and a chapter one wherever the person who made the file put it.
//!
//! ## Segment scan/failure schemas
//!
//! `media_segment_scans` is one successful detector answer per episode; its
//! generation and source mtime keep found-nothing answers settled while that
//! rendition remains current. `media_segment_failures` is different by design:
//! one row per exact `(item,module,collection,root,path,size,mtime,detector)` revision.
//! Scheduling skips only failed physical sources, can fall through to another
//! rendition of the same episode, and stops retrying when every current
//! rendition has failed. A move/replacement or detector bump asks again; a
//! successful rendition clears only its own exact failure. Migration 72 removes
//! the short-lived false failures whose error was comparison insufficiency,
//! because that state says nothing about the readable source's bytes.
//!
//! Boundaries are MEASURED ON — and chapters read from — the playback-ranked
//! rendition, and stored per item. A client that negotiates an alternative
//! rendition with a different cut may find them shifted; per-rendition
//! segments are a schema change deferred until renditions with different
//! cuts prove real. The QUERY chapter override already covers the chapters
//! half, because chapters exist per file in `streams_json`.
//!
//! `mtime_unix` is what makes the row a statement about BYTES rather than
//! about a name: the modification time of the file the detector actually
//! read. Replace that file — a re-download of a truncated one, a re-encode,
//! a restore — and its mtime leaves the item's rendition set, the row stops
//! matching, and the episode is asked again. The predicate is MEMBERSHIP in
//! that set, not "equals the best-ranked rendition": rank in SQL cannot see
//! connectivity, while the resolver reads the best CONNECTED rendition, and
//! the two disagreeing re-read whole seasons in a loop for the length of a
//! partial outage. The cost of membership is that a NEW rendition merely
//! outranking the analysed one does not re-ask — consistent with the
//! per-rendition deferral above, which already treats an item's renditions
//! as the same content. Migration 0063's backfill stamped the MAX across
//! every rendition's files (its checksum is frozen; this doc is the
//! authority) — a member of the set except when a multi-part sibling holds
//! the newest mtime, which costs that row one re-analysis. Rows for items
//! with no files stayed `NULL` and match any bytes.
//!
//! ## Per-file chapter classifier index (authority for migration 0066)
//!
//! `files.chapter_segment_kinds` is a bitmask derived from the file's raw
//! `streams_json.chapters`: recap = 1, intro = 2, credits = 4.
//! `chapter_segments_detector` says which
//! [`kahawai_core::segments::DETECTOR_GENERATION`] interpreted those names.
//! File upserts and later chapter declarations replace raw facts and mask
//! together; startup rebuilds null/stale masks by parsing SQLite JSON in Rust.
//! Rebuild cost is one pure pass over stored metadata — no source opens
//! or media bytes — while point-of-need scheduling is an indexed integer test.
//! The sweep admits an incompatible host's chapter exception only when every
//! episode has some complete single-part source with both intro and credits
//! bits; generic scene chapter lists stay module-skipped.
//!
//! Accepted residual: the sweep's no-progress guard compares the pending
//! COUNT, not the set. A pass that scans one episode while a replacement
//! makes another pending again can present the same count and trip the
//! guard; the failed set's expiry retries it six hours later, so the cost
//! is latency, not loss.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use kahawai_core::segments::DETECTOR_GENERATION as DETECTOR;
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;

use crate::registry::Registry;
use crate::sessions::Sessions;

/// How long a failed season stays set aside before the sweep tries it again.
const FAILED_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// One boundary pair for one episode.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Segment {
    /// `recap`, `intro` or `credits`.
    pub kind: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Which analyzer answered: `chapter`, `chromaprint` or `blackframe`.
    pub source: String,
}

/// Segments of one item, earliest first. Empty for anything not analyzed, or
/// analyzed and found to have none — the two are the same to a player.
pub async fn for_item(db: &sqlx::SqlitePool, item_id: &str) -> Result<Vec<Segment>> {
    let rows = sqlx::query(
        "SELECT kind, start_ms, end_ms, source FROM media_segments
          WHERE item_id = ? ORDER BY start_ms",
    )
    .bind(item_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Segment {
            kind: r.get("kind"),
            start_ms: r.get("start_ms"),
            end_ms: r.get("end_ms"),
            source: r.get("source"),
        })
        .collect())
}

/// Persist one protocol-4 source-owned segment fact by mapping its exact file
/// into this hub's independent logical item graph. The mediahost scheduled and
/// retained the analysis; this is projection only.
pub async fn store_catalog_result(
    registry: &Registry,
    module_id: &str,
    collection_id: &str,
    result: &kahawai_proto::v1::SegmentDetectionResult,
) -> Result<usize> {
    let mut stored = 0usize;
    for episode in &result.episodes {
        if episode.retryable {
            continue;
        }
        let source = episode
            .source
            .as_ref()
            .context("catalogue segment missing source")?;
        let item_ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT ps.item_id
               FROM files f
               JOIN collection_roots r ON r.id=f.root_id
               JOIN playable_source_parts psp ON psp.file_id=f.id
               JOIN playable_sources ps ON ps.id=psp.playable_source_id
              WHERE f.module_id=? AND f.collection_id=?
                AND r.root_token=? AND f.path_rel=?
                AND f.size=? AND f.mtime_unix=?",
        )
        .bind(module_id)
        .bind(collection_id)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .bind(episode.observed_size as i64)
        .bind(episode.observed_mtime_unix)
        .fetch_all(registry.db())
        .await?;
        if item_ids.is_empty() {
            tracing::debug!(%module_id, collection = collection_id,
                path = %source.path_rel, "catalogue segment result is stale or unresolved");
            continue;
        }
        let mut tx = registry.db().begin().await?;
        for item_id in item_ids {
            if episode.unreadable || !episode.error.is_empty() {
                sqlx::query(
                    "INSERT OR REPLACE INTO media_segment_failures
                       (item_id,module_id,collection_id,root_token,path_rel,size,mtime_unix,
                        detector,error,failed_at)
                     VALUES(?,?,?,?,?,?,?,?,?,unixepoch())",
                )
                .bind(&item_id)
                .bind(module_id)
                .bind(collection_id)
                .bind(&source.root_token)
                .bind(&source.path_rel)
                .bind(episode.observed_size as i64)
                .bind(episode.observed_mtime_unix)
                .bind(result.detector)
                .bind(&episode.error)
                .execute(&mut *tx)
                .await?;
                continue;
            }
            sqlx::query("DELETE FROM media_segments WHERE item_id=?")
                .bind(&item_id)
                .execute(&mut *tx)
                .await?;
            for segment in &episode.segments {
                sqlx::query(
                    "INSERT INTO media_segments(item_id,kind,start_ms,end_ms,source)
                     VALUES(?,?,?,?,?)",
                )
                .bind(&item_id)
                .bind(&segment.kind)
                .bind(segment.start_ms as i64)
                .bind(segment.end_ms as i64)
                .bind(&segment.analyzer)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "INSERT INTO media_segment_scans
                   (item_id,detector,mtime_unix,module_id,collection_id,root_token,path_rel,size,error)
                 VALUES(?,?,?,?,?,?,?,?, '')
                 ON CONFLICT(item_id) DO UPDATE SET detector=excluded.detector,
                   mtime_unix=excluded.mtime_unix,module_id=excluded.module_id,
                   collection_id=excluded.collection_id,root_token=excluded.root_token,
                   path_rel=excluded.path_rel,size=excluded.size,error=''",
            )
            .bind(&item_id)
            .bind(result.detector)
            .bind(episode.observed_mtime_unix)
            .bind(module_id)
            .bind(collection_id)
            .bind(&source.root_token)
            .bind(&source.path_rel)
            .bind(episode.observed_size as i64)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM media_segment_failures
                  WHERE item_id=? AND module_id=? AND collection_id=?
                    AND root_token=? AND path_rel=? AND size=? AND mtime_unix=?
                    AND detector=?",
            )
            .bind(&item_id)
            .bind(module_id)
            .bind(collection_id)
            .bind(&source.root_token)
            .bind(&source.path_rel)
            .bind(episode.observed_size as i64)
            .bind(episode.observed_mtime_unix)
            .bind(result.detector)
            .execute(&mut *tx)
            .await?;
            stored += 1;
        }
        tx.commit().await?;
    }
    Ok(stored)
}

pub async fn remove_catalog_result(
    registry: &Registry,
    module_id: &str,
    collection_id: &str,
    source: &crate::registry::SourcePath,
) -> Result<u64> {
    let mut tx = registry.db().begin().await?;
    // A logical episode can have several source renditions. Only remove the
    // projected result when this tombstone names the source that produced the
    // current scan; an older rendition's tombstone must not erase a newer one.
    let item_ids: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM media_segment_scans
          WHERE module_id=? AND collection_id=? AND root_token=? AND path_rel=?",
    )
    .bind(module_id)
    .bind(collection_id)
    .bind(&source.root_token)
    .bind(&source.path_rel)
    .fetch_all(&mut *tx)
    .await?;
    let mut removed = 0;
    for item_id in item_ids {
        removed += sqlx::query("DELETE FROM media_segments WHERE item_id=?")
            .bind(&item_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query(
            "DELETE FROM media_segment_scans
              WHERE item_id=? AND module_id=? AND collection_id=?
                AND root_token=? AND path_rel=?",
        )
        .bind(&item_id)
        .bind(module_id)
        .bind(collection_id)
        .bind(&source.root_token)
        .bind(&source.path_rel)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "DELETE FROM media_segment_failures
          WHERE module_id=? AND collection_id=? AND root_token=? AND path_rel=?",
    )
    .bind(module_id)
    .bind(collection_id)
    .bind(&source.root_token)
    .bind(&source.path_rel)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(removed)
}

/// A season worth analyzing: its show, its number, and how many of its episodes
/// have never been looked at.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PendingSeason {
    /// Scheduler ownership. Internal: clients need the season, not which
    /// control link will execute inferred analysis.
    #[serde(skip)]
    #[schema(ignore)]
    pub(crate) module_id: String,
    /// Conservative chapter fast-path hint: true when every episode has at
    /// least one complete single-part source with stored chapter data. Names
    /// are checked later; false is the only value safe for module-wide skip.
    #[serde(skip)]
    #[schema(ignore)]
    pub(crate) may_be_all_named: bool,
    pub series_id: String,
    pub title: String,
    pub season: i64,
    pub episodes: i64,
    pub pending: i64,
}

/// What the detector is doing, for the admin page.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Status {
    pub running: bool,
    /// The last pass left episodes waiting on an absent mediahost.
    pub awaiting_host: bool,
    /// The last pass ended in an error (the hub log has the story).
    pub last_failed: bool,
    /// When the hub process started (unix seconds). The dispatch counter
    /// resets with the process; compare this against the dispatch answer's
    /// `boot` before trusting the count.
    pub boot: u64,
    /// Admin-dispatched runs finished since the hub started.
    pub dispatched: usize,
    /// The latest dispatched run left episodes waiting on an absent
    /// mediahost.
    pub dispatched_awaiting_host: bool,
    /// The latest dispatched run ended in an error.
    pub dispatched_failed: bool,
    /// Episodes analyzed since this hub started.
    pub analyzed: usize,
    /// Seasons still waiting.
    pub pending_seasons: usize,
    pub detector: i64,
}

struct SegmentWaiter {
    module_id: String,
    generation: u64,
    current: Arc<AtomicBool>,
    tx: tokio::sync::oneshot::Sender<SegmentReply>,
}

#[derive(Debug)]
pub(crate) enum SegmentJobFailure {
    Disconnected,
    UnsupportedDetector(String),
    Rejected(String),
}

pub(crate) type SegmentReply =
    std::result::Result<kahawai_proto::v1::SegmentDetectionResult, SegmentJobFailure>;

pub struct Detector {
    running: AtomicBool,
    /// Whether the LAST pass left episodes waiting on an absent mediahost —
    /// the admin page's poller reads this through the status endpoint, or a
    /// dispatched run that could reach nothing would toast as "analysed".
    awaiting_host: AtomicBool,
    /// Whether the LAST pass ended in an error, for the same reader: a
    /// failed run must not toast as an analysed one.
    last_failed: AtomicBool,
    /// Admin-dispatched runs FINISHED since the hub started, and the latest
    /// one's outcome. Dedicated cells, not the shared pass flags above: the
    /// sweep grinds on beside a dispatched run and overwrites those with
    /// whichever pass finishes last, so the admin page's completion toast
    /// reported the sweep's weather as the run's. The poller instead
    /// watches this counter pass the mark the dispatch answered with, and
    /// reads the outcome nobody else writes.
    dispatched: AtomicUsize,
    dispatched_awaiting: AtomicBool,
    dispatched_failed: AtomicBool,
    /// When this process started, to seconds. The dispatch counter above
    /// lives in memory, and a poller comparing counts across a hub restart
    /// reads a reset 0 as "still running" (or a later admin's run as its
    /// own). The boot value rides on both the dispatch answer and the
    /// status, so "not the hub I asked" is one comparison.
    boot: u64,
    analyzed: AtomicUsize,
    /// Seasons that failed, with when: a broken file must not become a
    /// re-analysis loop that never reaches the season behind it — but an
    /// entry EXPIRES, because a flapping link can land an ordinary season
    /// here (the reads die, the host is back by the time connectivity is
    /// consulted) and "until somebody restarts the hub" is the wrong
    /// sentence for weather. A genuinely broken season retries a few times
    /// a day, minutes of reading, which is an acceptable price for the
    /// flap victims recovering on their own.
    failed: tokio::sync::Mutex<std::collections::HashMap<(String, i64), std::time::Instant>>,
    /// One season at a time, across the sweep and the admin trigger alike.
    /// The mediahost also serializes its local jobs, but this lock prevents the
    /// hub from queueing the same global work twice.
    one_at_a_time: tokio::sync::Mutex<()>,
    waiters: parking_lot::Mutex<std::collections::HashMap<String, SegmentWaiter>>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

async fn pending_episode_ids(
    db: &sqlx::SqlitePool,
    series_id: &str,
    season: i64,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT i.id FROM items i
          WHERE i.parent_id = ? AND i.season = ? AND i.kind = 'episode'
            AND NOT EXISTS (
                SELECT 1 FROM media_segment_scans s
                 WHERE s.item_id = i.id AND s.detector = ?
                   AND (s.mtime_unix IS NULL OR s.mtime_unix IN (
                       SELECT f.mtime_unix
                         FROM playable_sources ps
                         JOIN playable_source_parts psp
                              ON psp.playable_source_id = ps.id
                         JOIN files f ON f.id = psp.file_id
                        WHERE ps.item_id = i.id AND ps.expected_parts = 1)))
            AND EXISTS (
                SELECT 1
                  FROM playable_sources ps
                  JOIN playable_source_parts psp ON psp.playable_source_id = ps.id
                  JOIN files f ON f.id = psp.file_id
                  LEFT JOIN collection_roots r ON r.id = f.root_id
                 WHERE ps.item_id = i.id AND ps.expected_parts = 1
                   AND (SELECT COUNT(*) FROM playable_source_parts all_parts
                         WHERE all_parts.playable_source_id = ps.id) = 1
                   AND NOT EXISTS (
                       SELECT 1 FROM media_segment_failures failure
                        WHERE failure.item_id = i.id AND failure.detector = ?
                          AND failure.module_id = f.module_id
                          AND failure.collection_id = f.collection_id
                          AND failure.root_token = COALESCE(r.root_token,'')
                          AND failure.path_rel = f.path_rel
                          AND failure.size = f.size
                          AND failure.mtime_unix = f.mtime_unix))",
    )
    .bind(series_id)
    .bind(season)
    .bind(DETECTOR)
    .bind(DETECTOR)
    .fetch_all(db)
    .await?)
}
async fn segment_source_failed(
    db: &sqlx::SqlitePool,
    item_id: &str,
    revision: &SourceRevision,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM media_segment_failures
            WHERE item_id=? AND detector=? AND module_id=? AND collection_id=?
              AND root_token=? AND path_rel=? AND size=? AND mtime_unix=?)",
    )
    .bind(item_id)
    .bind(DETECTOR)
    .bind(&revision.module_id)
    .bind(&revision.collection_id)
    .bind(&revision.root_token)
    .bind(&revision.path_rel)
    .bind(revision.size as i64)
    .bind(revision.mtime_unix)
    .fetch_one(db)
    .await?)
}

struct SegmentCandidate {
    part: crate::sessions::PartSource,
    info: kahawai_core::media::MediaInfo,
}

struct SegmentEpisodeOptions {
    item_id: String,
    pending: bool,
    candidates: Vec<SegmentCandidate>,
}

fn choose_segment_home(options: &[SegmentEpisodeOptions]) -> Option<(String, String)> {
    let mut scores: std::collections::BTreeMap<(String, String), (usize, usize, usize)> =
        Default::default();
    for episode in options {
        let mut seen = std::collections::HashSet::new();
        for (rank, candidate) in episode.candidates.iter().enumerate() {
            let home = (
                candidate.part.module_id.clone(),
                candidate.part.collection_id.clone(),
            );
            if !seen.insert(home.clone()) {
                continue;
            }
            let score = scores.entry(home).or_default();
            score.0 += usize::from(episode.pending);
            score.1 += 1;
            score.2 += rank;
        }
    }
    scores
        .into_iter()
        .filter(|(_, (pending, total, _))| *pending > 0 && *total >= 2)
        .min_by_key(|(home, (pending, total, rank))| {
            (
                std::cmp::Reverse(*pending),
                std::cmp::Reverse(*total),
                *rank,
                home.clone(),
            )
        })
        .map(|(home, _)| home)
}

impl Detector {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            awaiting_host: AtomicBool::new(false),
            last_failed: AtomicBool::new(false),
            dispatched: AtomicUsize::new(0),
            dispatched_awaiting: AtomicBool::new(false),
            dispatched_failed: AtomicBool::new(false),
            boot: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            analyzed: AtomicUsize::new(0),
            failed: Default::default(),
            one_at_a_time: Default::default(),
            waiters: Default::default(),
        }
    }

    pub(crate) fn wait_for_segment_result(
        &self,
        module_id: &str,
        generation: u64,
        current: Arc<AtomicBool>,
        request_id: &str,
    ) -> tokio::sync::oneshot::Receiver<SegmentReply> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiters.lock().insert(
            request_id.to_string(),
            SegmentWaiter {
                module_id: module_id.to_string(),
                generation,
                current,
                tx,
            },
        );
        rx
    }

    pub(crate) fn segment_accepted(
        &self,
        module_id: &str,
        generation: u64,
        accepted: kahawai_proto::v1::SegmentDetectionAccepted,
    ) {
        if accepted.state != "rejected" {
            return;
        }
        let failure = if accepted.rejection
            == kahawai_proto::v1::SegmentDetectionRejection::UnsupportedDetector as i32
        {
            SegmentJobFailure::UnsupportedDetector(accepted.error)
        } else {
            // Default/unknown reasons retain the old protocol's generic
            // rejection behavior. An old mediahost sends zero here.
            SegmentJobFailure::Rejected(accepted.error)
        };
        let waiter = {
            let mut waiters = self.waiters.lock();
            waiters
                .get(&accepted.request_id)
                .is_some_and(|waiter| {
                    waiter.module_id == module_id
                        && waiter.generation == generation
                        && waiter.current.load(Ordering::Acquire)
                })
                .then(|| waiters.remove(&accepted.request_id))
                .flatten()
        };
        if let Some(waiter) = waiter {
            let _ = waiter.tx.send(Err(failure));
        }
    }

    pub(crate) fn segment_result(
        &self,
        module_id: &str,
        generation: u64,
        result: kahawai_proto::v1::SegmentDetectionResult,
    ) {
        let waiter = {
            let mut waiters = self.waiters.lock();
            waiters
                .get(&result.request_id)
                .is_some_and(|waiter| {
                    waiter.module_id == module_id
                        && waiter.generation == generation
                        && waiter.current.load(Ordering::Acquire)
                })
                .then(|| waiters.remove(&result.request_id))
                .flatten()
        };
        if let Some(waiter) = waiter {
            let _ = waiter.tx.send(Ok(result));
        } else {
            tracing::warn!(%module_id, request = %result.request_id,
                "late or wrong-host segment result dropped");
        }
    }

    pub(crate) fn segment_link_disconnected(&self, module_id: &str, generation: u64) {
        let lost = {
            let mut waiters = self.waiters.lock();
            let ids = waiters
                .iter()
                .filter(|(_, waiter)| {
                    waiter.module_id == module_id && waiter.generation == generation
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| waiters.remove(&id))
                .collect::<Vec<_>>()
        };
        for waiter in lost {
            let _ = waiter.tx.send(Err(SegmentJobFailure::Disconnected));
        }
    }

    /// The next season a dispatch should work on: the pending list minus
    /// whatever is set aside as failed. The admin route used to take the
    /// pending HEAD instead — and an unfinishable season somebody is
    /// mid-watch on sorts first, so every press of the button re-read that
    /// season's bytes and the season behind it could never be reached.
    pub async fn next_season(&self, db: &sqlx::SqlitePool) -> Result<Option<PendingSeason>> {
        let seasons = pending_seasons(db).await?;
        let mut failed = self.failed.lock().await;
        failed.retain(|_, at| at.elapsed() < FAILED_RETRY_AFTER);
        Ok(seasons
            .into_iter()
            .find(|s| !failed.contains_key(&(s.series_id.clone(), s.season))))
    }

    /// How many dispatched runs have finished, for a dispatch answer to
    /// hand the poller as its mark.
    pub fn dispatched_so_far(&self) -> usize {
        self.dispatched.load(Ordering::Acquire)
    }

    /// The process's boot stamp, for the same answer.
    pub fn boot(&self) -> u64 {
        self.boot
    }

    /// Record a dispatched run's own outcome. The flags land before the
    /// counter moves, so a poller that sees the new count reads this run's
    /// outcome, not the previous one's.
    pub fn record_dispatched(&self, outcome: &Result<Analysis>) {
        self.dispatched_awaiting.store(
            matches!(outcome, Ok(a) if a.awaiting > 0),
            Ordering::Relaxed,
        );
        self.dispatched_failed
            .store(outcome.is_err(), Ordering::Relaxed);
        self.dispatched.fetch_add(1, Ordering::Release);
    }

    /// The counters alone, for a caller that already holds the pending list
    /// and must not walk it a second time to disagree with itself.
    pub fn status_counters(&self) -> Status {
        Status {
            running: self.running.load(Ordering::Relaxed),
            awaiting_host: self.awaiting_host.load(Ordering::Relaxed),
            last_failed: self.last_failed.load(Ordering::Relaxed),
            boot: self.boot,
            dispatched: self.dispatched.load(Ordering::Acquire),
            dispatched_awaiting_host: self.dispatched_awaiting.load(Ordering::Relaxed),
            dispatched_failed: self.dispatched_failed.load(Ordering::Relaxed),
            analyzed: self.analyzed.load(Ordering::Relaxed),
            pending_seasons: 0,
            detector: DETECTOR,
        }
    }

    /// Walk the library a season at a time, forever. The mediahost owns the
    /// foreground gate because only it can see scans and viewer leases.
    pub fn spawn_sweep(self: &Arc<Self>, registry: Arc<Registry>, sessions: Arc<Sessions>) {
        let detector = self.clone();
        tokio::spawn(async move {
            // Let the satellites link and the scans settle.
            tokio::time::sleep(std::time::Duration::from_secs(90)).await;
            // Every season a pass has worked on, and how much of it was
            // outstanding then. A map rather than the single last season:
            // with only one remembered, TWO unfinishable seasons whose order
            // flips with watch activity alternated for ever, each pass a
            // full season analysis, and neither ever seen "twice in a row".
            let mut offered: std::collections::HashMap<(String, i64), i64> = Default::default();
            // Seasons whose mediahost is away THIS cycle. Per-season weather
            // covers source/read failures on an otherwise usable host.
            let mut awaiting_host: std::collections::HashSet<(String, i64)> = Default::default();
            // Protocol/detector incompatibility is module-wide. Once observed,
            // skip every other season on that module for this cycle rather than
            // resolving every episode only to rediscover the same registration.
            let mut awaiting_modules: std::collections::HashSet<String> = Default::default();
            loop {
                // One season per look, from a FRESH list. Walking a snapshot of
                // the whole library instead put anything that became pending
                // mid-pass — an episode replaced, a season just added, a show
                // somebody started watching — behind every season already in
                // hand, which on a library this size is days. The ordering is
                // only worth having if it is consulted.
                // An Err is NOT an empty library: read as one it cleared the
                // guard's memory and idled silently while detection was
                // dead. Say so and retry on the outage cadence.
                let seasons = match pending_seasons(registry.db()).await {
                    Ok(seasons) => seasons,
                    Err(e) => {
                        tracing::warn!(
                            error = format!("{e:#}"),
                            "intro detection cannot read the pending list"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                        continue;
                    }
                };
                let next = {
                    let mut failed = detector.failed.lock().await;
                    failed.retain(|_, at| at.elapsed() < FAILED_RETRY_AFTER);
                    next_pending_season(seasons, &failed, &awaiting_host, &awaiting_modules)
                };
                let Some(season) = next else {
                    offered.clear();
                    // End of a cycle. Seasons that were only waiting on their
                    // host get another look next cycle — sooner when an
                    // outage is what emptied the list, since a host that
                    // comes back should not wait out the long idle sleep.
                    let outage = !awaiting_host.is_empty() || !awaiting_modules.is_empty();
                    awaiting_host.clear();
                    awaiting_modules.clear();
                    tokio::time::sleep(std::time::Duration::from_secs(if outage {
                        300
                    } else {
                        900
                    }))
                    .await;
                    continue;
                };
                if let Some(link) = registry
                    .host_link(&season.module_id)
                    .filter(|link| !link.supports_segment_detection())
                {
                    if awaiting_modules.insert(season.module_id.clone()) {
                        tracing::warn!(
                            module_id = %season.module_id,
                            offered = link.segment_detector_generation(),
                            required = DETECTOR,
                            "intro detection awaits a mediahost with matching detector support"
                        );
                    }
                    // Chapter-complete candidates still enter `analyze_season`:
                    // their stored names can settle the season without sending
                    // one message to this incompatible host.
                    if !season.may_be_all_named {
                        detector.awaiting_host.store(true, Ordering::Relaxed);
                        continue;
                    }
                }

                // Asking for the same season twice with nothing crossed off
                // means it cannot be finished: an episode with no running time
                // recorded is skipped by the analyzer and so never gets a scan
                // row, and the query goes on offering the season for ever.
                // Left alone that is not a slow sweep, it is a season re-read
                // from the mediahost in a loop.
                let key = (season.series_id.clone(), season.season);
                if offered.get(&key) == Some(&season.pending) {
                    tracing::info!(
                        series = %season.title, season = season.season, pending = season.pending,
                        "intro detection cannot finish this season, leaving it"
                    );
                    detector
                        .failed
                        .lock()
                        .await
                        .insert(key.clone(), std::time::Instant::now());
                    // Same reason as the Err arm: the failed set's expiry IS
                    // the retry policy. With the offer left standing, a
                    // multi-day first sweep saw the entry expire mid-cycle,
                    // compared the untouched pending count, and re-set the
                    // season aside every six hours without one fresh read.
                    offered.remove(&key);
                    continue;
                }
                offered.insert(key, season.pending);

                while !sessions.list().is_empty() {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
                match detector
                    .analyze_season(&registry, &sessions, &season.series_id, season.season)
                    .await
                {
                    // A PURE outage steps aside for this cycle: nothing was
                    // even attempted, so retrying next cycle costs nothing —
                    // never failed (the host's absence is the hub's weather,
                    // not the season's defect), never guard-tripped (the
                    // `offered` entry goes, so the retry is not mistaken for
                    // no progress), and never blocking, since one host's
                    // outage must not starve every other host's seasons.
                    //
                    // Only the pure case: a pass that ATTEMPTED pending
                    // episodes and scanned none is the season's own trouble
                    // (dead files on a live host, with or without an offline
                    // sibling), and it falls through to the no-progress
                    // guard below — routed here it re-read the whole season's
                    // bytes every cycle for the length of a partial outage.
                    //
                    // Accepted residual: a host that FLAPS on the sweep's own
                    // cadence alternates this arm (which clears `offered`)
                    // with attempted passes, so the guard never sees the same
                    // pending twice in a row and each up-cycle re-reads the
                    // season. Bounded by the flapping itself: a stable host,
                    // up or down, settles into the guard or this arm.
                    Ok(Analysis {
                        scanned: 0,
                        awaiting,
                        attempted: 0,
                    }) if awaiting > 0 => {
                        tracing::info!(
                            series = %season.title, season = season.season, awaiting,
                            "intro detection: episodes await their mediahost"
                        );
                        let key = (season.series_id.clone(), season.season);
                        offered.remove(&key);
                        awaiting_host.insert(key);
                    }
                    Ok(Analysis { scanned: 0, .. }) => {}
                    Ok(Analysis {
                        scanned, awaiting, ..
                    }) => {
                        tracing::info!(
                            series = %season.title, season = season.season,
                            episodes = scanned, awaiting,
                            "intro detection advanced a season"
                        );
                        // Progress resets the guard's memory: the next offer
                        // has a smaller pending count anyway, and a season
                        // that keeps advancing must never be set aside.
                        offered.remove(&(season.series_id.clone(), season.season));
                    }
                    Err(e) => {
                        tracing::warn!(
                            series = %season.title, season = season.season,
                            error = format!("{e:#}"), "intro detection failed"
                        );
                        detector.failed.lock().await.insert(
                            (season.series_id.clone(), season.season),
                            std::time::Instant::now(),
                        );
                        // The failed set's expiry IS the retry policy. With
                        // the offer left in place, the retry six hours later
                        // saw its own pending count unchanged and the
                        // no-progress guard called the season unfinishable
                        // without a single fresh attempt.
                        offered.remove(&(season.series_id.clone(), season.season));
                    }
                }
            }
        });
    }

    /// Orchestrate one season and store what the owning mediahost returns.
    /// `Done(0)` covers a season finished by another runner and one with too
    /// few comparable episodes.
    pub async fn analyze_season(
        &self,
        registry: &Arc<Registry>,
        sessions: &Arc<Sessions>,
        series_id: &str,
        season: i64,
    ) -> Result<Analysis> {
        let outcome = self
            .analyze_season_inner(registry, sessions, series_id, season)
            .await;
        // The status flags describe THIS pass however it ended. Left unset
        // on an error, they described an earlier pass, and the admin page's
        // toast reported that pass's weather about this one's failure.
        self.awaiting_host.store(
            matches!(&outcome, Ok(a) if a.awaiting > 0),
            Ordering::Relaxed,
        );
        self.last_failed.store(outcome.is_err(), Ordering::Relaxed);
        outcome
    }

    async fn analyze_season_inner(
        &self,
        registry: &Arc<Registry>,
        sessions: &Arc<Sessions>,
        series_id: &str,
        season: i64,
    ) -> Result<Analysis> {
        let _one = self.one_at_a_time.lock().await;
        // Busy for the WHOLE pass, not just the blocking analysis: resolving
        // a big season's sources is seconds of work before the flag used to
        // rise, and a status poll in that window read "not running" about a
        // run that had already been dispatched. RAII, so every early return
        // and error path lowers it.
        self.running.store(true, Ordering::Relaxed);
        let _busy = Lowered(&self.running);
        // Re-check under the lock: the sweep picks its season before blocking
        // here, and the admin route answers before its detached run begins.
        // Another runner may have completed it in that interval.
        let pending_ids = pending_episode_ids(registry.db(), series_id, season).await?;
        if pending_ids.is_empty() {
            return Ok(Analysis {
                scanned: 0,
                awaiting: 0,
                attempted: 0,
            });
        }
        let rows = sqlx::query(
            "SELECT i.id, i.title, i.episode,
                    (SELECT c.media_type FROM collections c
                      WHERE c.module_id = i.module_id
                        AND c.collection_id = i.collection_id) AS media_type
               FROM items i
              WHERE i.parent_id = ? AND i.season = ? AND i.kind = 'episode'
                AND EXISTS (SELECT 1 FROM playable_sources ps WHERE ps.item_id = i.id)
              ORDER BY i.episode, i.id",
        )
        .bind(series_id)
        .bind(season)
        .fetch_all(registry.db())
        .await?;
        if rows.len() < 2 {
            return Ok(Analysis {
                scanned: 0,
                awaiting: 0,
                attempted: 0,
            });
        }
        let anime = rows
            .first()
            .and_then(|r| r.get::<Option<String>, _>("media_type"))
            .map(|t| t == "anime")
            .unwrap_or(false);

        tracing::info!(
            series = %series_id, season, episodes = rows.len(), anime,
            "intro detection: opening a season"
        );
        let mut options = Vec::with_capacity(rows.len());
        let mut awaiting = 0usize;
        for row in &rows {
            let item_id: String = row.get("id");
            let title: String = row.get("title");
            let candidates = match sessions.candidate_sources(registry, &item_id).await {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::debug!(episode = %title, error = format!("{error:#}"),
                        "intro detection: no playable rendition, skipped");
                    continue;
                }
            };
            if candidates.is_empty() {
                if sessions.has_any_source(registry, &item_id).await {
                    awaiting += 1;
                }
                continue;
            }
            let mut eligible = Vec::new();
            for (parts, info) in candidates {
                if parts.len() != 1 {
                    continue;
                }
                let part = parts.into_iter().next().expect("one part checked");
                if part.duration_ms == 0 {
                    continue;
                }
                let revision = SourceRevision::from(&part);
                if !segment_source_failed(registry.db(), &item_id, &revision).await? {
                    eligible.push(SegmentCandidate { part, info });
                }
            }
            let pending = pending_ids.contains(&item_id);
            if eligible.is_empty() {
                // The scheduler saw an unfailed current source, but none of
                // the connected candidates is eligible. It is on an offline
                // host (or changed under this snapshot), so remember the
                // season as awaiting rather than selecting it every pass.
                if pending {
                    awaiting += 1;
                }
                continue;
            }
            options.push(SegmentEpisodeOptions {
                item_id,
                pending,
                candidates: eligible,
            });
        }
        let Some(job_home) = choose_segment_home(&options) else {
            // A segment job runs on one mediahost. Split libraries can only
            // proceed once at least two episodes share a reachable home.
            awaiting += options.iter().filter(|episode| episode.pending).count();
            return Ok(Analysis {
                scanned: 0,
                awaiting,
                attempted: 0,
            });
        };

        let mut episode_ids = Vec::with_capacity(options.len());
        // What each episode's bytes were when this pass read them, recorded
        // with the scan so a replaced file asks again.
        let mut identity: std::collections::HashMap<String, Option<i64>> = Default::default();
        let mut named: std::collections::HashMap<String, Vec<kahawai_core::segments::Named>> =
            Default::default();
        let mut revisions: std::collections::HashMap<String, SourceRevision> = Default::default();
        let mut descriptors = Vec::with_capacity(options.len());
        for episode in options {
            let Some(candidate) = episode.candidates.into_iter().find(|candidate| {
                candidate.part.module_id == job_home.0 && candidate.part.collection_id == job_home.1
            }) else {
                if episode.pending {
                    awaiting += 1;
                }
                continue;
            };
            let part = candidate.part;
            identity.insert(episode.item_id.clone(), Some(part.mtime_unix));
            named.insert(
                episode.item_id.clone(),
                candidate
                    .info
                    .chapters
                    .as_deref()
                    .map(|chapters| kahawai_core::segments::named(chapters, part.duration_ms))
                    .unwrap_or_default(),
            );
            episode_ids.push(episode.item_id.clone());
            revisions.insert(episode.item_id.clone(), SourceRevision::from(&part));
            descriptors.push(kahawai_proto::v1::SegmentEpisode {
                item_id: episode.item_id,
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: part.root_token,
                    path_rel: part.path_rel,
                }),
                expected_size: part.size,
                expected_mtime_unix: part.mtime_unix,
                duration_ms: part.duration_ms,
            });
        }
        let pending_reachable = episode_ids
            .iter()
            .filter(|id| pending_ids.contains(id))
            .count();
        if pending_reachable == 0 || descriptors.len() < 2 {
            return Ok(Analysis {
                scanned: 0,
                awaiting,
                attempted: 0,
            });
        }
        let progress =
            |stored: &[String]| stored.iter().filter(|id| pending_ids.contains(id)).count();
        let all_named = !named.is_empty()
            && episode_ids.iter().all(|item_id| {
                let found = named.get(item_id).map(Vec::as_slice).unwrap_or_default();
                found.iter().any(|n| n.kind == "intro") && found.iter().any(|n| n.kind == "credits")
            });
        if all_named {
            tracing::info!(
                series = %series_id, season, episodes = episode_ids.len(),
                "intro detection: the files name their own boundaries"
            );
            let outcomes = episode_ids
                .iter()
                .map(|item_id| {
                    Answered {
                        item_id: item_id.clone(),
                        found: from_chapters(named.get(item_id)),
                        scanned: true,
                        wholesale: pending_ids.contains(item_id),
                    }
                    .into()
                })
                .collect::<Vec<_>>();
            let stored = self
                .store_guarded(registry, outcomes, &identity, &revisions)
                .await?;
            let scanned = progress(&stored);
            self.analyzed.fetch_add(scanned, Ordering::Relaxed);
            return Ok(Analysis {
                scanned,
                awaiting,
                attempted: 0,
            });
        }
        let (module_id, collection_id) = job_home;
        let Some(link) = registry.host_link(&module_id) else {
            return Ok(Analysis {
                scanned: 0,
                awaiting: awaiting + pending_reachable,
                attempted: 0,
            });
        };
        if !link.supports_segment_detection() {
            tracing::warn!(
                %module_id,
                offered = link.segment_detector_generation(),
                required = DETECTOR,
                "intro detection awaits a mediahost with matching detector support"
            );
            return Ok(Analysis {
                scanned: 0,
                awaiting: awaiting + pending_reachable,
                attempted: 0,
            });
        }
        let generation = link.generation();
        let request_id = ulid::Ulid::generate().to_string();
        let mut reply_rx =
            self.wait_for_segment_result(&module_id, generation, link.current_token(), &request_id);
        let job = kahawai_proto::v1::DetectSegments {
            request_id: request_id.clone(),
            detector: DETECTOR,
            collection_id,
            anime,
            episodes: descriptors.clone(),
        };
        if !registry.host_link_is_current(&module_id, generation) {
            self.segment_link_disconnected(&module_id, generation);
            return Ok(Analysis {
                scanned: 0,
                awaiting: awaiting + pending_reachable,
                attempted: 0,
            });
        }
        let message = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::DetectSegments(job)),
        };
        let early_reply = tokio::select! {
            send_result = link.send(message) => {
                if let Err(error) = send_result {
                    self.waiters.lock().remove(&request_id);
                    tracing::warn!(%module_id, error = format!("{error:#}"),
                        "segment job could not reach mediahost");
                    return Ok(Analysis {
                        scanned: 0,
                        awaiting: awaiting + pending_reachable,
                        attempted: 0,
                    });
                }
                None
            }
            reply = &mut reply_rx => Some(reply),
        };
        let reply = match early_reply {
            Some(reply) => reply,
            None => reply_rx.await,
        };
        let result = match reply {
            Ok(Ok(result)) => result,
            Ok(Err(SegmentJobFailure::Disconnected)) => {
                return Ok(Analysis {
                    scanned: 0,
                    awaiting: awaiting + pending_reachable,
                    attempted: 0,
                });
            }
            Ok(Err(SegmentJobFailure::UnsupportedDetector(error))) => {
                tracing::warn!(%module_id, %error,
                    "intro detection awaits a mediahost with matching detector support");
                return Ok(Analysis {
                    scanned: 0,
                    awaiting: awaiting + pending_reachable,
                    attempted: 0,
                });
            }
            Ok(Err(SegmentJobFailure::Rejected(error))) => {
                anyhow::bail!("mediahost rejected segment job: {error}");
            }
            Err(_) => anyhow::bail!("segment result waiter dropped"),
        };
        anyhow::ensure!(
            result.detector == DETECTOR,
            "segment result has wrong detector generation"
        );
        anyhow::ensure!(
            result.error.is_empty(),
            "mediahost segment analysis failed: {}",
            result.error
        );
        validate_result_set(&descriptors, &result.episodes)?;

        let expected = descriptors
            .iter()
            .map(|episode| (episode.item_id.as_str(), episode))
            .collect::<std::collections::HashMap<_, _>>();
        let mut gone_mid_read = Vec::new();
        let mut outcomes = Vec::with_capacity(result.episodes.len());
        for episode in result.episodes {
            let descriptor = expected.get(episode.item_id.as_str()).with_context(|| {
                format!("segment result names unknown item {}", episode.item_id)
            })?;
            validate_episode_segments(&episode, descriptor)?;
            let revision_matches = result_matches_request(&episode, descriptor)?;
            if !revision_matches && !episode.unreadable {
                outcomes.push(
                    Answered {
                        item_id: episode.item_id,
                        found: Vec::new(),
                        scanned: false,
                        wholesale: false,
                    }
                    .into(),
                );
                continue;
            }
            let failure = if episode.unreadable {
                anyhow::ensure!(
                    !episode.error.is_empty(),
                    "unreadable segment result has no error for {}",
                    episode.item_id
                );
                if !registry.is_connected(&module_id) {
                    gone_mid_read.push(episode.item_id.clone());
                    None
                } else if episode.retryable {
                    // The source was readable, but too few siblings survived
                    // preflight for a comparison. Protocol 4 carries that fact
                    // explicitly; the error string has no wire semantics.
                    None
                } else {
                    Some(episode.error.clone())
                }
            } else {
                None
            };
            let mut found = from_chapters(named.get(&episode.item_id));
            for segment in episode.segments {
                let kind = match segment.kind.as_str() {
                    "recap" => "recap",
                    "intro" => "intro",
                    "credits" => "credits",
                    other => anyhow::bail!("unknown segment kind {other}"),
                };
                let analyzer = match segment.analyzer.as_str() {
                    "chromaprint" => "chromaprint",
                    "blackframe" => "blackframe",
                    other => anyhow::bail!("unknown segment analyzer {other}"),
                };
                if !found.iter().any(|(existing, ..)| *existing == kind) {
                    found.push((kind, segment.start_ms, segment.end_ms, analyzer));
                }
            }
            outcomes.push(EpisodeOutcome {
                answer: Answered {
                    item_id: episode.item_id,
                    found,
                    scanned: !episode.unreadable,
                    wholesale: !episode.unreadable,
                },
                failure,
            });
        }
        let awaiting_mid_read = gone_mid_read.len();
        let awaiting = awaiting + awaiting_mid_read;
        if outcomes.iter().all(|outcome| !outcome.answer.scanned)
            && awaiting == 0
            && outcomes.iter().all(|outcome| outcome.failure.is_none())
        {
            anyhow::bail!("no episode's bytes could be read, and the mediahost is up");
        }
        let stored = self
            .store_guarded(registry, outcomes, &identity, &revisions)
            .await?;
        let scanned = progress(&stored);
        self.analyzed.fetch_add(scanned, Ordering::Relaxed);
        let pending_gone = gone_mid_read
            .iter()
            .filter(|gone| pending_ids.contains(gone))
            .count();
        Ok(Analysis {
            scanned,
            awaiting,
            attempted: pending_reachable.saturating_sub(pending_gone),
        })
    }

    #[cfg(test)]
    async fn store(
        &self,
        registry: &Arc<Registry>,
        boundaries: Vec<Answered>,
        identity: &std::collections::HashMap<String, Option<i64>>,
    ) -> Result<Vec<String>> {
        self.store_guarded(
            registry,
            boundaries.into_iter().map(Into::into).collect(),
            identity,
            &Default::default(),
        )
        .await
    }

    /// Write one season's boundaries and mark its episodes scanned, found
    /// something or not. A source revision supplied by the dispatcher must
    /// still be the item's current exact source inside this transaction.
    async fn store_guarded(
        &self,
        registry: &Arc<Registry>,
        outcomes: Vec<EpisodeOutcome>,
        identity: &std::collections::HashMap<String, Option<i64>>,
        revisions: &std::collections::HashMap<String, SourceRevision>,
    ) -> Result<Vec<String>> {
        // IMMEDIATE, because the first statement is a READ: a deferred
        // transaction pins its snapshot there, and any write committing
        // before our first DELETE — a viewer's ten-second progress ping,
        // since only the sweep waits out playback — invalidates it
        // (SQLITE_BUSY_SNAPSHOT, which no busy_timeout retries). That threw
        // away minutes of finished analysis and set the season aside for
        // six hours over sub-millisecond weather.
        let mut tx = registry.db().begin_with("BEGIN IMMEDIATE").await?;
        let mut scanned = Vec::new();
        for outcome in &outcomes {
            let answer = &outcome.answer;
            let item_id = &answer.item_id;
            if let Some(revision) = revisions.get(item_id) {
                let current: bool = sqlx::query_scalar(
                    "SELECT EXISTS (
                       SELECT 1 FROM files f
                       JOIN collection_roots r ON r.id = f.root_id
                       WHERE f.module_id = ? AND f.collection_id = ?
                         AND r.root_token = ? AND f.path_rel = ?
                         AND f.size = ? AND f.mtime_unix = ?
                         AND EXISTS (
                           SELECT 1 FROM playable_source_parts psp
                           JOIN playable_sources ps ON ps.id = psp.playable_source_id
                           WHERE psp.file_id = f.id AND ps.item_id = ?
                             AND ps.expected_parts = 1 AND psp.ordinal = 1
                             AND (SELECT COUNT(*) FROM playable_source_parts all_parts
                                  WHERE all_parts.playable_source_id = ps.id) = 1
                         )
                     )",
                )
                .bind(&revision.module_id)
                .bind(&revision.collection_id)
                .bind(&revision.root_token)
                .bind(&revision.path_rel)
                .bind(revision.size as i64)
                .bind(revision.mtime_unix)
                .bind(item_id)
                .fetch_one(&mut *tx)
                .await?;
                if !current {
                    tracing::debug!(item = %item_id,
                        "segment answer stale against current source; dropped");
                    continue;
                }
            }
            // A season is minutes of analysis, long enough for a library
            // resync to delete and re-key its items. Writing this answer
            // would abort the WHOLE transaction on the foreign key and lose
            // every sibling's result with it; the vanished episode's bytes
            // will be re-read under whatever id they carry now.
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM items WHERE id = ?)")
                    .bind(item_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !exists {
                tracing::debug!(item = %item_id, "intro detection: item vanished mid-pass, answer dropped");
                continue;
            }
            anyhow::ensure!(
                !(answer.scanned && outcome.failure.is_some()),
                "segment answer cannot be both successful and failed"
            );
            if let Some(error) = &outcome.failure {
                let revision = revisions
                    .get(item_id)
                    .context("failed segment answer has no source revision")?;
                sqlx::query(
                    "INSERT INTO media_segment_failures
                       (item_id,module_id,collection_id,root_token,path_rel,size,
                        mtime_unix,detector,error,failed_at)
                     VALUES(?,?,?,?,?,?,?,?,?,unixepoch())
                     ON CONFLICT(item_id,module_id,collection_id,root_token,path_rel,
                                 size,mtime_unix,detector)
                     DO UPDATE SET error=excluded.error,failed_at=excluded.failed_at",
                )
                .bind(item_id)
                .bind(&revision.module_id)
                .bind(&revision.collection_id)
                .bind(&revision.root_token)
                .bind(&revision.path_rel)
                .bind(revision.size as i64)
                .bind(revision.mtime_unix)
                .bind(DETECTOR)
                .bind(error)
                .execute(&mut *tx)
                .await?;
                tracing::warn!(item = %item_id, path = %revision.path_rel, %error,
                    "segment failure recorded for source revision");
            }
            if answer.scanned && answer.wholesale {
                // A finished full-search episode is rewritten wholesale.
                sqlx::query("DELETE FROM media_segments WHERE item_id = ?")
                    .bind(item_id)
                    .execute(&mut *tx)
                    .await?;
            } else if answer.scanned {
                // The chapter branch: a complete statement about what the
                // FILE names, so every chapter-sourced row is replaced —
                // a re-muxed file that stopped naming its recap must not
                // keep the old cut's recap by omission — while INFERRED
                // rows survive: names do not erase what they never
                // mentioned.
                sqlx::query("DELETE FROM media_segments WHERE item_id = ? AND source = 'chapter'")
                    .bind(item_id)
                    .execute(&mut *tx)
                    .await?;
                for (kind, ..) in &answer.found {
                    sqlx::query("DELETE FROM media_segments WHERE item_id = ? AND kind = ?")
                        .bind(item_id)
                        .bind(kind)
                        .execute(&mut *tx)
                        .await?;
                }
            } else {
                // Half an answer replaces only the kinds it found: the file's
                // tail would not read, so whatever a previous pass knew about
                // the OTHER kinds is still the best available.
                for (kind, ..) in &answer.found {
                    sqlx::query("DELETE FROM media_segments WHERE item_id = ? AND kind = ?")
                        .bind(item_id)
                        .bind(kind)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            for (kind, start_ms, end_ms, source) in &answer.found {
                let (Ok(start_ms), Ok(end_ms)) = (i64::try_from(*start_ms), i64::try_from(*end_ms))
                else {
                    tracing::warn!(item = %item_id, kind,
                        "segment boundary exceeds SQLite integer range; dropped");
                    continue;
                };
                if end_ms <= start_ms {
                    continue;
                }
                sqlx::query(
                    "INSERT INTO media_segments (item_id, kind, start_ms, end_ms, source)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(item_id)
                .bind(kind)
                .bind(start_ms)
                .bind(end_ms)
                .bind(source)
                .execute(&mut *tx)
                .await?;
            }
            // A successful rendition clears only its own exact failure.
            // Other physical sources remain known-bad if this one disappears.
            if answer.scanned {
                if let Some(revision) = revisions.get(item_id) {
                    sqlx::query(
                        "DELETE FROM media_segment_failures
                          WHERE item_id=? AND detector=? AND module_id=? AND collection_id=?
                            AND root_token=? AND path_rel=? AND size=? AND mtime_unix=?",
                    )
                    .bind(item_id)
                    .bind(DETECTOR)
                    .bind(&revision.module_id)
                    .bind(&revision.collection_id)
                    .bind(&revision.root_token)
                    .bind(&revision.path_rel)
                    .bind(revision.size as i64)
                    .bind(revision.mtime_unix)
                    .execute(&mut *tx)
                    .await?;
                }
                scanned.push(item_id.clone());
                let revision = revisions.get(item_id);
                sqlx::query(
                    "INSERT INTO media_segment_scans
                       (item_id,scanned_at,detector,mtime_unix,module_id,
                        collection_id,root_token,path_rel,size,error)
                     VALUES(?,unixepoch(),?,?,?,?,?,?,?,'')
                     ON CONFLICT(item_id) DO UPDATE SET
                       scanned_at=excluded.scanned_at,detector=excluded.detector,
                       mtime_unix=excluded.mtime_unix,module_id=excluded.module_id,
                       collection_id=excluded.collection_id,root_token=excluded.root_token,
                       path_rel=excluded.path_rel,size=excluded.size,error=''",
                )
                .bind(item_id)
                .bind(DETECTOR)
                .bind(identity.get(item_id).copied().flatten())
                .bind(revision.map(|revision| revision.module_id.as_str()))
                .bind(revision.map(|revision| revision.collection_id.as_str()))
                .bind(revision.map(|revision| revision.root_token.as_str()))
                .bind(revision.map(|revision| revision.path_rel.as_str()))
                .bind(revision.map(|revision| revision.size as i64))
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(scanned)
    }
}

#[derive(Clone)]
struct SourceRevision {
    module_id: String,
    collection_id: String,
    root_token: String,
    path_rel: String,
    size: u64,
    mtime_unix: i64,
}

impl From<&crate::sessions::PartSource> for SourceRevision {
    fn from(part: &crate::sessions::PartSource) -> Self {
        Self {
            module_id: part.module_id.clone(),
            collection_id: part.collection_id.clone(),
            root_token: part.root_token.clone(),
            path_rel: part.path_rel.clone(),
            size: part.size,
            mtime_unix: part.mtime_unix,
        }
    }
}

fn result_matches_request(
    result: &kahawai_proto::v1::SegmentEpisodeResult,
    request: &kahawai_proto::v1::SegmentEpisode,
) -> Result<bool> {
    let expected_source = request.source.as_ref().context("job source missing")?;
    let source = result.source.as_ref().context("result source missing")?;
    anyhow::ensure!(
        source.root_token == expected_source.root_token
            && source.path_rel == expected_source.path_rel,
        "segment result source does not match its job"
    );
    let matches = result.observed_size == request.expected_size
        && result.observed_mtime_unix == request.expected_mtime_unix;
    anyhow::ensure!(
        matches || result.unreadable,
        "readable segment result changed source revision"
    );
    Ok(matches)
}

fn validate_result_set(
    requests: &[kahawai_proto::v1::SegmentEpisode],
    results: &[kahawai_proto::v1::SegmentEpisodeResult],
) -> Result<()> {
    anyhow::ensure!(
        results.len() == requests.len(),
        "segment result returned {} of {} episodes",
        results.len(),
        requests.len()
    );
    let expected = requests
        .iter()
        .map(|episode| episode.item_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut returned = std::collections::HashSet::new();
    for result in results {
        anyhow::ensure!(
            expected.contains(result.item_id.as_str()),
            "segment result names unknown item {}",
            result.item_id
        );
        anyhow::ensure!(
            returned.insert(result.item_id.as_str()),
            "segment result duplicates item {}",
            result.item_id
        );
    }
    anyhow::ensure!(returned == expected, "segment result omitted an item");
    Ok(())
}

fn validate_episode_segments(
    result: &kahawai_proto::v1::SegmentEpisodeResult,
    request: &kahawai_proto::v1::SegmentEpisode,
) -> Result<()> {
    anyhow::ensure!(
        !result.retryable || result.unreadable,
        "readable segment result marked retryable for {}",
        result.item_id
    );
    let mut kinds = std::collections::HashSet::new();
    for segment in &result.segments {
        anyhow::ensure!(
            kinds.insert(segment.kind.as_str()),
            "segment result duplicates {} for {}",
            segment.kind,
            result.item_id
        );
        anyhow::ensure!(
            segment.end_ms <= i64::MAX as u64,
            "segment range exceeds storage bounds for {}",
            result.item_id
        );
        anyhow::ensure!(
            kahawai_core::segments::inferred_within_bounds(
                &segment.kind,
                &segment.analyzer,
                segment.start_ms,
                segment.end_ms,
                request.duration_ms,
            ),
            "segment range is outside detector bounds for {}",
            result.item_id
        );
    }
    Ok(())
}

/// Lowers the detector's busy flag however its scope ends.
struct Lowered<'a>(&'a AtomicBool);
impl Drop for Lowered<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// What one pass concluded about an episode.
struct Answered {
    item_id: String,
    found: Vec<Boundary>,
    /// Whether the episode was READ to the end of the analysis. Unread or
    /// half-read episodes keep their found kinds but stay pending.
    scanned: bool,
    /// Whether this answer speaks for EVERY kind. The byte path does — it
    /// searched for all three — so it replaces wholesale; the chapter path
    /// answers only the kinds the names cover, and deleting the rest threw
    /// away a previously inferred recap the chapters never mentioned.
    wholesale: bool,
}

/// Everything one episode contributes to a commit. A terminal source failure
/// and useful partial boundaries are orthogonal: the failure stops this exact
/// revision being retried, while `answer.found` still improves what viewers
/// know. A successful scan is the only state that clears the exact failure.
struct EpisodeOutcome {
    answer: Answered,
    failure: Option<String>,
}

impl From<Answered> for EpisodeOutcome {
    fn from(answer: Answered) -> Self {
        Self {
            answer,
            failure: None,
        }
    }
}

/// How a season pass ended: how much was settled, and how much is waiting
/// on an absent mediahost. Values rather than errors, because the caller
/// acts on the difference — awaiting episodes are the HOST'S weather and are
/// retried when it returns, never marked failed; an outage that marched the
/// sweep through the whole pending list would otherwise disable detection
/// until a hub restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Analysis {
    /// PENDING episodes this pass newly marked scanned — progress, not
    /// throughput: the analysis re-reads whole seasons by design, and
    /// counting rewrites made the analysed counter drift from reality.
    pub scanned: usize,
    /// Episodes whose source (or mid-read host) was away.
    pub awaiting: usize,
    /// Pending episodes whose attempt TOLD us something about the season.
    /// An attempt the host died under is not one — a retry resolves it as
    /// plain offline — and the all-named branch attempts nothing at all.
    /// Zero with `awaiting` set is an outage shape: retrying next cycle is
    /// cheap and right. Attempted-and-unscanned is the season's own
    /// trouble, and the no-progress guard must see it.
    pub attempted: usize,
}

/// One stored boundary: kind, start and end in milliseconds, and which
/// analyzer said so.
type Boundary = (&'static str, u64, u64, &'static str);

/// The boundaries a file named, in the shape [`Detector::store`] writes.
fn from_chapters(named: Option<&Vec<kahawai_core::segments::Named>>) -> Vec<Boundary> {
    named
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|n| (n.kind, n.start_ms, n.end_ms, "chapter"))
        .collect()
}

fn next_pending_season(
    seasons: Vec<PendingSeason>,
    failed: &std::collections::HashMap<(String, i64), std::time::Instant>,
    awaiting_seasons: &std::collections::HashSet<(String, i64)>,
    awaiting_modules: &std::collections::HashSet<String>,
) -> Option<PendingSeason> {
    seasons.into_iter().find(|season| {
        let key = (season.series_id.clone(), season.season);
        !failed.contains_key(&key)
            && !awaiting_seasons.contains(&key)
            && (!awaiting_modules.contains(&season.module_id) || season.may_be_all_named)
    })
}

/// Seasons with at least two playable episodes and at least one never analyzed
/// by this detector generation.
///
/// Ordered by what somebody is actually watching: the most recently touched
/// season first, then the smallest. A library has hundreds of these and each is
/// minutes of reading from a mediahost, so the order decides whether the
/// buttons appear on the show you are halfway through or on the one you have
/// never opened.
pub async fn pending_seasons(db: &sqlx::SqlitePool) -> Result<Vec<PendingSeason>> {
    let rows = sqlx::query(
        "SELECT i.module_id AS module_id,
                i.parent_id AS series_id,
                COALESCE(p.title, '') AS title,
                i.season AS season,
                COUNT(*) AS episodes,
                SUM(CASE WHEN s.item_id IS NULL AND EXISTS (
                    SELECT 1
                      FROM playable_sources pending_source
                      JOIN playable_source_parts pending_part
                           ON pending_part.playable_source_id = pending_source.id
                      JOIN files pending_file ON pending_file.id = pending_part.file_id
                      LEFT JOIN collection_roots pending_root
                             ON pending_root.id = pending_file.root_id
                     WHERE pending_source.item_id = i.id
                       AND pending_source.expected_parts = 1
                       AND (SELECT COUNT(*) FROM playable_source_parts all_pending_parts
                             WHERE all_pending_parts.playable_source_id = pending_source.id) = 1
                       AND NOT EXISTS (
                           SELECT 1 FROM media_segment_failures failure
                            WHERE failure.item_id = i.id AND failure.detector = ?
                              AND failure.module_id = pending_file.module_id
                              AND failure.collection_id = pending_file.collection_id
                              AND failure.root_token = COALESCE(pending_root.root_token,'')
                              AND failure.path_rel = pending_file.path_rel
                              AND failure.size = pending_file.size
                              AND failure.mtime_unix = pending_file.mtime_unix)
                ) THEN 1 ELSE 0 END) AS pending,
                SUM(CASE WHEN EXISTS (
                    SELECT 1
                      FROM playable_sources eligible_source
                      JOIN playable_source_parts eligible_part
                           ON eligible_part.playable_source_id = eligible_source.id
                      JOIN files eligible_file ON eligible_file.id = eligible_part.file_id
                      LEFT JOIN collection_roots eligible_root
                             ON eligible_root.id = eligible_file.root_id
                     WHERE eligible_source.item_id = i.id
                       AND eligible_source.expected_parts = 1
                       AND (SELECT COUNT(*) FROM playable_source_parts all_eligible_parts
                             WHERE all_eligible_parts.playable_source_id = eligible_source.id) = 1
                       AND NOT EXISTS (
                           SELECT 1 FROM media_segment_failures failure
                            WHERE failure.item_id = i.id AND failure.detector = ?
                              AND failure.module_id = eligible_file.module_id
                              AND failure.collection_id = eligible_file.collection_id
                              AND failure.root_token = COALESCE(eligible_root.root_token,'')
                              AND failure.path_rel = eligible_file.path_rel
                              AND failure.size = eligible_file.size
                              AND failure.mtime_unix = eligible_file.mtime_unix)
                ) THEN 1 ELSE 0 END) AS eligible,
                MIN(CASE WHEN EXISTS (
                    SELECT 1
                      FROM playable_sources chapter_source
                      JOIN playable_source_parts chapter_part
                           ON chapter_part.playable_source_id = chapter_source.id
                      JOIN files chapter_file ON chapter_file.id = chapter_part.file_id
                     WHERE chapter_source.item_id = i.id
                       AND chapter_source.expected_parts = 1
                       AND (SELECT COUNT(*) FROM playable_source_parts all_chapter_parts
                             WHERE all_chapter_parts.playable_source_id = chapter_source.id) = 1
                       AND chapter_file.chapter_segments_detector = ?
                       AND (chapter_file.chapter_segment_kinds & ?) = ?
                ) THEN 1 ELSE 0 END) AS may_be_all_named,
                -- A subselect, not a join: `watch_state` is keyed on
                -- (user, item), so joining it counts an episode once per
                -- viewer who has touched it, and BOTH counts above are the
                -- counts of that fan-out. Three users on this hub was already
                -- enough to inflate two seasons by three phantom episodes,
                -- and an inflated `pending` also defeats the sweep's own
                -- did-anything-change check below: somebody marking an
                -- episode watched between two looks moves the number without
                -- any analysis having happened.
                -- MAX() twice over: the subselect is one EPISODE's newest
                -- watch, and in a GROUP BY with COUNT/SUM aggregates a bare
                -- expression is taken from an ARBITRARY row of the group —
                -- SQLite's documented behaviour, verified here — which made
                -- the season somebody was mid-way through sort with the
                -- never-opened ones. The outer MAX makes it the season's.
                MAX(COALESCE((SELECT MAX(w.updated_at) FROM watch_state w
                               WHERE w.item_id = i.id), 0)) AS watched_at
           FROM items i
           JOIN items p ON p.id = i.parent_id
           LEFT JOIN media_segment_scans s
                  ON s.item_id = i.id AND s.detector = ?
                 AND (s.mtime_unix IS NULL OR s.mtime_unix IN (
                       SELECT f.mtime_unix
                         FROM playable_sources ps
                         JOIN playable_source_parts psp
                              ON psp.playable_source_id = ps.id
                         JOIN files f ON f.id = psp.file_id
                        WHERE ps.item_id = i.id AND ps.expected_parts = 1))
          WHERE i.kind = 'episode' AND i.season IS NOT NULL
            AND EXISTS (SELECT 1 FROM playable_sources ps WHERE ps.item_id = i.id)
          GROUP BY i.module_id, i.parent_id, i.season
         HAVING episodes >= 2 AND eligible >= 2 AND pending > 0
          ORDER BY watched_at DESC, pending ASC, title",
    )
    .bind(DETECTOR)
    .bind(DETECTOR)
    .bind(DETECTOR)
    .bind(kahawai_core::segments::NAMED_COMPLETE as i64)
    .bind(kahawai_core::segments::NAMED_COMPLETE as i64)
    .bind(DETECTOR)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| PendingSeason {
            module_id: r.get("module_id"),
            may_be_all_named: r.get::<i64, _>("may_be_all_named") != 0,
            series_id: r.get("series_id"),
            title: r.get("title"),
            season: r.get("season"),
            episodes: r.get("episodes"),
            pending: r.get("pending"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_modules_are_skipped_without_losing_fresh_order() {
        let season = |module: &str, series: &str, may_be_all_named: bool| PendingSeason {
            module_id: module.into(),
            may_be_all_named,
            series_id: series.into(),
            title: series.into(),
            season: 1,
            episodes: 2,
            pending: 2,
        };
        let seasons = vec![
            season("old-host", "newly-watched", false),
            season("old-host", "named-on-disk", true),
            season("old-host", "also-old", false),
            season("ready-host", "next-ready", false),
        ];
        let awaiting_modules = std::collections::HashSet::from(["old-host".to_string()]);

        let next = next_pending_season(
            seasons,
            &Default::default(),
            &Default::default(),
            &awaiting_modules,
        )
        .unwrap();

        assert_eq!(next.series_id, "named-on-disk");
    }
    #[tokio::test]
    async fn chapter_hint_requires_normalized_names_for_every_episode() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            r#"
            INSERT INTO collections(module_id,collection_id,media_type)
              VALUES('m','c','series');
            INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
              VALUES('m','c','r','/series');
            INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
              VALUES('show','show','Show','show','show','m','c');
            INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,
                              parent_id,season,episode)
              VALUES('e1','episode','One','one','one','m','c','show',1,1),
                    ('e2','episode','Two','two','two','m','c','show',1,2);
            INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                              head_xxh3,tail_xxh3,oshash,streams_json)
              SELECT 'm','c',id,'e1.mkv',10,1,0,0,0,'{}'
                FROM collection_roots
              UNION ALL
              SELECT 'm','c',id,'e2.mkv',10,1,0,0,0,'{}'
                FROM collection_roots;
            INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                         family_key,expected_parts)
              SELECT 'm','c',id,NULL,'file:' || id,1
                FROM items WHERE kind='episode';
            INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                              ordinal,file_id)
              SELECT ps.id,'m','c',1,f.id
                FROM playable_sources ps
                JOIN files f ON f.path_rel = ps.item_id || '.mkv';
            "#,
        )
        .execute(&db)
        .await
        .unwrap();
        let info = kahawai_core::media::MediaInfo {
            duration_ms: Some(600_000),
            chapters: Some(vec![
                kahawai_core::media::Chapter {
                    start_ms: 0,
                    end_ms: Some(60_000),
                    title: Some("Intro".into()),
                },
                kahawai_core::media::Chapter {
                    start_ms: 540_000,
                    end_ms: Some(600_000),
                    title: Some("Credits".into()),
                },
            ]),
            ..Default::default()
        };
        sqlx::query("UPDATE files SET streams_json=?")
            .bind(serde_json::to_string(&info).unwrap())
            .execute(&db)
            .await
            .unwrap();
        let registry = Registry::new(db.clone(), Default::default());
        assert_eq!(registry.backfill_chapter_segments().await.unwrap(), 2);

        let pending = pending_seasons(&db).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].may_be_all_named);

        sqlx::query("UPDATE files SET chapter_segment_kinds=? WHERE path_rel='e2.mkv'")
            .bind(kahawai_core::segments::NAMED_INTRO as i64)
            .execute(&db)
            .await
            .unwrap();
        let pending = pending_seasons(&db).await.unwrap();
        assert!(!pending[0].may_be_all_named);

        sqlx::query(
            "UPDATE files
                SET chapter_segment_kinds=?, chapter_segments_detector=?
              WHERE path_rel='e2.mkv'",
        )
        .bind(kahawai_core::segments::NAMED_COMPLETE as i64)
        .bind(DETECTOR - 1)
        .execute(&db)
        .await
        .unwrap();
        let pending = pending_seasons(&db).await.unwrap();
        assert!(!pending[0].may_be_all_named);
    }

    /// The toast's whole data path: a run's outcome lands in the dispatched
    /// cells and moves the counter, or the admin page waits for ever (or
    /// lies about which run failed).
    #[test]
    fn a_dispatched_run_records_its_own_outcome() {
        let detector = Detector::new();
        assert_eq!(detector.status_counters().dispatched, 0);

        detector.record_dispatched(&Ok(Analysis {
            scanned: 2,
            awaiting: 0,
            attempted: 0,
        }));
        let status = detector.status_counters();
        assert_eq!(status.dispatched, 1);
        assert!(!status.dispatched_awaiting_host && !status.dispatched_failed);

        detector.record_dispatched(&Ok(Analysis {
            scanned: 0,
            awaiting: 3,
            attempted: 0,
        }));
        let status = detector.status_counters();
        assert_eq!(status.dispatched, 2);
        assert!(status.dispatched_awaiting_host && !status.dispatched_failed);

        detector.record_dispatched(&Err(anyhow::anyhow!("boom")));
        let status = detector.status_counters();
        assert_eq!(status.dispatched, 3);
        assert!(status.dispatched_failed && !status.dispatched_awaiting_host);
    }

    /// The chapter branch answers only the kinds the names cover: a season
    /// re-taken by the all-named path must not delete a previously inferred
    /// recap the chapters never mentioned.
    #[tokio::test]
    async fn names_do_not_erase_what_they_never_mentioned() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        let identity = std::collections::HashMap::from([("e1".to_string(), Some(1000i64))]);

        // A byte pass inferred a recap.
        detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![("recap", 0, 30_000, "blackframe")],
                    scanned: true,
                    wholesale: true,
                }],
                &identity,
            )
            .await
            .unwrap();
        // A later all-named pass names intro and credits — and nothing else.
        detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![
                        ("intro", 30_000, 90_000, "chapter"),
                        ("credits", 1_300_000, 1_400_000, "chapter"),
                    ],
                    scanned: true,
                    wholesale: false,
                }],
                &identity,
            )
            .await
            .unwrap();

        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM media_segments ORDER BY kind")
                .fetch_all(registry.db())
                .await
                .unwrap();
        assert_eq!(kinds, ["credits", "intro", "recap"], "the recap survives");
    }

    /// An item deleted mid-pass (a library resync re-keying its episodes)
    /// must not abort the whole transaction on the foreign key: its answer
    /// is dropped and every sibling's still lands.
    #[tokio::test]
    async fn a_vanished_episode_does_not_take_the_season_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        let identity = std::collections::HashMap::from([("e1".to_string(), Some(1000i64))]);

        let scanned = detector
            .store(
                &registry,
                vec![
                    Answered {
                        item_id: "gone".into(),
                        found: vec![("intro", 30_000, 90_000, "chromaprint")],
                        scanned: true,
                        wholesale: true,
                    },
                    Answered {
                        item_id: "e1".into(),
                        found: vec![("intro", 30_000, 90_000, "chromaprint")],
                        scanned: true,
                        wholesale: true,
                    },
                ],
                &identity,
            )
            .await
            .unwrap();

        assert_eq!(scanned, ["e1"], "only the surviving episode counts");
        let items: Vec<String> = sqlx::query_scalar("SELECT item_id FROM media_segments")
            .fetch_all(registry.db())
            .await
            .unwrap();
        assert_eq!(items, ["e1"], "the sibling's answer landed");
    }

    /// A re-muxed file that stopped naming its recap must not keep the old
    /// cut's recap by omission: the chapter branch replaces every
    /// chapter-sourced row, while inferred rows survive.
    #[tokio::test]
    async fn a_renaming_replaces_what_the_names_once_said() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        let identity = std::collections::HashMap::from([("e1".to_string(), Some(1000i64))]);

        // v1 named a recap; a byte pass also inferred nothing else.
        detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![
                        ("recap", 0, 30_000, "chapter"),
                        ("intro", 30_000, 90_000, "chapter"),
                        ("credits", 1_300_000, 1_400_000, "chapter"),
                    ],
                    scanned: true,
                    wholesale: false,
                }],
                &identity,
            )
            .await
            .unwrap();
        // v2 names only intro and credits, at new positions.
        detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![
                        ("intro", 10_000, 70_000, "chapter"),
                        ("credits", 1_200_000, 1_300_000, "chapter"),
                    ],
                    scanned: true,
                    wholesale: false,
                }],
                &identity,
            )
            .await
            .unwrap();

        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM media_segments ORDER BY kind")
                .fetch_all(registry.db())
                .await
                .unwrap();
        assert_eq!(kinds, ["credits", "intro"], "the unmentioned recap is gone");
    }

    /// A truncated file's opening is found on every pass; only its tail
    /// refuses to read. Half an answer keeps the found kinds, keeps a
    /// PREVIOUS pass's other kinds, and does not mark the episode scanned —
    /// dropping the intro because the credits would not read served nobody.
    #[tokio::test]
    async fn half_an_answer_keeps_the_question_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        let identity = std::collections::HashMap::from([("e1".to_string(), Some(1000i64))]);

        // An earlier, whole pass knew the credits.
        detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![("credits", 1_300_000, 1_400_000, "blackframe")],
                    scanned: true,
                    wholesale: true,
                }],
                &identity,
            )
            .await
            .unwrap();

        // The file is later replaced by a truncated download: the opening
        // reads, the tail does not.
        let outcome = detector
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![("intro", 10_000, 40_000, "chromaprint")],
                    scanned: false,
                    wholesale: false,
                }],
                &identity,
            )
            .await
            .unwrap();
        assert!(outcome.is_empty(), "nothing was marked scanned");

        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM media_segments ORDER BY kind")
                .fetch_all(registry.db())
                .await
                .unwrap();
        assert_eq!(kinds, ["credits", "intro"], "found kept, previous kept");
        let scans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans")
            .fetch_one(registry.db())
            .await
            .unwrap();
        assert_eq!(scans, 1, "only the whole pass counts as a scan");
    }

    #[tokio::test]
    async fn unsupported_detector_rejection_is_machine_readable_and_terminal() {
        let detector = Detector::new();
        let current = Arc::new(AtomicBool::new(true));
        let reply = detector.wait_for_segment_result("host", 1, current, "job");
        detector.segment_accepted(
            "host",
            1,
            kahawai_proto::v1::SegmentDetectionAccepted {
                request_id: "job".into(),
                state: "rejected".into(),
                error: "generation mismatch".into(),
                rejection: kahawai_proto::v1::SegmentDetectionRejection::UnsupportedDetector as i32,
            },
        );

        assert!(matches!(
            reply.await.unwrap(),
            Err(SegmentJobFailure::UnsupportedDetector(error))
                if error == "generation mismatch"
        ));
    }

    #[tokio::test]
    async fn a_segment_result_is_bound_to_the_requested_host() {
        let detector = Detector::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        detector.waiters.lock().insert(
            "job".into(),
            SegmentWaiter {
                module_id: "right".into(),
                generation: 1,
                current: Arc::new(AtomicBool::new(true)),
                tx,
            },
        );
        let result = kahawai_proto::v1::SegmentDetectionResult {
            request_id: "job".into(),
            detector: DETECTOR,
            ..Default::default()
        };
        detector.segment_result("wrong", 1, result.clone());
        detector.segment_result("right", 2, result.clone());
        assert!(
            detector.waiters.lock().contains_key("job"),
            "wrong host or generation consumed the waiter"
        );
        detector.segment_result("right", 1, result);
        assert!(rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn disconnect_wakes_segment_waiters() {
        let detector = Detector::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        detector.waiters.lock().insert(
            "job".into(),
            SegmentWaiter {
                module_id: "host".into(),
                generation: 2,
                current: Arc::new(AtomicBool::new(true)),
                tx,
            },
        );
        let (new_tx, new_rx) = tokio::sync::oneshot::channel();
        detector.waiters.lock().insert(
            "new-job".into(),
            SegmentWaiter {
                module_id: "host".into(),
                generation: 3,
                current: Arc::new(AtomicBool::new(true)),
                tx: new_tx,
            },
        );
        detector.segment_link_disconnected("host", 2);
        assert!(matches!(
            rx.await.unwrap(),
            Err(SegmentJobFailure::Disconnected)
        ));
        assert!(
            detector.waiters.lock().contains_key("new-job"),
            "old teardown cancelled replacement generation"
        );
        detector.segment_result(
            "host",
            3,
            kahawai_proto::v1::SegmentDetectionResult {
                request_id: "new-job".into(),
                ..Default::default()
            },
        );
        assert!(new_rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_result_for_a_replaced_or_multipart_source_is_dropped_at_commit() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
               VALUES('m','c','r','/series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');
             INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                               head_xxh3,tail_xxh3,oshash,streams_json)
               SELECT 'm','c',id,'e1.mkv',10,2,0,0,0,'{}' FROM collection_roots;
             INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                          family_key,expected_parts)
               VALUES('m','c','e1',NULL,'file:e1.mkv',1);
             INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                               ordinal,file_id)
               SELECT ps.id,'m','c',1,f.id FROM playable_sources ps,files f;",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        let identity = std::collections::HashMap::from([("e1".to_string(), Some(1))]);
        let revisions = std::collections::HashMap::from([(
            "e1".to_string(),
            SourceRevision {
                module_id: "m".into(),
                collection_id: "c".into(),
                root_token: "r".into(),
                path_rel: "e1.mkv".into(),
                size: 10,
                mtime_unix: 1,
            },
        )]);
        let answer = || {
            vec![
                Answered {
                    item_id: "e1".into(),
                    found: vec![("intro", 1_000, 2_000, "chromaprint")],
                    scanned: true,
                    wholesale: true,
                }
                .into(),
            ]
        };

        let stored = detector
            .store_guarded(&registry, answer(), &identity, &revisions)
            .await
            .unwrap();
        assert!(stored.is_empty());
        let segments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
            .fetch_one(registry.db())
            .await
            .unwrap();
        assert_eq!(segments, 0);

        sqlx::raw_sql(
            "UPDATE files SET mtime_unix = 1;
             UPDATE playable_sources SET expected_parts = 2;",
        )
        .execute(registry.db())
        .await
        .unwrap();
        let stored = detector
            .store_guarded(&registry, answer(), &identity, &revisions)
            .await
            .unwrap();
        assert!(
            stored.is_empty(),
            "a file that became one part of a multipart source was accepted"
        );

        sqlx::raw_sql(
            "UPDATE playable_sources SET expected_parts = 1;
             INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                               head_xxh3,tail_xxh3,oshash,streams_json)
               SELECT 'm','c',id,'duplicate.mkv',10,1,0,0,0,'{}' FROM collection_roots;
             INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                               ordinal,file_id)
               SELECT ps.id,'m','c',1,f.id FROM playable_sources ps
               JOIN files f ON f.path_rel = 'duplicate.mkv';",
        )
        .execute(registry.db())
        .await
        .unwrap();
        let stored = detector
            .store_guarded(&registry, answer(), &identity, &revisions)
            .await
            .unwrap();
        assert!(
            stored.is_empty(),
            "a source with duplicate ordinal-one rows was accepted"
        );
    }
    #[test]
    fn season_source_selection_uses_one_common_home() {
        let candidate = |module: &str, path: &str| SegmentCandidate {
            part: crate::sessions::PartSource {
                file_id: 0,
                module_id: module.into(),
                collection_id: "shows".into(),
                root_token: "root".into(),
                path_rel: path.into(),
                size: 1,
                mtime_unix: 1,
                base_ms: 0,
                duration_ms: 1,
            },
            info: Default::default(),
        };
        let options = vec![
            SegmentEpisodeOptions {
                item_id: "e1".into(),
                pending: true,
                // Host A's revision failed, so this episode fell through to B.
                candidates: vec![candidate("b", "e1-b.mkv")],
            },
            SegmentEpisodeOptions {
                item_id: "e2".into(),
                pending: true,
                // Playback ranking still prefers A for its sibling.
                candidates: vec![candidate("a", "e2-a.mkv"), candidate("b", "e2-b.mkv")],
            },
        ];
        assert_eq!(
            choose_segment_home(&options),
            Some(("b".into(), "shows".into())),
            "the fallback must pull its sibling onto the same mediahost"
        );

        let split = vec![
            SegmentEpisodeOptions {
                item_id: "e1".into(),
                pending: true,
                candidates: vec![candidate("a", "e1-a.mkv")],
            },
            SegmentEpisodeOptions {
                item_id: "e2".into(),
                pending: true,
                candidates: vec![candidate("b", "e2-b.mkv")],
            },
        ];
        assert_eq!(
            choose_segment_home(&split),
            None,
            "one unreadable episode per host cannot form a detector job"
        );
    }

    #[tokio::test]
    async fn unreadable_episode_is_terminal_only_for_its_exact_revision() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
               VALUES('m','c','r','/series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('show','show','Show','show','show','m','c');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,
                               parent_id,season,episode)
               VALUES('e1','episode','One','one','one','m','c','show',1,1),
                     ('e2','episode','Two','two','two','m','c','show',1,2);
             INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                               head_xxh3,tail_xxh3,oshash,streams_json)
               SELECT 'm','c',id,'e1.mkv',10,1,0,0,0,'{}' FROM collection_roots
               UNION ALL
               SELECT 'm','c',id,'e2.mkv',10,1,0,0,0,'{}' FROM collection_roots;
             INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                          family_key,expected_parts)
               SELECT 'm','c',id,NULL,'file:' || id,1 FROM items WHERE kind='episode';
             INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                               ordinal,file_id)
               SELECT ps.id,'m','c',1,f.id FROM playable_sources ps
               JOIN files f ON f.path_rel = ps.item_id || '.mkv';",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        let detector = Detector::new();
        sqlx::query(
            "INSERT INTO media_segments(item_id,kind,start_ms,end_ms,source)
             VALUES('e1','credits',50_000,60_000,'blackframe')",
        )
        .execute(registry.db())
        .await
        .unwrap();
        let identity = std::collections::HashMap::from([
            ("e1".to_string(), Some(1)),
            ("e2".to_string(), Some(1)),
        ]);
        let revision = |path_rel: &str| SourceRevision {
            module_id: "m".into(),
            collection_id: "c".into(),
            root_token: "r".into(),
            path_rel: path_rel.into(),
            size: 10,
            mtime_unix: 1,
        };
        let revisions = std::collections::HashMap::from([
            ("e1".to_string(), revision("e1.mkv")),
            ("e2".to_string(), revision("e2.mkv")),
        ]);
        let outcomes = vec![
            EpisodeOutcome {
                answer: Answered {
                    item_id: "e1".into(),
                    found: vec![("intro", 1_000, 2_000, "chromaprint")],
                    scanned: false,
                    wholesale: false,
                },
                failure: Some("decoder failed".into()),
            },
            Answered {
                item_id: "e2".into(),
                found: Vec::new(),
                scanned: true,
                wholesale: true,
            }
            .into(),
        ];
        detector
            .store_guarded(&registry, outcomes, &identity, &revisions)
            .await
            .unwrap();

        assert!(
            pending_seasons(registry.db()).await.unwrap().is_empty(),
            "current terminal failure was queued again"
        );
        let error: String =
            sqlx::query_scalar("SELECT error FROM media_segment_failures WHERE item_id='e1'")
                .fetch_one(registry.db())
                .await
                .unwrap();
        assert_eq!(error, "decoder failed");
        let kinds: Vec<String> =
            sqlx::query_scalar("SELECT kind FROM media_segments WHERE item_id='e1' ORDER BY kind")
                .fetch_all(registry.db())
                .await
                .unwrap();
        assert_eq!(
            kinds,
            ["credits", "intro"],
            "terminal failure discarded its useful partial boundary"
        );
        let scans: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM media_segment_scans WHERE item_id='e1'")
                .fetch_one(registry.db())
                .await
                .unwrap();
        assert_eq!(scans, 0, "partial failure was marked successfully scanned");
        assert!(
            pending_episode_ids(registry.db(), "show", 1)
                .await
                .unwrap()
                .is_empty(),
            "hourly season re-check queued the terminal failure"
        );
        sqlx::raw_sql(
            "INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,
                               head_xxh3,tail_xxh3,oshash,streams_json)
               SELECT 'm','c',id,'e1-alt.mkv',11,1,0,0,0,'{}' FROM collection_roots;
             INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,
                                          family_key,expected_parts)
               VALUES('m','c','e1',NULL,'file:e1-alt',1);
             INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,
                                               ordinal,file_id)
               SELECT ps.id,'m','c',1,f.id FROM playable_sources ps
               JOIN files f ON f.path_rel='e1-alt.mkv'
              WHERE ps.family_key='file:e1-alt';",
        )
        .execute(registry.db())
        .await
        .unwrap();
        assert_eq!(
            pending_episode_ids(registry.db(), "show", 1).await.unwrap(),
            ["e1"],
            "an unfailed alternative rendition was hidden by another file's failure"
        );
        let alt_revisions = std::collections::HashMap::from([(
            "e1".to_string(),
            SourceRevision {
                module_id: "m".into(),
                collection_id: "c".into(),
                root_token: "r".into(),
                path_rel: "e1-alt.mkv".into(),
                size: 11,
                mtime_unix: 1,
            },
        )]);
        detector
            .store_guarded(
                &registry,
                vec![
                    Answered {
                        item_id: "e1".into(),
                        found: Vec::new(),
                        scanned: true,
                        wholesale: true,
                    }
                    .into(),
                ],
                &identity,
                &alt_revisions,
            )
            .await
            .unwrap();
        let original_failures: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM media_segment_failures
              WHERE item_id='e1' AND path_rel='e1.mkv'",
        )
        .fetch_one(registry.db())
        .await
        .unwrap();
        assert_eq!(
            original_failures, 1,
            "success on the alternative erased the known-bad source"
        );
        sqlx::query("DELETE FROM playable_sources WHERE family_key='file:e1-alt'")
            .execute(registry.db())
            .await
            .unwrap();
        assert!(
            pending_episode_ids(registry.db(), "show", 1)
                .await
                .unwrap()
                .is_empty(),
            "removing the successful source retried the known-bad rendition"
        );

        let part = |path_rel: &str, size: u64| crate::sessions::PartSource {
            file_id: 0,
            module_id: "m".into(),
            collection_id: "c".into(),
            root_token: "r".into(),
            path_rel: path_rel.into(),
            size,
            mtime_unix: 1,
            base_ms: 0,
            duration_ms: 1,
        };
        assert!(
            segment_source_failed(
                registry.db(),
                "e1",
                &SourceRevision::from(&part("e1.mkv", 10)),
            )
            .await
            .unwrap()
        );
        assert!(
            !segment_source_failed(
                registry.db(),
                "e1",
                &SourceRevision::from(&part("e1-alt.mkv", 11)),
            )
            .await
            .unwrap()
        );

        sqlx::query("UPDATE files SET mtime_unix=2 WHERE path_rel='e1.mkv'")
            .execute(registry.db())
            .await
            .unwrap();
        let pending = pending_seasons(registry.db()).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].pending, 1, "changed revision stayed terminal");
        assert_eq!(
            pending_episode_ids(registry.db(), "show", 1).await.unwrap(),
            ["e1"]
        );
        sqlx::query("DELETE FROM media_segment_scans WHERE item_id='e2'")
            .execute(registry.db())
            .await
            .unwrap();
        detector
            .store_guarded(
                &registry,
                vec![EpisodeOutcome {
                    answer: Answered {
                        item_id: "e2".into(),
                        found: Vec::new(),
                        scanned: false,
                        wholesale: false,
                    },
                    failure: Some("decoder failed".into()),
                }],
                &identity,
                &revisions,
            )
            .await
            .unwrap();
        assert_eq!(
            pending_episode_ids(registry.db(), "show", 1).await.unwrap(),
            ["e1"],
            "the changed source itself must remain pending"
        );
        assert!(
            pending_seasons(registry.db()).await.unwrap().is_empty(),
            "a season with only one comparison source was selected forever"
        );
    }

    #[test]
    fn an_unreadable_changed_revision_does_not_reject_its_siblings() {
        let request = kahawai_proto::v1::SegmentEpisode {
            source: Some(kahawai_proto::v1::SourcePath::new("root", "episode.mkv")),
            expected_size: 10,
            expected_mtime_unix: 1,
            ..Default::default()
        };
        let mut result = kahawai_proto::v1::SegmentEpisodeResult {
            source: request.source.clone(),
            observed_size: 11,
            observed_mtime_unix: 2,
            unreadable: true,
            ..Default::default()
        };
        assert!(!result_matches_request(&result, &request).unwrap());

        result.unreadable = false;
        assert!(result_matches_request(&result, &request).is_err());

        result.unreadable = true;
        result.source.as_mut().unwrap().path_rel = "other.mkv".into();
        assert!(result_matches_request(&result, &request).is_err());
    }

    #[test]
    fn comparison_insufficiency_requires_the_protocol_four_retryable_field() {
        let incomplete = kahawai_proto::v1::SegmentEpisodeResult {
            unreadable: true,
            error: kahawai_proto::SEGMENT_COMPARISON_INSUFFICIENT.into(),
            ..Default::default()
        };
        assert!(!incomplete.retryable);
        let current = kahawai_proto::v1::SegmentEpisodeResult {
            retryable: true,
            ..incomplete
        };
        assert!(current.retryable);
    }

    #[test]
    fn result_sets_are_exact_and_ranges_stay_on_the_file_timeline() {
        let request = |id: &str| kahawai_proto::v1::SegmentEpisode {
            item_id: id.into(),
            duration_ms: 1_000,
            ..Default::default()
        };
        let result = |id: &str| kahawai_proto::v1::SegmentEpisodeResult {
            item_id: id.into(),
            ..Default::default()
        };
        let requests = [request("a"), request("b")];
        assert!(validate_result_set(&requests, &[result("a"), result("b")]).is_ok());
        assert!(validate_result_set(&requests, &[result("a"), result("a")]).is_err());
        assert!(validate_result_set(&requests, &[result("a"), result("c")]).is_err());

        let mut episode = result("a");
        episode.segments = vec![kahawai_proto::v1::DetectedSegment {
            kind: "intro".into(),
            start_ms: 10,
            end_ms: 1_001,
            analyzer: "chromaprint".into(),
        }];
        assert!(validate_episode_segments(&episode, &requests[0]).is_err());

        episode.segments[0].end_ms = 100;
        episode.segments.push(episode.segments[0].clone());
        assert!(validate_episode_segments(&episode, &requests[0]).is_err());

        let long = kahawai_proto::v1::SegmentEpisode {
            item_id: "a".into(),
            duration_ms: 600_000,
            ..Default::default()
        };
        episode.segments.truncate(1);
        let set = |episode: &mut kahawai_proto::v1::SegmentEpisodeResult,
                   kind: &str,
                   analyzer: &str,
                   start_ms,
                   end_ms| {
            episode.segments[0] = kahawai_proto::v1::DetectedSegment {
                kind: kind.into(),
                analyzer: analyzer.into(),
                start_ms,
                end_ms,
            };
        };
        set(&mut episode, "recap", "blackframe", 1, 100_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "intro", "chromaprint", 0, 123_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "credits", "blackframe", 100_000, 600_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "intro", "blackframe", 1_000, 100_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "intro", "chromaprint", 1_000, 100_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "intro", "chromaprint", 6_000, 100_000);
        assert!(validate_episode_segments(&episode, &long).is_ok());

        set(&mut episode, "credits", "blackframe", 200_000, 500_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "credits", "chromaprint", 150_000, 300_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "recap", "blackframe", 0, 14_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "intro", "chromaprint", 1_000, 9_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "intro", "chromaprint", 140_000, 150_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "credits", "chromaprint", 590_000, 600_000);
        assert!(validate_episode_segments(&episode, &long).is_err());
        set(&mut episode, "credits", "blackframe", 590_000, 600_000);
        assert!(validate_episode_segments(&episode, &long).is_err());

        set(&mut episode, "credits", "blackframe", 500_000, 600_000);
        assert!(validate_episode_segments(&episode, &long).is_ok());
        set(&mut episode, "credits", "chromaprint", 300_000, 400_000);
        assert!(validate_episode_segments(&episode, &long).is_ok());
    }

    #[tokio::test]
    async fn protocol_milliseconds_are_stored_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Arc::new(Registry::new(db, Default::default()));
        Detector::new()
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![("intro", 1_001, 2_003, "chromaprint")],
                    scanned: true,
                    wholesale: true,
                }],
                &std::collections::HashMap::new(),
            )
            .await
            .unwrap();

        let stored: (i64, i64) =
            sqlx::query_as("SELECT start_ms,end_ms FROM media_segments WHERE item_id='e1'")
                .fetch_one(registry.db())
                .await
                .unwrap();
        assert_eq!(stored, (1_001, 2_003));

        Detector::new()
            .store(
                &registry,
                vec![Answered {
                    item_id: "e1".into(),
                    found: vec![(
                        "credits",
                        i64::MAX as u64 + 1,
                        i64::MAX as u64 + 2,
                        "blackframe",
                    )],
                    scanned: false,
                    wholesale: false,
                }],
                &std::collections::HashMap::new(),
            )
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
            .fetch_one(registry.db())
            .await
            .unwrap();
        assert_eq!(count, 1, "unrepresentable boundary was stored");
    }

    #[tokio::test]
    async fn catalog_tombstone_only_removes_the_source_that_owns_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO collections(module_id,collection_id,media_type)
               VALUES('m','c','series');
             INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
               VALUES('e1','episode','One','one','one','m','c');
             INSERT INTO media_segments(item_id,kind,start_ms,end_ms,source)
               VALUES('e1','intro',1000,2000,'chromaprint');
             INSERT INTO media_segment_scans
               (item_id,detector,module_id,collection_id,root_token,path_rel,size,error)
               VALUES('e1',4,'m','c','root','current.mkv',10,'');",
        )
        .execute(&db)
        .await
        .unwrap();
        let registry = Registry::new(db, Default::default());

        remove_catalog_result(
            &registry,
            "m",
            "c",
            &crate::registry::SourcePath {
                root_token: "root".into(),
                path_rel: "old.mkv".into(),
            },
        )
        .await
        .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
            .fetch_one(registry.db())
            .await
            .unwrap();
        assert_eq!(remaining, 1, "an old rendition erased the current result");

        remove_catalog_result(
            &registry,
            "m",
            "c",
            &crate::registry::SourcePath {
                root_token: "root".into(),
                path_rel: "current.mkv".into(),
            },
        )
        .await
        .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_segments")
            .fetch_one(registry.db())
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
