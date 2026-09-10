//! Assign collection copies by their selected metadata, within the source transaction.
use super::history::{import_preferences, import_state};
use super::*;
use anyhow::Context;

pub async fn initialize(db: &Database) -> Result<()> {
    let mut tx = db.begin_with_label("install library derivations").await?;
    for (table, column) in [
        ("collection_items", "id"),
        ("playable_sources", "item_id"),
        ("provider_metadata", "item_id"),
        ("item_match", "item_id"),
        ("manual_match", "item_id"),
        ("rejected_matches", "item_id"),
        ("rejected_library_matches", "collection_item_id"),
        ("state_imports", "item_id"),
    ] {
        for (event, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
            let name = format!("library_dirty_{table}_{event}");
            let event = if table == "collection_items" && event == "UPDATE" {
                "UPDATE OF kind,title,year,artist,parent_id,season,episode,episode_end,module_id,collection_id,assignment_manual"
            } else {
                event
            };
            let sql = format!("DROP TRIGGER IF EXISTS {name}; CREATE TRIGGER {name} AFTER {event} ON {table}
                BEGIN INSERT INTO library_pending VALUES({row}.{column}) ON CONFLICT DO NOTHING; END;");
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await?;
        }
    }
    for (event, row) in [
        ("INSERT", "NEW"),
        ("UPDATE OF streams_json", "NEW"),
        ("DELETE", "OLD"),
    ] {
        let suffix = event.split_whitespace().next().unwrap();
        let sql=format!("DROP TRIGGER IF EXISTS library_dirty_files_{suffix}; CREATE TRIGGER library_dirty_files_{suffix} AFTER {event} ON files BEGIN
              INSERT INTO library_pending SELECT ps.item_id FROM playable_source_parts p JOIN playable_sources ps ON ps.id=p.playable_source_id WHERE p.file_id={row}.id ON CONFLICT DO NOTHING;
              INSERT INTO library_pending SELECT ci.parent_id FROM playable_source_parts p JOIN playable_sources ps ON ps.id=p.playable_source_id JOIN collection_items ci ON ci.id=ps.item_id WHERE p.file_id={row}.id AND ci.parent_id IS NOT NULL ON CONFLICT DO NOTHING;
            END;");
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await?;
    }
    for (event, row) in [("INSERT", "NEW"), ("DELETE", "OLD")] {
        let sql=format!("DROP TRIGGER IF EXISTS library_dirty_parts_{event}; CREATE TRIGGER library_dirty_parts_{event} AFTER {event} ON playable_source_parts BEGIN
              INSERT INTO library_pending SELECT item_id FROM playable_sources WHERE id={row}.playable_source_id ON CONFLICT DO NOTHING;
              INSERT INTO library_pending SELECT ci.parent_id FROM playable_sources ps JOIN collection_items ci ON ci.id=ps.item_id WHERE ps.id={row}.playable_source_id AND ci.parent_id IS NOT NULL ON CONFLICT DO NOTHING;
            END;");
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut *tx)
            .await?;
    }
    // Reconnecting mediahosts repeat their collection announcement. An unchanged
    // type must not rematch every copy while holding the writer needed by auth.
    sqlx::raw_sql("DROP TRIGGER IF EXISTS library_dirty_collection_type;
        CREATE TRIGGER library_dirty_collection_type AFTER UPDATE OF media_type ON collections
        WHEN OLD.media_type IS NOT NEW.media_type BEGIN
          INSERT INTO library_pending SELECT id FROM collection_items WHERE module_id=NEW.module_id AND collection_id=NEW.collection_id ON CONFLICT DO NOTHING;
        END;
        DROP TRIGGER IF EXISTS library_dirty_parts_UPDATE;
        CREATE TRIGGER library_dirty_parts_UPDATE AFTER UPDATE ON playable_source_parts BEGIN
          INSERT INTO library_pending SELECT item_id FROM playable_sources WHERE id IN(OLD.playable_source_id,NEW.playable_source_id) ON CONFLICT DO NOTHING;
          INSERT INTO library_pending SELECT ci.parent_id FROM playable_sources ps JOIN collection_items ci ON ci.id=ps.item_id WHERE ps.id IN(OLD.playable_source_id,NEW.playable_source_id) AND ci.parent_id IS NOT NULL ON CONFLICT DO NOTHING;
        END;").execute(&mut *tx).await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(include_str!("views.sql").to_string()))
        .execute(&mut *tx)
        .await?;
    super::upgrade::repair_episode_copies(&mut tx).await?;
    reconcile(&mut tx).await?;
    tx.commit().await?;
    db.set_before_commit(|connection| Box::pin(reconcile(connection)));
    Ok(())
}

#[derive(Debug)]
struct Identity {
    kind: String,
    title: String,
    year: Option<i64>,
    artist: Option<String>,
    anime: bool,
    parent: Option<String>,
    season: Option<i64>,
    episode: Option<i64>,
    scheme: String,
    recording_id: Option<String>,
    edition: Option<String>,
    ambiguous: bool,
    descriptive: bool,
}

/// Called only inside the hub's serialized writer transaction.
pub async fn reconcile(connection: &mut SqliteConnection) -> Result<()> {
    if !sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM library_pending)")
        .fetch_one(&mut *connection)
        .await?
    {
        return Ok(());
    }
    loop {
        // Adding descendants is bounded by changed parents, never by library size.
        sqlx::query(
            "INSERT INTO library_pending SELECT id FROM collection_items WHERE parent_id IN
        (SELECT collection_item_id FROM library_pending) ON CONFLICT DO NOTHING",
        )
        .execute(&mut *connection)
        .await?;
        let pending: Vec<String> = sqlx::query_scalar("SELECT p.collection_item_id FROM library_pending p
        LEFT JOIN collection_items i ON i.id=p.collection_item_id ORDER BY i.parent_id IS NOT NULL,i.id")
        .fetch_all(&mut *connection).await?;
        if pending.is_empty() {
            break;
        }
        for id in pending {
            sqlx::query("DELETE FROM library_pending WHERE collection_item_id=?")
                .bind(&id)
                .execute(&mut *connection)
                .await?;
            reconcile_copy(connection, &id)
                .await
                .with_context(|| format!("collection item {id}"))?;
        }
        import_state(connection).await?;
    }
    Ok(())
}

async fn reconcile_copy(c: &mut SqliteConnection, source_id: &str) -> Result<()> {
    let Some(r) = sqlx::query("SELECT i.*,c.media_type,COALESCE(m.provider,pm.provider) AS provider,COALESCE(m.provider_id,pm.provider_id) AS provider_id,m.manual,
        pm.title AS assigned_title,pm.premiered,pm.proj_season,pm.proj_episode,
        p.library_item_id AS library_parent
        FROM collection_items i LEFT JOIN collections c ON (c.module_id,c.collection_id)=(i.module_id,i.collection_id)
        LEFT JOIN item_match m ON m.item_id=i.id
        LEFT JOIN provider_metadata pm ON pm.item_id=i.id AND pm.provider=COALESCE(m.provider,(SELECT provider FROM item_match WHERE item_id=i.parent_id))
          AND pm.provider_id<>'' AND (m.provider_id IS NULL OR pm.provider_id=m.provider_id)
          AND NOT EXISTS(SELECT 1 FROM rejected_matches rj WHERE rj.item_id=pm.item_id AND rj.provider=pm.provider AND rj.provider_id=pm.provider_id)
        LEFT JOIN collection_item_library_items p ON p.collection_item_id=i.parent_id AND p.ordinal=1
        WHERE i.id=?")
        .bind(source_id).fetch_optional(&mut *c).await? else {return Ok(());};
    let kind = match r.get::<String, _>("kind").as_str() {
        "show" => "series",
        "track" => "song",
        "movie" => "movie",
        "episode" => "episode",
        "album" => "album",
        _ => return Ok(()),
    }
    .to_string();
    let assigned: Option<String> = r.get("provider");
    let assigned_title: Option<String> = r.get("assigned_title");
    let title = assigned_title
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| r.get("title"));
    // An incomplete assignment must not borrow a different work's detected year.
    let year = if assigned.is_some() {
        r.get::<Option<String>, _>("premiered")
            .and_then(|s| s.get(..4).and_then(|s| s.parse().ok()))
    } else {
        r.get("year")
    };
    let parent: Option<String> = r.get("library_parent");
    let detected_first: Option<i64> = r.get("episode");
    let detected_end: Option<i64> = r.get("episode_end");
    let span = detected_end
        .zip(detected_first)
        .is_some_and(|(end, first)| end > first);
    // A provider projection names one episode, not a mapping of the whole span.
    // Keep unmapped coverage in its source numbering scheme.
    let season = if span {
        r.get("season")
    } else {
        r.get::<Option<i64>, _>("proj_season")
            .or_else(|| r.get("season"))
    };
    let first = if span {
        detected_first
    } else {
        r.get::<Option<i64>, _>("proj_episode").or(detected_first)
    };
    let end = if span {
        detected_end.unwrap()
    } else {
        first.unwrap_or(0)
    };
    let scheme = if kind == "episode" && season.is_none() {
        "absolute"
    } else {
        "aired"
    }
    .to_string();
    let season = if kind == "song" {
        Some(season.unwrap_or(1))
    } else {
        season
    };
    let old: Vec<String> = sqlx::query_scalar(
        "SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal",
    )
    .bind(source_id)
    .fetch_all(&mut *c)
    .await?;
    let tags: Vec<String> = if matches!(kind.as_str(), "album" | "song") {
        sqlx::query_scalar("SELECT f.streams_json FROM files f JOIN playable_source_parts p ON p.file_id=f.id JOIN playable_sources ps ON ps.id=p.playable_source_id JOIN collection_items ci ON ci.id=ps.item_id WHERE ci.id=?1 OR (ci.parent_id=?1 AND ci.kind='track')").bind(source_id).fetch_all(&mut *c).await?
    } else {
        Vec::new()
    };
    let mut editions = std::collections::BTreeSet::new();
    let mut recordings = std::collections::BTreeSet::new();
    // Picard tag mapping, verified 2026-09-08:
    // "MusicBrainz Recording ID" → Vorbis "MUSICBRAINZ_TRACKID";
    // "MusicBrainz Release ID" → "MUSICBRAINZ_ALBUMID".
    // https://picard-docs.musicbrainz.org/en/latest/appendices/tag_mapping.html
    // RELEASETRACKID is an album position, never a recording identity.
    for info in tags {
        if let Ok(info) = serde_json::from_str::<kahawai_core::media::MediaInfo>(&info) {
            for (name, value) in info.tags {
                if !value.trim().is_empty() {
                    let name: String = name
                        .to_lowercase()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if kind == "album"
                        && matches!(name.as_str(), "musicbrainzalbumid" | "musicbrainzreleaseid")
                    {
                        editions.insert(value.to_lowercase());
                    }
                    if kind == "song"
                        && matches!(
                            name.as_str(),
                            "musicbrainztrackid" | "musicbrainzrecordingid"
                        )
                    {
                        recordings.insert(value.to_lowercase());
                    }
                }
            }
        }
    }
    let ambiguous = editions.len() > 1 || recordings.len() > 1;
    let edition = if editions.len() == 1 {
        editions.into_iter().next()
    } else {
        None
    };
    let recording = if recordings.len() == 1 {
        recordings.into_iter().next()
    } else {
        None
    };
    let assignment_manual = r.get::<bool, _>("assignment_manual");
    let manual = assignment_manual || r.get::<Option<i64>, _>("manual").unwrap_or(0) != 0;
    let mut conflict =
        ambiguous.then(|| "Conflicting recording or release identifiers in this copy".to_string());
    let targets = if assignment_manual {
        let ids = canonical_ids(c, &old).await?;
        for id in &ids {
            let target: Option<(String,Option<String>)> = sqlx::query_as("SELECT c.kind,e.series_id FROM library_items c LEFT JOIN episode_details e ON e.item_id=c.id WHERE c.id=? AND c.merged_into IS NULL").bind(id).fetch_optional(&mut *c).await?;
            anyhow::ensure!(
                target.as_ref().is_some_and(|(k, _)| k == &kind),
                "incompatible library assignment"
            );
            if kind == "episode" && target.and_then(|(_, p)| p) != parent {
                conflict = Some("Assigned episode belongs to a different series".to_string());
            }
        }
        ids
    } else {
        let positions: Vec<Option<i64>> = if kind == "episode"
            && let Some(first) = first
            && end >= first
            && end - first < 1000
        {
            (first..=end).map(Some).collect()
        } else {
            vec![first]
        };
        let mut ids = Vec::new();
        for (ordinal, episode) in positions.into_iter().enumerate() {
            let identity = Identity {
                kind: kind.clone(),
                title: if ordinal > 0 {
                    format!("Episode {}", episode.unwrap_or(0))
                } else {
                    title.clone()
                },
                year,
                artist: r.get("artist"),
                anime: r.get::<Option<String>, _>("media_type").as_deref() == Some("anime"),
                parent: parent.clone(),
                season,
                episode,
                scheme: scheme.clone(),
                recording_id: recording.clone(),
                edition: edition.clone(),
                ambiguous,
                descriptive: assigned.is_some() && ordinal == 0,
            };
            ids.push(
                resolve(
                    c,
                    source_id,
                    ordinal,
                    &identity,
                    old.get(ordinal).map(String::as_str),
                )
                .await?,
            );
        }
        ids
    };
    let mut metadata_eligible = true;
    if assignment_manual {
        for target in targets
            .iter()
            .take(if kind == "episode" { 1 } else { targets.len() })
        {
            let item=sqlx::query("SELECT c.title,c.year,c.match_artist,c.edition,c.recording_id,e.series_id,e.season,e.episode FROM library_items c LEFT JOIN episode_details e ON e.item_id=c.id WHERE c.id=?").bind(target).fetch_one(&mut *c).await?;
            metadata_eligible &= if kind == "episode" {
                item.get::<Option<String>, _>("series_id") == parent
                    && item.get::<Option<i64>, _>("season")
                        == if scheme == "absolute" { None } else { season }
                    && item.get::<Option<i64>, _>("episode") == first
            } else if kind == "album" {
                name_key(&item.get::<String, _>("title")) == name_key(&title)
                    && item.get::<Option<i64>, _>("year") == year
                    && item.get::<Option<String>, _>("match_artist")
                        == r.get::<Option<String>, _>("artist")
                            .as_deref()
                            .map(name_key)
                    && item.get::<Option<String>, _>("edition") == edition
            } else if kind == "song" {
                let target_recording = item.get::<Option<String>, _>("recording_id");
                if let (Some(a), Some(b)) = (&recording, &target_recording) {
                    a == b
                } else {
                    sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM album_tracks WHERE song_id=? AND album_id=? AND disc_number=? AND track_number=?)")
                    .bind(target).bind(&parent).bind(season.unwrap_or(1)).bind(first).fetch_one(&mut *c).await?
                }
            } else {
                name_key(&item.get::<String, _>("title")) == name_key(&title)
                    && item.get::<Option<i64>, _>("year") == year
            };
        }
    }
    let unresolved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_items WHERE unidentified=1 AND id IN(SELECT value FROM json_each(?)))").bind(serde_json::to_string(&targets)?).fetch_one(&mut *c).await?;
    let mode = if manual {
        "manual"
    } else if unresolved {
        "unmatched"
    } else {
        "automatic"
    };
    // A queued source change invalidates the edit token, even if its chosen
    // item stays the same. No second copy of matching inputs is stored.
    sqlx::query("UPDATE collection_items SET assignment_revision=assignment_revision+1,match_mode=?,match_conflict=?,metadata_eligible=? WHERE id=?")
        .bind(mode).bind(conflict).bind(metadata_eligible).bind(source_id).execute(&mut *c).await?;
    if old != targets {
        sqlx::query("DELETE FROM collection_item_library_items WHERE collection_item_id=?")
            .bind(source_id)
            .execute(&mut *c)
            .await?;
        for (ordinal, id) in targets.iter().enumerate() {
            sqlx::query("INSERT INTO collection_item_library_items VALUES(?,?,?)")
                .bind(source_id)
                .bind(ordinal as i64 + 1)
                .bind(id)
                .execute(&mut *c)
                .await?;
            if let Some(old) = old.get(ordinal) {
                promote_state(c, old, id).await?;
            }
        }
    }
    if r.get::<i64, _>("assignment_revision") == 0 {
        import_preferences(c, source_id, &targets).await?;
    }
    // Album track identity remains available after its last copy is reassigned.
    // Only the copy's pointer determines current membership and visibility.
    let album_track_id = if kind == "song"
        && let (Some(album), Some(track), Some(song)) = (parent, first, targets.first())
    {
        Some(sqlx::query_scalar::<_,i64>("INSERT INTO album_tracks(album_id,song_id,disc_number,track_number) VALUES(?,?,?,?) ON CONFLICT(album_id,disc_number,track_number,song_id) DO UPDATE SET song_id=excluded.song_id RETURNING id")
            .bind(album).bind(song).bind(season.unwrap_or(1)).bind(track).fetch_one(&mut *c).await?)
    } else {
        None
    };
    sqlx::query("UPDATE collection_items SET album_track_id=? WHERE id=?")
        .bind(album_track_id)
        .bind(source_id)
        .execute(&mut *c)
        .await?;
    Ok(())
}

async fn resolve(
    c: &mut SqliteConnection,
    source: &str,
    ordinal: usize,
    i: &Identity,
    old: Option<&str>,
) -> Result<String> {
    let parent_known = if let Some(parent) = &i.parent {
        !sqlx::query_scalar::<_, bool>("SELECT unidentified FROM library_items WHERE id=?")
            .bind(parent)
            .fetch_one(&mut *c)
            .await?
    } else {
        false
    };
    let can_match = !i.ambiguous
        && match i.kind.as_str() {
            "movie" | "series" => !name_key(&i.title).is_empty() && i.year.is_some(),
            "album" => {
                !name_key(&i.title).is_empty()
                    && i.year.is_some()
                    && i.artist.as_ref().is_some_and(|a| !name_key(a).is_empty())
            }
            "episode" => parent_known && i.episode.is_some(),
            "song" => i.recording_id.is_some() || (parent_known && i.episode.is_some()),
            _ => false,
        };
    let matches = if can_match {
        matching_items(c, source, i).await?
    } else {
        Vec::new()
    };
    let refused = matches.iter().any(|(_, rejected)| *rejected);
    let candidates: Vec<String> = matches
        .into_iter()
        .filter_map(|(id, rejected)| (!rejected).then_some(id))
        .collect();
    if candidates.len() == 1 {
        let id = candidates.into_iter().next().unwrap();
        if i.kind == "song" && i.recording_id.is_some() {
            sqlx::query(
                "UPDATE library_items SET recording_id=COALESCE(recording_id,?) WHERE id=?",
            )
            .bind(&i.recording_id)
            .bind(&id)
            .execute(&mut *c)
            .await?;
        }
        if i.anime {
            sqlx::query("UPDATE library_items SET anime=1 WHERE id=? AND anime=0")
                .bind(&id)
                .execute(&mut *c)
                .await?;
        }
        if i.kind == "song" || (i.descriptive && i.kind == "episode") {
            // A selected provider description outranks another copy's file tags.
            // Copy ID breaks equal ranks so unrelated updates cannot change it.
            let title = if i.kind == "song" {
                sqlx::query_scalar::<_, String>("SELECT pm.title FROM collection_items ci JOIN item_match m ON m.item_id=ci.id JOIN provider_metadata pm ON pm.item_id=ci.id AND pm.provider=m.provider AND pm.provider_id=m.provider_id
                    WHERE pm.title IS NOT NULL AND trim(pm.title)<>'' AND (ci.id=?1 OR (ci.metadata_eligible=1 AND EXISTS(SELECT 1 FROM collection_item_library_items a WHERE a.collection_item_id=ci.id AND a.library_item_id=?2 AND a.ordinal=1)))
                    ORDER BY m.manual DESC,ci.id LIMIT 1")
                    .bind(source).bind(&id).fetch_optional(&mut *c).await?.unwrap_or_else(|| i.title.clone())
            } else {
                i.title.clone()
            };
            sqlx::query("UPDATE library_items SET title=?,norm_title=?,sort_title=?,revision=revision+1 WHERE id=? AND title<>?")
                .bind(&title).bind(crate::enrich::fold(&title)).bind(name_key(&title)).bind(&id).bind(&title).execute(&mut *c).await?;
        }
        return Ok(id);
    }
    // Ambiguity cannot be resolved by provider order or insertion order.
    let unidentified = !can_match || candidates.len() > 1 || (candidates.is_empty() && refused);
    let reusable: Option<String> = if let Some(id) = old {
        sqlx::query_scalar(
            "SELECT id FROM library_items WHERE id=? AND unidentified=1 AND merged_into IS NULL",
        )
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
    } else {
        None
    };
    let id = if let Some(id) = reusable {
        id
    } else {
        let used: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM library_items WHERE id=?)")
                .bind(source)
                .fetch_one(&mut *c)
                .await?;
        if ordinal == 0 && !used {
            source.to_string()
        } else {
            ulid::Ulid::generate().to_string()
        }
    };
    store_item(c, &id, i, unidentified, source).await?;
    Ok(id)
}

/// Ordinary typed comparisons; multiple matches require an explicit choice.
async fn matching_items(
    c: &mut SqliteConnection,
    source: &str,
    i: &Identity,
) -> Result<Vec<(String, bool)>> {
    let ids: Vec<String> = match i.kind.as_str() {
        "movie" | "series" => sqlx::query_scalar("SELECT id FROM library_items WHERE kind=? AND year=? AND sort_title=? AND unidentified=0 AND merged_into IS NULL")
            .bind(&i.kind).bind(i.year).bind(name_key(&i.title)).fetch_all(&mut *c).await?,
        "album" => sqlx::query_scalar("SELECT id FROM library_items WHERE kind='album' AND year=? AND match_artist=? AND sort_title=? AND edition IS ? AND unidentified=0 AND merged_into IS NULL")
            .bind(i.year).bind(i.artist.as_deref().map(name_key)).bind(name_key(&i.title)).bind(&i.edition).fetch_all(&mut *c).await?,
        "episode" => sqlx::query_scalar("SELECT i.id FROM episode_details e JOIN library_items i ON i.id=e.item_id WHERE e.series_id=? AND e.numbering=? AND e.season IS ? AND e.episode=? AND i.kind='episode' AND i.unidentified=0 AND i.merged_into IS NULL")
            .bind(&i.parent).bind(&i.scheme).bind(if i.scheme=="absolute" { None } else { i.season }).bind(i.episode).fetch_all(&mut *c).await?,
        "song" => {
            let mut ids:Vec<String> = if let Some(recording)=&i.recording_id {
                sqlx::query_scalar("SELECT id FROM library_items WHERE recording_id=? AND kind='song' AND unidentified=0 AND merged_into IS NULL")
                    .bind(recording).fetch_all(&mut *c).await?
            } else { Vec::new() };
            let positions:Vec<String>=sqlx::query_scalar("SELECT i.id FROM album_tracks a JOIN library_items i ON i.id=a.song_id WHERE a.album_id=? AND a.disc_number=? AND a.track_number=? AND i.kind='song' AND i.unidentified=0 AND i.merged_into IS NULL AND (?4 IS NULL OR i.recording_id IS NULL OR i.recording_id=?4)")
                .bind(&i.parent).bind(i.season.unwrap_or(1)).bind(i.episode).bind(&i.recording_id).fetch_all(&mut *c).await?;
            ids.extend(positions);
            ids.sort(); ids.dedup(); ids
        }
        _ => anyhow::bail!("unsupported library item kind"),
    };
    let rejected: Vec<String> = sqlx::query_scalar(
        "SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id=?",
    )
    .bind(source)
    .fetch_all(&mut *c)
    .await?;
    Ok(ids
        .into_iter()
        .map(|id| {
            let refused = rejected.contains(&id);
            (id, refused)
        })
        .collect())
}

async fn store_item(
    c: &mut SqliteConnection,
    id: &str,
    i: &Identity,
    unidentified: bool,
    added_id: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO library_items(id,kind,title,norm_title,sort_title,year,artist,artist_key,norm_artist,anime,unidentified,added_id,match_artist,edition,recording_id)
        VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,norm_title=excluded.norm_title,sort_title=excluded.sort_title,
        year=excluded.year,artist=excluded.artist,artist_key=excluded.artist_key,norm_artist=excluded.norm_artist,anime=excluded.anime,unidentified=excluded.unidentified,
        match_artist=excluded.match_artist,edition=excluded.edition,recording_id=excluded.recording_id,revision=library_items.revision+1")
        .bind(id).bind(&i.kind).bind(&i.title).bind(crate::enrich::fold(&i.title)).bind(name_key(&i.title)).bind(i.year).bind(&i.artist)
        .bind(i.artist.as_deref().map(crate::enrich::artist_key)).bind(i.artist.as_deref().map(crate::enrich::fold)).bind(i.anime).bind(unidentified).bind(added_id)
        .bind(i.artist.as_deref().map(name_key)).bind(&i.edition).bind(&i.recording_id).execute(&mut *c).await?;
    if i.kind == "episode"
        && let Some(parent) = &i.parent
    {
        sqlx::query("INSERT INTO episode_details(item_id,series_id,season,episode,numbering) VALUES(?,?,?,?,?) ON CONFLICT(item_id) DO UPDATE SET series_id=excluded.series_id,season=excluded.season,episode=excluded.episode,numbering=excluded.numbering")
            .bind(id).bind(parent).bind(if i.scheme=="absolute" {None} else {i.season}).bind(i.episode).bind(&i.scheme).execute(&mut *c).await?;
    }
    Ok(())
}

/// Create a distinct library item. Equal identifying fields require a manual choice.
pub async fn create(c: &mut SqliteConnection, item: NewItem) -> Result<String> {
    anyhow::ensure!(
        matches!(
            item.kind.as_str(),
            "movie" | "series" | "episode" | "album" | "song"
        ),
        "unknown library kind"
    );
    anyhow::ensure!(
        !item.title.trim().is_empty() && item.title.len() <= 1000,
        "title required (at most 1000 bytes)"
    );
    if matches!(item.kind.as_str(), "episode" | "song") {
        let parent = item.parent_id.as_deref().context("parent required")?;
        let kind: String =
            sqlx::query_scalar("SELECT kind FROM library_items WHERE id=? AND merged_into IS NULL")
                .bind(parent)
                .fetch_one(&mut *c)
                .await?;
        anyhow::ensure!(
            kind == if item.kind == "episode" {
                "series"
            } else {
                "album"
            },
            "incompatible parent"
        );
    }
    let id = ulid::Ulid::generate().to_string();
    let absolute = item.kind == "episode" && item.season.is_none();
    let identity = Identity {
        kind: item.kind,
        title: item.title,
        year: item.year,
        artist: item.artist,
        anime: false,
        parent: item.parent_id,
        season: if absolute { Some(0) } else { item.season },
        episode: item.episode,
        scheme: if absolute { "absolute" } else { "aired" }.into(),
        edition: item.edition,
        recording_id: None,
        ambiguous: false,
        descriptive: true,
    };
    store_item(c, &id, &identity, false, &id).await?;
    Ok(id)
}
