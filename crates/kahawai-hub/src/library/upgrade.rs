//! Repair legacy collection items that combined different episode coverages.
//! This reads stored paths, never media. Only matching pending at startup is
//! considered; migration 78 queues existing episodes once. Explicit assignments
//! and files whose parsed start disagrees with the stored start are left intact.
use super::*;
use kahawai_core::names;

pub(super) async fn repair_episode_copies(c: &mut SqliteConnection) -> Result<()> {
    let items: Vec<(String, Option<i64>, i64, Option<i64>)> = sqlx::query_as(
        "SELECT i.id,i.season,i.episode,i.episode_end FROM collection_items i JOIN library_pending p ON p.collection_item_id=i.id WHERE i.kind='episode' AND i.episode IS NOT NULL AND i.assignment_manual=0 ORDER BY i.id"
    ).fetch_all(&mut *c).await?;
    for (copy, season, first, last) in items {
        let sources: Vec<(i64,String)> = sqlx::query_as("SELECT ps.id,f.path_rel FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id AND p.ordinal=1 JOIN files f ON f.id=p.file_id WHERE ps.item_id=? ORDER BY ps.id")
            .bind(&copy).fetch_all(&mut *c).await?;
        let source_count = sources.len();
        let mut groups = std::collections::BTreeMap::<i64, Vec<i64>>::new();
        for (source, path) in sources {
            let Some(parsed) = names::parse_anime(&path) else {
                continue;
            };
            if parsed.season.map(i64::from) != season || i64::from(parsed.episode) != first {
                continue;
            }
            groups
                .entry(i64::from(parsed.episode_end.unwrap_or(parsed.episode)))
                .or_default()
                .push(source);
        }
        let original_end = last.unwrap_or(first);
        let keep = if groups.values().map(Vec::len).sum::<usize>() == source_count
            && !groups.contains_key(&original_end)
        {
            groups.keys().next().copied().unwrap_or(original_end)
        } else {
            original_end
        };
        if keep != original_end {
            sqlx::query("UPDATE collection_items SET episode_end=? WHERE id=?")
                .bind((keep > first).then_some(keep))
                .bind(&copy)
                .execute(&mut *c)
                .await?;
        }
        // Keep the original collection row for an existing range. New copies
        // inherit source metadata describing the same first episode.
        for (end, sources) in groups {
            if end == keep {
                continue;
            }
            let new = ulid::Ulid::generate().to_string();
            sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,parent_id,season,episode,artist,sort_title,norm_artist,episode_end,module_id,collection_id,artist_key)
                SELECT ?,kind,title,norm_title,year,parent_id,season,episode,artist,sort_title,norm_artist,?,module_id,collection_id,artist_key FROM collection_items WHERE id=?")
                .bind(&new).bind((end>first).then_some(end)).bind(&copy).execute(&mut *c).await?;
            sqlx::query("INSERT INTO provider_metadata(item_id,provider,provider_id,title,overview,poster_path,rating,premiered,original_language,genres,confidence,updated_at,proj_season,proj_episode,cast_json,provider_artist_id)
                SELECT ?,provider,provider_id,title,overview,poster_path,rating,premiered,original_language,genres,confidence,updated_at,proj_season,proj_episode,cast_json,provider_artist_id FROM provider_metadata WHERE item_id=?")
                .bind(&new).bind(&copy).execute(&mut *c).await?;
            sqlx::query("INSERT INTO manual_match SELECT ?,provider,provider_id,pinned_at FROM manual_match WHERE item_id=?")
                .bind(&new).bind(&copy).execute(&mut *c).await?;
            sqlx::query("INSERT INTO rejected_matches SELECT ?,provider,provider_id,rejected_at FROM rejected_matches WHERE item_id=?")
                .bind(&new).bind(&copy).execute(&mut *c).await?;
            sqlx::query("INSERT INTO rejected_library_matches SELECT ?,library_item_id FROM rejected_library_matches WHERE collection_item_id=?")
                .bind(&new).bind(&copy).execute(&mut *c).await?;
            let tracks = copy_subtitles(c, &copy, &new).await?;
            for source in sources {
                for (old_track, new_track) in &tracks {
                    sqlx::query("UPDATE user_prefs SET value=? WHERE scope=? AND key='subs.track' AND value=?")
                        .bind(new_track.to_string()).bind(format!("source:{source}")).bind(old_track.to_string()).execute(&mut *c).await?;
                }
                sqlx::query("UPDATE playable_sources SET item_id=? WHERE id=?")
                    .bind(&new)
                    .bind(source)
                    .execute(&mut *c)
                    .await?;
            }
            // Original collection metadata and history stay intact, including
            // a row that no longer has files. Only current sources grant access.
            sqlx::query("INSERT INTO library_pending VALUES(?) ON CONFLICT DO NOTHING")
                .bind(&new)
                .execute(&mut *c)
                .await?;
        }
    }
    let redirected: Vec<String> = sqlx::query_scalar("SELECT i.id FROM library_items i JOIN library_overrides o ON o.library_item_id=i.id WHERE i.merged_into IS NOT NULL").fetch_all(&mut *c).await?;
    for old in redirected {
        let new = canonical_id(c, &old).await?;
        super::history::merge_overrides(c, &old, &new).await?;
    }
    Ok(())
}

/// Downloaded tracks follow a collection copy; their immutable payloads already
/// have independent IDs. Preserve those and clone lineage when splitting a copy.
async fn copy_subtitles(
    c: &mut SqliteConnection,
    old: &str,
    new: &str,
) -> Result<std::collections::BTreeMap<i64, i64>> {
    let rows: Vec<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id,derived_from FROM subtitle_tracks WHERE item_id=? ORDER BY id")
            .bind(old)
            .fetch_all(&mut *c)
            .await?;
    let mut ids = std::collections::BTreeMap::new();
    for (id, _) in &rows {
        let new_id:i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(item_id,origin,stream_index,format,language,label,provider,machine,created_by,created_at,payload_id)
            SELECT ?,origin,stream_index,format,language,label,provider,machine,created_by,created_at,COALESCE(payload_id,id) FROM subtitle_tracks WHERE id=? RETURNING id")
            .bind(new).bind(id).fetch_one(&mut *c).await?;
        ids.insert(*id, new_id);
    }
    for (id, parent) in rows {
        if let Some(parent) = parent {
            sqlx::query("UPDATE subtitle_tracks SET derived_from=? WHERE id=?")
                .bind(ids[&parent])
                .bind(ids[&id])
                .execute(&mut *c)
                .await?;
        }
    }
    Ok(ids)
}
