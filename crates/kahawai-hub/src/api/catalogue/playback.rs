use super::*;

pub(crate) fn playback_routes(state: &AppState) -> Router<AppState> {
    let controls = Router::new()
        .route(
            "/api/v1/playback/sessions/{id}",
            axum::routing::delete(end_session),
        )
        .route(
            "/api/v1/playback/sessions/{id}/progress",
            post(post_progress),
        )
        .route("/api/v1/playback/sessions/{id}/seek", post(seek_session))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session_owner,
        ))
        .route("/api/v1/playback/sessions", post(start))
        .route(
            "/api/v1/catalogue/libraries/{library}/items/{item}/next",
            get(catalogue_next),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    controls.merge(
        Router::new()
            .route("/api/v1/playback/sessions/{id}/stream", get(stream_session))
            .route(
                "/api/v1/playback/sessions/{id}/subtitles/{file}",
                get(session_subtitle),
            )
            .route("/api/v1/playback/sessions/{id}/fonts", get(session_fonts))
            .route(
                "/api/v1/playback/sessions/{id}/fonts/{n}",
                get(session_font),
            )
            .route("/api/v1/playback/sessions/{id}/{file}", get(session_file))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_session_owner,
            ))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_bearer_or_media,
            )),
    )
}
async fn start(
    state: State<AppState>,
    claims: axum::Extension<crate::auth::Claims>,
    body: ApiJson<StartSessionRequest>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiError> {
    if body.0.library_id.is_none() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "library_id is required",
        ));
    }
    if body.0.source_id.is_some()
        && body.0.media_entry_id.is_none()
        && body.0.resume_source_fingerprint.is_none()
    {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "source selection requires media_entry_id",
        ));
    }
    start_session(state, claims, body).await
}

/// GET serves catalogue facts; QUERY negotiates with a client profile.
/// Check the method before extracting a body so every other method gets 405.
pub(super) async fn catalogue_method(
    State(s): State<AppState>,
    path: ApiPath<(String, String)>,
    claims: axum::Extension<crate::auth::Claims>,
    request: Request,
) -> Result<Response, ApiError> {
    if request.method().as_str() != "QUERY" {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [
                ("allow", "GET, HEAD, QUERY"),
                ("accept-query", "application/json"),
            ],
            Json(ApiErrorBody::new(
                ErrorCode::MethodNotAllowed,
                "use GET or QUERY on an item",
            )),
        )
            .into_response());
    }
    let body =
        <ApiJson<ItemQuery> as axum::extract::FromRequest<AppState>>::from_request(request, &s)
            .await?;
    Ok(catalogue_playback(State(s), path, claims, body)
        .await?
        .into_response())
}

#[utoipa::path(post,path="/api/v1/catalogue/libraries/{id}/items/{item_id}",tag="Catalogue",security(("bearer_auth"=[])),params(("id"=String,Path),("item_id"=String,Path)),request_body=ItemQuery,responses((status=200,body=CatalogueDetail),(status=404,body=ApiErrorBody)))]
pub(crate) async fn catalogue_playback(
    State(s): State<AppState>,
    ApiPath((library, id)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(q): ApiJson<ItemQuery>,
) -> Result<Json<CatalogueDetail>, ApiError> {
    let Json(mut detail) = item(
        State(s.clone()),
        ApiPath((library.clone(), id.clone())),
        axum::Extension(claims.clone()),
    )
    .await?;
    let input = crate::sessions::CataloguePlayback {
        subtitle_cache: s.subtitles.cache_dir().into(),
        library_id: library.clone(),
        item: s
            .registry
            .catalogue()
            .playback_item(&library, &id)
            .await
            .map_err(store_error)?,
        media_entry_id: if let Some(entry) = q.media_entry_id {
            Some(entry)
        } else {
            q.source_id
                .map(|id| {
                    detail
                        .sources
                        .iter()
                        .find(|source| source.source_id == id)
                        .and_then(|s| s.media_entry_id.clone())
                        .ok_or_else(|| hidden("source"))
                })
                .transpose()?
        },
    };
    if input
        .media_entry_id
        .as_ref()
        .is_some_and(|id| !input.item.renditions.iter().any(|r| r.entry.id == *id))
    {
        return Err(hidden("source"));
    }
    if input.item.renditions.is_empty() {
        detail.query.unavailable = Some(ApiErrorBody::new(
            ErrorCode::Unplayable,
            "this item has no media of its own",
        ));
        return Ok(Json(detail));
    }
    // Subtitle search is useful while a host is offline too. The eventual
    // negotiated source replaces this fallback when playback is available.
    detail.query.subtitle_source = input
        .item
        .renditions
        .iter()
        .find(|r| {
            input
                .media_entry_id
                .as_ref()
                .is_none_or(|id| *id == r.entry.id)
        })
        .map(|r| super::subtitles::SubtitleSource {
            media_entry_id: r.entry.id.clone(),
            source_version: r.source_version(),
        });
    for source in &mut detail.sources {
        if let Some(file) = input
            .item
            .renditions
            .iter()
            .find(|r| Some(&r.entry.id) == source.media_entry_id.as_ref())
            .and_then(|r| r.files.get((source.part - 1) as usize))
        {
            source.catalog_info = file.media.clone();
            source.streams = Some(file.media.clone().map(Into::into));
        }
    }
    let mut neg = crate::sessions::Negotiation::preferences(
        &s.sessions,
        &s.registry,
        &claims.sub,
        q.profile,
        q.audio_track,
        q.video_track,
    )
    .await
    .map_err(internal)?;
    neg.catalogue_subtitle(&input, &id, q.subtitle_track)
        .map_err(session_refusal)?;
    neg.catalogue_audio_tracks(
        &input,
        &detail
            .sources
            .iter()
            .filter_map(|source| {
                Some((
                    source.media_entry_id.clone()?,
                    *q.source_audio_tracks.get(&source.source_id)?,
                ))
            })
            .collect(),
    );
    let (parts, info, sp, mode) = match input.negotiate(&mut neg, q.mode.as_deref(), None).await {
        Ok(result) => result,
        Err(error) => {
            let error = session_refusal(error);
            detail.query.unavailable = Some(ApiErrorBody::new(
                error.code(),
                if error.code() == ErrorCode::SourceOffline {
                    "the machine holding this file is not connected right now"
                } else {
                    "this item has no playable rendition"
                },
            ));
            return Ok(Json(detail));
        }
    };
    let capture = input.capture(&parts, &id).map_err(internal)?;
    let mut subtitles = crate::sessions::catalogue::listing(
        &id,
        &parts,
        &info,
        neg.profile(),
        &neg.ass,
        &capture.subtitles,
    );
    for s in &mut subtitles {
        s.deletable = s.track.origin == "downloaded"
            && (claims.admin || s.track.created_by.as_deref() == Some(&claims.sub));
    }
    detail.query.subtitle_source = input
        .item
        .renditions
        .iter()
        .find(|r| r.entry.id == capture.media_entry_id)
        .map(|r| super::subtitles::SubtitleSource {
            media_entry_id: r.entry.id.clone(),
            source_version: r.source_version(),
        });
    detail.query.segments = capture.segments.clone();
    let source = detail
        .sources
        .iter()
        .find(|s| s.media_entry_id.as_ref() == Some(&capture.media_entry_id))
        .ok_or_else(|| ApiError::new(ErrorCode::Conflict, "sources changed; reload the item"))?;
    let video = info.video.first();
    let source_id = source.source_id;
    detail.duration_ms = if parts.len() > 1 {
        Some(parts.iter().map(|p| p.duration_ms).sum::<u64>())
    } else {
        info.duration_ms
    }
    .and_then(|v| i64::try_from(v).ok());
    detail.chapters = group_chapters(
        detail
            .sources
            .iter()
            .filter(|s| s.source_id == source_id)
            .map(|s| s.catalog_info.clone()),
    );
    detail.query.negotiated = Some(NegotiatedItem {
        source: Some(NegotiatedSource {
            source_id,
            module_id: parts[0].module_id.clone(),
            collection_id: parts[0].collection_id.clone(),
            path_rel: parts[0].path_rel.clone(),
            display_width: video.and_then(|v| v.display_width),
            display_height: video.and_then(|v| v.display_height),
            orientation: video.and_then(|v| v.orientation.clone()),
        }),
        mode,
        cost: sp.cost.as_str().into(),
        target_duration_secs: sp.target_duration_secs,
        streams: NegotiatedStreams {
            video: sp.video_verdict,
            audio: sp.audio_verdict,
            subtitles: sp.subtitles,
        },
        subtitles,
    });
    Ok(Json(detail))
}

#[utoipa::path(get,path="/api/v1/playback/sessions/{id}/fonts",tag="Playback media",security(("bearer_auth"=[]),("media_token"=[])),params(("id"=String,Path)),responses((status=200,body=FontsResponse)))]
pub(crate) async fn session_fonts(
    State(s): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<FontsResponse>, ApiError> {
    Ok(Json(FontsResponse {
        fonts: fonts(&s, &id)
            .await?
            .into_iter()
            .map(|(name, _)| name)
            .collect(),
    }))
}
async fn fonts(s: &AppState, id: &str) -> Result<Vec<(String, Vec<u8>)>, ApiError> {
    let session = s.sessions.get(id).ok_or_else(session_gone)?;
    let source = session.physical_source().ok_or_else(|| hidden("source"))?;
    s.subtitles
        .fonts_for_source(&s.registry, &s.sessions, source)
        .await
        .map_err(internal)
}
#[utoipa::path(get,path="/api/v1/playback/sessions/{id}/fonts/{n}",tag="Playback media",security(("bearer_auth"=[]),("media_token"=[])),params(("id"=String,Path),("n"=usize,Path)),responses((status=200,body=Vec<u8>,content_type="font/ttf")))]
pub(crate) async fn session_font(
    State(s): State<AppState>,
    ApiPath((id, n)): ApiPath<(String, usize)>,
) -> Result<Response, ApiError> {
    let (_, bytes) = fonts(&s, &id)
        .await?
        .into_iter()
        .nth(n)
        .ok_or_else(|| hidden("font"))?;
    Ok(([(axum::http::header::CONTENT_TYPE, "font/ttf")], bytes).into_response())
}
#[utoipa::path(get,path="/api/v1/playback/sessions/{id}/subtitles/{file}",tag="Playback media",security(("bearer_auth"=[]),("media_token"=[])),params(("id"=String,Path),("file"=String,Path),VttQuery),responses((status=200,body=String)))]
pub(crate) async fn session_subtitle(
    State(s): State<AppState>,
    ApiPath((id, file)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<VttQuery>,
) -> Result<Response, ApiError> {
    let session = s.sessions.get(&id).ok_or_else(session_gone)?;
    let (id, ext) = file.split_once('.').ok_or_else(|| hidden("subtitle"))?;
    let track = session
        .catalogue_track(id.parse().map_err(|_| hidden("subtitle"))?)
        .ok_or_else(|| hidden("subtitle"))?;
    if let Some(path) = &track.raster {
        if ext != "jsonl" {
            return Err(hidden("raster subtitle"));
        }
        return Ok((
            [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
            tokio::fs::read(path).await.map_err(internal)?,
        )
            .into_response());
    }
    if crate::tracks::is_image_format(&track.format) {
        return Err(ApiError::new(
            ErrorCode::UnsupportedTrack,
            "image subtitles have no text form",
        ));
    }
    if ext == "vtt" {
        let body = s
            .subtitles
            .vtt(&s.registry, &s.sessions, &track, q.shift_ms.round() as i64)
            .await
            .map_err(internal)?;
        return Ok((
            [(axum::http::header::CONTENT_TYPE, "text/vtt; charset=utf-8")],
            body,
        )
            .into_response());
    }
    if ext != "ass" {
        return Err(hidden("subtitle"));
    }
    let body = s
        .subtitles
        .ass_body(&s.registry, &s.sessions, &track)
        .await
        .map_err(internal)?;
    let headers = [(
        axum::http::header::CONTENT_TYPE,
        "text/x-ssa; charset=utf-8",
    )];
    Ok(match body {
        crate::subtitles::AssBody::Full(body) => (headers, body).into_response(),
        crate::subtitles::AssBody::Stream(rx) => {
            let stream = tokio_stream::StreamExt::map(
                tokio_stream::wrappers::ReceiverStream::new(rx),
                |s| Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(s)),
            );
            (headers, axum::body::Body::from_stream(stream)).into_response()
        }
    })
}

#[utoipa::path(get,path="/api/v1/catalogue/libraries/{library}/items/{item}/next",tag="Catalogue",security(("bearer_auth"=[])),params(("library"=String,Path),("item"=String,Path),NextSource),responses((status=200,body=Option<m::LibraryChild>)))]
pub(crate) async fn catalogue_next(
    State(s): State<AppState>,
    ApiPath((library, item)): ApiPath<(String, String)>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiQuery(q): ApiQuery<NextSource>,
) -> Result<Json<Option<m::LibraryChild>>, ApiError> {
    visible(&s, &claims, &library).await?;
    let current = s
        .registry
        .catalogue()
        .library_child(&library, &item)
        .await
        .map_err(store_error)?;
    let m::ChildPosition::Episode { season, episode } = current.child.position else {
        return Ok(Json(None));
    };
    let (season, episode) = if let Some(entry) = q.media_entry_id {
        let rendition = current
            .renditions
            .iter()
            .find(|r| r.id == entry)
            .ok_or_else(|| hidden("source"))?;
        let m::EntryKind::Episode { episodes } = &rendition.data.kind else {
            return Err(hidden("source"));
        };
        episodes
            .iter()
            .map(|s| (s.season, s.episode_end.unwrap_or(s.episode)))
            .max()
            .unwrap_or((season, episode))
    } else {
        (season, episode)
    };
    Ok(Json(
        s.registry
            .catalogue()
            .next_episode(
                &library,
                &current.child.parent_id,
                (season, episode),
                &Default::default(),
            )
            .await
            .map_err(store_error)?
            .map(|next| next.child),
    ))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in=Query)]
pub(crate) struct NextSource {
    media_entry_id: Option<String>,
}
