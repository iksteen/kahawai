//! Download ownership is the selected rendition/version, not a logical title.
//! Search captures that choice; download revalidates it before spending quota.
//! Completed downloads are retained even if the source changed during the fetch.
use super::*;
use crate::opensubtitles::SearchQuery;

#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct SubtitleSource {
    pub media_entry_id: String,
    pub source_version: String,
}
#[derive(Deserialize, ToSchema)]
pub struct SearchRequest {
    source: SubtitleSource,
    #[serde(default)]
    languages: Vec<String>,
}
#[derive(Deserialize, ToSchema)]
pub struct DownloadRequest {
    source: SubtitleSource,
    file_id: String,
    language: Option<String>,
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/search",
            post(catalogue_subtitle_search),
        )
        .route(
            "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/download",
            post(catalogue_subtitle_download),
        )
        .route(
            "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/{track}",
            axum::routing::delete(catalogue_subtitle_delete),
        )
}
async fn resolve(
    s: &AppState,
    claims: &crate::auth::Claims,
    library: &str,
    item: &str,
    source: &SubtitleSource,
) -> Result<m::PlaybackRendition, ApiError> {
    visible(s, claims, library).await?;
    let playback = s
        .registry
        .catalogue()
        .playback_item(library, item)
        .await
        .map_err(store_error)?;
    if playback.track {
        return Err(hidden("subtitle source"));
    }
    let rendition = playback
        .renditions
        .into_iter()
        .find(|r| r.entry.id == source.media_entry_id)
        .ok_or_else(|| hidden("subtitle source"))?;
    if rendition.source_version() != source.source_version {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "media changed; reload this source before downloading subtitles",
        ));
    }
    Ok(rendition)
}

#[utoipa::path(post,path="/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/search",tag="Subtitles",security(("bearer_auth"=[])),params(("library"=String,Path),("item"=String,Path)),request_body=SearchRequest,responses((status=200,body=SubtitleSearchResponse),(status=404,body=ApiErrorBody),(status=409,body=ApiErrorBody)))]
pub(super) async fn catalogue_subtitle_search(
    State(s): State<AppState>,
    ApiPath((library, item)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(b): ApiJson<SearchRequest>,
) -> Result<Json<SubtitleSearchResponse>, ApiError> {
    let r = resolve(&s, &claims, &library, &item, &b.source).await?;
    let input = s
        .registry
        .catalogue()
        .enrichment_input(&r.entry.item_id)
        .await
        .map_err(store_error)?;
    let parent = s
        .registry
        .catalogue()
        .library_item(&library, &input.library_item_id)
        .await
        .map_err(store_error)?;
    let provider = s
        .subtitles
        .external_provider(&s.registry, &claims.sub)
        .await
        .map_err(subtitle_provider_refusal)?;
    provider.refresh_quota().await;
    let (season, episode) = m::ChildId::parse(&item)
        .ok()
        .and_then(|c| match c.position {
            m::ChildPosition::Episode {
                season: Some(s),
                episode,
            } => Some((Some(i64::from(s)), Some(i64::from(episode)))),
            _ => None,
        })
        .unwrap_or((None, None));
    let mut query = SearchQuery {
        moviehash: None,
        tmdb_id: None,
        imdb_id: None,
        title: None,
        year: None,
        season: None,
        episode: None,
        languages: b.languages,
    };
    if r.files.len() == 1
        && let Some(hash) = r.files[0].oshash.filter(|h| *h != 0)
    {
        query.moviehash = Some(hash);
        let candidates = provider
            .search(&query)
            .await
            .map_err(subtitle_provider_refusal)?;
        if !candidates.is_empty() {
            return Ok(Json(SubtitleSearchResponse {
                candidates,
                quota: provider.quota(),
            }));
        }
    }
    query.moviehash = None;
    query.season = season;
    query.episode = episode;
    query.tmdb_id = video_provider_id(&input, parent.kind, "tmdb");
    if query.tmdb_id.is_some() {
        let candidates = provider
            .search(&query)
            .await
            .map_err(subtitle_provider_refusal)?;
        if !candidates.is_empty() {
            return Ok(Json(SubtitleSearchResponse {
                candidates,
                quota: provider.quota(),
            }));
        }
    }
    query.tmdb_id = None;
    query.title = Some(parent.title);
    query.year = (parent.kind == m::LibraryItemKind::Movie)
        .then_some(parent.year)
        .flatten()
        .map(i64::from);
    let candidates = provider
        .search(&query)
        .await
        .map_err(subtitle_provider_refusal)?;
    Ok(Json(SubtitleSearchResponse {
        candidates,
        quota: provider.quota(),
    }))
}

#[utoipa::path(post,path="/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/download",tag="Subtitles",security(("bearer_auth"=[])),params(("library"=String,Path),("item"=String,Path)),request_body=DownloadRequest,responses((status=200,body=SubtitleDownloadResponse),(status=404,body=ApiErrorBody),(status=409,body=ApiErrorBody)))]
pub(super) async fn catalogue_subtitle_download(
    State(s): State<AppState>,
    ApiPath((library, item)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(b): ApiJson<DownloadRequest>,
) -> Result<Json<SubtitleDownloadResponse>, ApiError> {
    resolve(&s, &claims, &library, &item, &b.source).await?;
    let lock = s.subtitles.download_lock(
        serde_json::to_string(&(
            &b.source.media_entry_id,
            &b.source.source_version,
            &b.file_id,
        ))
        .map_err(internal)?,
    );
    let _guard = lock.lock().await;
    let r = resolve(&s, &claims, &library, &item, &b.source).await?;
    let provider = s
        .subtitles
        .external_provider(&s.registry, &claims.sub)
        .await
        .map_err(subtitle_provider_refusal)?;
    if let Some(existing) = r
        .downloaded_subtitles
        .iter()
        .find(|d| d.provider == provider.name() && d.provider_file_id == b.file_id)
    {
        return Ok(Json(SubtitleDownloadResponse {
            track_id: -existing.id,
            quota: provider.quota(),
        }));
    }
    let downloaded = provider
        .download(&b.file_id)
        .await
        .map_err(subtitle_provider_refusal)?;
    let text = kahawai_media::subtitles::decode_text(&downloaded.bytes);
    let cues = kahawai_media::subtitles::parse(&downloaded.format, &text)
        .map_err(subtitle_provider_refusal)?;
    if cues.is_empty() {
        return Err(ApiError::new(
            ErrorCode::UnsupportedTrack,
            "downloaded subtitle has no text cues",
        ));
    }
    let extracted = kahawai_media::subtitles::Extracted {
        cues,
        ass: matches!(downloaded.format.as_str(), "ass" | "ssa").then_some(text),
    };
    let id = s
        .registry
        .catalogue()
        .put_downloaded_subtitle(&m::DownloadedSubtitle {
            id: 0,
            media_entry_id: b.source.media_entry_id.clone(),
            source_version: b.source.source_version.clone(),
            provider: provider.name().into(),
            provider_file_id: b.file_id,
            format: downloaded.format,
            language: b.language,
            label: downloaded.release_name,
            created_by: claims.sub.clone(),
            payload: serde_json::to_string(&extracted).map_err(internal)?,
        })
        .await
        .map_err(internal)?;
    // Preserve paid bytes, but do not report attachment to a replacement source.
    resolve(&s, &claims, &library, &item, &b.source).await?;
    Ok(Json(SubtitleDownloadResponse {
        track_id: -id,
        quota: provider.quota(),
    }))
}

#[utoipa::path(delete,path="/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/{track}",tag="Subtitles",security(("bearer_auth"=[])),params(("library"=String,Path),("item"=String,Path),("track"=i64,Path)),request_body=SubtitleSource,responses((status=200,body=RemovedResponse),(status=404,body=ApiErrorBody),(status=409,body=ApiErrorBody)))]
pub(super) async fn catalogue_subtitle_delete(
    State(s): State<AppState>,
    ApiPath((library, item, track)): ApiPath<(String, String, i64)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(source): ApiJson<SubtitleSource>,
) -> Result<Json<RemovedResponse>, ApiError> {
    resolve(&s, &claims, &library, &item, &source).await?;
    let id = track
        .checked_neg()
        .filter(|id| *id > 0)
        .ok_or_else(|| hidden("downloaded subtitle"))?;
    let removed = s
        .registry
        .catalogue()
        .remove_downloaded_subtitle(
            id,
            &source.media_entry_id,
            &source.source_version,
            &claims.sub,
            claims.admin,
        )
        .await
        .map_err(internal)?;
    Ok(Json(RemovedResponse { removed }))
}
