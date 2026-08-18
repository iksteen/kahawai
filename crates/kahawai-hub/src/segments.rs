//! Media segments: where the recap, the opening and the end credits are, so a
//! client can offer to skip them.
//!
//! Detection is `kahawai-intro`, run here rather than on the mediahost — the
//! same answer subtitle extraction already gave (see that module's doc): the
//! hub reads the bytes over a lease and satellites stay simple. A season is the
//! unit of work, because the opening is found by comparing episodes against
//! each other; one episode on its own has nothing to match.
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
//! ## `media_segment_scans` schema
//!
//! One row per episode the detector has *finished* with, found something or
//! not, with the `detector` generation that finished it. It is what stops the
//! sweep from re-analyzing a season whose episodes simply share no opening —
//! which is most films, most documentaries, and any show whose season was
//! ripped without one. Bump [`DETECTOR`] to ask every season again.
//!
//! (Migration 0062's inline comment predates the `chapter` source and its
//! checksum is frozen with the applied file; this doc is the authority.)
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
//! Accepted residual: the sweep's no-progress guard compares the pending
//! COUNT, not the set. A pass that scans one episode while a replacement
//! makes another pending again can present the same count and trip the
//! guard; the failed set's expiry retries it six hours later, so the cost
//! is latency, not loss.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::Row;
use utoipa::ToSchema;

use crate::registry::Registry;
use crate::sessions::Sessions;

/// How long a failed season stays set aside before the sweep tries it again.
const FAILED_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Detector generation. Bumping it invalidates every scan record, so the sweep
/// walks the library again with the new algorithm.
///
/// 2: the chapter-name analyzer adopted upstream's duration bounds,
/// word-boundary matching and last-credits selection — rows stored by the
/// looser matcher (a "Recapture" scene as a recap, an unbounded "Opening
/// Scene" as an intro) are wrong in ways no mtime change will re-ask about.
pub const DETECTOR: i64 = 2;

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

/// A season worth analyzing: its show, its number, and how many of its episodes
/// have never been looked at.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PendingSeason {
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
    /// Two at once double the byte plane's load for no gain: the work is
    /// bounded by reading, and the sweep would happily start a second while an
    /// administrator waits on the first.
    one_at_a_time: tokio::sync::Mutex<()>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
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

    /// Walk the library a season at a time, forever, pausing while anything is
    /// playing. Detection reads a quarter of every episode plus its tail
    /// through the byte plane; a viewer's stream comes first.
    pub fn spawn_sweep(self: &Arc<Self>, registry: Arc<Registry>, sessions: Arc<Sessions>) {
        let detector = self.clone();
        tokio::spawn(async move {
            // Let the satellites link and the scans settle.
            tokio::time::sleep(std::time::Duration::from_secs(90)).await;
            // Every season a pass has worked on, and how much of it was
            // outstanding then. A map rather than the single last season:
            // with only one remembered, TWO unfinishable seasons whose order
            // flips with watch activity alternated for ever, each pass a
            // full season read over the byte plane, and neither ever seen
            // "twice in a row".
            let mut offered: std::collections::HashMap<(String, i64), i64> = Default::default();
            // Seasons whose mediahost is away THIS cycle. Its own set, apart
            // from `failed`: an absent host is the hub's weather, so these
            // are stepped over — one host's outage must not starve the
            // seasons of every other host — and retried on the next cycle,
            // when the set clears.
            let mut awaiting_host: std::collections::HashSet<(String, i64)> = Default::default();
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
                    seasons.into_iter().find(|s| {
                        let key = (s.series_id.clone(), s.season);
                        !failed.contains_key(&key) && !awaiting_host.contains(&key)
                    })
                };
                let Some(season) = next else {
                    offered.clear();
                    // End of a cycle. Seasons that were only waiting on their
                    // host get another look next cycle — sooner when an
                    // outage is what emptied the list, since a host that
                    // comes back should not wait out the long idle sleep.
                    let outage = !awaiting_host.is_empty();
                    awaiting_host.clear();
                    tokio::time::sleep(std::time::Duration::from_secs(if outage {
                        300
                    } else {
                        900
                    }))
                    .await;
                    continue;
                };

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

    /// Analyze one season and store what it finds — or report the byte plane
    /// unreachable, which is the caller's cue to wait rather than to blame
    /// the season. `Done(0)` covers a season that was finished by another
    /// runner since it was picked, and one with too few comparable episodes.
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
        // Re-check under the lock: the sweep picks its season BEFORE blocking
        // here, and the admin route answers before its detached run begins —
        // both can hand this function a season another runner has just
        // finished, and without this the second runner re-reads the whole
        // season over the byte plane to rewrite identical rows.
        let pending_ids: Vec<String> = sqlx::query_scalar(
            "SELECT i.id FROM items i
              WHERE i.parent_id = ? AND i.season = ? AND i.kind = 'episode'
                AND EXISTS (SELECT 1 FROM playable_sources ps WHERE ps.item_id = i.id)
                AND NOT EXISTS (
                    SELECT 1 FROM media_segment_scans s
                     WHERE s.item_id = i.id AND s.detector = ?
                       AND (s.mtime_unix IS NULL OR s.mtime_unix IN (
                             -- The scan row holds the mtime of the file the
                             -- detector READ; it stays settled while that
                             -- file still exists among the item's single-part
                             -- renditions. Membership, not \"the ranked one\":
                             -- rank in SQL cannot see connectivity, and the
                             -- resolver reads the best CONNECTED rendition —
                             -- comparing against the best-ranked one re-read
                             -- the whole season in a loop for the length of
                             -- a partial outage.
                             SELECT f.mtime_unix
                               FROM playable_sources ps
                               JOIN playable_source_parts psp
                                    ON psp.playable_source_id = ps.id
                               JOIN files f ON f.id = psp.file_id
                              WHERE ps.item_id = i.id AND ps.expected_parts = 1)))",
        )
        .bind(series_id)
        .bind(season)
        .bind(DETECTOR)
        .fetch_all(registry.db())
        .await?;
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
        let handle = tokio::runtime::Handle::current();
        let mut episodes = Vec::with_capacity(rows.len());
        // What each episode's bytes were when this pass read them, recorded
        // with the scan so a replaced file asks again. Filled from the
        // RESOLVED rendition below — a MAX over every rendition's files here
        // stamped scans with a sibling's mtime and never matched the
        // predicate again.
        let mut identity: std::collections::HashMap<String, Option<i64>> = Default::default();
        // What each file says about itself, which is worth more than anything
        // we can infer and costs nothing to read.
        let mut named: std::collections::HashMap<String, Vec<kahawai_intro::chapters::Named>> =
            Default::default();
        // How many episodes had sources but no REACHABLE one right now.
        let mut awaiting = 0usize;
        // Which mediahost each analysed episode reads from, so a read that
        // died can later be classified: host gone = weather, host up = the
        // file itself.
        let mut homes: std::collections::HashMap<String, String> = Default::default();
        for row in &rows {
            let item_id: String = row.get("id");
            let title: String = row.get("title");
            // The SAME resolver playback uses — rank, completeness and
            // connectivity included — so the running time and the chapters
            // can only come from the file `open_source` will actually read.
            // Three hand-rolled SQL copies of its ordering drifted from it
            // three different ways before this.
            let (parts, info) = match sessions.source_parts(registry, &item_id).await {
                Ok(v) => v,
                Err(e) => {
                    if e.downcast_ref::<crate::sessions::SourceOffline>().is_some() {
                        awaiting += 1;
                    } else {
                        tracing::debug!(episode = %title, error = format!("{e:#}"),
                            "intro detection: no playable rendition, skipped");
                    }
                    continue;
                }
            };
            if parts.len() != 1 {
                // `open_source` opens ONE file; a CD1/CD2-only item must not
                // reach the byte path with a summed running time that maps
                // its credits window past CD1's end.
                tracing::debug!(episode = %title, parts = parts.len(),
                    "intro detection: multi-part only, skipped");
                continue;
            }
            let duration_ms = parts[0].duration_ms as i64;
            if duration_ms <= 0 {
                tracing::debug!(episode = %title, "intro detection: no running time, skipped");
                continue;
            }
            homes.insert(item_id.clone(), parts[0].module_id.clone());
            identity.insert(item_id.clone(), Some(parts[0].mtime_unix));
            named.insert(
                item_id.clone(),
                info.chapters
                    .as_deref()
                    .map(|chapters| kahawai_intro::chapters::named(chapters, duration_ms as u64))
                    .unwrap_or_default(),
            );
            // A lease per pass, opened when the analyzer actually reads and
            // dropped with the pipeline. Holding one per episode for the length
            // of a season keeps a file open on the mediahost for every episode
            // at once, and a season is minutes of work.
            //
            // The lease opens the EXACT file resolved above — not open_source
            // again. Re-resolving per read window let the ranking (or a host
            // flip) swap the rendition mid-pass, pairing this rendition's
            // stated running time and chapters with another's bytes, and the
            // wrong boundaries then froze under a matching scan row.
            let part = parts.into_iter().next().expect("len checked above");
            let (registry, sessions, handle) = (registry.clone(), sessions.clone(), handle.clone());
            let media = kahawai_intro::decode::Media::Remote {
                name: title.clone(),
                open: Arc::new(move || {
                    let (registry, sessions, part) =
                        (registry.clone(), sessions.clone(), part.clone());
                    let _guard = handle.enter();
                    tracing::debug!(path = %part.path_rel, "intro detection: opening a lease");
                    let lease = handle.block_on(async move {
                        sessions
                            .open_lease(
                                &registry,
                                &part.module_id,
                                &part.collection_id,
                                &part.root_token,
                                &part.path_rel,
                                crate::sessions::Reader::Sweep,
                            )
                            .await
                    })?;
                    tracing::debug!(size = part.size, "intro detection: lease open");
                    Ok(Box::new(crate::sessions::LeaseSource {
                        lease,
                        size: part.size,
                        handle: tokio::runtime::Handle::current(),
                        reads: 0,
                    })
                        as Box<dyn kahawai_media::remux::RemuxSource>)
                }),
            };
            episodes.push(
                kahawai_intro::season::Episode::new(media, title, duration_ms as f64 / 1000.0)
                    .with_id(item_id),
            );
        }
        // The season is the unit of ANALYSIS, but progress is per episode:
        // when none of the still-pending episodes could be resolved to a
        // readable file, this pass can settle nothing — and it says so
        // before the all-named branch too, which otherwise rewrote every
        // reachable episode's rows and inflated the analysed counter once
        // per outage cycle.
        let pending_reachable = episodes
            .iter()
            .filter(|e| pending_ids.iter().any(|id| id == &e.id))
            .count();
        if pending_reachable == 0 || episodes.len() < 2 {
            return Ok(Analysis {
                scanned: 0,
                awaiting,
                attempted: 0,
            });
        }
        // How much of THIS pass's work is new: the analysis re-reads the
        // whole season (the pairwise search needs every episode), but only
        // newly-scanned pending episodes are progress, and only they count —
        // counted from what `store` actually RECORDED, since an answer for
        // an item deleted mid-pass is dropped there and must not reset the
        // no-progress guard or move the admin counter.
        let progress =
            |stored: &[String]| stored.iter().filter(|id| pending_ids.contains(id)).count();
        // A season that names its own boundaries needs no analysis at all:
        // no fingerprints, no black-frame search, and not one byte across
        // the byte plane. The bar is every episode naming both an opening
        // and its credits — one episode short of that and the season still
        // has to be compared, since the fingerprint search works on the
        // whole season or not at all.
        let all_named = !named.is_empty()
            && episodes.iter().all(|episode| {
                let found = named
                    .get(&episode.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                found.iter().any(|n| n.kind == "intro") && found.iter().any(|n| n.kind == "credits")
            });
        if all_named {
            tracing::info!(
                series = %series_id, season, episodes = episodes.len(),
                "intro detection: the files name their own boundaries"
            );
            let boundaries = episodes
                .iter()
                .map(|episode| Answered {
                    item_id: episode.id.clone(),
                    found: from_chapters(named.get(&episode.id)),
                    scanned: true,
                    // Pending means the bytes are NEW to this detector —
                    // never scanned, or replaced since. Whatever an earlier
                    // file's analysis inferred is about bytes that no longer
                    // exist, so those episodes start clean; the rest keep
                    // their inferred rows for the kinds the names skip.
                    wholesale: pending_ids.iter().any(|id| id == &episode.id),
                })
                .collect::<Vec<_>>();
            let stored = self.store(registry, boundaries, &identity).await?;
            let scanned = progress(&stored);
            self.analyzed.fetch_add(scanned, Ordering::Relaxed);
            return Ok(Analysis {
                scanned,
                awaiting,
                // The whole point of this branch is that no byte was
                // attempted; the field says so.
                attempted: 0,
            });
        }

        let config = kahawai_intro::season::Config {
            anime,
            ..Default::default()
        };
        // Between episodes, wait out anything playing. The check belongs here
        // and not only before the season: a season is many minutes of reading,
        // and a viewer who presses play in the middle of one would otherwise
        // share the byte plane with it for the rest.
        let waiting_on = sessions.clone();
        let handle = tokio::runtime::Handle::current();
        let between = move || {
            while !waiting_on.list().is_empty() {
                tracing::debug!("intro detection: standing by while something plays");
                handle.block_on(tokio::time::sleep(std::time::Duration::from_secs(30)));
            }
        };

        let report = tokio::task::spawn_blocking(move || {
            kahawai_intro::season::analyze(&episodes, &config, &between)
        })
        .await;
        let report = report.context("detection task panicked")??;

        // A read that died under a host that has GONE is weather — counted
        // as awaiting, retried when the host returns. Under a host still
        // connected it is the file itself, which no retry fixes; those
        // episodes keep no scan row and the season's no-progress guard
        // eventually sets it aside. When every episode is a dead read on a
        // connected host, the season is a failure outright: returning
        // "awaiting" for it would re-read the whole season's bytes every
        // cycle for ever, which is the loop the failed set exists to stop.
        // One connectivity snapshot for both counts: taken twice (the second
        // after the store's await), they could disagree about a host that
        // flipped in between, costing one self-correcting no-op cycle.
        let gone_mid_read: Vec<&str> = report
            .episodes
            .iter()
            .filter(|e| {
                e.unreadable
                    && homes
                        .get(&e.id)
                        .is_some_and(|module| !registry.is_connected(module))
            })
            .map(|e| e.id.as_str())
            .collect();
        let awaiting_mid_read = gone_mid_read.len();
        let awaiting = awaiting + awaiting_mid_read;
        if report.episodes.iter().all(|e| e.unreadable)
            && !report.episodes.is_empty()
            && awaiting == 0
        {
            anyhow::bail!("no episode's bytes could be read, and the mediahost is up");
        }
        // What the analysis found, with anything the file NAMED on top of it:
        // a stated boundary beats an inferred one. An episode whose bytes
        // failed somewhere along the way keeps whatever WAS found — a
        // truncated file's opening is found on every pass, and dropping it
        // because the tail would not read served nobody — but is not marked
        // scanned: half an answer keeps the question open.
        let boundaries = report
            .episodes
            .iter()
            .map(|episode| {
                let mut found = from_chapters(named.get(&episode.id));
                let inferred = [
                    ("recap", episode.recap, "blackframe"),
                    ("intro", episode.intro, "chromaprint"),
                    (
                        "credits",
                        episode.credits,
                        episode.credits_source.unwrap_or("chromaprint"),
                    ),
                ];
                for (kind, range, source) in inferred {
                    let Some(range) = range else { continue };
                    if found.iter().any(|(k, ..)| *k == kind) {
                        continue;
                    }
                    found.push((kind, range.start, range.end, source));
                }
                Answered {
                    item_id: episode.id.clone(),
                    found,
                    scanned: !episode.unreadable,
                    wholesale: !episode.unreadable,
                }
            })
            .collect::<Vec<_>>();
        let stored = self.store(registry, boundaries, &identity).await?;
        let scanned = progress(&stored);
        self.analyzed.fetch_add(scanned, Ordering::Relaxed);
        // An attempt the host died under told us nothing about the season:
        // a retry would resolve it as plain offline (attempted zero, the
        // step-aside arm). Counting those attempts routed a mid-read outage
        // through the no-progress guard into the six-hour set-aside without
        // ever checking whether the host came back.
        let pending_gone = gone_mid_read
            .iter()
            .filter(|gone| pending_ids.iter().any(|id| id == *gone))
            .count();
        Ok(Analysis {
            scanned,
            awaiting,
            attempted: pending_reachable.saturating_sub(pending_gone),
        })
    }

    /// Write one season's boundaries and mark its episodes scanned, found
    /// something or not. Returns the episodes whose scan rows actually
    /// LANDED, so the caller counts progress from the database's answer.
    ///
    /// Both analyzers land here: the fingerprint pass and the chapter names,
    /// which are the same statement about an episode arrived at differently
    /// and must be stored identically or a client can tell them apart.
    async fn store(
        &self,
        registry: &Arc<Registry>,
        boundaries: Vec<Answered>,
        identity: &std::collections::HashMap<String, Option<i64>>,
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
        for answer in &boundaries {
            let item_id = &answer.item_id;
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
            for (kind, start, end, source) in &answer.found {
                let (start_ms, end_ms) = ((start * 1000.0) as i64, (end * 1000.0) as i64);
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
            // The scan row is the statement "this episode has been analysed",
            // and a half-read episode has not: the question stays open.
            if answer.scanned {
                scanned.push(item_id.clone());
                sqlx::query(
                    "INSERT INTO media_segment_scans (item_id, scanned_at, detector, mtime_unix)
                     VALUES (?, unixepoch(), ?, ?)
                     ON CONFLICT(item_id) DO UPDATE SET scanned_at = excluded.scanned_at,
                                                        detector = excluded.detector,
                                                        mtime_unix = excluded.mtime_unix",
                )
                .bind(item_id)
                .bind(DETECTOR)
                .bind(identity.get(item_id).copied().flatten())
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(scanned)
    }
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

/// One stored boundary: kind, start and end in seconds, and which analyzer
/// said so.
type Boundary = (&'static str, f64, f64, &'static str);

/// The boundaries a file named, in the shape [`Detector::store`] writes.
fn from_chapters(named: Option<&Vec<kahawai_intro::chapters::Named>>) -> Vec<Boundary> {
    named
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|n| (n.kind, n.start, n.end, "chapter"))
        .collect()
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
        "SELECT i.parent_id AS series_id,
                COALESCE(p.title, '') AS title,
                i.season AS season,
                COUNT(*) AS episodes,
                SUM(CASE WHEN s.item_id IS NULL THEN 1 ELSE 0 END) AS pending,
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
                       -- Same membership test as the re-check in
                       -- `analyze_season`: the read file still exists.
                       SELECT f.mtime_unix
                         FROM playable_sources ps
                         JOIN playable_source_parts psp
                              ON psp.playable_source_id = ps.id
                         JOIN files f ON f.id = psp.file_id
                        WHERE ps.item_id = i.id AND ps.expected_parts = 1))
          WHERE i.kind = 'episode' AND i.season IS NOT NULL
            AND EXISTS (SELECT 1 FROM playable_sources ps WHERE ps.item_id = i.id)
          GROUP BY i.parent_id, i.season
         HAVING episodes >= 2 AND pending > 0
          ORDER BY watched_at DESC, pending ASC, title",
    )
    .bind(DETECTOR)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| PendingSeason {
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
                    found: vec![("recap", 0.0, 30.0, "blackframe")],
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
                        ("intro", 30.0, 90.0, "chapter"),
                        ("credits", 1300.0, 1400.0, "chapter"),
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
                        found: vec![("intro", 30.0, 90.0, "chromaprint")],
                        scanned: true,
                        wholesale: true,
                    },
                    Answered {
                        item_id: "e1".into(),
                        found: vec![("intro", 30.0, 90.0, "chromaprint")],
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
                        ("recap", 0.0, 30.0, "chapter"),
                        ("intro", 30.0, 90.0, "chapter"),
                        ("credits", 1300.0, 1400.0, "chapter"),
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
                        ("intro", 10.0, 70.0, "chapter"),
                        ("credits", 1200.0, 1300.0, "chapter"),
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
                    found: vec![("credits", 1300.0, 1400.0, "blackframe")],
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
                    found: vec![("intro", 10.0, 40.0, "chromaprint")],
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
}
