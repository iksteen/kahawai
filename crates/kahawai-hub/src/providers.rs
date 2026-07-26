//! HUB-5: metadata providers behind a common trait, with the per-media-
//! type ordering declared as data. Adding a provider = one impl plus a
//! chain entry; the walker owns miss-recording and progress counting.
//!
//! Chains (first claim wins, per FIELD):
//!   anime  → anime (AniDB identity + AniList description) → tmdb → tvdb
//!   movies/series → tmdb → tvdb
//!   music  → musicbrainz
//! Local metadata (embedded tags) acts earlier, at resolution time —
//! it decides identity before enrichment ever runs (HUB-9, partial).
//!
//! # The tables, and what they mean
//!
//! This is the reference for the enrichment schema: migrations record
//! changes, this describes what the tables mean, next to the code that
//! enforces it.
//!
//! * `provider_metadata` — one row per (item, provider): what that
//!   provider said. `provider_id` is that provider's own record id,
//!   recorded whether it identified the item or merely described it. An
//!   EMPTY `provider_id` means one thing only: it looked and found
//!   nothing, always paired with `confidence = "miss"` — and that pair
//!   is how never-ask-twice is remembered.
//! * `item_match` — what a TOP-LEVEL item IS: one provider's record, and
//!   whether a human chose it. Episodes and tracks never carry one; they
//!   render through their parent's. Nothing is stored here that a read
//!   could derive, which is the point — see `resolved_metadata_sql`.
//! * `rejected_matches` — records a human refused. An item where every
//!   candidate is refused stays unassigned; a record that is NOT here may
//!   be assigned automatically, so the item recovers by itself when a
//!   provider offers something new.
//! * `anime_ids` — bridge identity (AniDB/AniList ids and the TVDB/TMDB
//!   mapping). Never side-filled: these say what the work IS, not what it
//!   looks like.
//! * `provider_ranks` — precedence, one row per chain position, per
//!   media type. Editable at runtime; changing it re-merges.
//! * `enrichment_queue` — work owed: a provider that refused (ban, rate
//!   limit, transport) with `due_at` and a growing backoff.

use anyhow::Result;
use sqlx::SqlitePool;

/// One item as the chain walker sees it. The `anime_*` fields carry the
/// selection context the anime chain needs (existing verified match,
/// pinned manual identity, current hash-verification state).
#[derive(Debug, Clone)]
pub struct ItemRef {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub artist: Option<String>,
    /// Alternative identity from the parent directory name (movies).
    pub alt: Option<kahawai_core::names::MovieGuess>,
    pub existing: Option<(String, String)>,
    pub manual: bool,
    pub known_aid: Option<u32>,
    pub identified: bool,
    /// Set by the walker: which provider owns this item's identity so
    /// far. A provider that finds someone else here must NOT re-identify
    /// the item — it may only add what is missing, bridging through
    /// mapped IDs where the chain declares that (anime, HUB-31).
    pub owner: Option<String>,
}

pub enum Outcome {
    /// Identified and persisted, with this confidence ("auto" | "weak").
    Matched(&'static str),
    /// Already correctly identified by an earlier run: identity stands.
    /// The walk continues — later providers may still fill gaps.
    Settled,
    /// Supplied missing fields without touching identity.
    Contributed,
    /// Looked, and had nothing to offer. This IS an answer: it is
    /// recorded as a miss so the data says the provider was consulted.
    Declined,
    /// Could not be consulted at all — wrong kind of item for this
    /// provider, or an anime item with no mapped id to bridge through.
    /// Recorded as nothing, because nothing was asked: should a mapping
    /// appear later, the provider is still eligible.
    NotApplicable,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Identify + persist metadata for one item, or decline.
    async fn enrich(&self, db: &SqlitePool, item: &ItemRef) -> Result<Outcome>;
    /// End-of-run teardown (session logout etc.).
    async fn finish(&self) {}
}

/// Every media type that enriches, each with its OWN chain. Movies and
/// series share a *default* order, which is not the same as sharing a
/// chain: someone may well want TVDB first for series and TMDB first for
/// films, and per-type ordering is the only way to say that.
pub const MEDIA_TYPES: [&str; 4] = ["movies", "series", "anime", "music"];

/// A collection's media type as a chain name. Anything unrecognised
/// enriches as movies rather than silently having no chain at all.
pub fn media_type_key(media_type: &str) -> &'static str {
    MEDIA_TYPES.into_iter().find(|m| *m == media_type).unwrap_or("movies")
}

/// The normative provider order for a media type — the default, and the
/// permutation whitelist a stored order must stay within.
pub fn chain_for(media_type: &str) -> &'static [&'static str] {
    match media_type_key(media_type) {
        "anime" => &["anime", "tmdb", "tvdb"],
        "music" => &["musicbrainz"],
        // movies and series: same default, separate chains.
        _ => &["tmdb", "tvdb"],
    }
}

/// The provider that is not in any chain (HUB-9).
///
/// `local` reads what is already beside the media — a cover, a Kodi
/// `.nfo` — which is the owner's own data rather than a service's guess
/// at it. Ranking it would imply there is a sensible order in which a
/// remote search beats the file on your disk, so it is not ranked: it is
/// asked before the chain and its answers sort ahead of every provider's.
///
/// The one thing that displaces it is the owner contradicting it — a
/// manual pin elsewhere, or a rejection of the record its `.nfo` claimed.
/// Then local steps aside wholesale, cover included: overriding what the
/// file says about a work is not a statement about one field of it.
pub const LOCAL: &str = "local";

/// The order in force for a media type, from `provider_ranks`.
///
/// Ordering is per MEDIA TYPE, not per library (decided 2026-07-26):
/// an item can belong to several libraries, so per-library precedence
/// has no single answer for "who owns this field", while media type is
/// fixed by the collection the item lives in.
pub async fn chain_in_force(db: &SqlitePool, media_type: &str) -> Vec<String> {
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT provider FROM provider_ranks WHERE media_type = ? ORDER BY rank",
    )
    .bind(media_type_key(media_type))
    .fetch_all(db)
    .await
    .unwrap_or_default();
    if stored.is_empty() {
        return chain_for(media_type).iter().map(|s| s.to_string()).collect();
    }
    stored
}

/// Reorder a media type's providers and re-merge everything it covers.
/// Only a permutation of the known set is accepted: dropping a provider
/// would silently disable it, adding an unknown one would do nothing.
pub async fn set_chain(db: &SqlitePool, media_type: &str, order: &[String]) -> Result<()> {
    let media_type = media_type_key(media_type);
    let known = chain_for(media_type);
    anyhow::ensure!(
        order.len() == known.len() && known.iter().all(|k| order.iter().any(|s| s == k)),
        "provider order must be a permutation of {known:?}"
    );
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM provider_ranks WHERE media_type = ?")
        .bind(media_type)
        .execute(&mut *tx)
        .await?;
    for (rank, provider) in order.iter().enumerate() {
        sqlx::query(
            "INSERT INTO provider_ranks (media_type, provider, rank) VALUES (?, ?, ?)",
        )
        .bind(media_type)
        .bind(provider)
        .bind(rank as i64)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    // Reordering re-decides ownership from answers already on disk: no
    // provider is contacted, which is what makes the knob affordable.
    assign_media_type(db, media_type).await
}

/// Drop an automatic assignment whose backing answer no longer qualifies
/// — downgraded to a miss, or since refused. Manual pins are left alone.
const DROP_STALE_ASSIGNMENT: &str = "\
DELETE FROM item_match
 WHERE manual = 0
   AND (?1 IS NULL OR item_id = ?1)
   AND (?2 IS NULL OR media_type = ?2)
   AND (NOT EXISTS (
          SELECT 1 FROM provider_metadata pm
           WHERE pm.item_id = item_match.item_id
             AND pm.provider = item_match.provider
             AND pm.provider_id <> ''
             AND pm.confidence IN ('auto', 'weak'))
        OR EXISTS (
          SELECT 1 FROM rejected_matches rj
           WHERE rj.item_id = item_match.item_id
             AND rj.provider = item_match.provider
             AND rj.provider_id = item_match.provider_id))";

/// Pick each item's assignment from the answers already on disk: a strong
/// match before a weak one, then the media type's preference order, then
/// the provider name so the outcome is deterministic. Refused records are
/// not candidates, and no candidate means NO ROW — absence is how "never
/// asked", "only misses" and "everything refused" are all expressed.
///
/// The pick is from scratch every time, which is what makes "a more
/// preferred provider that gains info replaces the automatic match" free.
/// Manual pins are declined in the ON CONFLICT, the single guard.
const PICK_ASSIGNMENT: &str = "\
INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
SELECT item_id, provider, provider_id, media_type, 0, unixepoch() FROM (
  SELECT t.item_id, t.media_type, pm.provider, pm.provider_id,
         ROW_NUMBER() OVER (PARTITION BY t.item_id ORDER BY
             CASE pm.confidence WHEN 'auto' THEN 0 WHEN 'weak' THEN 1 ELSE 2 END,
             -- HUB-9: local is unranked and first. A .nfo states what the
             -- owner says this is; nothing a search turned up outbids it.
             pm.provider <> 'local',
             COALESCE(r.rank, 99),
             pm.provider) AS n
    FROM (
      SELECT i.id AS item_id,
             COALESCE((SELECT CASE WHEN c.media_type IN ('movies','series','anime','music')
                                   THEN c.media_type ELSE 'movies' END
                         FROM item_sources s
                         JOIN collections c ON (c.module_id, c.collection_id)
                                             = (s.module_id, s.collection_id)
                        WHERE s.item_id = i.id
                           OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
                        LIMIT 1), 'movies') AS media_type
        FROM items i
       -- Top level only. Episodes and tracks follow their parent, and this
       -- filter is the only thing enforcing that.
       WHERE i.kind IN ('movie', 'show', 'album')
         AND (?1 IS NULL OR i.id = ?1)
    ) t
    JOIN provider_metadata pm ON pm.item_id = t.item_id
    LEFT JOIN provider_ranks r
           ON r.media_type = t.media_type
          AND r.provider = CASE pm.provider WHEN 'anilist' THEN 'anime' ELSE pm.provider END
   WHERE pm.confidence IN ('auto', 'weak') AND pm.provider_id <> ''
     AND (?2 IS NULL OR t.media_type = ?2)
     AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                      WHERE rj.item_id = pm.item_id
                        AND rj.provider = pm.provider
                        AND rj.provider_id = pm.provider_id)
) WHERE n = 1
ON CONFLICT (item_id) DO UPDATE SET
  provider = excluded.provider,
  provider_id = excluded.provider_id,
  media_type = excluded.media_type,
  updated_at = unixepoch()
WHERE item_match.manual = 0";

/// Re-pick one item's assignment. Local work only; no provider is asked.
pub async fn assign(db: &SqlitePool, item_id: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    reassign(&mut tx, Some(item_id), None).await?;
    tx.commit().await?;
    Ok(())
}

/// Re-pick every item of one media type — what a reorder costs. Still no
/// provider is asked: the answers are already stored.
pub async fn assign_media_type(db: &SqlitePool, media_type: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    reassign(&mut tx, None, Some(media_type_key(media_type))).await?;
    tx.commit().await?;
    tracing::info!(media_type, "assignments re-picked");
    Ok(())
}

/// Both statements must run in ONE transaction: between the delete and the
/// insert an item has no assignment, and a reader in that window would see
/// it as unmatched.
pub(crate) async fn reassign(
    tx: &mut sqlx::SqliteConnection,
    item_id: Option<&str>,
    media_type: Option<&str>,
) -> Result<()> {
    sqlx::query(DROP_STALE_ASSIGNMENT)
        .bind(item_id)
        .bind(media_type)
        .execute(&mut *tx)
        .await?;
    sqlx::query(PICK_ASSIGNMENT).bind(item_id).bind(media_type).execute(&mut *tx).await?;
    Ok(())
}

/// Backoff before retrying a provider that refused: 15 min, then an
/// hour, then 4, then a day — the same shape as the login throttle, and
/// well inside AniDB's "a ban decays after ~24 h of silence".
fn retry_delay(attempts: i64) -> i64 {
    match attempts {
        0 => 900,
        1 => 3600,
        2 => 4 * 3600,
        _ => 24 * 3600,
    }
}

/// A provider could not answer (banned, rate-limited, transport error).
/// The work is not lost — it comes back when the provider will listen.
pub async fn reschedule(db: &SqlitePool, item_id: &str, provider: &str, reason: &str) {
    let attempts: i64 = sqlx::query_scalar(
        "SELECT attempts FROM enrichment_queue WHERE item_id = ? AND provider = ?",
    )
    .bind(item_id)
    .bind(provider)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .unwrap_or(0);
    let due = retry_delay(attempts);
    let _ = sqlx::query(
        "INSERT INTO enrichment_queue (item_id, provider, due_at, attempts, reason)
         VALUES (?, ?, unixepoch() + ?, 1, ?)
         ON CONFLICT (item_id, provider) DO UPDATE SET
           due_at = unixepoch() + ?,
           attempts = enrichment_queue.attempts + 1,
           reason = excluded.reason",
    )
    .bind(item_id)
    .bind(provider)
    .bind(due)
    .bind(reason)
    .bind(due)
    .execute(db)
    .await;
    tracing::debug!(item_id, provider, retry_in_s = due, reason, "provider deferred");
}

/// This provider answered (or declined on the merits): stop tracking it.
pub async fn settled(db: &SqlitePool, item_id: &str, provider: &str) {
    let _ = sqlx::query("DELETE FROM enrichment_queue WHERE item_id = ? AND provider = ?")
        .bind(item_id)
        .bind(provider)
        .execute(db)
        .await;
}

/// Items with enrichment work that is due now.
pub async fn due_items(db: &SqlitePool, limit: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT DISTINCT item_id FROM enrichment_queue
         WHERE due_at <= unixepoch() ORDER BY due_at LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db)
    .await
    .unwrap_or_default()
}

/// A human's correction outranks every chain position (HUB-8/HUB-30a):
/// only a disagreeing canonical hash may overturn it, never a reorder.
/// It keeps its provider attribution — the confidence is what ranks.
pub const MANUAL: &str = "manual";

/// A stored answer's provider name is not always the chain entry's:
/// the anime composite records its AniList half under `anilist`, which
/// still ranks wherever `anime` sits in the chain.
fn chain_name(provider: &str) -> &str {
    match provider {
        "anilist" => "anime",
        other => other,
    }
}

/// The chain an item belongs to, from the collections its sources live
/// in. Anything that isn't anime or music enriches as movies/series.
pub async fn media_type_of_item(db: &SqlitePool, item_id: &str) -> String {
    let mt: Option<String> = sqlx::query_scalar(
        "SELECT c.media_type FROM item_sources s
         JOIN collections c ON (c.module_id, c.collection_id)
                             = (s.module_id, s.collection_id)
         WHERE s.item_id = ?1
            OR s.item_id IN (SELECT id FROM items WHERE parent_id = ?1)
         LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    media_type_key(mt.as_deref().unwrap_or_default()).to_string()
}

/// Has this chain entry already answered for this item? Compares by
/// chain name, so the anime composite's `anilist` row counts as `anime`.
async fn answered(db: &SqlitePool, item_id: &str, chain_entry: &str) -> bool {
    let providers: Vec<String> =
        sqlx::query_scalar("SELECT provider FROM provider_metadata WHERE item_id = ?")
            .bind(item_id)
            .fetch_all(db)
            .await
            .unwrap_or_default();
    providers.iter().any(|p| chain_name(p) == chain_entry)
}

/// Which provider currently owns this item's identity, if any has
/// actually matched it.
/// Which provider owns this item's identity, as a CHAIN name — the anime
/// composite's `anilist` answer reads as `anime`, which is what every
/// consumer actually compares against. Returning the raw column made
/// TmdbProvider's ("anime", …) arms unreachable, so TMDB has been
/// title-searching anime items instead of bridging through the mapping.
pub async fn identity_owner(db: &SqlitePool, item_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT provider FROM item_match WHERE item_id = ? AND provider_id <> ''",
    )
    .bind(item_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|p| chain_name(&p).to_string())
}

/// Record one provider's answer for an item, then re-merge.
///
/// `provider_id` is that provider's own record id, and a gap-filler
/// records its real one — the id is not what decides identity. An EMPTY
/// `provider_id` means only one thing: this provider looked and found
/// nothing (paired with `confidence = "miss"`), which is how the walker
/// remembers not to ask it again next run.
///
/// Identity is decided by rank in [`materialize`], not here: the
/// highest-ranked answer with a non-empty id owns it. So reordering can
/// move identity to a provider that had been merely describing — which
/// is the point of the knob, not a leak.
#[allow(clippy::too_many_arguments)]
pub async fn store_answer(
    db: &SqlitePool,
    item_id: &str,
    provider: &str,
    provider_id: &str,
    confidence: &str,
    fields: Fields,
) -> Result<()> {
    // Write FIRST, then read: a deferred transaction that reads before it
    // writes hits SQLITE_BUSY when it tries to upgrade, and up to seven
    // writers run concurrently here. The upsert taking the write lock is
    // what makes the re-pick below see every committed answer.
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO provider_metadata
           (item_id, provider, provider_id, title, overview, poster_path, rating,
            premiered, original_language, genres, cast_json, confidence, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, unixepoch())
         ON CONFLICT (item_id, provider) DO UPDATE SET
           provider_id = excluded.provider_id,
           title = excluded.title,
           overview = excluded.overview,
           poster_path = excluded.poster_path,
           rating = excluded.rating,
           premiered = excluded.premiered,
           original_language = excluded.original_language,
           genres = excluded.genres,
           -- Cast arrives later than the match (one details request fills
           -- language, genres and cast at once), so a plain overwrite would
           -- erase it every time this row is re-answered. Keep it only
           -- while the answer still describes the SAME record.
           cast_json = CASE
               WHEN excluded.provider_id <> provider_metadata.provider_id
                   THEN excluded.cast_json
               ELSE COALESCE(excluded.cast_json, provider_metadata.cast_json)
           END,
           confidence = excluded.confidence,
           updated_at = excluded.updated_at",
    )
    .bind(item_id)
    .bind(provider)
    .bind(provider_id)
    .bind(&fields.title)
    .bind(&fields.overview)
    .bind(&fields.poster_path)
    .bind(fields.rating)
    .bind(&fields.premiered)
    .bind(&fields.original_language)
    .bind(&fields.genres)
    .bind(&fields.cast_json)
    .bind(confidence)
    .execute(&mut *tx)
    .await?;
    // Re-pick in the SAME transaction: between dropping a stale assignment
    // and inserting the new one the item has none, and a reader in that
    // window would see it unmatched.
    reassign(&mut tx, Some(item_id), None).await?;
    tx.commit().await?;
    Ok(())
}

/// The user's choice: THIS provider's record is what the item is. Stored
/// as that provider's answer — a human match is strong by definition —
/// plus a pinned assignment automatic picking never touches, and the
/// record stops being refused if it had been.
pub async fn assign_manual(
    db: &SqlitePool,
    item_id: &str,
    provider: &str,
    provider_id: &str,
    fields: Fields,
) -> Result<()> {
    store_answer(db, item_id, provider, provider_id, "auto", fields).await?;
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM rejected_matches
          WHERE item_id = ? AND provider = ? AND provider_id = ?",
    )
    .bind(item_id)
    .bind(provider)
    .bind(provider_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
         VALUES (?, ?, ?, ?, 1, unixepoch())
         ON CONFLICT (item_id) DO UPDATE SET
           provider = excluded.provider,
           provider_id = excluded.provider_id,
           manual = 1,
           updated_at = unixepoch()",
    )
    .bind(item_id)
    .bind(provider)
    .bind(provider_id)
    .bind(media_type_of_item(db, item_id).await)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Promote the current automatic assignment to a human decision. Touches
/// nothing else — the answer it points at is already on file.
pub async fn confirm_assignment(db: &SqlitePool, item_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE item_match SET manual = 1, updated_at = unixepoch()
          WHERE item_id = ? AND provider_id <> ''",
    )
    .bind(item_id)
    .execute(db)
    .await?;
    Ok(())
}

/// "There is currently no correct record." Every record this item holds is
/// remembered as refused and the assignment goes, but the ANSWERS stay:
/// deleting them would make the next run re-ask every provider, AniDB
/// included, for one click in the UI. The refused set is what lets the
/// item recover by itself — a record that is not in it may be assigned
/// automatically, so a provider offering something new is picked up.
pub async fn reject_matches(db: &SqlitePool, item_id: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO rejected_matches (item_id, provider, provider_id, rejected_at)
         SELECT item_id, provider, provider_id, unixepoch() FROM provider_metadata
          WHERE item_id = ? AND provider_id <> ''
         ON CONFLICT (item_id, provider, provider_id) DO NOTHING",
    )
    .bind(item_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM item_match WHERE item_id = ?")
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    // Come back to the refused providers after the usual cooldown, so
    // "something new" is actually checked for without hammering anyone.
    sqlx::query(
        "INSERT INTO enrichment_queue (item_id, provider, due_at, reason)
         SELECT item_id, provider, unixepoch() + ?, 'match rejected' FROM provider_metadata
          WHERE item_id = ? AND provider_id <> ''
         ON CONFLICT (item_id, provider) DO UPDATE SET
           due_at = excluded.due_at, reason = excluded.reason",
    )
    .bind(retry_delay(3))
    .bind(item_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// What a provider has to say about an item's description.
#[derive(Debug, Default, Clone)]
pub struct Fields {
    pub title: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub rating: Option<f64>,
    pub premiered: Option<String>,
    pub original_language: Option<String>,
    /// JSON array, as stored.
    pub genres: Option<String>,
    /// JSON array of {name, character}, billing order, as stored.
    pub cast_json: Option<String>,
}

/// Providers instantiated for one enrichment run (credentials resolved,
/// sessions opened). Absent = unconfigured; the walker skips it.
#[derive(Default)]
pub struct ProviderSet {
    providers: Vec<Box<dyn Provider>>,
}

impl ProviderSet {
    pub fn add(&mut self, p: Box<dyn Provider>) {
        self.providers.push(p);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Provider> {
        self.providers.iter().find(|p| p.name() == name).map(|p| p.as_ref())
    }

    pub async fn finish(&self) {
        for p in &self.providers {
            p.finish().await;
        }
    }

    /// Walk the media type's chain for one item. Returns the outcome
    /// confidence, or None when every provider declined (the caller
    /// records the miss).
    ///
    /// HUB-5 first-claim-wins is per FIELD, so a match no longer ends
    /// the walk: the chain continues while anything is still missing,
    /// and each provider only gets to supply what nobody above it did.
    /// The walk stops early the moment the row is complete — which is
    /// what keeps the common case (TMDB answers everything) at exactly
    /// one provider's worth of traffic.
    pub async fn run_chain(
        &self,
        media_type: &str,
        db: &SqlitePool,
        item: &ItemRef,
    ) -> Option<&'static str> {
        // HUB-9: local is asked BEFORE the chain and is not in it, so
        // there is no ordering in which the owner's own files end up
        // behind a search result. See LOCAL.
        let mut chain = vec![LOCAL.to_string()];
        chain.extend(chain_in_force(db, media_type).await);
        let mut result: Option<&'static str> = None;
        for name in &chain {
            let Some(p) = self.get(name) else { continue };
            // Every provider gets asked, whatever the order says and
            // whether or not the row already looks complete: their
            // answers are stored separately, so precedence stays a local
            // decision that can be re-taken for free. Asking only until
            // the row filled up made the ranking a lottery over who
            // happened to be consulted first.
            //
            // Bounded by never-ask-twice: one request per (item,
            // provider), ever — a recorded miss counts as an answer. The
            // identity owner is exempt so it can re-verify (a late ED2K
            // result may disagree, HUB-30a).
            let owner = identity_owner(db, &item.id).await;
            let owns = owner.as_deref().map(chain_name) == Some(name.as_str());
            if !owns && answered(db, &item.id, name).await {
                continue;
            }
            let mut ctx = item.clone();
            ctx.owner = owner;
            match p.enrich(db, &ctx).await {
                Ok(Outcome::Matched(conf)) => {
                    settled(db, &item.id, name).await;
                    result = result.or(Some(conf));
                }
                Ok(Outcome::Settled) | Ok(Outcome::Contributed) => {
                    settled(db, &item.id, name).await;
                    result = result.or(Some("settled"));
                }
                Ok(Outcome::Declined) => {
                    // Record the miss only if this provider has nothing
                    // on file. A decline must never overwrite an existing
                    // answer: the identity owner is re-asked every run to
                    // re-verify, and a search that comes back empty this
                    // time would otherwise erase what it — or a HUMAN —
                    // established earlier. It erased a manual match once.
                    if !answered(db, &item.id, name).await {
                        let _ =
                            store_answer(db, &item.id, name, "", "miss", Fields::default())
                                .await;
                    }
                    settled(db, &item.id, name).await;
                }
                Ok(Outcome::NotApplicable) => settled(db, &item.id, name).await,
                Err(e) => {
                    let reason = format!("{e:#}");
                    tracing::warn!(provider = %name, title = %item.title, error = %reason,
                        "provider unavailable; rescheduling");
                    reschedule(db, &item.id, name, &reason).await;
                }
            }
        }
        result
    }
}

// ---------- the resolved view (HUB-5 read model) ----------

/// Fields resolved per read: `(column, "is present" test, value)`. The
/// projection pair is deliberately decided by ONE test — a season from
/// TVDB with an episode number from TMDB would be nonsense.
const RESOLVED_FIELDS: [(&str, &str, &str); 10] = [
    ("title", "NULLIF(pm.title, '') IS NOT NULL", "NULLIF(pm.title, '')"),
    ("overview", "NULLIF(pm.overview, '') IS NOT NULL", "NULLIF(pm.overview, '')"),
    ("poster_path", "NULLIF(pm.poster_path, '') IS NOT NULL", "NULLIF(pm.poster_path, '')"),
    ("premiered", "NULLIF(pm.premiered, '') IS NOT NULL", "NULLIF(pm.premiered, '')"),
    (
        "original_language",
        "NULLIF(pm.original_language, '') IS NOT NULL",
        "NULLIF(pm.original_language, '')",
    ),
    ("genres", "NULLIF(pm.genres, '') IS NOT NULL", "NULLIF(pm.genres, '')"),
    ("cast_json", "NULLIF(pm.cast_json, '') IS NOT NULL", "NULLIF(pm.cast_json, '')"),
    ("rating", "pm.rating IS NOT NULL", "pm.rating"),
    ("proj_season", "pm.proj_season IS NOT NULL", "pm.proj_season"),
    ("proj_episode", "pm.proj_season IS NOT NULL", "pm.proj_episode"),
];

/// The read model: an item's fields resolved from the providers' answers,
/// the assigned provider first and then the media type's preference order.
///
/// Installed at startup rather than by a migration: the resolution rule is
/// the thing being experimented with, and a migration is an immutable log.
///
/// TWO RULES, both measured, both silent when broken:
///
///  * **No JOIN in the view's FROM.** SQLite will not flatten a subquery
///    containing a join into the right operand of a LEFT JOIN, and every
///    read site joins this view that way. With a join here the whole view
///    materialises: `item_children` went 0.6 ms → 45 ms, still returning
///    correct rows. Scalar subqueries stay flattenable — the check is that
///    EXPLAIN QUERY PLAN for a per-item read shows no MATERIALIZE.
///  * **Never join it on a non-key column.** A full scan is ~400 ms; that
///    is why the anime ids live in their own table.
pub fn resolved_metadata_sql() -> String {
    // Every answer with the priority already attached, so the resolver
    // below never has to reach out of its own subquery: the bundled SQLite
    // (libsqlite3-sys, older than the CLI's) will not resolve a reference
    // two levels out, and doing it here keeps one join in one place.
    let priority = "\
DROP VIEW IF EXISTS resolved_metadata;
DROP VIEW IF EXISTS answer_priority;
CREATE VIEW answer_priority AS
SELECT i.id AS item_id, pm.provider, pm.provider_id, pm.confidence,
       pm.title, pm.overview, pm.poster_path, pm.rating, pm.premiered,
       pm.original_language, pm.genres, pm.cast_json, pm.proj_season, pm.proj_episode,
       pm.updated_at,
       -- The effective assignment: this item's own, else its parent's.
       -- Episodes and tracks never carry one, so they render as their show
       -- or album does.
       COALESCE(own.provider, par.provider) AS chosen,
       COALESCE(own.manual, par.manual, 0) AS manual,
       MAX(COALESCE(own.updated_at, 0), COALESCE(par.updated_at, 0)) AS assigned_at,
       -- 0 sorts first: the assigned provider, then the preference order.
       (pm.provider IS NOT COALESCE(own.provider, par.provider)) AS not_chosen,
       COALESCE(r.rank, 99) AS rank
  FROM items i
  JOIN provider_metadata pm ON pm.item_id = i.id
  LEFT JOIN item_match own ON own.item_id = i.id
  LEFT JOIN item_match par ON par.item_id = i.parent_id
  LEFT JOIN provider_ranks r
         ON r.media_type = COALESCE(own.media_type, par.media_type)
        AND r.provider = CASE WHEN pm.provider = 'anilist' THEN 'anime'
                              ELSE pm.provider END;
";
    // Identity is the assignment's to state, so the chosen answer first.
    let order = "ORDER BY ap.not_chosen, ap.rank LIMIT 1";
    // Fields come from the assigned record first and every OTHER record
    // after it, in chain order: a rival record describes some other
    // service's idea of this work, and letting it redescribe the assigned
    // one is how a wrong title used to appear under a right match.
    //
    // Before all of them sits `local`, which is unranked because there is
    // no order in which a search result should beat the file on the
    // owner's disk (HUB-9). It leads on the first key, not by holding
    // rank 0 — a rank is a knob, and this one had no sensible setting.
    //
    // It leads only while its record stands, though: the group key above
    // is what drops local behind the chain once the owner has pinned
    // somebody else or rejected what the .nfo claimed. A cover carries no
    // record, so it is never in that group and never displaced.
    let field_order = "ORDER BY (ap.provider_id <> '' AND ap.not_chosen), \
                       (ap.provider <> 'local'), ap.rank, ap.not_chosen LIMIT 1";
    let fields: String = RESOLVED_FIELDS
        .iter()
        .map(|(name, present, value)| {
            let present = present.replace("pm.", "ap.");
            let value = value.replace("pm.", "ap.");
            format!(
                "  (SELECT {value} FROM answer_priority ap
     WHERE ap.item_id = i.id AND {present}
       -- a weak answer describes only the item it was chosen for
       AND (ap.confidence <> 'weak' OR ap.not_chosen = 0)
     {field_order}) AS {name},\n"
            )
        })
        .collect();
    format!(
        "{priority}
CREATE VIEW resolved_metadata AS
SELECT i.id AS item_id,
  COALESCE((SELECT ap.manual FROM answer_priority ap
             WHERE ap.item_id = i.id LIMIT 1), 0) AS manual,
  (SELECT ap.provider FROM answer_priority ap
    WHERE ap.item_id = i.id AND ap.provider_id <> '' {order}) AS provider,
  (SELECT ap.provider_id FROM answer_priority ap
    WHERE ap.item_id = i.id AND ap.provider_id <> '' {order}) AS provider_id,
  -- The strength the API reports. A human decision comes only from the
  -- assignment. 'rejected' means every record this item holds was refused,
  -- 'miss' that somebody looked and nobody had anything.
  CASE WHEN (SELECT ap.manual FROM answer_priority ap
              WHERE ap.item_id = i.id AND ap.not_chosen = 0 LIMIT 1) = 1 THEN 'manual'
       WHEN EXISTS (SELECT 1 FROM answer_priority ap
                     WHERE ap.item_id = i.id AND ap.not_chosen = 0
                       AND ap.provider_id <> '')
         THEN (SELECT ap.confidence FROM answer_priority ap
                WHERE ap.item_id = i.id AND ap.not_chosen = 0 LIMIT 1)
       WHEN EXISTS (SELECT 1 FROM rejected_matches rj WHERE rj.item_id = i.id)
         THEN 'rejected'
       ELSE (SELECT 'miss' FROM answer_priority ap
              WHERE ap.item_id = i.id LIMIT 1) END AS confidence,
{fields}  -- Posters are cached for a day, so this must move when an answer
  -- lands AND when the assignment changes: picking a different provider
  -- changes no answer's updated_at.
  (SELECT NULLIF(MAX(MAX(ap.updated_at), MAX(ap.assigned_at)), 0)
     FROM answer_priority ap WHERE ap.item_id = i.id) AS updated_at
FROM items i"
    )
}
