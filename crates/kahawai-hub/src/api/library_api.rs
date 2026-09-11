//! Public item reads use library item IDs. Collection copies are explicit context
//! for matching and source playback, never the unit of browse pagination.
use super::*;

#[derive(Serialize, ToSchema)]
pub(super) struct CollectionCopy {
    pub id: String,
    pub title: String,
    pub year: Option<i64>,
    pub artist: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub parent_library_item_id: Option<String>,
    pub module_id: Option<String>,
    pub host_name: Option<String>,
    pub collection_id: Option<String>,
    pub paths: Vec<String>,
    pub match_confidence: Option<String>,
    /// The selected provider record's title, without display-field fallbacks.
    pub matched_title: Option<String>,
    /// The selected provider record's release year, if supplied.
    pub matched_year: Option<i64>,
    pub assignment: crate::library::Assignment,
}

pub(super) async fn apply_match(
    state: &AppState,
    id: &str,
    body: ApplyMatch,
) -> Result<crate::library::Assignment, ApiError> {
    apply_decision(state, id, Some(body.expected_revision), body.decision).await
}

pub(super) async fn apply_decision(
    state: &AppState,
    id: &str,
    expected_revision: Option<i64>,
    body: MatchDecision,
) -> Result<crate::library::Assignment, ApiError> {
    let mut tx = state.registry.db().begin().await.map_err(internal)?;
    let current = crate::library::assignment(&mut tx, id)
        .await
        .map_err(|_| hidden("collection item"))?;
    if expected_revision.is_some_and(|revision| current.revision != revision) {
        return Err(ApiError::new(
            ErrorCode::StaleWrite,
            "This copy's assignment changed. Reload it before applying a match.",
        ));
    }
    match body.action.as_str() {
        "new" => {
            let definition = body
                .new_item
                .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "new_item required"))?;
            let source_kind: String =
                sqlx::query_scalar("SELECT kind FROM collection_items WHERE id=?")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(internal)?;
            let kind = match source_kind.as_str() {
                "show" => "series",
                "track" => "song",
                k => k,
            };
            if kind != definition.kind {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "New entry must have this copy's kind",
                ));
            }
            let target = crate::library::create(&mut tx, definition)
                .await
                .map_err(|e| ApiError::new(ErrorCode::BadRequest, e.to_string()))?;
            crate::library::assign(&mut tx, id, &[target])
                .await
                .map_err(|e| ApiError::new(ErrorCode::BadRequest, e.to_string()))?;
        }
        "pick" => {
            let candidate = body
                .candidate
                .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "candidate required"))?;
            let provider = body
                .provider
                .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "provider required"))?;
            let provider_id = candidate
                .id
                .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "candidate.id required"))?
                .to_string();
            sqlx::query("UPDATE collection_items SET assignment_manual=0 WHERE id=?")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            crate::providers::assign_manual_in(
                &mut tx,
                id,
                &provider,
                &provider_id,
                crate::providers::Fields {
                    title: candidate.title,
                    overview: candidate.overview,
                    poster_path: candidate.poster_path,
                    rating: candidate.vote_average,
                    premiered: candidate.release_date,
                    ..Default::default()
                },
            )
            .await
            .map_err(internal)?;
        }
        "assign" | "confirm" => {
            let ids = if body.action == "confirm" {
                crate::providers::confirm_assignment_in(&mut tx, id)
                    .await
                    .map_err(internal)?;
                current.library_item_ids
            } else {
                body.library_item_ids.ok_or_else(|| {
                    ApiError::new(ErrorCode::BadRequest, "library_item_ids required")
                })?
            };
            crate::library::assign(&mut tx, id, &ids)
                .await
                .map_err(|e| ApiError::new(ErrorCode::BadRequest, e.to_string()))?;
        }
        "reject" | "reset" => {
            if body.action == "reject" {
                for target in &current.library_item_ids {
                    sqlx::query(
                        "INSERT INTO rejected_library_matches VALUES(?,?) ON CONFLICT DO NOTHING",
                    )
                    .bind(id)
                    .bind(target)
                    .execute(&mut *tx)
                    .await
                    .map_err(internal)?;
                }
                crate::providers::reject_matches_in(&mut tx, id)
                    .await
                    .map_err(internal)?;
            }
            sqlx::query("UPDATE collection_items SET assignment_manual=0 WHERE id=?")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            sqlx::query("DELETE FROM manual_match WHERE item_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        _ => return Err(ApiError::new(ErrorCode::BadRequest, "Unknown match action")),
    }
    crate::library::reconcile(&mut tx).await.map_err(internal)?;
    let result = crate::library::assignment(&mut tx, id)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(result)
}

const VISIBLE: &str = "EXISTS(SELECT 1 FROM collection_item_library_items a JOIN collection_items ci ON ci.id=a.collection_item_id
    WHERE a.library_item_id=c.id AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1))
    OR EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id
    WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(ci.module_id,ci.collection_id))))";

pub(super) fn visible(alias: &str) -> String {
    VISIBLE.replace("c.id", &format!("{alias}.id"))
}

/// An album child must have an accessible copy on this particular album.
pub(super) fn album_child(alias: &str, parent: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM album_copies at JOIN collection_items ac ON ac.id=at.collection_item_id WHERE at.album_id={parent} AND at.song_id={alias}.id AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(ac.module_id,ac.collection_id))))"
    )
}

// The artwork route tries each eligible, authorized copy. Capture those stored
// inputs on the returned page only: no artwork reads, provider calls or changes
// to cache retention. Parent metadata supplies an episode's fallback poster.
const ARTWORK_INPUTS: &str = "(SELECT json_group_array(json_array(id,assignment_revision,matched,updated_at,poster_path,parent_updated_at,parent_poster_path))
    FROM (SELECT ci.id,ci.assignment_revision,EXISTS(SELECT 1 FROM item_match m WHERE m.item_id=ci.id) AS matched,
        md.updated_at,md.poster_path,pmd.updated_at AS parent_updated_at,pmd.poster_path AS parent_poster_path
      FROM collection_item_library_items a JOIN collection_items ci ON ci.id=a.collection_item_id
      LEFT JOIN resolved_metadata md ON md.item_id=ci.id
      LEFT JOIN collection_items eligible_parent ON eligible_parent.id=ci.parent_id AND eligible_parent.metadata_eligible=1
      LEFT JOIN resolved_metadata pmd ON pmd.item_id=eligible_parent.id
      WHERE a.library_item_id=c.id AND a.ordinal=1 AND ci.metadata_eligible=1
        AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(
          SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id
          WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(ci.module_id,ci.collection_id)))
      ORDER BY ci.id))";

pub(super) fn artwork_version(row: &sqlx::sqlite::SqliteRow) -> Option<i64> {
    if let Ok(inputs) = row.try_get::<String, _>("artwork_inputs") {
        let revision: i64 = row.get("artwork_revision");
        // Keep the existing numeric API shape and exact JavaScript integer
        // representation. A fingerprint catches changes below another donor's
        // maximum timestamp, same-second replacements and donor removal.
        Some(
            (xxhash_rust::xxh3::xxh3_64(format!("{revision}:{inputs}").as_bytes())
                & ((1_u64 << 53) - 1)) as i64,
        )
    } else {
        row.try_get("art_version").ok().flatten()
    }
}

pub(super) fn page(inner: &str, order: &str) -> String {
    page_context(inner, order, false)
}
fn page_context(inner: &str, order: &str, album: bool) -> String {
    let source_scope = if album {
        " AND (c.kind<>'song' OR EXISTS(SELECT 1 FROM album_copies ac WHERE ac.album_id=?2 AND ac.collection_item_id=ps.collection_item_id))"
    } else {
        ""
    };
    let copy_scope = if album {
        " AND (c.kind<>'song' OR EXISTS(SELECT 1 FROM album_copies ac WHERE ac.album_id=?2 AND ac.collection_item_id=src.id))"
    } else {
        " AND (?2='' OR EXISTS(SELECT 1 FROM library_collections lc WHERE lc.library_id=?2 AND (lc.module_id,lc.collection_id)=(src.module_id,src.collection_id)))"
    };
    let projection_scope = format!(
        "{} AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(pc.module_id,pc.collection_id)))",
        copy_scope.replace("src.", "pc.")
    );
    let projection =
        crate::providers::library_episode_projection_sql("c.id", &projection_scope, false);
    format!("SELECT c.id,c.kind,c.title,c.year,c.artist,CASE WHEN c.kind='song' THEN ci.album_track_id END AS album_track_id,CASE WHEN c.kind='song' THEN ci.season ELSE c.season END AS season,CASE WHEN c.kind='song' THEN ci.episode ELSE c.episode END AS episode,COALESCE(c.parent_id,(SELECT library_item_id FROM collection_item_library_items a WHERE a.collection_item_id=ci.parent_id AND a.ordinal=1)) AS parent_id,c.episode_end,
        (SELECT title FROM library_items WHERE id=COALESCE(c.parent_id,(SELECT library_item_id FROM collection_item_library_items a WHERE a.collection_item_id=ci.parent_id AND a.ordinal=1))) AS parent_title,
        ci.title AS file_title,ci.year AS file_year,md.title AS matched_title,CASE WHEN ci.match_mode='manual' THEN 'manual' ELSE md.confidence END AS match_confidence,
        CASE WHEN c.kind='episode' THEN json_extract({projection},'$[0]') END AS proj_season,
        CASE WHEN c.kind='episode' THEN json_extract({projection},'$[1]') END AS proj_episode,
        {ARTWORK_INPUTS} AS artwork_inputs,c.revision AS artwork_revision,
        (SELECT COUNT(*) FROM library_sources ps WHERE ps.item_id=c.id {source_scope} AND
          (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(
          SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id
          WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(ps.module_id,ps.collection_id)))) AS sources,
        (SELECT lc.library_id FROM library_membership lc WHERE lc.item_id=c.id
          AND (c.kind<>'song' OR (lc.module_id,lc.collection_id)=(ci.module_id,ci.collection_id)) AND
          (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(
            SELECT 1 FROM user_libraries ul WHERE ul.library_id=lc.library_id AND ul.user_id=?1))
          ORDER BY lc.library_id<>?2,lc.library_id LIMIT 1) AS library_id,
        w.position_ms,w.duration_ms,w.played,w.play_count
        FROM ({inner}) page JOIN library_entries c ON c.id=page.id
        LEFT JOIN collection_items ci ON ci.id=(SELECT a.collection_item_id FROM collection_item_library_items a JOIN collection_items src ON src.id=a.collection_item_id
          WHERE a.library_item_id=c.id {copy_scope} AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(
            SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(src.module_id,src.collection_id)))
          ORDER BY (src.metadata_eligible AND a.ordinal=1) DESC,EXISTS(SELECT 1 FROM item_match m WHERE m.item_id=src.id) DESC,src.id LIMIT 1)
        LEFT JOIN resolved_metadata md ON md.item_id=ci.id AND ci.metadata_eligible=1 AND EXISTS(SELECT 1 FROM collection_item_library_items ma WHERE ma.collection_item_id=ci.id AND ma.library_item_id=c.id AND ma.ordinal=1)
        LEFT JOIN user_item_state w ON w.item_id=c.id AND w.user_id=?1 ORDER BY {order}")
}

pub(super) async fn browse(
    state: &AppState,
    claims: &crate::auth::Claims,
    q: ItemsQuery,
) -> Result<ItemsResponse, ApiError> {
    let db = state.registry.db();
    if let Some(lib) = &q.library
        && !crate::grants::can_see_library(db, claims, lib)
            .await
            .map_err(internal)?
    {
        return Err(hidden("library"));
    }
    let limit = q.limit.unwrap_or(ITEMS_PAGE_DEFAULT).min(ITEMS_PAGE_MAX);
    let offset = q.offset.unwrap_or(0);
    let needle =
        q.q.as_deref()
            .map(crate::enrich::fold)
            .filter(|x| !x.is_empty());
    // A library grant already authorizes every copy in that library. One
    // membership lookup both limits the page and proves that it has a copy.
    let mut filter = if q.library.is_some() {
        "(EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(SELECT 1 FROM user_libraries WHERE user_id=?1 AND library_id=?2)) AND EXISTS(SELECT 1 FROM library_membership m WHERE m.item_id=c.id AND m.library_id=?2)"
            .to_string()
    } else {
        VISIBLE.to_string()
    };
    if needle.is_some() {
        // Reject non-matching titles before visiting membership/grant joins.
        filter = format!(
            "(c.norm_title LIKE '%'||?3||'%' OR (c.kind='album' AND c.norm_artist LIKE '%'||?3||'%') OR EXISTS(SELECT 1 FROM collection_item_library_items a JOIN collection_items ci ON ci.id=a.collection_item_id WHERE a.library_item_id=c.id AND ci.norm_title LIKE '%'||?3||'%' AND (EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1)) OR EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(ci.module_id,ci.collection_id))))) AND {filter}"
        );
    } else if !q.in_progress.unwrap_or(false) {
        filter.push_str(" AND c.kind IN('movie','series','album')");
    }
    let order = if q.in_progress.unwrap_or(false) {
        filter.push_str(" AND c.kind<>'song' AND EXISTS(SELECT 1 FROM user_item_state w WHERE w.item_id=c.id AND w.user_id=?1 AND w.played=0 AND w.position_ms>=60000 AND (w.duration_ms IS NULL OR w.position_ms*100>=w.duration_ms))");
        "(SELECT updated_at FROM user_item_state WHERE item_id=c.id AND user_id=?1) DESC,c.id DESC"
    } else {
        match q.sort.as_deref() {
            Some("year") => "c.year IS NULL,c.year,c.sort_title,c.id",
            Some("-year") => "c.year IS NULL,c.year DESC,c.sort_title,c.id",
            Some("added") => "c.added_id,c.id",
            Some("-added") => "c.added_id DESC,c.id DESC",
            Some("-title") => "c.sort_title DESC,c.id DESC",
            _ => "c.sort_title,c.id",
        }
    };
    let inner = format!(
        "SELECT c.id FROM library_entries c WHERE {filter} ORDER BY {order} LIMIT ?4 OFFSET ?5"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(page(&inner, order)))
        .bind(&claims.sub)
        .bind(q.library.as_deref().unwrap_or(""))
        .bind(needle.as_deref().unwrap_or(""))
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await
        .map_err(internal)?;
    let total = if rows.len() < limit as usize && (!rows.is_empty() || offset == 0) {
        offset as i64 + rows.len() as i64
    } else {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM library_entries c WHERE {filter}"
        )))
        .bind(&claims.sub)
        .bind(q.library.as_deref().unwrap_or(""))
        .bind(needle.as_deref().unwrap_or(""))
        .fetch_one(db)
        .await
        .map_err(internal)?
    };
    Ok(ItemsResponse {
        items: rows
            .iter()
            .map(|r| item_row(r, r.get::<i64, _>("sources")))
            .collect(),
        total,
        limit,
        offset,
    })
}

pub(super) async fn children(
    state: &AppState,
    claims: &crate::auth::Claims,
    id: &str,
) -> Result<ChildrenResponse, ApiError> {
    let canonical = crate::library::resolve_id(state.registry.db(), id)
        .await
        .map_err(internal)?;
    let id = canonical.as_str();
    let album = album_child("c", "?2");
    let inner = format!(
        "SELECT c.id FROM library_entries c WHERE (c.parent_id=?2 OR {album}) AND {VISIBLE}"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(page_context(
        &inner,
        "c.season,c.episode,c.id",
        true,
    )))
    .bind(&claims.sub)
    .bind(id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let mut children = Vec::new();
    for row in &rows {
        let item_id: String = row.get("id");
        let copies = crate::library::copies(state.registry.db(), &claims.sub, &item_id)
            .await
            .map_err(internal)?;
        let contexts: Vec<(Option<i64>, Option<i64>, Option<i64>)> = if row.get::<String, _>("kind")
            == "song"
        {
            sqlx::query_as("SELECT DISTINCT id,disc_number,track_number FROM album_copies WHERE album_id=? AND song_id=? AND collection_item_id IN(SELECT value FROM json_each(?)) ORDER BY disc_number,track_number,id")
                .bind(id).bind(&item_id).bind(serde_json::to_string(&copies).map_err(internal)?).fetch_all(state.registry.db()).await.map_err(internal)?
        } else {
            vec![(None, row.get("season"), row.get("episode"))]
        };
        for (album_track, disc, track) in contexts {
            let mut item = item_row(row, row.get::<i64, _>("sources"));
            let scoped = if let Some(track_id) = album_track {
                let allowed: Vec<String> =
                    sqlx::query_scalar("SELECT id FROM collection_items WHERE album_track_id=?")
                        .bind(track_id)
                        .fetch_all(state.registry.db())
                        .await
                        .map_err(internal)?;
                item.parent_id = Some(id.into());
                copies
                    .iter()
                    .filter(|copy| allowed.contains(copy))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                copies.clone()
            };
            item.album_track_id = album_track;
            item.season = disc;
            item.episode = track;
            let list = serde_json::to_string(&scoped).map_err(internal)?;
            item.sources=sqlx::query_scalar("SELECT COUNT(*) FROM playable_sources WHERE item_id IN(SELECT value FROM json_each(?))")
                .bind(&list).fetch_one(state.registry.db()).await.map_err(internal)?;
            item.duration_ms=sqlx::query_scalar("SELECT MIN(d) FROM (SELECT SUM(json_extract(f.streams_json,'$.duration_ms')) d FROM playable_sources ps
                JOIN playable_source_parts p ON p.playable_source_id=ps.id JOIN files f ON f.id=p.file_id
                WHERE ps.item_id IN(SELECT value FROM json_each(?)) GROUP BY ps.id
                HAVING COUNT(DISTINCT p.ordinal)=ps.expected_parts AND MAX(p.ordinal)=ps.expected_parts AND COUNT(json_extract(f.streams_json,'$.duration_ms'))=ps.expected_parts)")
                .bind(&list).fetch_one(state.registry.db()).await.map_err(internal)?;
            let gain:Option<String>=sqlx::query_scalar("SELECT json_extract(f.streams_json,'$.replay_gain') FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id AND p.ordinal=1 JOIN files f ON f.id=p.file_id WHERE ps.item_id IN(SELECT value FROM json_each(?)) ORDER BY ps.id LIMIT 1")
                .bind(&list).fetch_optional(state.registry.db()).await.map_err(internal)?.flatten();
            item.replay_gain = gain.and_then(|s| serde_json::from_str(&s).ok());
            children.push(item);
        }
    }
    children.sort_by_key(|x| (x.season, x.episode, x.id.clone()));
    Ok(ChildrenResponse { children })
}

pub(super) async fn item_body(
    state: &AppState,
    id: &str,
    user: &str,
    streams: bool,
) -> Result<ItemDetailResponse, ApiError> {
    let db = state.registry.db();
    let canonical = crate::library::resolve_id(db, id).await.map_err(internal)?;
    let id = canonical.as_str();
    let copies = crate::library::copies(db, user, id)
        .await
        .map_err(internal)?;
    let source = copies.first().ok_or_else(|| hidden("item"))?;
    let mut out = collection_body(state, source, user, streams).await?;
    let metadata_eligible: bool =
        sqlx::query_scalar("SELECT metadata_eligible AND EXISTS(SELECT 1 FROM collection_item_library_items WHERE collection_item_id=ci.id AND library_item_id=?2 AND ordinal=1) FROM collection_items ci WHERE id=?1")
            .bind(source).bind(id)
            .fetch_one(db)
            .await
            .map_err(internal)?;
    if !metadata_eligible {
        out.metadata = None;
        out.item.premiered = None;
        out.item.proj_season = None;
        out.item.proj_episode = None;
        out.related.clear();
        out.item.matched_title = None;
        out.item.match_confidence = Some("manual".into());
    }
    for copy in copies.iter().skip(1) {
        let another = collection_body(state, copy, user, streams).await?;
        out.item.sources.extend(another.item.sources);
        let eligible: bool =
            sqlx::query_scalar("SELECT metadata_eligible AND EXISTS(SELECT 1 FROM collection_item_library_items WHERE collection_item_id=ci.id AND library_item_id=?2 AND ordinal=1) FROM collection_items ci WHERE id=?1")
                .bind(copy).bind(id)
                .fetch_one(db)
                .await
                .map_err(internal)?;
        if eligible && let Some(extra) = another.metadata {
            if let Some(md) = &mut out.metadata {
                md.overview = md.overview.take().or(extra.overview);
                md.rating = md.rating.or(extra.rating);
                md.original_language = md.original_language.take().or(extra.original_language);
                md.genres = md.genres.take().or(extra.genres);
                md.cast = md.cast.take().or(extra.cast);
            } else {
                out.metadata = Some(extra);
            }
        }
    }
    for copy in &copies {
        let row=sqlx::query("SELECT ci.title,ci.year,ci.artist,ci.season,ci.episode,ci.module_id,s.name AS host_name,ci.collection_id,
            CASE WHEN ci.match_mode='manual' THEN 'manual' ELSE md.confidence END AS match_confidence,
            matched.title AS matched_title,matched.premiered AS matched_premiered,
            (SELECT library_item_id FROM collection_item_library_items a WHERE a.collection_item_id=ci.parent_id AND a.ordinal=1) AS parent_library_item_id
            FROM collection_items ci LEFT JOIN resolved_metadata md ON md.item_id=ci.id
            LEFT JOIN provider_metadata matched ON matched.item_id=ci.id AND matched.provider=md.provider AND matched.provider_id=md.provider_id
            LEFT JOIN satellites s ON s.module_id=ci.module_id WHERE ci.id=?").bind(copy).fetch_one(db).await.map_err(internal)?;
        let paths=sqlx::query_scalar("SELECT f.path_rel FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id JOIN files f ON f.id=p.file_id WHERE ps.item_id=? ORDER BY ps.id,p.ordinal").bind(copy).fetch_all(db).await.map_err(internal)?;
        let mut read = db.read_pool().acquire().await.map_err(internal)?;
        let assignment = crate::library::assignment(&mut read, copy)
            .await
            .map_err(internal)?;
        out.copies.push(CollectionCopy {
            id: copy.clone(),
            title: row.get("title"),
            year: row.get("year"),
            artist: row.get("artist"),
            season: row.get("season"),
            episode: row.get("episode"),
            parent_library_item_id: row.get("parent_library_item_id"),
            module_id: row.get("module_id"),
            host_name: row.get("host_name"),
            collection_id: row.get("collection_id"),
            paths,
            match_confidence: row.get("match_confidence"),
            matched_title: row.get("matched_title"),
            matched_year: row
                .get::<Option<String>, _>("matched_premiered")
                .and_then(|date| date.get(..4).and_then(|year| year.parse().ok())),
            assignment,
        });
    }
    if out
        .copies
        .first()
        .is_some_and(|copy| copy.assignment.mode == "manual")
    {
        out.item.match_confidence = Some("manual".into());
    }
    let r=sqlx::query(sqlx::AssertSqlSafe(format!("SELECT c.*,{ARTWORK_INPUTS} AS artwork_inputs,c.revision AS artwork_revision,w.position_ms,w.duration_ms,w.played,w.play_count FROM library_entries c LEFT JOIN user_item_state w ON w.item_id=c.id AND w.user_id=?1 WHERE c.id=?2"))).bind(user).bind(id).fetch_one(db).await.map_err(internal)?;
    out.item.art_version = artwork_version(&r);
    out.library_revision = r.get("revision");
    if let Some(fields) = sqlx::query_scalar::<_, String>(
        "SELECT fields FROM library_overrides WHERE library_item_id=?",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map_err(internal)?
    {
        let fields: crate::library::MetadataOverrides =
            serde_json::from_str(&fields).map_err(internal)?;
        if fields.overview.is_some()
            || fields.rating.is_some()
            || fields.original_language.is_some()
            || fields.genres.is_some()
        {
            let md = out.metadata.get_or_insert_with(ItemMetadata::default);
            md.overview = fields.overview.or(md.overview.take());
            md.rating = fields.rating.or(md.rating);
            md.original_language = fields.original_language.or(md.original_language.take());
            md.genres = fields.genres.or(md.genres.take());
            md.confidence = "manual".into();
        }
    }
    out.item.id = id.into();
    out.item.kind = r.get("kind");
    out.item.title = r.get("title");
    out.item.year = r.get("year");
    out.item.artist = r.get("artist");
    out.item.parent_id = r.get("parent_id");
    if out.item.kind == "song" {
        out.item.parent_id = out
            .copies
            .first()
            .and_then(|c| c.parent_library_item_id.clone());
    }
    out.item.season = r.get("season");
    out.item.episode = r.get("episode");
    if out.item.kind == "song" {
        out.item.season = out.copies.first().and_then(|c| c.season);
        out.item.episode = out.copies.first().and_then(|c| c.episode);
    }
    if out.item.kind == "episode" {
        // A secondary work receives only its own numbering and the matching
        // parent provider ID. The primary copy's title/overview remain gated.
        let scope = "AND pc.id IN (SELECT value FROM json_each(?2))";
        let projection = crate::providers::library_episode_projection_sql("?1", scope, false);
        let keyed = crate::providers::library_episode_projection_sql("?1", scope, true);
        let (display, keyed): (Option<String>, Option<String>) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {projection},{keyed}")))
                .bind(id)
                .bind(serde_json::to_string(&copies).map_err(internal)?)
                .fetch_one(db)
                .await
                .map_err(internal)?;
        out.item.proj_season = None;
        out.item.proj_episode = None;
        if let Some(metadata) = &mut out.metadata {
            metadata.proj_season = None;
            metadata.proj_episode = None;
        }
        if let Some(pair) = display.and_then(|json| serde_json::from_str::<(i64, i64)>(&json).ok())
        {
            out.item.proj_season = Some(pair.0);
            out.item.proj_episode = Some(pair.1);
        }
        if let Some((provider, raw_id, season, episode)) =
            keyed.and_then(|json| serde_json::from_str::<(String, String, i64, i64)>(&json).ok())
            && let Ok(provider_id) = raw_id.parse::<i64>()
            && provider_id > 0
            && provider_id.to_string() == raw_id
        {
            let metadata = out.metadata.get_or_insert_with(ItemMetadata::default);
            metadata.tmdb_id = (provider == "tmdb").then_some(provider_id);
            metadata.tvdb_id = (provider == "tvdb").then_some(provider_id);
            metadata.proj_season = Some(season);
            metadata.proj_episode = Some(episode);
        }
    }
    out.item.episode_end = None;
    out.item.played = r.get::<Option<i64>, _>("played").unwrap_or(0) != 0;
    out.item.play_count = r.get::<Option<i64>, _>("play_count").unwrap_or(0);
    out.item.resume_position_ms = if out.item.played {
        None
    } else {
        r.get("position_ms")
    };
    out.item.resume_duration_ms = r.get("duration_ms");
    out.show_title = if let Some(parent) = &out.item.parent_id {
        sqlx::query_scalar("SELECT title FROM library_items WHERE id=?")
            .bind(parent)
            .fetch_optional(db)
            .await
            .map_err(internal)?
    } else {
        None
    };
    out.item.parent_title = out.show_title.clone();
    for relation in &mut out.related {
        if let Some(copy) = &relation.item_id {
            relation.item_id=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? AND ordinal=1").bind(copy).fetch_optional(db).await.map_err(internal)?;
            if let Some(library_item) = &relation.item_id
                && crate::library::copies(db, user, library_item)
                    .await
                    .map_err(internal)?
                    .is_empty()
            {
                relation.item_id = None;
            }
        }
    }
    Ok(out)
}
