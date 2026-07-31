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
use serde_json::{Value, json};
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
    pub proxy_trust: Arc<crate::proxy::ProxyTrust>,
    pub metrics_token: Arc<Option<String>>,
}

/// OPS-8 knobs, both defaulting to "off" (same-origin, no proxies).
#[derive(Default, Clone)]
pub struct NetOptions {
    /// Shared so a reload can swap its contents under a running
    /// router (NFR-6) instead of rebuilding one.
    pub proxy_trust: Arc<crate::proxy::ProxyTrust>,
    /// CORS allowlist: exact origins, or a single "*" for any (no
    /// credentials either way — third-party clients use bearer tokens).
    pub cors_origins: Vec<String>,
    /// NFR-6 scrape credential. None = `/metrics` is not served.
    pub metrics_token: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn router(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<crate::sessions::Sessions>,
    enrollments: Arc<crate::enrollment_service::EnrollmentService>,
    subtitles: Arc<crate::subtitles::Subtitles>,
    artwork: Arc<crate::artwork::Artwork>,
    enricher: Arc<crate::enrich::Enricher>,
    net: NetOptions,
) -> Router {
    let cors = cors_layer(&net.cors_origins);
    let state = AppState {
        registry,
        auth,
        sessions,
        enrollments,
        subtitles,
        artwork,
        enricher,
        proxy_trust: net.proxy_trust,
        metrics_token: Arc::new(net.metrics_token),
    };
    let protected = Router::new()
        .route("/api/v1/collections", get(list_collections))
        .route("/api/v1/libraries", get(list_libraries))
        .route("/api/v1/items", get(list_items))
        .route("/api/v1/items/{id}", get(item_detail))
        .route("/api/v1/items/{id}/children", get(item_children))
        .route("/api/v1/items/{id}/artwork", get(item_artwork))
        .route("/api/v1/items/{id}/subtitles", get(item_subtitles))
        .route("/api/v1/items/{id}/subtitles/search", post(subtitle_search))
        .route(
            "/api/v1/items/{id}/subtitles/download",
            post(subtitle_download),
        )
        .route(
            "/api/v1/subtitles/{track_id}",
            axum::routing::delete(subtitle_delete),
        )
        .route("/api/v1/subtitles/{track_id}/ocr", post(subtitle_ocr))
        .route(
            "/api/v1/items/{id}/subtitles/{file}",
            get(item_subtitle_file),
        )
        .route("/api/v1/items/{id}/fonts", get(item_fonts))
        .route("/api/v1/items/{id}/fonts/{n}", get(item_font))
        .route("/api/v1/prefs", get(get_prefs).put(put_pref))
        .route("/api/v1/events", get(events))
        .route("/api/v1/playback/sessions", post(start_session))
        .route(
            "/api/v1/playback/sessions/{id}",
            axum::routing::delete(end_session),
        )
        .route("/api/v1/playback/sessions/{id}/stream", get(stream_session))
        .route(
            "/api/v1/playback/sessions/{id}/progress",
            post(post_progress),
        )
        .route("/api/v1/playback/sessions/{id}/seek", post(seek_session))
        .route("/api/v1/playback/sessions/{id}/{file}", get(session_file))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));
    let admin = Router::new()
        .route("/admin/v1/enrollments", get(admin_enrollments))
        .route("/admin/v1/enrollments/approve", post(admin_approve))
        .route("/admin/v1/satellites", get(admin_satellites))
        .route(
            "/admin/v1/satellites/{id}",
            axum::routing::delete(admin_delete_satellite),
        )
        .route(
            "/admin/v1/satellites/{id}/disabled",
            post(admin_set_disabled),
        )
        .route(
            "/admin/v1/libraries",
            get(admin_libraries).post(admin_create_library),
        )
        .route(
            "/admin/v1/libraries/{id}",
            axum::routing::delete(admin_delete_library),
        )
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
        .route(
            "/admin/v1/providers/chains/{media_type}",
            post(admin_set_chain),
        )
        .route("/admin/v1/providers/tmdb", post(admin_set_tmdb))
        .route("/admin/v1/providers/tvdb", post(admin_set_tvdb))
        .route("/admin/v1/providers/anidb", post(admin_set_anidb))
        .route("/admin/v1/providers/anidb/verify", post(admin_verify_anidb))
        .route(
            "/admin/v1/enrich",
            get(admin_enrich_status).post(admin_enrich_run),
        )
        .route(
            "/admin/v1/libraries/{id}/refresh",
            post(admin_refresh_library),
        )
        .route("/admin/v1/enrich/review", get(admin_review_list))
        .route("/admin/v1/enrich/search", post(admin_review_search))
        .route("/admin/v1/items/{id}/match", post(admin_apply_match))
        .route("/admin/v1/sessions", get(admin_sessions))
        .route(
            "/admin/v1/sessions/{id}",
            axum::routing::delete(admin_end_session),
        )
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ));
    let mut app = Router::new()
        .merge(admin)
        // NFR-6: public on purpose. It names modules and their state and
        // nothing else — a load balancer or uptime check must be able to
        // ask without holding a credential, and there is nothing here
        // that a failed login does not already reveal.
        .route("/health", get(health))
        // NFR-6: its own static credential, not a login token — see
        // `metrics`. Outside the admin group on purpose.
        .route("/metrics", get(metrics))
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/setup", post(setup))
        .route("/api/v1/auth/token", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .merge(protected)
        .with_state(state);
    if let Some(cors) = cors {
        app = app.layer(cors);
    }
    app.merge(crate::web::router())
}

/// OPS-8 CORS: absent config = no CORS headers (same-origin only, the
/// embedded web UI). "*" = any origin. Credentials stay off — cookies
/// don't cross origins here; third-party clients hold bearer tokens.
fn cors_layer(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    use tower_http::cors::{AllowOrigin, Any, CorsLayer};
    if origins.is_empty() {
        return None;
    }
    let origin = if origins.iter().any(|o| o == "*") {
        AllowOrigin::from(Any)
    } else {
        AllowOrigin::list(
            origins
                .iter()
                .filter_map(|o| o.parse::<axum::http::HeaderValue>().ok()),
        )
    };
    Some(
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(Any)
            .allow_headers(Any),
    )
}

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The token as a client presents it: Authorization header first, the
/// kahawai_token cookie as the fallback for <video>/HLS requests, which
/// cannot set headers (HUB-27). Shared with `bootstrap`, so what counts
/// as "signed in" cannot drift between the gate and what the gate says.
fn presented_token(req: &Request) -> Option<String> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    header.or_else(|| {
        req.headers()
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split(';')
                    .filter_map(|kv| kv.trim().split_once('='))
                    .find(|(k, _)| *k == "kahawai_token")
                    .map(|(_, v)| v.to_string())
            })
    })
}

/// Which screen the client should open on, stated rather than inferred.
///
/// Public by necessity, and deliberately so: every route behind
/// `require_auth` answers 503 before setup and 401 without a token, so a
/// client reading those is guessing its own state off an error path — and
/// pays whatever the endpoint it picked costs. The web UI probed
/// `/api/v1/items` and pulled the entire catalogue (1.4 MB, 578 ms here)
/// to read a status line it then threw away.
///
/// Says nothing a caller could not learn by trying to log in, so it needs
/// no token: `setup_required` is already printed on the console at
/// startup, and `authenticated` describes the request's OWN token.
/// NFR-6: Prometheus text exposition, behind its own static token.
///
/// NOT a login token. Access tokens live 15 minutes and no scraper
/// refreshes them, so an admin-token endpoint would serve one scrape and
/// then 401 forever. `hub.metrics_token` is a static credential scoped to
/// this one read-only route — no user, no session, no refresh.
///
/// Unset means NOT SERVED: 404, so a hub that was never configured for
/// scraping does not advertise an endpoint reporting its library size.
/// A configured-but-wrong token is 401, so an operator can tell "off
/// here" from "wrong secret".
async fn metrics(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let Some(expected) = state.metrics_token.as_deref() else {
        return Err((StatusCode::NOT_FOUND, "metrics are not enabled".into()));
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !ct_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "bad metrics token".into()));
    }
    let snap = crate::metrics::gather(&state.registry, &state.sessions, state.enricher.data_dir())
        .await
        .map_err(internal)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::render(&snap),
    )
        .into_response())
}

/// NFR-6: health for the hub and every module it knows.
///
/// 200 while the hub itself is serving, even when a satellite is away —
/// its collections go unavailable, nothing is lost (AR-6), and a check
/// that fails the whole server because one Pi is unplugged gets muted.
/// The body carries the detail, and `status` distinguishes the two.
async fn health(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let snap = crate::metrics::gather(&state.registry, &state.sessions, state.enricher.data_dir())
        .await
        .map_err(internal)?;
    Ok(Json(crate::metrics::health(&snap)))
}

async fn bootstrap(State(state): State<AppState>, req: Request) -> Json<Value> {
    let setup_required = state.auth.setup_required();
    Json(json!({
        "setup_required": setup_required,
        "authenticated": !setup_required
            && presented_token(&req).and_then(|t| state.auth.verify(&t).ok()).is_some(),
    }))
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
    let claims = presented_token(&req)
        .and_then(|t| state.auth.verify(&t).ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "invalid or missing token".to_string(),
        ))?;
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

/// Layered after require_auth: the Claims extension is already present.
/// Constant-time compare, so a wrong token cannot be discovered a byte at
/// a time. Length is not hidden and does not need to be.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
    let db = state.registry.db();
    let mut chains = serde_json::Map::new();
    for mt in crate::providers::MEDIA_TYPES {
        chains.insert(
            mt.to_string(),
            json!({
                "order": crate::providers::chain_in_force(db, mt).await,
                "default": crate::providers::chain_for(mt),
            }),
        );
    }
    Ok(Json(json!({
        "tmdb": { "configured": tmdb },
        "tvdb": { "configured": tvdb },
        "anidb": { "configured": anidb },
        "chains": chains,
    })))
}

#[derive(Deserialize)]
struct SetChain {
    order: Vec<String>,
}

/// HUB-5: reorder a media type's providers. Precedence is per field, so
/// this decides who wins where two providers both have an answer — and
/// it re-merges from stored answers, sending no provider a request.
async fn admin_set_chain(
    State(state): State<AppState>,
    Path(media_type): Path<String>,
    Json(body): Json<SetChain>,
) -> Result<Json<Value>, ApiError> {
    crate::providers::set_chain(state.registry.db(), &media_type, &body.order)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    state
        .registry
        .emit(json!({ "kind": "enrich", "chain": media_type }));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SubtitleSearchRequest {
    /// Preferred languages, ordered. Empty = whatever the provider has.
    #[serde(default)]
    languages: Vec<String>,
}

/// HUB-21/22: search external providers for this item's subtitles.
async fn subtitle_search(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<SubtitleSearchRequest>,
) -> Result<Json<Value>, ApiError> {
    let (candidates, quota) = state
        .subtitles
        .search_external(&state.registry, &id, body.languages, &claims.sub)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "candidates": candidates, "quota": quota })))
}

#[derive(Deserialize)]
struct SubtitleDownloadRequest {
    file_id: String,
    #[serde(default)]
    language: Option<String>,
}

/// HUB-24: user-initiated download; the result becomes a normal
/// subtitle track on the item.
async fn subtitle_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<SubtitleDownloadRequest>,
) -> Result<Json<Value>, ApiError> {
    let (track_id, quota) = state
        .subtitles
        .download_external(
            &state.registry,
            &id,
            &body.file_id,
            body.language,
            &claims.sub,
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "track_id": track_id, "quota": quota })))
}

/// HUB-32c: OCR an image subtitle track (embedded or VobSub sidecar)
/// into a text track. Synchronous — a feature film OCRs in ~30 s and
/// the caller is a human who pressed a button; the result is cached,
/// so it runs once per track. Feature-gated: without `ocr` the route
/// answers with what is missing rather than 404.
async fn subtitle_ocr(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    #[cfg(feature = "ocr")]
    {
        let new_id = state
            .subtitles
            .ocr_generate(&state.registry, track_id, &claims.sub)
            .await
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
        Ok(Json(json!({ "track_id": new_id })))
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = (claims, track_id);
        Err((
            StatusCode::NOT_IMPLEMENTED,
            "this build has no OCR support (compiled with --no-default-features)".into(),
        )
            .into())
    }
}

/// Remove a hub-stored (downloaded/OCR) track. Scan-owned tracks
/// refuse with 404-shaped `removed: false`.
async fn subtitle_delete(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let removed = state
        .subtitles
        .delete_track(&state.registry, track_id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "removed": removed })))
}

/// Re-validate the STORED AniDB credentials (no resend needed).
async fn admin_verify_anidb(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let user = state
        .registry
        .get_setting(crate::anidb::USER_SETTING)
        .await
        .map_err(internal)?;
    let pass = state
        .registry
        .get_setting(crate::anidb::PASS_SETTING)
        .await
        .map_err(internal)?;
    let key = state
        .registry
        .get_setting(crate::anidb::APIKEY_SETTING)
        .await
        .map_err(internal)?
        .filter(|k| !k.is_empty());
    let (Some(user), Some(pass)) = (user, pass) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "no AniDB account configured".into(),
        ));
    };
    match crate::anidb::Anidb::login(state.enricher.data_dir(), &user, &pass, key.as_deref()).await
    {
        Ok(client) => {
            client.finish().await;
            Ok(Json(json!({ "verified": true })))
        }
        Err(e) => Ok(Json(
            json!({ "verified": false, "error": format!("{e:#}") }),
        )),
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
        return Err((
            StatusCode::BAD_REQUEST,
            "username and password required".into(),
        ));
    }
    state
        .registry
        .set_setting(crate::anidb::USER_SETTING, user)
        .await
        .map_err(internal)?;
    state
        .registry
        .set_setting(crate::anidb::PASS_SETTING, pass)
        .await
        .map_err(internal)?;
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
    match crate::anidb::Anidb::login(state.enricher.data_dir(), user, pass, key.as_deref()).await {
        Ok(client) => {
            client.finish().await;
            let enricher = state.enricher.clone();
            let registry = state.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = enricher.run_once(&registry).await {
                    tracing::warn!(error = format!("{e:#}"), "enrichment run failed");
                }
            });
            Ok(Json(json!({ "saved": true, "verified": true })))
        }
        Err(e) => Ok(Json(
            json!({ "saved": true, "verified": false, "error": format!("{e:#}") }),
        )),
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

#[derive(serde::Deserialize, Default)]
struct RefreshQuery {
    deep: Option<bool>,
}

/// HUB-35: granular refresh. The admin-facing unit is the LIBRARY —
/// fan out collection-scoped scan requests to each member collection's
/// mediahost. There is deliberately no global rescan.
async fn admin_refresh_library(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<RefreshQuery>,
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
        // ?deep=true: re-probe every file, stat-unchanged or not — how
        // rows probed by an older binary pick up new stream facts.
        if q.deep.unwrap_or(false) {
            state.registry.mark_deep_rescan(&module_id, &collection_id);
        }
        if request_scan(&state, &module_id, &collection_id).await {
            asked += 1;
        } else {
            offline += 1;
        }
    }
    Ok(Json(json!({ "asked": asked, "offline": offline })))
}

/// Send one collection-scoped scan request (MH-2); the mediahost's
/// trigger sink coalesces with any running scan.
async fn request_scan(state: &AppState, module_id: &str, collection_id: &str) -> bool {
    if !state.registry.is_connected(module_id) {
        return false;
    }
    let msg = kahawai_proto::v1::HubToHost {
        msg: Some(kahawai_proto::v1::hub_to_host::Msg::RescanRequest(
            kahawai_proto::v1::RescanRequest {
                collection_id: collection_id.to_string(),
            },
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
         JOIN resolved_metadata m ON m.item_id = i.id
         -- Only what a human can act on: episodes and tracks inherit their
         -- parent's match and have no re-match affordance in the UI.
         WHERE m.confidence IN ('miss', 'weak', 'rejected')
           AND i.kind IN ('movie', 'show', 'album')
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
                "provider": r.try_get::<Option<String>, _>("provider").ok().flatten(),
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
    /// The item being matched — lets ranking favour the provider that
    /// owns its collection's identity space (anilist for anime).
    item: Option<String>,
}

async fn admin_review_search(
    State(state): State<AppState>,
    Json(body): Json<ReviewSearch>,
) -> Result<Json<Value>, ApiError> {
    let candidates = state
        .enricher
        .search_candidates(
            &state.registry,
            &body.kind,
            &body.query,
            body.year,
            body.item.as_deref(),
        )
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
            // Pin what is already assigned: automatic re-picking then leaves
            // it alone, whatever a later answer or a reorder says.
            crate::providers::confirm_assignment(db, &id)
                .await
                .map_err(internal)?;
        }
        "reject" => {
            // The refused records are remembered and the assignment goes;
            // the ANSWERS stay. Deleting them made the next run re-ask every
            // provider, AniDB included, for one click — and it is the
            // refused set, not their absence, that keeps the item
            // unassigned until a provider offers something new.
            crate::providers::reject_matches(db, &id)
                .await
                .map_err(internal)?;
        }
        "pick" => {
            let c = body
                .candidate
                .ok_or((StatusCode::BAD_REQUEST, "candidate required".into()))?;
            let provider = body
                .provider
                .ok_or((StatusCode::BAD_REQUEST, "provider required".into()))?;
            let pid = c["id"]
                .as_u64()
                .ok_or((StatusCode::BAD_REQUEST, "candidate.id required".into()))?;
            // A human's choice: stored as that provider's answer and pinned,
            // so automatic re-picking leaves it alone whatever lands later.
            crate::providers::assign_manual(
                db,
                &id,
                &provider,
                &pid.to_string(),
                crate::providers::Fields {
                    title: c["title"].as_str().map(str::to_string),
                    overview: c["overview"].as_str().map(str::to_string),
                    poster_path: c["poster_path"].as_str().map(str::to_string),
                    rating: c["vote_average"].as_f64(),
                    premiered: c["release_date"].as_str().map(str::to_string),
                    ..Default::default()
                },
            )
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
    Ok(Json(
        json!({ "id": id, "username": body.username, "admin": body.admin }),
    ))
}

async fn admin_satellites(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let sats = state
        .registry
        .satellites_overview()
        .await
        .map_err(internal)?;
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
    Ok(Json(
        json!({ "deleted": id, "removed": fingerprint, "sessions_ended": ended }),
    ))
}

#[derive(serde::Deserialize)]
struct SetDisabled {
    disabled: bool,
}

/// Admin drain toggle: placement skips a disabled satellite; running
/// sessions finish on their own.
async fn admin_libraries(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let libraries = state
        .registry
        .libraries_overview()
        .await
        .map_err(internal)?;
    Ok(Json(json!({ "libraries": libraries })))
}

async fn admin_collections(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let collections = state
        .registry
        .collections_overview()
        .await
        .map_err(internal)?;
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
    state
        .registry
        .set_disabled(&id, body.disabled)
        .await
        .map_err(internal)?;
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
        let username: Option<String> =
            sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
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
            "streams": s.verdict.lock().unwrap().as_ref().map(|(video, audio)| json!({
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

/// Source address for OPS-2 throttling: the socket peer (None in
/// in-process tests), or the X-Forwarded-For client when — and only
/// when — the peer is a configured trusted proxy (OPS-8).
struct ClientIp(Option<std::net::IpAddr>);

impl axum::extract::FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        let xff = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        Ok(ClientIp(state.proxy_trust.client_ip(peer, xff)))
    }
}

/// OPS-2 thresholds: consecutive failures before lockout. The per-IP
/// bar is higher so one shared NAT doesn't lock a household out.
const THROTTLE_USER_AFTER: u32 = 5;
const THROTTLE_IP_AFTER: u32 = 20;

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LoginRequest>,
) -> Result<Json<Value>, ApiError> {
    if state.auth.setup_required() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "setup required".into()));
    }
    let user_key = format!("u:{}", body.username.to_lowercase());
    let ip_key = ip.map(|i| format!("ip:{i}"));
    let locked = state.auth.throttle.locked(&user_key).or_else(|| {
        ip_key
            .as_deref()
            .and_then(|k| state.auth.throttle.locked(k))
    });
    if let Some(wait) = locked {
        tracing::warn!(username = %body.username, ip = ?ip, "login throttled");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many attempts; retry in {}s", wait.as_secs().max(1)),
        ));
    }
    match state.auth.login(&body.username, &body.password).await {
        Ok(tokens) => {
            state.auth.throttle.clear(&user_key);
            if let Some(k) = &ip_key {
                state.auth.throttle.clear(k);
            }
            Ok(Json(json!(tokens)))
        }
        Err(_) => {
            let lock = state.auth.throttle.fail(&user_key, THROTTLE_USER_AFTER);
            if let Some(k) = &ip_key {
                state.auth.throttle.fail(k, THROTTLE_IP_AFTER);
            }
            tracing::warn!(username = %body.username, ip = ?ip, locked = ?lock, "login failed");
            Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()))
        }
    }
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<Value>, ApiError> {
    let tokens = state.auth.refresh(&body.refresh_token).await.map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            "invalid refresh token".to_string(),
        )
    })?;
    Ok(Json(json!(tokens)))
}

#[derive(Deserialize)]
struct StartSessionRequest {
    item_id: String,
    /// Explicit mode = the pre-negotiation contract, verbatim (scripts,
    /// debugging). Absent = the hub negotiates from `profile` (HUB-14).
    #[serde(default)]
    mode: Option<String>,
    /// The client's capability profile; absent = conservative fallback.
    #[serde(default)]
    profile: Option<kahawai_core::media::CapabilityProfile>,
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
    /// Unified subtitle track id (subtitle unification). An IMAGE
    /// track pick forces its burn-in and pins the source it binds to;
    /// text picks are a no-op here (the client fetches them itself).
    #[serde(default)]
    subtitle_track: Option<i64>,
}

/// HUB-11 event channel: server-sent invalidation hints ({kind, ...}).
/// EventSource authenticates via the kahawai_token cookie (it cannot
/// set headers), same as <video>/HLS requests. Hints, not state —
/// clients refetch whatever a hint names.
async fn events(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio_stream::StreamExt;
    let rx = state.registry.subscribe_events();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|v| {
        v.ok()
            .map(|v| Ok::<_, std::convert::Infallible>(Event::default().data(v.to_string())))
    });
    // OPS-8: tell buffering proxies (nginx) to pass events through live.
    (
        axum::response::AppendHeaders([("x-accel-buffering", "no")]),
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
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
            &state.subtitles,
            &claims.sub,
            &body.item_id,
            body.mode.as_deref(),
            body.profile.clone(),
            body.start_ms,
            body.audio_track,
            body.video_track,
            body.subtitle_track,
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
            "streams": session.verdict.lock().unwrap().as_ref().map(|(video, audio)| json!({
                "video": video,
                "audio": audio,
                // Additive (HUB-32a/b): per-subtitle tier verdicts on
                // negotiated sessions; [] on explicit-mode sessions.
                "subtitles": *session.sub_verdicts.lock().unwrap(),
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
    /// Switch the burned subtitle mid-session (unified track id): an
    /// image track starts burning it, a text track withdraws an
    /// explicit burn. Absent = keep as is.
    #[serde(default)]
    subtitle_track: Option<i64>,
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
        .seek(
            &state.registry,
            &state.subtitles,
            &id,
            body.position_ms,
            body.audio_track,
            body.video_track,
            body.subtitle_track,
        )
        .await
        .map_err(|e| {
            // Every failed seek tells its story here, not just the ones
            // the fallback retried — a 409 must never be untraceable.
            tracing::warn!(session = %id, position_ms = body.position_ms,
                audio_track = ?body.audio_track, video_track = ?body.video_track,
                error = format!("{e:#}"), "seek failed");
            (StatusCode::CONFLICT, format!("{e:#}"))
        })?;
    // A track switch re-planned: hand back the verdicts of what plays
    // NOW so the overlay never lies about the current streams.
    let session = state.sessions.get(&id);
    let streams = session.as_ref().and_then(|s| {
        s.verdict.lock().unwrap().as_ref().map(|(video, audio)| {
            json!({ "video": video, "audio": audio,
                        "subtitles": *s.sub_verdicts.lock().unwrap() })
        })
    });
    Ok(Json(
        json!({ "part_base_ms": part_base_ms, "streams": streams }),
    ))
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
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok());

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

/// `?size=` names one of `artwork::SIZES`; anything else, including
/// nothing, serves the original.
#[derive(serde::Deserialize, Default)]
struct ArtworkQuery {
    size: Option<String>,
    /// Cache-buster the client appends; read only so it does not land in
    /// `size` by accident.
    #[allow(dead_code)]
    v: Option<String>,
}

async fn item_artwork(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ArtworkQuery>,
) -> Result<Response, ApiError> {
    match state
        .artwork
        .get_at(&state.registry, &state.sessions, &id, q.size.as_deref())
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

#[derive(Deserialize)]
struct SubtitleCaps {
    /// Capability bits feed each track's computed DELIVERY; absent =
    /// true, so a declaration-less client sees the richest reading.
    /// Nothing is filtered out any more — a track a client cannot
    /// render lists as `delivery: none` and the UI disables it.
    graphics_overlay: Option<bool>,
    ass_render: Option<bool>,
}

async fn item_subtitles(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(caps): Query<SubtitleCaps>,
) -> Result<Json<Value>, ApiError> {
    let subs = state
        .subtitles
        .list(
            &state.registry,
            &id,
            caps.ass_render.unwrap_or(true),
            caps.graphics_overlay.unwrap_or(true),
        )
        .await
        .map_err(internal)?;
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
    // The public keyspace is TRACK IDS ({id}.vtt / {id}.ass); the
    // resolver maps them onto the internal cache/pipeline notation.
    let resolve = |raw: &str| -> Option<i64> { raw.parse().ok() };
    if let Some(raw) = file.strip_suffix(".ass") {
        let track_id = resolve(raw)
            .ok_or_else(|| ApiError::from((StatusCode::BAD_REQUEST, "bad track id".to_string())))?;
        let track = state
            .subtitles
            .internal_key(&state.registry, track_id)
            .await
            .map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
        let key = track.internal_key();
        let body = state
            .subtitles
            .ass_body(&state.registry, &state.sessions, &id, &key)
            .await
            .map_err(internal)?;
        let headers = [
            (
                axum::http::header::CONTENT_TYPE,
                "text/x-ssa; charset=utf-8",
            ),
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
    let raw = file.strip_suffix(".vtt").unwrap_or(&file);
    let track_id = resolve(raw)
        .ok_or_else(|| ApiError::from((StatusCode::BAD_REQUEST, "bad track id".to_string())))?;
    let track = state
        .subtitles
        .internal_key(&state.registry, track_id)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
    let vtt = state
        .subtitles
        .vtt(
            &state.registry,
            &state.sessions,
            &id,
            &track.internal_key(),
            q.shift_ms.round() as i64,
        )
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
    let rows = sqlx::query("SELECT id, name, media_type FROM libraries ORDER BY name")
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
            })
        })
        .collect();
    Ok(Json(json!({ "libraries": libraries })))
}

#[derive(Deserialize)]
struct ItemsQuery {
    library: Option<String>,
    /// HUB-12 server-side search: a substring of the title, folded the
    /// same way titles are stored so accents and case do not matter.
    q: Option<String>,
    /// `title` (default), `year`, `added`. Prefixed with `-` for
    /// descending: `-year`.
    sort: Option<String>,
    /// NFR-1: a page, not the catalogue. Absent = the default page size,
    /// never "everything" — that is the shape that took 13 s over 250k
    /// items and shipped 100 MB.
    limit: Option<u32>,
    offset: Option<u32>,
}

/// Page sizes. The cap exists so a client cannot ask for the old
/// behaviour by accident; the default is a screenful of cards with room
/// to scroll past the fold.
const ITEMS_PAGE_DEFAULT: u32 = 200;
const ITEMS_PAGE_MAX: u32 = 1000;

/// `sort` → an ORDER BY the query can interpolate. Never the raw
/// parameter: this is the one place a browse request touches SQL text.
/// The tiebreakers use i.year, not the resolved one, on purpose. Every
/// view field named in an ORDER BY is a correlated subquery run for every
/// candidate row BEFORE the LIMIT applies — naming two instead of one
/// took the browse query from 19 ms to 80 ms at 39k items. A tiebreaker
/// only decides identical titles, where the stored year is as good.
///
/// Used for the search/unscoped candidate scan (via [`items_order_c`])
/// and for re-ordering the joined page. Deliberately NOT ending in a
/// unique column: rows that tie come out in the index's own rowid order,
/// stable between consecutive pages while the plan stays an index scan.
/// Appending `i.id` forced a temp b-tree and 96 ms → 912 ms deep pages.
/// The membership orders below DO end in a unique column, because there
/// `item_id` is inside the covering index and costs nothing.
fn items_order(sort: Option<&str>) -> &'static str {
    match sort.unwrap_or("title") {
        "year" => "i.year IS NULL, i.year, i.sort_title",
        "-year" => "i.year IS NULL, i.year DESC, i.sort_title",
        // Item ids are ULIDs, which sort lexicographically by the time
        // they were minted — so "recently added" needs no column, cannot
        // disagree with one, and is already total on its own.
        "added" => "i.id, i.sort_title",
        "-added" => "i.id DESC, i.sort_title",
        "-title" => "i.sort_title DESC, i.year",
        _ => "i.sort_title, i.year",
    }
}

/// [`items_order`] for the inner candidate scan, whose alias is `c` so it
/// cannot collide with the outer join's `i`.
fn items_order_c(sort: Option<&str>) -> String {
    items_order(sort).replace("i.", "c.")
}

/// ORDER BY pairs for a library page driven from the membership table:
/// the inner scan of `item_libraries_browse` (0040) and the outer
/// re-order of the joined page.
///
/// Every inner order ends in `item_id`, which is IN the covering index,
/// so the order is total for free — a tie cannot straddle a page
/// boundary differently on two requests. `-title` runs the whole index
/// backwards (year descends within a tied title, where it used to
/// ascend): a uniform direction is what keeps a deep reverse page a
/// plain backward scan instead of a temp sort.
fn membership_order(sort: Option<&str>) -> (&'static str, &'static str) {
    match sort.unwrap_or("title") {
        "year" => (
            "year IS NULL, year, sort_title, item_id",
            "i.year IS NULL, i.year, i.sort_title, i.id",
        ),
        "-year" => (
            "year IS NULL, year DESC, sort_title, item_id",
            "i.year IS NULL, i.year DESC, i.sort_title, i.id",
        ),
        "added" => ("item_id", "i.id"),
        "-added" => ("item_id DESC", "i.id DESC"),
        "-title" => (
            "sort_title DESC, year DESC, item_id DESC",
            "i.sort_title DESC, i.year DESC, i.id DESC",
        ),
        _ => ("sort_title, year, item_id", "i.sort_title, i.year, i.id"),
    }
}

/// The total for a library with no search term — the overwhelmingly
/// common browse.
///
/// `item_libraries` IS the answer: it is keyed `(library_id, item_id)`,
/// so counting a library is one key-range scan. Counting `items` and
/// probing membership per row instead cost 47 ms at 50k and 523 ms at
/// 250k, against 3.4 ms and 15.5 ms here — on every request, including
/// the first page.
///
/// No `kind` filter, because membership only ever holds TOP-LEVEL items:
/// the 0036/0039 triggers project `COALESCE(parent_id, id)`, so an
/// episode's source lands its show. Re-checking the invariant with a
/// join costs more than the query it protects (73 ms / 594 ms — worse
/// than what it replaced), so it is pinned by a test instead:
/// `library_membership_holds_only_top_level_items` in tests/sort_title.rs.
const COUNT_IN_LIBRARY: &str = "SELECT COUNT(*) FROM item_libraries WHERE library_id = ?1";

/// The columns a browse row carries, resolved for the ≤200 rows of ONE
/// page — never for a candidate. See [`item_page_sql`].
const ITEM_PAGE_COLS: &str = "\
i.id, i.kind, i.season, i.episode, i.artist,
COALESCE(md.title, i.title) AS title,
COALESCE(i.year, CAST(substr(md.premiered, 1, 4) AS INTEGER)) AS year,
i.title AS file_title, i.year AS file_year,
md.title AS matched_title,
md.confidence AS match_confidence,
md.updated_at AS art_version,
(SELECT COUNT(*) FROM item_sources s WHERE s.item_id = i.id) AS sources,
i.parent_id,
(SELECT p.sort_title FROM items p WHERE p.id = i.parent_id) AS parent_title,
w.position_ms, w.duration_ms, w.played, w.play_count";

/// Wrap an id-producing inner query in the joins that dress a page.
///
/// Every branch of the browse pages this way — a deferred join. The
/// inner query decides WHICH ≤200 items make the page using only indexed
/// scalar columns; the resolved-metadata view, the watch state and the
/// source count are joined onto those ids afterwards. Joining first and
/// paging second resolves the view for every candidate the sort visits,
/// which is the 912 ms failure mode that keeps re-appearing whenever an
/// ORDER BY stops matching an index.
///
/// The outer ORDER BY re-sorts only the returned page: the inner query
/// already chose and ordered the ids, the join just does not promise to
/// preserve that order.
fn item_page_sql(inner: &str, order_out: &str) -> String {
    format!(
        "SELECT {ITEM_PAGE_COLS}
           FROM ({inner}) page
           JOIN items i ON i.id = page.item_id
           LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?1
           LEFT JOIN resolved_metadata md ON md.item_id = i.id
          ORDER BY {order_out}"
    )
}

async fn list_items(
    State(state): State<AppState>,
    Query(q): Query<ItemsQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(ITEMS_PAGE_DEFAULT).min(ITEMS_PAGE_MAX);
    let offset = q.offset.unwrap_or(0);
    // Folded once here, matched against norm_title (already folded) and
    // the resolved title, so a search finds an item by what it is called
    // now as well as by its filename.
    let needle =
        q.q.as_deref()
            .map(crate::enrich::fold)
            .filter(|s| !s.is_empty());
    let db = state.registry.db();

    // Three explicit shapes rather than one query with
    // `(?N IS NULL OR ...)` guards: a guard is opaque at plan time, which
    // is the pattern that has cost us an index twice now.
    let (rows, total) = match (&q.library, &needle) {
        // A library, no search — the overwhelmingly common browse. The
        // page comes off `item_libraries_browse` (0040) alone: membership
        // and sort keys live in one covering index, so a deep page skips
        // rows at one index step each instead of probing `items` per
        // skipped row. That probe chain is what made the last page of a
        // 250k library cost 1.2 s; this shape measures 21 ms there.
        (Some(library), None) => {
            let (order_in, order_out) = membership_order(q.sort.as_deref());
            let sql = item_page_sql(
                &format!(
                    "SELECT item_id FROM item_libraries WHERE library_id = ?2
                      ORDER BY {order_in} LIMIT ?3 OFFSET ?4"
                ),
                order_out,
            );
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&claims.sub)
                .bind(library)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await
                .map_err(internal)?;
            let total: i64 = sqlx::query_scalar(COUNT_IN_LIBRARY)
                .bind(library)
                .fetch_one(db)
                .await
                .map_err(internal)?;
            (rows, total)
        }
        // Searching. The title predicate has to look at candidates, so
        // the scan follows the sort index and streams: an underfull page
        // means the scan ran out, and the total is known without a
        // second pass. Only a FULL page pays the counting scan.
        //
        // What is searchable: titles by their folded filename and their
        // resolved title, albums additionally by folded artist, and
        // EPISODES by their resolved titles — sort_title is parent-aware
        // since 0041, so an episode's is the title its show's assigned
        // provider gave it. Episodes belong to a library through their
        // parent, hence the COALESCE in the membership probe. Tracks
        // stay out deliberately: matching "iron maiden" should offer the
        // albums, not five hundred track rows above them.
        (library, Some(needle)) => {
            let member = match library {
                Some(_) => {
                    "AND EXISTS (SELECT 1 FROM item_libraries il
                                  WHERE il.library_id = ?2
                                    AND il.item_id = COALESCE(c.parent_id, c.id))"
                }
                None => "",
            };
            let order_c = items_order_c(q.sort.as_deref());
            let sql = item_page_sql(
                &format!(
                    // +c.kind: degraded on purpose. As a plain term the
                    // 5-value IN steers the planner onto items_kind_title
                    // and every candidate pays a random table probe for
                    // its LIKE columns — a search predicate this dense
                    // (LIKE over most rows) wants the sequential scan.
                    "SELECT c.id AS item_id FROM items c
                      WHERE +c.kind IN ('movie', 'show', 'album', 'episode', 'track') {member}
                        AND (c.norm_title LIKE '%' || ?3 || '%'
                             OR c.sort_title LIKE '%' || ?3 || '%'
                             -- Artist matches ALBUMS only. A track row for
                             -- every song by the artist would bury the
                             -- albums; titles are how songs are found.
                             OR (c.kind = 'album' AND c.norm_artist LIKE '%' || ?3 || '%'))
                      ORDER BY {order_c} LIMIT ?4 OFFSET ?5"
                ),
                items_order(q.sort.as_deref()),
            );
            // ?2 must exist even without a library, so numbering is
            // uniform; it is simply never referenced then.
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&claims.sub)
                .bind(library.as_deref().unwrap_or(""))
                .bind(needle)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await
                .map_err(internal)?;
            let total: i64 = if rows.len() < limit as usize && !(rows.is_empty() && offset > 0) {
                // The page underfilled: the scan saw everything.
                offset as i64 + rows.len() as i64
            } else {
                let count = format!(
                    // Same +c.kind degrade as the page query above.
                    "SELECT COUNT(*) FROM items c
                      WHERE +c.kind IN ('movie', 'show', 'album', 'episode', 'track') {member}
                        AND (c.norm_title LIKE '%' || ?3 || '%'
                             OR c.sort_title LIKE '%' || ?3 || '%'
                             OR (c.kind = 'album' AND c.norm_artist LIKE '%' || ?3 || '%'))"
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(count))
                    .bind("") // ?1 unused here; keeps the numbering shared
                    .bind(library.as_deref().unwrap_or(""))
                    .bind(needle)
                    .fetch_one(db)
                    .await
                    .map_err(internal)?
            };
            (rows, total)
        }
        // Unscoped, no search: everything, in sort order.
        (None, None) => {
            let order_c = items_order_c(q.sort.as_deref());
            let sql = item_page_sql(
                &format!(
                    "SELECT c.id AS item_id FROM items c
                      WHERE c.kind NOT IN ('episode', 'track')
                      ORDER BY {order_c} LIMIT ?2 OFFSET ?3"
                ),
                items_order(q.sort.as_deref()),
            );
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&claims.sub)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await
                .map_err(internal)?;
            let total: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM items WHERE kind NOT IN ('episode', 'track')",
            )
            .fetch_one(db)
            .await
            .map_err(internal)?;
            (rows, total)
        }
    };
    let items: Vec<Value> = rows.iter().map(item_row_json).collect();
    Ok(Json(json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

fn item_row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "kind": r.get::<String, _>("kind"),
        "title": r.get::<String, _>("title"),
        "artist": r.try_get::<Option<String>, _>("artist").ok().flatten(),
        "match_confidence": r.try_get::<Option<String>, _>("match_confidence").ok().flatten(),
        // Artwork is cached hard by the browser (a day), so the URL has
        // to change when the metadata does — otherwise re-matching an
        // item leaves yesterday's poster on the card.
        "art_version": r.try_get::<Option<i64>, _>("art_version").ok().flatten(),
        "premiered": r.try_get::<Option<String>, _>("premiered").ok().flatten(),
        "file_title": r.try_get::<Option<String>, _>("file_title").ok().flatten(),
        "file_year": r.try_get::<Option<i64>, _>("file_year").ok().flatten(),
        "matched_title": r.try_get::<Option<String>, _>("matched_title").ok().flatten(),
        "year": r.get::<Option<i64>, _>("year"),
        "season": r.get::<Option<i64>, _>("season"),
        "episode": r.get::<Option<i64>, _>("episode"),
        // Batch span (0045): the file covers episode..episode_end.
        "episode_end": r.try_get::<Option<i64>, _>("episode_end").ok().flatten(),
        // The show an episode belongs to, so a search hit called "Pilot"
        // says which of its eight namesakes it is. The id lets a track
        // hit open its ALBUM — tracks have no detail view of their own.
        "parent_id": r.try_get::<Option<String>, _>("parent_id").ok().flatten(),
        "parent_title": r.try_get::<Option<String>, _>("parent_title").ok().flatten(),
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
        "SELECT i.id, i.kind, i.year, i.season, i.episode, i.episode_end, i.artist,
                COALESCE(md.title, i.title) AS title,
                md.premiered AS premiered,
                md.updated_at AS art_version,
                md.proj_season, md.proj_episode,
                COUNT(s.item_id) AS sources,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN item_sources s ON s.item_id = i.id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
         LEFT JOIN resolved_metadata md ON md.item_id = i.id
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
                md.updated_at AS art_version,
                COALESCE(pmd.title, p.title) AS show_title,
                (SELECT COUNT(*) FROM item_sources s WHERE s.item_id = i.id) AS sources,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN items p ON p.id = i.parent_id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
         LEFT JOIN resolved_metadata md ON md.item_id = i.id
         LEFT JOIN resolved_metadata pmd ON pmd.item_id = p.id
         WHERE i.id = ?",
    )
    .bind(&claims.sub)
    .bind(&id)
    .fetch_optional(state.registry.db())
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such item".to_string()))?;

    let sources = sqlx::query(
        "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json,
                COALESCE(f.revision, 1) AS revision
         FROM item_sources s
         JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                       = (s.module_id, s.collection_id, s.path_rel)
         WHERE s.item_id = ?
         -- Same order playback picks in (sessions::source_parts), so the
         -- list a user reads is the preference the player acts on.
         ORDER BY COALESCE(json_extract(f.streams_json, '$.video[0].height'), 0) DESC,
                  COALESCE(f.revision, 1) DESC,
                  f.size DESC",
    )
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;

    let sources: Vec<Value> = sources
        .iter()
        .map(|r| {
            let module_id: String = r.get("module_id");
            let streams: Value = serde_json::from_str(r.get::<String, _>("streams_json").as_str())
                .unwrap_or(Value::Null);
            json!({
                "module_id": module_id,
                "collection_id": r.get::<String, _>("collection_id"),
                "path_rel": r.get::<String, _>("path_rel"),
                "size": r.get::<i64, _>("size"),
                "available": state.registry.is_connected(&module_id),
                "revision": r.get::<i64, _>("revision"),
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
        "SELECT m.overview, m.rating, m.premiered, m.confidence, m.provider,
                -- An episode carries neither; both describe the work, so
                -- they come from the show when the episode has none.
                COALESCE(NULLIF(m.genres, ''), NULLIF(pm.genres, '')) AS genres,
                COALESCE(NULLIF(m.cast_json, ''), NULLIF(pm.cast_json, '')) AS cast_json,
                COALESCE(NULLIF(m.original_language, ''),
                         NULLIF(pm.original_language, '')) AS original_language
         FROM items i
         JOIN resolved_metadata m ON m.item_id IN (i.id, i.parent_id)
         LEFT JOIN resolved_metadata pm ON pm.item_id = i.parent_id
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
            "provider": m.try_get::<Option<String>, _>("provider").ok().flatten(),
            "original_language": m
                .get::<Option<String>, _>("original_language")
                .filter(|l| !l.is_empty()),
            // Stored as JSON; hand them out as arrays rather than making
            // every client parse a string out of a field (HUB-6).
            "genres": m
                .get::<Option<String>, _>("genres")
                .and_then(|g| serde_json::from_str::<Value>(&g).ok()),
            "cast": m
                .get::<Option<String>, _>("cast_json")
                .and_then(|c| serde_json::from_str::<Value>(&c).ok()),
        });
    }

    // Anime relations (HUB-29): watchable related entries, resolved to
    // in-library items where the target exists here.
    let related = sqlx::query(
        "SELECT r.kind, r.target_title, r.target_anilist, m2.item_id AS local_id
         FROM item_relations r
         LEFT JOIN anime_ids m2 ON m2.anilist_id = r.target_anilist
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
    state
        .sessions
        .viewer_position(&state.registry, &id, body.position_ms);

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
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
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
        let valid = file[5..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.');
        if !valid {
            return Err((StatusCode::BAD_REQUEST, "invalid file name".into()));
        }
        // The public keyspace is the track id; the pipeline writes
        // internal stream-index names (subs-e{n}.*). Translate here —
        // only embedded tracks are in the pipeline, so only they tap.
        let file = match file[5..].split_once('.') {
            Some((num, ext)) if num.chars().all(|c| c.is_ascii_digit()) => {
                let track = crate::tracks::get(state.registry.db(), num.parse().unwrap())
                    .await
                    .map_err(internal)?
                    .filter(|t| t.origin == "embedded")
                    .ok_or((StatusCode::NOT_FOUND, "no such embedded track".to_string()))?;
                format!("subs-{}.{ext}", track.internal_key())
            }
            _ => file.clone(),
        };
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
                let Some(session) = sessions.get(&sid) else {
                    break;
                };
                session.touch();
                let snapshot: Option<Vec<u8>> = match &session.mode {
                    crate::sessions::Mode::Remux { dir, .. } => {
                        tokio::fs::read(dir.join(&file)).await.ok()
                    }
                    crate::sessions::Mode::Transcode { .. } => sessions
                        .fetch_artifact(&registry, &session, &file)
                        .await
                        .ok(),
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
        let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
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
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
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
        assert_eq!(
            parse_range(Some("bytes=900-5000"), size),
            Ok(Some((900, 100)))
        );
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
