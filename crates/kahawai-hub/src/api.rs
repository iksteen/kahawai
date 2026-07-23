//! Client API (HUB-11/12 first cut): setup + token auth, then browse —
//! collections, items, item detail with full technical stream info.
//! During setup mode (OPS-1) nothing but /setup is reachable.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
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
}

pub fn router(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<crate::sessions::Sessions>,
    enrollments: Arc<crate::enrollment_service::EnrollmentService>,
) -> Router {
    let state = AppState { registry, auth, sessions, enrollments };
    let protected = Router::new()
        .route("/api/v1/collections", get(list_collections))
        .route("/api/v1/items", get(list_items))
        .route("/api/v1/items/{id}", get(item_detail))
        .route("/api/v1/playback/sessions", post(start_session))
        .route("/api/v1/playback/sessions/{id}", axum::routing::delete(end_session))
        .route("/api/v1/playback/sessions/{id}/stream", get(stream_session))
        .route("/api/v1/playback/sessions/{id}/progress", post(post_progress))
        .route("/api/v1/playback/sessions/{id}/{file}", get(session_file))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));
    let admin = Router::new()
        .route("/admin/v1/enrollments", get(admin_enrollments))
        .route("/admin/v1/enrollments/approve", post(admin_approve))
        .route("/admin/v1/satellites", get(admin_satellites))
        .route("/admin/v1/satellites/{id}", axum::routing::delete(admin_delete_satellite))
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
}

fn default_mode() -> String {
    "direct".into()
}

async fn start_session(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    Json(body): Json<StartSessionRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let session = state
        .sessions
        .start(&state.registry, &claims.sub, &body.item_id, &body.mode)
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
    };
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "session_id": session.id,
            "mode": mode,
            "size": session.size,
            "content_type": ctype,
            "stream_url": stream_url,
            "streams": session.verdict.as_ref().map(|(video, audio)| json!({
                "video": video,
                "audio": audio,
            })),
        })),
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

async fn list_items(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        "SELECT i.id, i.title, i.year, COUNT(s.item_id) AS sources,
                w.position_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN item_sources s ON s.item_id = i.id
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
         GROUP BY i.id ORDER BY i.title, i.year",
    )
    .bind(&claims.sub)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let items: Vec<Value> = rows.iter().map(item_row_json).collect();
    Ok(Json(json!({ "items": items })))
}

fn item_row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "title": r.get::<String, _>("title"),
        "year": r.get::<Option<i64>, _>("year"),
        "sources": r.get::<i64, _>("sources"),
        "resume_position_ms": r.get::<Option<i64>, _>("position_ms"),
        "played": r.get::<Option<i64>, _>("played").unwrap_or(0) != 0,
        "play_count": r.get::<Option<i64>, _>("play_count").unwrap_or(0),
    })
}

async fn item_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<Value>, ApiError> {
    let item = sqlx::query(
        "SELECT i.id, i.title, i.year,
                (SELECT COUNT(*) FROM item_sources s WHERE s.item_id = i.id) AS sources,
                w.position_ms, w.played, w.play_count
         FROM items i
         LEFT JOIN watch_state w ON w.item_id = i.id AND w.user_id = ?
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
    out["sources"] = json!(sources);
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

/// Serve remux artifacts (playlist + segments) from the session's scratch
/// dir. Only plain filenames are accepted — no separators, no dotfiles —
/// so traversal is impossible by construction.
async fn session_file(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let session = state
        .sessions
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "no such session".to_string()))?;
    session.touch();
    let crate::sessions::Mode::Remux { dir, .. } = &session.mode else {
        return Err((StatusCode::NOT_FOUND, "not a remux session".into()));
    };
    let valid = !file.starts_with('.')
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err((StatusCode::BAD_REQUEST, "invalid file name".into()));
    }
    let ctype = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file.ends_with(".ts") {
        "video/mp2t"
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
