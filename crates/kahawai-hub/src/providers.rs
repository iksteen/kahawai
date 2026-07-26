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
//! * `merged_metadata` — derived, one row per item. Rebuilt from the
//!   answers by rank: first non-empty value per field. Identity
//!   (`provider`/`provider_id`/`confidence`) goes to the highest-ranked
//!   answer with a non-empty id, so a reorder can move identity to a
//!   provider that had merely been describing — intended. Its anime ids
//!   and season projection are written directly and never merged.
//!   Nothing else in the hub may write this table.
//! * `provider_ranks` — precedence, one row per chain position, per
//!   media type. Editable at runtime; changing it re-merges.
//! * `enrichment_queue` — work owed: a provider that refused (ban, rate
//!   limit, transport) with `due_at` and a growing backoff.

use anyhow::Result;
use sqlx::Row;
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
    rematerialize_media_type(db, media_type).await
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
pub async fn identity_owner(db: &SqlitePool, item_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT provider FROM merged_metadata WHERE item_id = ? AND provider_id != ''")
        .bind(item_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
}

/// Recompute `merged_metadata` for one item from the stored per-provider
/// answers: chain order decides, first non-NULL wins per field. Identity
/// (provider/provider_id/confidence) goes to the highest-ranked provider
/// that actually matched something.
pub async fn materialize(db: &SqlitePool, item_id: &str, chain: &[String]) -> Result<()> {
    let rows = sqlx::query(
        "SELECT provider, provider_id, title, overview, poster_path, rating,
                premiered, original_language, genres, confidence
         FROM provider_metadata WHERE item_id = ?",
    )
    .bind(item_id)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    // Two orthogonal keys, and they must stay orthogonal: how much the
    // match is TRUSTED, then where the provider sits in the chain.
    // Collapsing both into one number made every manual match rank 0,
    // so with two manual matches the winner fell out of insertion order
    // — the chain said TMDB first and the row kept TVDB.
    let chain_pos =
        |p: &str| chain.iter().position(|c| c == chain_name(p)).unwrap_or(usize::MAX);
    let tier = |r: &&sqlx::sqlite::SqliteRow| {
        if r.get::<String, _>("provider_id").is_empty() {
            return 3; // looked, found nothing
        }
        match r.get::<String, _>("confidence").as_str() {
            MANUAL => 0,
            "weak" => 2,
            _ => 1,
        }
    };
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by_key(|r| (tier(r), chain_pos(&r.get::<String, _>("provider"))));
    // Best-trusted, then best-ranked: that is the identity, and the same
    // order decides every field below.
    let owner = *ordered.first().expect("non-empty");
    let owner_provider = owner.get::<String, _>("provider");
    // A weak match may describe the item it identified, but must not
    // donate fields to somebody else's item: an uncertain match filling
    // a synopsis for the wrong film is worse than an empty synopsis.
    let mergeable = |r: &&&sqlx::sqlite::SqliteRow| -> bool {
        r.get::<String, _>("confidence") != "weak"
            || r.get::<String, _>("provider") == owner_provider
    };
    let text = |field: &str| -> Option<String> {
        ordered
            .iter()
            .filter(mergeable)
            .find_map(|r| r.get::<Option<String>, _>(field).filter(|s| !s.is_empty()))
    };
    let rating: Option<f64> =
        ordered.iter().filter(mergeable).find_map(|r| r.get::<Option<f64>, _>("rating"));

    sqlx::query(
        "UPDATE merged_metadata SET
           provider = ?, provider_id = ?, confidence = ?,
           title = ?, overview = ?, poster_path = ?, rating = ?,
           premiered = ?, original_language = ?, genres = ?, updated_at = unixepoch()
         WHERE item_id = ?",
    )
    .bind(owner.get::<String, _>("provider"))
    .bind(owner.get::<String, _>("provider_id"))
    .bind(owner.get::<String, _>("confidence"))
    .bind(text("title"))
    .bind(text("overview"))
    .bind(text("poster_path"))
    .bind(rating)
    .bind(text("premiered"))
    .bind(text("original_language"))
    .bind(text("genres"))
    .bind(item_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Re-merge every item of one media type — what a reorder costs. Pure
/// local work: no provider is contacted.
pub async fn rematerialize_media_type(db: &SqlitePool, media_type: &str) -> Result<()> {
    let media_type = media_type_key(media_type);
    let chain = chain_in_force(db, media_type).await;
    // Must classify items exactly as `media_type_of_item` does —
    // including its default — or a reorder would silently skip the
    // items the two disagree about.
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pm.item_id FROM provider_metadata pm
         JOIN items i ON i.id = pm.item_id
         WHERE COALESCE((
                 SELECT CASE WHEN c.media_type IN ('movies','series','anime','music')
                             THEN c.media_type ELSE 'movies' END
                 FROM item_sources s
                 JOIN collections c ON (c.module_id, c.collection_id)
                                     = (s.module_id, s.collection_id)
                 WHERE s.item_id = i.id
                    OR s.item_id IN (SELECT id FROM items WHERE parent_id = i.id)
                 LIMIT 1), 'movies') = ?",
    )
    .bind(media_type)
    .fetch_all(db)
    .await?;
    for id in &ids {
        materialize(db, id, &chain).await?;
    }
    tracing::info!(media_type, items = ids.len(), ?chain, "provider order applied");
    Ok(())
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
    chain: &[String],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO provider_metadata
           (item_id, provider, provider_id, title, overview, poster_path, rating,
            premiered, original_language, genres, confidence, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, unixepoch())
         ON CONFLICT (item_id, provider) DO UPDATE SET
           provider_id = excluded.provider_id,
           title = excluded.title,
           overview = excluded.overview,
           poster_path = excluded.poster_path,
           rating = excluded.rating,
           premiered = excluded.premiered,
           original_language = excluded.original_language,
           genres = excluded.genres,
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
    .bind(confidence)
    .execute(db)
    .await?;
    // merged_metadata is keyed by item and materialized from these rows;
    // make sure the row exists before merging into it.
    sqlx::query(
        "INSERT INTO merged_metadata (item_id, provider, provider_id, confidence, updated_at)
         VALUES (?, ?, ?, ?, unixepoch())
         ON CONFLICT (item_id) DO NOTHING",
    )
    .bind(item_id)
    .bind(provider)
    .bind(provider_id)
    .bind(confidence)
    .execute(db)
    .await?;
    materialize(db, item_id, chain).await
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
}

impl Fields {
    /// Which of these are still missing from the merged row — what is
    /// left for the rest of the chain to supply.
    pub async fn gaps(db: &SqlitePool, item_id: &str) -> Result<Vec<&'static str>> {
        let row = sqlx::query(
            "SELECT title, overview, poster_path, rating, premiered,
                    original_language, genres
             FROM merged_metadata WHERE item_id = ?",
        )
        .bind(item_id)
        .fetch_optional(db)
        .await?;
        let Some(row) = row else {
            return Ok(vec![
                "title",
                "overview",
                "poster_path",
                "rating",
                "premiered",
                "original_language",
                "genres",
            ]);
        };
        let mut out = Vec::new();
        for f in ["title", "overview", "poster_path", "premiered", "original_language", "genres"] {
            if row.get::<Option<String>, _>(f).filter(|s| !s.is_empty()).is_none() {
                out.push(f);
            }
        }
        if row.get::<Option<f64>, _>("rating").is_none() {
            out.push("rating");
        }
        Ok(out)
    }
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
        let chain = chain_in_force(db, media_type).await;
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
                            store_answer(db, &item.id, name, "", "miss", Fields::default(), &chain)
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
