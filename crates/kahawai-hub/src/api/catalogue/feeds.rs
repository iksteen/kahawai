//! Home rows are history-driven queries, with no copied catalogue or sync job.
//! Each identity appears once, through a currently permitted library with physical
//! sources. Continue and Up next use the same meaningful-progress predicate.
//! Up next follows the most recently finished native position (highest position
//! breaks batch-mark ties), skipping earlier gaps and already-finished positions.
//! A series stays current for 30 days after watching, or when its next episode's
//! oldest surviving physical rendition arrived within 30 days. Derived child IDs
//! have no creation timestamp; physical entry ULIDs supply this arrival date.
use super::*;
use std::collections::{BTreeSet, HashMap};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeedPage {
    library: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
}
#[derive(Serialize, ToSchema)]
pub struct FeedItem {
    library_id: String,
    #[serde(flatten)]
    item: CatalogueItem,
    child: Option<m::LibraryChild>,
    parent_title: Option<String>,
}
impl FeedItem {
    fn new(library: &str, parent: m::LibraryItem, child: Option<m::LibraryChild>) -> Self {
        let parent_title = child.as_ref().map(|_| parent.title.clone());
        let mut item = CatalogueItem::from(parent);
        if let Some(c) = &child {
            item.id = c.id.clone();
            item.title = c.title.clone();
            item.artist = c.artist.clone();
            item.representative_id = c.representative_id.clone();
            item.metadata = c.metadata.clone();
        }
        Self {
            library_id: library.into(),
            item,
            child,
            parent_title,
        }
    }
}
#[derive(Serialize, ToSchema)]
pub struct FeedItems {
    items: Vec<FeedItem>,
    total: usize,
    offset: u32,
    limit: u32,
}
struct History {
    id: String,
    parent: String,
    played: bool,
    meaningful: bool,
    updated: i64,
}
fn position(id: &str) -> Option<(Option<u32>, u32)> {
    match m::ChildId::parse(id).ok()?.position {
        m::ChildPosition::Episode { season, episode } => Some((season, episode)),
        _ => None,
    }
}
fn absent(error: &anyhow::Error) -> bool {
    error.is::<m::NotFound>()
        || matches!(
            error.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::RowNotFound)
        )
}
async fn resolve(
    s: &AppState,
    libraries: &[String],
    row: &History,
) -> Result<Option<FeedItem>, ApiError> {
    for library in libraries {
        if row.id.starts_with("child1:") {
            if position(&row.id).is_none() {
                return Ok(None);
            }
            match s.registry.catalogue().library_child(library, &row.id).await {
                Ok(c) => return Ok(Some(FeedItem::new(library, c.parent, Some(c.child)))),
                Err(e) if absent(&e) => {}
                Err(e) => return Err(internal(e)),
            }
        } else {
            match s.registry.catalogue().library_item(library, &row.id).await {
                Ok(i) if i.kind == m::LibraryItemKind::Movie => {
                    return Ok(Some(FeedItem::new(library, i, None)));
                }
                Ok(_) => return Ok(None),
                Err(e) if absent(&e) => {}
                Err(e) => return Err(internal(e)),
            }
        }
    }
    Ok(None)
}
async fn feed(
    s: AppState,
    claims: crate::auth::Claims,
    q: FeedPage,
    next: bool,
) -> Result<Json<FeedItems>, ApiError> {
    let limit = q.limit.unwrap_or(12);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "limit must be between 1 and 200",
        ));
    }
    let offset = q.offset.unwrap_or(0);
    if let Some(library) = &q.library {
        visible(&s, &claims, library).await?;
    }
    let mut libraries = Vec::new();
    for l in s.registry.catalogue().libraries().await.map_err(internal)? {
        if q.library.as_ref().is_none_or(|id| *id == l.id)
            && crate::grants::can_see_library(s.registry.db(), &claims, &l.id)
                .await
                .map_err(internal)?
        {
            libraries.push(l.id);
        }
    }
    libraries.sort();
    let meaningful = meaningful_unfinished("w");
    let predicate = if next {
        format!("played=1 OR ({meaningful})")
    } else {
        meaningful.clone()
    };
    let sql = format!(
        "SELECT w.*,({meaningful}) AS meaningful FROM catalogue_watch_state w WHERE user_id=? AND ({predicate}) ORDER BY updated_at DESC,item_id DESC"
    );
    let history: Vec<_> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&claims.sub)
        .fetch_all(s.registry.db())
        .await
        .map_err(internal)?
        .into_iter()
        .map(|r| {
            let played = r.get("played");

            History {
                id: r.get("item_id"),
                parent: r.get("parent_id"),
                played,
                meaningful: r.get("meaningful"),
                updated: r.get("updated_at"),
            }
        })
        .collect();
    let mut continuing = BTreeSet::new();
    let mut rows = Vec::new();
    for h in history.iter().filter(|h| h.meaningful) {
        if let Some(item) = resolve(&s, &libraries, h).await? {
            continuing.insert(h.parent.clone());
            if !next {
                rows.push((h.updated, item));
            }
        }
    }
    if next {
        let mut shows: HashMap<&str, Vec<&History>> = HashMap::new();
        for h in history
            .iter()
            .filter(|h| h.played && position(&h.id).is_some())
        {
            shows.entry(&h.parent).or_default().push(h);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let since = now.saturating_sub(UP_NEXT_WINDOW_SECS);
        for (parent, history) in shows {
            if continuing.contains(parent) {
                continue;
            }
            let last = history
                .iter()
                .max_by_key(|h| (h.updated, position(&h.id)))
                .expect("nonempty history");
            let finished = history.iter().filter_map(|h| position(&h.id)).collect();
            let mut candidate: Option<(String, m::LibraryChildDetail)> = None;
            let mut oldest = None;
            for library in &libraries {
                match s
                    .registry
                    .catalogue()
                    .next_episode(
                        library,
                        parent,
                        position(&last.id).expect("episode"),
                        &finished,
                    )
                    .await
                {
                    Ok(Some(c)) => {
                        let pos = position(&c.child.id);
                        let current = candidate.as_ref().and_then(|(_, c)| position(&c.child.id));
                        let arrived = c
                            .renditions
                            .iter()
                            .filter_map(|r| ulid::Ulid::from_string(&r.id).ok())
                            .map(|id| id.timestamp_ms() / 1000)
                            .min();
                        if candidate.is_none() || pos < current {
                            oldest = arrived;
                            candidate = Some((library.clone(), c));
                        } else if pos == current {
                            oldest = oldest.into_iter().chain(arrived).min();
                        }
                    }
                    Ok(None) => {}
                    Err(e) if absent(&e) => {}
                    Err(e) => return Err(internal(e)),
                }
            }
            if let Some((library, c)) = candidate
                && (last.updated >= since as i64 || oldest.is_some_and(|t| t >= since))
            {
                rows.push((
                    last.updated,
                    FeedItem::new(&library, c.parent, Some(c.child)),
                ));
            }
        }
    }
    rows.sort_by(|(at, a), (bt, b)| bt.cmp(at).then_with(|| b.item.id.cmp(&a.item.id)));
    let total = rows.len();
    let mut items: Vec<FeedItem> = rows
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|(_, item)| item)
        .collect();
    let mut states = watch::read(
        s.registry.db(),
        &claims.sub,
        &items.iter().map(|i| i.item.id.clone()).collect::<Vec<_>>(),
    )
    .await
    .map_err(internal)?;
    for i in &mut items {
        i.item.watch = states.remove(&i.item.id).unwrap_or_default();
    }
    Ok(Json(FeedItems {
        items,
        total,
        offset,
        limit,
    }))
}
#[utoipa::path(get,path="/api/v1/catalogue/continue-watching",tag="Catalogue",security(("bearer_auth"=[])),params(FeedPage),responses((status=200,body=FeedItems)))]
pub(in crate::api) async fn catalogue_continue(
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<FeedPage>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<FeedItems>, ApiError> {
    feed(s, claims, q, false).await
}
#[utoipa::path(get,path="/api/v1/catalogue/up-next",tag="Catalogue",security(("bearer_auth"=[])),params(FeedPage),responses((status=200,body=FeedItems)))]
pub(in crate::api) async fn catalogue_up_next(
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<FeedPage>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<FeedItems>, ApiError> {
    feed(s, claims, q, true).await
}

/// The one authority for the partition between Continue Watching and Up Next.
///
/// `duration_ms` can be absent on imported or legacy state. In that case the
/// absolute minute remains the honest criterion we can evaluate.
fn meaningful_unfinished(alias: &str) -> String {
    format!(
        "{alias}.position_ms >= MAX({CONTINUE_MIN_POSITION_MS}, \
         COALESCE({alias}.duration_ms, 0) / {CONTINUE_MIN_RUNTIME_FRACTION}) \
         AND {alias}.played = 0"
    )
}
