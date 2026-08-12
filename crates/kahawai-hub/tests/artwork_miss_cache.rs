//! An artwork miss is cacheable only as long as its URL can change.
//!
//! A provider with no poster for a release is answered with nothing written to
//! disk on purpose, so an upload later is picked up with nothing to invalidate.
//! That made the 404 uncacheable, and a shelf of coverless cards became one live
//! request per card on every render — repeated on scroll-back, a route change, a
//! second tab, and doubled by the `srcset`.
//!
//! Caching it for an hour needs the URL to be able to change, and one caller
//! deliberately omits the version: an episode row asks for its SHOW's poster,
//! because pinning the parent's URL with the child's `art_version` would be a
//! cache key that lies. Under that URL an hour-long cached miss outlives the
//! poster's arrival — the visible bug here, since every track row borrows its
//! album's cover the same way.
//!
//! Declaration-only, so this builds the router directly rather than using
//! `tests/common`: that harness renders real media with ffmpeg and stands up an
//! mTLS mediahost so a session can read bytes, and nothing here opens a lease.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::registry::{FileUpsertRecord, Registry};
use tower::ServiceExt;

struct Fx {
    api: axum::Router,
    bearer: String,
    id: String,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    reg.announce_collection("01H", "movies", "movies", &[])
        .await
        .unwrap();
    reg.upsert_files(
        "01H",
        "movies",
        vec![FileUpsertRecord {
            path_rel: "Heat (1995).mkv".into(),
            size: 100,
            mtime_unix: 1,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: r#"{"container":"matroska"}"#.into(),
        }],
    )
    .await
    .unwrap();
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    let token = auth.setup_token().unwrap();
    let pair = auth
        .complete_setup(&token, "admin", "password-123")
        .await
        .unwrap();
    let id: String = sqlx::query_scalar("SELECT id FROM items LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let ca = Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        reg.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    let api = kahawai_hub::api::router(
        reg,
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            Arc::new(kahawai_hub::enrich::Enricher::new(
                tempfile::tempdir().unwrap().keep(),
            )),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        kahawai_hub::api::NetOptions::default(),
    );
    Fx {
        api,
        bearer: format!("Bearer {}", pair.access_token),
        id,
        _dir: dir,
    }
}

/// The 404's `cache-control`, or `"(absent)"` — which is what a grant refusal or
/// a bad id produces, so neither can satisfy the assertions below.
async fn cache_control(fx: &Fx, uri: &str) -> (StatusCode, String) {
    let resp = fx
        .api
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("authorization", &fx.bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let header = resp
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_else(|| "(absent)".into());
    (status, header)
}

#[tokio::test]
async fn a_versionless_miss_is_barely_cached() {
    let fx = fixture().await;
    let id = &fx.id;

    let (status, versionless) =
        cache_control(&fx, &format!("/api/v1/items/{id}/artwork?size=card")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the fixture has no artwork");
    assert_eq!(
        versionless, "private, max-age=30",
        "long enough to collapse a per-render storm within one browse, short \
         enough that a poster arriving a minute later is not hidden for an hour \
         under a URL that cannot change"
    );

    let (status, versioned) =
        cache_control(&fx, &format!("/api/v1/items/{id}/artwork?size=card&v=7")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        versioned, "private, max-age=3600",
        "a versioned URL is safe to cache for longer: a new poster changes the \
         version, so nothing has to expire for it to appear"
    );
}
