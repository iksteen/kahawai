//! Preserve library history when an unidentified copy first joins a known item.
//! Refusals follow permanent aliases when read, keeping their recorded IDs.
//! Promotion queues the copies refusing any predecessor alias. The partial
//! alias-target index stores only aliases; the refusal-target index stores only
//! human refusals, so this invalidation uses their keys rather than rebuilding
//! a lookup over the whole library for each newly identified item. Migration 84
//! queues existing alias refusals once, including derived music conflicts.
use super::*;

/// Retained items keep their identity and history when coverage is reordered.
/// Only removed unidentified items can be promoted into newly added targets.
pub(super) async fn promote_replaced_state(
    c: &mut SqliteConnection,
    old: &[String],
    new: &[String],
) -> Result<()> {
    let old_set: std::collections::HashSet<_> = old.iter().collect();
    let new_set: std::collections::HashSet<_> = new.iter().collect();
    for (old, new) in old
        .iter()
        .filter(|id| !new_set.contains(id))
        .zip(new.iter().filter(|id| !old_set.contains(id)))
    {
        promote_state(c, old, new).await?;
    }
    Ok(())
}

pub(super) async fn promote_state(c: &mut SqliteConnection, old: &str, new: &str) -> Result<()> {
    let resolved = canonical_id(c, new).await?;
    let new = resolved.as_str();
    if old == new {
        return Ok(());
    }
    let promote: bool = sqlx::query_scalar(
        "SELECT unidentified=1 AND merged_into IS NULL FROM library_items WHERE id=?",
    )
    .bind(old)
    .fetch_one(&mut *c)
    .await?;
    if !promote {
        return Ok(());
    }
    sqlx::query("INSERT INTO user_item_state SELECT user_id,?1,position_ms,duration_ms,played,play_count,updated_at,resume_source_fingerprint FROM user_item_state WHERE item_id=?2
        ON CONFLICT(user_id,item_id) DO UPDATE SET
        position_ms=CASE WHEN excluded.updated_at>user_item_state.updated_at THEN excluded.position_ms ELSE user_item_state.position_ms END,
        duration_ms=CASE WHEN excluded.updated_at>user_item_state.updated_at THEN excluded.duration_ms ELSE user_item_state.duration_ms END,
        played=CASE WHEN excluded.updated_at>user_item_state.updated_at THEN excluded.played ELSE user_item_state.played END,
        resume_source_fingerprint=CASE WHEN excluded.updated_at>user_item_state.updated_at THEN excluded.resume_source_fingerprint ELSE user_item_state.resume_source_fingerprint END,
        play_count=MAX(user_item_state.play_count,excluded.play_count),updated_at=MAX(user_item_state.updated_at,excluded.updated_at)")
        .bind(new).bind(old).execute(&mut *c).await?;
    // First identification promotes the parent itself. Typed child references
    // follow it even when a child copy has an explicit assignment.
    sqlx::query(
        "UPDATE provider_metadata SET parent_library_item_id=? WHERE parent_library_item_id=?",
    )
    .bind(new)
    .bind(old)
    .execute(&mut *c)
    .await?;
    sqlx::query("UPDATE episode_details SET series_id=? WHERE series_id=?")
        .bind(new)
        .bind(old)
        .execute(&mut *c)
        .await?;
    let positions: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        "SELECT id,song_id,disc_number,track_number FROM album_tracks WHERE album_id=?",
    )
    .bind(old)
    .fetch_all(&mut *c)
    .await?;
    for (position, song, disc, track) in positions {
        let existing:Option<i64>=sqlx::query_scalar("SELECT id FROM album_tracks WHERE album_id=? AND song_id=? AND disc_number=? AND track_number=?")
            .bind(new).bind(&song).bind(disc).bind(track).fetch_optional(&mut *c).await?;
        if let Some(existing) = existing {
            sqlx::query("UPDATE collection_items SET album_track_id=? WHERE album_track_id=?")
                .bind(existing)
                .bind(position)
                .execute(&mut *c)
                .await?;
            // Queues may already carry this numeric position. Keep its original
            // album/song/coordinates so playback can resolve the permanent
            // library aliases while current copies use the surviving position.
        } else {
            sqlx::query("UPDATE album_tracks SET album_id=? WHERE id=?")
                .bind(new)
                .bind(position)
                .execute(&mut *c)
                .await?;
        }
    }
    merge_overrides(c, old, new).await?;
    import_preferences(c, old, &[new.to_owned()]).await?;
    sqlx::query("DELETE FROM user_item_state WHERE item_id=?")
        .bind(old)
        .execute(&mut *c)
        .await?;
    let copies: Vec<String> = sqlx::query_scalar(
        "SELECT collection_item_id FROM collection_item_library_items WHERE library_item_id=?",
    )
    .bind(old)
    .fetch_all(&mut *c)
    .await?;
    for copy in copies {
        let ids: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal")
            .bind(&copy).fetch_all(&mut *c).await?;
        let mut targets = Vec::new();
        for id in ids {
            let id = if id == old { new.to_owned() } else { id };
            if !targets.contains(&id) {
                targets.push(id);
            }
        }
        replace_links(c, &copy, &targets).await?;
        sqlx::query("INSERT INTO library_pending VALUES(?) ON CONFLICT DO NOTHING")
            .bind(&copy)
            .execute(&mut *c)
            .await?;
    }
    sqlx::query("UPDATE library_items SET merged_into=? WHERE id=?")
        .bind(new)
        .bind(old)
        .execute(&mut *c)
        .await?;
    // This identification also changes the meaning of refusals recorded under
    // the old ID or an earlier alias. Revisit those copies now, including copies
    // already linked to the target, using reverse alias/refusal key lookups.
    sqlx::query(
        "WITH RECURSIVE aliases(id) AS (
        SELECT ? UNION SELECT previous.id FROM library_items previous
        JOIN aliases target ON previous.merged_into=target.id
      ) INSERT INTO library_pending(collection_item_id)
      SELECT DISTINCT r.collection_item_id FROM rejected_library_matches r
      WHERE r.library_item_id IN(SELECT id FROM aliases)
      ON CONFLICT DO NOTHING",
    )
    .bind(old)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Keep an existing library preference on conflicts. Original source preferences
/// remain stored, so ambiguous legacy choices are retained rather than guessed.
/// Exact track choices name both their collection copy and physical source;
/// a reused numeric source ID cannot carry another copy's previous selection.
pub(super) async fn import_preferences(
    c: &mut SqliteConnection,
    copy: &str,
    items: &[String],
) -> Result<()> {
    for item in items {
        sqlx::query("INSERT INTO user_prefs(user_id,scope,key,value) SELECT user_id,?1,key,value FROM user_prefs WHERE scope=?2 AND key NOT IN('audio.track','subs.track') AND NOT(key='audio' AND value LIKE '#%') ON CONFLICT DO NOTHING")
            .bind(item).bind(copy).execute(&mut *c).await?;
    }
    sqlx::query("INSERT INTO user_prefs(user_id,scope,key,value) SELECT user_id,'source:' || ps.item_id || ':' || ps.id,CASE WHEN key='audio' THEN 'audio.track' ELSE key END,value FROM user_prefs JOIN playable_sources ps ON ps.item_id=scope WHERE scope=? AND (key IN('audio.track','subs.track') OR (key='audio' AND value LIKE '#%')) AND (SELECT COUNT(*) FROM playable_sources WHERE item_id=scope)=1 ON CONFLICT DO NOTHING")
        .bind(copy).execute(&mut *c).await?;
    Ok(())
}

pub(super) async fn import_state(c: &mut SqliteConnection) -> Result<()> {
    // Consume in timestamp order: the latest decision wins, count never falls.
    let rows=sqlx::query("SELECT w.*,a.library_item_id FROM state_imports w JOIN collection_item_library_items a ON a.collection_item_id=w.item_id
        ORDER BY w.updated_at,w.item_id,a.ordinal").fetch_all(&mut *c).await?;
    for r in rows {
        let target: String = r.get("library_item_id");
        if let Some(expected) = r.get::<Option<String>, _>("expected_library_item_id")
            && expected != target
        {
            let unidentified:bool=sqlx::query_scalar("SELECT COALESCE((SELECT unidentified=1 AND merged_into IS NULL FROM library_items WHERE id=?),0)").bind(&expected).fetch_one(&mut *c).await?;
            if !unidentified {
                continue;
            }
            promote_state(c, &expected, &target).await?;
        }
        sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,played,play_count,updated_at,resume_source_fingerprint) VALUES(?,?,?,?,?,?,?,?)
            ON CONFLICT(user_id,item_id) DO UPDATE SET
            position_ms=CASE WHEN excluded.updated_at>=user_item_state.updated_at THEN excluded.position_ms ELSE user_item_state.position_ms END,
            duration_ms=CASE WHEN excluded.updated_at>=user_item_state.updated_at THEN excluded.duration_ms ELSE user_item_state.duration_ms END,
            played=CASE WHEN excluded.updated_at>=user_item_state.updated_at THEN excluded.played ELSE user_item_state.played END,
            resume_source_fingerprint=CASE WHEN excluded.updated_at>=user_item_state.updated_at THEN excluded.resume_source_fingerprint ELSE user_item_state.resume_source_fingerprint END,
            play_count=MAX(user_item_state.play_count,excluded.play_count),updated_at=MAX(user_item_state.updated_at,excluded.updated_at)")
            .bind(r.get::<String,_>("user_id")).bind(r.get::<String,_>("library_item_id")).bind(r.get::<i64,_>("position_ms"))
            .bind(r.get::<Option<i64>,_>("duration_ms")).bind(r.get::<i64,_>("played")).bind(r.get::<i64,_>("play_count")).bind(r.get::<i64,_>("updated_at")).bind(r.get::<Option<String>,_>("resume_source_fingerprint")).execute(&mut *c).await?;
    }
    sqlx::query("DELETE FROM state_imports WHERE item_id IN(SELECT collection_item_id FROM collection_item_library_items)").execute(&mut *c).await?;
    Ok(())
}

/// Resolve only permanent unidentified promotions. Established reassignment never
/// creates an alias, so a session cannot move history to a newly selected work.
pub async fn canonical_id(c: &mut SqliteConnection, id: &str) -> Result<String> {
    let canonical = sqlx::query_scalar("WITH RECURSIVE chain(id,merged_into) AS (SELECT id,merged_into FROM library_items WHERE id=? UNION SELECT c.id,c.merged_into FROM library_items c JOIN chain a ON c.id=a.merged_into) SELECT id FROM chain WHERE merged_into IS NULL")
        .bind(id).fetch_optional(&mut *c).await?;
    if let Some(canonical) = canonical {
        return Ok(canonical);
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_items WHERE id=?)")
        .bind(id)
        .fetch_one(c)
        .await?;
    anyhow::ensure!(!exists, "cyclic library item alias for {id}");
    Ok(id.to_owned())
}

pub async fn canonical_ids(c: &mut SqliteConnection, ids: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in ids {
        let id = canonical_id(c, id).await?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Refusals keep their recorded ID, but compare against the permanently
/// identified work. Return both IDs so an explicit correction can remove only
/// equivalent refusals without rewriting or discarding unrelated decisions.
pub(super) async fn resolved_rejections(
    c: &mut SqliteConnection,
    copy: &str,
) -> Result<Vec<(String, String)>> {
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id=?",
    )
    .bind(copy)
    .fetch_all(&mut *c)
    .await?;
    let mut resolved = Vec::with_capacity(stored.len());
    for id in stored {
        let canonical = canonical_id(c, &id).await?;
        resolved.push((id, canonical));
    }
    Ok(resolved)
}

pub(super) async fn clear_equivalent_rejections(
    c: &mut SqliteConnection,
    copy: &str,
    ids: &[String],
) -> Result<()> {
    for (stored, canonical) in resolved_rejections(c, copy).await? {
        if ids.contains(&canonical) {
            sqlx::query("DELETE FROM rejected_library_matches WHERE collection_item_id=? AND library_item_id=?")
                .bind(copy).bind(stored).execute(&mut *c).await?;
        }
    }
    Ok(())
}

/// Existing choices on the identified item win; fill only unset fields.
pub(super) async fn merge_overrides(c: &mut SqliteConnection, old: &str, new: &str) -> Result<()> {
    let Some(previous) = sqlx::query_scalar::<_, String>(
        "SELECT fields FROM library_overrides WHERE library_item_id=?",
    )
    .bind(old)
    .fetch_optional(&mut *c)
    .await?
    else {
        return Ok(());
    };
    let previous: MetadataOverrides = serde_json::from_str(&previous)?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT fields FROM library_overrides WHERE library_item_id=?")
            .bind(new)
            .fetch_optional(&mut *c)
            .await?;
    let mut current: MetadataOverrides = current
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    current.overview = current.overview.or(previous.overview);
    current.rating = current.rating.or(previous.rating);
    current.original_language = current.original_language.or(previous.original_language);
    current.genres = current.genres.or(previous.genres);
    sqlx::query("INSERT INTO library_overrides VALUES(?,?) ON CONFLICT(library_item_id) DO UPDATE SET fields=excluded.fields").bind(new).bind(serde_json::to_string(&current)?).execute(&mut *c).await?;
    sqlx::query("DELETE FROM library_overrides WHERE library_item_id=?")
        .bind(old)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE library_items SET revision=revision+1 WHERE id=?")
        .bind(new)
        .execute(&mut *c)
        .await?;
    Ok(())
}
