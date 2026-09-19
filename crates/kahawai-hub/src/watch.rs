//! User state belongs to the hub, keyed by mediadb's immutable parent or child ID.
//! Library membership is an access boundary, not part of a watch identity: the
//! same item in two permitted libraries shares state. The parent ID groups child
//! marks for season summaries. No FK points into mediadb, so losing a source or
//! deleting a library cannot erase history. User deletion does cascade.
//! Legacy user_item_state and its archived history are deliberately not imported.
//! Played is a boolean, never a count. Manual marks clear resume position; progress
//! updates it, and a nonzero report decides completion at 90%. Zero reports do not
//! change played/recency. Tracks retain completion, but no resume offset.
/// Existing atomic watch-operation bound, also used when capturing combined coverage.
pub const MAX_BATCH_ITEMS: usize = 2000;

use kahawai_sqlite::Database;
use serde::Serialize;
use sqlx::Row;
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Clone, Default, Serialize, ToSchema)]
pub struct WatchState {
    pub played: bool,
    pub resume_position_ms: Option<i64>,
    pub resume_duration_ms: Option<i64>,
}
pub async fn read(
    db: &Database,
    user: &str,
    ids: &[String],
) -> anyhow::Result<BTreeMap<String, WatchState>> {
    let rows = sqlx::query("SELECT item_id,position_ms,duration_ms,played FROM catalogue_watch_state WHERE user_id=? AND item_id IN (SELECT value FROM json_each(?))")
        .bind(user).bind(serde_json::to_string(ids)?).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let played = r.get::<bool, _>("played");
            let position = r.get::<i64, _>("position_ms");
            (
                r.get("item_id"),
                WatchState {
                    played,
                    resume_position_ms: (!played && position > 0).then_some(position),
                    resume_duration_ms: r.get("duration_ms"),
                },
            )
        })
        .collect())
}
pub async fn mark(
    db: &Database,
    user: &str,
    parent: &str,
    ids: &[String],
    played: bool,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO catalogue_watch_state(user_id,item_id,parent_id,played) SELECT ?1,value,?2,?3 FROM json_each(?4) WHERE 1 ON CONFLICT(user_id,item_id) DO UPDATE SET played=excluded.played,position_ms=0,updated_at=unixepoch()")
        .bind(user).bind(parent).bind(played).bind(serde_json::to_string(ids)?).execute(db).await?;
    Ok(())
}

#[derive(Serialize)]
pub struct Progress {
    pub id: String,
    pub parent: String,
    pub position: u64,
    pub duration: Option<u64>,
    pub track: bool,
}
/// Write one session report atomically, including only members it actually visited.
pub async fn progress(db: &Database, user: &str, reports: &[Progress]) -> anyhow::Result<()> {
    let values: Vec<_> = reports.iter().map(|r| serde_json::json!({
        "id": r.id, "parent": r.parent,
        "position": if r.track { 0 } else { r.position.min(i64::MAX as u64) },
        "duration": r.duration.map(|d| d.min(i64::MAX as u64)),
        "played": r.duration.is_some_and(|d| d > 0 && u128::from(r.position) * 10 >= u128::from(d) * 9),
        "at_start": r.position == 0,
    })).collect();
    sqlx::query("INSERT INTO catalogue_watch_state(user_id,item_id,parent_id,position_ms,duration_ms,played)
        SELECT ?1,json_extract(value,'$.id'),json_extract(value,'$.parent'),json_extract(value,'$.position'),json_extract(value,'$.duration'),json_extract(value,'$.played') FROM json_each(?2) WHERE 1
        ON CONFLICT(user_id,item_id) DO UPDATE SET position_ms=excluded.position_ms,duration_ms=excluded.duration_ms,
        played=CASE WHEN json_extract((SELECT value FROM json_each(?2) WHERE json_extract(value,'$.id')=excluded.item_id),'$.at_start') THEN played ELSE excluded.played END,
        updated_at=CASE WHEN json_extract((SELECT value FROM json_each(?2) WHERE json_extract(value,'$.id')=excluded.item_id),'$.at_start') THEN updated_at ELSE unixepoch() END")
        .bind(user).bind(serde_json::to_string(&values)?).execute(db).await?;
    Ok(())
}
