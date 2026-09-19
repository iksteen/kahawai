//! Client API (HUB-11/12 first cut): setup + token auth, then browse —
//! collections, items, item detail with full technical stream info.
//! During setup mode (OPS-1) the public router is locked; initial-admin
//! creation exists only on the separately bound trusted-local router.
//!
//! ## Watch-state meaning
//!
//! Catalogue user-state semantics live in [`crate::watch`]. Legacy playback
//! retains boolean completion and resume positions, without a seen counter.

mod catalogue;
mod copies;
mod enrichment;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::auth::{Auth, CompleteSetupError};
use crate::registry::Registry;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicOrigin {
    value: String,
    secure: bool,
}

impl PublicOrigin {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(value)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "must be an absolute HTTP(S) origin without credentials, path, query, or fragment"
        );
        Ok(Self {
            value: url.origin().ascii_serialization(),
            secure: url.scheme() == "https",
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn secure(&self) -> bool {
        self.secure
    }
}

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
    pub setup_url: Arc<Option<String>>,
    pub public_origin: Option<PublicOrigin>,
}

/// The bearer Prometheus scrapes `/metrics` with, kept beside `jwt.secret`
/// and `credentials.secret` in the data directory rather than in the config
/// file. Named here because the composition root reads it and `backup` has to
/// carry it, and two spellings of one file name is one too many.
pub const METRICS_TOKEN_FILE: &str = "metrics.secret";

/// Network and feature knobs, defaulting to what a bare hub ships with.
#[derive(Clone, Default)]
pub struct NetOptions {
    /// Shared so a reload can swap its contents under a running
    /// router (NFR-6) instead of rebuilding one.
    pub proxy_trust: Arc<crate::proxy::ProxyTrust>,
    /// CORS allowlist: exact origins, or a single "*" for any (no
    /// credentials either way — third-party clients use bearer tokens).
    pub cors_origins: Vec<String>,
    /// NFR-6 scrape credential. None = `/metrics` is not served.
    pub metrics_token: Option<String>,
    /// Trusted-local first-run URL advertised while the public API is locked.
    pub setup_url: Option<String>,
    /// Configured canonical browser origin; absent disables Origin validation.
    pub public_origin: Option<PublicOrigin>,
    /// `--web-dir`: serve `/app/` from this directory instead of the bundle
    /// embedded at build time. None = embedded, which is what a release ships.
    pub web_dir: Option<std::path::PathBuf>,
}

#[derive(OpenApi)]
#[openapi(
    version = "3.2.0",
    paths(
        catalogue::feeds::catalogue_continue, catalogue::feeds::catalogue_up_next,
        catalogue::collections, catalogue::libraries, catalogue::create_library, catalogue::refresh_library,
        catalogue::set_collections, catalogue::delete_library, catalogue::items, catalogue::item, catalogue::playback::catalogue_playback, catalogue::playback::catalogue_next, catalogue::playback::session_fonts, catalogue::playback::session_font, catalogue::playback::session_subtitle, catalogue::catalogue_children, catalogue::catalogue_set_watched, catalogue::catalogue_artists, catalogue::catalogue_artwork, catalogue::catalogue_artist_artwork,
        health,
        metrics,
        bootstrap,
        setup,
        login,
        refresh,
        logout,
        events,
        get_prefs,
        put_pref,
        catalogue::subtitles::catalogue_subtitle_search,
        catalogue::subtitles::catalogue_subtitle_download,
        catalogue::subtitles::catalogue_subtitle_delete,
        account_opensubtitles,
        set_account_opensubtitles,
        delete_account_opensubtitles,
        start_session,
        end_session,
        post_progress,
        seek_session,
        stream_session,
        session_file,
        admin_enrollments,
        admin_approve,
        admin_satellites,
        admin_delete_satellite,
        admin_set_disabled,
        admin_users,
        catalogue::set_user_libraries,
        admin_create_user,
        admin_delete_user,
        admin_set_user_admin,
        admin_providers,
        admin_set_chain,
        admin_set_tmdb,
        admin_set_fanart,
        admin_set_theaudiodb,
        admin_set_tvdb,
        admin_set_anidb,
        admin_disconnect_provider,
        admin_verify_anidb,
        admin_enrich_status,
        admin_enrich_run,
        enrichment::enrichment_items,
        enrichment::enrichment_detail,
        enrichment::enrichment_correct,
        enrichment::enrichment_search,
        enrichment::enrichment_progress,
        enrichment::enrichment_artwork,
        enrichment::enrichment_identities,
        enrichment::enrichment_artist_artwork,
        admin_sessions,
        admin_end_session,
        admin_session_log,
        admin_item_log,
        admin_segments_status
    ),
    modifiers(&BearerSecurity)
)]
struct ApiDoc;

struct BearerSecurity;

impl Modify for BearerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{
            ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme,
        };

        openapi
            .components
            .as_mut()
            .expect("the generated API document has components")
            .add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
        let components = openapi
            .components
            .as_mut()
            .expect("the generated API document has components");
        components.add_security_scheme(
            "media_token",
            SecurityScheme::ApiKey(ApiKey::Query(ApiKeyValue::new("token"))),
        );
        components.add_security_scheme(
            "candidate_artwork_ticket",
            SecurityScheme::ApiKey(ApiKey::Query(ApiKeyValue::new("ticket"))),
        );
        components.add_security_scheme(
            "metrics_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("Static metrics token")
                    .build(),
            ),
        );
    }
}

/// Build the exact OpenAPI document served by the hub.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    let mut openapi = ApiDoc::openapi();
    // utoipa models QUERY but its path macro still requires the POST arm.
    let item = openapi
        .paths
        .paths
        .get_mut("/api/v1/catalogue/libraries/{id}/items/{item_id}")
        .expect("catalogue QUERY path is generated");
    item.query = item.post.take();
    for item in openapi.paths.paths.values_mut() {
        for operation in [
            item.get.as_mut(),
            item.put.as_mut(),
            item.post.as_mut(),
            item.delete.as_mut(),
            item.options.as_mut(),
            item.head.as_mut(),
            item.patch.as_mut(),
            item.trace.as_mut(),
            item.query.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            document_request_id_header(operation);
        }
        for operation in item.additional_operations.values_mut() {
            document_request_id_header(operation);
        }
    }
    openapi
}

/// The request-context layer puts this header on successes and refusals alike.
/// Add it centrally for the same reason the runtime behavior is central: an
/// operation added later must not have to remember correlation separately.
fn document_request_id_header(operation: &mut utoipa::openapi::path::Operation) {
    let mut header = utoipa::openapi::header::Header::default();
    header.description = Some(
        "Server-generated ULID for correlating this response with hub logs; an inbound value is never reused"
            .into(),
    );
    for response in operation.responses.responses.values_mut() {
        let utoipa::openapi::RefOr::T(response) = response else {
            panic!("response references must carry X-Request-Id on their component")
        };
        response
            .headers
            .insert("X-Request-Id".into(), header.clone().into());
    }
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
    sessions.attach_registry(registry.clone());
    enricher.attach_sessions(sessions.clone());
    enricher.attach_artwork(&artwork);
    enricher.start_catalogue(registry.clone());
    let cors = cors_layer(&net.cors_origins);
    let web_dir = net.web_dir;
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
        setup_url: Arc::new(net.setup_url),
        public_origin: net.public_origin,
    };
    let bearer = Router::new()
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/prefs", get(get_prefs).put(put_pref))
        .route(
            "/api/v1/account/opensubtitles",
            get(account_opensubtitles)
                .post(set_account_opensubtitles)
                .delete(delete_account_opensubtitles),
        );

    let bearer = bearer.route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        require_bearer,
    ));
    let mut admin = Router::new()
        .route("/admin/v1/enrollments", get(admin_enrollments))
        .route("/admin/v1/sessions", get(admin_sessions))
        .route(
            "/admin/v1/sessions/{id}",
            axum::routing::delete(admin_end_session),
        )
        .route("/admin/v1/sessions/{id}/log", get(admin_session_log))
        .route("/admin/v1/items/{id}/log", get(admin_item_log))
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
        .route("/admin/v1/users", get(admin_users).post(admin_create_user))
        .route(
            "/admin/v1/users/{id}",
            axum::routing::delete(admin_delete_user),
        )
        .route(
            "/admin/v1/users/{id}/libraries",
            axum::routing::put(catalogue::set_user_libraries),
        )
        .route(
            "/admin/v1/users/{id}/admin",
            axum::routing::put(admin_set_user_admin),
        );
    admin = admin
        .merge(enrichment::routes())
        .route("/admin/v1/segments", get(admin_segments_status))
        .route("/admin/v1/providers", get(admin_providers))
        .route(
            "/admin/v1/providers/chains/{media_type}",
            post(admin_set_chain),
        )
        .route("/admin/v1/providers/tmdb", post(admin_set_tmdb))
        .route("/admin/v1/providers/tvdb", post(admin_set_tvdb))
        .route("/admin/v1/providers/anidb", post(admin_set_anidb))
        .route("/admin/v1/providers/anidb/verify", post(admin_verify_anidb))
        .route("/admin/v1/providers/fanart", post(admin_set_fanart))
        .route("/admin/v1/providers/theaudiodb", post(admin_set_theaudiodb))
        .route(
            "/admin/v1/providers/{provider}/credentials",
            axum::routing::delete(admin_disconnect_provider),
        )
        .route(
            "/admin/v1/enrich",
            get(admin_enrich_status).post(admin_enrich_run),
        );

    let admin = admin
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let events = Router::new()
        .route("/api/v1/events", get(events))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
        ));
    let mut app = Router::new()
        .merge(events)
        .merge(bearer)
        .merge(admin)
        .merge(catalogue::playback_routes(&state))
        .merge(catalogue::routes(&state))
        .merge(enrichment::artwork_routes(&state))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/auth/token", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .with_state(state)
        .merge(Router::from(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi_document()),
        ))
        .merge(crate::web::router(web_dir))
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method);
    if let Some(cors) = cors {
        app = app.layer(cors);
    }
    app.layer(axum::middleware::from_fn(crate::error::request_context))
}

async fn unknown_route() -> ApiError {
    ApiError::new(ErrorCode::NotFound, "no such route")
}

async fn wrong_method() -> ApiError {
    ApiError::new(
        ErrorCode::MethodNotAllowed,
        "that method is not allowed here",
    )
}

#[derive(Clone)]
struct SetupState {
    auth: Arc<Auth>,
}

/// First-admin browser flow on a dedicated loopback listener. Keeping this a
/// separate router makes accidental publication impossible: the public router
/// has no setup mutation to protect with a header or source-address check.
pub fn setup_router(auth: Arc<Auth>, web_dir: Option<std::path::PathBuf>) -> Router {
    let state = SetupState { auth };
    Router::new()
        .route("/api/v1/bootstrap", get(setup_bootstrap))
        .route("/api/v1/setup", post(setup))
        .with_state(state)
        .merge(crate::web::router(web_dir))
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(axum::middleware::from_fn(crate::error::request_context))
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
            .allow_headers(Any)
            .expose_headers([axum::http::HeaderName::from_static("x-request-id")]),
    )
}

use crate::error::{ApiError, ApiErrorBody, ApiJson, ApiPath, ApiQuery, ErrorCode};

#[derive(Serialize, ToSchema)]
struct BootstrapResponse {
    setup_required: bool,
    setup_available: bool,
    #[schema(required)]
    setup_url: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct PendingEnrollment {
    csr_fingerprint: String,
    module_type: String,
    module_id: String,
    name: String,
}

#[derive(Serialize, ToSchema)]
struct EnrollmentsResponse {
    pending: Vec<PendingEnrollment>,
}

#[derive(Serialize, ToSchema)]
struct ApprovedResponse {
    approved: String,
}

/// The credential store. `None` is unreachable in production — only a test
/// registry is built without one — so this is a 500 rather than a state the
/// API has to describe.
fn store(registry: &Registry) -> Result<&crate::secrets::Credentials, ApiError> {
    registry
        .credentials()
        .ok_or_else(|| internal(anyhow::anyhow!("no credential store")))
}

#[derive(Serialize, ToSchema)]
struct ProviderConfiguration {
    configured: bool,
}

#[derive(Serialize, ToSchema)]
struct TheAudioDbConfiguration {
    /// The free public key is always active; this only reports an override.
    premium_key_configured: bool,
}

#[derive(Serialize, ToSchema)]
struct ProviderChain {
    order: Vec<String>,
    default: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct ProvidersResponse {
    tmdb: ProviderConfiguration,
    tvdb: ProviderConfiguration,
    anidb: ProviderConfiguration,
    fanart: ProviderConfiguration,
    theaudiodb: TheAudioDbConfiguration,
    chains: std::collections::BTreeMap<String, ProviderChain>,
    #[serde(default)]
    available: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize, ToSchema)]
struct SavedResponse {
    saved: bool,
}

#[derive(Serialize, ToSchema)]
struct SubtitleSearchResponse {
    candidates: Vec<crate::opensubtitles::Candidate>,
    quota: crate::opensubtitles::Quota,
}

#[derive(Serialize, ToSchema)]
struct SubtitleDownloadResponse {
    track_id: i64,
    quota: crate::opensubtitles::Quota,
}

#[derive(Serialize, ToSchema)]
struct RemovedResponse {
    removed: bool,
}

#[derive(Serialize, ToSchema)]
struct VerificationResponse {
    verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct SavedVerificationResponse {
    saved: bool,
    verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct StartedResponse {
    started: bool,
}

#[derive(Serialize, ToSchema)]
struct UsersResponse {
    users: Vec<crate::grants::UserAccess>,
}

#[derive(Serialize, ToSchema)]
struct UserAccessResponse {
    id: String,
    all_libraries: bool,
    libraries: Vec<String>,
    /// The version this write produced, so a panel that edits twice without
    /// reloading sends the right one the second time.
    grants_version: i64,
}

#[derive(Serialize, ToSchema)]
struct UserAdminResponse {
    id: String,
    is_admin: bool,
}

#[derive(Serialize, ToSchema)]
struct CreatedUserResponse {
    id: String,
    username: String,
    admin: bool,
}

#[derive(Serialize, ToSchema)]
struct DeletedUserResponse {
    deleted: String,
    username: String,
    sessions_ended: usize,
}

#[derive(Serialize, ToSchema)]
struct SatellitesResponse {
    satellites: Vec<crate::registry::SatelliteOverview>,
}

#[derive(Serialize, ToSchema)]
struct DeletedSatelliteResponse {
    deleted: String,
    removed: String,
    sessions_ended: usize,
    subtitle_payloads_removed: usize,
}

#[derive(Serialize, ToSchema)]
struct SessionStreamSummary {
    #[schema(required)]
    cost: Option<&'static str>,
    video: String,
    audio: String,
}

#[derive(Serialize, ToSchema)]
struct AdminSession {
    session_id: String,
    #[schema(required)]
    username: Option<String>,
    #[schema(required)]
    title: Option<String>,
    mode: &'static str,
    module_id: String,
    idle_secs: u64,
    #[schema(required)]
    streams: Option<SessionStreamSummary>,
}

#[derive(Serialize, ToSchema)]
struct AdminSessionsResponse {
    sessions: Vec<AdminSession>,
}

#[derive(Serialize, ToSchema)]
struct BrowserTokenResponse {
    access_token: String,
    expires_in: i64,
}

#[allow(dead_code)]
#[derive(ToSchema)]
#[serde(untagged)]
enum AuthSuccessResponse {
    Api(crate::auth::TokenPair),
    Browser(BrowserTokenResponse),
}

#[derive(Serialize, ToSchema)]
struct Preference {
    scope: String,
    key: String,
    value: String,
}

#[derive(Serialize, ToSchema)]
struct PreferencesResponse {
    prefs: Vec<Preference>,
}

#[derive(Serialize, ToSchema)]
struct PlaybackStreams {
    #[schema(required)]
    cost: Option<&'static str>,
    video: String,
    audio: String,
    subtitles: Vec<kahawai_media::negotiate::SubtitleVerdict>,
}

#[derive(Serialize, ToSchema)]
struct StartSessionResponse {
    /// Skip markers on this session's captured physical timeline.
    segments: Vec<crate::segments::Segment>,
    media_entry_id: Option<String>,
    session_id: String,
    /// Start accepted for this physical version. Clients must use this offset.
    effective_start_ms: u64,
    source_fingerprint: String,
    source_id: i64,
    replay_gain: Option<kahawai_core::media::ReplayGain>,
    library_item_ids: Vec<String>,
    mode: &'static str,
    size: u64,
    #[schema(required)]
    duration_ms: Option<u64>,
    part_base_ms: u64,
    parts: usize,
    content_type: String,
    stream_url: String,
    #[schema(required)]
    streams: Option<PlaybackStreams>,
    /// The unified track list with `delivery` computed against THIS
    /// session's effective profile and the source it negotiated. The
    /// item QUERY's listing reflects the profile at page load: after a
    /// capability-masked restart the two disagree, and a client reading
    /// the stale one kept rendering ASS client-side while asking the
    /// hub for a burn. The session is the authority on what it serves.
    subtitle_listing: Vec<crate::subtitles::TrackListing>,
}

#[derive(Serialize, ToSchema)]
struct SeekResponse {
    part_base_ms: u64,
    #[schema(required)]
    streams: Option<PlaybackStreams>,
}

#[derive(Serialize, ToSchema)]
struct FontsResponse {
    fonts: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct SegmentCollectionStatus {
    collection_id: String,
    mediahost_id: String,
    mediahost_name: String,
    name: String,
    connected: bool,
    media_type: String,
    /// Last reported source count; absence is unknown, never zero.
    pending_sources: Option<u64>,
    enabled: Option<bool>,
    /// Sources still awaiting a loudness result; absence means no report yet.
    pending_loudness: Option<u64>,
}

#[derive(Serialize, ToSchema)]
struct SegmentStatusResponse {
    collections: Vec<SegmentCollectionStatus>,
}

#[derive(Serialize, ToSchema)]
struct ArtistSummary {
    /// Opaque, URL-safe synthetic identity used by artist navigation.
    key: String,
    /// Stored Album Artist spelling used for display.
    name: String,
    album_count: i64,
}

#[derive(Serialize, ToSchema)]
struct ProgressResponse {
    position_ms: u64,
    played: bool,
}

#[derive(Serialize, ToSchema)]
struct WatchUpdate {
    item_id: String,
    position_ms: u64,
    played: bool,
}

#[derive(Serialize, ToSchema)]
struct UpdatedResponse {
    updated: Vec<WatchUpdate>,
}

/// A refusal, unless the cause is the database — in which case it is ours.
///
/// These call sites map a whole `anyhow::Error` onto one client code, and the
/// producers mix "you asked for something that is not there" with "the write
/// failed". Answering `not_found` to an admin whose delete hit a locked
/// database tells them to stop asking about a satellite that is still there.
/// Both halves are the hub's fault and both are 500s.
///
/// Three causes are tested for, and the last two were added because leaving
/// them out misfiled a real failure. `sqlx::Error` covers the database.
/// `std::io::Error` and `serde_json::Error` cover the producers that WRITE:
/// the subtitle download finishes by creating its cache directory,
/// serialising the record and writing the file, so a full or unwritable disk
/// answered "the subtitle provider did not answer" on a 502 — after the
/// viewer's download entitlement had already been spent on a fetch that
/// worked.
///
/// It is a floor, not a taxonomy, and its premise is narrow enough to state:
/// **the producer must express its client-visible refusals as plain `anyhow`
/// and its faults as one of those three types.** Where that does not hold it
/// inverts, in both directions, and it did at four sites — a producer whose
/// only failure is `io` (so the refusal arm is unreachable and everything is a
/// 500), a producer that says "no such item" with `Option::context` behind a
/// code meaning "upstream is down", and two fallback arms where every
/// remaining error is `sqlx` and the refusal they named was dead.
///
/// A producer with more than one refusal worth telling apart wants typed
/// errors, the way `sessions::SessionCap` and `opensubtitles::QuotaSpent` do.
fn refusal_or_internal(code: ErrorCode, message: &'static str, e: anyhow::Error) -> ApiError {
    let ours = e.downcast_ref::<sqlx::Error>().is_some()
        || e.downcast_ref::<std::io::Error>().is_some()
        || e.downcast_ref::<serde_json::Error>().is_some()
        // A credential that will not open is a tampered row or the wrong key,
        // both of them here. Without this the subtitle search answers "the
        // provider did not answer", which sends a viewer to blame
        // OpenSubtitles and an operator's alerting to file our fault as an
        // upstream outage — the inversion this helper exists to prevent.
        || e.downcast_ref::<crate::secrets::UnreadableCredential>()
            .is_some();
    if ours {
        return internal(e);
    }
    ApiError::log(code, message, e)
}

/// Whether a failure is a UNIQUE constraint firing.
///
/// The one database error that is about the REQUEST rather than the hub's
/// health — nothing is unwell, two rows just cannot both have that name. It
/// matters because `refusal_or_internal`'s premise is the opposite one, and a
/// producer that expresses "already taken" by letting a constraint fire
/// inverts it: the first cut of that helper turned a duplicate library name
/// into a 500 with nothing an admin could act on.
///
/// `is_unique_violation`, not a string match on the message. The driver
/// already knows, and a client-visible decision made by reading English is
/// exactly what this module replaces.
pub fn is_unique_violation(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(db)) if db.is_unique_violation()
    )
}

/// The subtitle provider refused. Its entitlement running out is not an
/// outage, and it is the common case — the anonymous budget is five downloads
/// a day — so it gets a code and keeps the sentence the provider module wrote,
/// which names the way out. Everything else is upstream being upstream.
fn subtitle_provider_refusal(e: anyhow::Error) -> ApiError {
    if e.downcast_ref::<crate::subtitles::NoSuchItem>().is_some() {
        // Before the provider is blamed for it. An unknown id arrived as 502
        // "the subtitle provider did not answer", so a typo reported an outage
        // that was not happening — and this route's own 404 was unreachable
        // for an admin, whose grant check always passes.
        return ApiError::new(ErrorCode::NotFound, "no such item");
    }
    match e.downcast_ref::<crate::opensubtitles::QuotaSpent>() {
        Some(spent) => ApiError::new(ErrorCode::SubtitleQuotaSpent, spent.to_string()),
        // `refusal_or_internal`, not `log`: a SQLITE_BUSY inside the search
        // is the hub's, and answering "the provider did not answer" on a 502
        // sends a viewer to blame OpenSubtitles while an operator's alerting
        // files our fault as an upstream one.
        None => refusal_or_internal(
            ErrorCode::ProviderError,
            "the subtitle provider did not answer",
            e,
        ),
    }
}

/// A failure that is OURS. The detail goes to the log and a fixed sentence
/// goes to the client: a 500's cause is by definition something the caller
/// cannot act on, and it is the one that carries scratch paths, worker argv
/// and a subprocess's stderr. `item_artwork` has answered this way since
/// SEC-WEB-7; this makes it the rule.
fn internal(e: impl std::fmt::Display) -> ApiError {
    // `{e:#}` and not `{e}`: with the client getting a fixed sentence, this
    // line is the only place the cause exists at all, and `Display` on an
    // anyhow error prints one layer. The context somebody attached upstream —
    // `no subtitle {key} on this item` over a sqlx error — was being dropped
    // from the only record of it.
    tracing::error!(error = format!("{e:#}"), "request failed");
    ApiError::new(
        ErrorCode::Internal,
        "the hub could not complete this request",
    )
}

/// Session absence is deliberately indistinguishable from denied ownership.
/// Keep races after the ownership boundary and administrative misses on the
/// same non-oracular response too.
fn session_gone() -> ApiError {
    hidden("session")
}

fn cookie<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .filter_map(|entry| entry.trim().split_once('='))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
}

/// An Authorization header always wins, including when malformed or invalid.
fn request_token(req: &Request, allow_media_cookie: bool) -> Result<Option<&str>, ()> {
    if let Some(value) = req.headers().get(axum::http::header::AUTHORIZATION) {
        return value
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty())
            .map(Some)
            .ok_or(());
    }
    Ok(allow_media_cookie
        .then(|| cookie(req.headers(), "kahawai_media"))
        .flatten())
}

/// Prometheus metrics exposition
///
/// Returns Prometheus text exposition for the hub. Requires a static metrics
/// token as a bearer credential; returns 404 when no metrics token is
/// configured and 401 when the token does not match.
// The token is deliberately NOT a login credential: access tokens live
// 15 minutes and no scraper refreshes them, so an admin-token endpoint
// would serve one scrape and 401 for ever. Unset means 404 rather than
// 401, so a hub never configured for scraping does not advertise that
// it has a library to measure.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "Observability",
    security(("metrics_token" = [])),
    responses(
        (status = 200, description = "Prometheus exposition", body = String, content_type = "text/plain; version=0.0.4; charset=utf-8"),
        (status = 401, description = "Wrong metrics token", body = ApiErrorBody),
        (status = 404, description = "Metrics are not enabled", body = ApiErrorBody),
        (status = 500, description = "Snapshot failed", body = ApiErrorBody)
    )
)]
async fn metrics(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let Some(expected) = state.metrics_token.as_deref() else {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            "metrics are not enabled",
        ));
    };
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !ct_eq(presented.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            ErrorCode::Unauthenticated,
            "bad metrics token",
        ));
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

/// Hub and satellite health
///
/// Returns health for the hub and every module it knows. Answers 200 while
/// the hub itself is serving even if a satellite is unreachable; the body's
/// status field carries that detail.
#[utoipa::path(
    get,
    path = "/health",
    tag = "Observability",
    responses(
        (status = 200, description = "Hub and satellite health", body = crate::metrics::HealthResponse),
        (status = 500, description = "Snapshot failed", body = ApiErrorBody)
    )
)]
async fn health(
    State(state): State<AppState>,
) -> Result<Json<crate::metrics::HealthResponse>, ApiError> {
    let snap = crate::metrics::gather(&state.registry, &state.sessions, state.enricher.data_dir())
        .await
        .map_err(internal)?;
    Ok(Json(crate::metrics::health(&snap)))
}

/// Client startup state
///
/// Returns the startup state a client should open on, including whether
/// first-time setup is required and the setup URL. Unauthenticated and always
/// available, so clients need not probe authenticated routes.
// Public on purpose, and safe to keep public: it states only what a
// caller learns by attempting to log in, and the alternative is every
// client inferring its own state from 401/503 error paths.
#[utoipa::path(
    get,
    path = "/api/v1/bootstrap",
    tag = "Authentication",
    responses((status = 200, description = "Client startup state", body = BootstrapResponse))
)]
async fn bootstrap(State(state): State<AppState>) -> Json<BootstrapResponse> {
    let setup_required = state.auth.setup_required();
    Json(BootstrapResponse {
        setup_required,
        setup_available: false,
        setup_url: if setup_required {
            state.setup_url.as_ref().clone()
        } else {
            None
        },
    })
}

async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    require_auth(state, req, next, false).await
}

async fn require_bearer_or_media(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    require_auth(state, req, next, true).await
}

async fn require_auth(
    state: AppState,
    mut req: Request,
    next: Next,
    allow_media_cookie: bool,
) -> Result<Response, ApiError> {
    if state.auth.setup_required() {
        tracing::warn!(path = %req.uri(), "503: setup_required returned true");
        return Err(ApiError::new(ErrorCode::SetupRequired, "setup required"));
    }
    let token = request_token(&req, allow_media_cookie)
        .ok()
        .flatten()
        .ok_or(ApiError::new(
            ErrorCode::Unauthenticated,
            "invalid or missing token",
        ))?;
    let claims = state.auth.authenticate(token).await.map_err(|_| {
        ApiError::new(
            ErrorCode::Unauthenticated,
            "invalid or missing token".to_string(),
        )
    })?;
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

/// AUTH-11: one ownership check for every user-facing session resource.
///
/// Missing and foreign ids deliberately have the same 404 response. A caller
/// cannot use stream/control/artifact routes to discover another user's live
/// session ids. A session disappearing after this check gets the same response.
async fn require_session_owner(
    State(state): State<AppState>,
    ApiPath(params): ApiPath<std::collections::HashMap<String, String>>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let id = params.get("id").map(String::as_str).unwrap_or_default();
    let session = state
        .sessions
        .get(id)
        .filter(|session| session.user_id == claims.sub)
        .ok_or_else(|| hidden("session"))?;
    {
        let captured = &session.catalogue;
        let granted =
            crate::grants::can_see_library(state.registry.db(), &claims, &captured.library_id)
                .await
                .map_err(internal)?;
        let member = if let Some(part) = session.parts.first() {
            state
                .registry
                .catalogue()
                .library_contains_source(&captured.library_id, &part.module_id, &part.collection_id)
                .await
                .map_err(internal)?
        } else {
            false
        };
        if !granted || !member {
            state.sessions.end(id).await;
            return Err(hidden("session"));
        }
    }
    Ok(next.run(req).await)
}

/// Layered after require_auth: the Claims extension is already present.
/// Constant-time compare, so a wrong token cannot be discovered a byte at
/// a time. Length is not hidden and does not need to be.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HUB-10: a denial the caller cannot tell from absence. See the
/// `grants` module doc for why this is never 403.
fn hidden(what: &str) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("no such {what}"))
}

/// Layered after require_auth: the Claims extension is already present.
async fn require_admin(req: Request, next: Next) -> Result<Response, ApiError> {
    let is_admin = req
        .extensions()
        .get::<crate::auth::Claims>()
        .is_some_and(|c| c.admin);
    if !is_admin {
        return Err(ApiError::new(ErrorCode::AdminRequired, "admin only"));
    }
    Ok(next.run(req).await)
}

/// List pending satellite enrollments
///
/// Admin only. Lists satellite enrollments awaiting approval, with each
/// entry's CSR fingerprint, module type, module id and name. Returns 503
/// before the hub has an administrator.
#[utoipa::path(
    get, path = "/admin/v1/enrollments", tag = "Admin",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = EnrollmentsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_enrollments(State(state): State<AppState>) -> Json<EnrollmentsResponse> {
    let pending = state
        .enrollments
        .pending()
        .iter()
        .map(|pending| PendingEnrollment {
            csr_fingerprint: pending.csr_fingerprint.clone(),
            module_type: pending.module_type.clone(),
            module_id: pending.module_id.clone(),
            name: pending.name.clone(),
        })
        .collect();
    Json(EnrollmentsResponse { pending })
}

#[derive(Deserialize, ToSchema)]
struct ApproveRequest {
    code: String,
}

/// Approve a pending satellite enrollment
///
/// Admin only. Approves a pending enrollment by its code, signing the
/// satellite's certificate and recording the satellite. Returns 404 when no
/// pending enrollment matches the code.
#[utoipa::path(
    post, path = "/admin/v1/enrollments/approve", tag = "Admin",
    security(("bearer_auth" = [])),
    request_body = ApproveRequest,
    responses(
        (status = 200, body = ApprovedResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, description = "Not an admin (require_admin); the handler itself no longer answers this", body = ApiErrorBody),
        (status = 404, description = "No pending enrollment matches that code", body = ApiErrorBody),
        (status = 500, description = "Signing the certificate or recording the satellite failed", body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_approve(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ApproveRequest>,
) -> Result<Json<ApprovedResponse>, ApiError> {
    let summary = state.enrollments.approve(&body.code).await.map_err(|e| {
        match e.downcast_ref::<crate::enrollment::EnrollError>() {
            // The only failure here that is about the REQUEST. Signing the CSR
            // and recording the satellite are the hub's own work, and
            // answering FORBIDDEN for those told an admin whose CA failed to
            // sign that they were not allowed to approve — the one code that
            // means "a different account might".
            Some(crate::enrollment::EnrollError::NoMatch) => ApiError::new(
                ErrorCode::NotFound,
                "no pending enrollment matches that code; if only one was \
                 waiting it has been dropped as a possible substitution (§7.2) \
                 and the satellite must enroll again",
            ),
            _ => internal(e),
        }
    })?;
    Ok(Json(ApprovedResponse { approved: summary }))
}

/// Metadata provider configuration
///
/// Admin only. Reports credential state for metadata and artwork providers,
/// plus the effective and default provider chain for each media type.
#[utoipa::path(
    get, path = "/admin/v1/providers", tag = "Admin providers",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = ProvidersResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_providers(
    State(state): State<AppState>,
) -> Result<Json<ProvidersResponse>, ApiError> {
    let tmdb = crate::enrich::tmdb_key(&state.registry)
        .await
        .map_err(internal)?
        .is_some();
    let tvdb = crate::enrich::tvdb_creds(&state.registry)
        .await
        .map_err(internal)?
        .is_some();
    let anidb = state
        .registry
        .hub_credential(crate::anidb::ANIDB)
        .await
        .map_err(internal)?;
    let anidb = anidb
        .get(crate::anidb::USERNAME)
        .is_some_and(|value| !value.is_empty())
        && anidb
            .get(crate::anidb::PASSWORD)
            .is_some_and(|value| !value.is_empty());
    let fanart = crate::enrich::fanart_client_key(&state.registry)
        .await
        .map_err(internal)?
        .is_some();
    let theaudiodb = crate::enrich::theaudiodb_premium_key(&state.registry)
        .await
        .map_err(internal)?
        .is_some();
    let mut chains = std::collections::BTreeMap::new();
    for media_type in crate::providers::MEDIA_TYPES {
        chains.insert(
            media_type.to_string(),
            ProviderChain {
                order: {
                    state
                        .registry
                        .catalogue()
                        .provider_order(
                            kahawai_mediadb::MediaType::parse(media_type).map_err(internal)?,
                        )
                        .await
                        .map_err(internal)?
                },
                default: if media_type == "anime" {
                    vec![
                        "anidb".into(),
                        "anilist".into(),
                        "tmdb".into(),
                        "tvdb".into(),
                    ]
                } else {
                    crate::providers::chain_for(media_type)
                        .iter()
                        .map(|p| (*p).to_owned())
                        .collect()
                },
            },
        );
    }
    Ok(Json(ProvidersResponse {
        tmdb: ProviderConfiguration { configured: tmdb },
        tvdb: ProviderConfiguration { configured: tvdb },
        anidb: ProviderConfiguration { configured: anidb },
        fanart: ProviderConfiguration { configured: fanart },
        theaudiodb: TheAudioDbConfiguration {
            premium_key_configured: theaudiodb,
        },
        available: [
            Some("local"),
            Some("anilist"),
            Some("musicbrainz"),
            Some("theaudiodb"),
            tmdb.then_some("tmdb"),
            tvdb.then_some("tvdb"),
            anidb.then_some("anidb"),
            fanart.then_some("fanart"),
        ]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect(),
        chains,
    }))
}

#[derive(Deserialize, ToSchema)]
struct SetChain {
    order: Vec<String>,
}

/// Set provider order for a media type
///
/// Admin only. Sets the provider precedence order for one media type and
/// re-merges metadata from stored answers without contacting any provider.
/// The order must be a permutation of that media type's providers or the call
/// returns 400.
#[utoipa::path(
    post, path = "/admin/v1/providers/chains/{media_type}", tag = "Admin providers",
    security(("bearer_auth" = [])),
    params(("media_type" = String, Path)),
    request_body = SetChain,
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_chain(
    State(state): State<AppState>,
    ApiPath(media_type): ApiPath<String>,
    ApiJson(body): ApiJson<SetChain>,
) -> Result<Json<OkResponse>, ApiError> {
    {
        let kind = kahawai_mediadb::MediaType::parse(&media_type)
            .map_err(|_| ApiError::new(ErrorCode::BadRequest, "unknown media type"))?;
        let mut expected = state
            .registry
            .catalogue()
            .provider_order(kind)
            .await
            .map_err(internal)?;
        let mut actual = body.order.clone();
        expected.sort();
        actual.sort();
        if expected != actual {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "order must contain each provider exactly once",
            ));
        }
        state
            .registry
            .catalogue()
            .set_provider_order(kind, &body.order)
            .await
            .map_err(internal)?;
        Ok(Json(OkResponse { ok: true }))
    }
}

fn same_fields(current: &BTreeMap<String, String>, proposed: &BTreeMap<&str, &str>) -> bool {
    current.len() == proposed.len()
        && proposed
            .iter()
            .all(|(field, value)| current.get(*field).is_some_and(|held| held == value))
}

/// Verify stored AniDB credentials
///
/// Admin only. Re-validates the AniDB credentials already stored on the hub
/// by logging in, without resending them. A failed login returns 200 with
/// verified false and an error message, while 503 means no AniDB account is
/// configured.
#[utoipa::path(
    post, path = "/admin/v1/providers/anidb/verify", tag = "Admin providers",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = VerificationResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        // Both codes, in one entry. OpenAPI has one response per status and
        // utoipa keeps the LAST of two — so declaring them separately did not
        // document both, it silently dropped the one this route actually
        // returns, leaving a client to read "the hub has no administrator yet"
        // for a missing AniDB account.
        (status = 503, description = "No AniDB credentials on this deployment (`provider_unconfigured`), or the hub has no administrator yet (`setup_required`)", body = ApiErrorBody)
    )
)]
async fn admin_verify_anidb(
    State(state): State<AppState>,
) -> Result<Json<VerificationResponse>, ApiError> {
    let (mut account, lease) = state
        .enricher
        .credential_snapshot(&state.registry, crate::anidb::ANIDB)
        .await
        .map_err(internal)?;
    let user = account.remove(crate::anidb::USERNAME);
    let pass = account.remove(crate::anidb::PASSWORD);
    let key = account
        .remove(crate::anidb::UDP_API_KEY)
        .filter(|k| !k.is_empty());
    let (Some(user), Some(pass)) = (user, pass) else {
        return Err(ApiError::new(
            ErrorCode::ProviderUnconfigured,
            "no AniDB account configured",
        ));
    };
    match crate::anidb::Anidb::login_current(
        state.enricher.data_dir(),
        &user,
        &pass,
        key.as_deref(),
        lease,
    )
    .await
    {
        Ok(client) => {
            client.finish().await;
            Ok(Json(VerificationResponse {
                verified: true,
                error: None,
            }))
        }
        // The chain, deliberately, and the one place it still goes out. This
        // route exists to tell an admin why a credential they just typed did
        // not work, and the chain IS that answer — it is a log line delivered
        // to the person who would otherwise have to go and read the log. It
        // is admin-only, it is a 200 rather than a refusal, and the account it
        // describes is the one they are holding.
        Err(error) => Ok(Json(VerificationResponse {
            verified: false,
            error: Some(format!("{error:#}")),
        })),
    }
}

#[derive(Deserialize, ToSchema)]
struct SetAnidb {
    username: String,
    password: String,
    #[serde(default)]
    udp_api_key: Option<String>,
}

/// Set AniDB credentials
///
/// Admin only. Stores the AniDB username, password and optional UDP API key,
/// then attempts a login. A failed login still returns 200 with saved true
/// and verified false plus the error; a successful one starts an enrichment
/// run.
#[utoipa::path(
    post, path = "/admin/v1/providers/anidb", tag = "Admin providers",
    security(("bearer_auth" = [])),
    request_body = SetAnidb,
    responses(
        (status = 200, body = SavedVerificationResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_anidb(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetAnidb>,
) -> Result<Json<SavedVerificationResponse>, ApiError> {
    // Stored as sent. What is inside a password or a UDP key is the account
    // holder's business, and the key is a cipher input: a trimmed one is a
    // different key, which AniDB answers by refusing to decrypt.
    let (user, pass) = (body.username.as_str(), body.password.as_str());
    if user.is_empty() || pass.is_empty() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "username and password required",
        ));
    }
    let change = state.enricher.changing_credentials().await;
    // Read before the write below replaces it: whether this is the same
    // account decides whether the stored session is still ours to use.
    // An unreadable old value is unknown, so overwrite it and invalidate
    // anything that might still hold its plaintext.
    let held = match state.registry.hub_credential(crate::anidb::ANIDB).await {
        Ok(fields) => Some(fields),
        Err(error) if error.is::<crate::secrets::UnreadableCredential>() => None,
        Err(error) => return Err(internal(error)),
    };
    let same_account = held.as_ref().is_some_and(|held| {
        held.get(crate::anidb::USERNAME).map(String::as_str) == Some(user)
            && held.get(crate::anidb::PASSWORD).map(String::as_str) == Some(pass)
    });
    // No key = a plaintext session, which works; an absent row says that
    // without an empty one having to mean it.
    let key = body.udp_api_key.as_deref().filter(|k| !k.is_empty());
    let mut fields = std::collections::BTreeMap::from([
        (crate::anidb::USERNAME, user),
        (crate::anidb::PASSWORD, pass),
    ]);
    if let Some(key) = key {
        fields.insert(crate::anidb::UDP_API_KEY, key);
    }
    let changed = held
        .as_ref()
        .is_none_or(|current| !same_fields(current, &fields));
    store(&state.registry)?
        .set_provider(crate::secrets::HUB, crate::anidb::ANIDB, &fields)
        .await
        .map_err(internal)?;
    // Invalidate copied credentials only when the whole plaintext provider
    // set changed. A UDP-key-only change keeps the durable session, but the
    // held client still carries the old cipher and must go stale.
    if changed {
        state.enricher.revoke_provider(crate::anidb::ANIDB);
    }
    // A durable session is bound to username/password. Clear it when that
    // account changed; revocation above remains in force if this write fails.
    if !same_account {
        crate::anidb::forget_session(state.enricher.data_dir()).map_err(internal)?;
    }
    let lease = state.enricher.provider_lease(crate::anidb::ANIDB);
    drop(change);
    match crate::anidb::Anidb::login_current(state.enricher.data_dir(), user, pass, key, lease)
        .await
    {
        Ok(client) => {
            client.finish().await;
            state.enricher.request_run(state.registry.clone());
            Ok(Json(SavedVerificationResponse {
                saved: true,
                verified: true,
                error: None,
            }))
        }
        // The chain, deliberately — see `admin_verify_anidb` above for why
        // this route and no other.
        Err(error) => Ok(Json(SavedVerificationResponse {
            saved: true,
            verified: false,
            error: Some(format!("{error:#}")),
        })),
    }
}

#[derive(Deserialize, ToSchema)]
struct SetTvdb {
    api_key: String,
    #[serde(default)]
    pin: Option<String>,
}

/// Set TVDB credentials
///
/// Admin only. Stores the TVDB API key and optional subscriber PIN, then
/// starts an enrichment run in the background. An empty api_key is rejected
/// with 400. The pair is stored whole: a request without a pin stores an
/// account without one, rather than keeping the pin already there.
#[utoipa::path(
    post, path = "/admin/v1/providers/tvdb", tag = "Admin providers",
    security(("bearer_auth" = [])),
    request_body = SetTvdb,
    responses(
        (status = 200, body = SavedResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_tvdb(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetTvdb>,
) -> Result<Json<SavedResponse>, ApiError> {
    // Stored as sent: a credential is the account holder's to compose, and a
    // hub that edits one hands the provider something nobody typed.
    let key = body.api_key.as_str();
    if key.is_empty() {
        return Err(ApiError::new(
            // Not ProviderUnconfigured: that says the DEPLOYMENT has no
            // credentials, and this is a blank field in the form setting them.
            // A client configuring a provider has to tell those apart.
            ErrorCode::BadRequest,
            "api_key required",
        ));
    }
    // The whole provider, so a save without a pin is a TVDB account without
    // one — credentials for a provider move together, and keeping a field the
    // caller did not send is how a pair stops agreeing with itself.
    let mut fields = std::collections::BTreeMap::from([(crate::enrich::TVDB_API_KEY, key)]);
    if let Some(pin) = body.pin.as_deref().filter(|p| !p.is_empty()) {
        fields.insert(crate::enrich::TVDB_PIN, pin);
    }
    let change = state.enricher.changing_credentials().await;
    let changed = match state.registry.hub_credential(crate::enrich::TVDB).await {
        Ok(current) => !same_fields(&current, &fields),
        Err(error) if error.is::<crate::secrets::UnreadableCredential>() => true,
        Err(error) => return Err(internal(error)),
    };
    store(&state.registry)?
        .set_provider(crate::secrets::HUB, crate::enrich::TVDB, &fields)
        .await
        .map_err(internal)?;
    if changed {
        state.enricher.revoke_provider(crate::enrich::TVDB);
    }
    drop(change);
    state.enricher.request_run(state.registry.clone());
    Ok(Json(SavedResponse { saved: true }))
}

#[derive(Deserialize, ToSchema)]
struct SetTmdb {
    api_key: String,
}

/// Set TMDB credentials
///
/// Admin only. Stores the TMDB API key and starts an enrichment run in the
/// background. An empty api_key is rejected with 400.
#[utoipa::path(
    post, path = "/admin/v1/providers/tmdb", tag = "Admin providers",
    security(("bearer_auth" = [])),
    request_body = SetTmdb,
    responses(
        (status = 200, body = SavedResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_tmdb(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetTmdb>,
) -> Result<Json<SavedResponse>, ApiError> {
    // Stored as sent, like every other credential here.
    let key = body.api_key.as_str();
    if key.is_empty() {
        return Err(ApiError::new(
            // Not ProviderUnconfigured: that says the DEPLOYMENT has no
            // credentials, and this is a blank field in the form setting them.
            // A client configuring a provider has to tell those apart.
            ErrorCode::BadRequest,
            "api_key required",
        ));
    }
    let fields = BTreeMap::from([(crate::enrich::TMDB_API_KEY, key)]);
    let change = state.enricher.changing_credentials().await;
    let changed = match state.registry.hub_credential(crate::enrich::TMDB).await {
        Ok(current) => !same_fields(&current, &fields),
        Err(error) if error.is::<crate::secrets::UnreadableCredential>() => true,
        Err(error) => return Err(internal(error)),
    };
    store(&state.registry)?
        .set_provider(crate::secrets::HUB, crate::enrich::TMDB, &fields)
        .await
        .map_err(internal)?;
    if changed {
        state.enricher.revoke_provider(crate::enrich::TMDB);
    }
    drop(change);
    // Saving still requests a pass when the plaintext is identical.
    state.enricher.request_run(state.registry.clone());
    Ok(Json(SavedResponse { saved: true }))
}

#[derive(Deserialize, ToSchema)]
struct SetFanart {
    client_key: String,
}

/// Set Fanart.tv credentials
///
/// Admin only. Stores the personal API key and starts the Album Artist artwork
/// prefetch in the background. Fanart.tv supplies artwork only and is not
/// inserted into the music metadata chain.
#[utoipa::path(
    post, path = "/admin/v1/providers/fanart", tag = "Admin providers",
    security(("bearer_auth" = [])),
    request_body = SetFanart,
    responses(
        (status = 200, body = SavedResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_fanart(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetFanart>,
) -> Result<Json<SavedResponse>, ApiError> {
    let key = body.client_key.as_str();
    if key.is_empty() {
        return Err(ApiError::new(ErrorCode::BadRequest, "client_key required"));
    }
    let fields = BTreeMap::from([(crate::enrich::FANART_CLIENT_KEY, key)]);
    let change = state.enricher.changing_credentials().await;
    let changed = match state.registry.hub_credential(crate::enrich::FANART).await {
        Ok(current) => !same_fields(&current, &fields),
        Err(error) if error.is::<crate::secrets::UnreadableCredential>() => true,
        Err(error) => return Err(internal(error)),
    };
    store(&state.registry)?
        .set_provider(crate::secrets::HUB, crate::enrich::FANART, &fields)
        .await
        .map_err(internal)?;
    if changed {
        state.enricher.revoke_provider(crate::enrich::FANART);
    }
    drop(change);
    state.enricher.request_run(state.registry.clone());
    Ok(Json(SavedResponse { saved: true }))
}

#[derive(Deserialize, ToSchema)]
struct SetTheAudioDb {
    api_key: String,
}

/// Set a premium TheAudioDB API key
///
/// Admin only. Overrides TheAudioDB's public free key and retries Album Artist
/// artwork misses. Deleting this provider's credentials restores the free key.
#[utoipa::path(
    post, path = "/admin/v1/providers/theaudiodb", tag = "Admin providers",
    security(("bearer_auth" = [])),
    request_body = SetTheAudioDb,
    responses(
        (status = 200, body = SavedResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_theaudiodb(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetTheAudioDb>,
) -> Result<Json<SavedResponse>, ApiError> {
    let key = body.api_key.as_str();
    if key.is_empty() {
        return Err(ApiError::new(ErrorCode::BadRequest, "api_key required"));
    }
    let fields = BTreeMap::from([(crate::enrich::THEAUDIODB_API_KEY, key)]);
    let change = state.enricher.changing_credentials().await;
    let changed = match state
        .registry
        .hub_credential(crate::enrich::THEAUDIODB)
        .await
    {
        Ok(current) => !same_fields(&current, &fields),
        Err(error) if error.is::<crate::secrets::UnreadableCredential>() => true,
        Err(error) => return Err(internal(error)),
    };
    store(&state.registry)?
        .set_provider(crate::secrets::HUB, crate::enrich::THEAUDIODB, &fields)
        .await
        .map_err(internal)?;
    if changed {
        state.enricher.revoke_provider(crate::enrich::THEAUDIODB);
        // A premium account can expose images absent from the free result.
        // Keep fully prefetched portraits; re-open only negative answers.
    }
    drop(change);
    state.enricher.request_run(state.registry.clone());
    Ok(Json(SavedResponse { saved: true }))
}

/// Disconnect a provider
///
/// Admin only. Deletes every credential stored for one provider. TheAudioDB
/// returns to its public free key; other providers become unconfigured.
/// Metadata already merged from that provider stays.
#[utoipa::path(
    delete, path = "/admin/v1/providers/{provider}/credentials", tag = "Admin providers",
    security(("bearer_auth" = [])),
    params(("provider" = String, Path)),
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_disconnect_provider(
    State(state): State<AppState>,
    ApiPath(provider): ApiPath<String>,
) -> Result<Json<OkResponse>, ApiError> {
    if !matches!(
        provider.as_str(),
        crate::enrich::TMDB
            | crate::enrich::TVDB
            | crate::anidb::ANIDB
            | crate::enrich::FANART
            | crate::enrich::THEAUDIODB
    ) {
        return Err(ApiError::new(ErrorCode::BadRequest, "unknown provider"));
    }
    let _change = state.enricher.changing_credentials().await;
    crate::secrets::delete_provider(state.registry.db(), crate::secrets::HUB, &provider)
        .await
        .map_err(internal)?;
    // Credentials copied by a running enrichment pass are invalid now, not
    // when that pass eventually ends. AniDB additionally keeps both a live
    // client and a session on disk; revoke marks the client stale without
    // waiting on its UDP mutex, then the durable session is removed below.
    state.enricher.revoke_provider(&provider);
    if provider == crate::anidb::ANIDB {
        let forgotten = crate::anidb::forget_session(state.enricher.data_dir());
        // Revocation above remains in force even if removing the persisted
        // session fails.
        forgotten.map_err(internal)?;
    }
    drop(_change);
    if provider == crate::enrich::THEAUDIODB {
        state.enricher.request_run(state.registry.clone());
    }
    Ok(Json(OkResponse { ok: true }))
}

/// Get enrichment status
///
/// Admin only. Returns the current state of the metadata enricher, including
/// whether a run is in progress.
#[utoipa::path(
    get, path = "/admin/v1/enrich", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = crate::enrich::EnrichStatus),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_enrich_status(
    State(state): State<AppState>,
) -> Result<Json<crate::enrich::EnrichStatus>, ApiError> {
    {
        let store = state.registry.catalogue();
        let rows = store.enrichment_status().await.map_err(internal)?;
        let (matched, weak, missed) = store.enrichment_counts().await.map_err(internal)?;
        Ok(Json(crate::enrich::EnrichStatus {
            running: rows.iter().any(|r| r.state == "running"),
            matched: matched as usize,
            weak: weak as usize,
            missed: missed as usize,
        }))
    }
}

/// Start an enrichment run
///
/// Admin only. Starts a metadata enrichment run in the background and
/// responds immediately with started true. Poll GET /admin/v1/enrich for
/// progress.
#[utoipa::path(
    post, path = "/admin/v1/enrich", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = StartedResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_enrich_run(
    State(state): State<AppState>,
) -> Result<Json<StartedResponse>, ApiError> {
    state.enricher.request_run(state.registry.clone());
    Ok(Json(StartedResponse { started: true }))
}

#[derive(serde::Deserialize, Default, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct RefreshQuery {
    deep: Option<bool>,
}

/// List users
///
/// Admin only. Returns every account with its admin flag and the libraries it
/// may see.
#[utoipa::path(
    get, path = "/admin/v1/users", tag = "Admin users",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = UsersResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_users(State(state): State<AppState>) -> Result<Json<UsersResponse>, ApiError> {
    let users = crate::grants::users_with_access(state.registry.db())
        .await
        .map_err(internal)?;
    Ok(Json(UsersResponse { users }))
}

#[derive(Deserialize, ToSchema)]
struct SetAccess {
    /// Everything, including libraries made later. When true the list is
    /// stored but not consulted — see the `grants` module doc.
    all_libraries: bool,
    #[serde(default)]
    libraries: Vec<String>,
    /// The `grants_version` this admin was shown (UI-25).
    ///
    /// Required, and not defaulted, because a guard a client can omit is not
    /// one. The panel sends the COMPLETE set rather than a delta, so without
    /// it two admins editing the same account do not merge — the second write
    /// replaces the first and the first admin's change is gone with nothing
    /// said.
    grants_version: i64,
}

#[derive(Deserialize, ToSchema)]
struct SetAdminBody {
    admin: bool,
}

/// Promote or demote a user
///
/// Admin only. Sets the account's admin flag, leaving its library grants
/// untouched, and revokes the account's existing tokens so the change applies
/// to the next request. Demoting the last admin returns 409 last_admin.
#[utoipa::path(
    put, path = "/admin/v1/users/{id}/admin", tag = "Admin users",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SetAdminBody,
    responses(
        (status = 200, body = UserAdminResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_user_admin(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<SetAdminBody>,
) -> Result<Json<UserAdminResponse>, ApiError> {
    match state
        .auth
        .set_admin(&id, body.admin)
        .await
        .map_err(internal)?
    {
        crate::auth::SetAdmin::NoSuchUser => Err(hidden("user")),
        crate::auth::SetAdmin::LastAdmin => Err(ApiError::new(
            // Its own code, not FORBIDDEN and not a bare CONFLICT.
            // `require_admin` above already answers FORBIDDEN for "your token
            // is not an admin", and a client could not tell re-authenticate
            // from pick-another-account without reading the prose. It can now.
            ErrorCode::LastAdmin,
            "refusing to demote the last admin",
        )),
        _ => Ok(Json(UserAdminResponse {
            id,
            is_admin: body.admin,
        })),
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateUser {
    username: String,
    password: String,
    #[serde(default)]
    admin: bool,
}

/// Create a user
///
/// Admin only. Creates an account with a username, password and optional
/// admin flag, returning its id. A taken username returns 409; a username or
/// password that breaks the credential policy returns 400 naming the rule.
#[utoipa::path(
    post, path = "/admin/v1/users", tag = "Admin users",
    security(("bearer_auth" = [])),
    request_body = CreateUser,
    responses(
        (status = 200, body = CreatedUserResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 409, description = "That username is already taken", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_create_user(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateUser>,
) -> Result<Json<CreatedUserResponse>, ApiError> {
    let id = state
        .auth
        .create_user(&body.username, &body.password, body.admin)
        .await
        // `auth::create_user` turns the UNIQUE violation into a fresh
        // `anyhow!` with no sqlx underneath, and passes every other database
        // error through as one — so this split lands the way it reads.
        .map_err(|e| {
            // The taken name is its own answer, and the same one a taken
            // library name gets — the two create routes disagreed about the
            // code for an identical collision until this.
            if e.downcast_ref::<crate::auth::UsernameTaken>().is_some() {
                ApiError::new(ErrorCode::Conflict, "that username is already taken")
            } else if let Some(policy) = e.downcast_ref::<crate::auth::CredentialPolicyError>() {
                // The policy already says which rule was broken, and `setup`
                // has always reported it. A blank username was being told the
                // PASSWORD was too short, because one fixed sentence stood in
                // for two different refusals on the same route.
                ApiError::new(ErrorCode::BadRequest, policy.to_string())
            } else {
                // Not a refusal this route knows how to name — hashing failed,
                // or the write did. Neither is the admin's to fix.
                internal(e)
            }
        })?;
    Ok(Json(CreatedUserResponse {
        id,
        username: body.username,
        admin: body.admin,
    }))
}

/// Delete a user account
///
/// Admin only. Deletes the user and ends their playback sessions, returning
/// the deleted id, username and session count. Returns 409 when deleting
/// yourself or the last remaining admin.
#[utoipa::path(
    delete, path = "/admin/v1/users/{id}", tag = "Admin users",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = DeletedUserResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_delete_user(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<DeletedUserResponse>, ApiError> {
    // Deleting yourself would revoke your own token mid-request and,
    // for the only admin, leave nobody who can undo it.
    if id == claims.sub {
        return Err(ApiError::new(
            // Its own code, for the reason the admin-flag route gives:
            // FORBIDDEN is what `require_admin` says when your token is not an
            // admin at all, so a client could not tell "re-authenticate" from
            // "pick a different target" without reading the prose.
            ErrorCode::SelfTarget,
            "cannot delete the account you are signed in as",
        ));
    }
    let username = match state.auth.delete_user(&id).await.map_err(internal)? {
        crate::auth::DeleteUser::Deleted(username) => username,
        crate::auth::DeleteUser::NoSuchUser => return Err(hidden("user")),
        crate::auth::DeleteUser::LastAdmin => {
            return Err(ApiError::new(
                ErrorCode::LastAdmin,
                "refusing to delete the last admin",
            ));
        }
    };
    // After the committed delete, not before: a refused operation must not end
    // somebody's sessions. Authentication now rejects the missing user row on
    // every request, so no process-local tombstone is needed.
    let sessions_ended = state.sessions.end_for_user(&id).await;
    Ok(Json(DeletedUserResponse {
        deleted: id,
        username,
        sessions_ended,
    }))
}

/// List registered satellites
///
/// Admin only. Returns an overview of every registered satellite, including
/// the hub's in-process mediahost.
#[utoipa::path(
    get, path = "/admin/v1/satellites", tag = "Admin satellites",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = SatellitesResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_satellites(
    State(state): State<AppState>,
) -> Result<Json<SatellitesResponse>, ApiError> {
    let satellites = state
        .registry
        .satellites_overview()
        .await
        .map_err(internal)?;
    Ok(Json(SatellitesResponse { satellites }))
}

/// Remove a satellite
///
/// Admin only. Removes the satellite from the allowlist, ends its sessions
/// and removes its mediadb catalogue. Legacy payloads remain untouched. Returns 409 for the in-process
/// mediahost and 404 for an unknown id.

#[utoipa::path(
    delete, path = "/admin/v1/satellites/{id}", tag = "Admin satellites",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = DeletedSatelliteResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_delete_satellite(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<DeletedSatelliteResponse>, ApiError> {
    // Before ending anything: the hub's own mediahost is not a satellite
    // this operation can act on, and refusing after tearing its sessions
    // down would be the destructive half of an operation that then fails.
    if state.registry.is_in_process(&id).await.map_err(internal)? {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the in-process mediahost cannot be deleted: it is the hub itself",
        ));
    }
    let ended = state.sessions.end_for_module(&id).await;
    let deleted = state
        .registry
        .delete_satellite(&id)
        .await
        // The pre-check above is load-bearing, not belt and braces: the
        // registry ALSO refuses the in-process mediahost, with a plain
        // `ensure!` that `refusal_or_internal` would read as "no such
        // satellite" — a 404 for a box that is plainly there. Typing it in the
        // registry would be the durable fix; until then the two must not
        // diverge.
        .map_err(|e| refusal_or_internal(ErrorCode::NotFound, "no such satellite", e))?;
    // Old subtitle/cache state remains untouched until its consumer is ported.
    let removed_payloads = 0;
    Ok(Json(DeletedSatelliteResponse {
        deleted: id,
        removed: deleted.fingerprint,
        sessions_ended: ended,
        subtitle_payloads_removed: removed_payloads,
    }))
}

#[derive(serde::Deserialize, ToSchema)]
struct SetDisabled {
    disabled: bool,
}

/// Set satellite placement state
///
/// Admin only. Marks a satellite disabled or enabled for session placement
/// and returns 204. Disabling only stops new placements; sessions already
/// running on it continue.
#[utoipa::path(
    post, path = "/admin/v1/satellites/{id}/disabled", tag = "Admin satellites",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SetDisabled,
    responses(
        (status = 204, description = "Placement state updated"),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_set_disabled(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<SetDisabled>,
) -> Result<StatusCode, ApiError> {
    state
        .registry
        .set_disabled(&id, body.disabled)
        .await
        .map_err(internal)?;
    tracing::info!(module_id = %id, disabled = body.disabled, "satellite placement toggle");
    Ok(StatusCode::NO_CONTENT)
}

/// List active playback sessions
///
/// Admin only. Returns every live session with its user, item title, playback
/// mode, satellite, idle time and, once negotiated, its stream and delivery
/// cost summary.
#[utoipa::path(
    get, path = "/admin/v1/sessions", tag = "Admin sessions",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = AdminSessionsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_sessions(
    State(state): State<AppState>,
) -> Result<Json<AdminSessionsResponse>, ApiError> {
    let mut sessions = Vec::new();
    for session in state.sessions.list() {
        let title = {
            let captured = &session.catalogue;
            state
                .registry
                .catalogue()
                .library_item_record(&captured.parent_id)
                .await
                .ok()
                .map(|i| i.title)
        };
        let username = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(&session.user_id)
            .fetch_optional(state.registry.db())
            .await
            .map_err(internal)?;
        let streams = session
            .verdict
            .lock()
            .unwrap()
            .as_ref()
            .map(|(video, audio)| SessionStreamSummary {
                cost: session.delivery_cost(),
                video: video.clone(),
                audio: audio.clone(),
            });
        sessions.push(AdminSession {
            session_id: session.id.clone(),
            username,
            title,
            mode: match &session.mode {
                crate::sessions::Mode::Direct { .. } => "direct",
                crate::sessions::Mode::Remux { .. } => "remux",
                crate::sessions::Mode::Transcode { .. } => "transcode",
            },
            module_id: session.module_id.clone(),
            idle_secs: session.idle_for().as_secs(),
            streams,
        });
    }
    Ok(Json(AdminSessionsResponse { sessions }))
}

/// Download session diagnostics log
///
/// Admin only. Returns the session's diagnostics as a plain-text attachment.
/// Returns 404 when no log exists and 503 when the satellite holding the log
/// is not answering.
#[utoipa::path(
    get, path = "/admin/v1/sessions/{id}/log", tag = "Admin sessions",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = String, content_type = "text/plain; charset=utf-8",
            headers(("content-disposition" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The transcoder running this session is not answering, or the hub has no administrator yet (`setup_required`)", body = ApiErrorBody)
    )
)]
async fn admin_session_log(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Response, ApiError> {
    let body = state
        .sessions
        .collect_logs(&state.registry, &id)
        .await
        .map_err(
            |e| match e.downcast_ref::<crate::sessions::SatelliteSilent>() {
                // A wedged transcoder is not an absent log. This is the route an
                // operator reaches for when a session is misbehaving, and telling
                // them the logs do not exist — when the box holding them simply
                // is not answering — sends them to look somewhere else.
                // `log`, so the transport chain the producer attached is kept —
                // the sentence is the type's own and says nothing about which
                // link failed, which is the next thing an operator asks.
                Some(silent) => {
                    let message = silent.to_string();
                    ApiError::log(ErrorCode::SatelliteUnreachable, message, e)
                }
                None => refusal_or_internal(ErrorCode::NotFound, "no logs for that session", e),
            },
        )?;
    Ok(log_attachment(format!("kahawai-session-{id}.log"), body))
}

/// Download newest session log for an item
///
/// Admin only. Returns the most recent session diagnostics recorded for the
/// stable mediadb item (including its episodes/tracks), by any user, as a plain-text attachment.
/// Returns 404 when no such log has been stored.
#[utoipa::path(
    get, path = "/admin/v1/items/{id}/log", tag = "Admin sessions",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = String, content_type = "text/plain; charset=utf-8",
            headers(("content-disposition" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_item_log(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<Response, ApiError> {
    let data_dir = state
        .sessions
        .data_dir()
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no data dir"))?;
    let path = crate::sessionlog::newest_for_item(data_dir, &id)
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no session logs for this item"))?;
    let body = std::fs::read_to_string(&path).map_err(internal)?;
    Ok(log_attachment(format!("kahawai-item-{id}.log"), body))
}

fn log_attachment(filename: String, body: String) -> Response {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// End a playback session
///
/// Admin only. Ends the session and returns 204, or 404 when no session with
/// that id is active.
#[utoipa::path(
    delete, path = "/admin/v1/sessions/{id}", tag = "Admin sessions",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 204, description = "Session ended"),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_end_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.sessions.end(&id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(session_gone())
    }
}

#[derive(Deserialize, ToSchema)]
struct SetupRequest {
    username: String,
    password: String,
}

async fn setup_bootstrap(State(state): State<SetupState>) -> Json<BootstrapResponse> {
    Json(BootstrapResponse {
        setup_required: state.auth.setup_required(),
        setup_available: true,
        setup_url: None,
    })
}

/// Create the initial administrator
///
/// Served only on the hub's dedicated loopback setup listener. Creates the
/// first admin and returns 204; refuses with 403 unless Host and Origin match
/// a loopback address, and 409 once setup is complete.
#[utoipa::path(
    post, path = "/api/v1/setup", tag = "Setup (trusted local listener)",
    request_body = SetupRequest,
    responses(
        (status = 204, description = "Initial admin created"),
        (status = 400, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody)
    )
)]
async fn setup(
    State(state): State<SetupState>,
    headers: axum::http::HeaderMap,
    ApiJson(body): ApiJson<SetupRequest>,
) -> Result<StatusCode, ApiError> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok());
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("http://"))
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok());
    let local_host = |authority: &axum::http::uri::Authority| {
        authority.host().eq_ignore_ascii_case("localhost")
            || authority
                .host()
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    if !matches!((&host, &origin), (Some(h), Some(o)) if h == o && local_host(h)) {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "setup requires its local same-origin page",
        ));
    }
    if !state.auth.setup_required() {
        return Err(ApiError::new(
            ErrorCode::SetupComplete,
            "setup already completed",
        ));
    }
    state
        .auth
        .complete_setup(&body.username, &body.password)
        .await
        .map_err(|e| match e {
            error @ CompleteSetupError::InvalidInput(_) => {
                ApiError::new(ErrorCode::BadRequest, error.to_string())
            }
            error @ CompleteSetupError::AlreadyCompleted => {
                // The same condition as the pre-check above, so the same
                // code: this is the arm two concurrent setups race into, and a
                // client branching on `setup_complete` must not depend on
                // which one it was.
                ApiError::new(ErrorCode::SetupComplete, error.to_string())
            }
            CompleteSetupError::Internal(source) => {
                tracing::error!(error = format!("{source:#}"), "initial-admin setup failed");
                ApiError::new(ErrorCode::Internal, "initial-admin setup failed")
            }
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "lowercase")]
enum AuthClient {
    Browser,
    Api,
}

#[derive(Deserialize, ToSchema)]
struct LoginRequest {
    client: AuthClient,
    username: String,
    password: String,
}

#[derive(Deserialize, ToSchema)]
struct RefreshRequest {
    client: AuthClient,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct LogoutRequest {
    client: AuthClient,
    #[serde(default)]
    refresh_token: Option<String>,
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

struct AuthRequestMeta {
    headers: axum::http::HeaderMap,
    peer: Option<std::net::IpAddr>,
}

impl axum::extract::FromRequestParts<AppState> for AuthRequestMeta {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self {
            headers: parts.headers.clone(),
            peer: parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|c| c.0.ip()),
        })
    }
}

fn request_browser_origin(state: &AppState, meta: &AuthRequestMeta) -> Option<PublicOrigin> {
    let header = |name| meta.headers.get(name).and_then(|value| value.to_str().ok());
    state
        .proxy_trust
        .forwarded_origin(
            meta.peer,
            header("x-forwarded-proto"),
            header("x-forwarded-host"),
        )
        .and_then(|origin| PublicOrigin::parse(&origin).ok())
        .or_else(|| {
            header("host").and_then(|host| PublicOrigin::parse(&format!("http://{host}")).ok())
        })
}

fn browser_cookie_secure(state: &AppState, meta: &AuthRequestMeta) -> Result<bool, ApiError> {
    let Some(expected) = &state.public_origin else {
        return Ok(request_browser_origin(state, meta).is_some_and(|origin| origin.secure()));
    };
    let forbidden = || {
        ApiError::new(
            ErrorCode::Forbidden,
            "browser authentication requires the canonical Origin",
        )
    };
    let presented = meta
        .headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| PublicOrigin::parse(value).ok())
        .ok_or_else(forbidden)?;
    if &presented != expected {
        return Err(forbidden());
    }
    Ok(expected.secure())
}

fn auth_cookie(
    name: &str,
    value: &str,
    path: &str,
    max_age: i64,
    secure: bool,
) -> axum::http::HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}")
        .parse()
        .expect("generated auth token is a valid cookie value")
}

fn append_auth_cookies(response: &mut Response, tokens: &crate::auth::TokenPair, secure: bool) {
    response.headers_mut().append(
        axum::http::header::SET_COOKIE,
        auth_cookie(
            "kahawai_refresh",
            &tokens.refresh_token,
            "/api/v1/auth",
            crate::auth::REFRESH_TTL_SECS,
            secure,
        ),
    );
    response.headers_mut().append(
        axum::http::header::SET_COOKIE,
        auth_cookie(
            "kahawai_media",
            &tokens.access_token,
            "/api/v1",
            crate::auth::ACCESS_TTL_SECS,
            secure,
        ),
    );
}

fn clear_auth_cookies(response: &mut Response, secure: bool) {
    for (name, path) in [
        ("kahawai_refresh", "/api/v1/auth"),
        ("kahawai_media", "/api/v1"),
    ] {
        response.headers_mut().append(
            axum::http::header::SET_COOKIE,
            auth_cookie(name, "", path, 0, secure),
        );
    }
}

fn token_response(tokens: crate::auth::TokenPair, client: AuthClient, secure: bool) -> Response {
    if client == AuthClient::Api {
        return Json(tokens).into_response();
    }
    let mut response = Json(BrowserTokenResponse {
        access_token: tokens.access_token.clone(),
        expires_in: tokens.expires_in,
    })
    .into_response();
    append_auth_cookies(&mut response, &tokens, secure);
    response
}

/// OPS-2 thresholds: consecutive failures before lockout. The per-IP
/// bar is higher so one shared NAT doesn't lock a household out.
const THROTTLE_USER_AFTER: u32 = 5;
const THROTTLE_IP_AFTER: u32 = 20;

/// Sign in and get an access token
///
/// Exchanges a username and password for tokens. API clients receive the
/// refresh token in the body; browser clients receive HttpOnly cookies and
/// must send a matching Origin. Repeated failures return 429 with a
/// Retry-After header.
#[utoipa::path(
    post, path = "/api/v1/auth/token", tag = "Authentication",
    request_body = LoginRequest,
    params(("Origin" = Option<String>, Header, description = "Required for browser mode when hub.public_url is configured; must match it exactly")),
    responses(
        (status = 200, body = AuthSuccessResponse, headers(("set-cookie" = String, description = "Browser clients receive refresh and media cookies"))),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 429, body = ApiErrorBody, headers(("retry-after" = String, description = "Seconds until the lockout clears"))),
        (status = 503, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody)
    )
)]
async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    meta: AuthRequestMeta,
    ApiJson(body): ApiJson<LoginRequest>,
) -> Result<Response, ApiError> {
    if state.auth.setup_required() {
        return Err(ApiError::new(ErrorCode::SetupRequired, "setup required"));
    }
    let secure = match body.client {
        AuthClient::Browser => browser_cookie_secure(&state, &meta)?,
        AuthClient::Api => false,
    };
    let user_key = format!("u:{}", body.username.to_lowercase());
    let ip_key = ip.map(|i| format!("ip:{i}"));
    let locked = state.auth.throttle.locked(&user_key).or_else(|| {
        ip_key
            .as_deref()
            .and_then(|key| state.auth.throttle.locked(key))
    });
    if let Some(wait) = locked {
        tracing::warn!(username = %body.username, ip = ?ip, "login throttled");
        // `Retry-After` as well as the sentence: a lockout runs from 30 s to
        // fifteen minutes, and until this the only statement of which was in
        // `message` — prose the contract tells clients not to read.
        return Err(ApiError::new(
            ErrorCode::LoginThrottled,
            format!("too many attempts; retry in {}s", wait.as_secs().max(1)),
        )
        .retry_after(wait.as_secs().max(1)));
    }
    match state.auth.login(&body.username, &body.password).await {
        Ok(tokens) => {
            state.auth.throttle.clear(&user_key);
            if let Some(key) = &ip_key {
                state.auth.throttle.clear(key);
            }
            Ok(token_response(tokens, body.client, secure))
        }
        Err(_) => {
            let lock = state.auth.throttle.fail(&user_key, THROTTLE_USER_AFTER);
            if let Some(key) = &ip_key {
                state.auth.throttle.fail(key, THROTTLE_IP_AFTER);
            }
            tracing::warn!(username = %body.username, ip = ?ip, locked = ?lock, "login failed");
            Err(ApiError::new(
                ErrorCode::InvalidCredentials,
                "invalid credentials",
            ))
        }
    }
}

/// Rotate a refresh token for a new access token
///
/// API clients send refresh_token in the body; browser clients omit it, send
/// the kahawai_refresh cookie and get rotated cookies back. An invalid
/// browser refresh clears the auth cookies.
#[utoipa::path(
    post, path = "/api/v1/auth/refresh", tag = "Authentication",
    request_body = RefreshRequest,
    params(("Origin" = Option<String>, Header, description = "Required for browser mode when hub.public_url is configured; must match it exactly")),
    responses(
        (status = 200, body = AuthSuccessResponse, headers(("set-cookie" = String, description = "Rotated browser cookies"))),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody)
    )
)]
async fn refresh(
    State(state): State<AppState>,
    meta: AuthRequestMeta,
    ApiJson(body): ApiJson<RefreshRequest>,
) -> Result<Response, ApiError> {
    let (token, secure) = match body.client {
        AuthClient::Api => (
            body.refresh_token
                .as_deref()
                .filter(|token| !token.is_empty())
                .ok_or(ApiError::new(
                    ErrorCode::BadRequest,
                    "refresh_token required",
                ))?,
            None,
        ),
        AuthClient::Browser => {
            if body.refresh_token.is_some() {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "browser refresh_token must be omitted",
                ));
            }
            let secure = browser_cookie_secure(&state, &meta)?;
            let Some(token) = cookie(&meta.headers, "kahawai_refresh") else {
                let mut response =
                    ApiError::new(ErrorCode::InvalidRefresh, "invalid refresh token")
                        .into_response();
                clear_auth_cookies(&mut response, secure);
                return Ok(response);
            };
            (token, Some(secure))
        }
    };
    match state.auth.refresh(token).await {
        Ok(tokens) => Ok(token_response(tokens, body.client, secure.unwrap_or(false))),
        Err(crate::auth::RefreshError::Invalid) if body.client == AuthClient::Browser => {
            let mut response =
                ApiError::new(ErrorCode::InvalidRefresh, "invalid refresh token").into_response();
            clear_auth_cookies(&mut response, secure.unwrap_or(false));
            Ok(response)
        }
        Err(crate::auth::RefreshError::Invalid) => Err(ApiError::new(
            ErrorCode::InvalidRefresh,
            "invalid refresh token",
        )),
        Err(crate::auth::RefreshError::Internal(error)) => Err(internal(error)),
    }
}

/// Revoke a refresh token
///
/// Requires a bearer access token. API clients pass refresh_token in the
/// body; browser clients omit it and send the kahawai_refresh cookie.
/// Responds 204 and clears the auth cookies for browser clients.
#[utoipa::path(
    post, path = "/api/v1/auth/logout", tag = "Authentication",
    security(("bearer_auth" = [])),
    request_body = LogoutRequest,
    params(("Origin" = Option<String>, Header, description = "Required for browser mode when hub.public_url is configured; must match it exactly")),
    responses(
        (status = 204, description = "Refresh token revoked", headers(("set-cookie" = String, description = "Browser cookies cleared"))),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn logout(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    meta: AuthRequestMeta,
    ApiJson(body): ApiJson<LogoutRequest>,
) -> Result<Response, ApiError> {
    let (token, secure) = match body.client {
        AuthClient::Api => (
            body.refresh_token
                .as_deref()
                .filter(|token| !token.is_empty())
                .ok_or(ApiError::new(
                    ErrorCode::BadRequest,
                    "refresh_token required",
                ))?,
            None,
        ),
        AuthClient::Browser => {
            if body.refresh_token.is_some() {
                return Err(ApiError::new(
                    ErrorCode::BadRequest,
                    "browser refresh_token must be omitted",
                ));
            }
            let secure = browser_cookie_secure(&state, &meta)?;
            let token = cookie(&meta.headers, "kahawai_refresh").ok_or(ApiError::new(
                ErrorCode::Unauthenticated,
                "refresh cookie required",
            ))?;
            (token, Some(secure))
        }
    };
    state
        .auth
        .logout(&claims.sub, token)
        .await
        .map_err(internal)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Some(secure) = secure {
        clear_auth_cookies(&mut response, secure);
    }
    Ok(response)
}

#[derive(Deserialize, ToSchema)]
struct StartSessionRequest {
    /// Required by the mediadb playback API; limits physical sources to this library.
    library_id: Option<String>,
    /// Stable mediadb rendition ID.
    media_entry_id: Option<String>,
    item_id: String,
    /// Response-local source selection must be accompanied by media_entry_id.
    source_id: Option<i64>,
    /// Explicit mode = the pre-negotiation contract, verbatim (scripts,
    /// debugging). Absent = the hub negotiates from `profile`.
    #[serde(default)]
    mode: Option<String>,
    /// The client's capability profile; absent = conservative fallback.
    #[serde(default)]
    profile: Option<kahawai_core::media::CapabilityProfile>,
    /// Begin playback here (resume without waiting for a transcode to
    /// catch up) — keyframe-snapped by the pipeline.
    #[serde(default)]
    start_ms: Option<u64>,
    /// For recovery or a seek, the fingerprint returned by the preceding session.
    /// Saved resume is item-relative only when `resume` is true.
    resume_source_fingerprint: Option<String>,
    /// True for an item-relative saved resume; false for an explicit chapter position.
    #[serde(default)]
    resume: bool,
    /// Track indexes in the source's discovery order. The UI
    /// resolves defaults from /api/v1/prefs client-side.
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

/// Subscribe to server-sent invalidation events
///
/// Streams server-sent invalidation hints; a client refetches whatever a hint
/// names. Authenticates with a bearer token or, for EventSource, the
/// kahawai_media cookie.
#[utoipa::path(
    get, path = "/api/v1/events", tag = "Events",
    security(("bearer_auth" = []), ("media_token" = [])),
    responses(
        (status = 200, body = String, content_type = "text/event-stream", headers(("x-accel-buffering" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn events(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio_stream::StreamExt;
    let rx = state.registry.subscribe_events();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|event| {
        event.ok().map(|event| {
            Ok::<_, std::convert::Infallible>(
                Event::default().data(serde_json::to_string(&event).expect("serializable event")),
            )
        })
    });
    // OPS-8: tell buffering proxies (nginx) to pass events through live.
    (
        axum::response::AppendHeaders([("x-accel-buffering", "no")]),
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

/// List the current user's preferences
///
/// Returns every stored preference for the authenticated user as scope, key
/// and value, where scope is a library id or an empty string for user-global
/// keys.
#[utoipa::path(
    get, path = "/api/v1/prefs", tag = "Preferences",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = PreferencesResponse),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn get_prefs(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<PreferencesResponse>, ApiError> {
    let rows = sqlx::query("SELECT scope, key, value FROM user_prefs WHERE user_id = ?")
        .bind(&claims.sub)
        .fetch_all(state.registry.db())
        .await
        .map_err(internal)?;
    let prefs = rows
        .iter()
        .map(|row| Preference {
            scope: row.get("scope"),
            key: row.get("key"),
            value: row.get("value"),
        })
        .collect();
    Ok(Json(PreferencesResponse { prefs }))
}

#[derive(Deserialize, ToSchema)]
struct PutPrefRequest {
    #[serde(default)]
    scope: String,
    key: String,
    /// Empty value deletes the preference.
    value: String,
}

/// Set or delete a preference
///
/// Stores one preference for the authenticated user; an empty value deletes
/// it. Scope and key are limited to 64 characters and value to 256, beyond
/// which the request is rejected with 400.
#[utoipa::path(
    put, path = "/api/v1/prefs", tag = "Preferences",
    security(("bearer_auth" = [])),
    request_body = PutPrefRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn put_pref(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<PutPrefRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    if body.key.len() > 64 || body.value.len() > 256 || body.scope.len() > 64 {
        return Err(ApiError::new(ErrorCode::BadRequest, "preference too long"));
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
    Ok(Json(OkResponse { ok: true }))
}

/// Whether an OpenSubtitles account is attached
///
/// Both halves or neither: a username with no password cannot log in, and
/// answering `configured` for it sends the viewer looking for a fault
/// somewhere else. The account itself is never returned — the store does not
/// read secrets back out to clients.
#[utoipa::path(
    get, path = "/api/v1/account/opensubtitles", tag = "Preferences",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = ProviderConfiguration),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn account_opensubtitles(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ProviderConfiguration>, ApiError> {
    let fields = store(&state.registry)?
        .get_provider(&claims.sub, crate::opensubtitles::OPENSUBTITLES)
        .await
        .map_err(internal)?;
    Ok(Json(ProviderConfiguration {
        configured: fields.contains_key(crate::opensubtitles::USERNAME)
            && fields.contains_key(crate::opensubtitles::PASSWORD),
    }))
}

#[derive(Deserialize, ToSchema)]
struct SetOpenSubtitlesAccount {
    username: String,
    password: String,
}

/// Attach an OpenSubtitles account
///
/// The account is the authenticated user's own, and its download entitlement
/// is what their searches spend. Both fields are stored together, replacing
/// whatever was there; either one empty is rejected with 400. Detaching is
/// DELETE, not a blank save.
#[utoipa::path(
    post, path = "/api/v1/account/opensubtitles", tag = "Preferences",
    security(("bearer_auth" = [])),
    request_body = SetOpenSubtitlesAccount,
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn set_account_opensubtitles(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<SetOpenSubtitlesAccount>,
) -> Result<Json<OkResponse>, ApiError> {
    // Both halves required: a blank either side is an account that reports
    // itself attached and can never log in. Stored as sent -- what is inside
    // a password is the account holder's business, not this route's.
    let (user, pass) = (body.username.as_str(), body.password.as_str());
    if user.is_empty() || pass.is_empty() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "username and password required",
        ));
    }
    store(&state.registry)?
        .set_provider(
            &claims.sub,
            crate::opensubtitles::OPENSUBTITLES,
            &std::collections::BTreeMap::from([
                (crate::opensubtitles::USERNAME, user),
                (crate::opensubtitles::PASSWORD, pass),
            ]),
        )
        .await
        .map_err(internal)?;
    Ok(Json(OkResponse { ok: true }))
}

/// Detach the OpenSubtitles account
///
/// Searches then fall back to the deployment's shared anonymous budget.
#[utoipa::path(
    delete, path = "/api/v1/account/opensubtitles", tag = "Preferences",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = OkResponse),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn delete_account_opensubtitles(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<OkResponse>, ApiError> {
    crate::secrets::delete_provider(
        state.registry.db(),
        &claims.sub,
        crate::opensubtitles::OPENSUBTITLES,
    )
    .await
    .map_err(internal)?;
    Ok(Json(OkResponse { ok: true }))
}

/// 409 says "not with this item"; 503 says "not right now". Every other
/// refusal from the session layer is about the item and will refuse again
/// forever; an absent mediahost is about the moment, and a client is meant
/// to stand by and retry rather than give up.
fn session_refusal(e: anyhow::Error) -> ApiError {
    // The caller's own input, before anything about the item is considered: a
    // seek that names a subtitle track this item does not have. Everything
    // else here is a verdict on the item, and folding this in with them told a
    // viewer changing subtitles that a film which was playing a second earlier
    // could not be played.
    if let Some(missing) = e.downcast_ref::<crate::sessions::NoSuchTrack>() {
        return ApiError::new(ErrorCode::BadRequest, missing.to_string());
    }
    let code = if e.downcast_ref::<crate::sessions::SourceOffline>().is_some() {
        ErrorCode::SourceOffline
    } else if e.downcast_ref::<crate::sessions::SessionCap>().is_some() {
        // Not `Unplayable`, which is what every other refusal here means and
        // what this used to arrive as. The cap clears the moment a session
        // ends, and a client playing a queue — the album player holds two
        // sessions, so a film beside it is enough to reach the limit — has to
        // be able to tell "wait" from "never". Both were 409 with the
        // difference in the prose, and ours guessed at three more tries.
        ErrorCode::SessionCap
    } else {
        ErrorCode::Unplayable
    };
    // The sentence comes from the CODE, not from the error, and this is the
    // route that decides it: `Sessions::start` fails with the hub's scratch
    // layout, the worker's executable path and four lines of GStreamer stderr
    // baked into its outermost layer. All of that goes to the log.
    //
    // `SourceOffline` and `SessionCap` are types with sentences of their own,
    // but they are not read from here either — an `anyhow` chain can carry
    // them at any depth, and one of them is already wrapped with the transport
    // prose above it. Reading the code and writing the sentence is the only
    // spelling that cannot be undone from a distance.
    let message = match code {
        ErrorCode::SourceOffline => "the machine holding this file is not connected right now",
        // It has to name the action, because the screen it lands on offers
        // none: the player prints this sentence and a way home, and standing
        // by instead would claim the machine holding the file is unreachable.
        ErrorCode::SessionCap => {
            "this account is already watching as much as it may at once; close one first"
        }
        _ => "this item cannot be played",
    };
    // `new` for the two that are EXPECTED and polled — a player standing by
    // and an album queue both re-ask every five seconds for as long as the
    // condition lasts, so one waiting viewer would write around 720 chained
    // warn lines an hour into the log this change exists to make useful. Both
    // are self-clearing states with authored messages and nothing to diagnose.
    // `Unplayable` keeps its chain: it is asked once, and its cause is the
    // whole reason anybody reads this log.
    match code {
        ErrorCode::Unplayable => ApiError::log(code, message, e),
        // `debug`. A polled outage would write around 720 warn lines an hour,
        // and dropping the cause entirely left a refused seek recorded with
        // its position and no reason at all. Below the default filter, so the
        // standing record of a host going away is the registry's own
        // `satellite disconnected` at info; this is the per-request detail for
        // somebody who has turned debug on. The seek's own warn carries the
        // CODE either way.
        _ => {
            tracing::debug!(code = ?code, error = format!("{e:#}"), "playback refused");
            ApiError::new(code, message)
        }
    }
}

/// Start a playback session
///
/// Creates a session for an item and returns its id, negotiated mode (direct,
/// remux or transcode), stream URL and subtitle listing. Returns 409 for an
/// unplayable item, 429 at the stream cap and 503 when the mediahost is
/// offline.
#[utoipa::path(
    post, path = "/api/v1/playback/sessions", tag = "Playback",
    security(("bearer_auth" = [])),
    request_body = StartSessionRequest,
    responses(
        (status = 201, body = StartSessionResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, description = "Refused about the ITEM, and forever: `unplayable`", body = ApiErrorBody),
        (status = 429, description = "This account is at its stream cap; clears when one ends: `session_cap`", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The mediahost holding the bytes is away: `source_offline`, or the hub has no administrator yet (`setup_required`)", body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody)
    )
)]
async fn start_session(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<StartSessionRequest>,
) -> Result<(StatusCode, Json<StartSessionResponse>), ApiError> {
    let catalogue = {
        let library = body
            .library_id
            .as_ref()
            .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "library_id is required"))?;
        catalogue::visible(&state, &claims, library).await?;
        let item = state
            .registry
            .catalogue()
            .playback_item(library, &body.item_id)
            .await
            .map_err(catalogue::store_error)?;
        if body.resume_source_fingerprint.is_none()
            && body
                .media_entry_id
                .as_ref()
                .is_some_and(|id| !item.renditions.iter().any(|r| r.entry.id == *id))
        {
            return Err(hidden("source"));
        }
        Some(crate::sessions::CataloguePlayback {
            subtitle_cache: state.subtitles.cache_dir().into(),
            library_id: library.clone(),
            item,
            media_entry_id: body
                .resume_source_fingerprint
                .is_none()
                .then(|| body.media_entry_id.clone())
                .flatten(),
        })
    };
    let session = state
        .sessions
        .start(
            &state.registry,
            &state.subtitles,
            &claims.sub,
            &body.item_id,
            body.mode.as_deref(),
            body.profile.clone(),
            crate::sessions::StartOptions {
                catalogue,
                ms: body.start_ms.unwrap_or(0),
                explicit_position: body.start_ms.is_some(),
                resume: body.resume,
                source_fingerprint: body.resume_source_fingerprint,
            },
            body.audio_track,
            body.video_track,
            body.subtitle_track,
        )
        .await
        .map_err(session_refusal)?;
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
    let streams = session
        .verdict
        .lock()
        .unwrap()
        .as_ref()
        .map(|(video, audio)| PlaybackStreams {
            // Aggregate semantic work, separate from `mode`, which says
            // where/how the pipeline runs.
            cost: session.delivery_cost(),
            video: video.clone(),
            audio: audio.clone(),
            // Additive (HUB-32a/b); [] on explicit-mode sessions.
            subtitles: session.sub_verdicts.lock().unwrap().clone(),
        });
    let mut subtitle_listing = { session.catalogue_listing() };
    // Extraction uses the captured physical owner; public resource URLs use
    // the canonical requested member, including a member of combined coverage.
    for listing in &mut subtitle_listing {
        listing.track.item_id.clone_from(&session.item_id);
        {
            listing.deletable = listing.track.origin == "downloaded"
                && (claims.admin || listing.track.created_by.as_deref() == Some(&claims.sub));
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            segments: session.catalogue.segments.clone(),
            media_entry_id: Some(session.catalogue.media_entry_id.clone()),
            session_id: session.id.clone(),
            effective_start_ms: session.effective_start_ms,
            source_fingerprint: session.source_fingerprint.clone(),
            source_id: session.playable_source_id,
            replay_gain: session.replay_gain.clone(),
            library_item_ids: session.library_item_ids.clone(),
            mode,
            size: session.size,
            duration_ms: session.duration_ms,
            part_base_ms: session.part_base_ms(),
            parts: session.parts.len(),
            content_type: ctype,
            stream_url,
            streams,
            subtitle_listing,
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
struct SeekRequest {
    position_ms: u64,
    /// Switch tracks during the restart.
    audio_track: Option<u32>,
    video_track: Option<u32>,
    /// Switch the burned subtitle mid-session (unified track id): an
    /// image track starts burning it, a text track withdraws an
    /// explicit burn. Absent = keep as is.
    #[serde(default)]
    subtitle_track: Option<i64>,
}

/// Seek a playback session
///
/// Restarts the session's pipeline at the given position, optionally
/// switching the audio, video or burned subtitle track. The session id and
/// URLs are unchanged; an unknown session returns 404.
#[utoipa::path(
    post, path = "/api/v1/playback/sessions/{id}/seek", tag = "Playback",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SeekRequest,
    responses(
        (status = 200, body = SeekResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 503, body = ApiErrorBody, description = "The mediahost holding the bytes went away while the lease was re-opened (`source_offline`), or the hub has no administrator yet (`setup_required`)"),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody)
    )
)]
async fn seek_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<SeekRequest>,
) -> Result<Json<SeekResponse>, ApiError> {
    // Before the seek, not inside it: `Sessions::seek` reports a missing
    // session as an ordinary error, which lands as the same 409 a real
    // seek failure does. A client cannot recover from an ambiguous
    // status, so the one case it CAN act on gets answered first.
    if state.sessions.get(&id).is_none() {
        return Err(session_gone());
    }
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
            // Every failed seek tells its story here, not just the ones the
            // fallback retried — a refusal must never be untraceable. This
            // line carries what only the request knows; the chain is
            // `session_refusal`'s, at warn for a dead item and at debug for
            // the two self-clearing states, whose causes are polled. Logging
            // `{e:#}` here as well wrote every failed seek out twice.
            // The CODE at warn, the chain at debug (inside `session_refusal`).
            // Without it a refused seek logged a position and nothing else,
            // which is the untraceable refusal this comment forbids.
            let refusal = session_refusal(e);
            tracing::warn!(session = %id, position_ms = body.position_ms,
                audio_track = ?body.audio_track, video_track = ?body.video_track,
                code = ?refusal.code(), "seek failed");
            refusal
        })?;
    // A track switch re-planned: hand back the verdicts of what plays
    // NOW so the overlay never lies about the current streams.
    let session = state.sessions.get(&id);
    let streams = session.as_ref().and_then(|session| {
        session
            .verdict
            .lock()
            .unwrap()
            .as_ref()
            .map(|(video, audio)| PlaybackStreams {
                cost: session.delivery_cost(),
                video: video.clone(),
                audio: audio.clone(),
                subtitles: session.sub_verdicts.lock().unwrap().clone(),
            })
    });
    Ok(Json(SeekResponse {
        part_base_ms,
        streams,
    }))
}

/// End a playback session
///
/// Stops the session and releases its resources, responding 204. An unknown
/// or already-ended session returns 404.
#[utoipa::path(
    delete, path = "/api/v1/playback/sessions/{id}", tag = "Playback",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 204, description = "Session ended"),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn end_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.sessions.end(&id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(session_gone())
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

/// Stream a direct-play session
///
/// Serves the session's media bytes with byte-range support, answering 206
/// for a range and 416 when the range is unsatisfiable. Only direct-play
/// sessions serve here; other modes return 409.
#[utoipa::path(
    get, path = "/api/v1/playback/sessions/{id}/stream", tag = "Playback media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(
        ("id" = String, Path),
        ("range" = Option<String>, Header)
    ),
    responses(
        (status = 200, body = Vec<u8>, content_type = "application/octet-stream", headers(("accept-ranges" = String), ("content-length" = u64))),
        (status = 206, body = Vec<u8>, content_type = "application/octet-stream", headers(("accept-ranges" = String), ("content-length" = u64), ("content-range" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 416, description = "Invalid or unsatisfiable byte range", headers(("content-range" = String))),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn stream_session(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let session = state.sessions.get(&id).ok_or_else(session_gone)?;
    session.touch();
    let crate::sessions::Mode::Direct { lease } = &session.mode else {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "not a direct-play session",
        ));
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

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct VttQuery {
    /// f64 so a client that computed a fractional shift still works.
    #[serde(default)]
    shift_ms: f64,
}

async fn segment_collections(state: &AppState) -> Result<SegmentStatusResponse, ApiError> {
    let hosts = state
        .registry
        .satellites_overview()
        .await
        .map_err(internal)?;
    let mut collections = Vec::new();
    for row in state
        .registry
        .catalogue()
        .collection_summaries()
        .await
        .map_err(internal)?
    {
        let c = row.collection;
        if c.media_type == kahawai_mediadb::MediaType::Music {
            continue;
        }
        let host = hosts.iter().find(|h| h.module_id == c.mediahost_id);
        let report = state
            .registry
            .discovery_status(&c.mediahost_id, &c.remote_id);
        collections.push(SegmentCollectionStatus {
            collection_id: c.id,
            mediahost_name: host
                .map(|h| h.name.clone())
                .unwrap_or_else(|| c.mediahost_id.clone()),
            connected: host.is_some_and(|h| h.connected),
            mediahost_id: c.mediahost_id,
            name: c.remote_id,
            media_type: c.media_type.as_str().to_string(),
            pending_sources: report.as_ref().map(|r| r.pending_segments),
            pending_loudness: report.as_ref().map(|r| r.pending_loudness),
            enabled: report.and_then(|r| r.segments_enabled),
        });
    }
    collections.sort_by(|a, b| {
        (&a.mediahost_name, &a.name, &a.collection_id).cmp(&(
            &b.mediahost_name,
            &b.name,
            &b.collection_id,
        ))
    });
    Ok(SegmentStatusResponse { collections })
}

/// Segment detection and loudness measurement status
///
/// Admin only. Reports source-level discovery status from current mediahost links.
#[utoipa::path(
    get, path = "/admin/v1/segments", tag = "Admin segments",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = SegmentStatusResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_segments_status(
    State(state): State<AppState>,
) -> Result<Json<SegmentStatusResponse>, ApiError> {
    Ok(Json(segment_collections(&state).await?))
}

/// An accidental start is not something to resume. Requiring both one minute
/// and one percent keeps a long film out after a brief probe without making a
/// short episode wait for a film-sized absolute cutoff.
const CONTINUE_MIN_POSITION_MS: i64 = 60_000;
const CONTINUE_MIN_RUNTIME_FRACTION: i64 = 100;

/// How long "recent" lasts for the up-next row: a month, taken as 30
/// days. Two independent things are measured against it — when this
/// account last finished an episode of a series, and when the episode it
/// would watch next was added — and either one alone keeps the series in
/// the row.
const UP_NEXT_WINDOW_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Serialize, ToSchema)]
struct ItemSource {
    /// Stable physical rendition identity in mediadb, independent of response ordering.
    #[serde(skip_serializing_if = "Option::is_none")]
    media_entry_id: Option<String>,
    /// Assignment owner shared by every file part of this source.
    collection_item_id: String,
    module_id: String,
    host_name: Option<String>,
    collection_id: String,
    /// Collection-relative media path. This is intentional client data: the
    /// release name often carries rendition facts that discovery cannot
    /// normalize (edition, source, codec, group, or revision). It never
    /// contains the configured root or an absolute host path (SEC-WEB-7).
    path_rel: String,
    size: i64,
    available: bool,
    revision: i64,
    /// UI-27. Which playable source this file belongs to, which part of it
    /// this is, and how many parts that source has.
    ///
    /// The list is one row per FILE, ordered by what playback would pick. That
    /// made one film split across seven numbered parts indistinguishable from
    /// seven alternative encodes — both are "7 sources" in an order that means
    /// nothing to a reader, and no amount of UI work fixes it from the client
    /// side because the grouping was not in the response.
    ///
    /// `source_id` is opaque and only stable within one response; it exists to
    /// be grouped on, not stored. Rows sharing it are parts of one work, in
    /// `part` order; rows with different ones are alternatives to choose
    /// between.
    source_id: i64,
    part: i64,
    parts: i64,
    /// Outer `None` omits streams from GET; inner `None` preserves a
    /// malformed legacy stream record as JSON null on QUERY.
    #[serde(skip_serializing_if = "Option::is_none")]
    streams: Option<Option<ClientMediaInfo>>,
    /// The complete catalogue record is needed to fold chapters after QUERY
    /// chooses a different rendition. It is never an API field.
    #[serde(skip_serializing)]
    #[schema(ignore)]
    catalog_info: Option<kahawai_core::media::MediaInfo>,
}

/// The technical facts a playback client can act on.
///
/// `MediaInfo` is a source-owned catalogue record and is intentionally not an
/// API type: besides these facts it contains sidecar, artwork and NFO paths,
/// attachment byte ranges, container tags and a terminal probe error. Passing
/// it through made an authenticated viewer a catalogue-debugging endpoint.
#[derive(Serialize, ToSchema)]
struct ClientMediaInfo {
    #[schema(required)]
    container: Option<String>,
    #[schema(required)]
    duration_ms: Option<u64>,
    video: Vec<ClientVideoStream>,
    audio: Vec<ClientAudioStream>,
    /// Embedded and sidecar tracks have the same client-visible facts. A
    /// sidecar's path is deliberately discarded while its format/language is
    /// retained.
    subtitles: Vec<ClientSubtitleStream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_gain: Option<ClientReplayGain>,
}

/// The explicit client projection continues through every nested value.
/// Reusing the catalogue structs here would make a future source-only field an
/// API field merely by adding it to `VideoStream`, `AudioStream`, or
/// `ReplayGain`.
#[derive(Serialize, ToSchema)]
struct ClientVideoStream {
    codec: String,
    width: u32,
    height: u32,
    #[schema(required)]
    fps: Option<(u32, u32)>,
    #[schema(required)]
    bit_depth: Option<u32>,
    interlaced: bool,
    #[schema(required)]
    hdr: Option<String>,
    #[schema(required)]
    profile: Option<String>,
    #[schema(required)]
    level: Option<String>,
    #[schema(required)]
    bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_keyframe_interval_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pixel_aspect_ratio: Option<(u32, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    orientation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_height: Option<u32>,
}

impl From<kahawai_core::media::VideoStream> for ClientVideoStream {
    fn from(stream: kahawai_core::media::VideoStream) -> Self {
        Self {
            codec: stream.codec,
            width: stream.width,
            height: stream.height,
            fps: stream.fps,
            bit_depth: stream.bit_depth,
            interlaced: stream.interlaced,
            hdr: stream.hdr,
            profile: stream.profile,
            level: stream.level,
            bitrate_kbps: stream.bitrate_kbps,
            max_keyframe_interval_ms: stream.max_keyframe_interval_ms,
            pixel_aspect_ratio: stream.pixel_aspect_ratio,
            orientation: stream.orientation,
            display_width: stream.display_width,
            display_height: stream.display_height,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ClientAudioStream {
    codec: String,
    channels: u32,
    sample_rate: u32,
    #[schema(required)]
    language: Option<String>,
    #[schema(required)]
    bitrate_kbps: Option<u32>,
    #[schema(required)]
    layout: Option<String>,
}

impl From<kahawai_core::media::AudioStream> for ClientAudioStream {
    fn from(stream: kahawai_core::media::AudioStream) -> Self {
        Self {
            codec: stream.codec,
            channels: stream.channels,
            sample_rate: stream.sample_rate,
            language: stream.language,
            bitrate_kbps: stream.bitrate_kbps,
            layout: stream.layout,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ClientSubtitleStream {
    format: String,
    #[schema(required)]
    language: Option<String>,
}

impl From<kahawai_core::media::SubtitleStream> for ClientSubtitleStream {
    fn from(stream: kahawai_core::media::SubtitleStream) -> Self {
        Self {
            format: stream.format,
            language: stream.language,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ClientReplayGain {
    #[serde(skip_serializing_if = "Option::is_none")]
    track_gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_level_db: Option<f64>,
}

impl From<kahawai_core::media::ReplayGain> for ClientReplayGain {
    fn from(gain: kahawai_core::media::ReplayGain) -> Self {
        Self {
            track_gain_db: gain.track_gain_db,
            track_peak: gain.track_peak,
            album_gain_db: gain.album_gain_db,
            album_peak: gain.album_peak,
            reference_level_db: gain.reference_level_db,
        }
    }
}

impl From<kahawai_core::media::MediaInfo> for ClientMediaInfo {
    fn from(info: kahawai_core::media::MediaInfo) -> Self {
        let mut subtitles: Vec<ClientSubtitleStream> =
            info.subtitles.into_iter().map(Into::into).collect();
        subtitles.extend(
            info.external_subtitles
                .into_iter()
                .map(|track| ClientSubtitleStream {
                    format: track.format,
                    language: track.language,
                }),
        );
        Self {
            container: info.container,
            duration_ms: info.duration_ms,
            video: info.video.into_iter().map(Into::into).collect(),
            audio: info.audio.into_iter().map(Into::into).collect(),
            subtitles,
            replay_gain: info.replay_gain.map(Into::into),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ItemQueryResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle_source: Option<catalogue::subtitles::SubtitleSource>,
    #[schema(required)]
    negotiated: Option<NegotiatedItem>,
    /// Skip markers on the negotiated physical rendition's timeline. Empty
    /// without a selected source or usable observations. The accepted session
    /// supplies the authoritative list if selection changes between requests.
    segments: Vec<crate::segments::Segment>,
    /// Why the converged half is null. Not an error — the item loaded, and
    /// its page must render — but the same distinction as an error carries,
    /// in the same shape: `source_offline` comes back once the host does,
    /// `unplayable` does not. It used to be a bare string holding
    /// `format!("{e:#}")`, so the detail page had a pipeline's chain to
    /// print and no way to tell a wait from a dead end.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<ApiErrorBody>,
}

#[derive(Serialize, ToSchema)]
struct NegotiatedItem {
    #[schema(required)]
    source: Option<NegotiatedSource>,
    mode: String,
    cost: String,
    target_duration_secs: u32,
    streams: NegotiatedStreams,
    subtitles: Vec<crate::subtitles::TrackListing>,
}

#[derive(Serialize, ToSchema)]
struct NegotiatedSource {
    /// Opaque source-group identity shared with `ItemSource::source_id`.
    /// Clients may correlate on this without treating a path as identity.
    source_id: i64,
    module_id: String,
    collection_id: String,
    /// The collection-relative media path of the selected part. Exposing this
    /// is deliberate; absolute roots and catalogue-internal paths stay private.
    path_rel: String,
    #[schema(required)]
    display_width: Option<u32>,
    #[schema(required)]
    display_height: Option<u32>,
    #[schema(required)]
    orientation: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct NegotiatedStreams {
    video: String,
    audio: String,
    subtitles: Vec<kahawai_media::negotiate::SubtitleVerdict>,
}

/// One source's parts, in ordinal order, folded onto the item's timeline.
/// `None` (an unparseable record) ends the fold: past it every offset would
/// be wrong, and a chapter mark in the wrong place is worse than none.
fn group_chapters(
    parts: impl Iterator<Item = Option<kahawai_core::media::MediaInfo>>,
) -> Vec<kahawai_core::media::Chapter> {
    let mut out = Vec::new();
    let mut offset_ms = 0u64;
    let mut parts = parts.peekable();
    while let Some(info) = parts.next() {
        let Some(info) = info else {
            break;
        };
        // The part's length is the fold's clock. Zero is a probe that
        // failed, not a length, and without a real length the parts after
        // this one cannot be placed.
        let duration = info.duration_ms.filter(|ms| *ms > 0);
        let last = parts.peek().is_none();
        out.extend(
            info.chapters
                .iter()
                .flatten()
                // A chapter stamped at or past its own part's end is the
                // author's mistake; offset, it would claim a timestamp in
                // the NEXT part's stretch of the timeline. The final part
                // has no next part to trespass on.
                .filter(|c| last || duration.is_none_or(|ms| c.start_ms < ms))
                .filter_map(|c| {
                    Some(kahawai_core::media::Chapter {
                        start_ms: c.start_ms.checked_add(offset_ms)?,
                        // Clamped like the starts are filtered: a stated end
                        // past the part's own length is the same authoring
                        // mistake, and offset unclamped it claimed a span
                        // inside the NEXT part's stretch of the timeline.
                        end_ms: c.end_ms.and_then(|e| e.checked_add(offset_ms)).map(|e| {
                            match duration.and_then(|d| offset_ms.checked_add(d)) {
                                Some(part_end) if !last => e.min(part_end),
                                _ => e,
                            }
                        }),
                        title: c.title.clone(),
                    })
                }),
        );
        match duration.and_then(|ms| offset_ms.checked_add(ms)) {
            Some(next) => offset_ms = next,
            None => break,
        }
    }
    // Part boundaries can land two chapters on one timestamp (part N's tail
    // chapter at exactly its duration, part N+1's opener at 0); the readers
    // dedup within one file, this dedups across the fold.
    out.sort_by_key(|c| c.start_ms);
    out.dedup_by_key(|c| c.start_ms);
    out
}

/// The catalogue detail query. Same inputs a
/// session start takes, minus everything that only matters once you
/// are actually playing.
#[derive(Deserialize, Default, ToSchema)]
struct ItemQuery {
    /// Stable mediadb rendition identity; preferred over response-local source numbers.
    media_entry_id: Option<String>,
    source_id: Option<i64>,
    /// Absent = the conservative fallback, exactly as `start_session`
    /// treats a missing profile.
    #[serde(default)]
    profile: Option<kahawai_core::media::CapabilityProfile>,
    #[serde(default)]
    audio_track: u32,
    /// Per-rendition audio preferences for automatic ranking: source ID to
    /// audio stream index. Missing sources use audio_track. START still takes
    /// the chosen source and its resolved index, never a cross-source index.
    #[serde(default)]
    #[schema(value_type = std::collections::BTreeMap<String, u32>)]
    source_audio_tracks: std::collections::BTreeMap<i64, u32>,
    #[serde(default)]
    video_track: u32,
    #[serde(default)]
    subtitle_track: Option<i64>,
    /// Operator override (scripts, pipeline debugging).
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct ProgressRequest {
    position_ms: u64,
}

/// Report playback progress
///
/// Stores the resume position, keeps the session alive and paces the
/// pipeline, marking the item played past 90 percent. An unknown or expired
/// session answers 404.
///
/// `played` is a boolean and not a high-water mark: it is what the last
/// report said — bar one at position zero, which says nothing and leaves
/// it alone — so an item watched again stops being played as soon as its
/// playhead has moved at all. What makes that happen
/// without any client knowing the rule is the other half — a played item
/// is served with no `resume_position_ms` (see [`item_row`]), so the next
/// `Play` on it begins at the beginning.
#[utoipa::path(
    post, path = "/api/v1/playback/sessions/{id}/progress", tag = "Playback",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = ProgressRequest,
    responses(
        (status = 200, body = ProgressResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn post_progress(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<ProgressRequest>,
) -> Result<Json<ProgressResponse>, ApiError> {
    let session = state.sessions.get(&id).ok_or_else(session_gone)?;
    let _report_guard = session.begin_report().await.ok_or_else(session_gone)?;
    session.touch();
    // Pacing (§4.6): the worker throttles its lead over this position.
    state
        .sessions
        .viewer_position(&state.registry, &id, body.position_ms);

    {
        let captured = &session.catalogue;
        let finished = session
            .duration_ms
            .is_some_and(|d| d > 0 && u128::from(body.position_ms) * 10 >= u128::from(d) * 9);
        let ids = if finished {
            session.library_item_ids.clone()
        } else {
            vec![session.item_id.clone()]
        };
        let reports = ids
            .into_iter()
            .map(|id| crate::watch::Progress {
                id,
                parent: captured.parent_id.clone(),
                position: body.position_ms,
                duration: session.duration_ms,
                track: captured.track,
            })
            .collect::<Vec<_>>();
        crate::watch::progress(state.registry.db(), &claims.sub, &reports)
            .await
            .map_err(internal)?;
        session
            .last_position_ms
            .store(body.position_ms, std::sync::atomic::Ordering::Relaxed);
        let played = crate::watch::read(
            state.registry.db(),
            &claims.sub,
            std::slice::from_ref(&session.item_id),
        )
        .await
        .map_err(internal)?
        .remove(&session.item_id)
        .is_some_and(|w| w.played);
        Ok(Json(ProgressResponse {
            position_ms: body.position_ms,
            played,
        }))
    }
}

/// The most items one mark may touch. A season is tens and a show is
/// hundreds; past this the caller is doing something other than ticking
/// off what it just listed.
const WATCHED_BATCH_MAX: usize = crate::watch::MAX_BATCH_ITEMS;

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
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid file name"));
    }
    let bytes = state
        .sessions
        .fetch_artifact(&state.registry, session, file)
        .await
        // `debug`, because this path is POLLED: the player asks for
        // `start.pos` up to three times per seek precisely because it is often
        // not written yet, and hls.js probes segments the same way. At the
        // default filter (`info`) these lines are not emitted at all, and that
        // is the trade — a dropped link is already `satellite disconnected` at
        // info from the registry, so an operator is not left guessing; this
        // adds which request noticed, for somebody who has turned debug on.
        .map_err(|e| {
            tracing::debug!(session = %session.id, file = %file, error = format!("{e:#}"), "artifact not served");
            ApiError::new(ErrorCode::NotFound, "no such file in this session")
        })?;
    let bytes = if file.ends_with(".m3u8") {
        declare_target_duration(bytes, session.target_duration_secs)
    } else {
        bytes
    };
    let ctype = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file == "start.pos" {
        "text/plain"
    } else if file.ends_with(".m4s") || file.ends_with(".mp4") {
        // HUB-15b fMP4 path: init.mp4 + segment%05d.m4s.
        "video/mp4"
    } else {
        "video/mp2t"
    };
    Ok((
        [(axum::http::header::CONTENT_TYPE, ctype)],
        axum::body::Bytes::from(bytes),
    )
        .into_response())
}

/// Stamp the session's decided `EXT-X-TARGETDURATION` onto a playlist
/// as it is served.
///
/// The sinks cannot do this themselves. hlssink3's `target-duration`
/// property is the FRAGMENT interval it cuts on *and* the value it
/// writes, so raising it to declare honestly would also make it pack
/// longer fragments and overshoot again; the two numbers have to come
/// apart, and this is where. It also covers playlists produced on a
/// transcoder, which the hub only ever sees as bytes.
///
/// The value is fixed at session start, so every client sees one
/// value for the session's life — §6.2.1 forbids it changing, and
/// rewriting per-request would violate that even while looking like a
/// fix.
fn declare_target_duration(bytes: Vec<u8>, secs: u32) -> Vec<u8> {
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let out: String = text
        .lines()
        .map(|line| {
            if line.starts_with("#EXT-X-TARGETDURATION:") {
                format!("#EXT-X-TARGETDURATION:{secs}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    out.into_bytes()
}

/// Fetch a session artifact
///
/// Serves a session's playlist, media segments and subtitle files, proxying
/// from the transcoder for dispatched sessions. Accepts a bearer token or the
/// media cookie; live subtitle files are followed until the session ends.
#[utoipa::path(
    get, path = "/api/v1/playback/sessions/{id}/{file}", tag = "Playback media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(
        ("id" = String, Path),
        ("file" = String, Path)
    ),
    responses(
        (status = 200, content((Vec<u8> = "application/vnd.apple.mpegurl"), (Vec<u8> = "video/mp4"), (Vec<u8> = "video/mp2t"), (Vec<u8> = "text/plain"), (Vec<u8> = "text/x-ssa"), (Vec<u8> = "application/x-ndjson"))),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn session_file(
    State(state): State<AppState>,
    ApiPath((id, file)): ApiPath<(String, String)>,
) -> Result<Response, ApiError> {
    let session = state.sessions.get(&id).ok_or_else(session_gone)?;
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
            return Err(ApiError::new(ErrorCode::BadRequest, "invalid file name"));
        }
        // The public keyspace is the track id; the pipeline writes
        // internal stream-index names (subs-e{n}.*). Translate here —
        // only embedded tracks are in the pipeline, so only they tap.
        let file = match file[5..].split_once('.') {
            Some((num, ext)) if num.chars().all(|c| c.is_ascii_digit()) => {
                let track = { session.catalogue_track(num.parse().map_err(|_| hidden("track"))?) }
                    .filter(|t| t.origin == "embedded")
                    .ok_or(ApiError::new(ErrorCode::NotFound, "no such embedded track"))?;
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
            return Err(ApiError::new(ErrorCode::NotFound, "not a remux session"));
        }
    };
    let dir = &dir;
    let valid = !file.starts_with('.')
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(ApiError::new(ErrorCode::BadRequest, "invalid file name"));
    }
    let ctype = if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file.ends_with(".ts") {
        "video/mp2t"
    } else if file.ends_with(".m4s") || file.ends_with(".mp4") {
        // HUB-15b fMP4 path: init.mp4 + segment%05d.m4s.
        "video/mp4"
    } else if file == "start.pos" {
        // True playlist origin after keyframe snapping (§6): players
        // align subtitles and the seekbar to it.
        "text/plain"
    } else {
        return Err(ApiError::new(ErrorCode::NotFound, "unknown file type"));
    };
    let bytes = tokio::fs::read(dir.join(&file))
        .await
        .map_err(|_| ApiError::new(ErrorCode::NotFound, "no such file"))?;
    let bytes = if file.ends_with(".m3u8") {
        declare_target_duration(bytes, session.target_duration_secs)
    } else {
        bytes
    };
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", ctype)
        .header("cache-control", "no-store")
        .body(axum::body::Body::from(bytes))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorCode, PublicOrigin, group_chapters, openapi_document, parse_range,
        refusal_or_internal, same_fields,
    };
    use std::collections::BTreeMap;

    #[test]
    fn catalogue_description_schema_uses_mediadb_types() {
        let document = serde_json::to_value(openapi_document()).unwrap();
        let schemas = &document["components"]["schemas"];
        assert_eq!(
            schemas["CatalogueItem"]["allOf"][1]["properties"]["metadata"]["$ref"],
            "#/components/schemas/ResolvedDescription"
        );
        assert_eq!(
            schemas["ResolvedDescription"]["properties"]["description"]["$ref"],
            "#/components/schemas/Description"
        );
    }

    #[test]
    fn source_audio_track_schema_accepts_json_property_names() {
        let document = serde_json::to_value(openapi_document()).unwrap();
        let schema =
            &document["components"]["schemas"]["ItemQuery"]["properties"]["source_audio_tracks"];
        assert_eq!(schema["type"], "object");
        assert!(
            schema.get("propertyNames").is_none() || schema["propertyNames"]["type"] == "string",
            "JSON object keys are strings, including source IDs: {schema}"
        );
        let request: super::ItemQuery = serde_json::from_value(serde_json::json!({
            "source_audio_tracks": {"1":1,"2":0}
        }))
        .unwrap();
        assert_eq!(
            request.source_audio_tracks,
            BTreeMap::from([(1, 1), (2, 0)])
        );
    }

    #[test]
    fn credential_fields_change_only_when_the_plaintext_set_differs() {
        let current = BTreeMap::from([
            ("api_key".to_string(), "key".to_string()),
            ("pin".to_string(), "1234".to_string()),
        ]);

        assert!(same_fields(
            &current,
            &BTreeMap::from([("api_key", "key"), ("pin", "1234")])
        ));
        assert!(!same_fields(
            &current,
            &BTreeMap::from([("api_key", "rotated"), ("pin", "1234")])
        ));
        assert!(!same_fields(
            &current,
            &BTreeMap::from([("api_key", "key"), ("pin", "1234"), ("extra", "field"),])
        ));
        assert!(!same_fields(
            &current,
            &BTreeMap::from([("api_key", "key")])
        ));
    }

    /// The subtitle routes hand every provider failure to this classifier, so
    /// it is the one place that decides whether a viewer is told OpenSubtitles
    /// is down. A credential this hub cannot decrypt is not.
    #[test]
    fn a_credential_that_will_not_open_is_ours_not_the_providers() {
        let ours = anyhow::Error::new(crate::secrets::UnreadableCredential)
            .context("stored opensubtitles password");
        assert_eq!(
            refusal_or_internal(
                ErrorCode::ProviderError,
                "the provider did not answer",
                ours
            )
            .code(),
            ErrorCode::Internal
        );
        // The control: a plain refusal from the provider still reads as one,
        // so the assertion above is about the type and not about the helper
        // having stopped classifying anything.
        assert_eq!(
            refusal_or_internal(
                ErrorCode::ProviderError,
                "the provider did not answer",
                anyhow::anyhow!("opensubtitles returned 503"),
            )
            .code(),
            ErrorCode::ProviderError
        );
    }

    /// A CD1 whose author stamped a chapter at (or past) the disc's own end
    /// must not claim a boundary in CD2's stretch of the timeline — and a
    /// part whose probe reported zero duration cannot place the parts after
    /// it at all. The final part keeps its overhang: there is nothing after
    /// it to trespass on.
    #[test]
    fn a_part_fold_keeps_chapters_inside_their_part() {
        use kahawai_core::media::{Chapter, MediaInfo};
        let part = |duration_ms: Option<u64>, chapters: Vec<(u64, &str)>| MediaInfo {
            duration_ms,
            chapters: Some(
                chapters
                    .into_iter()
                    .map(|(start_ms, title)| Chapter {
                        start_ms,
                        end_ms: None,
                        title: Some(title.into()),
                    })
                    .collect(),
            ),
            ..Default::default()
        };

        // CD1 is 10 s; its "end" chapter at 10 s would land on CD2's opener.
        let folded = group_chapters(
            [
                Some(part(Some(10_000), vec![(0, "one"), (10_000, "stray")])),
                Some(part(Some(10_000), vec![(0, "two"), (12_000, "tail")])),
            ]
            .into_iter(),
        );
        let titles: Vec<_> = folded.iter().filter_map(|c| c.title.as_deref()).collect();
        assert_eq!(
            titles,
            ["one", "two", "tail"],
            "the stray is dropped, the final tail kept"
        );
        assert_eq!(folded[1].start_ms, 10_000);

        // A zero-duration probe stops the fold before it misplaces CD2.
        let folded = group_chapters(
            [
                Some(part(Some(0), vec![(0, "one")])),
                Some(part(Some(10_000), vec![(0, "two")])),
            ]
            .into_iter(),
        );
        let titles: Vec<_> = folded.iter().filter_map(|c| c.title.as_deref()).collect();
        assert_eq!(titles, ["one"], "an unplaceable second part is left out");

        // A STATED end past its own part is the same authoring mistake as a
        // stray start: clamped to the part's end on non-final parts, kept on
        // the final one (nothing after it to trespass on).
        let stated = |start_ms: u64, end_ms: u64, title: &str| Chapter {
            start_ms,
            end_ms: Some(end_ms),
            title: Some(title.into()),
        };
        let folded = group_chapters(
            [
                Some(MediaInfo {
                    duration_ms: Some(10_000),
                    chapters: Some(vec![stated(5_000, 14_000, "overhang")]),
                    ..Default::default()
                }),
                Some(MediaInfo {
                    duration_ms: Some(10_000),
                    chapters: Some(vec![stated(2_000, 25_000, "last")]),
                    ..Default::default()
                }),
            ]
            .into_iter(),
        );
        assert_eq!(
            folded[0].end_ms,
            Some(10_000),
            "clamped to CD1's end, not 4 s into CD2"
        );
        assert_eq!(
            folded[1].end_ms,
            Some(35_000),
            "the final part keeps its stated end"
        );
    }

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

    #[test]
    fn public_origin_normalizes_and_rejects_non_origins() {
        assert_eq!(
            PublicOrigin::parse("HTTPS://Example.COM:443")
                .unwrap()
                .as_str(),
            "https://example.com"
        );
        assert_eq!(
            PublicOrigin::parse("http://Example.COM:8420/")
                .unwrap()
                .as_str(),
            "http://example.com:8420"
        );
        assert!(PublicOrigin::parse("https://example.com").unwrap().secure());
        assert!(!PublicOrigin::parse("http://example.com").unwrap().secure());
        for invalid in [
            "example.com",
            "ftp://example.com",
            "https://user@example.com",
            "https://example.com/app",
            "https://example.com/?q=1",
            "https://example.com/#fragment",
        ] {
            assert!(PublicOrigin::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn checked_in_openapi_matches_generated_document() {
        let mut committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../web/openapi.json")).unwrap();
        let fingerprint = committed
            .as_object_mut()
            .unwrap()
            .remove("x-kahawai-source-sha256")
            .expect("openapi.json has a source fingerprint");
        assert_eq!(fingerprint.as_str().map(str::len), Some(64));
        assert_eq!(
            committed,
            serde_json::to_value(openapi_document()).unwrap(),
            "web/openapi.json is stale; run `npm --prefix web run api:export`"
        );
    }

    #[test]
    fn openapi_covers_exact_application_surface_with_typed_bodies() {
        use std::collections::BTreeSet;

        let document = serde_json::to_value(openapi_document()).unwrap();
        let expected = [
            ("get", "/admin/v1/catalogue/collections"),
            ("post", "/admin/v1/catalogue/libraries"),
            ("delete", "/admin/v1/catalogue/libraries/{id}"),
            ("put", "/admin/v1/catalogue/libraries/{id}/collections"),
            ("post", "/admin/v1/catalogue/libraries/{id}/refresh"),
            ("get", "/api/v1/catalogue/libraries"),
            ("get", "/api/v1/catalogue/continue-watching"),
            ("get", "/api/v1/catalogue/up-next"),
            ("get", "/api/v1/catalogue/libraries/{id}/items"),
            ("get", "/api/v1/catalogue/libraries/{id}/items/{item_id}"),
            (
                "get",
                "/api/v1/catalogue/libraries/{id}/items/{item_id}/children",
            ),
            (
                "put",
                "/api/v1/catalogue/libraries/{id}/items/{item_id}/watched",
            ),
            ("get", "/api/v1/catalogue/libraries/{id}/artists"),
            (
                "get",
                "/api/v1/catalogue/libraries/{id}/items/{item_id}/artwork",
            ),
            (
                "get",
                "/api/v1/catalogue/libraries/{id}/artists/{key}/artwork",
            ),
            ("get", "/health"),
            ("get", "/metrics"),
            ("get", "/api/v1/bootstrap"),
            ("post", "/api/v1/setup"),
            ("post", "/api/v1/auth/token"),
            ("post", "/api/v1/auth/refresh"),
            ("post", "/api/v1/auth/logout"),
            ("get", "/api/v1/events"),
            (
                "post",
                "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/search",
            ),
            (
                "post",
                "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/download",
            ),
            (
                "delete",
                "/api/v1/catalogue/libraries/{library}/items/{item}/subtitles/{track}",
            ),
            ("get", "/api/v1/prefs"),
            ("put", "/api/v1/prefs"),
            ("get", "/api/v1/account/opensubtitles"),
            ("post", "/api/v1/account/opensubtitles"),
            ("delete", "/api/v1/account/opensubtitles"),
            ("post", "/api/v1/playback/sessions"),
            ("delete", "/api/v1/playback/sessions/{id}"),
            ("post", "/api/v1/playback/sessions/{id}/progress"),
            ("post", "/api/v1/playback/sessions/{id}/seek"),
            ("query", "/api/v1/catalogue/libraries/{id}/items/{item_id}"),
            (
                "get",
                "/api/v1/catalogue/libraries/{library}/items/{item}/next",
            ),
            ("get", "/api/v1/playback/sessions/{id}/fonts"),
            ("get", "/api/v1/playback/sessions/{id}/fonts/{n}"),
            ("get", "/api/v1/playback/sessions/{id}/subtitles/{file}"),
            ("get", "/api/v1/playback/sessions/{id}/stream"),
            ("get", "/api/v1/playback/sessions/{id}/{file}"),
            ("get", "/admin/v1/enrollments"),
            ("post", "/admin/v1/enrollments/approve"),
            ("get", "/admin/v1/satellites"),
            ("delete", "/admin/v1/satellites/{id}"),
            ("post", "/admin/v1/satellites/{id}/disabled"),
            ("get", "/admin/v1/users"),
            ("post", "/admin/v1/users"),
            ("delete", "/admin/v1/users/{id}"),
            ("put", "/admin/v1/users/{id}/libraries"),
            ("put", "/admin/v1/users/{id}/admin"),
            ("get", "/admin/v1/providers"),
            ("post", "/admin/v1/providers/chains/{media_type}"),
            ("post", "/admin/v1/providers/tmdb"),
            ("post", "/admin/v1/providers/tvdb"),
            ("post", "/admin/v1/providers/anidb"),
            ("post", "/admin/v1/providers/anidb/verify"),
            ("post", "/admin/v1/providers/fanart"),
            ("post", "/admin/v1/providers/theaudiodb"),
            ("delete", "/admin/v1/providers/{provider}/credentials"),
            ("get", "/admin/v1/enrich/items"),
            ("get", "/admin/v1/enrich/items/{id}"),
            ("get", "/api/v1/catalogue/collection-items/{id}/artwork"),
            ("get", "/admin/v1/enrich/items/{id}/identities"),
            ("get", "/admin/v1/enrich/items/{id}/artist-artwork"),
            ("get", "/admin/v1/enrich/progress"),
            ("post", "/admin/v1/enrich/items/{id}/match"),
            ("post", "/admin/v1/enrich/items/{id}/candidates"),
            ("get", "/admin/v1/enrich"),
            ("post", "/admin/v1/enrich"),
            ("get", "/admin/v1/sessions"),
            ("delete", "/admin/v1/sessions/{id}"),
            ("get", "/admin/v1/sessions/{id}/log"),
            ("get", "/admin/v1/items/{id}/log"),
            ("get", "/admin/v1/segments"),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let methods = [
            "get", "put", "post", "delete", "options", "head", "patch", "trace", "query",
        ];
        let actual = document["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, item)| {
                methods
                    .into_iter()
                    .filter(|method| item.get(*method).is_some())
                    .map(|method| (method, path.as_str()))
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
        assert_eq!(document["openapi"], "3.2.0");
        assert!(document["paths"].get("/api/v1/items/{id}").is_none());
        let client_media = &document["components"]["schemas"]["ClientMediaInfo"];
        for (field, schema) in [
            ("video", "ClientVideoStream"),
            ("audio", "ClientAudioStream"),
            ("subtitles", "ClientSubtitleStream"),
        ] {
            assert_eq!(
                client_media["properties"][field]["items"]["$ref"],
                format!("#/components/schemas/{schema}"),
                "{field} fell back to a source-owned catalogue schema"
            );
        }
        assert!(
            client_media["properties"]["replay_gain"]
                .to_string()
                .contains("#/components/schemas/ClientReplayGain"),
            "loudness fell back to a source-owned catalogue schema"
        );
        assert_eq!(
            document["paths"]["/health"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/HealthResponse"
        );
        assert_eq!(
            document["components"]["securitySchemes"]["bearer_auth"]["scheme"],
            "bearer"
        );
        assert_eq!(
            document["components"]["securitySchemes"]["media_token"]["in"],
            "query"
        );
        assert_eq!(
            document["components"]["securitySchemes"]["media_token"]["name"],
            "token"
        );
        assert!(
            document["components"]["schemas"].get("Value").is_none(),
            "generic serde_json::Value leaked into the contract"
        );
        let bootstrap_required = document["components"]["schemas"]["BootstrapResponse"]["required"]
            .as_array()
            .expect("BootstrapResponse has a required field list");
        assert!(
            bootstrap_required.iter().any(|field| field == "setup_url"),
            "setup_url is always present and nullable: {}",
            document["components"]["schemas"]["BootstrapResponse"]
        );
        assert!(
            document["components"]["schemas"]["BootstrapResponse"]["properties"]
                .get("authenticated")
                .is_none(),
            "bootstrap no longer inspects credentials"
        );
        let login = &document["paths"]["/api/v1/auth/token"]["post"];
        assert!(
            login["parameters"].as_array().is_some_and(|parameters| {
                parameters
                    .iter()
                    .any(|parameter| parameter["name"] == "Origin")
            }),
            "browser login must document its Origin boundary"
        );
        assert!(
            login["responses"].get("403").is_some(),
            "browser login must document foreign-Origin rejection"
        );
        fn schema_requires(schema: &serde_json::Value, field: &str) -> bool {
            schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == field))
                || schema.as_object().is_some_and(|object| {
                    object.values().any(|value| schema_requires(value, field))
                })
                || schema
                    .as_array()
                    .is_some_and(|array| array.iter().any(|value| schema_requires(value, field)))
        }
        let is_required = |schema: &str, field: &str| {
            schema_requires(&document["components"]["schemas"][schema], field)
        };
        let item_source = &document["components"]["schemas"]["ItemSource"];
        let item_source_required = item_source["required"]
            .as_array()
            .expect("detail source fields have a required list");
        assert!(item_source_required.iter().any(|field| field == "path_rel"));
        assert!(!item_source_required.iter().any(|field| field == "streams"));
        assert!(is_required("NegotiatedSource", "path_rel"));
        assert!(is_required("ItemQueryResult", "negotiated"));
        assert!(!is_required("ItemQueryResult", "unavailable"));
        assert!(!is_required("VerificationResponse", "error"));
        for field in ["stream_index", "language", "label", "derived_from"] {
            assert!(is_required("Track", field), "Track.{field}");
        }
        assert!(
            document["components"]["schemas"]["Track"]["properties"]
                .get("source_id")
                .is_none(),
            "serde-skipped Track internals are not part of the API"
        );
        for (method, path) in expected {
            let operation = &document["paths"][path][method];
            assert!(
                operation["responses"].as_object().is_some(),
                "{method} {path}"
            );
            assert!(
                operation["tags"]
                    .as_array()
                    .is_some_and(|tags| !tags.is_empty()),
                "{method} {path} has no tag"
            );
            for response in operation["responses"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(_, response)| response)
            {
                assert!(
                    response["headers"].get("X-Request-Id").is_some(),
                    "{method} {path} response omits X-Request-Id"
                );
                if let Some(schema) = response["content"]["application/json"]["schema"].as_object()
                {
                    assert!(
                        schema.contains_key("$ref")
                            || schema.contains_key("type")
                            || schema
                                .get("oneOf")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|variants| !variants.is_empty()
                                    && variants.iter().all(
                                        |v| v.get("$ref").is_some() || v.get("type").is_some()
                                    )),
                        "{method} {path} has a generic JSON response schema: {schema:?}"
                    );
                }
            }
            if let Some(schema) =
                operation["requestBody"]["content"]["application/json"]["schema"].as_object()
            {
                assert!(
                    schema.contains_key("$ref")
                        || schema.contains_key("type")
                        || serde_json::to_string(schema).unwrap().contains("\"$ref\""),
                    "{method} {path} has a generic JSON request schema: {schema:?}"
                );
            }
            let public = matches!(
                (method, path),
                ("get", "/health")
                    | ("get", "/api/v1/bootstrap")
                    | ("post", "/api/v1/setup")
                    | ("post", "/api/v1/auth/token")
                    | ("post", "/api/v1/auth/refresh")
            );
            if public {
                assert!(operation.get("security").is_none(), "{method} {path}");
            } else if path == "/api/v1/candidate-artwork" {
                assert_eq!(
                    operation["security"][0]["candidate_artwork_ticket"],
                    serde_json::json!([])
                );
            } else if path == "/metrics" {
                assert_eq!(
                    operation["security"][0]["metrics_token"],
                    serde_json::json!([])
                );
            } else {
                assert!(
                    operation["security"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|requirement| requirement.get("bearer_auth").is_some()),
                    "{method} {path} does not declare bearer authentication"
                );
            }
        }
    }
}
