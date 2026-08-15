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

const TEST_ROOT: &str = "/kahawai-test-root";

/// Built from the real structs, never hand-written JSON. A literal
/// omitting one required field parses to an EMPTY `MediaInfo` —
/// `parse_info` in the negotiator swallows the error — and every stream
/// then negotiates to "none" while shape-only assertions stay green.
/// That is exactly what the hand-written fixture here did until
/// `the_fixture_declarations_actually_parse` caught it.
fn info(subs: &[(&str, &str)]) -> kahawai_core::media::MediaInfo {
    use kahawai_core::media::{AudioStream, MediaInfo, SubtitleStream, VideoStream};
    MediaInfo {
        container: Some("matroska".into()),
        duration_ms: Some(60_000),
        video: vec![VideoStream {
            codec: "h264".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        }],
        audio: vec![AudioStream {
            codec: "aac".into(),
            channels: 2,
            sample_rate: 48_000,
            ..Default::default()
        }],
        subtitles: subs
            .iter()
            .map(|(format, lang)| SubtitleStream {
                format: (*format).into(),
                language: Some((*lang).into()),
            })
            .collect(),
        ..Default::default()
    }
}

/// An item whose only subtitle is ASS, so the ladder has something to
/// resolve. The declaration is what negotiation and the track sync both
/// read, so no bytes are needed to pose the question.
fn ass_rec(path: &str) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        streams_json: serde_json::to_string(&info(&[("ass", "en")])).unwrap(),
        ..rec(path, 200)
    }
}

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        streams_json: serde_json::to_string(&info(&[])).unwrap(),
    }
}

fn test_router(
    registry: Arc<Registry>,
    auth: Arc<kahawai_hub::auth::Auth>,
    sessions: Arc<kahawai_hub::sessions::Sessions>,
    subs_dir: std::path::PathBuf,
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
        Arc::new(kahawai_hub::subtitles::Subtitles::new(subs_dir)),
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

/// `db` and `subs_dir` come back so a test can look for the artefacts
/// QUERY must NOT have produced.
struct Fx {
    reg: Arc<Registry>,
    // Keep this bound for the whole test. Dropping it unlinks hub.db while
    // the router's lazy SQLite pool is still using the directory.
    _dir: tempfile::TempDir,
    api: axum::Router,
    bearer: String,
    id: String,
    db: sqlx::SqlitePool,
    subs_dir: std::path::PathBuf,
}

async fn fixture() -> Fx {
    fixture_with(rec("Heat (1995).mkv", 100)).await
}

/// UI-27: one film in several parts must be tellable from several encodes.
///
/// The list is one row per FILE, ordered by what playback would pick, so both
/// cases read as "N sources" in an order that means nothing. The grouping was
/// in the database — `playable_sources` collects a family and states how many
/// parts it expects — and simply was not in the response, so no client could
/// say what it was looking at.
#[tokio::test]
async fn a_multi_part_film_is_tellable_from_alternative_encodes() {
    // Two CDs of one film, and a second encode of the same film beside them.
    let fx = fixture_with(rec("Heat (1995)/Heat (1995) cd1.mkv", 100)).await;
    fx.reg
        .upsert_files(
            "01H",
            "movies",
            vec![
                rec("Heat (1995)/Heat (1995) cd1.mkv", 100),
                rec("Heat (1995)/Heat (1995) cd2.mkv", 110),
                rec("Heat (1995)/Heat (1995) REPACK.mkv", 400),
            ],
        )
        .await
        .unwrap();

    let response = fx
        .api
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/api/v1/items/{}", fx.id))
                .header("authorization", &fx.bearer)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // `ItemRow<S>`'s generic field: a count when browsing, the rows themselves
    // on a detail.
    let sources = item["sources"].as_array().expect("sources on the detail");

    // Group the way a client would have to, on the fields the response now
    // carries and did not before.
    let mut by_source: std::collections::BTreeMap<i64, Vec<i64>> = Default::default();
    let mut expected: std::collections::BTreeMap<i64, i64> = Default::default();
    for s in sources {
        let id = s["source_id"].as_i64().expect("source_id on every source");
        by_source
            .entry(id)
            .or_default()
            .push(s["part"].as_i64().expect("part on every source"));
        expected.insert(id, s["parts"].as_i64().expect("parts on every source"));
    }

    let multi: Vec<_> = by_source.values().filter(|p| p.len() > 1).collect();
    assert_eq!(
        multi.len(),
        1,
        "expected exactly one multi-part source in {by_source:?}"
    );
    assert_eq!(*multi[0], vec![1, 2], "parts are not numbered in order");
    assert!(
        by_source.len() > 1,
        "the alternative encode did not come back as its own source: {by_source:?}"
    );
    for (id, parts) in &by_source {
        assert_eq!(
            expected[id],
            parts.len() as i64,
            "source {id} reports {} parts and returned {}",
            expected[id],
            parts.len()
        );
    }
}

async fn fixture_with(file: FileUpsertRecord) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    reg.announce_collection("01H", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.upsert_files("01H", "movies", vec![file]).await.unwrap();
    // A source nobody can reach cannot be negotiated against, so the
    // mediahost has to be up for the question to have an answer.
    reg.connected("01H", "mediahost", "mh", "fp", "test");

    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    auth.complete_setup("admin", "password-123").await.unwrap();
    let pair = auth.login("admin", "password-123").await.unwrap();
    let bearer = format!("Bearer {}", pair.access_token);

    let id: String = sqlx::query_scalar("SELECT id FROM items LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let subs_dir = tempfile::tempdir().unwrap().keep();
    let api = test_router(
        reg.clone(),
        auth,
        Arc::new(kahawai_hub::sessions::Sessions::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        subs_dir.clone(),
    );
    Fx {
        reg,
        _dir: dir,
        api,
        bearer,
        id,
        db,
        subs_dir,
    }
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
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
    let profile = r#"{"profile":{"containers":["mp4"],
        "video":[{"codec":"h264"}],"audio":["aac"],
        "hdr":false,"graphics_overlay":false,"ass_render":false,
        "target_duration":{"mode":"ignore"}}}"#;
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
    // A REAL verdict, not merely a present one: "none" and "unplayable"
    // are non-empty strings, so the loose form of this assertion sat
    // green for a whole fixture that never parsed.
    assert_ne!(n["cost"], "unplayable", "{n}");
    assert_ne!(n["streams"]["video"], "none", "{n}");
    assert_ne!(n["streams"]["audio"], "none", "{n}");
    assert!(n["subtitles"].is_array());
}

/// The failure this would otherwise hide: `Router::route_layer` maps
/// `require_auth` onto the method fallback, but `MethodRouter::route_layer`
/// would not. Getting the wrong one serves item data unauthenticated.
#[tokio::test]
async fn query_without_a_token_is_refused() {
    let Fx { _dir, api, id, .. } = fixture().await;
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
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
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
    let Fx {
        _dir,
        api,
        bearer,
        id,
        ..
    } = fixture().await;
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

/// **QUERY is safe.** The whole design rests on this: it reports the
/// plan as it stands and never does the work. Point it at an ASS track
/// with the ladder set overlay-FIRST — the arrangement that makes
/// rasterising the preferred answer — and it must still come back
/// without having rasterised anything.
///
/// The failure this catches is silent and plausible: `overlay_ready`
/// (`subtitles.rs`) generates a `raster` row and its NDJSON, takes up
/// to 30 s, and reaching for it here would make every item page pay a
/// session's start-up cost. Nothing else in the suite would notice,
/// because the ANSWER would be right — only its price would be wrong.
#[tokio::test]
async fn query_rasterises_nothing() {
    let Fx {
        _dir,
        api,
        bearer,
        id,
        db,
        subs_dir,
        ..
    } = fixture_with(ass_rec("Subbed (2011).mkv")).await;

    let user: String = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_prefs (user_id, scope, key, value)
         VALUES (?, '', 'ass_order', 'overlay,flatten,burn')",
    )
    .bind(&user)
    .execute(&db)
    .await
    .unwrap();

    // A client that renders no ASS itself but does composite bitmaps:
    // overlay is both reachable and first, so a QUERY willing to
    // generate would generate here.
    let resp = api
        .oneshot(query(
            &format!("/api/v1/items/{id}"),
            Some(&bearer),
            Some("application/json"),
            // Direct-playable (matroska accepted), so the plan is a real
            // one: an unplayable verdict carries no subtitles at all and
            // would pass this test for the wrong reason.
            r#"{"profile":{"containers":["matroska","mp4"],"video":[{"codec":"h264"}],
                "audio":["aac"],"hdr":false,
                "graphics_overlay":true,"ass_render":false,
                "target_duration":{"mode":"ignore"}}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j = json_of(resp).await;
    let n = &j["negotiated"];
    assert!(
        n.is_object(),
        "the connected fixture must have a negotiation: {}",
        j["unavailable"]
    );
    assert_ne!(n["cost"], "unplayable", "nothing to rasterise FOR: {n}");
    let subs = n["subtitles"].as_array().unwrap();
    assert_eq!(subs.len(), 1, "the ASS track must be listed: {j}");

    // No raster row...
    let rasters: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM subtitle_tracks WHERE origin = 'raster'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(rasters, 0, "QUERY created a raster track row");

    // ...and no raster body on disk. Both, because either one alone
    // could be the half that a partial implementation writes.
    let stray: Vec<String> = std::fs::read_dir(&subs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("raster-"))
        .collect();
    assert!(stray.is_empty(), "QUERY wrote raster artefacts: {stray:?}");

    // And having generated nothing, it must not have PROMISED overlay
    // either: the rung it reports is the one that is real right now.
    assert_ne!(
        subs[0]["delivery"], "overlay",
        "overlay promised with no rasterised track in existence: {j}"
    );
}

/// The fixtures declare their streams as JSON, and `parse_info`
/// swallows a bad parse into an EMPTY `MediaInfo` — which negotiates to
/// "none" for every stream and would make the tests above pass for
/// reasons that have nothing to do with what they claim to test.
#[test]
fn the_fixture_declarations_actually_parse() {
    for r in [rec("x.mkv", 1), ass_rec("y.mkv")] {
        let info: kahawai_core::media::MediaInfo =
            serde_json::from_str(&r.streams_json).unwrap_or_else(|e| panic!("{}: {e}", r.path_rel));
        assert_eq!(info.container.as_deref(), Some("matroska"));
        assert_eq!(info.video.len(), 1, "{}: no video", r.path_rel);
        assert_eq!(info.audio.len(), 1, "{}: no audio", r.path_rel);
    }
    let subs =
        serde_json::from_str::<kahawai_core::media::MediaInfo>(&ass_rec("y.mkv").streams_json)
            .unwrap()
            .subtitles;
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].format, "ass");
}
