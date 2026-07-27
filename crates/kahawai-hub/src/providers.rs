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
//! # Inputs and derivations
//!
//! Every table here is one of two things, and which one it is decides who
//! writes it.
//!
//! **Inputs** are facts nothing can recompute: what a provider answered,
//! the order the owner wants providers tried in, which records they
//! refused, which record they pinned, where an item's files live. Code
//! writes these.
//!
//! **Derivations** are functions of the inputs, stored only because a
//! read cannot afford to compute them — browse cannot sort or filter on a
//! value resolved per read and still answer in 200 ms. `item_match`
//! (which record an item IS), `items.sort_title` (0035) and
//! `item_libraries` (0036) are all of this kind. **Nothing in this crate
//! writes them.** Triggers do, on every write to the inputs they depend
//! on:
//!
//! > Write `provider_metadata`, `provider_ranks`, `rejected_matches`,
//! > `manual_match`, `item_sources`, `collections`, `items` or
//! > `library_collections`, and everything derived from them is already
//! > correct. There is nothing to remember and nothing to call.
//!
//! Storing what a read can derive is what `merged_metadata` did, and it
//! was wrong for exactly one reason: it went stale, because staying
//! correct depended on someone remembering to call something. Triggers
//! remove the someone. That is why `set_chain` is one INSERT and
//! `store_answer` is one upsert — reordering the chain re-decides
//! ownership of a whole media type, and neither function knows it.
//!
//! It also means a derivation must not carry an input as a column. The
//! human pin used to be `item_match.manual`, which forced the pick to
//! recompute *around* rows it must not touch — three separate
//! `manual = 0` predicates, each a chance to get it wrong. It lives in
//! `manual_match` now, and the pick recomputes every row from scratch
//! with the pin as its first sort key.
//!
//! Three things would break this, so avoid all three:
//!
//! * `INSERT OR REPLACE` into an input table. SQLite does not fire DELETE
//!   triggers for REPLACE unless `recursive_triggers` is on, and it is
//!   off. Use `ON CONFLICT ... DO UPDATE`, as `store_answer` does.
//! * Bulk-loading an input with triggers disabled or via a path that
//!   bypasses them, then assuming a later pass will fix it up.
//! * Writing a derivation directly, "just this once", to correct
//!   something. The next input write recomputes it and your correction is
//!   gone — if it disagreed with the pick, fix the pick.
//!
//! Within one transaction the last input write must cover every item
//! whose inputs changed; intermediate states are invisible (WAL readers
//! see the pre-transaction snapshot) and each recompute is total, so the
//! last one wins.
//!
//! `tests/item_match_derived.rs` and `tests/sort_title.rs` are the
//! guards: each re-derives the truth from scratch after every kind of
//! write, including raw SQL, and fails if the stored answer disagrees.
//!
//! # The tables, and what they mean
//!
//! This is the reference for the enrichment schema: migrations record
//! changes, this describes what the tables mean, next to the code that
//! enforces it.
//!
//! Inputs:
//!
//! * `provider_metadata` — one row per (item, provider): what that
//!   provider said. `provider_id` is that provider's own record id,
//!   recorded whether it identified the item or merely described it. An
//!   EMPTY `provider_id` means one thing only: it looked and found
//!   nothing, always paired with `confidence = "miss"` — and that pair
//!   is how never-ask-twice is remembered.
//! * `provider_ranks` — precedence, one row per chain position, per
//!   media type. Editable at runtime; changing it re-picks.
//! * `rejected_matches` — records a human refused. An item where every
//!   candidate is refused stays unassigned; a record that is NOT here may
//!   be assigned automatically, so the item recovers by itself when a
//!   provider offers something new.
//! * `manual_match` — the record a human pinned, at most one per item.
//!   Stateless intent: it names a record, and every pick re-applies it.
//!   A pin whose record no longer exists simply does not win, and starts
//!   winning again if the record comes back.
//! * `anime_ids` — bridge identity (AniDB/AniList ids and the TVDB/TMDB
//!   mapping). Never side-filled: these say what the work IS, not what it
//!   looks like.
//! * `enrichment_queue` — work owed: a provider that refused (ban, rate
//!   limit, transport) with `due_at` and a growing backoff.
//!
//! Derived — read these, never write them:
//!
//! * `item_match` — what a TOP-LEVEL item IS: one provider's record, plus
//!   `manual`, which reads 1 when the winner is the pin. Episodes and
//!   tracks never carry one; they render through their parent's. Absence
//!   is meaningful and is the only representation of "unmatched": "never
//!   asked", "only misses" and "everything refused" are all no row.
//!   Maintained by [`repick_triggers`].

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
    // ONE statement, and only for the positions that actually moved.
    // Delete-then-reinsert would have written every row twice and left a
    // window in between where the media type had no ranks at all — and
    // once the pick is trigger-driven, each of those writes is a
    // catalogue-wide recompute against ranks that were never real.
    sqlx::query(
        "INSERT INTO provider_ranks (media_type, provider, rank)
         -- `WHERE true` is not decoration: after a SELECT with a FROM,
         -- SQLite cannot tell ON CONFLICT from a join's ON without it.
         SELECT ?1, j.value, j.key FROM json_each(?2) j WHERE true
         ON CONFLICT (media_type, provider) DO UPDATE SET rank = excluded.rank
          WHERE provider_ranks.rank IS NOT excluded.rank",
    )
    .bind(media_type)
    .bind(serde_json::to_string(order)?)
    .execute(db)
    .await?;
    // And that is the whole function. Reordering re-decides ownership of
    // every item of this media type from answers already on disk — no
    // provider is contacted, which is what makes the knob affordable —
    // but re-deciding it is the database's job, not this one's. See the
    // module doc, "Derived state is maintained by the DATABASE".
    Ok(())
}

/// Drop an assignment whose backing answer no longer qualifies —
/// downgraded to a miss, or since refused.
///
/// This applies to pinned assignments too. A pin lives in `manual_match`
/// and says which RECORD the owner chose; it cannot keep an `item_match`
/// row alive after the answer behind it is gone, or the table would hold
/// a match no provider still offers. If the answer comes back, so does
/// the pin — it is stateless intent, re-applied by every pick.
const DROP_STALE_ASSIGNMENT: &str = "\
DELETE FROM item_match
 WHERE (?1 IS NULL OR item_id = ?1)
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
/// The pick is from scratch every time — EVERY row, pins included — which
/// is what makes "a more preferred provider that gains info replaces the
/// automatic match" free.
///
/// A human pin is not an exception to the pick, it is its first sort key.
/// That is the whole reason `manual_match` exists: when the pin was a
/// column on the result, three separate `manual = 0` predicates had to
/// remember not to touch it, and each was a chance to get it wrong.
const PICK_ASSIGNMENT: &str = "\
INSERT INTO item_match (item_id, provider, provider_id, media_type, manual, updated_at)
SELECT item_id, provider, provider_id, media_type, pinned, unixepoch() FROM (
  SELECT t.item_id, t.media_type, pm.provider, pm.provider_id,
         mm.item_id IS NOT NULL AS pinned,
         ROW_NUMBER() OVER (PARTITION BY t.item_id ORDER BY
             -- The owner naming a record outranks everything, local
             -- included: a .nfo says what the file is, a pin says the
             -- .nfo is wrong about it.
             mm.item_id IS NULL,
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
    -- One row per item at most (item_id is its primary key), so this
    -- cannot multiply candidates.
    LEFT JOIN manual_match mm ON mm.item_id = pm.item_id
                             AND mm.provider = pm.provider
                             AND mm.provider_id = pm.provider_id
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
  manual = excluded.manual,
  updated_at = unixepoch()
-- Only when the answer actually moved. `updated_at` is what the browser
-- caches artwork against for a day, and an UPDATE fires the sort_title
-- trigger whether or not it changed a byte — so a pick that re-decides
-- the same thing must be a no-op, not a write.
WHERE item_match.provider    IS NOT excluded.provider
   OR item_match.provider_id IS NOT excluded.provider_id
   OR item_match.media_type  IS NOT excluded.media_type
   OR item_match.manual      IS NOT excluded.manual";

/// The recompute, as a trigger body: the same two statements
/// [`reassign`] runs, with each `(?N IS NULL OR col = ?N)` filter
/// replaced by a plain equality against an expression the trigger has in
/// scope, or by `1` when this trigger recomputes everything.
///
/// The whole clause is replaced, not just the placeholder, and that is
/// the entire performance story. A bound parameter has no value when the
/// plan is chosen, so `(?1 IS NULL OR item_id = ?1)` is the only way to
/// write an optional filter — but SQLite then cannot use an index for
/// either branch, and measured `SCAN item_match` plus a kind-scan of
/// `items` per invocation. In a trigger the filter IS known when the
/// statement is compiled, so it can be an equality, and the same work
/// becomes two index lookups. At 50k items that is the difference
/// between a rescan finishing and a rescan being quadratic.
///
/// `None` means no filter at all. An expression that could evaluate to
/// NULL would silently match nothing instead — every caller below passes
/// a `NOT NULL` column or a `COALESCE` ending in one.
fn repick_body(item: Option<&str>, media_type: Option<&str>) -> String {
    let eq = |col: &str, value: Option<&str>| match value {
        Some(v) => format!("{col} = {v}"),
        None => "1".to_string(),
    };
    let body = format!(
        "{}; {};",
        DROP_STALE_ASSIGNMENT
            .replace("(?1 IS NULL OR item_id = ?1)", &eq("item_id", item))
            .replace("(?2 IS NULL OR media_type = ?2)", &eq("media_type", media_type)),
        PICK_ASSIGNMENT
            .replace("(?1 IS NULL OR i.id = ?1)", &eq("i.id", item))
            .replace("(?2 IS NULL OR t.media_type = ?2)", &eq("t.media_type", media_type))
    );
    // Those replacements match on the filters' exact text. If one is
    // reworded and a `.replace` silently stops matching, the placeholder
    // survives — and a placeholder that reaches SQLite as part of a
    // trigger is a scan that nobody notices until the catalogue is big.
    assert!(!body.contains('?'), "a repick filter was not substituted:\n{body}");
    body
}

/// Every write that can change which record an item is, and the trigger
/// that re-derives `item_match` from it.
///
/// Returned rather than written into a migration for the same reason the
/// views are: these DERIVE, and a derivation's definition is free to
/// change, where a migration is an immutable log of changes to what is
/// stored. [`crate::db::install_derived`] handles the one hazard that
/// creates — a definition left behind by an older binary.
///
/// Two rules run through the list. **Column-scoping**: an UPDATE trigger
/// names only the columns the pick actually reads, because the enrichment
/// run's own `UPDATE provider_metadata` statements (details backfill,
/// episode projection) touch none of them and would otherwise cost a
/// recompute each, thousands per run. **WHEN guards**: the cascade from
/// `DELETE FROM items` and the bulk `item_sources` insert on every scan
/// are the two hottest paths in the system, and both can skip the body
/// entirely.
pub fn repick_triggers() -> Vec<(String, String)> {
    // The item an `item_sources` row decides for: episodes and tracks
    // give their PARENT its media type, never themselves one.
    let src_item = |side: &str| {
        format!(
            "COALESCE((SELECT parent_id FROM items WHERE id = {side}.item_id), {side}.item_id)"
        )
    };
    // A scan inserts item_sources in bulk for items nothing has enriched
    // yet, where the pick has nothing to find. Skip those outright.
    let has_answers = |side: &str| {
        format!(
            "EXISTS (SELECT 1 FROM provider_metadata pm WHERE pm.item_id = {})",
            src_item(side)
        )
    };
    // An FK cascade from `DELETE FROM items` fires these with the parent
    // already gone. The pick would find nothing; don't ask it to look.
    let survives = "EXISTS (SELECT 1 FROM items WHERE id = OLD.item_id)";

    let mut out = Vec::new();
    let mut add = |name: &str, event: &str, table: &str, when: Option<String>, body: String| {
        let when = when.map(|w| format!("\nWHEN {w}")).unwrap_or_default();
        out.push((
            name.to_string(),
            format!("CREATE TRIGGER {name} AFTER {event} ON {table}{when}\nBEGIN\n{body}\nEND"),
        ));
    };

    // Answers. `title` is NOT in the update list on purpose: it decides
    // the sort key (0035's own triggers), never which record an item is.
    let by_new_item = repick_body(Some("NEW.item_id"), None);
    let by_old_item = repick_body(Some("OLD.item_id"), None);
    add("repick_answer_ins", "INSERT", "provider_metadata", None, by_new_item.clone());
    add(
        "repick_answer_upd",
        "UPDATE OF provider_id, confidence",
        "provider_metadata",
        None,
        by_new_item.clone(),
    );
    add(
        "repick_answer_del",
        "DELETE",
        "provider_metadata",
        Some(survives.into()),
        by_old_item.clone(),
    );

    // Chain order: one media type's worth of items, all of them.
    add("repick_rank_ins", "INSERT", "provider_ranks", None, repick_body(None, Some("NEW.media_type")));
    add(
        "repick_rank_upd",
        "UPDATE OF rank",
        "provider_ranks",
        None,
        repick_body(None, Some("NEW.media_type")),
    );
    add("repick_rank_del", "DELETE", "provider_ranks", None, repick_body(None, Some("OLD.media_type")));

    // Refusals.
    add("repick_reject_ins", "INSERT", "rejected_matches", None, by_new_item.clone());
    add(
        "repick_reject_del",
        "DELETE",
        "rejected_matches",
        Some(survives.into()),
        by_old_item.clone(),
    );

    // Human pins.
    add("repick_pin_ins", "INSERT", "manual_match", None, by_new_item.clone());
    add(
        "repick_pin_upd",
        "UPDATE OF provider, provider_id",
        "manual_match",
        None,
        by_new_item,
    );
    add("repick_pin_del", "DELETE", "manual_match", Some(survives.into()), by_old_item);

    // Which collection an item's files live in decides its media type,
    // and the media type decides the chain — so moving a source can move
    // the answer. Nothing maintained this before; it simply drifted.
    add(
        "repick_source_ins",
        "INSERT",
        "item_sources",
        Some(has_answers("NEW")),
        repick_body(Some(&src_item("NEW")), None),
    );
    add(
        "repick_source_upd",
        "UPDATE OF module_id, collection_id",
        "item_sources",
        Some(has_answers("NEW")),
        repick_body(Some(&src_item("NEW")), None),
    );
    add(
        "repick_source_del",
        "DELETE",
        "item_sources",
        Some(format!("{survives} AND {}", has_answers("OLD"))),
        repick_body(Some(&src_item("OLD")), None),
    );

    // A satellite re-announcing a collection can change its media type.
    // Rare, and it moves every item in it, so recompute the lot.
    add(
        "repick_collection_upd",
        "UPDATE OF media_type",
        "collections",
        None,
        repick_body(None, None),
    );
    out
}

/// Recompute assignments from scratch, filtered or not.
///
/// The triggers do this on every input write, so nothing in normal
/// operation calls it. It exists for the one moment there are no triggers
/// to rely on: [`crate::db::install_derived`] replacing a definition an
/// older binary left behind, where what that definition maintained is of
/// unknown provenance and has to be rebuilt.
pub(crate) async fn reassign(
    tx: &mut sqlx::SqliteConnection,
    item_id: Option<&str>,
    media_type: Option<&str>,
) -> Result<()> {
    // One transaction, always: between the delete and the insert an item
    // has no assignment, and a reader in that window would see it as
    // unmatched.
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
    // One statement, no transaction. The assignment follows from the
    // trigger on this write, inside the same implicit transaction, so
    // the window where an item briefly has no assignment cannot be
    // observed — a guarantee the previous two-statement transaction
    // provided by convention and this provides by construction. It also
    // takes one multi-statement transaction off the path seven writers
    // share.
    bind_answer(sqlx::query(STORE_ANSWER), item_id, provider, provider_id, confidence, &fields)
        .execute(db)
        .await?;
    Ok(())
}

/// One provider's answer, upserted. Standalone so a caller that already
/// holds a transaction (`assign_manual`) can run it there rather than
/// opening a second one.
const STORE_ANSWER: &str = "\
INSERT INTO provider_metadata
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
           updated_at = excluded.updated_at";

/// The twelve binds [`STORE_ANSWER`] wants, in its order.
fn bind_answer<'a>(
    q: sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    item_id: &'a str,
    provider: &'a str,
    provider_id: &'a str,
    confidence: &'a str,
    fields: &'a Fields,
) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
    q.bind(item_id)
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
}

/// The user's choice: THIS provider's record is what the item is.
///
/// Three inputs, one transaction: the answer itself (a human match is
/// strong by definition), the record stops being refused if it had been,
/// and the pin. The assignment is not written here — it is derived from
/// those three, and the pick is what derives it.
pub async fn assign_manual(
    db: &SqlitePool,
    item_id: &str,
    provider: &str,
    provider_id: &str,
    fields: Fields,
) -> Result<()> {
    let mut tx = db.begin().await?;
    bind_answer(sqlx::query(STORE_ANSWER), item_id, provider, provider_id, "auto", &fields)
        .execute(&mut *tx)
        .await?;
    // A pin and a refusal of the same record contradict each other. The
    // pin is the newer statement, so it is the one that stands — and it
    // has to land BEFORE the pick, which treats a refused record as no
    // candidate at all.
    sqlx::query(
        "DELETE FROM rejected_matches
          WHERE item_id = ? AND provider = ? AND provider_id = ?",
    )
    .bind(item_id)
    .bind(provider)
    .bind(provider_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(PIN_MATCH)
        .bind(item_id)
        .bind(provider)
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The human pin itself: one record per item, replaced rather than added
/// to, because an item is one thing.
const PIN_MATCH: &str = "\
INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
VALUES (?1, ?2, ?3, unixepoch())
ON CONFLICT (item_id) DO UPDATE SET
  provider = excluded.provider,
  provider_id = excluded.provider_id,
  pinned_at = excluded.pinned_at";

/// Promote the current automatic assignment to a human decision. Touches
/// no answer — the record it points at is already on file. The only
/// difference from [`assign_manual`] is where the record comes from: what
/// is already assigned, rather than a click on a search result.
pub async fn confirm_assignment(db: &SqlitePool, item_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO manual_match (item_id, provider, provider_id, pinned_at)
         SELECT item_id, provider, provider_id, unixepoch() FROM item_match
          WHERE item_id = ?1 AND provider_id <> ''
         ON CONFLICT (item_id) DO UPDATE SET
           provider = excluded.provider,
           provider_id = excluded.provider_id,
           pinned_at = excluded.pinned_at",
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
    // The pin goes too, or it would reassert the very record just
    // refused. "No correct record" outranks an earlier "this one".
    //
    // Between them these two are the whole of it: every candidate is now
    // refused and nothing is pinned, so the pick has no winner to find
    // and the assignment goes on its own.
    sqlx::query("DELETE FROM manual_match WHERE item_id = ?")
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
