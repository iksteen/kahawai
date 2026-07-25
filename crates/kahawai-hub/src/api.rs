//! Client API (HUB-11/12 first cut): setup + token auth, then browse —
//! collections, items, item detail with full technical stream info.
//! During setup mode (OPS-1) nothing but /setup is reachable.

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::auth::Auth;
use crate::registry::Registry;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub auth: Arc<Auth>,
    pub sessions: Arc<crate::sessions::Sessions>,
    pub enrollments: Arc<crate::enrollment_service::EnrollmentService>,
    pub subtitles: Arc<crate::subtitles::Subtitles>,
    pub artwork: Arc<crate::artwork::Artwork>,
    pub enricher: Arc<crate::enrich::Enricher>,
}

pub fn router(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<crate::sessions::Sessions>,
    enrollments: Arc<crate::enrollment_service::EnrollmentService>,
    subtitles: Arc<crate::subtitles::Subtitles>,
    artwork: Arc<crate::artwork::Artwork>,
    enricher: Arc<crate::enrich::Enricher>,
) -> Router {
    let state =
        AppState { registry, auth, sessions, enrollments, subtitles, artwork, enricher };
    let protected = Router::new()
        .route("/api/v1/collections", get(list_collections))
        .route("/api/v1/libraries", get(list_libraries))
        .route("/api/v1/items", get(list_items))
        .route("/api/v1/items/{id}", get(item_detail))
        .route("/api/v1/items/{id}/children", get(item_children))
        .route("/api/v1/items/{id}/artwork", get(item_artwork))
        .route("/api/v1/items/{id}/subtitles", get(item_subtitles))
        .route("/api/v1/items/{id}/subtitles/{file}", get(item_subtitle_file))
        .route("/api/v1/items/{id}/fonts", get(item_fonts))
        .route("/api/v1/items/{id}/fonts/{n}", get(item_font))
        .route("/api/v1/prefs", get(get_prefs).put(put_pref))
        .route("/api/v1/playback/sessions", post(start_session))
        .route("/api/v1/playback/sessions/{id}", axum::routing::delete(end_session))
        .route("/api/v1/playback/sessions/{id}/stream", get(stream_session))
        .route("/api/v1/playback/sessions/{id}/progress", post(post_progress))
        .route("/api/v1/playback/sessions/{id}/seek", post(seek_session))
        .route("/api/v1/playback/sessions/{id}/{file}", get(session_file))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));
    let admin = Router::new()
        .route("/admin/v1/enrollments", get(admin_enrollments))
        .route("/admin/v1/enrollments/approve", post(admin_approve))
        .route("/admin/v1/satellites", get(admin_satellites))
        .route("/admin/v1/satellites/{id}", axum::routing::delete(admin_delete_satellite))
        .route("/admin/v1/satellites/{id}/disabled", post(admin_set_disabled))
        .route("/admin/v1/libraries", get(admin_libraries).post(admin_create_library))
        .route("/admin/v1/libraries/{id}", axum::routing::delete(admin_delete_library))
        .route(
            "/admin/v1/libraries/{id}/collections",
            post(admin_attach_collection),
        )
        .route(
            "/admin/v1/libraries/{id}/collections/{module_id}/{collection_id}",
            axum::routing::delete(admin_detach_collection),
        )
        .route("/admin/v1/collections", get(admin_collections))
        .route("/admin/v1/users", post(admin_create_user))
        .route("/admin/v1/providers", get(admin_providers))
        .route("/admin/v1/providers/tmdb", post(admin_set_tmdb))
        .route("/admin/v1/providers/tvdb", post(admin_set_tvdb))
        .route("/admin/v1/providers/anidb", post(admin_set_anidb))
        .route("/admin/v1/providers/anidb/verify", post(admin_verify_anidb))
        .route("/admin/v1/enrich", get(admin_enrich_status).post(admin_enrich_run))
        .route("/admin/v1/libraries/{id}/refresh", post(admin_refresh_library))
        .route("/admin/v1/libraries/{id}/anime-view", post(admin_set_anime_view))
        .route("/admin/v1/collections/refresh", post(admin_refresh_collection))
        .route("/admin/v1/enrich/review", get(admin_review_list))
        .route("/admin/v1/enrich/search", post(admin_review_search))
        .route("/admin/v1/items/{id}/match", post(admin_apply_match))
        .route("/admin/v1/sessions", get(admin_sessions))
        .route("/admin/v1/sessions/{id}", axum::routing::delete(admin_end_session))
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .merge(admin)
        .route("/api/v1/setup", post(setup))
        .route("/api/v1/auth/token", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .merge(protected)
        .with_state(state)
        .merge(crate::web::router())
}

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.auth.setup_required() {
        // OPS-1: nothing else is reachable until setup completes.
        tracing::warn!(path = %req.uri(), "503: setup_required returned true");
        return Err((StatusCode::SERVICE_UNAVAILABLE, "setup required".into()));
    }
    // Bearer header first; the kahawai_token cookie is the fallback for
    // <video>/HLS requests, which cannot set headers (HUB-27).
    let header_token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let cookie_token = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .filter_map(|kv| kv.trim().split_once('='))
                .find(|(k, _)| *k == "kahawai_token")
                .map(|(_, v)| v.to_string())
        });
    let claims = header_token
        .or(cookie_token)
        .and_then(|t| state.auth.verify(&t).ok())
        .ok_or((StatusCode::UNAUTHORIZED, "invalid or missing token".to_string()))?;
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

/// Layered after require_auth: the Claims extension is already present.
async fn require_admin(req: Request, next: Next) -> Result<Response, ApiError> {
    let is_admin = req
        .extensions()
        .get::<crate::auth::Claims>()
        .is_some_and(|c| c.admin);
    if !is_admin {
        return Err((StatusCode::FORBIDDEN, "admin only".into()));
    }
    Ok(next.run(req).await)
}

async fn admin_enrollments(State(state): State<AppState>) -> Json<Value> {
    let pending: Vec<Value> = state
        .enrollments
        .pending()
        .iter()
        .map(|p| {
            json!({
                "csr_fingerprint": p.csr_fingerprint,
                "module_type": p.module_type,
                "module_id": p.module_id,
                "name": p.name,
            })
        })
        .collect();
    Json(json!({ "pending": pending }))
}

#[derive(Deserialize)]
struct ApproveRequest {
    code: String,
}

async fn admin_approve(
    State(state): State<AppState>,
    Json(body): Json<ApproveRequest>,
) -> Result<Json<Value>, ApiError> {
    let summary = state
        .enrollments
        .approve(&body.code)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(Json(json!({ "approved": summary })))
}

async fn admin_providers(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let tmdb = state
        .registry
        .get_setting(crate::enrich::TMDB_KEY_SETTING)
        .await
        .map_err(internal)?
        .is_some();
    let tvdb = state
        .registry
        .get_setting(crate::enrich::TVDB_KEY_SETTING)
        .await
        .map_err(internal)?
        .is_some();
    let anidb = state
        .registry
        .get_setting(crate::anidb::USER_SETTING)
        .await
        .map_err(internal)?
        .is_some();
    Ok(Json(json!({
        "tmdb": { "configured": tmdb },
        "tvdb": { "configured": tvdb },
        "anidb": { "configured": anidb },
    })))
}

/// Re-validate the STORED AniDB credentials (no resend needed).
async fn admin_verify_anidb(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let user = state.registry.get_setting(crate::anidb::USER_SETTING).await.map_err(internal)?;
    let pass = state.registry.get_setting(crate::anidb::PASS_SETTING).await.map_err(internal)?;
    let key = state
        .registry
        .get_setting(crate::anidb::APIKEY_SETTING)
        .await
        .map_err(internal)?
        .filter(|k| !k.is_empty());
    let (Some(user), Some(pass)) = (user, pass) else {
        return Err((StatusCode::BAD_REQUEST, "no AniDB account configured".into()));
    };
    match crate::anidb::Anidb::login(&user, &pass, key.as_deref()).await {
        Ok(client) => {
            client.logout().await;
            Ok(Json(json!({ "verified": true })))
        }
        Err(e) => Ok(Json(json!({ "verified": false, "error": format!("{e:#}") }))),
    }
}

#[derive(Deserialize)]
struct SetAnidb {
    username: String,
    password: String,
    #[serde(default)]
    udp_api_key: Option<String>,
}

/// AniDB account for the UDP FILE-by-ED2K gold path (HUB-30). The
/// client identity ("kahawai" v1) is compiled in; only the account is
/// configuration. Optional UDP API key upgrades to an encrypted session.
async fn admin_set_anidb(
    State(state): State<AppState>,
    Json(body): Json<SetAnidb>,
) -> Result<Json<Value>, ApiError> {
    let (user, pass) = (body.username.trim(), body.password.trim());
    if user.is_empty() || pass.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "username and password required".into()));
    }
    state.registry.set_setting(crate::anidb::USER_SETTING, user).await.map_err(internal)?;
    state.registry.set_setting(crate::anidb::PASS_SETTING, pass).await.map_err(internal)?;
    // Empty key = clear it (plaintext session); the login path treats
    // an empty stored key as absent.
    state
        .registry
        .set_setting(
            crate::anidb::APIKEY_SETTING,
            body.udp_api_key.as_deref().map(str::trim).unwrap_or(""),
        )
        .await
        .map_err(internal)?;
    // Validate immediately: a bad login should fail HERE, not silently
    // during the next enrichment run.
    let key = state
        .registry
        .get_setting(crate::anidb::APIKEY_SETTING)
        .await
        .map_err(internal)?
        .filter(|k| !k.is_empty());
    match crate::anidb::Anidb::login(user, pass, key.as_deref()).await {
        Ok(client) => {
            client.logout().await;
            let enricher = state.enricher.clone();
            let registry = state.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = enricher.run_once(&registry).await {
                    tracing::warn!(error = format!("{e:#}"), "enrichment run failed");
                }
            });
            Ok(Json(json!({ "saved": true, "verified": true })))
        }
        Err(e) => Ok(Json(json!({ "saved": true, "verified": false, "error": format!("{e:#}") }))),
    }
}

#[derive(Deserialize)]
struct SetTvdb {
    api_key: String,
    #[serde(default)]
    pin: Option<String>,
}

async fn admin_set_tvdb(
    State(state): State<AppState>,
    Json(body): Json<SetTvdb>,
) -> Result<Json<Value>, ApiError> {
    let key = body.api_key.trim();
    if key.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "api_key required".into()));
    }
    state
        .registry
        .set_setting(crate::enrich::TVDB_KEY_SETTING, key)
        .await
        .map_err(internal)?;
    if let Some(pin) = body.pin.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        state
            .registry
            .set_setting(crate::enrich::TVDB_PIN_SETTING, pin)
            .await
            .map_err(internal)?;
    }
    let enricher = state.enricher.clone();
    let registry = state.registry.clone();
    tokio::spawn(async move {
        if let Err(e) = enricher.run_once(&registry).await {
            tracing::warn!(error = format!("{e:#}"), "enrichment run failed");
        }
    });
    Ok(Json(json!({ "saved": true })))
}

#[derive(Deserialize)]
struct SetTmdb {
    api_key: String,
}

async fn admin_set_tmdb(
    State(state): State<AppState>,
    Json(body): Json<SetTmdb>,
) -> Result<Json<Value>, ApiError> {
    let key = body.api_key.trim();
    if key.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "api_key required".into()));
    }
    state
        .registry
        .set_setting(crate::enrich::TMDB_KEY_SETTING, key)
        .await
        .map_err(internal)?;
    // Kick a run right away — saving the key is the natural trigger.
    let enricher = state.enricher.clone();
    let registry = state.registry.clone();
    tokio::spawn(async move {
        if let Err(e) = enricher.run_once(&registry).await {
            tracing::warn!(error = format!("{e:#}"), "enrichment run failed");
        }
    });
    Ok(Json(json!({ "saved": true })))
}

async fn admin_enrich_status(State(state): State<AppState>) -> Json<Value> {
    Json(state.enricher.status())
}

async fn admin_enrich_run(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let enricher = state.enricher.clone();
    let registry = state.registry.clone();
    tokio::spawn(async move {
        if let Err(e) = enricher.run_once(&registry).await {
            tracing::warn!(error = format!("{e:#}"), "enrichment run failed");
        }
    });
    Ok(Json(json!({ "started": true })))
}

#[derive(Deserialize, Default)]
struct RescanRequest {
    #[serde(default)]
    collection_id: Option<String>,
}

/// HUB-31: per-library anime presentation — 'seasons' (TVDB-style
/// projection) or 'native' (flat absolute order).
async fn admin_set_anime_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let view = body.get("anime_view").and_then(|v| v.as_str()).unwrap_or_default();
    if !matches!(view, "seasons" | "native") {
        return Err((StatusCode::BAD_REQUEST, "anime_view must be seasons|native".into()));
    }
    let n = sqlx::query("UPDATE libraries SET anime_view = ? WHERE id = ?")
        .bind(view)
        .bind(&id)
        .execute(state.registry.db())
        .await
        .map_err(internal)?
        .rows_affected();
    if n == 0 {
        return Err((StatusCode::NOT_FOUND, "no such library".into()));
    }
    Ok(Json(json!({ "anime_view": view })))
}

/// HUB-35: granular refresh. The admin-facing unit is the LIBRARY —
/// fan out collection-scoped scan requests to each member collection's
/// mediahost. There is deliberately no global rescan.
async fn admin_refresh_library(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT module_id, collection_id FROM library_collections WHERE library_id = ?",
    )
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    if members.is_empty() {
        return Err((StatusCode::NOT_FOUND, "library has no collections".into()));
    }
    let (mut asked, mut offline) = (0, 0);
    for (module_id, collection_id) in members {
        if request_scan(&state, &module_id, &collection_id).await {
            asked += 1;
        } else {
            offline += 1;
        }
    }
    Ok(Json(json!({ "asked": asked, "offline": offline })))
}

#[derive(Deserialize)]
struct RefreshCollectionRequest {
    module_id: String,
    collection_id: String,
}

async fn admin_refresh_collection(
    State(state): State<AppState>,
    Json(body): Json<RefreshCollectionRequest>,
) -> Result<Json<Value>, ApiError> {
    let asked = request_scan(&state, &body.module_id, &body.collection_id).await;
    Ok(Json(json!({ "asked": asked as u32, "offline": !asked as u32 })))
}

/// Send one collection-scoped scan request (MH-2); the mediahost's
/// trigger sink coalesces with any running scan.
async fn request_scan(state: &AppState, module_id: &str, collection_id: &str) -> bool {
    if !state.registry.is_connected(module_id) {
        return false;
    }
    let msg = kahawai_proto::v1::HubToHost {
        msg: Some(kahawai_proto::v1::hub_to_host::Msg::RescanRequest(
            kahawai_proto::v1::RescanRequest { collection_id: collection_id.to_string() },
        )),
    };
    state.registry.send_to_host(module_id, msg).await.is_ok()
}

/// HUB-8 review queue: everything not matched confidently — misses,
/// weak matches (with their current guess for confirm/reject), and
/// rejected items.
async fn admin_review_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        "SELECT i.id, i.kind, i.title, i.year, m.confidence,
                m.title AS matched_title, m.premiered, m.provider, m.provider_id,
                (SELECT s.path_rel FROM item_sources s WHERE s.item_id = i.id LIMIT 1) AS path
         FROM items i
         JOIN item_metadata m ON m.item_id = i.id
         WHERE m.confidence IN ('miss', 'weak', 'rejected')
         ORDER BY m.confidence != 'miss', i.title",
    )
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "item_id": r.get::<String, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "title": r.get::<String, _>("title"),
                "year": r.get::<Option<i64>, _>("year"),
                "path": r.get::<Option<String>, _>("path"),
                "confidence": r.get::<String, _>("confidence"),
                "matched_title": r.get::<Option<String>, _>("matched_title"),
                "premiered": r.get::<Option<String>, _>("premiered"),
                "provider": r.get::<String, _>("provider"),
            })
        })
        .collect();
    Ok(Json(json!({ "entries": entries })))
}

#[derive(Deserialize)]
struct ReviewSearch {
    kind: String,
    query: String,
    year: Option<i64>,
}

async fn admin_review_search(
    State(state): State<AppState>,
    Json(body): Json<ReviewSearch>,
) -> Result<Json<Value>, ApiError> {
    let candidates = state
        .enricher
        .search_candidates(&state.registry, &body.kind, &body.query, body.year)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "candidates": candidates })))
}

#[derive(Deserialize)]
struct ApplyMatch {
    /// "pick": store the supplied candidate; "confirm": promote the
    /// current weak match; "reject": clear the match, excluded from
    /// auto-retries.
    action: String,
    provider: Option<String>,
    candidate: Option<Value>,
}

async fn admin_apply_match(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApplyMatch>,
) -> Result<Json<Value>, ApiError> {
    let db = state.registry.db();
    match body.action.as_str() {
        "confirm" => {
            sqlx::query(
                "UPDATE item_metadata SET confidence = 'manual', updated_at = unixepoch()
                 WHERE item_id = ? AND provider_id != ''",
            )
            .bind(&id)
            .execute(db)
            .await
            .map_err(internal)?;
        }
        "reject" => {
            sqlx::query(
                "UPDATE item_metadata SET provider_id = '', title = NULL, overview = NULL,
                        poster_path = NULL, rating = NULL, premiered = NULL,
                        confidence = 'rejected', updated_at = unixepoch()
                 WHERE item_id = ?",
            )
            .bind(&id)
            .execute(db)
            .await
            .map_err(internal)?;
        }
        "pick" => {
            let c = body.candidate.ok_or((StatusCode::BAD_REQUEST, "candidate required".into()))?;
            let provider = body
                .provider
                .ok_or((StatusCode::BAD_REQUEST, "provider required".into()))?;
            let pid = c["id"]
                .as_u64()
                .ok_or((StatusCode::BAD_REQUEST, "candidate.id required".into()))?;
            sqlx::query(
                "INSERT INTO item_metadata
                   (item_id, provider, provider_id, title, overview, poster_path,
                    rating, premiered, genres, confidence, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, 'manual', unixepoch())
                 ON CONFLICT (item_id) DO UPDATE SET
                   provider = excluded.provider,
                   provider_id = excluded.provider_id,
                   title = excluded.title,
                   overview = excluded.overview,
                   poster_path = excluded.poster_path,
                   rating = excluded.rating,
                   premiered = excluded.premiered,
                   confidence = 'manual',
                   updated_at = excluded.updated_at",
            )
            .bind(&id)
            .bind(&provider)
            .bind(pid.to_string())
            .bind(c["title"].as_str())
            .bind(c["overview"].as_str())
            .bind(c["poster_path"].as_str())
            .bind(c["vote_average"].as_f64())
            .bind(c["release_date"].as_str())
            .execute(db)
            .await
            .map_err(internal)?;
        }
        other => return Err((StatusCode::BAD_REQUEST, format!("unknown action {other}"))),
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CreateUser {
    username: String,
    password: String,
    #[serde(default)]
    admin: bool,
}

async fn admin_create_user(
    State(state): State<AppState>,
    Json(body): Json<CreateUser>,
) -> Result<Json<Value>, ApiError> {
    let id = state
        .auth
        .create_user(&body.username, &body.password, body.admin)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "id": id, "username": body.username, "admin": body.admin })))
}

async fn admin_satellites(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let sats = state.registry.satellites_overview().await.map_err(internal)?;
    Ok(Json(json!({ "satellites": sats })))
}

/// SEC-6/HUB-20: allowlist removal + end sessions + cascade. Refusal of
/// reconnection happens at the TLS layer (fingerprint no longer admitted).
async fn admin_delete_satellite(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let ended = state.sessions.end_for_module(&id);
    let fingerprint = state
        .registry
        .delete_satellite(&id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
    Ok(Json(json!({ "deleted": id, "removed": fingerprint, "sessions_ended": ended })))
}

#[derive(serde::Deserialize)]
struct SetDisabled {
    disabled: bool,
}

/// Admin drain toggle: placement skips a disabled satellite; running
/// sessions finish on their own.
async fn admin_libraries(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let libraries = state.registry.libraries_overview().await.map_err(internal)?;
    Ok(Json(json!({ "libraries": libraries })))
}

async fn admin_collections(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let collections = state.registry.collections_overview().await.map_err(internal)?;
    Ok(Json(json!({ "collections": collections })))
}

#[derive(Deserialize)]
struct CreateLibraryRequest {
    name: String,
    media_type: String,
}

async fn admin_create_library(
    State(state): State<AppState>,
    Json(body): Json<CreateLibraryRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "library name required".into()));
    }
    let id = state
        .registry
        .create_library(name, &body.media_type)
        .await
        .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({ "id": id })))
}

async fn admin_delete_library(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.registry.delete_library(&id).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such library".into()))
    }
}

#[derive(Deserialize)]
struct AttachCollectionRequest {
    module_id: String,
    collection_id: String,
}

async fn admin_attach_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AttachCollectionRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .registry
        .attach_collection(&id, &body.module_id, &body.collection_id)
        .await
        .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_detach_collection(
    State(state): State<AppState>,
    Path((id, module_id, collection_id)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    if state
        .registry
        .detach_collection(&id, &module_id, &collection_id)
        .await
        .map_err(internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "not attached".into()))
    }
}

async fn admin_set_disabled(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetDisabled>,
) -> Result<StatusCode, ApiError> {
    state.registry.set_disabled(&id, body.disabled).await.map_err(internal)?;
    tracing::info!(module_id = %id, disabled = body.disabled, "satellite placement toggle");
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_sessions(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mut out = Vec::new();
    for s in state.sessions.list() {
        let title: Option<String> = sqlx::query_scalar("SELECT title FROM items WHERE id = ?")
            .bind(&s.item_id)
            .fetch_optional(state.registry.db())
            .await
            .map_err(internal)?;
        let username: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(&s.user_id)
            .fetch_optional(state.registry.db())
            .await
            .map_err(internal)?;
        out.push(json!({
            "session_id": s.id,
            "username": username,
            "title": title,
            "mode": match &s.mode {
                crate::sessions::Mode::Direct { .. } => "direct",
                crate::sessions::Mode::Remux { .. } => "remux",
                crate::sessions::Mode::Transcode { .. } => "transcode",
            },
            "module_id": s.module_id,
            "idle_secs": s.idle_for().as_secs(),
            "streams": s.verdict.as_ref().map(|(video, audio)| json!({
                "video": video,
                "audio": audio,
            })),
        }));
    }
    Ok(Json(json!({ "sessions": out })))
}

async fn admin_end_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.sessions.end(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such session".into()))
    }
}

#[derive(Deserialize)]
struct SetupRequest {
    token: String,
    username: String,
    password: String,
}

async fn setup(
    State(state): State<AppState>,
    Json(body): Json<SetupRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !state.auth.setup_required() {
        return Err((StatusCode::CONFLICT, "setup already completed".into()));
    }
    let tokens = state
        .auth
        .complete_setup(&body.token, &body.username, &body.password)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(json!(tokens))))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<Value>, ApiError> {
    if state.auth.setup_required() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "setup required".into()));
    }
    let tokens = state
        .auth
        .login(&body.username, &body.password)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid credentials".to_string()))?;
    Ok(Json(json!(tokens)))
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<Value>, ApiError> {
    let tokens = state
        .auth
        .refresh(&body.refresh_token)
        .await
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid refresh token".to_string()))?;
    Ok(Json(json!(tokens)))
}

#[derive(Deserialize)]
struct StartSessionRequest {
    item_id: String,
    #[serde(default = "default_mode")]
    mode: String,
    /// Begin playback here (resume without waiting for a transcode to
    /// catch up) — keyframe-snapped by the pipeline.
    #[serde(default)]
    start_ms: u64,
    /// Track indexes in the source's discovery order (HUB-27). The UI
    /// resolves defaults from /api/v1/prefs client-side (HUB-33).
    #[serde(default)]
    audio_track: u32,
    #[serde(default)]
    video_track: u32,
}

fn default_mode() -> String {
    "direct".into()
}

/// Per-user preferences (HUB-33): tiny generic KV, scope = library id
/// or '' for user-global keys.
async fn get_prefs(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query("SELECT scope, key, value FROM user_prefs WHERE user_id = ?")
        .bind(&claims.sub)
        .fetch_all(state.registry.db())
        .await
        .map_err(internal)?;
    let prefs: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "scope": r.get::<String, _>("scope"),
                "key": r.get::<String, _>("key"),
                "value": r.get::<String, _>("value"),
            })
        })
        .collect();
    Ok(Json(json!({ "prefs": prefs })))
}

#[derive(Deserialize)]
struct PutPrefRequest {
    #[serde(default)]
    scope: String,
    key: String,
    /// Empty value deletes the preference.
    value: String,
}

async fn put_pref(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<PutPrefRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.key.len() > 64 || body.value.len() > 256 || body.scope.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "preference too long".into()));
    }
    if body.value.is_empty() {
        sqlx::query("DELETE FROM user_prefs WHERE user_id = ? AND scope = ? AND key = ?")
            .bind(&claims.sub)
            .bind(&body.scope)
            .bind(&body.key)
            .execute(state.registry.db())
            .await
            .map_err(internal)?;
    } else {
        sqlx::query(
            "INSERT INTO user_prefs (user_id, scope, key, value) VALUES (?, ?, ?, ?)
             ON CONFLICT (user_id, scope, key) DO UPDATE SET value = excluded.value",
        )
        .bind(&claims.sub)
        .bind(&body.scope)
        .bind(&body.key)
        .bind(&body.value)
        .execute(state.registry.db())
        .await
        .map_err(internal)?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn start_session(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<StartSessionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let session = state
        .sessions
        .start(
            &state.registry,
            &claims.sub,
            &body.item_id,
            &body.mode,
            body.start_ms,
            body.audio_track,
            body.video_track,
        )
        .await
        .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    let (mode, stream_url, ctype) = match &session.mode {
        crate::sessions::Mode::Direct { .. } => (
            "direct",
            format!("/api/v1/playback/sessions/{}/stream", session.id),
            content_type(session.container.as_deref()).to_string(),
        ),
        crate::sessions::Mode::Remux { .. } => (
            "remux",
            format!("/api/v1/playback/sessions/{}/master.m3u8", session.id),
            "application/vnd.apple.mpegurl".to_string(),
        ),
        crate::sessions::Mode::Transcode { .. } => (
            "transcode",
            format!("/api/v1/playback/sessions/{}/master.m3u8", session.id),
            "application/vnd.apple.mpegurl".to_string(),
        ),
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "mode": mode,
            "size": session.size,
            "duration_ms": session.duration_ms,
            "part_base_ms": session.part_base_ms(),
            "parts": session.parts.len(),
            "content_type": ctype,
            "stream_url": stream_url,
            "streams": session.verdict.as_ref().map(|(video, audio)| json!({
                "video": video,
                "audio": audio,
            })),
        })),
    ))
}

#[derive(Deserialize)]
struct SeekRequest {
    position_ms: u64,
    /// Switch tracks during the restart (HUB-27).
    audio_track: Option<u32>,
    video_track: Option<u32>,
}

/// Seek-restart (§6): restart the session's pipeline at the offset.
/// Same session id and URLs; the client re-attaches to the playlist.
async fn seek_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SeekRequest>,
) -> Result<Json<Value>, ApiError> {
    let part_base_ms = state
        .sessions
        .seek(&state.registry, &id, body.position_ms, body.audio_track, body.video_track)
        .await
        .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({ "part_base_ms": part_base_ms })))
}

async fn end_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.sessions.end(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such session".into()))
    }
}

fn content_type(container: Option<&str>) -> &'static str {
    match container {
        Some("matroska") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mp4") => "video/mp4",
        Some("mpegts") => "video/mp2t",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Parse a Range header against a resource of `size` bytes.
/// Returns `(offset, len)`, or None for absent/unsupported forms.
fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = header else { return Ok(None) };
    let spec = h.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(()); // multi-range unsupported
    }
    let (start_s, end_s) = spec.split_once('-').ok_or(())?;
    match (start_s.is_empty(), end_s.is_empty()) {
        // bytes=-N → last N bytes
        (true, false) => {
            let n: u64 = end_s.parse().map_err(|_| ())?;
            if n == 0 || size == 0 {
                return Err(());
            }
            let n = n.min(size);
            Ok(Some((size - n, n)))
        }
        // bytes=S- → from S to end
        (false, true) => {
            let s: u64 = start_s.parse().map_err(|_| ())?;
            if s >= size {
                return Err(());
            }
            Ok(Some((s, size - s)))
        }
        // bytes=S-E inclusive
        (false, false) => {
            let s: u64 = start_s.parse().map_err(|_| ())?;
            let e: u64 = end_s.parse().map_err(|_| ())?;
            if s > e || s >= size {
                return Err(());
            }
            let e = e.min(size - 1);
            Ok(Some((s, e - s + 1)))
        }
        (true, true) => Err(()),
    }
}


async fn stream_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such session".to_string()))?;
    session.touch();
    let crate::sessions::Mode::Direct { lease } = &session.mode else {
        return Err((StatusCode::CONFLICT, "not a direct-play session".into()));
    };
    let range = headers.get(axum::http::header::RANGE).and_then(|v| v.to_str().ok());

    let (status, offset, len) = match parse_range(range, session.size) {
        Ok(None) => (StatusCode::OK, 0, session.size),
        Ok(Some((offset, len))) => (StatusCode::PARTIAL_CONTENT, offset, len),
        Err(()) => {
            return Ok(axum::response::Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{}", session.size))
                .body(axum::body::Body::empty())
                .unwrap());
        }
    };

    // Long-running transfers count as activity chunk by chunk (HUB-18).
    let keepalive = session.clone();
    let body = axum::body::Body::from_stream(tokio_stream::StreamExt::map(
        lease.read_range(offset, len),
        move |chunk| {
            keepalive.touch();
            chunk
        },
    ));
    let mut resp = axum::response::Response::builder()
        .status(status)
        .header("accept-ranges", "bytes")
        .header("content-length", len)
        .header("content-type", content_type(session.container.as_deref()));
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(
            "content-range",
            format!("bytes {}-{}/{}", offset, offset + len - 1, session.size),
        );
    }
    Ok(resp.body(body).unwrap())
}

async fn list_collections(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let cols = state.registry.collections().await.map_err(internal)?;
    Ok(Json(json!({ "collections": cols })))
}

async fn item_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    match state
        .artwork
        .get(&state.registry, &state.sessions, &id)
        .await
        .map_err(internal)?
    {
        Some((bytes, ctype)) => Ok((
            [
                (axum::http::header::CONTENT_TYPE, ctype),
                // Local artwork changes only on rescan; let clients keep it.
                (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            bytes,
        )
            .into_response()),
        None => Err((StatusCode::NOT_FOUND, "no artwork".into())),
    }
}

async fn item_subtitles(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let subs = state.subtitles.list(&state.registry, &id).await.map_err(internal)?;
    Ok(Json(json!({ "subtitles": subs })))
}

#[derive(Deserialize)]
struct VttQuery {
    /// f64 so a client that computed a fractional shift still works.
    #[serde(default)]
    shift_ms: f64,
}

/// .vtt (flattened, shiftable) or .ass (faithful, absolute times —
/// ASS renderers offset via the player clock).
async fn item_subtitle_file(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
    Query(q): Query<VttQuery>,
) -> Result<Response, ApiError> {
    if let Some(key) = file.strip_suffix(".ass") {
        let body = state
            .subtitles
            .ass_body(&state.registry, &state.sessions, &id, key)
            .await
            .map_err(internal)?;
        let headers = [
            (axum::http::header::CONTENT_TYPE, "text/x-ssa; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "private, max-age=3600"),
        ];
        return Ok(match body {
            crate::subtitles::AssBody::Full(ass) => (headers, ass).into_response(),
            crate::subtitles::AssBody::Stream(rx) => {
                let stream = tokio_stream::StreamExt::map(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                    |s| Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(s)),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }
        });
    }
    let key = file.strip_suffix(".vtt").unwrap_or(&file);
    let vtt = state
        .subtitles
        .vtt(&state.registry, &state.sessions, &id, key, q.shift_ms.round() as i64)
        .await
        .map_err(internal)?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "private, max-age=60"),
        ],
        vtt,
    )
        .into_response())
}

async fn item_fonts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let fonts = state
        .subtitles
        .fonts(&state.registry, &state.sessions, &id)
        .await
        .map_err(internal)?;
    let names: Vec<&String> = fonts.iter().map(|(n, _)| n).collect();
    Ok(Json(json!({ "fonts": names })))
}

async fn item_font(
    State(state): State<AppState>,
    Path((id, n)): Path<(String, usize)>,
) -> Result<Response, ApiError> {
    let fonts = state
        .subtitles
        .fonts(&state.registry, &state.sessions, &id)
        .await
        .map_err(internal)?;
    let (_, bytes) = fonts
        .into_iter()
        .nth(n)
        .ok_or((StatusCode::NOT_FOUND, "no such font".into()))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "font/ttf"),
            (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        bytes,
    )
        .into_response())
}

async fn list_libraries(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let rows =
        sqlx::query("SELECT id, name, media_type, anime_view FROM libraries ORDER BY name")
            .fetch_all(state.registry.db())
            .await
            .map_err(internal)?;
    let libraries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "name": r.get::<String, _>("name"),
                "media_type": r.get::<String, _>("media_type"),
                "anime_view": r.get::<String, _>("anime_view"),
            })
        })
        .collect();
    Ok(Json(json!({ "libraries": libraries })))
}

#[derive(Deserialize)]
struct ItemsQuery {
    library: Option<String>,
}

async fn list_items(
    State(state): State<AppState>,
    Query(q): Query<ItemsQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    // Shows carry no item_sources of their own; their library membership
    // flows up from their episodes' sources.
    let rows = sqlx::query(
        "SELECT i.id, i.kind, i.season, i.episode, i.artist,
                COALESCE(md.title, i.title) AS title,
                COALESCE(i.year, CAST(substr(md.premiered, 1, 4) AS INTEGER)) AS year,
                i.title AS file_title, i.year AS file_year,
                md.title AS matched_title,
                mdc.confidence AS match_confidence,
                COUNT(s.item_id) AS sources,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN item_sources s ON s.item_id = i.id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?1
         LEFT JOIN item_metadata md ON md.item_id = i.id AND md.provider_id != ''
         LEFT JOIN item_metadata mdc ON mdc.item_id = i.id
         WHERE i.kind NOT IN ('episode', 'track')
           AND (?2 IS NULL OR i.id IN (
             SELECT COALESCE(ci.parent_id, ci.id)
             FROM library_collections lc
             JOIN item_sources ls
               ON ls.module_id = lc.module_id AND ls.collection_id = lc.collection_id
             JOIN items ci ON ci.id = ls.item_id
             WHERE lc.library_id = ?2
           ))
         GROUP BY i.id ORDER BY title, year",
    )
    .bind(&claims.sub)
    .bind(&q.library)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let items: Vec<Value> = rows.iter().map(item_row_json).collect();
    Ok(Json(json!({ "items": items })))
}

fn item_row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "kind": r.get::<String, _>("kind"),
        "title": r.get::<String, _>("title"),
        "artist": r.try_get::<Option<String>, _>("artist").ok().flatten(),
        "match_confidence": r.try_get::<Option<String>, _>("match_confidence").ok().flatten(),
        "premiered": r.try_get::<Option<String>, _>("premiered").ok().flatten(),
        "file_title": r.try_get::<Option<String>, _>("file_title").ok().flatten(),
        "file_year": r.try_get::<Option<i64>, _>("file_year").ok().flatten(),
        "matched_title": r.try_get::<Option<String>, _>("matched_title").ok().flatten(),
        "year": r.get::<Option<i64>, _>("year"),
        "season": r.get::<Option<i64>, _>("season"),
        "episode": r.get::<Option<i64>, _>("episode"),
        "proj_season": r.try_get::<Option<i64>, _>("proj_season").ok().flatten(),
        "proj_episode": r.try_get::<Option<i64>, _>("proj_episode").ok().flatten(),
        "sources": r.get::<i64, _>("sources"),
        "resume_position_ms": r.get::<Option<i64>, _>("position_ms"),
        "resume_duration_ms": r.try_get::<Option<i64>, _>("duration_ms").ok().flatten(),
        "played": r.get::<Option<i64>, _>("played").unwrap_or(0) != 0,
        "play_count": r.get::<Option<i64>, _>("play_count").unwrap_or(0),
    })
}

/// Episodes of a show (docs: /items/{id}/children — seasons are a
/// projection of the season column, not items).
async fn item_children(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        "SELECT i.id, i.kind, i.year, i.season, i.episode, i.artist,
                COALESCE(md.title, i.title) AS title,
                md.premiered AS premiered,
                md.proj_season, md.proj_episode,
                COUNT(s.item_id) AS sources,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN item_sources s ON s.item_id = i.id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
         LEFT JOIN item_metadata md ON md.item_id = i.id AND md.provider_id != ''
         WHERE i.parent_id = ?
         GROUP BY i.id ORDER BY i.season, i.episode",
    )
    .bind(&claims.sub)
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let children: Vec<Value> = rows.iter().map(item_row_json).collect();
    Ok(Json(json!({ "children": children })))
}

async fn item_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let item = sqlx::query(
        "SELECT i.id, i.kind, i.season, i.episode, i.artist,
                COALESCE(md.title, i.title) AS title,
                COALESCE(i.year, CAST(substr(md.premiered, 1, 4) AS INTEGER)) AS year,
                p.id AS parent_id,
                COALESCE(pmd.title, p.title) AS show_title,
                (SELECT COUNT(*) FROM item_sources s WHERE s.item_id = i.id) AS sources,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN items p ON p.id = i.parent_id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
         LEFT JOIN item_metadata md ON md.item_id = i.id AND md.provider_id != ''
         LEFT JOIN item_metadata pmd ON pmd.item_id = p.id AND pmd.provider_id != ''
         WHERE i.id = ?",
    )
        .bind(&claims.sub)
        .bind(&id)
        .fetch_optional(state.registry.db())
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such item".to_string()))?;

    let sources = sqlx::query(
        "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE s.item_id = ? ORDER BY f.size DESC",
    )
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;

    let sources: Vec<Value> = sources
        .iter()
        .map(|r| {
            let module_id: String = r.get("module_id");
            let streams: Value =
                serde_json::from_str(r.get::<String, _>("streams_json").as_str())
                    .unwrap_or(Value::Null);
            json!({
                "module_id": module_id,
                "collection_id": r.get::<String, _>("collection_id"),
                "path_rel": r.get::<String, _>("path_rel"),
                "size": r.get::<i64, _>("size"),
                "available": state.registry.is_connected(&module_id),
                "streams": streams,
            })
        })
        .collect();

    let mut out = item_row_json(&item);
    out["show_title"] = json!(item.get::<Option<String>, _>("show_title"));
    // Hierarchical navigation (episode → its show):
    out["parent_id"] = json!(item.get::<Option<String>, _>("parent_id"));
    out["sources"] = json!(sources);
    // Enrichment (own metadata, or the parent show's for episodes).
    let meta = sqlx::query(
        "SELECT m.overview, m.rating, m.premiered, m.confidence, m.provider FROM items i
         JOIN item_metadata m ON m.item_id IN (i.id, i.parent_id)
         WHERE i.id = ? AND m.provider_id != ''
         ORDER BY m.item_id = i.id DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(state.registry.db())
    .await
    .map_err(internal)?;
    if let Some(m) = meta {
        out["metadata"] = json!({
            "overview": m.get::<Option<String>, _>("overview"),
            "rating": m.get::<Option<f64>, _>("rating"),
            "premiered": m.get::<Option<String>, _>("premiered"),
            "confidence": m.get::<String, _>("confidence"),
            "provider": m.get::<String, _>("provider"),
        });
    }

    // Anime relations (HUB-29): watchable related entries, resolved to
    // in-library items where the target exists here.
    let related = sqlx::query(
        "SELECT r.kind, r.target_title, r.target_anilist, m2.item_id AS local_id
         FROM item_relations r
         LEFT JOIN item_metadata m2 ON m2.anilist_id = r.target_anilist
         WHERE r.from_item = ?
         ORDER BY CASE r.kind
             WHEN 'prequel' THEN 0 WHEN 'sequel' THEN 1 WHEN 'parent' THEN 2
             WHEN 'side_story' THEN 3 WHEN 'spin_off' THEN 4 ELSE 5 END,
             r.target_title",
    )
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    if !related.is_empty() {
        out["related"] = Value::Array(
            related
                .iter()
                .map(|r| {
                    json!({
                        "kind": r.get::<String, _>("kind"),
                        "title": r.get::<Option<String>, _>("target_title"),
                        "item_id": r.get::<Option<String>, _>("local_id"),
                    })
                })
                .collect(),
        );
    }
    Ok(Json(out))
}


#[derive(Deserialize)]
struct ProgressRequest {
    position_ms: u64,
}

/// Record playback progress (HUB-10/18): durable resume position, played
/// flag + play count on crossing 90%, and a session keep-alive.
async fn post_progress(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<ProgressRequest>,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such session".to_string()))?;
    if session.user_id != claims.sub {
        return Err((StatusCode::FORBIDDEN, "not your session".into()));
    }
    session.touch();
    // Pacing (§4.6): the worker throttles its lead over this position.
    state.sessions.viewer_position(&state.registry, &id, body.position_ms);

    let duration = session.duration_ms;
    let finished = duration.is_some_and(|d| d > 0 && body.position_ms * 10 >= d * 9);
    let row = sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, unixepoch())
         ON CONFLICT (user_id, item_id) DO UPDATE SET
           position_ms = excluded.position_ms,
           duration_ms = excluded.duration_ms,
           play_count = play_count + (excluded.played AND NOT played),
           played = MAX(played, excluded.played),
           updated_at = unixepoch()
         RETURNING played, play_count",
    )
    .bind(&claims.sub)
    .bind(&session.item_id)
    .bind(body.position_ms as i64)
    .bind(duration.map(|d| d as i64))
    .bind(finished)
    .fetch_one(state.registry.db())
    .await
    .map_err(internal)?;
    Ok(Json(json!({
        "position_ms": body.position_ms,
        "played": row.get::<i64, _>("played") != 0,
        "play_count": row.get::<i64, _>("play_count"),
    })))
}

/// Proxy one artifact of a dispatched session from its transcoder.
async fn transcode_file(
    state: &AppState,
    session: &std::sync::Arc<crate::sessions::Session>,
    file: &str,
) -> Result<Response, ApiError> {
    let valid = !file.starts_with('.')
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err((StatusCode::BAD_REQUEST, "invalid file name".into()));
    }
    let bytes = state
        .sessions
        .fetch_artifact(&state.registry, session, file)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
    let ctype = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file == "start.pos" {
        "text/plain"
    } else {
        "video/mp2t"
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, ctype)],
        axum::body::Bytes::from(bytes),
    )
        .into_response())
}

/// Serve session artifacts (playlist + segments): remux sessions from
/// local scratch, dispatched sessions via the transcoder proxy. Only
/// plain filenames are accepted — no separators, no dotfiles — so
/// traversal is impossible by construction.
async fn session_file(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such session".to_string()))?;
    session.touch();
    // Live subtitle tap (HUB-32): the remux pipeline — local or on a
    // transcoder — appends ASS events to subs-e{n}.ass from the session
    // origin. Follow the file's growth until the client leaves, the
    // session dies, or a seek-restart truncates it (the player then
    // re-opens against the new origin).
    if file.starts_with("subs-") && (file.ends_with(".ass") || file.ends_with(".jsonl")) {
        let valid = file[5..].chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
        if !valid {
            return Err((StatusCode::BAD_REQUEST, "invalid file name".into()));
        }
        let ctype = if file.ends_with(".ass") {
            "text/x-ssa; charset=utf-8"
        } else {
            "application/x-ndjson; charset=utf-8"
        };
        let sessions = state.sessions.clone();
        let registry = state.registry.clone();
        let sid = id.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(8);
        tokio::spawn(async move {
            let mut pos: usize = 0;
            let appear_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                // Re-resolve each cycle: seek-restarts swap the dir.
                let Some(session) = sessions.get(&sid) else { break };
                session.touch();
                let snapshot: Option<Vec<u8>> = match &session.mode {
                    crate::sessions::Mode::Remux { dir, .. } => {
                        tokio::fs::read(dir.join(&file)).await.ok()
                    }
                    crate::sessions::Mode::Transcode { .. } => {
                        sessions.fetch_artifact(&registry, &session, &file).await.ok()
                    }
                    crate::sessions::Mode::Direct { .. } => break,
                };
                match snapshot {
                    Some(bytes) => {
                        if bytes.len() < pos {
                            break; // truncated: new origin, player re-opens
                        }
                        if bytes.len() > pos {
                            let delta = axum::body::Bytes::copy_from_slice(&bytes[pos..]);
                            pos = bytes.len();
                            if tx.send(Ok(delta)).await.is_err() {
                                break; // client gone
                            }
                        }
                    }
                    None if std::time::Instant::now() < appear_deadline && pos == 0 => {}
                    None => break, // no ASS track tapped, or session dir gone
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });
        let body =
            axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
        return Ok(axum::response::Response::builder()
            .status(StatusCode::OK)
            .header("content-type", ctype)
            .header("cache-control", "no-store")
            .body(body)
            .unwrap());
    }
    let dir = match &session.mode {
        crate::sessions::Mode::Remux { dir, .. } => dir.clone(),
        crate::sessions::Mode::Transcode { .. } => {
            return transcode_file(&state, &session, &file).await;
        }
        crate::sessions::Mode::Direct { .. } => {
            return Err((StatusCode::NOT_FOUND, "not a remux session".into()));
        }
    };
    let dir = &dir;
    let valid = !file.starts_with('.')
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err((StatusCode::BAD_REQUEST, "invalid file name".into()));
    }
    let ctype = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file.ends_with(".ts") {
        "video/mp2t"
    } else if file == "start.pos" {
        // True playlist origin after keyframe snapping (§6): players
        // align subtitles and the seekbar to it.
        "text/plain"
    } else {
        return Err((StatusCode::NOT_FOUND, "unknown file type".into()));
    };
    let bytes = tokio::fs::read(dir.join(&file))
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "no such file".to_string()))?;
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", ctype)
        .header("cache-control", "no-store")
        .body(axum::body::Body::from(bytes))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn range_forms() {
        let size = 1000;
        assert_eq!(parse_range(None, size), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-499"), size), Ok(Some((0, 500))));
        assert_eq!(parse_range(Some("bytes=500-"), size), Ok(Some((500, 500))));
        assert_eq!(parse_range(Some("bytes=-200"), size), Ok(Some((800, 200))));
        // End clamped to size.
        assert_eq!(parse_range(Some("bytes=900-5000"), size), Ok(Some((900, 100))));
        // Suffix longer than the file → whole file.
        assert_eq!(parse_range(Some("bytes=-5000"), size), Ok(Some((0, 1000))));
        // Unsatisfiable / malformed.
        assert!(parse_range(Some("bytes=1000-"), size).is_err());
        assert!(parse_range(Some("bytes=5-2"), size).is_err());
        assert!(parse_range(Some("bytes=-"), size).is_err());
        assert!(parse_range(Some("bytes=0-1,5-9"), size).is_err());
        assert!(parse_range(Some("chunks=0-1"), size).is_err());
        assert!(parse_range(Some("bytes=-0"), size).is_err());
    }
}
