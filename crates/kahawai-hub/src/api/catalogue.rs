//! HTTP boundary for mediadb. Identity, composition and queries belong to Store;
//! authentication and external library grants belong to the hub.
use super::*;
pub(super) mod feeds;
#[cfg(test)]
mod images_tests;
pub(super) mod playback;
pub(super) mod subtitles;
use crate::watch::{self, WatchState};
use kahawai_mediadb as m;
pub(super) use playback::playback_routes;

#[derive(Serialize, ToSchema)]
pub struct CatalogueLibrary {
    id: String,
    name: String,
    media_type: String,
    collection_ids: Vec<String>,
}
impl From<m::Library> for CatalogueLibrary {
    fn from(l: m::Library) -> Self {
        Self {
            id: l.id,
            name: l.name,
            media_type: l.media_type.as_str().into(),
            collection_ids: l.collection_ids,
        }
    }
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueRoot {
    id: String,
    token: String,
    path: String,
    active: bool,
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueCollection {
    id: String,
    mediahost_id: String,
    remote_id: String,
    media_type: String,
    epoch: String,
    version: u64,
    snapshot: bool,
    file_count: i64,
    roots: Vec<CatalogueRoot>,
    scanning: bool,
    connected: bool,
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueItem {
    kind: m::LibraryItemKind,
    #[serde(flatten)]
    watch: WatchState,
    artist: Option<String>,
    media_type: String,
    id: String,
    title: String,
    year: Option<i32>,
    match_confidence: Option<String>,
    representative_id: String,
    copy_ids: Vec<String>,
    /// Resolved descriptive fields and their per-field evidence IDs.
    metadata: m::ResolvedDescription,
}
impl From<m::LibraryItem> for CatalogueItem {
    fn from(i: m::LibraryItem) -> Self {
        Self {
            kind: i.kind,
            watch: WatchState::default(),
            artist: i.artist,
            media_type: i.media_type.as_str().into(),
            id: i.id,
            title: i.title,
            year: i.year,
            match_confidence: i.match_confidence,
            representative_id: i.representative_id,
            copy_ids: i.copy_ids,
            metadata: i.metadata,
        }
    }
}
#[derive(Deserialize, ToSchema)]
pub struct CreateCatalogueLibrary {
    name: String,
    media_type: String,
    #[serde(default)]
    collection_ids: Vec<String>,
}
#[derive(Deserialize, ToSchema)]
pub struct CatalogueMembership {
    collection_ids: Vec<String>,
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueItems {
    total: i64,
    items: Vec<CatalogueItem>,
    offset: u32,
    limit: u32,
}

pub(super) fn routes(state: &AppState) -> Router<AppState> {
    let admin = Router::new()
        .route("/admin/v1/catalogue/collections", get(collections))
        .route("/admin/v1/catalogue/libraries", post(create_library))
        .route(
            "/admin/v1/catalogue/libraries/{id}/refresh",
            post(refresh_library),
        )
        .route(
            "/admin/v1/catalogue/libraries/{id}",
            axum::routing::delete(delete_library),
        )
        .route(
            "/admin/v1/catalogue/libraries/{id}/collections",
            axum::routing::put(set_collections),
        )
        .route_layer(axum::middleware::from_fn(require_admin));
    let reads = Router::new()
        .merge(admin)
        .merge(subtitles::routes())
        .route(
            "/api/v1/catalogue/continue-watching",
            get(feeds::catalogue_continue),
        )
        .route("/api/v1/catalogue/up-next", get(feeds::catalogue_up_next))
        .route("/api/v1/catalogue/libraries", get(libraries))
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/watched",
            axum::routing::put(catalogue_set_watched),
        )
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/children",
            get(catalogue_children),
        )
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/seasons",
            get(catalogue_seasons),
        )
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}/seasons/{season}/items",
            get(catalogue_season_items),
        )
        .route("/api/v1/catalogue/search", get(catalogue_search))
        .route(
            "/api/v1/catalogue/libraries/{id}/items/{item_id}",
            get(item).fallback(playback::catalogue_method),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    reads.merge(
        Router::new()
            .route(
                "/api/v1/catalogue/libraries/{id}/items/{item_id}/artwork",
                get(catalogue_artwork),
            )
            .route(
                "/api/v1/catalogue/libraries/{id}/items/{item_id}/cast/{person}/artwork",
                get(catalogue_cast_artwork),
            )
            .route(
                "/api/v1/catalogue/libraries/{id}/artists/{key}/artwork",
                get(catalogue_artist_artwork),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_bearer_or_media,
            )),
    )
}

pub(super) fn store_error(e: anyhow::Error) -> ApiError {
    if e.is::<m::NotFound>() || e.downcast_ref::<sqlx::Error>().is_some_and(|e| matches!(e, sqlx::Error::RowNotFound)) {
        hidden("catalogue entry")
    } else if e.downcast_ref::<sqlx::Error>().is_some_and(|e| matches!(e, sqlx::Error::Database(d) if d.is_foreign_key_violation() || d.is_unique_violation())) {
        ApiError::new(ErrorCode::Conflict, "catalogue membership changed; reload and try again")
    } else { internal(e) }
}
pub(super) async fn visible(
    state: &AppState,
    claims: &crate::auth::Claims,
    library: &str,
) -> Result<(), ApiError> {
    if !crate::grants::can_see_library(state.registry.db(), claims, library)
        .await
        .map_err(internal)?
    {
        return Err(hidden("library"));
    }
    if !state
        .registry
        .catalogue()
        .libraries()
        .await
        .map_err(internal)?
        .iter()
        .any(|l| l.id == library)
    {
        return Err(hidden("library"));
    }
    Ok(())
}
async fn validate_members(
    state: &AppState,
    kind: m::MediaType,
    ids: &[String],
) -> Result<(), ApiError> {
    let all = state
        .registry
        .catalogue()
        .collection_summaries()
        .await
        .map_err(internal)?;
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if !seen.insert(id)
            || !all
                .iter()
                .any(|c| c.collection.id == *id && c.collection.media_type == kind)
        {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "collections must be distinct, exist and match the library media type",
            ));
        }
    }
    Ok(())
}
#[utoipa::path(get, path="/admin/v1/catalogue/collections", tag="Catalogue", security(("bearer_auth"=[])), responses((status=200,body=Vec<CatalogueCollection>)))]
pub(super) async fn collections(
    State(s): State<AppState>,
) -> Result<Json<Vec<CatalogueCollection>>, ApiError> {
    let hosts = s.registry.satellites_overview().await.map_err(internal)?;
    let rows = s
        .registry
        .catalogue()
        .collection_summaries()
        .await
        .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let c = r.collection;
                let scanning = s
                    .registry
                    .scan_state(&c.mediahost_id, &c.remote_id)
                    .is_some_and(|s| !s.complete);
                let connected = hosts
                    .iter()
                    .any(|h| h.module_id == c.mediahost_id && h.connected);
                CatalogueCollection {
                    id: c.id,
                    mediahost_id: c.mediahost_id,
                    remote_id: c.remote_id,
                    media_type: c.media_type.as_str().into(),
                    epoch: c.epoch,
                    version: c.version,
                    snapshot: r.snapshot,
                    file_count: r.file_count,
                    scanning,
                    connected,
                    roots: r
                        .roots
                        .into_iter()
                        .map(|r| CatalogueRoot {
                            id: r.id,
                            token: r.token,
                            path: r.path,
                            active: r.active,
                        })
                        .collect(),
                }
            })
            .collect(),
    ))
}
#[utoipa::path(get,path="/api/v1/catalogue/libraries",tag="Catalogue",security(("bearer_auth"=[])),responses((status=200,body=Vec<CatalogueLibrary>)))]
pub(super) async fn libraries(
    State(s): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Vec<CatalogueLibrary>>, ApiError> {
    let mut result = Vec::new();
    for l in s.registry.catalogue().libraries().await.map_err(internal)? {
        if crate::grants::can_see_library(s.registry.db(), &claims, &l.id)
            .await
            .map_err(internal)?
        {
            result.push(l.into());
        }
    }
    Ok(Json(result))
}
#[utoipa::path(post,path="/admin/v1/catalogue/libraries",tag="Catalogue",security(("bearer_auth"=[])),request_body=CreateCatalogueLibrary,responses((status=201,body=CatalogueLibrary)))]
pub(super) async fn create_library(
    State(s): State<AppState>,
    ApiJson(b): ApiJson<CreateCatalogueLibrary>,
) -> Result<(StatusCode, Json<CatalogueLibrary>), ApiError> {
    let kind = m::MediaType::parse(&b.media_type)
        .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid media type"))?;
    if b.name.trim().is_empty() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "library name is empty",
        ));
    }
    validate_members(&s, kind, &b.collection_ids).await?;
    let id = s
        .registry
        .catalogue()
        .create_library(&b.name, kind, &b.collection_ids)
        .await
        .map_err(store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(CatalogueLibrary {
            id,
            name: b.name,
            media_type: kind.as_str().into(),
            collection_ids: b.collection_ids,
        }),
    ))
}
#[utoipa::path(put,path="/admin/v1/catalogue/libraries/{id}/collections",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path)),request_body=CatalogueMembership,responses((status=204)))]
pub(super) async fn set_collections(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<CatalogueMembership>,
) -> Result<StatusCode, ApiError> {
    let l = s
        .registry
        .catalogue()
        .libraries()
        .await
        .map_err(internal)?
        .into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| hidden("library"))?;
    validate_members(&s, l.media_type, &b.collection_ids).await?;
    s.registry
        .catalogue()
        .set_library_collections(&id, &b.collection_ids)
        .await
        .map_err(store_error)?;
    retire_removed_sources(&s, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn retire_removed_sources(s: &AppState, library: &str) -> Result<(), ApiError> {
    for session in s
        .sessions
        .list()
        .into_iter()
        .filter(|session| session.catalogue.library_id == library)
    {
        if let Some(part) = session.parts.first()
            && !s
                .registry
                .catalogue()
                .library_contains_source(library, &part.module_id, &part.collection_id)
                .await
                .map_err(internal)?
        {
            s.sessions.end(&session.id).await;
        }
    }
    Ok(())
}
#[utoipa::path(delete,path="/admin/v1/catalogue/libraries/{id}",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path)),responses((status=204)))]
pub(super) async fn delete_library(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    s.registry
        .catalogue()
        .remove_library(&id)
        .await
        .map_err(internal)?;
    let mut tx = s.registry.db().begin().await.map_err(internal)?;
    sqlx::query("UPDATE users SET grants_version=grants_version+1 WHERE id IN(SELECT user_id FROM user_libraries WHERE library_id=?)").bind(&id).execute(&mut *tx).await.map_err(internal)?;
    sqlx::query("DELETE FROM user_libraries WHERE library_id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    retire_removed_sources(&s, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
/// What every library and kind of one search shares: who asks, for what text,
/// and which page.
struct Ask<'a> {
    s: &'a AppState,
    claims: &'a crate::auth::Claims,
    text: &'a str,
    offset: u32,
    limit: u32,
}
/// One page of a library's items with the viewer's watch state, for a sort
/// and limit the caller has validated.
async fn item_page(
    ask: &Ask<'_>,
    id: &str,
    sort: &str,
    artist: Option<&str>,
) -> Result<CatalogueItems, ApiError> {
    let Ask {
        s,
        claims,
        text,
        offset,
        limit,
    } = *ask;
    let (rows, total) = s
        .registry
        .catalogue()
        .browse_page(id, offset, limit, text, sort, artist)
        .await
        .map_err(store_error)?;
    let mut marks = watch::read(
        s.registry.db(),
        &claims.sub,
        &rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
    )
    .await
    .map_err(internal)?;
    let items = rows
        .into_iter()
        .map(|r| {
            let mut item = CatalogueItem::from(r);
            item.watch = marks.remove(&item.id).unwrap_or_default();
            item
        })
        .collect();
    Ok(CatalogueItems {
        total,
        items,
        offset,
        limit,
    })
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChildPage {
    offset: Option<u64>,
    limit: Option<u32>,
    /// Legacy season filter; prefer /seasons/{season}/items.
    season: Option<String>,
    /// A disc number, or "unknown".
    disc: Option<String>,
}
#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/children",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path),ChildPage),responses((status=200,body=CatalogueChildren)))]
pub(super) async fn catalogue_children(
    State(s): State<AppState>,
    ApiPath((id, parent)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<ChildPage>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CatalogueChildren>, ApiError> {
    visible(&s, &claims, &id).await?;
    let parse = |value: Option<String>, unknown: &str| -> Result<Option<Option<u32>>, ApiError> {
        value
            .map(|v| {
                if v == unknown {
                    Ok(None)
                } else {
                    v.parse()
                        .map(Some)
                        .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid child group"))
                }
            })
            .transpose()
    };
    let filter = m::ChildFilter {
        season: parse(q.season, "absolute")?,
        disc: parse(q.disc, "unknown")?,
    };
    child_page(&s, &claims, &id, &parent, q.offset, q.limit, filter).await
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SeasonItemsPage {
    offset: Option<u64>,
    limit: Option<u32>,
}
#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/seasons/{season}/items",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path),("season"=String,Path,description="Native season number, including 0 for specials, or absolute for unnumbered episodes"),SeasonItemsPage),responses((status=200,body=CatalogueChildren),(status=400,body=ApiErrorBody),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_season_items(
    State(s): State<AppState>,
    ApiPath((library, parent, season)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<SeasonItemsPage>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CatalogueChildren>, ApiError> {
    visible(&s, &claims, &library).await?;
    let season = if season == "absolute" {
        None
    } else {
        Some(
            season
                .parse()
                .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid season"))?,
        )
    };
    let page = child_page(
        &s,
        &claims,
        &library,
        &parent,
        q.offset,
        q.limit,
        m::ChildFilter {
            season: Some(season),
            disc: None,
        },
    )
    .await?;
    if !page
        .groups
        .iter()
        .any(|g| g.group.kind == "episode" && g.group.number == season)
    {
        return Err(hidden("season"));
    }
    Ok(page)
}

async fn child_page(
    s: &AppState,
    claims: &crate::auth::Claims,
    id: &str,
    parent: &str,
    offset: Option<u64>,
    limit: Option<u32>,
    filter: m::ChildFilter,
) -> Result<Json<CatalogueChildren>, ApiError> {
    let limit = limit.unwrap_or(200);
    if !(1..=200).contains(&limit)
        || (filter.season.is_some() && filter.disc.is_some())
        || filter.disc == Some(Some(0))
    {
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid child page"));
    }
    let page = s
        .registry
        .catalogue()
        .library_children(id, parent, offset.unwrap_or(0), limit, &filter)
        .await
        .map_err(store_error)?;
    let watch = watch::read(
        s.registry.db(),
        &claims.sub,
        &page
            .children
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .map_err(internal)?;
    let groups = watched_groups(s, &claims.sub, id, parent, page.groups).await?;
    Ok(Json(CatalogueChildren {
        children: page.children,
        groups,
        watch,
        total: page.total,
        offset: page.offset,
        limit: page.limit,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct CatalogueSeasons {
    seasons: Vec<CatalogueChildGroup>,
}
#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/seasons",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path)),responses((status=200,body=CatalogueSeasons),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_seasons(
    State(s): State<AppState>,
    ApiPath((library, parent)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CatalogueSeasons>, ApiError> {
    visible(&s, &claims, &library).await?;
    let groups = s
        .registry
        .catalogue()
        .library_seasons(&library, &parent)
        .await
        .map_err(store_error)?;
    Ok(Json(CatalogueSeasons {
        seasons: watched_groups(&s, &claims.sub, &library, &parent, groups).await?,
    }))
}
async fn watched_groups(
    s: &AppState,
    user: &str,
    library: &str,
    parent: &str,
    groups: Vec<m::ChildGroup>,
) -> Result<Vec<CatalogueChildGroup>, ApiError> {
    let played: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM catalogue_watch_state WHERE user_id=? AND parent_id=? AND played=1",
    )
    .bind(user)
    .bind(parent)
    .fetch_all(s.registry.db())
    .await
    .map_err(internal)?;
    let existing = s
        .registry
        .catalogue()
        .existing_children(library, parent, &played)
        .await
        .map_err(store_error)?;
    Ok(groups
        .into_iter()
        .map(|group| {
            let played = existing
                .iter()
                .filter(|id| match id.position {
                    m::ChildPosition::Episode { season, .. } => {
                        group.kind == "episode" && group.number == season
                    }
                    m::ChildPosition::Track { disc, .. }
                    | m::ChildPosition::UnnumberedTrack { disc, .. } => {
                        group.kind == "track" && group.number == disc
                    }
                })
                .count() as u64;
            CatalogueChildGroup { group, played }
        })
        .collect())
}

#[derive(Serialize, ToSchema)]
pub struct CatalogueChildGroup {
    #[serde(flatten)]
    group: m::ChildGroup,
    played: u64,
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueChildren {
    children: Vec<m::LibraryChild>,
    groups: Vec<CatalogueChildGroup>,
    watch: BTreeMap<String, WatchState>,
    total: u64,
    offset: u64,
    limit: u32,
}

#[derive(Deserialize, ToSchema)]
pub struct CatalogueWatched {
    played: bool,
    /// Explicit children, or absent to mark just the addressed item.
    items: Option<Vec<String>>,
    /// Mark the whole native season, including children beyond the loaded page.
    season: Option<String>,
}
#[utoipa::path(put,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/watched",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path)),request_body=CatalogueWatched,responses((status=200,body=UpdatedResponse),(status=400,body=ApiErrorBody),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_set_watched(
    State(s): State<AppState>,
    ApiPath((library, id)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<CatalogueWatched>,
) -> Result<Json<UpdatedResponse>, ApiError> {
    visible(&s, &claims, &library).await?;
    let parent = if id.starts_with("child1:") {
        let key = m::ChildId::parse(&id).map_err(|_| hidden("item"))?;
        if s.registry
            .catalogue()
            .existing_children(&library, &key.parent, std::slice::from_ref(&id))
            .await
            .map_err(store_error)?
            .is_empty()
        {
            return Err(hidden("item"));
        }
        key.parent
    } else {
        s.registry
            .catalogue()
            .library_item(&library, &id)
            .await
            .map_err(store_error)?;
        id.clone()
    };
    let mut ids = if let Some(season) = body.season {
        if body.items.is_some() || parent != id {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "season and explicit items cannot be combined",
            ));
        }
        let season = if season == "absolute" {
            None
        } else {
            Some(
                season
                    .parse::<u32>()
                    .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid season"))?,
            )
        };
        let filter = m::ChildFilter {
            season: Some(season),
            disc: None,
        };
        let mut ids = Vec::new();
        loop {
            let page = s
                .registry
                .catalogue()
                .library_children(&library, &parent, ids.len() as u64, 200, &filter)
                .await
                .map_err(store_error)?;
            if page.total > WATCHED_BATCH_MAX as u64 {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "at most 2000 items per mark",
                ));
            }
            let done = page.children.is_empty()
                || ids.len() as u64 + page.children.len() as u64 >= page.total;
            ids.extend(page.children.into_iter().map(|c| c.id));
            if done {
                break;
            }
        }
        ids
    } else {
        body.items.unwrap_or_else(|| vec![id.clone()])
    };
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > WATCHED_BATCH_MAX {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "mark requires 1 to 2000 items",
        ));
    }
    let children: Vec<_> = ids.iter().filter(|key| **key != id).cloned().collect();
    if parent != id && !children.is_empty() {
        return Err(hidden("item"));
    }
    let valid = s
        .registry
        .catalogue()
        .existing_children(&library, &parent, &children)
        .await
        .map_err(store_error)?;
    if valid.len() != children.len() {
        return Err(hidden("item"));
    }
    watch::mark(s.registry.db(), &claims.sub, &parent, &ids, body.played)
        .await
        .map_err(internal)?;
    Ok(Json(UpdatedResponse {
        updated: ids
            .into_iter()
            .map(|item_id| WatchUpdate {
                item_id,
                position_ms: 0,
                played: body.played,
            })
            .collect(),
    }))
}

/// Detail-only physical context. Browse remains a page of identities; opening
/// one item reads only its accessible copies. Source numbers group file parts
/// within this response and are not mediadb file IDs or playback handles.
#[derive(Serialize, ToSchema)]
pub struct CatalogueDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    child: Option<m::LibraryChild>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_title: Option<String>,
    #[serde(flatten)]
    item: CatalogueItem,
    sources: Vec<ItemSource>,
    duration_ms: Option<i64>,
    chapters: Vec<kahawai_core::media::Chapter>,
    provider: Option<String>,
    /// IDs from the representative copy's selected identity and verified links.
    tmdb_id: Option<i64>,
    tvdb_id: Option<i64>,
    copies: Vec<copies::CollectionCopy>,
    #[serde(flatten)]
    query: ItemQueryResult,
}
#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path)),responses((status=200,body=CatalogueDetail)))]
pub(super) async fn item(
    State(s): State<AppState>,
    ApiPath((id, item)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CatalogueDetail>, ApiError> {
    visible(&s, &claims, &id).await?;
    let store = s.registry.catalogue();
    let child = if item.starts_with("child1:") {
        m::ChildId::parse(&item)
            .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid child ID"))?;
        Some(store.library_child(&id, &item).await.map_err(store_error)?)
    } else {
        None
    };
    let mut row = if let Some(child) = &child {
        child.parent.clone()
    } else {
        store.library_item(&id, &item).await.map_err(store_error)?
    };
    let parent_title = child.as_ref().map(|c| c.parent.title.clone());
    if let Some(child) = &child {
        row.id = child.child.id.clone();
        row.title = child.child.title.clone();
        row.artist = child.child.artist.clone();
        row.metadata = child.child.metadata.clone();
        row.representative_id = child.child.representative_id.clone();
        row.copy_ids
            .retain(|id| child.renditions.iter().any(|e| e.item_id == *id));
    }
    let hosts = s.registry.satellites_overview().await.map_err(internal)?;
    let mut copies = Vec::new();
    let mut sources = Vec::new();
    let mut source_id = 0;
    let mut provider = None;
    let mut tmdb_id = None;
    let mut tvdb_id = None;
    let mut combined = false;
    for copy in &row.copy_ids {
        let input = store.enrichment_input(copy).await.map_err(store_error)?;
        if *copy == row.representative_id {
            tmdb_id = video_provider_id(&input, row.kind, "tmdb");
            tvdb_id = video_provider_id(&input, row.kind, "tvdb");
            provider = input
                .selected
                .as_ref()
                .map(|(_, record)| record.provider.clone());
        }
        let host = hosts.iter().find(|h| h.module_id == input.mediahost_id);
        let host_name = host.map(|h| h.name.clone());
        copies.push(copies::CollectionCopy {
            id: copy.clone(),
            title: input.title.clone(),
            year: input.year.map(i64::from),
            artist: input.artist.clone(),
            season: None,
            episode: None,
            parent_library_item_id: None,
            module_id: Some(input.mediahost_id.clone()),
            host_name: host_name.clone(),
            collection_id: Some(input.remote_id.clone()),
            paths: input
                .sources
                .iter()
                .filter(|file| {
                    child.as_ref().is_none_or(|child| {
                        child.renditions.iter().any(|entry| {
                            entry.item_id == *copy
                                && entry
                                    .data
                                    .parts
                                    .iter()
                                    .any(|part| part.file_id == file.file_id)
                        })
                    })
                })
                .map(|file| file.path.clone())
                .collect(),
            match_confidence: input
                .selected
                .as_ref()
                .map(|_| if input.manual { "manual" } else { "auto" }.into()),
            matched_title: input.selected.as_ref().map(|(_, r)| r.title.clone()),
            matched_year: input
                .selected
                .as_ref()
                .and_then(|(_, r)| r.year)
                .map(i64::from),
            assignment: copies::CopyAssignment {
                collection_item_id: copy.clone(),
                revision: input.revision,
                mode: if input.manual { "manual" } else { "auto" }.into(),
                library_item_ids: vec![input.library_item_id.clone()],
                conflict: None,
            },
        });
        let entries = if let Some(child) = &child {
            child
                .renditions
                .iter()
                .filter(|e| e.item_id == *copy)
                .cloned()
                .collect()
        } else {
            store.media_entries(copy).await.map_err(store_error)?
        };
        for entry in entries {
            // A show's episodes and an album's tracks are not alternative
            // renditions of their parent. Their copies are listed separately.
            if child.is_none() && !matches!(entry.data.kind, m::EntryKind::Movie) {
                continue;
            }
            source_id += 1;
            if source_id == 1
                && let m::EntryKind::Episode { episodes } = &entry.data.kind
            {
                combined = episodes.len() != 1
                    || episodes
                        .iter()
                        .any(|e| e.episode_end.is_some_and(|end| end != e.episode));
            }
            let parts = entry
                .data
                .parts
                .iter()
                .map(|p| p.ordinal)
                .max()
                .unwrap_or(0);
            for part in entry.data.parts {
                let file = input
                    .sources
                    .iter()
                    .find(|f| f.file_id == part.file_id)
                    .ok_or_else(|| {
                        ApiError::new(ErrorCode::Conflict, "source changed; reload the item")
                    })?;
                sources.push(ItemSource {
                    media_entry_id: Some(entry.id.clone()),
                    collection_item_id: copy.clone(),
                    module_id: input.mediahost_id.clone(),
                    host_name: host_name.clone(),
                    collection_id: input.remote_id.clone(),
                    path_rel: file.path.clone(),
                    size: file.size.unwrap_or(0),
                    available: host.is_some_and(|h| h.connected),
                    revision: i64::from(kahawai_core::names::release_revision(&file.path)),
                    source_id,
                    part: i64::from(part.ordinal),
                    parts: i64::from(parts),
                    streams: Some(file.media.clone().map(Into::into)),
                    catalog_info: file.media.clone(),
                });
            }
        }
    }
    // Describe the same first rendition the UI displays. Never fold across
    // alternative copies, or past a missing physical part.
    let first: Vec<_> = sources
        .iter()
        .take_while(|source| source.source_id == 1)
        .collect();
    let complete = !combined
        && first
            .first()
            .is_some_and(|source| source.parts as usize == first.len())
        && first
            .iter()
            .enumerate()
            .all(|(at, source)| source.part == at as i64 + 1);
    let duration_ms = complete
        .then(|| {
            first.iter().try_fold(0u64, |total, source| {
                total.checked_add(source.catalog_info.as_ref()?.duration_ms?)
            })
        })
        .flatten()
        .and_then(|ms| i64::try_from(ms).ok());
    let chapters = if complete {
        group_chapters(first.iter().map(|source| source.catalog_info.clone()))
    } else {
        vec![]
    };
    let mut item = CatalogueItem::from(row);
    item.watch = watch::read(s.registry.db(), &claims.sub, std::slice::from_ref(&item.id))
        .await
        .map_err(internal)?
        .remove(&item.id)
        .unwrap_or_default();
    Ok(Json(CatalogueDetail {
        query: ItemQueryResult {
            subtitle_source: None,
            negotiated: None,
            unavailable: None,
            segments: vec![],
        },
        child: child.map(|c| c.child),
        parent_title,
        duration_ms,
        chapters,
        provider,
        tmdb_id,
        tvdb_id,
        item,
        sources,
        copies,
    }))
}

/// Movie and show IDs use separate namespaces, even within an anime library.
/// Never expose candidates or borrow an ID from a different representative copy.
fn video_provider_id(
    input: &m::EnrichmentInput,
    kind: m::LibraryItemKind,
    provider: &str,
) -> Option<i64> {
    let namespace = match kind {
        m::LibraryItemKind::Movie => "movie",
        m::LibraryItemKind::Series => "show",
        m::LibraryItemKind::Album => return None,
    };
    input
        .selected
        .iter()
        .map(|(_, r)| (&r.provider, &r.namespace, &r.external_id))
        .chain(
            input
                .links
                .iter()
                .map(|r| (&r.provider, &r.namespace, &r.external_id)),
        )
        .filter(|(p, ns, _)| p.as_str() == provider && ns.as_str() == namespace)
        .find_map(|(_, _, id)| {
            id.parse::<i64>()
                .ok()
                .filter(|id| (1..=9_007_199_254_740_991).contains(id))
        })
}

#[utoipa::path(
    operation_id = "admin_set_user_libraries",
    put, path = "/admin/v1/users/{id}/libraries", tag = "Admin users",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SetAccess,
    responses(
        (status = 200, body = UserAccessResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody),
        (status = 409, description = "Somebody else changed these grants since they were read: `stale_write`", body = ApiErrorBody)
    )
)]
pub(super) async fn set_user_libraries(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<SetAccess>,
) -> Result<Json<UserAccessResponse>, ApiError> {
    match crate::grants::set_catalogue_access(
        s.registry.db(),
        s.registry.catalogue(),
        &id,
        b.grants_version,
        b.all_libraries,
        &b.libraries,
    )
    .await
    .map_err(internal)?
    {
        crate::grants::SetAccess::Applied {
            grants_version,
            libraries,
        } => Ok(Json(UserAccessResponse {
            id,
            all_libraries: b.all_libraries,
            libraries,
            grants_version,
        })),
        crate::grants::SetAccess::Stale => Err(ApiError::new(
            ErrorCode::StaleWrite,
            "library grants changed; reload and try again",
        )),
        crate::grants::SetAccess::NoSuchUser => Err(hidden("user")),
    }
}

#[derive(Serialize, ToSchema)]
pub struct CatalogueArtists {
    artists: Vec<ArtistSummary>,
    total: i64,
    offset: u32,
    limit: u32,
}
async fn artist_page(
    ask: &Ask<'_>,
    id: &str,
    descending: bool,
) -> Result<CatalogueArtists, ApiError> {
    let Ask {
        s,
        text,
        offset,
        limit,
        ..
    } = *ask;
    let (rows, total) = s
        .registry
        .catalogue()
        .browse_artists(id, offset, limit, text, descending)
        .await
        .map_err(store_error)?;
    Ok(CatalogueArtists {
        artists: rows
            .into_iter()
            .map(|(name, album_count)| ArtistSummary {
                key: name.clone(),
                name,
                album_count,
            })
            .collect(),
        total,
        offset,
        limit,
    })
}

#[derive(Serialize, ToSchema)]
pub struct CatalogueTracks {
    tracks: Vec<m::TrackHit>,
    total: u64,
    offset: u64,
    limit: u32,
}
async fn track_page(ask: &Ask<'_>, id: &str) -> Result<CatalogueTracks, ApiError> {
    let Ask {
        s,
        text,
        offset,
        limit,
        ..
    } = *ask;
    let offset = u64::from(offset);
    let hits = s
        .registry
        .catalogue()
        .search_tracks(id, text, offset, limit)
        .await
        .map_err(store_error)?;
    Ok(CatalogueTracks {
        tracks: hits.tracks,
        total: hits.total,
        offset,
        limit,
    })
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Matched case-insensitively against titles (items, songs) and names
    /// (album artists). Empty matches everything, which makes this a browse.
    q: Option<String>,
    /// Comma-separated library IDs, answered in that order. Every library the
    /// viewer may see, in the libraries endpoint's order, when absent.
    libraries: Option<String>,
    /// Comma-separated: `items`, `artists`, `tracks`. Every kind each library
    /// holds when absent; `artists` and `tracks` exist in music libraries only.
    kinds: Option<String>,
    /// With exactly one kind: items `title` (default), `-title`, `year`,
    /// `-year`, `added`, `-added`; artists `name` (default), `-name`; tracks
    /// `title`.
    sort: Option<String>,
    /// With `kinds=items` only: an album artist's key, for that artist's albums.
    artist: Option<String>,
    offset: Option<u32>,
    /// Hits per library and kind, 1 to 200; 20 when absent.
    limit: Option<u32>,
}
#[derive(Serialize, ToSchema)]
pub struct SearchLibrary {
    id: String,
    name: String,
    media_type: String,
}
/// One library's answer. Each kind is exactly the page its own listing would
/// be, total included; absent when not asked for, or not held by the library.
#[derive(Serialize, ToSchema)]
pub struct LibraryHits {
    library: SearchLibrary,
    items: Option<CatalogueItems>,
    /// Album artists; music libraries only.
    artists: Option<CatalogueArtists>,
    /// Songs, one hit per album position, keyed by the album's child id;
    /// music libraries only.
    tracks: Option<CatalogueTracks>,
}
#[derive(Serialize, ToSchema)]
pub struct CatalogueSearch {
    libraries: Vec<LibraryHits>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Items,
    Artists,
    Tracks,
}
async fn maybe<T>(
    page: Option<impl std::future::Future<Output = Result<T, ApiError>>>,
) -> Result<Option<T>, ApiError> {
    match page {
        Some(page) => page.await.map(Some),
        None => Ok(None),
    }
}
/// Every catalogue search and filtered listing: the home screen's search
/// across libraries, and a library's (or an artist's) items, album artists
/// and songs, paged.
///
/// One request for any number of libraries and kinds, multiplexed here
/// rather than by the client: on a slow link every request costs a round
/// trip of its own. Each library-and-kind is the same bounded query its
/// listing runs, and they are asked concurrently, so this costs the slowest
/// of them rather than their sum — on the same reader pool a client's
/// concurrent requests would share. Not one grouped query across libraries:
/// that was measured and withdrawn (`docs/web-next-plan.md`, B3).
///
/// All or nothing: a library that cannot be answered fails the request,
/// since a partial answer would read as fewer matches.
#[utoipa::path(get,path="/api/v1/catalogue/search",tag="Catalogue",security(("bearer_auth"=[])),params(SearchQuery),responses((status=200,body=CatalogueSearch),(status=400,body=ApiErrorBody),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_search(
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<SearchQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CatalogueSearch>, ApiError> {
    let bad = |message: &'static str| ApiError::new(ErrorCode::BadRequest, message);
    let limit = q.limit.unwrap_or(20);
    if !(1..=200).contains(&limit) {
        return Err(bad("invalid search limit"));
    }
    let offset = q.offset.unwrap_or(0);
    let kinds: Vec<SearchKind> = match q.kinds.as_deref() {
        None => vec![SearchKind::Items, SearchKind::Artists, SearchKind::Tracks],
        Some(list) => {
            let mut kinds = Vec::new();
            for kind in list.split(',') {
                let kind = match kind {
                    "items" => SearchKind::Items,
                    "artists" => SearchKind::Artists,
                    "tracks" => SearchKind::Tracks,
                    _ => return Err(bad("invalid search kind")),
                };
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            kinds
        }
    };
    let single = (q.kinds.is_some() && kinds.len() == 1).then(|| kinds[0]);
    let (item_sort, artist_desc) = match (q.sort.as_deref(), single) {
        (None, _) => ("title", false),
        (Some(sort), Some(SearchKind::Items))
            if ["title", "-title", "year", "-year", "-added", "added"].contains(&sort) =>
        {
            (sort, false)
        }
        (Some(sort), Some(SearchKind::Artists)) if sort == "name" || sort == "-name" => {
            ("title", sort == "-name")
        }
        (Some("title"), Some(SearchKind::Tracks)) => ("title", false),
        _ => return Err(bad("invalid search sort")),
    };
    if q.artist.is_some() && single != Some(SearchKind::Items) {
        return Err(bad("artist needs kinds=items"));
    }

    let all = s.registry.catalogue().libraries().await.map_err(internal)?;
    let mut libraries = Vec::new();
    match q.libraries.as_deref() {
        None => {
            for library in all {
                if crate::grants::can_see_library(s.registry.db(), &claims, &library.id)
                    .await
                    .map_err(internal)?
                {
                    libraries.push(library);
                }
            }
        }
        Some(list) => {
            for id in list.split(',') {
                if libraries.iter().any(|l: &m::Library| l.id == id) {
                    continue;
                }
                let library = all
                    .iter()
                    .find(|l| l.id == id)
                    .ok_or_else(|| hidden("library"))?;
                if !crate::grants::can_see_library(s.registry.db(), &claims, id)
                    .await
                    .map_err(internal)?
                {
                    return Err(hidden("library"));
                }
                libraries.push(library.clone());
            }
        }
    }

    let ask = Ask {
        s: &s,
        claims: &claims,
        text: q.q.as_deref().unwrap_or(""),
        offset,
        limit,
    };
    let (ask, artist) = (&ask, q.artist.as_deref());
    let answers = futures_util::future::try_join_all(libraries.into_iter().map(|library| {
        let music = library.media_type == m::MediaType::Music;
        let wants_items = kinds.contains(&SearchKind::Items);
        let wants_artists = music && kinds.contains(&SearchKind::Artists);
        let wants_tracks = music && kinds.contains(&SearchKind::Tracks);
        async move {
            let id = library.id.as_str();
            let (items, artists, tracks) = tokio::try_join!(
                maybe(wants_items.then(|| item_page(ask, id, item_sort, artist))),
                maybe(wants_artists.then(|| artist_page(ask, id, artist_desc))),
                maybe(wants_tracks.then(|| track_page(ask, id))),
            )?;
            Ok::<_, ApiError>(LibraryHits {
                library: SearchLibrary {
                    id: library.id.clone(),
                    name: library.name.clone(),
                    media_type: library.media_type.as_str().into(),
                },
                items,
                artists,
                tracks,
            })
        }
    }))
    .await?;
    Ok(Json(CatalogueSearch { libraries: answers }))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkKind {
    #[default]
    Poster,
    Logo,
    Banner,
    Backdrop,
    Still,
}

#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/artwork",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path),("size"=Option<String>,Query),("kind"=Option<ArtworkKind>,Query,description="Defaults to poster. Logo, banner, backdrop and episode still reads use cached images only."),("season"=Option<String>,Query,description="A native season number, or absolute; serves cached season artwork with a show-poster fallback")),responses((status=200,body=String,content_type="image/*"),(status=400,body=ApiErrorBody),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_artwork(
    State(s): State<AppState>,
    ApiPath((id, item)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<std::collections::HashMap<String, String>>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    visible(&s, &claims, &id).await?;
    let store = s.registry.catalogue();
    let kind: ArtworkKind = q
        .get("kind")
        .map(|value| serde_json::from_value(serde_json::Value::String(value.clone())))
        .transpose()
        .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid artwork kind"))?
        .unwrap_or_default();
    if kind == ArtworkKind::Still && !item.starts_with("child1:") {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "still artwork requires an episode",
        ));
    }
    if let Some(selector) = q.get("season") {
        if kind != ArtworkKind::Poster {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "season artwork requires poster kind",
            ));
        }
        if item.starts_with("child1:") {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "season artwork requires a parent item",
            ));
        }
        let season = if selector == "absolute" {
            None
        } else {
            Some(
                selector
                    .parse::<u32>()
                    .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid season"))?,
            )
        };
        let cover = store
            .library_season_artwork(&id, &item, season)
            .await
            .map_err(store_error)?;
        let parent = store.library_item(&id, &item).await.map_err(store_error)?;
        let mut candidates = Vec::new();
        if let Some(cover) = cover {
            candidates.push((cover.representative_id, cover.metadata));
        }
        candidates.push((parent.representative_id, parent.metadata));
        for (copy, metadata) in candidates {
            let input = store.enrichment_input(&copy).await.map_err(store_error)?;
            for poster in metadata.description.artwork.unwrap_or_default() {
                if let Some((bytes, mime)) = s
                    .artwork
                    .catalogue_cached_at(&input, &poster, q.get("size").map(String::as_str))
                    .await
                    .map_err(store_error)?
                {
                    return Ok((
                        [
                            (axum::http::header::CONTENT_TYPE, mime),
                            (axum::http::header::CACHE_CONTROL, "private, no-cache"),
                        ],
                        bytes,
                    )
                        .into_response());
                }
            }
        }
        return Err(hidden("artwork"));
    }
    let row = if item.starts_with("child1:") {
        m::ChildId::parse(&item)
            .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid child ID"))?;
        let child = store.library_child(&id, &item).await.map_err(store_error)?;
        if kind == ArtworkKind::Still
            && !matches!(child.child.position, m::ChildPosition::Episode { .. })
        {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "still artwork requires an episode",
            ));
        }
        let mut row = child.parent;
        if matches!(kind, ArtworkKind::Poster | ArtworkKind::Still) {
            row.representative_id = child.child.representative_id;
            row.metadata = child.child.metadata;
        }
        if !matches!(child.child.position, m::ChildPosition::Episode { .. })
            && row.metadata.description.artwork.is_none()
        {
            row.metadata.description.artwork = store
                .resolve_metadata(&row.representative_id)
                .await
                .map_err(store_error)?
                .description
                .artwork;
        }
        row
    } else {
        store.library_item(&id, &item).await.map_err(store_error)?
    };
    let input = s
        .registry
        .catalogue()
        .enrichment_input(&row.representative_id)
        .await
        .map_err(store_error)?;
    let description = &row.metadata.description;
    let poster = match kind {
        ArtworkKind::Poster | ArtworkKind::Still => {
            description.artwork.as_ref().and_then(|p| p.first())
        }
        ArtworkKind::Logo => description.logo.as_ref(),
        ArtworkKind::Banner => description.banner.as_ref(),
        ArtworkKind::Backdrop => description.backdrop.as_ref(),
    }
    .ok_or_else(|| hidden("artwork"))?;
    if kind != ArtworkKind::Poster {
        let (bytes, mime) = s
            .artwork
            .catalogue_cached_at(&input, poster, q.get("size").map(String::as_str))
            .await
            .map_err(store_error)?
            .ok_or_else(|| hidden("artwork"))?;
        return Ok((
            [
                (axum::http::header::CONTENT_TYPE, mime),
                (axum::http::header::CACHE_CONTROL, "private, no-cache"),
            ],
            bytes,
        )
            .into_response());
    }
    let (bytes, mime) = s
        .artwork
        .catalogue_at(
            &s.registry,
            &s.sessions.bytes,
            &input,
            poster,
            q.get("size").map(String::as_str),
        )
        .await
        .map_err(store_error)?
        .ok_or_else(|| hidden("artwork"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "private, no-cache"),
        ],
        bytes,
    )
        .into_response())
}

/// A headshot is selected only from this accessible title's resolved cast.
/// The caller cannot name an arbitrary image URL or browse a global people list.
#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/items/{item_id}/cast/{person}/artwork",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path),("person"=String,Path,description="Provider-qualified cast ID from metadata, for example tmdb:123"),("size"=Option<String>,Query)),responses((status=200,body=String,content_type="image/*"),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_cast_artwork(
    State(s): State<AppState>,
    ApiPath((id, item, person)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<std::collections::HashMap<String, String>>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<axum::response::Response, ApiError> {
    visible(&s, &claims, &id).await?;
    let store = s.registry.catalogue();
    let row = if item.starts_with("child1:") {
        m::ChildId::parse(&item)
            .map_err(|_| ApiError::new(ErrorCode::BadRequest, "invalid child ID"))?;
        store
            .library_child(&id, &item)
            .await
            .map_err(store_error)?
            .parent
    } else {
        store.library_item(&id, &item).await.map_err(store_error)?
    };
    let image = row
        .metadata
        .description
        .cast
        .as_ref()
        .into_iter()
        .flatten()
        .find(|credit| credit.id.as_deref() == Some(person.as_str()))
        .and_then(|credit| credit.image.as_deref())
        .ok_or_else(|| hidden("cast artwork"))?;
    let (bytes, mime) = s
        .artwork
        .get_cached_remote_at(image, q.get("size").map(String::as_str))
        .await
        .map_err(store_error)?
        .ok_or_else(|| hidden("cast artwork"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "private, no-cache"),
        ],
        bytes,
    )
        .into_response())
}

#[utoipa::path(get,path="/api/v1/catalogue/libraries/{id}/artists/{key}/artwork",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("key"=String,Path)),responses((status=200,body=String,content_type="image/jpeg"),(status=404,body=ApiErrorBody)))]
pub(super) async fn catalogue_artist_artwork(
    State(s): State<AppState>,
    ApiPath((id, key)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    visible(&s, &claims, &id).await?;
    let (rows, _) = s
        .registry
        .catalogue()
        .browse_page(&id, 0, 1, "", "title", Some(&key))
        .await
        .map_err(store_error)?;
    let row = rows.first().ok_or_else(|| hidden("artist"))?;
    let (bytes, mime) = s
        .artwork
        .catalogue_artist_at(&s.registry, &row.representative_id, &id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| hidden("artist artwork"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, "private, no-cache"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Serialize, ToSchema)]
pub struct CatalogueRefreshResponse {
    asked: usize,
    offline: usize,
    unsupported: usize,
}

/// Rescan committed membership. Disconnected collections are reported, not queued.
#[utoipa::path(post,path="/admin/v1/catalogue/libraries/{id}/refresh",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),RefreshQuery),responses((status=200,body=CatalogueRefreshResponse),(status=404,body=ApiErrorBody)))]
pub(super) async fn refresh_library(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RefreshQuery>,
) -> Result<Json<CatalogueRefreshResponse>, ApiError> {
    let library = s
        .registry
        .catalogue()
        .libraries()
        .await
        .map_err(internal)?
        .into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| hidden("library"))?;
    let mut asked = 0;
    let mut offline = 0;
    let mut unsupported = 0;
    for collection in s
        .registry
        .catalogue()
        .collection_summaries()
        .await
        .map_err(internal)?
    {
        let c = collection.collection;
        if library.collection_ids.contains(&c.id) {
            match s
                .registry
                .rescan_collection(&c.mediahost_id, &c.remote_id, q.deep.unwrap_or(false))
                .await
            {
                crate::registry::RescanResult::Requested => asked += 1,
                crate::registry::RescanResult::Offline => offline += 1,
                crate::registry::RescanResult::Unsupported => unsupported += 1,
            }
        }
    }
    Ok(Json(CatalogueRefreshResponse {
        asked,
        offline,
        unsupported,
    }))
}
