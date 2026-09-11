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

const TEST_ROOT: &str = "/kahawai-test-root";

struct Fx {
    api: axum::Router,
    bearer: String,
    id: String,
    db: kahawai_hub::library::Database,
    artwork_dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));
    reg.announce_collection("01H", "movies", "movies", &[TEST_ROOT.into()])
        .await
        .unwrap();
    reg.upsert_files(
        "01H",
        "movies",
        vec![FileUpsertRecord {
            root_token: kahawai_core::media::root_token(std::path::Path::new(TEST_ROOT)),
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
    auth.complete_setup("admin", "password-123").await.unwrap();
    let pair = auth.login("admin", "password-123").await.unwrap();
    let id: String = sqlx::query_scalar("SELECT id FROM collection_items LIMIT 1")
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
    let artwork_dir = dir.path().join("artwork");
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
            artwork_dir.clone(),
            Arc::new(kahawai_hub::enrich::Enricher::new(
                tempfile::tempdir().unwrap().keep(),
            )),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    );
    Fx {
        api,
        bearer: format!("Bearer {}", pair.access_token),
        id,
        db,
        artwork_dir,
        _dir: dir,
    }
}

#[tokio::test]
async fn artwork_falls_back_only_to_eligible_accessible_collection_copies() {
    let fx = fixture().await;
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type)
         VALUES('01H','other','movies')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id)
         VALUES('zz-with-art','movie','Heat','heat',1995,'01H','other')",
    )
    .execute(&fx.db)
    .await
    .unwrap();
    for (copy, poster) in [(fx.id.as_str(), None), ("zz-with-art", Some("/heat.jpg"))] {
        kahawai_hub::providers::assign_manual(
            &fx.db,
            copy,
            "tmdb",
            "949",
            kahawai_hub::providers::Fields {
                title: Some("Heat".into()),
                premiered: Some("1995-01-01".into()),
                poster_path: poster.map(str::to_owned),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    std::fs::create_dir_all(&fx.artwork_dir).unwrap();
    let cache_key = format!("tmdb-{:016x}", xxhash_rust::xxh3::xxh3_64(b"/heat.jpg"));
    std::fs::write(fx.artwork_dir.join(cache_key), b"second copy poster").unwrap();
    let user: String = sqlx::query_scalar("SELECT id FROM users WHERE username='admin'")
        .fetch_one(&fx.db)
        .await
        .unwrap();
    assert_eq!(
        kahawai_hub::library::copies(&fx.db, &user, &fx.id)
            .await
            .unwrap(),
        vec![fx.id.clone(), "zz-with-art".into()]
    );
    let response = fx
        .api
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/items/{}/artwork", fx.id))
                .header("authorization", &fx.bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .as_ref(),
        b"second copy poster"
    );

    // A source's retained answer cannot describe a different manually assigned work.
    sqlx::query("UPDATE collection_items SET metadata_eligible=0 WHERE id='zz-with-art'")
        .execute(&fx.db)
        .await
        .unwrap();
    assert_eq!(
        cache_control(&fx, &format!("/api/v1/items/{}/artwork", fx.id))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE collection_items SET metadata_eligible=1 WHERE id='zz-with-art'")
        .execute(&fx.db)
        .await
        .unwrap();

    // Even an already authenticated request must not fall through to an ungranted copy.
    sqlx::raw_sql("INSERT INTO libraries(id,name,media_type) VALUES('visible','Visible','movies');
        INSERT INTO library_collections(library_id,module_id,collection_id) VALUES('visible','01H','movies');
        INSERT INTO user_libraries(user_id,library_id) SELECT id,'visible' FROM users;
        UPDATE users SET is_admin=0,all_libraries=0;")
        .execute(&fx.db).await.unwrap();
    assert_eq!(
        cache_control(&fx, &format!("/api/v1/items/{}/artwork", fx.id))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
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

async fn detail(fx: &Fx, id: &str) -> serde_json::Value {
    let response = fx
        .api
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/items/{id}"))
                .header("authorization", &fx.bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn artwork_bytes(fx: &Fx, id: &str) -> Option<Vec<u8>> {
    let response = fx
        .api
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/items/{id}/artwork"))
                .header("authorization", &fx.bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    if response.status() == StatusCode::NOT_FOUND {
        return None;
    }
    assert_eq!(response.status(), StatusCode::OK);
    Some(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
}

#[tokio::test]
async fn independently_matched_episode_borrows_only_an_eligible_parents_metadata_and_artwork() {
    use kahawai_hub::providers::{Fields, assign_manual};
    let fx = fixture().await;
    sqlx::raw_sql("INSERT INTO collections(module_id,collection_id,media_type) VALUES('01H','series','series');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('parent','show','Old show','old show',2000,'01H','series');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
          VALUES('episode','episode','Episode 1','episode 1','parent',1,1,'01H','series');")
        .execute(&fx.db).await.unwrap();
    assign_manual(
        &fx.db,
        "parent",
        "tmdb",
        "101",
        Fields {
            title: Some("Old show".into()),
            premiered: Some("2000-01-01".into()),
            overview: Some("Old parent overview".into()),
            poster_path: Some("/old-parent.jpg".into()),
            genres: Some(r#"["Parent genre"]"#.into()),
            cast_json: Some(r#"[{"name":"Parent actor","character":"Parent role"}]"#.into()),
            original_language: Some("ja".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assign_manual(
        &fx.db,
        "episode",
        "tmdb",
        "10101",
        Fields {
            title: Some("Independent episode".into()),
            overview: Some("Own episode overview".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    std::fs::create_dir_all(&fx.artwork_dir).unwrap();
    for (poster, bytes) in [
        ("/old-parent.jpg", b"parent poster".as_slice()),
        ("/own-episode.jpg", b"own episode poster".as_slice()),
    ] {
        std::fs::write(
            fx.artwork_dir.join(format!(
                "tmdb-{:016x}",
                xxhash_rust::xxh3::xxh3_64(poster.as_bytes())
            )),
            bytes,
        )
        .unwrap();
    }
    let current_episode = || async {
        sqlx::query_scalar::<_, String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='episode' AND ordinal=1")
            .fetch_one(&fx.db).await.unwrap()
    };
    let before = detail(&fx, &current_episode().await).await;
    assert_eq!(before["metadata"]["overview"], "Own episode overview");
    assert_eq!(
        before["metadata"]["genres"],
        serde_json::json!(["Parent genre"])
    );
    assert_eq!(before["metadata"]["cast"][0]["name"], "Parent actor");
    assert_eq!(before["metadata"]["original_language"], "ja");
    assert_eq!(
        artwork_bytes(&fx, &current_episode().await)
            .await
            .as_deref(),
        Some(b"parent poster".as_slice())
    );

    let mut tx = fx.db.begin().await.unwrap();
    let corrected = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "series".into(),
            title: "Correct show".into(),
            year: Some(2020),
            artist: None,
            parent_id: None,
            season: None,
            episode: None,
            edition: None,
        },
    )
    .await
    .unwrap();
    kahawai_hub::library::assign(&mut tx, "parent", &[corrected])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='parent'"
        )
        .fetch_one(&fx.db)
        .await
        .unwrap()
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='episode'"
        )
        .fetch_one(&fx.db)
        .await
        .unwrap()
    );
    let episode = current_episode().await;
    let corrected = detail(&fx, &episode).await;
    assert_eq!(corrected["metadata"]["overview"], "Own episode overview");
    for field in ["genres", "cast", "original_language", "tmdb_id"] {
        assert!(
            corrected["metadata"][field].is_null(),
            "ineligible parent donated {field}: {corrected}"
        );
    }
    assert!(
        artwork_bytes(&fx, &episode).await.is_none(),
        "the retained parent poster is not eligible for this child"
    );

    // Isolate the dependency: changing only parent eligibility must change the
    // child's artwork URL even if its own assignment and metadata stay put.
    let child_revision: i64 =
        sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id='episode'")
            .fetch_one(&fx.db)
            .await
            .unwrap();
    sqlx::query("UPDATE collection_items SET metadata_eligible=1 WHERE id='parent'")
        .execute(&fx.db)
        .await
        .unwrap();
    let eligible = detail(&fx, &episode).await;
    sqlx::query("UPDATE collection_items SET metadata_eligible=0 WHERE id='parent'")
        .execute(&fx.db)
        .await
        .unwrap();
    let ineligible = detail(&fx, &episode).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT assignment_revision FROM collection_items WHERE id='episode'"
        )
        .fetch_one(&fx.db)
        .await
        .unwrap(),
        child_revision
    );
    assert_ne!(
        eligible["art_version"], ineligible["art_version"],
        "the URL fingerprint must drop the ineligible parent poster"
    );

    assign_manual(
        &fx.db,
        "episode",
        "tmdb",
        "10101",
        Fields {
            title: Some("Independent episode".into()),
            overview: Some("Own episode overview".into()),
            poster_path: Some("/own-episode.jpg".into()),
            genres: Some(r#"["Own genre"]"#.into()),
            cast_json: Some(r#"[{"name":"Own actor","character":null}]"#.into()),
            original_language: Some("en".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for parent_eligible in [false, true] {
        sqlx::query("UPDATE collection_items SET metadata_eligible=? WHERE id='parent'")
            .bind(parent_eligible)
            .execute(&fx.db)
            .await
            .unwrap();
        let own = detail(&fx, &current_episode().await).await;
        assert_eq!(own["metadata"]["original_language"], "en");
        assert_eq!(own["metadata"]["genres"], serde_json::json!(["Own genre"]));
        assert_eq!(own["metadata"]["cast"][0]["name"], "Own actor");
        assert_eq!(
            artwork_bytes(&fx, &current_episode().await)
                .await
                .as_deref(),
            Some(b"own episode poster".as_slice())
        );
    }
    assert!(
        fx.artwork_dir
            .join(format!(
                "tmdb-{:016x}",
                xxhash_rust::xxh3::xxh3_64(b"/old-parent.jpg")
            ))
            .exists(),
        "cached parent image remains retained"
    );
}
