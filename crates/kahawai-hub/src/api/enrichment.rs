//! Administrative review targets physical mediadb occurrences, never a merged
//! library item. All mutations are revision guarded and administrator-only.
use super::*;
use kahawai_mediadb as m;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in=Query)]
pub struct ReviewQuery {
    library: Option<String>,
    collection: Option<String>,
    review_only: Option<bool>,
    q: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
}
#[derive(Serialize, ToSchema)]
pub struct EnrichmentDetail {
    pub host_name: Option<String>,
    pub input: m::EnrichmentInput,
    pub candidates: Vec<m::ReviewCandidate>,
    pub metadata: m::ResolvedDescription,
    pub entries: Vec<m::MediaEntry>,
}
#[derive(Deserialize, ToSchema)]
pub struct Correction {
    revision: i64,
    action: String,
    record_id: Option<String>,
    library_item_id: Option<String>,
}
#[derive(Deserialize, ToSchema)]
pub struct CandidateSearch {
    revision: i64,
    provider: String,
    query: String,
}
#[derive(Serialize, ToSchema)]
pub struct EnrichmentProgress {
    pub providers: Vec<m::ProviderWorkStatus>,
}

pub(super) fn artwork_routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/catalogue/collection-items/{id}/artwork",
            get(enrichment_artwork),
        )
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
        ))
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/admin/v1/enrich/items/{id}/artist-artwork",
            get(enrichment_artist_artwork),
        )
        .route("/admin/v1/enrich/items", get(enrichment_items))
        .route("/admin/v1/enrich/items/{id}", get(enrichment_detail))
        .route(
            "/admin/v1/enrich/items/{id}/identities",
            get(enrichment_identities),
        )
        .route(
            "/admin/v1/enrich/items/{id}/match",
            post(enrichment_correct),
        )
        .route(
            "/admin/v1/enrich/items/{id}/candidates",
            post(enrichment_search),
        )
        .route("/admin/v1/enrich/progress", get(enrichment_progress))
}
fn error(e: anyhow::Error) -> ApiError {
    if e.is::<m::StaleEnrichment>() {
        ApiError::new(ErrorCode::Conflict, e.to_string())
    } else if e
        .downcast_ref::<sqlx::Error>()
        .is_some_and(|e| matches!(e, sqlx::Error::RowNotFound))
    {
        hidden("collection item")
    } else {
        internal(e)
    }
}
#[utoipa::path(get,path="/admin/v1/enrich/items",tag="Admin enrichment",params(ReviewQuery),security(("bearer_auth"=[])),responses((status=200,body=Vec<m::ReviewItem>)))]
pub(super) async fn enrichment_items(
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ReviewQuery>,
) -> Result<Json<Vec<m::ReviewItem>>, ApiError> {
    Ok(Json(
        s.registry
            .catalogue()
            .enrichment_items(
                q.library.as_deref(),
                q.collection.as_deref(),
                q.review_only.unwrap_or(false),
                q.q.as_deref().unwrap_or(""),
                q.offset.unwrap_or(0),
                q.limit.unwrap_or(100).clamp(1, 1000),
            )
            .await
            .map_err(error)?,
    ))
}
#[utoipa::path(get,path="/admin/v1/enrich/items/{id}",tag="Admin enrichment",params(("id"=String,Path)),security(("bearer_auth"=[])),responses((status=200,body=EnrichmentDetail)))]
pub(super) async fn enrichment_detail(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<EnrichmentDetail>, ApiError> {
    let store = s.registry.catalogue();
    let input = store.enrichment_input(&id).await.map_err(error)?;
    let host_name = s
        .registry
        .satellites_overview()
        .await
        .map_err(internal)?
        .into_iter()
        .find(|h| h.module_id == input.mediahost_id)
        .map(|h| h.name);
    Ok(Json(EnrichmentDetail {
        host_name,
        input,
        candidates: store.review_candidates(&id).await.map_err(error)?,
        metadata: store.resolve_metadata(&id).await.map_err(error)?,
        entries: store.media_entries(&id).await.map_err(error)?,
    }))
}
#[utoipa::path(get,path="/admin/v1/enrich/items/{id}/identities",tag="Admin enrichment",params(("id"=String,Path),ReviewQuery),security(("bearer_auth"=[])),responses((status=200,body=Vec<m::IdentityChoice>)))]
pub(super) async fn enrichment_identities(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<ReviewQuery>,
) -> Result<Json<Vec<m::IdentityChoice>>, ApiError> {
    Ok(Json(
        s.registry
            .catalogue()
            .matching_identities(
                &id,
                q.q.as_deref().unwrap_or(""),
                q.offset.unwrap_or(0),
                q.limit.unwrap_or(200).clamp(1, 200),
            )
            .await
            .map_err(error)?,
    ))
}
#[utoipa::path(post,path="/admin/v1/enrich/items/{id}/match",tag="Admin enrichment",params(("id"=String,Path)),request_body=Correction,security(("bearer_auth"=[])),responses((status=200,body=OkResponse),(status=409,body=ApiErrorBody)))]
pub(super) async fn enrichment_correct(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(q): ApiJson<Correction>,
) -> Result<Json<OkResponse>, ApiError> {
    if !matches!(
        q.action.as_str(),
        "pick" | "confirm" | "reject" | "clear" | "retry" | "restore" | "supplement" | "assign"
    ) {
        return Err(ApiError::new(ErrorCode::BadRequest, "unknown correction"));
    }
    if matches!(
        q.action.as_str(),
        "pick" | "confirm" | "reject" | "restore" | "supplement"
    ) && q.record_id.is_none()
    {
        return Err(ApiError::new(ErrorCode::BadRequest, "record required"));
    }
    if q.action == "assign" {
        let saved = q
            .library_item_id
            .as_deref()
            .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "library item required"))?;
        s.registry
            .catalogue()
            .assign_identity(&id, q.revision, saved)
            .await
            .map_err(error)?;
    } else if q.action == "supplement" {
        let record = q
            .record_id
            .as_deref()
            .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "record required"))?;
        s.registry
            .catalogue()
            .add_supplement(&id, q.revision, record)
            .await
            .map_err(error)?;
    } else {
        s.registry
            .catalogue()
            .correct_metadata(&id, q.revision, &q.action, q.record_id.as_deref())
            .await
            .map_err(error)?;
    }
    s.enricher.request_run(s.registry.clone());
    Ok(Json(OkResponse { ok: true }))
}
#[utoipa::path(post,path="/admin/v1/enrich/items/{id}/candidates",tag="Admin enrichment",params(("id"=String,Path)),request_body=CandidateSearch,security(("bearer_auth"=[])),responses((status=200,body=Vec<m::ReviewCandidate>),(status=409,body=ApiErrorBody)))]
pub(super) async fn enrichment_search(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(q): ApiJson<CandidateSearch>,
) -> Result<Json<Vec<m::ReviewCandidate>>, ApiError> {
    let input = s
        .registry
        .catalogue()
        .enrichment_input(&id)
        .await
        .map_err(error)?;
    if input.revision != q.revision {
        return Err(error(m::StaleEnrichment.into()));
    }
    if !matches!(
        q.provider.as_str(),
        "tmdb" | "tvdb" | "anidb" | "anilist" | "musicbrainz"
    ) {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "not an identity provider",
        ));
    }
    if q.query.trim().is_empty() {
        return Err(ApiError::new(ErrorCode::BadRequest, "enter a search title"));
    }
    s.enricher
        .catalogue_search(&s.registry, input, &q.provider, &q.query)
        .await
        .map_err(error)?;
    Ok(Json(
        s.registry
            .catalogue()
            .review_candidates(&id)
            .await
            .map_err(error)?,
    ))
}
#[utoipa::path(get,path="/admin/v1/enrich/progress",tag="Admin enrichment",security(("bearer_auth"=[])),responses((status=200,body=EnrichmentProgress)))]
pub(super) async fn enrichment_progress(
    State(s): State<AppState>,
) -> Result<Json<EnrichmentProgress>, ApiError> {
    Ok(Json(EnrichmentProgress {
        providers: s
            .registry
            .catalogue()
            .enrichment_status()
            .await
            .map_err(error)?,
    }))
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in=Query)]
pub struct PreviewQuery {
    library: Option<String>,
    record_id: Option<String>,
}
#[utoipa::path(get,path="/api/v1/catalogue/collection-items/{id}/artwork",tag="Admin enrichment",params(("id"=String,Path),PreviewQuery),security(("bearer_auth"=[])),responses((status=200,body=String,content_type="image/jpeg"),(status=404,body=ApiErrorBody)))]
pub(super) async fn enrichment_artwork(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PreviewQuery>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let store = s.registry.catalogue();
    let input = store.enrichment_input(&id).await.map_err(error)?;
    let description = if let Some(record) = q.record_id {
        let allowed = input
            .selected
            .as_ref()
            .is_some_and(|(selected, _)| selected == &record)
            || store
                .review_candidates(&id)
                .await
                .map_err(error)?
                .iter()
                .any(|c| c.id == record);
        if !allowed {
            return Err(hidden("candidate"));
        }
        store
            .provider_record(&record)
            .await
            .map_err(error)?
            .description
    } else {
        store
            .resolve_metadata(&id)
            .await
            .map_err(error)?
            .description
    };
    let poster = description
        .artwork
        .as_ref()
        .and_then(|p| p.first())
        .ok_or_else(|| hidden("artwork"))?;
    let (bytes, mime) = s
        .artwork
        .catalogue_at(&s.registry, &s.sessions, &input, poster, Some("card"))
        .await
        .map_err(error)?
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
#[utoipa::path(get,path="/admin/v1/enrich/items/{id}/artist-artwork",tag="Admin enrichment",params(("id"=String,Path),PreviewQuery),security(("bearer_auth"=[])),responses((status=200,body=String,content_type="image/jpeg"),(status=404,body=ApiErrorBody)))]
pub(super) async fn enrichment_artist_artwork(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PreviewQuery>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;
    let libraries = s
        .registry
        .catalogue()
        .copy_libraries(&id)
        .await
        .map_err(error)?;
    let library = q
        .library
        .as_ref()
        .or(libraries.first())
        .ok_or_else(|| hidden("artist artwork"))?;
    let (bytes, mime) = s
        .artwork
        .catalogue_artist_at(&s.registry, &id, library)
        .await
        .map_err(error)?
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
