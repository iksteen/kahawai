//! `QUERY /api/v1/items/{id}` (RFC 10008) — the converged half of the
//! item resource.
//!
//! Three of these guard properties that are invisible at a glance and
//! would fail silently:
//!
//! - the route is reached through `MethodRouter::fallback`, because
//!   axum's `MethodFilter` has no extension methods — and whether the
//!   `require_auth` layer reaches a fallback depends on which of two
//!   near-identically-named axum functions the router uses
//!   (`Router::route_layer` maps it, `MethodRouter::route_layer` does
//!   not). An unauthenticated 200 would be the failure;
//! - that fallback swallows EVERY unmatched method, so axum's own 405
//!   machinery stops running and the `Allow` header is ours to write;
//! - RFC 10008 requires rejecting a missing or inconsistent
//!   `Content-Type`.

use std::sync::Arc;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use tower::ServiceExt;

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        streams_json: r#"{"container":"matroska","duration_ms":60000,
            "video":[{"codec":"h264","width":1920,"height":1080}],
            "audio":[{"codec":"aac","channels":2}]}"#
            .into(),
    }
}

fn test_router(
    registry: Arc<Registry>,
    auth: Arc<kahawai_hub::auth::Auth>,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
) -> axum::Router {
    let ca = Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    kahawai_hub::api::router(
        registry,
        auth,
        sessions,
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
    )
}

async fn fixture() -> (tempfile::TempDir, axum::Router, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    reg.announce_collection("01H", "movies", "movies", &[])
        .await
        .unwrap();
    reg.upsert_files("01H", "movies", vec![rec("Heat (1995).mkv", 100)])
        .await
        .unwrap();
    // A source nobody can reach cannot be negotiated against, so the
    // mediahost has to be up for the question to have an answer.
    reg.connected("01H", "mediahost", "mh", "fp", "test");

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
    let bearer = format!("Bearer {}", pair.access_token);

    let id: String = sqlx::query_scalar("SELECT id FROM items LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let api = test_router(
        reg,
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    (dir, api, bearer, id)
}

fn query(
    uri: &str,
    bearer: Option<&str>,
    ctype: Option<&str>,
    body: &str,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method("QUERY").uri(uri);
    if let Some(t) = bearer {
        b = b.header("authorization", t);
    }
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    b.body(axum::body::Body::from(body.to_string())).unwrap()
}

async fn json_of(resp: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// The converged answer: the item, its discovered streams, and what
/// this client would actually be served.
#[tokio::test]
async fn query_returns_the_item_and_what_it_would_be_served() {
    let (_d, api, bearer, id) = fixture().await;
    let profile = r#"{"profile":{"containers":["mp4"],
        "video":[{"codec":"h264"}],"audio":["aac"],
        "hdr":false,"graphics_overlay":false,"ass_render":false}}"#;
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            Some(&bearer),
            Some("application/json"),
            profile,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("accept-query").unwrap(),
        "application/json",
        "the resource must advertise the query format it takes"
    );
    let j = json_of(resp).await;
    assert_eq!(j["title"], "Heat");
    // The discovered half is still there — QUERY is a superset of GET.
    assert_eq!(j["sources"][0]["streams"]["container"], "matroska");
    // ...and the converged half names the source it judged, so a
    // multi-source item cannot describe one file and play another.
    let n = &j["negotiated"];
    assert_eq!(n["source"]["path_rel"], "Heat (1995).mkv");
    assert!(n["cost"].is_string(), "cost missing: {n}");
    assert!(
        n["streams"]["video"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "no video verdict: {n}"
    );
    assert!(n["subtitles"].is_array());
}

/// The failure this would otherwise hide: `Router::route_layer` maps
/// `require_auth` onto the method fallback, but `MethodRouter::route_layer`
/// would not. Getting the wrong one serves item data unauthenticated.
#[tokio::test]
async fn query_without_a_token_is_refused() {
    let (_d, api, _bearer, id) = fixture().await;
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            None,
            Some("application/json"),
            "{}",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "the method fallback bypassed auth");
}

/// The fallback swallows every unmatched method, so axum's own 405
/// response — `Allow` header included — no longer happens for us.
#[tokio::test]
async fn an_unsupported_method_still_says_what_is_allowed() {
    let (_d, api, bearer, id) = fixture().await;
    let resp = api
        .oneshot(
            axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/items/{id}"))
                .header("authorization", &bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.headers().get("allow").unwrap(), "GET, QUERY");
}

/// RFC 10008: "Servers MUST fail the request if the Content-Type
/// request field is missing or is inconsistent with the request
/// content."
#[tokio::test]
async fn a_query_without_a_json_content_type_is_refused() {
    let (_d, api, bearer, id) = fixture().await;
    for ctype in [None, Some("text/plain")] {
        let resp = api
            .clone()
            .oneshot(query(
                &format!("/api/v1/items/{id}"),
                Some(&bearer),
                ctype,
                "{}",
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            415,
            "content-type {ctype:?} should have been refused"
        );
    }
}
