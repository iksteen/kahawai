//! Library items are the things a user browses, plays and keeps history for.
//!
//! `collection_items` stores detected and assigned metadata for a collection copy.
//! `collection_item_library_items` links that copy to its library item. Its ordinal
//! is only needed when one copy contains several episodes. CD1/CD2 are physical
//! parts of one `playable_sources` row, not separate library items or assignments.
//!
//! Matching compares ordinary fields: movie/series title and year; episode series
//! and number; album artist, title, year and edition; song album position or a
//! known recording ID. Provider IDs never define a movie or series. Missing or
//! ambiguous identities stay separate until identified or explicitly assigned.
//!
//! A manual assignment uses the same links as an automatic assignment, with
//! `collection_items.assignment_manual` preventing automatic replacement.
//! `assignment_revision` rejects stale edits. A correction changes this copy's
//! links, leaving other copies and history with the previous library item.
//! Descriptive provider answers stay on the copy; `metadata_eligible` prevents an
//! answer for the previous item from describing a manually selected different one.
//!
//! `episode_details` contains series/native numbering/season/episode. Provider
//! season projections are read for presentation and never rekey an episode.
//! `album_tracks` is
//! the stable album position of a recording. `collection_items.album_track_id`
//! selects the position occupied by this copy; only current copies grant access.
//! Creating a song also stores its supplied album position here, independently
//! of the assigning copy's detected position. A supplied track defaults to disc 1;
//! a song created without a track remains available for explicit assignment.
//! A recording can occur on several albums. Both child kinds remain
//! ordinary, globally searchable library items. Anime is a classification.
//!
//! An unidentified item keeps its ID on first identification when possible. When
//! it joins an existing item, its history transfers once and `merged_into` lets
//! already playing sessions finish against that item. Correcting an identified
//! item never redirects its history. `source_boundaries` optionally maps combined
//! playback to episodes; without complete known boundaries playback stays combined.
//!
//! Matching runs before the source transaction commits. Triggers enqueue changed
//! collection items; parents are matched before their children. Reads do no repair,
//! and matching requires no provider calls or media reads.

mod history;
mod matching;
mod playback;
mod upgrade;
pub use history::{canonical_id, canonical_ids};
use history::{clear_equivalent_rejections, promote_replaced_state, resolved_rejections};
pub(crate) use matching::copy_regroup_conflict;
pub use matching::{create, initialize, reconcile};
pub use playback::{
    PlaybackSnapshot, SourceBoundary, playback_snapshot, resume_fingerprint, same_resume_version,
};

pub use kahawai_sqlite::{Database, WriterTransaction as Transaction};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};
use unicode_normalization::UnicodeNormalization;

/// Case and whitespace folding only. Punctuation and diacritics distinguish works.
pub fn name_key(value: &str) -> String {
    value
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Assignment {
    pub collection_item_id: String,
    pub revision: i64,
    pub mode: String,
    pub library_item_ids: Vec<String>,
    pub conflict: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct NewItem {
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub artist: Option<String>,
    pub parent_id: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub edition: Option<String>,
}

pub(super) async fn replace_links(
    c: &mut SqliteConnection,
    copy: &str,
    ids: &[String],
) -> Result<()> {
    sqlx::query("DELETE FROM collection_item_library_items WHERE collection_item_id=?")
        .bind(copy)
        .execute(&mut *c)
        .await?;
    for (ordinal, id) in ids.iter().enumerate() {
        sqlx::query("INSERT INTO collection_item_library_items VALUES(?,?,?)")
            .bind(copy)
            .bind(ordinal as i64 + 1)
            .bind(id)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}

/// Choose library items for the entire collection copy, including all its CDs.
/// Validation is shared by API callers and maintenance scripts.
pub async fn assign(c: &mut SqliteConnection, copy: &str, ids: &[String]) -> Result<()> {
    let kind: String = sqlx::query_scalar("SELECT CASE kind WHEN 'show' THEN 'series' WHEN 'track' THEN 'song' ELSE kind END FROM collection_items WHERE id=?")
        .bind(copy).fetch_one(&mut *c).await?;
    anyhow::ensure!(
        !ids.is_empty() && ids.len() <= 1000 && (kind == "episode" || ids.len() == 1),
        "only episode copies may cover multiple library items"
    );
    let mut unique = std::collections::HashSet::new();
    for id in ids {
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_items WHERE id=? AND kind=? AND merged_into IS NULL)")
            .bind(id).bind(&kind).fetch_one(&mut *c).await?;
        anyhow::ensure!(
            valid && unique.insert(id),
            "incompatible or duplicate library item"
        );
    }
    let old: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal")
        .bind(copy).fetch_all(&mut *c).await?;
    replace_links(c, copy, ids).await?;
    promote_replaced_state(c, &old, ids).await?;
    sqlx::query("UPDATE collection_items SET assignment_manual=1 WHERE id=?")
        .bind(copy)
        .execute(&mut *c)
        .await?;
    clear_equivalent_rejections(c, copy, ids).await?;
    Ok(())
}

pub async fn assignment(c: &mut SqliteConnection, id: &str) -> Result<Assignment> {
    let r = sqlx::query(
        "SELECT assignment_revision AS revision,match_mode AS mode,match_conflict AS conflict FROM collection_items WHERE id=?",
    )
    .bind(id)
    .fetch_one(&mut *c)
    .await?;
    Ok(Assignment {
        collection_item_id: id.into(),
        revision: r.get("revision"),
        mode: r.get("mode"),
        conflict: r.get("conflict"),
        library_item_ids: sqlx::query_scalar(
            "SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal",
        )
        .bind(id)
        .fetch_all(&mut *c)
        .await?,
    })
}

/// Only copies reachable through this account's libraries. Album visibility is
/// obtained from album assignments, never from a recording on a different album.
/// Resolve a permanent first-identification alias at an item API boundary.
pub async fn resolve_id(db: &Database, id: &str) -> Result<String> {
    let mut c = db.read_pool().acquire().await?;
    canonical_id(&mut c, id).await
}

pub async fn copies(db: &Database, user: &str, library_item_id: &str) -> Result<Vec<String>> {
    let library_item_id = resolve_id(db, library_item_id).await?;
    Ok(sqlx::query_scalar("SELECT a.collection_item_id FROM collection_item_library_items a
        JOIN collection_items i ON i.id=a.collection_item_id
        WHERE a.library_item_id=?2 AND (
          EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR
          EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id
            WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(i.module_id,i.collection_id)))
        ORDER BY (i.metadata_eligible AND a.ordinal=1) DESC, EXISTS(SELECT 1 FROM item_match m WHERE m.item_id=i.id) DESC,i.id")
        .bind(user).bind(library_item_id).fetch_all(db).await?)
}

/// Shared descriptions, independent of collection metadata and matching.
/// Replacing this object with an empty one restores provider descriptions.
#[derive(Debug, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MetadataOverrides {
    pub overview: Option<String>,
    pub rating: Option<f64>,
    pub original_language: Option<String>,
    pub genres: Option<Vec<String>>,
}
