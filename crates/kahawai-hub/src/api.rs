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
mod enrichment;
mod library_api;
use library_api::item_body;

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
    pub segments: Arc<crate::segments::Detector>,
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
        list_collections,
        list_libraries,
        list_artists,
        artist_albums,
        artist_artwork,
        list_items,
        up_next,
        item_detail,
        item_query,
        item_children,
        item_set_watched,
        subtitle_search,
        subtitle_download,
        subtitle_delete,
        item_artwork,
        item_subtitle_file,
        item_fonts,
        item_font,
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
        admin_libraries,
        admin_create_library,
        admin_delete_library,
        admin_attach_collection,
        admin_detach_collection,
        admin_collections,
        admin_users,
        admin_create_user,
        admin_delete_user,
        admin_set_user_libraries,
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
        admin_refresh_library,
        admin_review_list,
        admin_review_search,
        candidate_artwork,
        admin_apply_match,
        admin_apply_legacy_match,
        admin_catalog_metadata,
        admin_sessions,
        admin_end_session,
        admin_session_log,
        admin_item_log,
        admin_segments_status,
        admin_segments_run
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
    let item = openapi
        .paths
        .paths
        .get_mut("/api/v1/items/{id}")
        .expect("item QUERY path is generated");
    // utoipa's 3.2 model supports QUERY, but its path macro does not yet
    // accept the verb. Generate the operation through its POST arm, then
    // move it to the correct 3.2 Path Item field.
    item.query = item.post.take();
    for (path, item) in &mut openapi.paths.paths {
        let retired = [
            "/api/v1/items",
            "/api/v1/collections",
            "/api/v1/libraries",
            "/api/v1/artists",
            "/api/v1/up-next",
            "/api/v1/subtitles",
            "/api/v1/candidate-artwork",
            "/admin/v1/libraries",
            "/admin/v1/collections",
            "/admin/v1/collection-items",
            "/admin/v1/library-items",
            "/admin/v1/items/{id}/match",
        ]
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
            || matches!(
                path.as_str(),
                "/admin/v1/enrich/review" | "/admin/v1/enrich/search"
            );
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
            if retired {
                operation.description = Some("Unavailable in the mediadb ingestion milestone: authenticated requests return 501 feature_unavailable. Success schemas document the retained consumer contract for its later port.".into());
                operation.responses.responses.insert("501".into(), utoipa::openapi::response::ResponseBuilder::new()
                    .description("feature_unavailable: this consumer has not been integrated with mediadb")
                    .build().into());
            }
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
    segments: Arc<crate::segments::Detector>,
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
        segments,
        proxy_trust: net.proxy_trust,
        metrics_token: Arc::new(net.metrics_token),
        setup_url: Arc::new(net.setup_url),
        public_origin: net.public_origin,
    };
    let mut bearer = Router::new()
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/prefs", get(get_prefs).put(put_pref))
        .route(
            "/api/v1/account/opensubtitles",
            get(account_opensubtitles)
                .post(set_account_opensubtitles)
                .delete(delete_account_opensubtitles),
        )
        .route("/api/v1/events", get(events));
    for path in [
        "/api/v1/items",
        "/api/v1/items/{*rest}",
        "/api/v1/collections",
        "/api/v1/libraries",
        "/api/v1/artists",
        "/api/v1/artists/{*rest}",
        "/api/v1/up-next",
        "/api/v1/subtitles/{*rest}",
        "/api/v1/candidate-artwork",
    ] {
        bearer = bearer.route(path, axum::routing::any(catalogue_unavailable));
    }
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
        .route(
            "/admin/v1/segments",
            get(admin_segments_status).post(admin_segments_run),
        )
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
    for path in [
        "/admin/v1/enrich/review",
        "/admin/v1/enrich/search",
        "/admin/v1/libraries",
        "/admin/v1/libraries/{*rest}",
        "/admin/v1/collections",
        "/admin/v1/collection-items/{*rest}",
        "/admin/v1/library-items/{*rest}",
        "/admin/v1/items/{id}/match",
    ] {
        admin = admin.route(path, axum::routing::any(catalogue_unavailable));
    }
    let admin = admin
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let mut app = Router::new()
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

async fn catalogue_unavailable() -> ApiError {
    ApiError::new(
        ErrorCode::FeatureUnavailable,
        "this feature is unavailable during catalogue integration",
    )
}

/// Isolated regression fixture for consumers awaiting their mediadb port.
/// The running hub never routes requests through this implementation.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn legacy_router_fixture(
    registry: Arc<Registry>,
    auth: Arc<Auth>,
    sessions: Arc<crate::sessions::Sessions>,
    enrollments: Arc<crate::enrollment_service::EnrollmentService>,
    subtitles: Arc<crate::subtitles::Subtitles>,
    artwork: Arc<crate::artwork::Artwork>,
    enricher: Arc<crate::enrich::Enricher>,
    segments: Arc<crate::segments::Detector>,
    net: NetOptions,
) -> Router {
    let cors = cors_layer(&net.cors_origins);
    let web_dir = net.web_dir;
    // Teardown uses the registry to release satellite sessions and emit events.
    sessions.attach_registry(registry.clone());
    let state = AppState {
        registry,
        auth,
        sessions,
        enrollments,
        subtitles,
        artwork,
        enricher,
        segments,
        proxy_trust: net.proxy_trust,
        metrics_token: Arc::new(net.metrics_token),
        setup_url: Arc::new(net.setup_url),
        public_origin: net.public_origin,
    };
    // Method/path membership is the cookie authority. Item/session ownership
    // remains inside authentication for both transport groups.
    let bearer_items = Router::new()
        .route("/api/v1/items/{id}", get(item_detail).fallback(item_query))
        .route("/api/v1/items/{id}/children", get(item_children))
        .route(
            "/api/v1/items/{id}/watched",
            axum::routing::put(item_set_watched),
        )
        .route("/api/v1/items/{id}/subtitles/search", post(subtitle_search))
        .route(
            "/api/v1/items/{id}/subtitles/download",
            post(subtitle_download),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_item_access,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let media_items = Router::new()
        .route("/api/v1/items/{id}/artwork", get(item_artwork))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_item_access,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
        ));
    let bearer_sessions = Router::new()
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
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let media_sessions = Router::new()
        .route("/api/v1/playback/sessions/{id}/stream", get(stream_session))
        .route("/api/v1/playback/sessions/{id}/{file}", get(session_file))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session_owner,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
        ));
    let bearer = Router::new()
        // Resource resolvers check current links or an owned session's captured
        // copy and current collection grant. The item gate cannot run first:
        // an established item may have lost its final copy to a correction.
        .route("/api/v1/items/{id}/fonts", get(item_fonts))
        .route("/api/v1/collections", get(list_collections))
        .route("/api/v1/libraries", get(list_libraries))
        .route("/api/v1/artists", get(list_artists))
        .route("/api/v1/artists/{key}/albums", get(artist_albums))
        .route("/api/v1/items", get(list_items))
        .route("/api/v1/up-next", get(up_next))
        .route("/api/v1/auth/logout", post(logout))
        .route(
            "/api/v1/subtitles/{track_id}",
            axum::routing::delete(subtitle_delete),
        )
        .route("/api/v1/prefs", get(get_prefs).put(put_pref))
        .route(
            "/api/v1/account/opensubtitles",
            get(account_opensubtitles)
                .post(set_account_opensubtitles)
                .delete(delete_account_opensubtitles),
        )
        .route("/api/v1/playback/sessions", post(start_session))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let media = Router::new()
        .route(
            "/api/v1/items/{id}/subtitles/{file}",
            get(item_subtitle_file),
        )
        .route("/api/v1/items/{id}/fonts/{n}", get(item_font))
        .route("/api/v1/events", get(events))
        .route("/api/v1/artists/{key}/artwork", get(artist_artwork))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer_or_media,
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
        .route("/admin/v1/users", get(admin_users).post(admin_create_user))
        .route(
            "/admin/v1/users/{id}",
            axum::routing::delete(admin_delete_user),
        )
        .route(
            "/admin/v1/users/{id}/libraries",
            axum::routing::put(admin_set_user_libraries),
        )
        .route(
            "/admin/v1/users/{id}/admin",
            axum::routing::put(admin_set_user_admin),
        )
        .route(
            "/admin/v1/segments",
            get(admin_segments_status).post(admin_segments_run),
        )
        .route("/admin/v1/providers", get(admin_providers))
        .route(
            "/admin/v1/providers/chains/{media_type}",
            post(admin_set_chain),
        )
        .route("/admin/v1/providers/tmdb", post(admin_set_tmdb))
        .route("/admin/v1/providers/fanart", post(admin_set_fanart))
        .route("/admin/v1/providers/theaudiodb", post(admin_set_theaudiodb))
        .route("/admin/v1/providers/tvdb", post(admin_set_tvdb))
        .route("/admin/v1/providers/anidb", post(admin_set_anidb))
        .route("/admin/v1/providers/anidb/verify", post(admin_verify_anidb))
        .route(
            "/admin/v1/providers/{provider}/credentials",
            axum::routing::delete(admin_disconnect_provider),
        )
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
        .route(
            "/admin/v1/collection-items/{id}/match",
            post(admin_apply_match),
        )
        .route("/admin/v1/items/{id}/match", post(admin_apply_legacy_match))
        .route(
            "/admin/v1/library-items/{id}/metadata",
            axum::routing::put(admin_catalog_metadata),
        )
        .route("/admin/v1/sessions", get(admin_sessions))
        .route(
            "/admin/v1/sessions/{id}",
            axum::routing::delete(admin_end_session),
        )
        // OPS-10: one session's diagnostics as a downloadable bundle,
        // and the newest bundle for an item (whoever played it).
        .route("/admin/v1/sessions/{id}/log", get(admin_session_log))
        .route("/admin/v1/items/{id}/log", get(admin_item_log))
        .route_layer(axum::middleware::from_fn(require_admin))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    let app = Router::new()
        .merge(admin)
        .merge(bearer)
        .merge(media)
        .merge(bearer_items)
        .merge(media_items)
        .merge(bearer_sessions)
        .merge(media_sessions)
        // NFR-6: public on purpose. It names modules and their state and
        // nothing else — a load balancer or uptime check must be able to
        // ask without holding a credential, and there is nothing here
        // that a failed login does not already reveal.
        .route("/api/v1/candidate-artwork", get(candidate_artwork))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/api/v1/bootstrap", get(bootstrap))
        // Initial-admin creation deliberately does not exist on this public
        // router. It lives on the dedicated loopback setup listener.
        .route("/api/v1/auth/token", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .with_state(state);
    // Merge, then fall back, then layer. All three orderings matter and two of
    // them were wrong first:
    //
    // - AFTER the merges, because `method_not_allowed_fallback` walks the
    //   routes registered at the time it is called. Set before, `POST /app/`
    //   still answered axum's bare 405 while every other path answered JSON.
    // - BEFORE the CORS layer, because `layer` wraps each route AND each
    //   method router's default fallback. Set after, these replaced the
    //   wrapped ones with bare handlers — and since no route registers
    //   `options`, a browser preflight lands on exactly that fallback. Every
    //   cross-origin POST/PUT/DELETE then preflighted, got a 405 with no
    //   `Access-Control-Allow-Origin`, and was blocked.
    //
    // `tests/error_bodies.rs` pins both ends: the preflight is answered, and
    // the fallbacks still speak JSON through the layer.
    //
    // One consequence worth stating rather than rediscovering: a 405 is no
    // longer behind `require_bearer`. The sub-routers attach that with
    // `Router::route_layer`, which goes through `Endpoint::layer` and so wraps
    // a method router's DEFAULT 405 fallback while leaving it the `Default`
    // variant — and `method_not_allowed_fallback` replaces exactly that
    // variant. (Not `MethodRouter::route_layer`, which does leave the fallback
    // alone; two readings of axum disagreed about this and the answer is
    // measured, not read: with the call removed, `POST /admin/v1/users/abc`
    // with no credentials answers 401; with it, 405.)
    //
    // So an unauthenticated caller can tell an admin path that exists from one
    // that does not. That is not new information — `/api-docs/openapi.json` is
    // served unauthenticated and lists all 29 of them, also measured — so it
    // is accepted rather than worked around. Putting the fallback behind auth
    // would mean giving up the JSON body on every 405, which is the thing this
    // is for.
    let mut app = app
        .merge(Router::from(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi_document()),
        ))
        .merge(crate::web::router(web_dir))
        .fallback(unknown_route)
        .method_not_allowed_fallback(wrong_method);
    if let Some(cors) = cors {
        app = app.layer(cors);
    }
    // Outermost: it must wrap CORS responses, router fallbacks and every auth
    // rejection as well as ordinary handlers. The hub generates the id; a
    // caller cannot choose what privileged logs use as their correlation key.
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
struct RefreshResponse {
    asked: usize,
    offline: usize,
}

#[derive(Serialize, ToSchema)]
struct ReviewEntry {
    /// The reviewed collection copy; retained for original v1 clients.
    item_id: String,
    collection_item_id: String,
    /// Shared library identity for navigation, never an implicit mutation target.
    library_item_id: String,
    /// Detected collection kind, using the original v1 show/track vocabulary.
    kind: String,
    title: String,
    #[schema(required)]
    year: Option<i64>,
    #[schema(required)]
    path: Option<String>,
    confidence: String,
    #[schema(required)]
    matched_title: Option<String>,
    #[schema(required)]
    premiered: Option<String>,
    #[schema(required)]
    provider: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct ReviewEntriesResponse {
    entries: Vec<ReviewEntry>,
}

#[derive(Serialize, ToSchema)]
struct ReviewCandidatesResponse {
    candidates: Vec<crate::enrich::ProviderCandidate>,
}

#[derive(Deserialize, ToSchema)]
struct ManualMatchCandidate {
    #[serde(default, deserialize_with = "candidate_id")]
    id: Option<u64>,
    title: Option<String>,
    overview: Option<String>,
    poster_path: Option<String>,
    vote_average: Option<f64>,
    release_date: Option<String>,
}

fn candidate_id<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(serde_json::Value::deserialize(deserializer)
        .ok()
        .and_then(|value| value.as_u64()))
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
struct AdminLibrariesResponse {
    libraries: Vec<crate::registry::LibraryOverview>,
}

#[derive(Serialize, ToSchema)]
struct AdminCollectionsResponse {
    collections: Vec<crate::registry::CollectionOverview>,
}

#[derive(Serialize, ToSchema)]
struct CreatedLibraryResponse {
    id: String,
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
    coverage: Vec<crate::library::SourceBoundary>,
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
struct CollectionsResponse {
    collections: Vec<crate::registry::CollectionRow>,
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
    /// Last reported source count; absence is unknown, never zero.
    pending_sources: Option<u64>,
    enabled: Option<bool>,
}

#[derive(Serialize, ToSchema)]
struct SegmentStatusResponse {
    collections: Vec<SegmentCollectionStatus>,
}

#[derive(Serialize, ToSchema)]
struct SegmentRunResponse {
    /// Mediahost wake messages accepted, not completed analysis jobs.
    asked: usize,
    unavailable: usize,
}

#[derive(Serialize, ToSchema)]
struct LibrarySummary {
    id: String,
    name: String,
    media_type: String,
}

#[derive(Serialize, ToSchema)]
struct LibrariesResponse {
    libraries: Vec<LibrarySummary>,
}

#[derive(Serialize, ToSchema)]
struct ItemsResponse {
    items: Vec<ItemRow<i64>>,
    total: i64,
    limit: u32,
    offset: u32,
}

#[derive(Serialize, ToSchema)]
struct ArtistSummary {
    /// Opaque, URL-safe synthetic identity used by artist navigation.
    key: String,
    /// Stored Album Artist spelling used for display.
    name: String,
    album_count: i64,
    /// Changes when the selected portrait or generated album collage changes.
    #[schema(required)]
    art_version: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct ArtistsResponse {
    artists: Vec<ArtistSummary>,
    total: i64,
    limit: u32,
    offset: u32,
}

#[derive(Serialize, ToSchema)]
struct ArtistAlbumsResponse {
    artist: ArtistSummary,
    albums: Vec<ItemRow<i64>>,
    total: i64,
    limit: u32,
    offset: u32,
}

#[derive(Serialize, ToSchema)]
struct ChildrenResponse {
    children: Vec<ItemRow<i64>>,
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
    if let Some(captured) = &session.catalogue {
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

/// HUB-10: the grant gate for every route keyed by an item id.
///
/// Layered inside `require_auth`, so Claims is present. The id is read
/// through `Path` rather than by counting URI segments: the group holds
/// routes of three different depths, and `/items/{id}/fonts/{n}` would
/// otherwise be one refactor away from checking the font number.
async fn require_item_access(
    State(state): State<AppState>,
    ApiPath(params): ApiPath<std::collections::HashMap<String, String>>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let id = params.get("id").map(String::as_str).unwrap_or_default();
    if !crate::grants::can_see(state.registry.db(), &claims, id)
        .await
        .map_err(internal)?
    {
        tracing::debug!(user = %claims.username, item = %id, "item hidden by grants");
        return Err(hidden("item"));
    }
    Ok(next.run(req).await)
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
    let db = state.registry.db();
    let mut chains = std::collections::BTreeMap::new();
    for media_type in crate::providers::MEDIA_TYPES {
        chains.insert(
            media_type.to_string(),
            ProviderChain {
                order: if state.enricher.catalogue_active() {
                    state
                        .registry
                        .catalogue()
                        .provider_order(
                            kahawai_mediadb::MediaType::parse(media_type).map_err(internal)?,
                        )
                        .await
                        .map_err(internal)?
                } else {
                    crate::providers::chain_in_force(db, media_type).await
                },
                default: if state.enricher.catalogue_active() && media_type == "anime" {
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
    if state.enricher.catalogue_active() {
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
        return Ok(Json(OkResponse { ok: true }));
    }
    crate::providers::set_chain(state.registry.db(), &media_type, &body.order)
        .await
        .map_err(
            |e| match e.downcast_ref::<crate::providers::NotAPermutation>() {
                // Its own sentence, because it is the only thing that names a
                // valid order — an admin who dropped one provider otherwise got a
                // 400 with nothing to correct against.
                Some(wrong) => ApiError::new(ErrorCode::BadRequest, wrong.to_string()),
                None => internal(e),
            },
        )?;
    state
        .registry
        .emit(crate::registry::RegistryEvent::EnrichChain {
            kind: "enrich",
            chain: media_type,
        });
    Ok(Json(OkResponse { ok: true }))
}

/// Physical source context for actions whose result depends on a rendition.
#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct SourceQuery {
    source_id: Option<i64>,
}

async fn source_copy(
    state: &AppState,
    user: &str,
    item: &str,
    source: Option<i64>,
) -> Result<(String, i64), ApiError> {
    let copies = crate::library::copies(state.registry.db(), user, item)
        .await
        .map_err(internal)?;
    if copies.is_empty() {
        return Err(hidden("item"));
    }
    let sources: Vec<(String, i64)> = sqlx::query_as("SELECT ps.item_id,ps.id FROM playable_sources ps WHERE ps.item_id IN(SELECT value FROM json_each(?)) ORDER BY ps.id")
        .bind(serde_json::to_string(&copies).map_err(internal)?).fetch_all(state.registry.db()).await.map_err(internal)?;
    if let Some(source) = source {
        return sources
            .into_iter()
            .find(|(_, id)| *id == source)
            .ok_or_else(|| hidden("source"));
    }
    if sources.len() == 1 {
        return Ok(sources.into_iter().next().unwrap());
    }
    Err(ApiError::new(
        ErrorCode::BadRequest,
        "source_id is required when choosing a physical copy",
    ))
}

/// A rematch changes current catalogue links, while playback keeps its captured
/// copy. Resource fallback is limited to that user's active session coverage
/// and requires a current grant for every captured physical collection.
async fn captured_resource_sessions(
    state: &AppState,
    user: &str,
    item: &str,
) -> Result<Vec<Arc<crate::sessions::Session>>, ApiError> {
    let canonical = crate::library::resolve_id(state.registry.db(), item)
        .await
        .map_err(internal)?;
    let mut allowed = Vec::new();
    for session in state.sessions.list().into_iter().filter(|s| {
        s.user_id == user
            && (s.item_id == item
                || s.item_id == canonical
                || s.library_item_ids
                    .iter()
                    .any(|id| id == item || id == &canonical))
    }) {
        let mut granted = !session.parts.is_empty();
        for part in &session.parts {
            let visible: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND (is_admin=1 OR all_libraries=1))
                    OR EXISTS(SELECT 1 FROM library_collections lc JOIN user_libraries ul ON ul.library_id=lc.library_id
                        WHERE ul.user_id=?1 AND (lc.module_id,lc.collection_id)=(?2,?3))",
            )
            .bind(user)
            .bind(&part.module_id)
            .bind(&part.collection_id)
            .fetch_one(state.registry.db())
            .await
            .map_err(internal)?;
            if !visible {
                granted = false;
                break;
            }
        }
        if granted {
            allowed.push(session);
        }
    }
    Ok(allowed)
}

async fn resource_track(
    state: &AppState,
    user: &str,
    item: &str,
    track_id: i64,
) -> Result<crate::tracks::Track, ApiError> {
    if let Some(track) =
        crate::tracks::get_for_library_item(state.registry.db(), user, item, track_id)
            .await
            .map_err(internal)?
    {
        return Ok(track);
    }
    for session in captured_resource_sessions(state, user, item).await? {
        if let Some(track) =
            crate::tracks::get_for_item(state.registry.db(), &session.collection_item_id, track_id)
                .await
                .map_err(internal)?
            && track.source_id.is_none_or(|file| {
                session
                    .parts
                    .iter()
                    .any(|p| p.file_id == crate::sessions::FileId::Legacy(file))
            })
        {
            return Ok(track);
        }
    }
    Err(hidden("track"))
}

async fn resource_source_copy(
    state: &AppState,
    user: &str,
    item: &str,
    source: Option<i64>,
) -> Result<(String, i64), ApiError> {
    let error = match source_copy(state, user, item, source).await {
        Ok(copy) => return Ok(copy),
        Err(error) if error.code() == ErrorCode::NotFound && source.is_some() => error,
        Err(error) => return Err(error),
    };
    for session in captured_resource_sessions(state, user, item).await? {
        if Some(session.playable_source_id) != source {
            continue;
        }
        let Some(part) = session.parts.first() else {
            continue;
        };
        let bound: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id
                WHERE ps.id=? AND ps.item_id=? AND p.ordinal=1 AND p.file_id=?)",
        ).bind(session.playable_source_id).bind(&session.collection_item_id).bind(part.file_id.legacy().map_err(internal)?)
            .fetch_one(state.registry.db()).await.map_err(internal)?;
        if bound {
            return Ok((
                session.collection_item_id.clone(),
                session.playable_source_id,
            ));
        }
    }
    Err(error)
}

#[derive(Deserialize, ToSchema)]
struct SubtitleSearchRequest {
    source_id: Option<i64>,
    /// Preferred languages, ordered. Empty = whatever the provider has.
    #[serde(default)]
    languages: Vec<String>,
}

/// Search external subtitle providers
///
/// Searches external providers for subtitles matching an item, optionally
/// restricted to preferred languages, and returns candidates with the
/// caller's remaining quota. Returns 409 when the download entitlement is
/// spent and 502 when a provider refuses.
#[utoipa::path(
    post, path = "/api/v1/items/{id}/subtitles/search", tag = "Subtitles",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SubtitleSearchRequest,
    responses(
        (status = 200, body = SubtitleSearchResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, description = "The download entitlement is spent: `subtitle_quota_spent`", body = ApiErrorBody),
        (status = 502, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn subtitle_search(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<SubtitleSearchRequest>,
) -> Result<Json<SubtitleSearchResponse>, ApiError> {
    let library_item_id = crate::library::resolve_id(state.registry.db(), &id)
        .await
        .map_err(internal)?;
    let (id, source_id) = source_copy(&state, &claims.sub, &id, body.source_id).await?;
    let (candidates, quota) = state
        .subtitles
        .search_external(
            &state.registry,
            &library_item_id,
            &id,
            Some(source_id),
            body.languages,
            &claims.sub,
        )
        .await
        .map_err(subtitle_provider_refusal)?;
    Ok(Json(SubtitleSearchResponse { candidates, quota }))
}

#[derive(Deserialize, ToSchema)]
struct SubtitleDownloadRequest {
    source_id: Option<i64>,
    file_id: String,
    #[serde(default)]
    language: Option<String>,
}

/// Download a subtitle for an item
///
/// Downloads the chosen provider file and attaches it to the item as a
/// subtitle track, returning the new track id and remaining quota. Returns
/// 409 when the download entitlement is spent and 502 when a provider
/// refuses.
#[utoipa::path(
    post, path = "/api/v1/items/{id}/subtitles/download", tag = "Subtitles",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = SubtitleDownloadRequest,
    responses(
        (status = 200, body = SubtitleDownloadResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 409, description = "The download entitlement is spent: `subtitle_quota_spent`", body = ApiErrorBody),
        (status = 502, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn subtitle_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<SubtitleDownloadRequest>,
) -> Result<Json<SubtitleDownloadResponse>, ApiError> {
    let (id, _source_id) = source_copy(&state, &claims.sub, &id, body.source_id).await?;
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
        .map_err(subtitle_provider_refusal)?;
    Ok(Json(SubtitleDownloadResponse { track_id, quota }))
}

/// Delete a downloaded subtitle track
///
/// Deletes a downloaded subtitle track when the caller downloaded it or is an
/// admin. Any other track, including cached or scan-owned rows, is left in
/// place and answered with 200 and removed set to false.
#[utoipa::path(
    delete, path = "/api/v1/subtitles/{track_id}", tag = "Subtitles",
    security(("bearer_auth" = [])),
    params(("track_id" = i64, Path)),
    responses(
        (status = 200, body = RemovedResponse),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn subtitle_delete(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath(track_id): ApiPath<i64>,
) -> Result<Json<RemovedResponse>, ApiError> {
    let removed = state
        .subtitles
        .delete_track(&state.registry, track_id, &claims.sub, claims.admin)
        .await
        .map_err(internal)?;
    Ok(Json(RemovedResponse { removed }))
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
        // A replacement key can have a different visibility tier. Ready
        // images are useful offline; only previous negative answers are owed.
        sqlx::query("DELETE FROM artist_artwork WHERE outcome <> 'ready'")
            .execute(state.registry.db())
            .await
            .map_err(internal)?;
    }
    drop(change);
    state.enricher.request_artist_run(state.registry.clone());
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
        sqlx::query("DELETE FROM artist_artwork WHERE outcome <> 'ready'")
            .execute(state.registry.db())
            .await
            .map_err(internal)?;
    }
    drop(change);
    state.enricher.request_artist_run(state.registry.clone());
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
    if provider == crate::enrich::THEAUDIODB {
        sqlx::query("DELETE FROM artist_artwork WHERE outcome <> 'ready'")
            .execute(state.registry.db())
            .await
            .map_err(internal)?;
    }
    if provider == crate::anidb::ANIDB {
        let forgotten = crate::anidb::forget_session(state.enricher.data_dir());
        // Revocation above remains in force even if removing the persisted
        // session fails.
        forgotten.map_err(internal)?;
    }
    drop(_change);
    if provider == crate::enrich::THEAUDIODB {
        state.enricher.request_artist_run(state.registry.clone());
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
    if state.enricher.catalogue_active() {
        let store = state.registry.catalogue();
        let rows = store.enrichment_status().await.map_err(internal)?;
        let (matched, weak, missed) = store.enrichment_counts().await.map_err(internal)?;
        return Ok(Json(crate::enrich::EnrichStatus {
            running: rows.iter().any(|r| r.state == "running"),
            matched: matched as usize,
            weak: weak as usize,
            missed: missed as usize,
        }));
    }
    Ok(Json(state.enricher.status()))
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

/// Refresh a library
///
/// Admin only. Sends a scan request to the mediahost of every collection in
/// the library and returns how many were asked and how many were offline.
/// Pass deep=true to re-probe every file. Returns 404 if the library has no
/// collections.
#[utoipa::path(
    post, path = "/admin/v1/libraries/{id}/refresh", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    params(("id" = String, Path), RefreshQuery),
    responses(
        (status = 200, body = RefreshResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, description = "No such library, or it has no collections attached", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_refresh_library(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RefreshQuery>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT module_id, collection_id FROM library_collections WHERE library_id = ?",
    )
    .bind(&id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    if members.is_empty() {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            "library has no collections",
        ));
    }
    let (mut asked, mut offline) = (0usize, 0usize);
    for (module_id, collection_id) in members {
        if matches!(
            state
                .registry
                .rescan_collection(&module_id, &collection_id, q.deep.unwrap_or(false))
                .await,
            crate::registry::RescanResult::Requested
        ) {
            asked += 1;
        } else {
            offline += 1;
        }
    }
    Ok(Json(RefreshResponse { asked, offline }))
}

/// List items needing match review
///
/// Admin only. Returns unresolved library assignments and conflicting choices,
/// independently of provider confidence. Top-level items also appear for weak
/// or missing provider matches; children inherit those provider answers.
#[utoipa::path(
    get, path = "/admin/v1/enrich/review", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = ReviewEntriesResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_review_list(
    State(state): State<AppState>,
) -> Result<Json<ReviewEntriesResponse>, ApiError> {
    let rows = sqlx::query(
        "SELECT i.id,a.library_item_id,i.kind, i.title, i.year, COALESCE(m.confidence,'miss') AS confidence,
                m.title AS matched_title, m.premiered, m.provider, m.provider_id,
                (SELECT f.path_rel FROM files f JOIN file_bindings fb ON fb.file_id=f.id
                  WHERE fb.item_id=i.id LIMIT 1) AS path
         FROM collection_items i
         JOIN collection_item_library_items a ON a.collection_item_id=i.id AND a.ordinal=1
         LEFT JOIN resolved_metadata m ON m.item_id = i.id
         WHERE (i.match_mode='unmatched' OR i.match_conflict IS NOT NULL
           OR (i.kind IN ('movie','show','album')
             AND (m.confidence IS NULL OR m.confidence IN ('miss', 'weak', 'rejected'))))
           AND NOT (i.match_mode='manual' AND i.match_conflict IS NULL)
         ORDER BY m.confidence != 'miss', i.title",
    )
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let entries = rows
        .iter()
        .map(|row| ReviewEntry {
            item_id: row.get("id"),
            collection_item_id: row.get("id"),
            library_item_id: row.get("library_item_id"),
            kind: row.get("kind"),
            title: row.get("title"),
            year: row.get("year"),
            path: row.get("path"),
            confidence: row.get("confidence"),
            matched_title: row.get("matched_title"),
            premiered: row.get("premiered"),
            provider: row.try_get("provider").ok().flatten(),
        })
        .collect();
    Ok(Json(ReviewEntriesResponse { entries }))
}

#[derive(Deserialize, ToSchema)]
struct ReviewSearch {
    kind: String,
    query: String,
    year: Option<i64>,
    /// The item being matched — lets ranking favour the provider that
    /// owns its collection's identity space (anilist for anime).
    item: Option<String>,
}

/// Search metadata candidates
///
/// Admin only. Searches the metadata providers for candidates matching a
/// title, kind and optional year, for use when manually matching an item.
/// Supply item to bias ranking toward the provider owning that item's
/// identity space.
#[utoipa::path(
    post, path = "/admin/v1/enrich/search", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    request_body = ReviewSearch,
    responses(
        (status = 200, body = ReviewCandidatesResponse),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_review_search(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReviewSearch>,
) -> Result<Json<ReviewCandidatesResponse>, ApiError> {
    let mut candidates = state
        .enricher
        .search_candidates(
            &state.registry,
            match body.kind.as_str() {
                "series" => "show",
                "song" => "track",
                k => k,
            },
            &body.query,
            body.year,
            body.item.as_deref(),
        )
        .await
        .map_err(internal)?;
    for candidate in &mut candidates {
        let poster = match candidate {
            crate::enrich::ProviderCandidate::Catalog(c) => &mut c.poster_url,
            crate::enrich::ProviderCandidate::Anilist(c) => &mut c.poster_url,
        };
        if let Some(url) = poster {
            let ticket = state.auth.candidate_artwork_ticket(url).map_err(internal)?;
            *url = format!("/api/v1/candidate-artwork?ticket={ticket}");
        }
    }
    Ok(Json(ReviewCandidatesResponse { candidates }))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct CandidateArtworkQuery {
    /// Short-lived poster capability returned by an administrator's search.
    ticket: String,
}

#[utoipa::path(
    get, path = "/api/v1/candidate-artwork", tag = "Admin enrichment",
    security(("candidate_artwork_ticket" = [])),
    params(CandidateArtworkQuery),
    responses((status = 200, body = Vec<u8>, content_type = "image/jpeg"),
        (status = 400, body = ApiErrorBody), (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody))
)]
async fn candidate_artwork(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<CandidateArtworkQuery>,
) -> Result<Response, ApiError> {
    let url = state
        .auth
        .candidate_artwork_url(&q.ticket)
        .map_err(|_| hidden("artwork"))?;
    let (bytes, content_type) = state
        .artwork
        .candidate_poster(&url)
        .await
        .map_err(internal)?
        .ok_or_else(|| hidden("artwork"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CACHE_CONTROL, "private, max-age=900"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize, ToSchema)]
struct CatalogMetadataRequest {
    expected_revision: i64,
    fields: crate::library::MetadataOverrides,
}

#[utoipa::path(put,path="/admin/v1/library-items/{id}/metadata",tag="Admin enrichment",
    security(("bearer_auth"=[])),params(("id"=String,Path)),request_body=CatalogMetadataRequest,
    responses((status=200,body=OkResponse),(status=400,body=ApiErrorBody),(status=401,body=ApiErrorBody),
    (status=403,body=ApiErrorBody),(status=404,body=ApiErrorBody),(status=409,body=ApiErrorBody),(status=500,body=ApiErrorBody)))]
async fn admin_catalog_metadata(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<CatalogMetadataRequest>,
) -> Result<Json<OkResponse>, ApiError> {
    let mut tx = state.registry.db().begin().await.map_err(internal)?;
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM library_items WHERE id=? AND merged_into IS NULL")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
    let revision = revision.ok_or_else(|| hidden("item"))?;
    if revision != body.expected_revision {
        return Err(ApiError::new(
            ErrorCode::StaleWrite,
            "Catalogue metadata changed; reload before saving",
        ));
    }
    if body
        .fields
        .rating
        .is_some_and(|r| !r.is_finite() || !(0.0..=10.0).contains(&r))
    {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "Rating must be between 0 and 10",
        ));
    }
    sqlx::query("INSERT INTO library_overrides VALUES(?,?) ON CONFLICT(library_item_id) DO UPDATE SET fields=excluded.fields").bind(&id).bind(serde_json::to_string(&body.fields).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
    sqlx::query("UPDATE library_items SET revision=revision+1 WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(OkResponse { ok: true }))
}

#[derive(Deserialize, ToSchema)]
struct ApplyMatch {
    expected_revision: i64,
    #[serde(flatten)]
    decision: MatchDecision,
}

#[derive(Deserialize, ToSchema)]
struct MatchDecision {
    library_item_ids: Option<Vec<String>>,
    new_item: Option<crate::library::NewItem>,
    /// "pick": store the supplied candidate; "confirm": promote the
    /// current weak match; "reject": clear the match, excluded from
    /// auto-retries.
    action: String,
    provider: Option<String>,
    candidate: Option<ManualMatchCandidate>,
}

/// Apply a match decision to an item
///
/// Admin only. Applies action pick, confirm or reject to the item's metadata
/// match. A pick requires provider and candidate with an id; any other
/// action, or a missing field, returns 400. Picked and confirmed matches are
/// pinned against automatic re-matching.
#[utoipa::path(
    post, path = "/admin/v1/collection-items/{id}/match", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = ApplyMatch,
    responses(
        (status = 200, body = crate::library::Assignment),
        (status = 409, body = ApiErrorBody),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_apply_match(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<ApplyMatch>,
) -> Result<Json<crate::library::Assignment>, ApiError> {
    library_api::apply_match(&state, &id, body).await.map(Json)
}

#[derive(Deserialize, ToSchema)]
struct LegacyApplyMatch {
    action: String,
    provider: Option<String>,
    candidate: Option<ManualMatchCandidate>,
}

/// Apply a legacy provider match decision to one collection item.
///
/// Retains the original v1 request and response for existing clients. The ID
/// identifies the original collection copy, including its grouped files; it
/// never chooses an arbitrary copy of a shared library item. New clients use
/// the collection-items route with an assignment revision to detect stale edits.
#[utoipa::path(
    post, path = "/admin/v1/items/{id}/match", tag = "Admin enrichment",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = LegacyApplyMatch,
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_apply_legacy_match(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<LegacyApplyMatch>,
) -> Result<Json<OkResponse>, ApiError> {
    if !matches!(body.action.as_str(), "pick" | "confirm" | "reject") {
        return Err(ApiError::new(ErrorCode::BadRequest, "Unknown match action"));
    }
    library_api::apply_decision(
        &state,
        &id,
        None,
        MatchDecision {
            action: body.action,
            provider: body.provider,
            candidate: body.candidate,
            library_item_ids: None,
            new_item: None,
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
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

/// Replace a user's library access
///
/// Admin only. Replaces the account's library grants wholesale and returns
/// what was stored along with the new grants_version. Send the grants_version
/// you read; a stale one returns 409 stale_write. Running sessions are
/// unaffected.
// Wholesale and idempotent so two admins toggling different checkboxes
// cannot interleave into a set neither chose; the response echoes what
// was stored, so a dropped stale library id is visible.
#[utoipa::path(
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
async fn admin_set_user_libraries(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<SetAccess>,
) -> Result<Json<UserAccessResponse>, ApiError> {
    let db = state.registry.db();
    let applied = crate::grants::set_access(
        db,
        &id,
        body.grants_version,
        body.all_libraries,
        &body.libraries,
    )
    .await
    .map_err(internal)?;
    let (grants_version, stored) = match applied {
        crate::grants::SetAccess::Applied {
            grants_version,
            libraries,
        } => (grants_version, libraries),
        crate::grants::SetAccess::Stale => {
            return Err(ApiError::new(
                ErrorCode::StaleWrite,
                "somebody else changed this account's libraries; reload and try again",
            ));
        }
        crate::grants::SetAccess::NoSuchUser => return Err(hidden("user")),
    };
    Ok(Json(UserAccessResponse {
        id,
        all_libraries: body.all_libraries,
        libraries: stored,
        grants_version,
    }))
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

fn retire_deleted_segment_link(
    detector: &crate::segments::Detector,
    module_id: &str,
    generation: Option<u64>,
) {
    if let Some(generation) = generation {
        detector.segment_link_disconnected(module_id, generation);
    }
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
    retire_deleted_segment_link(&state.segments, &id, deleted.mediahost_link_generation);
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

/// List libraries
///
/// Admin only. Returns every library with its administrative overview data.
#[utoipa::path(
    get, path = "/admin/v1/libraries", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = AdminLibrariesResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_libraries(
    State(state): State<AppState>,
) -> Result<Json<AdminLibrariesResponse>, ApiError> {
    let libraries = state
        .registry
        .libraries_overview()
        .await
        .map_err(internal)?;
    Ok(Json(AdminLibrariesResponse { libraries }))
}

/// List collections
///
/// Admin only. Returns every collection known to the hub across all
/// satellites, whether or not it is attached to a library.
#[utoipa::path(
    get, path = "/admin/v1/collections", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = AdminCollectionsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_collections(
    State(state): State<AppState>,
) -> Result<Json<AdminCollectionsResponse>, ApiError> {
    let collections = state
        .registry
        .collections_overview()
        .await
        .map_err(internal)?;
    Ok(Json(AdminCollectionsResponse { collections }))
}

#[derive(Deserialize, ToSchema)]
struct CreateLibraryRequest {
    name: String,
    media_type: String,
}

/// Create a library
///
/// Admin only. Creates a library with the given name and media type and
/// returns its id. Returns 400 for an empty name or unknown media type, and
/// 409 when the name is already taken.
#[utoipa::path(
    post, path = "/admin/v1/libraries", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    request_body = CreateLibraryRequest,
    responses(
        (status = 200, body = CreatedLibraryResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 409, description = "A library with that name already exists", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_create_library(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateLibraryRequest>,
) -> Result<Json<CreatedLibraryResponse>, ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            "library name required",
        ));
    }
    let id = state
        .registry
        .create_library(name, &body.media_type)
        .await
        .map_err(|e| {
            // Three arms, because this route has two refusals and one fault
            // and each is told apart by something different. `libraries.name`
            // is UNIQUE and the producer lets it fire; the media type is
            // checked with an `ensure!` before any statement runs; anything
            // else reaching here is the database being unwell.
            if is_unique_violation(&e) {
                ApiError::new(
                    ErrorCode::Conflict,
                    "a library with that name already exists",
                )
            } else if let Some(unknown) = e.downcast_ref::<crate::registry::UnknownMediaType>() {
                // Matched, not a catch-all. Written as `else` it would name
                // the media type for whatever refusal `create_library` grows
                // next — an empty name, a reserved one — and nothing would
                // fail.
                ApiError::new(ErrorCode::BadRequest, unknown.to_string())
            } else {
                internal(e)
            }
        })?;
    Ok(Json(CreatedLibraryResponse { id }))
}

/// Delete a library
///
/// Admin only. Deletes the library and returns 204 on success, or 404 when no
/// library has that id.
#[utoipa::path(
    delete, path = "/admin/v1/libraries/{id}", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 204, description = "Library deleted"),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_delete_library(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    if state.registry.delete_library(&id).await.map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(ErrorCode::NotFound, "no such library"))
    }
}

#[derive(Deserialize, ToSchema)]
struct AttachCollectionRequest {
    module_id: String,
    collection_id: String,
}

/// Attach a collection to a library
///
/// Admin only. Attaches the given satellite collection to the library and
/// returns 204. Returns 404 when the library or collection is unknown, and
/// 409 when their media types do not match.
#[utoipa::path(
    post, path = "/admin/v1/libraries/{id}/collections", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = AttachCollectionRequest,
    responses(
        (status = 204, description = "Collection attached"),
        (status = 400, description = "The request body is not the JSON this route takes", body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 409, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 404, description = "No such library, or no such collection", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_attach_collection(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<AttachCollectionRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .registry
        .attach_collection(&id, &body.module_id, &body.collection_id)
        .await
        .map_err(|e| {
            // Three refusals, three answers. `registry::AttachRefused` exists
            // so this route can give them: an absent library or collection is
            // a 404, and a media-type mismatch is the one an admin can act on
            // without going and looking something up. They were one opaque
            // 409 with the difference in a log line.
            match e.downcast_ref::<crate::registry::AttachRefused>() {
                Some(crate::registry::AttachRefused::NoLibrary) => {
                    ApiError::new(ErrorCode::NotFound, "no such library")
                }
                Some(crate::registry::AttachRefused::NoCollection) => {
                    ApiError::new(ErrorCode::NotFound, "no such collection")
                }
                Some(mismatch @ crate::registry::AttachRefused::TypeMismatch { .. }) => {
                    ApiError::new(ErrorCode::Conflict, mismatch.to_string())
                }
                // Every refusal this route has is an `AttachRefused` above,
                // so what reaches here is the database. A `Conflict` fallback
                // was unreachable and documented a 409 nothing could return.
                None => internal(e),
            }
        })?;
    // Portrait eligibility follows library composition. Existing collection
    // metadata is already present, so only re-evaluate artist artwork; a full
    // enrichment pass would spend provider traffic on unrelated media.
    state.enricher.request_artist_run(state.registry.clone());
    Ok(StatusCode::NO_CONTENT)
}

/// Detach a collection from a library
///
/// Admin only. Detaches the collection from the library and returns 204, or
/// 404 when that collection is not attached to it.
#[utoipa::path(
    delete, path = "/admin/v1/libraries/{id}/collections/{module_id}/{collection_id}", tag = "Admin libraries",
    security(("bearer_auth" = [])),
    params(
        ("id" = String, Path),
        ("module_id" = String, Path),
        ("collection_id" = String, Path)
    ),
    responses(
        (status = 204, description = "Collection detached"),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_detach_collection(
    State(state): State<AppState>,
    ApiPath((id, module_id, collection_id)): ApiPath<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    if state
        .registry
        .detach_collection(&id, &module_id, &collection_id)
        .await
        .map_err(internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(ErrorCode::NotFound, "not attached"))
    }
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
        let title = if let Some(captured) = &session.catalogue {
            state
                .registry
                .catalogue()
                .library_item_record(&captured.parent_id)
                .await
                .ok()
                .map(|i| i.title)
        } else {
            sqlx::query_scalar("SELECT title FROM library_items WHERE id=?")
                .bind(&session.item_id)
                .fetch_optional(state.registry.db())
                .await
                .map_err(internal)?
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
    /// Physical source whose stream indexes and chapter timeline are being used.
    source_id: Option<i64>,
    album_track_id: Option<i64>,
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
    let catalogue = if let Some(library) = &body.library_id {
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
    } else {
        if !crate::grants::can_see(state.registry.db(), &claims, &body.item_id)
            .await
            .map_err(internal)?
        {
            return Err(hidden("item"));
        }
        None
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
                source_id: body.source_id,
                album_track_id: body.album_track_id,
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
    let mut subtitle_listing = if session.catalogue.is_some() {
        session.catalogue_listing()
    } else {
        match session.parts.first() {
            Some(p) => state
                .subtitles
                .list(
                    &state.registry,
                    &session.collection_item_id,
                    session.effective_profile(),
                    session.ass_policy(),
                    &claims.sub,
                    claims.admin,
                    (&p.module_id, &p.collection_id, &p.root_token, &p.path_rel),
                )
                .await
                .map_err(internal)?,
            None => Vec::new(),
        }
    };
    // Extraction uses the captured physical owner; public resource URLs use
    // the canonical requested member, including a member of combined coverage.
    for listing in &mut subtitle_listing {
        listing.track.item_id.clone_from(&session.item_id);
        if session.catalogue.is_some() {
            listing.deletable = listing.track.origin == "downloaded"
                && (claims.admin || listing.track.created_by.as_deref() == Some(&claims.sub));
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(StartSessionResponse {
            segments: session
                .catalogue
                .as_ref()
                .map(|c| c.segments.clone())
                .unwrap_or_default(),
            media_entry_id: session.catalogue.as_ref().map(|c| c.media_entry_id.clone()),
            session_id: session.id.clone(),
            effective_start_ms: session.effective_start_ms,
            source_fingerprint: session.source_fingerprint.clone(),
            source_id: session.playable_source_id,
            replay_gain: session.replay_gain.clone(),
            library_item_ids: session.library_item_ids.clone(),
            coverage: session.boundaries.clone(),
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

/// List browsable collections
///
/// Returns the collections visible to the authenticated user. An account
/// restricted to specific libraries sees only the collections behind those
/// libraries.
#[utoipa::path(
    get, path = "/api/v1/collections", tag = "Browse",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = CollectionsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn list_collections(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<CollectionsResponse>, ApiError> {
    let db = state.registry.db();
    let cols = state.registry.collections().await.map_err(internal)?;
    // HUB-10: a restricted account is told about the collections behind
    // the libraries it holds and no others. Otherwise this route
    // enumerates the shape of the rest of the disk — names, hosts and
    // file counts — to somebody who was granted one shelf of it.
    if !crate::grants::restricted(db, &claims)
        .await
        .map_err(internal)?
    {
        return Ok(Json(CollectionsResponse { collections: cols }));
    }
    let mine: Vec<(String, String)> = sqlx::query_as(
        "SELECT lc.module_id, lc.collection_id FROM library_collections lc
           JOIN user_libraries ul ON ul.library_id = lc.library_id AND ul.user_id = ?",
    )
    .bind(&claims.sub)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let cols: Vec<_> = cols
        .into_iter()
        .filter(|c| {
            mine.iter()
                .any(|(m, i)| *m == c.module_id && *i == c.collection_id)
        })
        .collect();
    Ok(Json(CollectionsResponse { collections: cols }))
}

/// `?size=` names one of `artwork::SIZES`; anything else, including
/// nothing, serves the original.
#[derive(serde::Deserialize, Default, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ArtworkQuery {
    size: Option<String>,
    /// The client's cache-buster. Load-bearing rather than merely accepted: its
    /// presence is what makes caching a MISS safe, because only a URL that can
    /// change may hold a "there is nothing here" for long.
    v: Option<String>,
}

#[derive(serde::Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ArtistArtworkQuery {
    /// Music library providing both navigation and grant context.
    library: String,
    size: Option<String>,
    /// Client cache-buster from `ArtistSummary.art_version`.
    v: Option<String>,
}

/// Fetch Album Artist artwork
///
/// Serves a fully prefetched provider portrait, falling back to a generated
/// collage of the newest four covered albums. The library parameter provides
/// both grant context and the collage boundary: an artist can occur in several
/// libraries whose covers must remain separate. Accepts a bearer token or the
/// media cookie.
#[utoipa::path(
    get, path = "/api/v1/artists/{key}/artwork", tag = "Item media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(("key" = String, Path), ArtistArtworkQuery),
    responses(
        (status = 200, content((Vec<u8> = "image/jpeg")), headers(("cache-control" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody, headers(("cache-control" = String))),
        (status = 500, body = ApiErrorBody),
        (status = 400, body = ApiErrorBody),
        (status = 503, body = ApiErrorBody)
    )
)]
async fn artist_artwork(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<ArtistArtworkQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Response, ApiError> {
    music_library(state.registry.db(), &claims, &q.library).await?;
    let belongs: i64 = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM library_entries i JOIN library_membership lc ON lc.item_id=i.id
              WHERE lc.library_id=? AND i.kind='album' AND i.artist_key=?)",
    )
    .bind(&q.library)
    .bind(&key)
    .fetch_one(state.registry.db())
    .await
    .map_err(internal)?;
    if belongs == 0 {
        return Err(hidden("artist"));
    }
    let url: Option<String> = sqlx::query_scalar(
        "SELECT image_url FROM artist_artwork
          WHERE artist_key=? AND outcome='ready' AND image_url IS NOT NULL",
    )
    .bind(&key)
    .fetch_optional(state.registry.db())
    .await
    .map_err(internal)?
    .flatten();
    let portrait = match url {
        Some(url) => match state
            .artwork
            .get_cached_remote_at(&url, q.size.as_deref())
            .await
        {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(artist = %key, error = %error, "artist artwork could not be served");
                return Err(ApiError::new(ErrorCode::Internal, "artwork unavailable"));
            }
        },
        None => None,
    };
    let found = match portrait {
        Some(portrait) => Some(portrait),
        None => state
            .artwork
            .get_cached_artist_collage_at(&state.registry, &q.library, &key, q.size.as_deref())
            .await
            .map_err(internal)?,
    };
    match found {
        Some((bytes, ctype)) => Ok((
            [
                (axum::http::header::CONTENT_TYPE, ctype),
                (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            bytes,
        )
            .into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            [(
                axum::http::header::CACHE_CONTROL,
                if q.v.is_some() {
                    "private, max-age=3600"
                } else {
                    "private, max-age=30"
                },
            )],
            axum::Json(ApiErrorBody::new(ErrorCode::NotFound, "no artwork")),
        )
            .into_response()),
    }
}

/// Fetch item artwork
///
/// Serves the item's artwork, using the size query parameter or the original
/// when it is absent or unknown. Missing artwork returns a cacheable 404;
/// pass v as a cache-buster. Accepts a bearer token or the media cookie.
#[utoipa::path(
    get, path = "/api/v1/items/{id}/artwork", tag = "Item media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(("id" = String, Path), ArtworkQuery),
    responses(
        (status = 200, content((Vec<u8> = "image/jpeg"), (Vec<u8> = "image/png"), (Vec<u8> = "image/webp")), headers(("cache-control" = String))),
        (status = 401, body = ApiErrorBody),
        // Cacheable, unlike every other refusal — see the handler. The body is
        // an ApiErrorBody like the rest, including the grant gate's 404 on this
        // same operation, which a client would otherwise have to guess at.
        (status = 404, body = ApiErrorBody, headers(("cache-control" = String))),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_artwork(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<ArtworkQuery>,
) -> Result<Response, ApiError> {
    let library_item_id = crate::library::resolve_id(state.registry.db(), &id)
        .await
        .map_err(internal)?;
    let copies = crate::library::copies(state.registry.db(), &claims.sub, &id)
        .await
        .map_err(internal)?;
    if copies.is_empty() {
        return Err(hidden("item"));
    }
    // The detail goes to the log, not to the caller: what fails here is
    // usually a fetch from a metadata provider, and its error names the
    // upstream URL. SEC-WEB-7 — a provider's address is not the client's
    // business, and "the poster did not load" is all an <img> can use.
    let mut found = None;
    let mut failed = false;
    for copy in copies {
        let eligible: bool =
            sqlx::query_scalar("SELECT metadata_eligible AND EXISTS(SELECT 1 FROM collection_item_library_items WHERE collection_item_id=ci.id AND library_item_id=?2 AND ordinal=1) FROM collection_items ci WHERE id=?1")
                .bind(&copy).bind(&library_item_id)
                .fetch_one(state.registry.db())
                .await
                .map_err(internal)?;
        if !eligible {
            continue;
        }
        match state
            .artwork
            .get_at(&state.registry, &state.sessions, &copy, q.size.as_deref())
            .await
        {
            Ok(Some(artwork)) => {
                found = Some(artwork);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(item = %library_item_id, collection_item = %copy, error = %e, "artwork could not be served");
                failed = true;
            }
        }
    }
    if found.is_none() && failed {
        return Err(ApiError::new(ErrorCode::Internal, "artwork unavailable"));
    }
    match found {
        Some((bytes, ctype)) => Ok((
            [
                (axum::http::header::CONTENT_TYPE, ctype),
                // Local artwork changes only on rescan; let clients keep it.
                // Knowingly asymmetric with the miss below: a versionless HIT
                // is also cached for a day, so an episode row can show a
                // re-matched show's old poster until then. Kept because the
                // costs are not alike — shortening this spends image bandwidth
                // on every card, while a miss costs one cheap query.
                (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
            ],
            bytes,
        )
            .into_response()),
        // Cacheable only when the URL can change. A provider with no poster
        // for a release is answered with nothing written to disk on purpose —
        // an upload later is picked up with nothing to invalidate — but an
        // uncacheable 404 meant every render of a shelf of coverless albums was
        // a live request per card, repeated on every scroll back, route change
        // and second tab, and doubled by the srcset.
        //
        // The version is what makes caching safe, and one caller deliberately
        // omits it: an episode row asks for its SHOW's poster, and pinning the
        // parent's URL with the child's version would be a cache key that lies.
        // Caching a miss under a URL that never changes would hide a poster
        // that arrived minutes later for the rest of the hour.
        None => Ok((
            StatusCode::NOT_FOUND,
            // Half a minute for a URL that cannot change: enough to collapse
            // the per-render, per-scroll-back storm inside one browse, short
            // enough that a poster arriving moments later is not hidden.
            // `no-store` was the first answer and gave the storm back; an hour
            // hides the poster. Revalidation is not an option — this 404
            // carries no validator, so a conditional request costs a full one.
            if q.v.is_some() {
                [(axum::http::header::CACHE_CONTROL, "private, max-age=3600")]
            } else {
                [(axum::http::header::CACHE_CONTROL, "private, max-age=30")]
            },
            axum::Json(ApiErrorBody::new(ErrorCode::NotFound, "no artwork")),
        )
            .into_response()),
    }
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct VttQuery {
    /// f64 so a client that computed a fractional shift still works.
    #[serde(default)]
    shift_ms: f64,
}

/// Fetch a subtitle track file
///
/// Serves a track by id as {id}.vtt (shiftable with shift_ms), {id}.ass, or
/// {id}.jsonl for rasterised tracks. Image tracks have no text form and
/// return 422. Accepts a bearer token or the media cookie.
#[utoipa::path(
    get, path = "/api/v1/items/{id}/subtitles/{file}", tag = "Item media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(
        ("id" = String, Path),
        ("file" = String, Path),
        VttQuery
    ),
    responses(
        (status = 200, content((String = "text/vtt; charset=utf-8"), (String = "text/x-ssa; charset=utf-8"), (Vec<u8> = "application/x-ndjson; charset=utf-8")), headers(("cache-control" = String))),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 422, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_subtitle_file(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath((id, file)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<VttQuery>,
) -> Result<Response, ApiError> {
    let track_id = file
        .split('.')
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "bad track id"))?;
    let id = resource_track(&state, &claims.sub, &id, track_id)
        .await?
        .item_id;

    // The public keyspace is TRACK IDS ({id}.vtt / {id}.ass); the
    // resolver maps them onto the internal cache/pipeline notation.
    let resolve = |raw: &str| -> Option<i64> { raw.parse().ok() };
    // Image tracks have no text form — their deliveries are overlay
    // and burn. Refuse FAST: the extraction ladder would otherwise
    // stall for tens of seconds asking the mediahost for cues that
    // cannot exist, and a pending <track> load keeps the browser's own
    // buffering overlay latched over a playing video (found live:
    // Firefox + a burn track's phantom .vtt request).
    let refuse_image = |track: &crate::tracks::Track| -> Result<(), ApiError> {
        if crate::tracks::is_image_format(&track.format) {
            return Err(ApiError::new(
                ErrorCode::UnsupportedTrack,
                format!(
                    "track {} is {} (image): no text form — use overlay or burn delivery",
                    track.id, track.format
                ),
            ));
        }
        Ok(())
    };
    // HUB-32d: a rasterised track is display sets, served whole from
    // the cache. Unlike the embedded overlay tap it is item-level and
    // complete before the first byte goes out, so it needs no session
    // and no tail-following — the client fetches it once, like a .vtt.
    if let Some(raw) = file.strip_suffix(".jsonl") {
        let track_id =
            resolve(raw).ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "bad track id"))?;
        let track = crate::tracks::get_for_item(state.registry.db(), &id, track_id)
            .await
            .map_err(internal)?
            .filter(|t| t.origin == "raster")
            .ok_or(ApiError::new(
                ErrorCode::NotFound,
                "no such rasterised track",
            ))?;
        let bytes = tokio::fs::read(
            state
                .subtitles
                .raster_path(track.payload_id.unwrap_or(track.id)),
        )
        .await
        .map_err(|e| {
            // `tokio::fs::read` fails ONLY with an io error, and
            // `refusal_or_internal` reads any io error as ours — so its
            // refusal arm could never be reached here: every failure was a
            // 500 and this route's declared 404 was dead. The row is
            // INSERTed before the payload is written, so a missing file is
            // an ordinary orphan and the client's business; anything else
            // is the disk, and ours.
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::new(
                    ErrorCode::NotFound,
                    "that rasterised track has no body on disk",
                )
            } else {
                internal(e)
            }
        })?;
        return Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "application/x-ndjson; charset=utf-8",
            )],
            bytes,
        )
            .into_response());
    }
    if let Some(raw) = file.strip_suffix(".ass") {
        let track_id =
            resolve(raw).ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "bad track id"))?;
        let track = state
            .subtitles
            .internal_key(&state.registry, &id, track_id)
            .await
            .map_err(|e| refusal_or_internal(ErrorCode::NotFound, "no such subtitle track", e))?;
        refuse_image(&track)?;
        let body = state
            .subtitles
            .ass_body(&state.registry, &state.sessions, &track)
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
    let track_id =
        resolve(raw).ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "bad track id"))?;
    let track = state
        .subtitles
        .internal_key(&state.registry, &id, track_id)
        .await
        .map_err(|e| refusal_or_internal(ErrorCode::NotFound, "no such subtitle track", e))?;
    refuse_image(&track)?;
    let vtt = state
        .subtitles
        .vtt(
            &state.registry,
            &state.sessions,
            &track,
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
        if !matches!(
            c.media_type,
            kahawai_mediadb::MediaType::Series | kahawai_mediadb::MediaType::Anime
        ) {
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
            pending_sources: report.as_ref().map(|r| r.pending_segments),
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

/// Intro detector status
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

/// Wake segment discovery on eligible mediahosts
///
/// Admin only. Wakes connected protocol-4 mediahosts; each mediahost retains
/// ownership of local cohort selection and priority.
#[utoipa::path(
    post, path = "/admin/v1/segments", tag = "Admin segments",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = SegmentRunResponse),
        (status = 401, body = ApiErrorBody),
        (status = 403, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn admin_segments_run(
    State(state): State<AppState>,
) -> Result<Json<SegmentRunResponse>, ApiError> {
    let status = segment_collections(&state).await?;
    let modules: Vec<String> = status
        .collections
        .into_iter()
        .filter(|c| c.enabled != Some(false))
        .map(|c| c.mediahost_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let asked = state.registry.wake_discovery("segments", &modules).await;
    Ok(Json(SegmentRunResponse {
        asked,
        unavailable: modules.len() - asked,
    }))
}

/// List subtitle fonts for an item
///
/// Lists the attachment fonts available for rendering this item's subtitles,
/// in the order their indices are addressed by the font download route.
#[utoipa::path(
    get, path = "/api/v1/items/{id}/fonts", tag = "Item media",
    security(("bearer_auth" = [])),
    params(("id" = String, Path), SourceQuery),
    responses(
        (status = 200, body = FontsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_fonts(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<SourceQuery>,
) -> Result<Json<FontsResponse>, ApiError> {
    let (id, source_id) = resource_source_copy(&state, &claims.sub, &id, q.source_id).await?;
    let fonts = state
        .subtitles
        .fonts(&state.registry, &state.sessions, &id, Some(source_id))
        .await
        .map_err(internal)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    Ok(Json(FontsResponse { fonts }))
}

/// Download one subtitle font
///
/// Returns the nth font from this item's font list as font/ttf. Accepts a
/// bearer token or the media cookie; an index that does not exist returns
/// 404.
#[utoipa::path(
    get, path = "/api/v1/items/{id}/fonts/{n}", tag = "Item media",
    security(("bearer_auth" = []), ("media_token" = [])),
    params(
        ("id" = String, Path),
        ("n" = usize, Path),
        SourceQuery
    ),
    responses(
        (status = 200, body = Vec<u8>, content_type = "font/ttf", headers(("cache-control" = String))),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_font(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath((id, n)): ApiPath<(String, usize)>,
    ApiQuery(q): ApiQuery<SourceQuery>,
) -> Result<Response, ApiError> {
    let (id, source_id) = resource_source_copy(&state, &claims.sub, &id, q.source_id).await?;
    let fonts = state
        .subtitles
        .fonts(&state.registry, &state.sessions, &id, Some(source_id))
        .await
        .map_err(internal)?;
    let (_, bytes) = fonts
        .into_iter()
        .nth(n)
        .ok_or(ApiError::new(ErrorCode::NotFound, "no such font"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "font/ttf"),
            (axum::http::header::CACHE_CONTROL, "private, max-age=86400"),
        ],
        bytes,
    )
        .into_response())
}

/// List libraries
///
/// Returns the libraries this account may see, ordered by name. An account
/// restricted by library grants receives only those it was granted.
#[utoipa::path(
    get, path = "/api/v1/libraries", tag = "Browse",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, body = LibrariesResponse),
        (status = 401, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn list_libraries(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<LibrariesResponse>, ApiError> {
    let db = state.registry.db();
    let restricted = crate::grants::restricted(db, &claims)
        .await
        .map_err(internal)?;
    let mut query = if restricted {
        sqlx::query(
            "SELECT l.id, l.name, l.media_type FROM libraries l
               JOIN user_libraries ul ON ul.library_id = l.id AND ul.user_id = ?
              ORDER BY l.name",
        )
    } else {
        sqlx::query("SELECT id, name, media_type FROM libraries ORDER BY name")
    };
    if restricted {
        query = query.bind(&claims.sub);
    }
    let rows = query.fetch_all(db).await.map_err(internal)?;
    let libraries = rows
        .iter()
        .map(|row| LibrarySummary {
            id: row.get("id"),
            name: row.get("name"),
            media_type: row.get("media_type"),
        })
        .collect();
    Ok(Json(LibrariesResponse { libraries }))
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ArtistsQuery {
    /// Music library whose Album Artists are being browsed.
    library: String,
    /// Folded substring of the Album Artist display name.
    q: Option<String>,
    /// `name` (default) or `-name`.
    sort: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ArtistAlbumsQuery {
    /// Music library providing navigation and access context.
    library: String,
    /// Album or child-track title substring.
    q: Option<String>,
    /// `year` (default), `-year`, `title`, `-title`, `added`, or `-added`.
    sort: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn music_library(
    db: &crate::library::Database,
    claims: &crate::auth::Claims,
    library: &str,
) -> Result<(), ApiError> {
    let restricted = crate::grants::restricted(db, claims)
        .await
        .map_err(internal)?;
    if restricted
        && !crate::grants::can_see_library(db, claims, library)
            .await
            .map_err(internal)?
    {
        return Err(hidden("library"));
    }
    let media_type: Option<String> =
        sqlx::query_scalar("SELECT media_type FROM libraries WHERE id=?")
            .bind(library)
            .fetch_optional(db)
            .await
            .map_err(internal)?;
    match media_type.as_deref() {
        None => Err(hidden("library")),
        Some("music") => Ok(()),
        Some(_) => Err(ApiError::new(
            ErrorCode::BadRequest,
            "artists belong to a music library",
        )),
    }
}

fn artist_group_sql(descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    format!(
        "SELECT artists.key,artists.name,artists.album_count,
                CASE WHEN aa.outcome='ready' THEN aa.updated_at END AS portrait_version
           FROM (
           SELECT i.artist_key AS key,MIN(i.artist) AS name,COUNT(*) AS album_count
             FROM library_entries i JOIN (SELECT DISTINCT item_id,library_id FROM library_membership) lc ON lc.item_id=i.id
            WHERE lc.library_id=?1 AND i.kind='album'
              AND i.artist_key IS NOT NULL AND i.artist IS NOT NULL
              AND (?2='' OR i.norm_artist LIKE '%' || ?2 || '%')
            GROUP BY i.artist_key
         ) artists
           LEFT JOIN artist_artwork aa ON aa.artist_key=artists.key
          ORDER BY name COLLATE NOCASE {direction},key {direction}"
    )
}

/// Browse the Album Artists in a music library
#[utoipa::path(
    get, path = "/api/v1/artists", tag = "Browse",
    security(("bearer_auth" = [])),
    params(ArtistsQuery),
    responses(
        (status = 200, body = ArtistsResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, body = ApiErrorBody)
    )
)]
async fn list_artists(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<ArtistsQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ArtistsResponse>, ApiError> {
    let db = state.registry.db();
    music_library(db, &claims, &q.library).await?;
    let limit = q.limit.unwrap_or(ITEMS_PAGE_DEFAULT).min(ITEMS_PAGE_MAX);
    let offset = q.offset.unwrap_or(0);
    let needle = q.q.as_deref().map(crate::enrich::fold).unwrap_or_default();
    let descending = q.sort.as_deref() == Some("-name");
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{} LIMIT ?3 OFFSET ?4",
        artist_group_sql(descending)
    )))
    .bind(&q.library)
    .bind(&needle)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT i.artist_key)
           FROM library_entries i JOIN (SELECT DISTINCT item_id,library_id FROM library_membership) lc ON lc.item_id=i.id
          WHERE lc.library_id=?1 AND i.kind='album'
            AND i.artist_key IS NOT NULL AND i.artist IS NOT NULL
            AND (?2='' OR i.norm_artist LIKE '%' || ?2 || '%')",
    )
    .bind(&q.library)
    .bind(&needle)
    .fetch_one(db)
    .await
    .map_err(internal)?;
    Ok(Json(ArtistsResponse {
        artists: rows
            .into_iter()
            .map(|row| {
                let key: String = row.get("key");
                ArtistSummary {
                    art_version: row
                        .get::<Option<i64>, _>("portrait_version")
                        .or_else(|| state.artwork.artist_collage_version(&q.library, &key)),
                    key,
                    name: row.get("name"),
                    album_count: row.get("album_count"),
                }
            })
            .collect(),
        total,
        limit,
        offset,
    }))
}

fn artist_album_order(sort: Option<&str>, alias: &str, metadata: &str) -> String {
    // An album's release year can come from an embedded tag (`items.year`) or
    // from MusicBrainz (`resolved_metadata.premiered`). The card has always
    // displayed that resolved value; ordering on only the former made an
    // enriched catalogue look alphabetic under "Oldest first".
    let year = format!("COALESCE({alias}.year,CAST(substr({metadata}.premiered,1,4) AS INTEGER))");
    match sort.unwrap_or("year") {
        "-year" => format!("{year} IS NULL,{year} DESC,{alias}.sort_title,{alias}.id"),
        "title" => format!("{alias}.sort_title,{year},{alias}.id"),
        "-title" => format!("{alias}.sort_title DESC,{year} DESC,{alias}.id DESC"),
        "added" => format!("{alias}.id,{alias}.sort_title"),
        "-added" => format!("{alias}.id DESC,{alias}.sort_title"),
        _ => format!("{year} IS NULL,{year},{alias}.sort_title,{alias}.id"),
    }
}

/// Browse one Album Artist's records
#[utoipa::path(
    get, path = "/api/v1/artists/{key}/albums", tag = "Browse",
    security(("bearer_auth" = [])),
    params(("key" = String, Path, description = "Opaque URL-safe key returned by the artist index"), ArtistAlbumsQuery),
    responses(
        (status = 200, body = ArtistAlbumsResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 503, body = ApiErrorBody)
    )
)]
async fn artist_albums(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<ArtistAlbumsQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ArtistAlbumsResponse>, ApiError> {
    let db = state.registry.db();
    music_library(db, &claims, &q.library).await?;
    let artist_row = sqlx::query(
        "SELECT MIN(i.artist) AS name,COUNT(*) AS album_count,
                (SELECT updated_at FROM artist_artwork
                  WHERE artist_key=?2 AND outcome='ready') AS portrait_version
           FROM library_entries i JOIN (SELECT DISTINCT item_id,library_id FROM library_membership) lc ON lc.item_id=i.id
          WHERE lc.library_id=?1 AND i.kind='album' AND i.artist_key=?2
         HAVING COUNT(*) > 0",
    )
    .bind(&q.library)
    .bind(&key)
    .fetch_optional(db)
    .await
    .map_err(internal)?
    .ok_or_else(|| hidden("artist"))?;
    let artist = ArtistSummary {
        key: key.clone(),
        name: artist_row.get("name"),
        album_count: artist_row.get("album_count"),
        art_version: artist_row
            .get::<Option<i64>, _>("portrait_version")
            .or_else(|| state.artwork.artist_collage_version(&q.library, &key)),
    };
    let limit = q.limit.unwrap_or(ITEMS_PAGE_DEFAULT).min(ITEMS_PAGE_MAX);
    let offset = q.offset.unwrap_or(0);
    let needle = q.q.as_deref().map(crate::enrich::fold).unwrap_or_default();
    let order_in = artist_album_order(q.sort.as_deref(), "c", "candidate_md");
    let order_out = artist_album_order(q.sort.as_deref(), "i", "md");
    let inner = format!(
        "SELECT c.id AS item_id FROM library_entries c JOIN (SELECT DISTINCT item_id,library_id FROM library_membership) lc ON lc.item_id=c.id
          LEFT JOIN resolved_metadata candidate_md ON candidate_md.item_id=c.representative_id
          WHERE lc.library_id=?2 AND c.kind='album' AND c.artist_key=?3
            AND (?4='' OR c.norm_title LIKE '%' || ?4 || '%'
                 OR c.sort_title LIKE '%' || ?4 || '%'
                 OR EXISTS(SELECT 1 FROM album_copies at JOIN library_entries t ON t.id=at.song_id WHERE at.album_id=c.id
                            AND EXISTS(SELECT 1 FROM collection_items ac JOIN library_collections alc ON (alc.module_id,alc.collection_id)=(ac.module_id,ac.collection_id)
                                WHERE ac.id=at.collection_item_id AND alc.library_id=?2)
                            AND (t.norm_title LIKE '%' || ?4 || '%'
                                 OR t.sort_title LIKE '%' || ?4 || '%')))
          ORDER BY {order_in} LIMIT ?5 OFFSET ?6"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(item_page_sql(
        &inner, &order_out, false, true,
    )))
    .bind(&claims.sub)
    .bind(&q.library)
    .bind(&key)
    .bind(&needle)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await
    .map_err(internal)?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM library_entries c JOIN (SELECT DISTINCT item_id,library_id FROM library_membership) lc ON lc.item_id=c.id
          WHERE lc.library_id=?1 AND c.kind='album' AND c.artist_key=?2
            AND (?3='' OR c.norm_title LIKE '%' || ?3 || '%'
                 OR c.sort_title LIKE '%' || ?3 || '%'
                 OR EXISTS(SELECT 1 FROM album_copies at JOIN library_entries t ON t.id=at.song_id WHERE at.album_id=c.id
                            AND EXISTS(SELECT 1 FROM collection_items ac JOIN library_collections alc ON (alc.module_id,alc.collection_id)=(ac.module_id,ac.collection_id)
                                WHERE ac.id=at.collection_item_id AND alc.library_id=?1)
                            AND (t.norm_title LIKE '%' || ?3 || '%'
                                 OR t.sort_title LIKE '%' || ?3 || '%')))",
    )
    .bind(&q.library)
    .bind(&key)
    .bind(&needle)
    .fetch_one(db)
    .await
    .map_err(internal)?;
    Ok(Json(ArtistAlbumsResponse {
        artist,
        albums: rows
            .iter()
            .map(|row| item_row(row, row.get::<i64, _>("sources")))
            .collect(),
        total,
        limit,
        offset,
    }))
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct ItemsQuery {
    library: Option<String>,
    /// Search: a substring of the title, folded the same way titles are
    /// stored so accents and case do not matter.
    q: Option<String>,
    /// `title` (default), `year`, `added`. Prefixed with `-` for
    /// descending: `-year`.
    sort: Option<String>,
    /// Meaningfully started and not finished — at least one minute and 1%
    /// of the runtime — which is what a "continue watching" row is made
    /// of. Ordered by when you last watched it, so `sort` and `q` do not
    /// apply; `library` still scopes it.
    in_progress: Option<bool>,
    /// A page, not the catalogue. Absent means the default page size
    /// rather than everything; the cap is applied either way.
    limit: Option<u32>,
    offset: Option<u32>,
}

/// Page sizes. The cap exists so a client cannot ask for the old
/// behaviour by accident; the default is a screenful of cards with room
/// to scroll past the fold.
const ITEMS_PAGE_DEFAULT: u32 = 200;
const ITEMS_PAGE_MAX: u32 = 1000;

/// An accidental start is not something to resume. Requiring both one minute
/// and one percent keeps a long film out after a brief probe without making a
/// short episode wait for a film-sized absolute cutoff.
const CONTINUE_MIN_POSITION_MS: i64 = 60_000;
const CONTINUE_MIN_RUNTIME_FRACTION: i64 = 100;

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
fn item_page_sql(inner: &str, order_out: &str, _restricted: bool, _scoped: bool) -> String {
    library_api::page(
        &format!("SELECT *,item_id AS id FROM ({inner})"),
        &order_out.replace("i.", "c."),
    )
}

/// Browse and search items
///
/// Returns one page of items with the caller's watch state, filtered by
/// library or title search and sorted by title, year or date added. Pass
/// `in_progress=true` to list meaningfully started, unfinished items instead.
#[utoipa::path(
    get, path = "/api/v1/items", tag = "Browse",
    security(("bearer_auth" = [])),
    params(ItemsQuery),
    responses(
        (status = 200, body = ItemsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn list_items(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<ItemsQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ItemsResponse>, ApiError> {
    library_api::browse(&state, &claims, q).await.map(Json)
}

/// How long "recent" lasts for the up-next row: a month, taken as 30
/// days. Two independent things are measured against it — when this
/// account last finished an episode of a series, and when the episode it
/// would watch next was added — and either one alone keeps the series in
/// the row.
const UP_NEXT_WINDOW_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct UpNextQuery {
    /// Scopes the row to one library, as on the browse. There is no
    /// `sort` and no `q`: the order is when you last watched the series,
    /// and one episode per series is not a thing to search.
    library: Option<String>,
    /// A page, not the catalogue — the browse's default and cap.
    limit: Option<u32>,
    offset: Option<u32>,
}

/// The eligible set for [`up_next`]: one row per series, carrying the
/// episode to play next and the series' last viewing.
///
/// Shared verbatim by the page and its count, so the two cannot come to
/// disagree — a total that does not match the rows it counts is a paging
/// bug that only shows up on the last page.
///
/// `member` is the caller's reach, correlated on the SERIES alias `c`
/// rather than on the episode: membership only ever holds top-level
/// items, and an episode belongs to a library through its show.
///
/// Bindings: `?1` the account, `?2` the library (bound even when
/// unscoped, so the numbering is uniform), `?3` the watched-since cut,
/// `?4` the added-since cut.
///
/// Driven from `user_item_state` for the reason continue watching is: the
/// set is "series this account has finished an episode of", which is
/// small by construction, so this reads one key range of one user's rows
/// instead of asking every show whether it has been started.
fn up_next_from(member: &str) -> String {
    let meaningful = meaningful_unfinished("pw");
    let next_visible = library_api::visible("n");
    let progress_visible = library_api::visible("pe");
    let next_scope = member.replace("c.id", "n.id");
    let progress_scope = member.replace("c.id", "pe.id");
    format!(
        "FROM (SELECT e.series_id AS show_id, MAX(w.updated_at) AS last_watched
                 FROM user_item_state w
                 JOIN episode_details e ON e.item_id = w.item_id
                WHERE w.user_id = ?1 AND w.played = 1
                  AND EXISTS(SELECT 1 FROM library_items watched WHERE watched.id=e.item_id
                              AND watched.kind='episode' AND watched.merged_into IS NULL)
                GROUP BY e.series_id) seen
         -- What to play next: the first episode, in (season, episode, id)
         -- order, that comes after the last one finished and has not
         -- itself been finished. A series with nothing after it leaves
         -- the row entirely, and it is this join that drops it — there is
         -- no such thing as an up-next entry with no episode in it.
         --
         -- A gap is skipped rather than gone back for: what follows the
         -- last thing watched is the question, so finishing S01E05 out of
         -- order offers S01E06 and not the four before it.
         --
         -- A null season or episode is ordered, not excluded. SQLite
         -- sorts NULL before every value ascending and after it
         -- descending, which is exactly where COALESCE(...,-1) puts it —
         -- so the ORDER BYs stay on `episodes_series` while the row-value
         -- comparison, where one NULL makes the whole predicate NULL and
         -- silently drops the row, spells the -1 out.
         -- Read candidates through their series coordinates. Filtering the
         -- parent projected by library_entries hides that indexed range and
         -- scans every episode; active-work and grant checks are keyed to
         -- each candidate instead.
         JOIN library_entries nx ON nx.id = (
               SELECT nd.item_id FROM episode_details nd
                WHERE nd.series_id = seen.show_id
                  AND EXISTS(SELECT 1 FROM library_entries n WHERE n.id=nd.item_id
                              AND n.kind='episode' AND {next_visible} {next_scope})
                  AND (COALESCE(nd.season, -1), COALESCE(nd.episode, -1), nd.item_id) >
                      (SELECT COALESCE(pd.season, -1), COALESCE(pd.episode, -1), p.id
                         FROM episode_details pd JOIN library_entries p ON p.id=pd.item_id
                         JOIN user_item_state pw ON pw.item_id = p.id
                          AND pw.user_id = ?1 AND pw.played = 1
                        WHERE pd.series_id = seen.show_id AND p.kind = 'episode'
                        -- Last is temporal. The sequence order is only the
                        -- deterministic tie-breaker for a batch mark whose
                        -- rows share one second-resolution timestamp.
                        ORDER BY pw.updated_at DESC,
                                 pd.season DESC, pd.episode DESC, p.id DESC LIMIT 1)
                  AND NOT EXISTS (SELECT 1 FROM user_item_state nw
                                   WHERE nw.user_id = ?1 AND nw.item_id = nd.item_id
                                     AND nw.played = 1)
                ORDER BY nd.season, nd.episode, nd.item_id LIMIT 1)
         -- Meaningfully part-way through an episode of this series? Then the
         -- series belongs to continue watching and not here. Deliberately the
         -- same one-minute-and-one-percent predicate that row is made of, so
         -- the two rows partition the series between them instead of both
         -- claiming one or neither offering it. A barely opened next episode
         -- remains eligible here.
        WHERE EXISTS(SELECT 1 FROM library_entries c WHERE c.id=seen.show_id
                      AND c.kind='series' {member})
          AND NOT EXISTS (SELECT 1 FROM library_entries pe
                            JOIN user_item_state pw ON pw.item_id = pe.id AND pw.user_id = ?1
                           WHERE pe.parent_id = seen.show_id AND {progress_visible} {progress_scope}
                             AND {meaningful})
          -- Still current, either way round: you watched one lately, or
          -- the one you would watch next arrived lately — the season
          -- that starts up again after a year off is the case the second
          -- half is for. `nx.id` IS when it was added: item ids are
          -- ULIDs, which sort lexicographically by the moment they were
          -- minted, and `sort=-added` orders the browse by the same
          -- thing, so the two cannot drift apart.
          AND (seen.last_watched >= ?3 OR nx.id >= ?4)"
    )
}

/// What to watch next, per series
///
/// One episode per series: the one after the last you finished. A series
/// is here when you have finished an episode of it, are not meaningfully
/// part-way through one (that is continue watching's row, and the two never
/// both claim a series), and it is still current — you watched an episode
/// within the last month, or the episode you would watch next was added
/// within it. Most recently watched series first; `library` scopes it and
/// grants bind it, as on the browse.
#[utoipa::path(
    get, path = "/api/v1/up-next", tag = "Browse",
    security(("bearer_auth" = [])),
    params(UpNextQuery),
    responses(
        (status = 200, body = ItemsResponse),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn up_next(
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<UpNextQuery>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ItemsResponse>, ApiError> {
    let limit = q.limit.unwrap_or(ITEMS_PAGE_DEFAULT).min(ITEMS_PAGE_MAX);
    let offset = q.offset.unwrap_or(0);
    let db = state.registry.db();

    // HUB-10, resolved once and for the same reason the browse does it:
    // an unrestricted account pays nothing for grants it does not have.
    let restricted = crate::grants::restricted(db, &claims)
        .await
        .map_err(internal)?;
    if restricted
        && let Some(library) = &q.library
        && !crate::grants::can_see_library(db, &claims, library)
            .await
            .map_err(internal)?
    {
        return Err(hidden("library"));
    }
    let member = match (&q.library, restricted) {
        (Some(_), _) => {
            "AND EXISTS(SELECT 1 FROM library_membership lc WHERE lc.library_id=?2 AND lc.item_id=c.id)"
        }
        (None, true) => crate::grants::VISIBLE_C,
        (None, false) => "",
    };

    // One clock for both cuts. They are two readings of the same "a
    // month ago", and taking one from `SystemTime` and the other from
    // `unixepoch()` would let them disagree for no reason at all.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let since_ms = now_ms.saturating_sub(UP_NEXT_WINDOW_SECS * 1000);
    let watched_since = (since_ms / 1000) as i64;
    // The smallest ULID that could have been minted at the cut, so "added
    // since" is a plain string comparison against ids that are already in
    // that order. An id that is not a ULID — a fixture, or a row from
    // before they were — is not being dated by this and does not pretend
    // to be.
    let added_since = ulid::Ulid::from_parts(since_ms, 0).to_string();

    let body = up_next_from(member);
    let sql = item_page_sql(
        &format!(
            "SELECT nx.id AS item_id, seen.last_watched AS last_watched {body}
              ORDER BY seen.last_watched DESC, nx.id DESC LIMIT ?5 OFFSET ?6"
        ),
        // The SERIES' last viewing, which is the inner query's own
        // column: the watch row the dressing join brings is the next
        // episode's, and it has none — not having one is what makes it
        // next. Ends in a unique column, so a tie cannot straddle a page
        // boundary two different ways.
        "page.last_watched DESC, i.id DESC",
        restricted,
        true,
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&claims.sub)
        .bind(q.library.as_deref().unwrap_or(""))
        .bind(watched_since)
        .bind(&added_since)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await
        .map_err(internal)?;
    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) {body}")))
        .bind(&claims.sub)
        .bind(q.library.as_deref().unwrap_or(""))
        .bind(watched_since)
        .bind(&added_since)
        .fetch_one(db)
        .await
        .map_err(internal)?;
    if !rows.is_empty() {
        let mut hints = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT ps.item_id,ps.module_id,ps.collection_id,r.root_token,f.path_rel \
             FROM library_sources ps \
             JOIN playable_source_parts p ON p.playable_source_id=ps.id AND p.ordinal=1 \
             JOIN files f ON f.id=p.file_id \
             JOIN collection_roots r ON r.id=f.root_id WHERE 0",
        );
        hints.push(" OR ps.item_id IN (");
        let mut separated = hints.separated(",");
        for row in &rows {
            separated.push_bind(row.get::<String, _>("id"));
        }
        separated.push_unseparated(
            ") ORDER BY ps.item_id,ps.module_id,f.mtime_unix DESC,f.path_rel,ps.id",
        );
        let hint_rows = hints.build().fetch_all(db).await.map_err(internal)?;
        let mut hinted = std::collections::HashSet::new();
        for row in hint_rows {
            let item_id: String = row.get("item_id");
            let module_id: String = row.get("module_id");
            if !hinted.insert((item_id, module_id.clone())) {
                continue;
            }
            state.registry.hint_discovery(
                &module_id,
                "segments",
                row.get("collection_id"),
                kahawai_proto::v1::SourcePath::new(
                    row.get::<String, _>("root_token"),
                    row.get::<String, _>("path_rel"),
                ),
                "up-next",
                30 * 60,
            );
        }
    }
    let items = rows
        .iter()
        .map(|row| item_row(row, row.get::<i64, _>("sources")))
        .collect();
    Ok(Json(ItemsResponse {
        items,
        total,
        limit,
        offset,
    }))
}

#[derive(Serialize, ToSchema)]
#[schema(as = ItemRow<S>)]
struct ItemRow<S> {
    id: String,
    /// Album position context; the same song may occur more than once.
    #[serde(skip_serializing_if = "Option::is_none")]
    album_track_id: Option<i64>,
    kind: String,
    title: String,
    /// Album Artist for albums; recording Artist for tracks; absent elsewhere.
    #[schema(required)]
    artist: Option<String>,
    #[schema(required)]
    match_confidence: Option<String>,
    #[schema(required)]
    art_version: Option<i64>,
    #[schema(required)]
    premiered: Option<String>,
    #[schema(required)]
    file_title: Option<String>,
    #[schema(required)]
    file_year: Option<i64>,
    #[schema(required)]
    matched_title: Option<String>,
    #[schema(required)]
    year: Option<i64>,
    /// Season number for episodes; tagged physical disc number for tracks.
    #[schema(required)]
    season: Option<i64>,
    #[schema(required)]
    episode: Option<i64>,
    /// The last episode of a multi-episode file (`E01-E02` stores 2);
    /// null for a single-episode file. Populated on the item detail and
    /// children listings; null on browse pages, like `duration_ms` — so
    /// read it from the detail before acting on it. Per-episode
    /// third-party lookups must skip items that carry one — the single
    /// answer would belong to the first episode only.
    #[schema(required)]
    episode_end: Option<i64>,
    #[schema(required)]
    parent_id: Option<String>,
    #[schema(required)]
    parent_title: Option<String>,
    #[schema(required)]
    library_id: Option<String>,
    /// The DESCRIBING provider's curated numbering, populated on
    /// children listings only. For third-party lookups use
    /// `metadata.proj_season`, which is paired with the keyed id.
    #[schema(required)]
    proj_season: Option<i64>,
    #[schema(required)]
    proj_episode: Option<i64>,
    sources: S,
    #[schema(required)]
    replay_gain: Option<kahawai_core::media::ReplayGain>,
    /// Where a `Play` on this item should begin, in milliseconds. Absent
    /// means the beginning — which is what a played item reports, so
    /// watching something again starts it again rather than dropping the
    /// viewer into its last ten percent. Not "the last position seen":
    /// that is the hub's own business and outlives this field.
    #[schema(required)]
    resume_position_ms: Option<i64>,
    #[schema(required)]
    resume_duration_ms: Option<i64>,
    /// UI-4: the running time the FILES state, as opposed to
    /// `resume_duration_ms`, which is what a player last reported while
    /// watching. An album track list had neither, so it printed no times at
    /// all: a track nobody has played has no watch state to borrow one from.
    ///
    /// Summed across a source's parts and minimised across alternatives, over
    /// the sources that could actually play — an incomplete one undercounts,
    /// and the minimum would otherwise prefer exactly that.
    ///
    /// On a detail and on children. **Null on a browse page**, which does not
    /// reach `files`: a card shows a title and a poster, and resolving a
    /// running time for every row of a page is a cost that buys nothing there.
    #[schema(required)]
    duration_ms: Option<i64>,
    /// Whether the latest progress report or explicit mark says it is finished.
    played: bool,
}

fn item_row<S>(r: &sqlx::sqlite::SqliteRow, sources: S) -> ItemRow<S> {
    let played = r.get::<Option<i64>, _>("played").unwrap_or(0) != 0;
    ItemRow {
        id: r.get("id"),
        album_track_id: r.try_get("album_track_id").ok().flatten(),
        kind: r.get("kind"),
        title: r.get("title"),
        artist: r.try_get("artist").ok().flatten(),
        match_confidence: r.try_get("match_confidence").ok().flatten(),
        // Artwork is cached hard by the browser (a day), so the URL has
        // to change when the metadata does — otherwise re-matching an
        // item leaves yesterday's poster on the card.
        art_version: library_api::artwork_version(r),
        premiered: r.try_get("premiered").ok().flatten(),
        file_title: r.try_get("file_title").ok().flatten(),
        file_year: r.try_get("file_year").ok().flatten(),
        matched_title: r.try_get("matched_title").ok().flatten(),
        year: r.get("year"),
        season: r.get("season"),
        episode: r.get("episode"),
        episode_end: r.try_get("episode_end").ok().flatten(),
        parent_id: r.try_get("parent_id").ok().flatten(),
        parent_title: r.try_get("parent_title").ok().flatten(),
        library_id: r.try_get("library_id").ok().flatten(),
        proj_season: r.try_get("proj_season").ok().flatten(),
        proj_episode: r.try_get("proj_episode").ok().flatten(),
        sources,
        replay_gain: r
            .try_get::<Option<String>, _>("replay_gain")
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok()),
        // `try_get`, because the browse queries do not select it — only the
        // ones that reach `files`, which is where a running time lives.
        duration_ms: r.try_get("file_duration_ms").ok().flatten(),
        // A played item has nowhere to resume: the next Play starts at the
        // beginning, which is what clears `played` on that watch's first
        // progress report. Answered here rather than by storing a zero,
        // because `user_item_state.position_ms` is ALSO where a re-dispatched
        // transcode picks the stream back up (AR-6, `sessions.rs`) — zeroing
        // it would restart a failed-over film from the top for anyone in its
        // last ten minutes.
        resume_position_ms: (!played).then(|| r.get("position_ms")).flatten(),
        resume_duration_ms: r.try_get("duration_ms").ok().flatten(),
        played,
    }
}

/// List an item's children
///
/// Returns the direct children of a show or album — episodes or tracks —
/// ordered by season and episode, each with running time, source count and
/// the caller's watch state.
#[utoipa::path(
    get, path = "/api/v1/items/{id}/children", tag = "Items",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = ChildrenResponse),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_children(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ChildrenResponse>, ApiError> {
    library_api::children(&state, &claims, &id).await.map(Json)
}
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

#[derive(Default, Serialize, ToSchema)]
struct ItemMetadata {
    #[schema(required)]
    overview: Option<String>,
    #[schema(required)]
    rating: Option<f64>,
    #[schema(required)]
    premiered: Option<String>,
    confidence: String,
    #[schema(required)]
    provider: Option<String>,
    /// The TMDB id a third-party lookup should key on: the parent
    /// show's for an episode, the item's own for a parentless item, and
    /// never anything for an item under a non-show parent (a track's
    /// album keys nothing). Taken from that provider's stored answer
    /// whether or not it is the provider describing the item, so it can
    /// be set alongside `tvdb_id` and independently of `provider`. Null
    /// when TMDB holds no confident, unrejected answer for the keyed
    /// item — an absent match, an unconfirmed weak guess and a
    /// human-rejected id all read the same. Prefer this over `tvdb_id`
    /// when both are set — `proj_season`/`proj_episode` are paired with
    /// the preferred id, and when a curated numbering exists only for
    /// one provider the ids are narrowed to that provider so the pair
    /// stays coherent. Send the lookup the duration of the rendition
    /// actually playing.
    #[schema(required)]
    tmdb_id: Option<i64>,
    /// The TVDB id, keyed and filtered the same way as `tmdb_id`.
    #[schema(required)]
    tvdb_id: Option<i64>,
    /// The provider's curated season number where the file has none of
    /// its own (absolute-numbered releases). Comes from the same
    /// provider as the preferred id (`tmdb_id` first), so the pair is
    /// safe to send together; lookups should prefer it over `season`.
    #[schema(required)]
    proj_season: Option<i64>,
    /// The curated episode number, paired with `proj_season`.
    #[schema(required)]
    proj_episode: Option<i64>,
    #[schema(required)]
    original_language: Option<String>,
    #[schema(required)]
    genres: Option<Vec<String>>,
    #[schema(required)]
    cast: Option<Vec<CastMember>>,
}

#[derive(Serialize, serde::Deserialize, ToSchema)]
struct CastMember {
    name: String,
    #[schema(required)]
    character: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct RelatedItem {
    kind: String,
    #[schema(required)]
    title: Option<String>,
    #[schema(required)]
    item_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct ItemDetailResponse {
    library_revision: i64,
    copies: Vec<library_api::CollectionCopy>,
    #[serde(flatten)]
    item: ItemRow<Vec<ItemSource>>,
    #[schema(required)]
    show_title: Option<String>,
    /// The chapters the file declares, on the ITEM's timeline —
    /// what a seek bar puts ticks on and what a detail page lists. On the
    /// item rather than per source because a client is playing one of
    /// them: the first COMPLETE, reachable source in `sources` (not
    /// necessarily `sources[0]` — incomplete part sets and offline
    /// renditions are passed over), and on QUERY the source negotiation
    /// actually chose. Do not correlate them with `sources[0]`.
    ///
    /// Empty when the file declares none and when nothing has looked yet.
    /// A player cannot act on the difference, and the file being asked is
    /// the same file either way.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    chapters: Vec<kahawai_core::media::Chapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<ItemMetadata>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    related: Vec<RelatedItem>,
}

#[derive(Serialize, ToSchema)]
struct ItemQueryResponse {
    #[serde(flatten)]
    item: ItemDetailResponse,
    #[serde(flatten)]
    query: ItemQueryResult,
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

/// Get item details
///
/// Returns one item with its playable sources, metadata, chapters, relations
/// and the caller's watch state. It performs no playback negotiation; use
/// QUERY on the same path for that.
#[utoipa::path(
    get, path = "/api/v1/items/{id}", tag = "Items",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    responses(
        (status = 200, body = ItemDetailResponse),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 400, description = "A path segment or query parameter is not the shape this route takes", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_detail(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
) -> Result<Json<ItemDetailResponse>, ApiError> {
    Ok(Json(item_body(&state, &id, &claims.sub, false).await?))
}

/// The item as discovered: one row, its sources, its metadata and its
/// relations. Shared by `GET` and by `QUERY`, which adds the negotiated
/// half on top — one builder, so the two can never drift apart.
///
/// `with_streams` is what separates them: the scan's `MediaInfo` per
/// source is an answer to "what is in the file", and belongs to the
/// request that asked a question about playing it.
async fn collection_body(
    state: &AppState,
    id: &str,
    user_id: &str,
    with_streams: bool,
) -> Result<ItemDetailResponse, ApiError> {
    let item = sqlx::query(
        "SELECT i.id, i.kind, i.season, i.episode, i.episode_end, i.artist,
                COALESCE(md.title, i.title) AS title,
                COALESCE(i.year, CAST(substr(md.premiered, 1, 4) AS INTEGER)) AS year,
                p.id AS parent_id,
                md.updated_at AS art_version,
                i.title AS file_title, i.year AS file_year, md.title AS matched_title,
                CASE WHEN i.match_mode='manual' THEN 'manual' ELSE md.confidence END AS match_confidence,
                COALESCE(pmd.title, p.title) AS show_title,
                (SELECT COUNT(*) FROM playable_sources ps WHERE ps.item_id=i.id) AS sources,
                -- UI-4, the same shape as `item_children`: the running time
                -- the FILES state, once per item, from the sources that could
                -- actually play. One correlated subquery for one row.
                (SELECT MIN(d) FROM (
                   SELECT SUM(json_extract(f2.streams_json, '$.duration_ms')) AS d
                     FROM playable_sources ps2
                     JOIN playable_source_parts psp2 ON psp2.playable_source_id = ps2.id
                     JOIN files f2 ON f2.id = psp2.file_id
                    WHERE ps2.item_id = i.id
                    GROUP BY ps2.id
                   HAVING COUNT(DISTINCT psp2.ordinal) = ps2.expected_parts
                      AND MAX(psp2.ordinal) = ps2.expected_parts
                      AND COUNT(json_extract(f2.streams_json, '$.duration_ms'))
                          = ps2.expected_parts
                 )) AS file_duration_ms,
                w.position_ms, w.duration_ms, w.played, w.play_count
         FROM collection_items i
         LEFT JOIN collection_items p ON p.id = i.parent_id
         LEFT JOIN collection_watch_state w ON w.item_id = i.id AND w.user_id = ?
         LEFT JOIN resolved_metadata md ON md.item_id = i.id
         LEFT JOIN resolved_metadata pmd ON pmd.item_id = p.id
         WHERE i.id = ?",
    )
    .bind(user_id)
    .bind(id)
    .fetch_optional(state.registry.db())
    .await
    .map_err(internal)?
    .ok_or(ApiError::new(ErrorCode::NotFound, "no such item"))?;

    let sources = sqlx::query(
        "SELECT f.module_id,s.name AS host_name,f.collection_id,f.path_rel AS source_path,f.size,f.streams_json,
                COALESCE(f.revision,1) AS revision,
                ps.id AS source_id, p.ordinal AS part, ps.expected_parts AS parts
         FROM playable_sources ps
         JOIN playable_source_parts p ON p.playable_source_id=ps.id
         JOIN files f ON f.id=p.file_id
         LEFT JOIN satellites s ON s.module_id=f.module_id
         WHERE ps.item_id=?
         -- Playback's own order, from `sessions::playable_rows`, which this
         -- claimed to match and did not: that one ranks whole SOURCES by their
         -- weakest part and then lists a source's parts in sequence, and this
         -- ranked individual FILES by size. On a two-CD film the list came
         -- back cd2, cd1 — the preference the player acts on, described
         -- wrongly, on the endpoint whose job is to describe it.
         --
         -- Kept as a copy rather than shared: the two select different
         -- columns and this one has no root join, and a query that is the same
         -- shape is easier to compare than an abstraction over both.
         ORDER BY ps.expected_parts>1,
                  (SELECT MIN(COALESCE(json_extract(f2.streams_json,'$.video[0].height'),0))
                     FROM playable_source_parts p2 JOIN files f2 ON f2.id=p2.file_id
                    WHERE p2.playable_source_id=ps.id) DESC,
                  (SELECT MIN(COALESCE(f2.revision,1))
                     FROM playable_source_parts p2 JOIN files f2 ON f2.id=p2.file_id
                    WHERE p2.playable_source_id=ps.id) DESC,
                  (SELECT SUM(f2.size) FROM playable_source_parts p2
                     JOIN files f2 ON f2.id=p2.file_id
                    WHERE p2.playable_source_id=ps.id) DESC,
                  ps.id,p.ordinal,f.id",
    )
    .bind(id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;

    let chapters = item_chapters(&sources, |module_id| state.registry.is_connected(module_id));
    let sources: Vec<ItemSource> = sources
        .iter()
        .map(|r| {
            let module_id: String = r.get("module_id");
            let catalog_info = with_streams
                .then(|| {
                    serde_json::from_str::<kahawai_core::media::MediaInfo>(
                        r.get::<String, _>("streams_json").as_str(),
                    )
                    .ok()
                })
                .flatten();
            ItemSource {
                media_entry_id: None,
                collection_item_id: id.to_string(),
                available: state.registry.is_connected(&module_id),
                module_id,
                host_name: r.get("host_name"),
                collection_id: r.get("collection_id"),
                path_rel: r.get("source_path"),
                size: r.get("size"),
                revision: r.get("revision"),
                source_id: r.get("source_id"),
                part: r.get("part"),
                parts: r.get("parts"),
                streams: with_streams.then(|| catalog_info.clone().map(ClientMediaInfo::from)),
                catalog_info,
            }
        })
        .collect();

    let show_title = item.get("show_title");
    // Enrichment (own metadata, or the parent show's for episodes).
    let primary_projection =
        crate::providers::episode_projection_pair_sql("p3", "i.episode", "i.episode");
    let meta = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT m.overview, m.rating, m.premiered, m.confidence, m.provider,
                -- The ids a third-party lookup keys on. An episode's own
                -- provider_id names the EPISODE's record; services that
                -- key TV on the show (season/episode alongside) need the
                -- parent's id, so an item with a parent answers only from
                -- the parent — its own id would be the wrong namespace.
                -- Read all answers eligible for the current parent context.
                -- A `.nfo` library elects `local` as its
                -- describing provider while the tmdb answer sits stored
                -- beside it, and each id stands on its own row (the same
                -- shape the OpenSubtitles lookup uses).
                -- Confident or human-vouched answers only: a 'weak' guess
                -- is the wrong title often enough that keying a third-party
                -- lookup on it hands out another film's boundaries — unless
                -- a human confirmed it (`manual_match`, which is all the
                -- confirm action writes) — and a rejected id was rejected
                -- by a human.
                -- Retained answers from an earlier parent context cannot
                -- donate these external ids.
                (SELECT p2.provider_id FROM answer_priority p2
                  WHERE p2.item_id = COALESCE(
                          (SELECT sh.id FROM collection_items sh
                            WHERE sh.id = i.parent_id AND sh.kind = 'show' AND sh.metadata_eligible=1),
                          CASE WHEN i.parent_id IS NULL THEN i.id END)
                    AND p2.provider = 'tmdb' AND p2.provider_id != ''
                    AND (p2.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p2.item_id
                                       AND mm.provider = p2.provider
                                       AND mm.provider_id = p2.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p2.item_id
                                       AND rj.provider = p2.provider
                                       AND rj.provider_id = p2.provider_id)) AS keyed_tmdb,
                (SELECT p2.provider_id FROM answer_priority p2
                  WHERE p2.item_id = COALESCE(
                          (SELECT sh.id FROM collection_items sh
                            WHERE sh.id = i.parent_id AND sh.kind = 'show' AND sh.metadata_eligible=1),
                          CASE WHEN i.parent_id IS NULL THEN i.id END)
                    AND p2.provider = 'tvdb' AND p2.provider_id != ''
                    AND (p2.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p2.item_id
                                       AND mm.provider = p2.provider
                                       AND mm.provider_id = p2.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p2.item_id
                                       AND rj.provider = p2.provider
                                       AND rj.provider_id = p2.provider_id)) AS keyed_tvdb,
                -- Per provider, from the EPISODE's own rows: the curated
                -- numbering must come from the same provider as the id it
                -- will be paired with, or a TVDB projection rides a TMDB id
                -- into another episode's boundaries.
                -- The same trust filters as the ids: a rejected or weak
                -- episode record must not donate its numbering either.
                (SELECT json_extract(({primary_projection}),'$[0]') FROM answer_priority p3
                  WHERE p3.item_id = i.id AND p3.provider = 'tmdb'
                    AND (p3.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p3.item_id
                                       AND mm.provider = p3.provider
                                       AND mm.provider_id = p3.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p3.item_id
                                       AND rj.provider = p3.provider
                                       AND rj.provider_id = p3.provider_id)) AS tmdb_proj_season,
                (SELECT json_extract(({primary_projection}),'$[1]') FROM answer_priority p3
                  WHERE p3.item_id = i.id AND p3.provider = 'tmdb'
                    AND (p3.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p3.item_id
                                       AND mm.provider = p3.provider
                                       AND mm.provider_id = p3.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p3.item_id
                                       AND rj.provider = p3.provider
                                       AND rj.provider_id = p3.provider_id)) AS tmdb_proj_episode,
                (SELECT json_extract(({primary_projection}),'$[0]') FROM answer_priority p3
                  WHERE p3.item_id = i.id AND p3.provider = 'tvdb'
                    AND (p3.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p3.item_id
                                       AND mm.provider = p3.provider
                                       AND mm.provider_id = p3.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p3.item_id
                                       AND rj.provider = p3.provider
                                       AND rj.provider_id = p3.provider_id)) AS tvdb_proj_season,
                (SELECT json_extract(({primary_projection}),'$[1]') FROM answer_priority p3
                  WHERE p3.item_id = i.id AND p3.provider = 'tvdb'
                    AND (p3.confidence = 'auto'
                         OR EXISTS (SELECT 1 FROM manual_match mm
                                     WHERE mm.item_id = p3.item_id
                                       AND mm.provider = p3.provider
                                       AND mm.provider_id = p3.provider_id))
                    AND NOT EXISTS (SELECT 1 FROM rejected_matches rj
                                     WHERE rj.item_id = p3.item_id
                                       AND rj.provider = p3.provider
                                       AND rj.provider_id = p3.provider_id)) AS tvdb_proj_episode,
                -- An episode carries neither; both describe the work, so
                -- they come from the show when the episode has none.
                COALESCE(NULLIF(m.genres, ''), NULLIF(pm.genres, '')) AS genres,
                COALESCE(NULLIF(m.cast_json, ''), NULLIF(pm.cast_json, '')) AS cast_json,
                COALESCE(NULLIF(m.original_language, ''),
                         NULLIF(pm.original_language, '')) AS original_language
         FROM collection_items i
         LEFT JOIN collection_items eligible_parent ON eligible_parent.id=i.parent_id AND eligible_parent.metadata_eligible=1
         JOIN resolved_metadata m ON m.item_id IN (i.id, eligible_parent.id)
         LEFT JOIN resolved_metadata pm ON pm.item_id = eligible_parent.id
         WHERE i.id = ? AND m.provider_id != ''
         ORDER BY m.item_id = i.id DESC LIMIT 1"
    )))
    .bind(id)
    .fetch_optional(state.registry.db())
    .await
    .map_err(internal)?;
    let metadata = meta.map(|m| {
        // Round-trip, not just parse: '007' parses to a DIFFERENT valid id,
        // and a reinterpreted id keys another title's boundaries.
        let keyed = |col: &str| {
            m.get::<Option<String>, _>(col).and_then(|v| {
                v.parse::<i64>()
                    .ok()
                    .filter(|n| *n > 0 && n.to_string() == v)
            })
        };
        let tmdb_id = keyed("keyed_tmdb");
        let tvdb_id = keyed("keyed_tvdb");
        // The projection follows the id a client will key on (tmdb first,
        // matching the client's own preference), never another provider's.
        // And when a curated numbering exists only on the OTHER provider's
        // row, the ids narrow to that provider: serving a tmdb id with no
        // numbering while a usable tvdb pair sits beside it would refuse a
        // lookup that could have worked — an absolute-numbered episode has
        // no file numbering to fall back on.
        let proj = |prefix: &str| {
            (
                m.try_get(format!("{prefix}_proj_season").as_str())
                    .ok()
                    .flatten(),
                m.try_get(format!("{prefix}_proj_episode").as_str())
                    .ok()
                    .flatten(),
            )
        };
        let tmdb_proj: (Option<i64>, Option<i64>) = proj("tmdb");
        let tvdb_proj: (Option<i64>, Option<i64>) = proj("tvdb");
        let (tmdb_id, tvdb_id, (proj_season, proj_episode)) =
            if tmdb_id.is_some() && tmdb_proj.0.is_some() && tmdb_proj.1.is_some() {
                (tmdb_id, tvdb_id, tmdb_proj)
            } else if tvdb_id.is_some() && tvdb_proj.0.is_some() && tvdb_proj.1.is_some() {
                (None, tvdb_id, tvdb_proj)
            } else if tmdb_id.is_some() {
                (tmdb_id, tvdb_id, (None, None))
            } else {
                (None, tvdb_id, (None, None))
            };
        ItemMetadata {
            overview: m.get("overview"),
            rating: m.get("rating"),
            premiered: m.get("premiered"),
            confidence: m.get("confidence"),
            provider: m.try_get("provider").ok().flatten(),
            tmdb_id,
            tvdb_id,
            proj_season,
            proj_episode,
            original_language: m
                .get::<Option<String>, _>("original_language")
                .filter(|language| !language.is_empty()),
            // Stored as JSON; hand them out as arrays rather than making
            // every client parse a string out of a field (HUB-6).
            genres: m
                .get::<Option<String>, _>("genres")
                .and_then(|genres| serde_json::from_str(&genres).ok()),
            cast: m
                .get::<Option<String>, _>("cast_json")
                .and_then(|cast| serde_json::from_str(&cast).ok()),
        }
    });

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
    .bind(id)
    .fetch_all(state.registry.db())
    .await
    .map_err(internal)?;
    let related = related
        .iter()
        .map(|r| RelatedItem {
            kind: r.get("kind"),
            title: r.get("target_title"),
            item_id: r.get("local_id"),
        })
        .collect();
    Ok(ItemDetailResponse {
        library_revision: 0,
        copies: Vec::new(),
        chapters,
        item: item_row(&item, sources),
        show_title,
        metadata,
        related,
    })
}

/// The chapters of the source playback would pick, moved onto the ITEM's
/// timeline. `sources` is already in playback's order and parts are in
/// ordinal order within a source, so this is the first source's parts,
/// each shifted by the running time of the parts before it.
///
/// A part with no running time ends the list rather than guessing: after
/// it, every offset would be wrong, and a chapter mark in the wrong place
/// is worse than no chapter mark.
/// Reads the rows rather than the built `ItemSource`s: those carry their
/// streams only on QUERY, and the detail page asks with GET.
///
/// The group is the first one PLAYBACK COULD PICK, which is stricter than
/// "the first row": `playable_rows` skips incomplete part sets and sources
/// whose mediahost is away, and this must skip them the same way — a
/// cd2-only set that ranks first would otherwise publish cd2's chapters at
/// the start of the timeline, and an offline rendition would supply the
/// page's chapters while playback serves a different file.
fn item_chapters(
    rows: &[sqlx::sqlite::SqliteRow],
    connected: impl Fn(&str) -> bool,
) -> Vec<kahawai_core::media::Chapter> {
    let mut groups: Vec<i64> = Vec::new();
    for row in rows {
        let id = row.get::<i64, _>("source_id");
        if groups.last() != Some(&id) {
            groups.push(id);
        }
    }
    let complete = |id: i64| {
        let parts: Vec<_> = rows
            .iter()
            .filter(|r| r.get::<i64, _>("source_id") == id)
            .collect();
        let expected = parts.first().map(|r| r.get::<i64, _>("parts")).unwrap_or(0);
        parts.len() as i64 == expected
            && parts
                .iter()
                .enumerate()
                .all(|(at, r)| r.get::<i64, _>("part") == at as i64 + 1)
    };
    let reachable = |id: i64| {
        rows.iter()
            .filter(|r| r.get::<i64, _>("source_id") == id)
            .all(|r| connected(r.get::<String, _>("module_id").as_str()))
    };
    // Completeness is law — offsets folded over a missing part are lies —
    // but connectivity is only a preference: when every complete source is
    // offline there is nothing to play AT ALL, and the best-ranked one's
    // chapters are still last week's truth, the same stance `segments`
    // takes for an offline item.
    let eligible = groups
        .iter()
        .copied()
        .find(|&id| complete(id) && reachable(id))
        .or_else(|| groups.iter().copied().find(|&id| complete(id)));
    let Some(chosen) = eligible else {
        return Vec::new();
    };
    group_chapters(
        rows.iter()
            .filter(|r| r.get::<i64, _>("source_id") == chosen)
            .map(|r| serde_json::from_str(r.get::<String, _>("streams_json").as_str()).ok()),
    )
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

/// The question `QUERY /api/v1/items/{id}` answers. Same inputs a
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

/// Query an item for playback
///
/// Returns the item detail plus per-source stream information, intro segments
/// and what this client would be served for the capability profile in the
/// body. Starts no session and reserves nothing.
// Reached through `MethodRouter::fallback`, because axum's `MethodFilter`
// has no extension methods. That fallback swallows EVERY unmatched
// method, so the method is checked here and the `Allow` header written
// by hand — axum's own 405 machinery no longer runs for this route.
// Negotiation stops before anything that would materialise a burn or an
// overlay, so those tiers are reported only when the artefact exists.
#[utoipa::path(
    post,
    path = "/api/v1/items/{id}",
    tag = "Items",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "Library item identifier")),
    request_body = Option<ItemQuery>,
    responses(
        (status = 200, description = "Item details and current playback negotiation", body = ItemQueryResponse, headers(("accept-query" = String))),
        (status = 401, description = "Missing or invalid bearer token", body = ApiErrorBody),
        (status = 404, description = "Item does not exist or is outside the account's libraries", body = ApiErrorBody),
        (status = 400, description = "The QUERY body is not the JSON this route takes", body = ApiErrorBody),
        (status = 405, description = "Answered for any other verb on this path — POST, PUT, DELETE. Declared here because the document has no operation for those: QUERY is registered as this path's method-router fallback", body = ApiErrorBody, headers(("allow" = String), ("accept-query" = String))),
        (status = 415, description = "QUERY requires an application/json body", body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_query(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiPath(id): ApiPath<String>,
    method: axum::http::Method,
    body: Option<ApiJson<ItemQuery>>,
) -> Result<Response, ApiError> {
    if method.as_str() != "QUERY" {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [
                (axum::http::header::ALLOW, "GET, QUERY"),
                (
                    axum::http::HeaderName::from_static("accept-query"),
                    "application/json",
                ),
            ],
            // An error body like every other refusal. It was empty, which on
            // the one route that answers 405 by hand made it the exception to
            // a contract the document states without qualification.
            axum::Json(ApiErrorBody::new(
                ErrorCode::MethodNotAllowed,
                "use GET or QUERY on an item",
            )),
        )
            .into_response());
    }
    // RFC 10008: "Servers MUST fail the request if the Content-Type
    // request field is missing or is inconsistent with the request
    // content." `Json`'s own extractor rejection is that check — it
    // 415s on a missing or wrong type — so a body that failed to
    // extract is a refusal, not an empty query.
    let ApiJson(q) = body.ok_or(ApiError::new(
        ErrorCode::UnsupportedMediaType,
        "QUERY needs a body with Content-Type: application/json",
    ))?;

    let mut out = item_body(&state, &id, &claims.sub, true).await?;
    let id = out.item.id.clone();
    let representative_copy = out.copies[0].id.clone();
    let mut neg = crate::sessions::Negotiation::new(
        &state.sessions,
        &state.registry,
        &claims.sub,
        &id,
        q.profile,
        q.audio_track,
        q.video_track,
        q.subtitle_track,
    )
    .await
    .map_err(|e| {
        // Three answers, because three different things fail here. A track id
        // the REQUEST named is the caller's; a storage failure is the hub's;
        // what is left is about the item. As one 409 they were all final under
        // this contract — the SPA's `startRetry` maps it to `maybe` and stops
        // asking — so a client that asked for track 999 was told the film was
        // unplayable.
        match e.downcast_ref::<crate::sessions::NoSuchTrack>() {
            Some(missing) => ApiError::new(ErrorCode::BadRequest, missing.to_string()),
            // As above: this route's refusals are typed, and what is left is
            // the database.
            None => internal(e),
        }
    })?;
    neg.set_source_audio_tracks(&q.source_audio_tracks)
        .await
        .map_err(internal)?;
    // Nothing to negotiate is an ANSWER, not a failure: a show or an
    // album has no sources of its own, and a movie whose mediahost is
    // offline has none right now. Both are ordinary items whose detail
    // page must still load, so the converged half comes back null with
    // the reason beside it.
    let (parts, info, sp, mode) = match neg
        .best_source(&id, q.mode.as_deref(), None, q.source_id, None)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let unavailable = if out.item.sources.is_empty() {
                ApiErrorBody::new(ErrorCode::Unplayable, "this item has no media of its own")
            } else {
                let code = if e.downcast_ref::<crate::sessions::SourceOffline>().is_some() {
                    ErrorCode::SourceOffline
                } else {
                    ErrorCode::Unplayable
                };
                tracing::warn!(item = %id, code = ?code, error = format!("{e:#}"), "item has nothing to negotiate");
                // Authored, for the reason `session_refusal` gives: the error
                // reaching here is the same one, and its outermost layer is
                // the pipeline's, not a sentence.
                ApiErrorBody::new(
                    code,
                    match code {
                        ErrorCode::SourceOffline => {
                            "the machine holding this file is not connected right now"
                        }
                        _ => "this item cannot be played",
                    },
                )
            };
            let out = ItemQueryResponse {
                item: out,
                query: ItemQueryResult {
                    subtitle_source: None,
                    negotiated: None,
                    segments: crate::segments::for_item(state.registry.db(), &representative_copy)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!(item = %id, error = format!("{e:#}"),
                                "segments unreadable");
                            Vec::new()
                        }),
                    unavailable: Some(unavailable),
                },
            };
            return Ok((
                [(
                    axum::http::HeaderName::from_static("accept-query"),
                    "application/json",
                )],
                Json(out),
            )
                .into_response());
        }
    };

    out.item.duration_ms = if parts.len() > 1 {
        Some(parts.iter().map(|p| p.duration_ms).sum::<u64>())
    } else {
        info.duration_ms
    }
    .map(|ms| ms as i64);
    let (source_id, playable_source_id):(String,i64)=sqlx::query_as("SELECT ps.item_id,ps.id FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id WHERE p.file_id=?").bind(parts[0].file_id.legacy().map_err(internal)?).fetch_one(state.registry.db()).await.map_err(internal)?;
    let mut verdicts = sp.subtitles.clone();
    crate::sessions::fill_verdict_track_ids(&state.registry, &parts, &mut verdicts).await;
    // The unified track list for the source the negotiation ACTUALLY
    // chose. Asking `source_row` instead — as the old listing endpoint
    // did — can name a different file on a multi-source item, and then
    // every delivery describes something that will not be played.
    let mut subtitles = match parts.first() {
        Some(p) => state
            .subtitles
            .list(
                &state.registry,
                &source_id,
                neg.profile(),
                &neg.ass,
                &claims.sub,
                claims.admin,
                (&p.module_id, &p.collection_id, &p.root_token, &p.path_rel),
            )
            .await
            .map_err(internal)?,
        None => Vec::new(),
    };
    // Keep the physical owner inside the track subsystem. A listed track's
    // public item_id must resolve through the same library item as this QUERY.
    for listing in &mut subtitles {
        listing.track.item_id.clone_from(&id);
    }

    // The chapters must describe the file about to PLAY. `item_body` folded
    // the rank-first eligible source's; negotiation picks by COST with rank
    // as tiebreak, and on a multi-rendition item the two can disagree — a 4K
    // HEVC that needs a transcode beside a 1080p that direct-plays. When
    // negotiation chose, its choice supplies the ticks.
    if let Some(of_group) = out
        .item
        .sources
        .iter()
        .find(|s| s.source_id == playable_source_id)
    {
        let group = of_group.source_id;
        let members: Vec<_> = out
            .item
            .sources
            .iter()
            .filter(|s| s.source_id == group)
            .collect();
        // Completeness is law here as it is on GET: offsets folded over a
        // missing part are lies, and a stray duplicate (same module, path
        // and size in another root) can match into an incomplete group.
        // Anything less than complete keeps item_body's guarded choice.
        let complete = members.first().is_some_and(|first| {
            members.len() as i64 == first.parts
                && members
                    .iter()
                    .enumerate()
                    .all(|(at, s)| s.part == at as i64 + 1)
        });
        if complete {
            out.chapters = group_chapters(members.into_iter().map(|s| s.catalog_info.clone()));
        }
    }

    let source = parts.first().and_then(|part| {
        let video = info.video.first();
        out.item
            .sources
            .iter()
            .find(|source| source.source_id == playable_source_id)
            .map(|source| NegotiatedSource {
                source_id: source.source_id,
                module_id: part.module_id.clone(),
                collection_id: part.collection_id.clone(),
                path_rel: part.path_rel.clone(),
                display_width: video.and_then(|stream| stream.display_width),
                display_height: video.and_then(|stream| stream.display_height),
                orientation: video.and_then(|stream| stream.orientation.clone()),
            })
    });
    let out = ItemQueryResponse {
        item: out,
        query: ItemQueryResult {
            subtitle_source: None,
            negotiated: Some(NegotiatedItem {
                source,
                // What negotiation decided. A `remux` may still be dispatched to
                // a transcoder at session start — that is placement, which QUERY
                // does not do because it would claim a box.
                mode,
                cost: sp.cost.as_str().to_string(),
                // Part of "what would I be served": an accurate client learns
                // how long its player must wait before it may start.
                target_duration_secs: sp.target_duration_secs,
                streams: NegotiatedStreams {
                    video: sp.video_verdict,
                    audio: sp.audio_verdict,
                    subtitles: verdicts,
                },
                subtitles,
            }),
            segments: crate::segments::for_item(state.registry.db(), &source_id)
                .await
                .unwrap_or_else(|e| {
                    // Swallowed on purpose — the item must load — but LOUDLY:
                    // silent, a broken migration reads exactly like "nothing
                    // analysed yet" and gets debugged as a detector problem.
                    tracing::warn!(item = %id, error = format!("{e:#}"), "segments unreadable");
                    Vec::new()
                }),
            unavailable: None,
        },
    };
    Ok((
        [(
            axum::http::HeaderName::from_static("accept-query"),
            "application/json",
        )],
        Json(out),
    )
        .into_response())
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
    let report_guard = session.begin_report().await.ok_or_else(session_gone)?;
    session.touch();
    // Pacing (§4.6): the worker throttles its lead over this position.
    state
        .sessions
        .viewer_position(&state.registry, &id, body.position_ms);

    if let Some(captured) = &session.catalogue {
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
        return Ok(Json(ProgressResponse {
            position_ms: body.position_ms,
            played,
        }));
    }
    let duration = session.duration_ms;
    let finished = duration.is_some_and(|d| d > 0 && body.position_ms * 10 >= d * 9);
    // A track keeps no resume POSITION — but it keeps its played mark, which
    // the album page renders per row.
    //
    // Continue-watching is driven off `user_item_state` and excludes tracks by
    // kind, but only AFTER joining `items`: every track ever skipped part-way
    // left a row matching both of its predicates that was then thrown away, and
    // nothing prunes them. A shuffle listener accumulates tens of thousands and
    // the home page pays for all of them, twice, on every load. Storing zero
    // keeps them out of that set for good, and costs nothing else: a record is
    // resumed from its place in the queue, never from a stored offset.
    let is_track = sqlx::query_scalar::<_, String>("SELECT kind FROM library_items WHERE id = ?")
        .bind(&session.item_id)
        .fetch_optional(state.registry.db())
        .await
        .ok()
        .flatten()
        .is_some_and(|k| k == "song");
    let stored_position = if is_track { 0 } else { body.position_ms };
    // A report from the very start is not a statement that this has not
    // been seen — and one arrives for something nobody has touched. The
    // audio queue pings ZERO, every ten seconds, for the track it has
    // preloaded for a gapless handover (`keepalive.ts`: "A position that
    // never moves at all is the gapless preload — it pings zero"), and the
    // video player answers with `resumeMs` — now 0 on a played item —
    // until `loadedmetadata`. Letting either clear `played` erased seen
    // marks a row ahead of the playhead through an album already heard.
    // So zero leaves the mark alone, and the first position that is not
    // zero decides: starting something again clears it within a ping.
    let at_start = body.position_ms == 0;
    let mut tx = state.registry.db().begin().await.map_err(internal)?;
    let library_item_ids = crate::library::canonical_ids(&mut tx, &session.library_item_ids)
        .await
        .map_err(internal)?;
    let visited = session.visit_members(body.position_ms);
    let mut states = Vec::new();
    if session.boundaries.is_empty() {
        for id in &library_item_ids {
            states.push(serde_json::json!({"id":id,"position":stored_position,"duration":duration,"played":finished,"at_start":at_start}));
        }
    } else {
        for boundary in &session.boundaries {
            if body.position_ms < boundary.start_ms {
                continue;
            }
            let id = crate::library::canonical_id(&mut tx, &boundary.library_item_id)
                .await
                .map_err(internal)?;
            let duration = boundary.end_ms - boundary.start_ms;
            let position = body
                .position_ms
                .saturating_sub(boundary.start_ms)
                .min(duration);
            // Seeking into a later member is not a viewing of earlier members.
            if !visited.contains(&boundary.library_item_id) {
                continue;
            }
            states.push(serde_json::json!({"id":id,"position":position,"duration":duration,"played":position.saturating_mul(10)>=duration.saturating_mul(9),"at_start":position==0}));
        }
    }
    if states.is_empty() {
        session
            .last_position_ms
            .store(body.position_ms, std::sync::atomic::Ordering::Relaxed);
        return Ok(Json(ProgressResponse {
            position_ms: body.position_ms,
            played: false,
        }));
    }

    let row = sqlx::query(
        "INSERT INTO user_item_state (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at,resume_source_fingerprint)
         SELECT ?1,json_extract(value,'$.id'),json_extract(value,'$.position'),json_extract(value,'$.duration'),json_extract(value,'$.played'),0,unixepoch(),?3 FROM json_each(?2) WHERE 1
         ON CONFLICT (user_id, item_id) DO UPDATE SET
           position_ms = excluded.position_ms,
           resume_source_fingerprint = CASE WHEN excluded.position_ms=0 THEN user_item_state.resume_source_fingerprint ELSE ?3 END,
           duration_ms = excluded.duration_ms,
           -- Not MAX(): a high-water mark is what made this a counter
           -- wearing a boolean's clothes. `played` is where the playhead
           -- is now — past 90 percent or not — so watching again clears
           -- it, and finishing sets it once more.
           --
           -- Except at zero, which is not an answer to the question: see
           -- `at_start`. NOT `excluded.position_ms`, which is stored as 0
           -- for every track and would freeze their marks in both
           -- directions — this is what the report SAID.
           played = CASE WHEN json_extract((SELECT value FROM json_each(?2) WHERE json_extract(value,'$.id')=excluded.item_id),'$.at_start') THEN played ELSE excluded.played END,
           -- The up-next row reads this as the last time an episode was
           -- finished. A zero report says nothing about `played`, so letting
           -- it refresh the timestamp would make a preload or untouched
           -- restarted player look like a newly completed watch.
           updated_at = CASE WHEN json_extract((SELECT value FROM json_each(?2) WHERE json_extract(value,'$.id')=excluded.item_id),'$.at_start') THEN updated_at ELSE unixepoch() END
         RETURNING played",
    )
    .bind(&claims.sub)
    .bind(serde_json::to_string(&states).map_err(internal)?)
    .bind(&session.source_fingerprint)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    session
        .last_position_ms
        .store(body.position_ms, std::sync::atomic::Ordering::Relaxed);
    drop(report_guard);
    Ok(Json(ProgressResponse {
        position_ms: body.position_ms,
        played: row.get::<i64, _>("played") != 0,
    }))
}

/// The most items one mark may touch. A season is tens and a show is
/// hundreds; past this the caller is doing something other than ticking
/// off what it just listed.
const WATCHED_BATCH_MAX: usize = crate::watch::MAX_BATCH_ITEMS;

#[derive(Deserialize, ToSchema)]
struct WatchedRequest {
    played: bool,
    /// Apply to these items instead of just this one. Every id must be
    /// this item or one of its children — a show's episodes, an album's
    /// tracks — which is what lets one grant check cover the batch:
    /// access is keyed on `COALESCE(parent_id, id)`, so a visible show's
    /// episodes are visible by construction (`grants::can_see`).
    ///
    /// Absent means this item alone.
    #[serde(default)]
    items: Option<Vec<String>>,
}

/// Mark items watched or unwatched
///
/// Sets the played flag for this item, or for up to 2000 of its children
/// named in the body, clearing resume positions. Returns 404 when nothing matched.
#[utoipa::path(
    put, path = "/api/v1/items/{id}/watched", tag = "Items",
    security(("bearer_auth" = [])),
    params(("id" = String, Path)),
    request_body = WatchedRequest,
    responses(
        (status = 200, body = UpdatedResponse),
        (status = 400, body = ApiErrorBody),
        (status = 401, body = ApiErrorBody),
        (status = 404, body = ApiErrorBody),
        (status = 500, body = ApiErrorBody),
        (status = 415, description = "The body needs Content-Type: application/json", body = ApiErrorBody),
        (status = 413, description = "The body is past the hub's buffer limit", body = ApiErrorBody),
        (status = 503, description = "The hub has no administrator yet: `setup_required`", body = ApiErrorBody)
    )
)]
async fn item_set_watched(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    axum::Extension(claims): axum::Extension<crate::auth::Claims>,
    ApiJson(body): ApiJson<WatchedRequest>,
) -> Result<Json<UpdatedResponse>, ApiError> {
    let ids = body.items.unwrap_or_else(|| vec![id.clone()]);
    if ids.is_empty() {
        return Err(ApiError::new(ErrorCode::BadRequest, "no items to mark"));
    }
    if ids.len() > WATCHED_BATCH_MAX {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("at most {WATCHED_BATCH_MAX} items per mark"),
        ));
    }
    // Resolve permanent first-identification aliases in the same transaction
    // as the mark, so a concurrent merge cannot strand a bookmarked item ID.
    let mut tx = state.registry.db().begin().await.map_err(internal)?;
    let id = crate::library::canonical_id(&mut tx, &id)
        .await
        .map_err(internal)?;
    let ids = crate::library::canonical_ids(&mut tx, &ids)
        .await
        .map_err(internal)?;
    let list = serde_json::to_string(&ids).map_err(internal)?;

    // `json_each` unrolls the id list from a single bound parameter — no
    // placeholder building, and none of the caller's text reaches SQL.
    //
    // The join onto `items` is what enforces the boundary: an id that is
    // neither this item nor one of its children simply is not selected, so
    // a marked row is always one the grant check above already covered.
    // Ids outside it are skipped rather than reported, because saying
    // which ones failed would answer questions about items the caller
    // cannot see. The response lists what WAS marked; a caller that cares
    // can compare.
    //
    // `duration_ms` is absent from the SET list, so an existing one
    // survives being marked watched. Progress overwrites it because
    // progress has just measured it; this has not.
    let visible = library_api::visible("i");
    let album = library_api::album_child("i", "?4");
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO user_item_state (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at)
         SELECT ?1, i.id, 0, NULL, ?3, 0, unixepoch()
           FROM library_entries i
          WHERE i.id IN (SELECT value FROM json_each(?2))
            AND (i.id = ?4 OR i.parent_id = ?4 OR {album}) AND {visible}
         ON CONFLICT (user_id, item_id) DO UPDATE SET
           position_ms = 0,
           played = excluded.played,
           updated_at = unixepoch()
         RETURNING item_id, played"
    )))
    .bind(&claims.sub)
    .bind(&list)
    .bind(body.played)
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;

    if rows.is_empty() {
        // Nothing matched: either the item does not exist, or none of the
        // ids belong to it. The same 404 as a hidden item, deliberately —
        // which of the two it was is not the caller's business.
        return Err(hidden("item"));
    }
    let updated = rows
        .iter()
        .map(|row| WatchUpdate {
            item_id: row.get("item_id"),
            position_ms: 0,
            played: row.get::<i64, _>("played") != 0,
        })
        .collect();
    Ok(Json(UpdatedResponse { updated }))
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
                let track = if session.catalogue.is_some() {
                    session.catalogue_track(num.parse().map_err(|_| hidden("track"))?)
                } else {
                    crate::tracks::get_for_item(
                        state.registry.db(),
                        &session.collection_item_id,
                        num.parse().map_err(|_| hidden("track"))?,
                    )
                    .await
                    .map_err(internal)?
                }
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
        refusal_or_internal, retire_deleted_segment_link, same_fields,
    };
    use std::collections::BTreeMap;

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

    #[tokio::test]
    async fn deleting_a_satellite_wakes_its_segment_waiter() {
        let detector = crate::segments::Detector::new();
        let current = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reply = detector.wait_for_segment_result("host", 7, current, "job");

        retire_deleted_segment_link(&detector, "host", Some(7));

        assert!(matches!(
            reply.await.unwrap(),
            Err(crate::segments::SegmentJobFailure::Disconnected)
        ));
    }

    #[tokio::test]
    async fn candidate_artwork_serves_cached_bytes_only_with_a_valid_ticket() {
        use super::{Arc, Auth, NetOptions, Registry, StatusCode, legacy_router_fixture as router};
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open_legacy_fixture(dir.path()).await.unwrap();
        let registry = Arc::new(Registry::new(
            db.clone(),
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        ));
        let auth = Arc::new(Auth::new(db, dir.path()).await.unwrap());
        let enricher = Arc::new(crate::enrich::Enricher::new(dir.path().into()));
        let artwork_dir = dir.path().join("artwork");
        std::fs::create_dir_all(&artwork_dir).unwrap();
        let remote = "https://image.tmdb.org/t/p/w154/test.jpg";
        let bytes = b"cached provider image";
        std::fs::write(
            artwork_dir.join(format!(
                "tmdb-{:016x}",
                xxhash_rust::xxh3::xxh3_64(remote.as_bytes())
            )),
            bytes,
        )
        .unwrap();
        let ticket = auth.candidate_artwork_ticket(remote).unwrap();
        let ca = Arc::new(crate::pki::HubCa::load_or_create(dir.path()).unwrap());
        let enrollments = Arc::new(crate::enrollment_service::EnrollmentService::new(
            ca,
            registry.clone(),
            std::time::Duration::from_secs(900),
            90,
        ));
        let app = router(
            registry,
            auth,
            Arc::new(crate::sessions::Sessions::new(dir.path().join("scratch"))),
            enrollments,
            Arc::new(crate::subtitles::Subtitles::new(dir.path().join("subs"))),
            Arc::new(crate::artwork::Artwork::new(artwork_dir, enricher.clone())),
            enricher,
            Arc::new(crate::segments::Detector::new()),
            NetOptions::default(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/candidate-artwork?ticket={ticket}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        let response = app
            .oneshot(
                Request::get(format!("/api/v1/candidate-artwork?ticket={ticket}x"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
            ("get", "/api/v1/candidate-artwork"),
            ("post", "/api/v1/setup"),
            ("post", "/api/v1/auth/token"),
            ("post", "/api/v1/auth/refresh"),
            ("post", "/api/v1/auth/logout"),
            ("get", "/api/v1/events"),
            ("get", "/api/v1/collections"),
            ("get", "/api/v1/libraries"),
            ("get", "/api/v1/artists"),
            ("get", "/api/v1/artists/{key}/albums"),
            ("get", "/api/v1/artists/{key}/artwork"),
            ("get", "/api/v1/items"),
            ("get", "/api/v1/up-next"),
            ("get", "/api/v1/items/{id}"),
            ("query", "/api/v1/items/{id}"),
            ("get", "/api/v1/items/{id}/children"),
            ("put", "/api/v1/items/{id}/watched"),
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
            ("post", "/api/v1/items/{id}/subtitles/search"),
            ("post", "/api/v1/items/{id}/subtitles/download"),
            ("delete", "/api/v1/subtitles/{track_id}"),
            ("get", "/api/v1/items/{id}/artwork"),
            ("get", "/api/v1/items/{id}/subtitles/{file}"),
            ("get", "/api/v1/items/{id}/fonts"),
            ("get", "/api/v1/items/{id}/fonts/{n}"),
            ("get", "/api/v1/prefs"),
            ("put", "/api/v1/prefs"),
            ("get", "/api/v1/account/opensubtitles"),
            ("post", "/api/v1/account/opensubtitles"),
            ("delete", "/api/v1/account/opensubtitles"),
            ("post", "/api/v1/playback/sessions"),
            ("delete", "/api/v1/playback/sessions/{id}"),
            ("post", "/api/v1/playback/sessions/{id}/progress"),
            ("post", "/api/v1/playback/sessions/{id}/seek"),
            ("post", "/api/v1/catalogue/libraries/{id}/items/{item_id}"),
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
            ("get", "/admin/v1/libraries"),
            ("post", "/admin/v1/libraries"),
            ("delete", "/admin/v1/libraries/{id}"),
            ("post", "/admin/v1/libraries/{id}/collections"),
            (
                "delete",
                "/admin/v1/libraries/{id}/collections/{module_id}/{collection_id}",
            ),
            ("get", "/admin/v1/collections"),
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
            ("post", "/admin/v1/libraries/{id}/refresh"),
            ("get", "/admin/v1/enrich/review"),
            ("post", "/admin/v1/enrich/search"),
            ("post", "/admin/v1/collection-items/{id}/match"),
            ("post", "/admin/v1/items/{id}/match"),
            ("put", "/admin/v1/library-items/{id}/metadata"),
            ("get", "/admin/v1/sessions"),
            ("delete", "/admin/v1/sessions/{id}"),
            ("get", "/admin/v1/sessions/{id}/log"),
            ("get", "/admin/v1/items/{id}/log"),
            ("get", "/admin/v1/segments"),
            ("post", "/admin/v1/segments"),
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
        assert!(
            document["paths"]["/api/v1/items/{id}"]
                .get("post")
                .is_none()
        );
        let query_request_schema = &document["paths"]["/api/v1/items/{id}"]["query"]["requestBody"]
            ["content"]["application/json"]["schema"];
        assert!(
            query_request_schema
                .to_string()
                .contains("#/components/schemas/ItemQuery"),
            "{query_request_schema}"
        );
        assert_eq!(
            document["paths"]["/api/v1/items/{id}"]["query"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/ItemQueryResponse"
        );
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
        for (path, method, name) in [
            ("/admin/v1/libraries/{id}/refresh", "post", "deep"),
            ("/api/v1/items", "get", "library"),
            ("/api/v1/items", "get", "q"),
            ("/api/v1/items", "get", "sort"),
            ("/api/v1/items", "get", "in_progress"),
            ("/api/v1/items", "get", "limit"),
            ("/api/v1/items", "get", "offset"),
            ("/api/v1/up-next", "get", "library"),
            ("/api/v1/up-next", "get", "limit"),
            ("/api/v1/up-next", "get", "offset"),
            ("/api/v1/items/{id}/artwork", "get", "size"),
            ("/api/v1/items/{id}/artwork", "get", "v"),
            ("/api/v1/items/{id}/subtitles/{file}", "get", "shift_ms"),
        ] {
            let parameter = document["paths"][path][method]["parameters"]
                .as_array()
                .and_then(|parameters| {
                    parameters
                        .iter()
                        .find(|parameter| parameter["name"] == name)
                })
                .unwrap_or_else(|| panic!("{method} {path} has no {name} parameter"));
            assert_eq!(parameter["in"], "query", "{method} {path} {name}");
        }
        assert_eq!(
            document["paths"]["/api/v1/items"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/ItemsResponse"
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
        assert_ne!(
            document["paths"]["/api/v1/items/{id}"]["query"]["requestBody"]["required"], true,
            "QUERY body is optional"
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
        assert!(is_required("ItemDetailResponse", "show_title"));
        assert!(!is_required("ItemDetailResponse", "metadata"));
        assert!(!is_required("ItemDetailResponse", "related"));
        let item_source = &document["components"]["schemas"]["ItemRow_Vec_ItemSource"]["properties"]
            ["sources"]["items"];
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
