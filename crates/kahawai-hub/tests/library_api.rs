use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

async fn harness() -> (
    axum::Router,
    String,
    kahawai_hub::library::Database,
    Arc<kahawai_hub::registry::Registry>,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open_legacy_fixture(dir.path())
        .await
        .unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    ));
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        dir.path().join("sessions"),
    ));
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
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.path().to_path_buf()));
    let artwork_dir = dir.path().join("artwork");
    let api = kahawai_hub::api::legacy_router_fixture(
        registry.clone(),
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            dir.path().join("subtitles"),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            artwork_dir.clone(),
            enricher.clone(),
        )),
        enricher,
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    );
    auth.complete_setup("pager", "hunter22222hunter")
        .await
        .unwrap();
    let token = auth
        .login("pager", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    std::mem::forget(dir);
    (api, token, db, registry, artwork_dir)
}

async fn page(api: &axum::Router, token: &str, uri: &str) -> serde_json::Value {
    let resp = api
        .clone()
        .oneshot(
            Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{uri} -> {}", resp.status());
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap()
}

async fn apply_library_match(
    api: &axum::Router,
    token: &str,
    db: &kahawai_hub::library::Database,
    copy: &str,
    mut body: serde_json::Value,
) -> serde_json::Value {
    body["expected_revision"] =
        sqlx::query_scalar::<_, i64>("SELECT assignment_revision FROM collection_items WHERE id=?")
            .bind(copy)
            .fetch_one(db)
            .await
            .unwrap()
            .into();
    let response = api
        .clone()
        .oneshot(
            Request::post(format!("/admin/v1/collection-items/{copy}/match"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}

async fn provider_pick_fixture(db: &kahawai_hub::library::Database) {
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
      ('a','movie','Work A','work a',2000,'host','movies'),
      ('b','movie','Work B','work b',2001,'host','movies'),
      ('bare','movie','Unknown','unknown',NULL,'host','movies'),
      ('selected','movie','Work A','work a',2000,'host','movies');")
      .execute(db).await.unwrap();
}

fn provider_pick_b() -> serde_json::Value {
    serde_json::json!({"action":"pick","provider":"tmdb","candidate":{
        "id":22,"title":"Work B","release_date":"2001-01-01",
        "overview":"Selected provider description","poster_path":"/chosen.jpg","vote_average":8.5
    }})
}

async fn assert_provider_pick_b(db: &kahawai_hub::library::Database) {
    let pin: (String, String) =
        sqlx::query_as("SELECT provider,provider_id FROM manual_match WHERE item_id='selected'")
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(pin, ("tmdb".into(), "22".into()));
    let metadata: (String, String, String, String, f64) = sqlx::query_as("SELECT provider_id,title,overview,poster_path,rating FROM provider_metadata WHERE item_id='selected' AND provider='tmdb'")
        .fetch_one(db).await.unwrap();
    assert_eq!(
        metadata,
        (
            "22".into(),
            "Work B".into(),
            "Selected provider description".into(),
            "/chosen.jpg".into(),
            8.5
        )
    );
}

#[tokio::test]
async fn provider_pick_preserves_an_unrelated_refusal_after_reset_and_enrichment() {
    let (api, token, db, _, _) = harness().await;
    provider_pick_fixture(&db).await;
    apply_library_match(
        &api,
        &token,
        &db,
        "selected",
        serde_json::json!({"action":"reject"}),
    )
    .await;
    let picked = apply_library_match(&api, &token, &db, "selected", provider_pick_b()).await;
    assert_eq!(picked["library_item_ids"], serde_json::json!(["b"]));
    assert_provider_pick_b(&db).await;
    apply_library_match(
        &api,
        &token,
        &db,
        "selected",
        serde_json::json!({"action":"reset"}),
    )
    .await;
    kahawai_hub::providers::store_answer(
        &db,
        "selected",
        "tmdb",
        "11",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Work A".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let target: String = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='selected'")
        .fetch_one(&db).await.unwrap();
    assert_ne!(
        target, "a",
        "choosing B must not silently reaccept previously refused A"
    );
    let refused: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id='selected' ORDER BY library_item_id")
        .fetch_all(&db).await.unwrap();
    assert_eq!(refused, vec!["a"]);
}

#[tokio::test]
async fn provider_pick_clears_only_the_chosen_canonical_work_refusal() {
    let (api, token, db, _, _) = harness().await;
    provider_pick_fixture(&db).await;
    apply_library_match(
        &api,
        &token,
        &db,
        "selected",
        serde_json::json!({"action":"reject"}),
    )
    .await;
    apply_library_match(
        &api,
        &token,
        &db,
        "selected",
        serde_json::json!({"action":"assign","library_item_ids":["bare"]}),
    )
    .await;
    apply_library_match(
        &api,
        &token,
        &db,
        "selected",
        serde_json::json!({"action":"reject"}),
    )
    .await;
    apply_library_match(
        &api,
        &token,
        &db,
        "bare",
        serde_json::json!({"action":"assign","library_item_ids":["b"]}),
    )
    .await;
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, "bare").await.unwrap(),
        "b"
    );
    let picked = apply_library_match(&api, &token, &db, "selected", provider_pick_b()).await;
    assert_eq!(picked["library_item_ids"], serde_json::json!(["b"]));
    let refused: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM rejected_library_matches WHERE collection_item_id='selected' ORDER BY library_item_id")
        .fetch_all(&db).await.unwrap();
    assert_eq!(
        refused,
        vec!["a"],
        "only B's equivalent alias refusal is superseded by the explicit pick"
    );
    assert_provider_pick_b(&db).await;
}

#[tokio::test]
async fn scoped_song_search_keeps_its_album_and_position_in_that_library() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','music'),('host','two','music');
        INSERT INTO libraries(id,name,media_type) VALUES('A','First','music'),('B','Second','music');
        INSERT INTO library_collections VALUES('A','host','one'),('B','host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id)
        VALUES('album-a','album','First album','first album',2000,'Artist','host','one'),
              ('album-b','album','Second album','second album',2001,'Artist','host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
        VALUES('song-a','track','Shared recording','shared recording','album-a',1,1,'host','one'),
              ('song-b','track','Shared recording','shared recording','album-b',2,7,'host','two');")
        .execute(&db).await.unwrap();
    db.transaction("assign one recording to two albums", |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "song-b", &["song-a".into()]).await })
    })
    .await
    .unwrap();

    for (library, album, disc, track) in [("B", "album-b", 2, 7), ("A", "album-a", 1, 1)] {
        let result = page(
            &api,
            &token,
            &format!("/api/v1/items?library={library}&q=Shared"),
        )
        .await;
        assert_eq!(result["total"], 1, "{result}");
        let song = &result["items"][0];
        assert_eq!(song["id"], "song-a");
        assert_eq!(song["library_id"], library);
        assert_eq!(song["parent_id"], album, "{song}");
        assert_eq!(song["season"], disc);
        assert_eq!(song["episode"], track);
        let children = page(&api, &token, &format!("/api/v1/items/{album}/children")).await;
        assert_eq!(
            song["album_track_id"],
            children["children"][0]["album_track_id"]
        );
    }
}

#[tokio::test]
async fn corrected_album_artist_can_serve_its_cached_portrait() {
    let (api, token, db, _, artwork_dir) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','music','music');
        INSERT INTO libraries(id,name,media_type) VALUES('M','Music','music'),('empty','Empty','music');
        INSERT INTO library_collections VALUES('M','host','music');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id)
        VALUES('album','album','Record','record',2000,'Detected artist','host','music');")
        .execute(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    let corrected = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "album".into(),
            title: "Record".into(),
            year: Some(2000),
            artist: Some("Correct artist".into()),
            parent_id: None,
            season: None,
            episode: None,
            edition: None,
        },
    )
    .await
    .unwrap();
    kahawai_hub::library::assign(&mut tx, "album", &[corrected])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let listed = page(&api, &token, "/api/v1/artists?library=M").await;
    assert_eq!(listed["artists"][0]["name"], "Correct artist");
    let key = listed["artists"][0]["key"].as_str().unwrap();
    let url = "https://image.tmdb.org/correct-artist.jpg";
    sqlx::query("INSERT INTO artist_artwork(artist_key,artist_name,outcome,image_url,source_revision,updated_at) VALUES(?,'Correct artist','ready',?,'fixture',1000)")
        .bind(key).bind(url).execute(&db).await.unwrap();
    std::fs::create_dir_all(&artwork_dir).unwrap();
    std::fs::write(
        artwork_dir.join(format!(
            "tmdb-{:016x}",
            xxhash_rust::xxh3::xxh3_64(url.as_bytes())
        )),
        b"correct artist portrait",
    )
    .unwrap();
    for (library, expected) in [("M", 200), ("empty", 404)] {
        let response = api
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/artists/{key}/artwork?library={library}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        if expected == 200 {
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap()
                    .as_ref(),
                b"correct artist portrait"
            );
        }
    }
    let old_key: String =
        sqlx::query_scalar("SELECT artist_key FROM collection_items WHERE id='album'")
            .fetch_one(&db)
            .await
            .unwrap();
    let old_artist = api
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/artists/{old_key}/artwork?library=M"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        old_artist.status(),
        404,
        "retained collection metadata cannot keep the old artist in this library"
    );
}

#[tokio::test]
async fn artwork_versions_follow_every_eligible_authorized_donor() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
        INSERT INTO libraries(id,name,media_type) VALUES('A','First','movies');
        INSERT INTO library_collections VALUES('A','host','one');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id)
        VALUES('first','movie','Heat','heat',1995,'host','one'),('second','movie','Heat','heat',1995,'host','two');")
        .execute(&db).await.unwrap();
    for copy in ["first", "second"] {
        kahawai_hub::providers::store_answer(
            &db,
            copy,
            "tmdb",
            "949",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Heat".into()),
                premiered: Some("1995-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE provider_metadata SET updated_at=unixepoch()+CASE item_id WHEN 'first' THEN 2000 ELSE 1000 END",
    )
    .execute(&db)
    .await
    .unwrap();
    let before = page(&api, &token, "/api/v1/items").await["items"][0]["art_version"].clone();
    sqlx::query("UPDATE provider_metadata SET poster_path='/new-poster.jpg',updated_at=unixepoch()+1100 WHERE item_id='second'")
        .execute(&db).await.unwrap();
    let after = page(&api, &token, "/api/v1/items").await["items"][0]["art_version"].clone();
    assert_ne!(
        before, after,
        "a lower-timestamp donor still changes the served artwork"
    );
    assert_eq!(
        after,
        page(&api, &token, "/api/v1/items/first").await["art_version"]
    );

    // A donor outside this account's libraries cannot affect its artwork URL.
    sqlx::raw_sql(
        "INSERT INTO user_libraries(user_id,library_id) SELECT id,'A' FROM users;
        UPDATE users SET is_admin=0,all_libraries=0;",
    )
    .execute(&db)
    .await
    .unwrap();
    let restricted = page(&api, &token, "/api/v1/items").await["items"][0]["art_version"].clone();
    sqlx::query("UPDATE provider_metadata SET poster_path='/hidden-change.jpg',updated_at=unixepoch()+1200 WHERE item_id='second'")
        .execute(&db).await.unwrap();
    assert_eq!(
        restricted,
        page(&api, &token, "/api/v1/items").await["items"][0]["art_version"]
    );

    // Retained evidence for a different manually assigned work is not a donor.
    sqlx::query("UPDATE users SET is_admin=1,all_libraries=1")
        .execute(&db)
        .await
        .unwrap();
    kahawai_hub::providers::store_answer(
        &db,
        "second",
        "tmdb",
        "other",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Other movie".into()),
            premiered: Some("1995-01-01".into()),
            poster_path: Some("/other.jpg".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.transaction("retain another work's source evidence", |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "second", &["first".into()]).await })
    })
    .await
    .unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='second'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
    let eligible = page(&api, &token, "/api/v1/items").await["items"][0]["art_version"].clone();
    sqlx::query("UPDATE provider_metadata SET poster_path='/ineligible-change.jpg',updated_at=unixepoch()+1300 WHERE item_id='second'")
        .execute(&db).await.unwrap();
    assert_eq!(
        eligible,
        page(&api, &token, "/api/v1/items").await["items"][0]["art_version"]
    );
}

#[tokio::test]
async fn browse_and_matching_use_library_identity_and_guard_copy_revision() {
    let (api, token, db, registry, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
      ('bare','movie','X-Men','x-men',NULL,'host','one'),('dated','movie','X-Men','x-men',2000,'host','two');")
      .execute(&db).await.unwrap();
    let library = registry.create_library("Movies", "movies").await.unwrap();
    registry
        .attach_collection(&library, "host", "one")
        .await
        .unwrap();
    registry
        .attach_collection(&library, "host", "two")
        .await
        .unwrap();
    assert_eq!(
        page(&api, &token, &format!("/api/v1/items?library={library}")).await["total"],
        2
    );
    let detail = page(&api, &token, "/api/v1/items/bare").await;
    let revision = detail["copies"][0]["assignment"]["revision"]
        .as_i64()
        .unwrap();
    let request = serde_json::json!({"action":"pick","expected_revision":revision,"provider":"tmdb","candidate":{"id":36657,"title":"X-Men","release_date":"2000-01-01"}});
    async fn apply(
        api: &axum::Router,
        token: &str,
        body: &serde_json::Value,
    ) -> axum::response::Response {
        api.clone()
            .oneshot(
                Request::post("/admin/v1/collection-items/bare/match")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    let response = apply(&api, &token, &request).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let assignment: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(assignment["library_item_ids"], serde_json::json!(["dated"]));
    assert_eq!(apply(&api, &token, &request).await.status(), 409);
    let result = page(&api, &token, &format!("/api/v1/items?library={library}")).await;
    assert_eq!(result["total"], 1);
    assert_eq!(result["items"][0]["id"], "dated");
    let detail = page(&api, &token, "/api/v1/items/dated").await;
    assert_eq!(detail["copies"].as_array().unwrap().len(), 2);
    let metadata = serde_json::json!({"expected_revision":detail["library_revision"],"fields":{"overview":"Shared work description"}});
    let response = api
        .clone()
        .oneshot(
            Request::put("/admin/v1/library-items/dated/metadata")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(metadata.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        page(&api, &token, "/api/v1/items/dated").await["metadata"]["overview"],
        "Shared work description"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM provider_metadata WHERE overview='Shared work description'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "work override must leave provider evidence untouched"
    );

    let reject = serde_json::json!({"action":"reject","expected_revision":assignment["revision"]});
    let response = apply(&api, &token, &reject).await;
    assert_eq!(response.status(), 200);
    let result = page(&api, &token, &format!("/api/v1/items?library={library}")).await;
    assert_eq!(result["total"], 2);
}

#[tokio::test]
async fn rejecting_settled_answers_schedules_a_deferred_provider_check() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    for (provider, provider_id) in [("tmdb", "438631"), ("tvdb", "123")] {
        kahawai_hub::providers::store_answer(
            &db,
            "copy",
            provider,
            provider_id,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Dune".into()),
                premiered: Some("2021-09-15".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM enrichment_queue")
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "all providers have settled before the rejection"
    );
    let detail = page(&api, &token, "/api/v1/items/copy").await;
    let response = api.clone().oneshot(Request::post("/admin/v1/collection-items/copy/match")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"action":"reject","expected_revision":detail["copies"][0]["assignment"]["revision"]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let retries: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT provider,reason,due_at-unixepoch() FROM enrichment_queue WHERE item_id='copy' ORDER BY provider",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(retries.len(), 2, "each refused provider must be revisited");
    for ((provider, reason, delay), expected) in retries.into_iter().zip(["tmdb", "tvdb"]) {
        assert_eq!(provider, expected);
        assert_eq!(reason, "match rejected");
        assert!((86_390..=86_400).contains(&delay), "retry delay: {delay}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM provider_metadata WHERE item_id='copy'")
            .fetch_one(&db)
            .await
            .unwrap(),
        2,
        "retaining answers prevents an immediate provider request"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rejected_matches WHERE item_id='copy'")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn anime_children_and_search_keep_native_numbers_and_bridge_projection() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','anime','anime');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('show','show','Anime','anime',2000,'host','anime');
      INSERT INTO collection_items(id,kind,title,norm_title,parent_id,episode,module_id,collection_id) VALUES('episode','episode','Episode 27','episode 27','show',27,'host','anime');")
        .execute(&db).await.unwrap();
    kahawai_hub::providers::store_answer(
        &db,
        "show",
        "anilist",
        "1",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Anime".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    kahawai_hub::providers::store_answer(
        &db,
        "episode",
        "tvdb",
        "201",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("Bridge title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE provider_metadata SET proj_season=2,proj_episode=1 WHERE item_id='episode' AND provider='tvdb'").execute(&db).await.unwrap();
    let children = page(&api, &token, "/api/v1/items/show/children").await;
    let child = &children["children"][0];
    assert_eq!(child["title"], "Bridge title");
    assert!(child["season"].is_null());
    assert_eq!(child["episode"], 27);
    assert_eq!(child["proj_season"], 2);
    assert_eq!(child["proj_episode"], 1);
    let search = page(&api, &token, "/api/v1/items?q=Bridge").await;
    assert_eq!(search["items"][0]["id"], child["id"]);
    assert_eq!(search["items"][0]["proj_season"], 2);
    let detail = page(
        &api,
        &token,
        &format!("/api/v1/items/{}", child["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(detail["title"], "Bridge title");
    assert_eq!(detail["episode"], 27);
}

#[tokio::test]
async fn confirming_a_weak_copy_also_trusts_its_provider_identity() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    kahawai_hub::providers::store_answer(
        &db,
        "copy",
        "tmdb",
        "438631",
        "weak",
        kahawai_hub::providers::Fields {
            title: Some("Dune".into()),
            premiered: Some("2021-09-15".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let detail = page(&api, &token, "/api/v1/items/copy").await;
    assert_eq!(detail["copies"][0]["match_confidence"], "weak");
    assert_eq!(detail["copies"][0]["matched_title"], "Dune");
    assert_eq!(detail["copies"][0]["matched_year"], 2021);
    let response = api.clone().oneshot(Request::post("/admin/v1/collection-items/copy/match")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"action":"confirm","expected_revision":detail["copies"][0]["assignment"]["revision"]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let pinned: Option<(String, String)> =
        sqlx::query_as("SELECT provider,provider_id FROM manual_match WHERE item_id='copy'")
            .fetch_optional(&db)
            .await
            .unwrap();
    assert_eq!(pinned, Some(("tmdb".into(), "438631".into())));
    let confirmed = page(&api, &token, "/api/v1/items/copy").await;
    assert_eq!(confirmed["copies"][0]["match_confidence"], "manual");
    assert_eq!(
        confirmed["copies"][0]["assignment"]["library_item_ids"],
        detail["copies"][0]["assignment"]["library_item_ids"]
    );
}

#[tokio::test]
async fn copy_confirmation_names_its_selected_record_without_display_fallbacks() {
    use kahawai_hub::providers::{Fields, store_answer};
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','series'),('host','two','series');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
        ('show-a','show','Shared Show','shared show',2000,'host','one'),
        ('show-b','show','Shared Show','shared show',2000,'host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,year,parent_id,season,episode,module_id,collection_id) VALUES
        ('episode-a','episode','Episode 1','episode 1',2000,'show-a',1,1,'host','one'),
        ('episode-b','episode','Episode 1','episode 1',2000,'show-b',1,1,'host','two');")
        .execute(&db).await.unwrap();
    for show in ["show-a", "show-b"] {
        store_answer(
            &db,
            show,
            "tmdb",
            "10",
            "auto",
            Fields {
                title: Some("Shared Show".into()),
                premiered: Some("2000-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    for (copy, record, confidence, title, date) in [
        (
            "episode-a",
            "101",
            "auto",
            "Representative episode",
            "2000-01-01",
        ),
        ("episode-b", "202", "weak", "Selected episode", "2001-02-03"),
    ] {
        store_answer(
            &db,
            copy,
            "tmdb",
            record,
            confidence,
            Fields {
                title: Some(title.into()),
                premiered: Some(date.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    // A supplemental local description wins display fields, but it is not
    // the selected provider record the confirmation must describe.
    store_answer(
        &db,
        "episode-b",
        "local",
        "",
        "auto",
        Fields {
            title: Some("Local display title".into()),
            premiered: Some("1988-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let display_title: String =
        sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id='episode-b'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(display_title, "Local display title");

    let copy = |detail: &serde_json::Value, id: &str| {
        detail["copies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|copy| copy["id"] == id)
            .unwrap()
            .clone()
    };
    let detail = page(&api, &token, "/api/v1/items/episode-a").await;
    assert_eq!(detail["copies"].as_array().unwrap().len(), 2, "{detail}");
    let representative = copy(&detail, "episode-a");
    assert_eq!(representative["matched_title"], "Representative episode");
    assert_eq!(representative["matched_year"], 2000);
    let selected = copy(&detail, "episode-b");
    assert_eq!(selected["match_confidence"], "weak");
    assert_eq!(selected["matched_title"], "Selected episode");
    assert_eq!(selected["matched_year"], 2001);

    // Missing selected-record fields remain absent even though both local
    // metadata and the representative copy could supply a plausible fallback.
    store_answer(&db, "episode-b", "tmdb", "202", "weak", Fields::default())
        .await
        .unwrap();
    let detail = page(&api, &token, "/api/v1/items/episode-a").await;
    let selected = copy(&detail, "episode-b");
    assert!(selected["matched_title"].is_null(), "{selected}");
    assert!(selected["matched_year"].is_null(), "{selected}");
}

#[tokio::test]
async fn detail_preserves_confidence_and_reports_each_copys_match() {
    use kahawai_hub::providers::{Fields, assign_manual, store_answer};
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
        ('auto-copy','movie','Men in Black','men in black',1997,'host','one'),
        ('manual-copy','movie','Men in Black','men in black',1997,'host','two');")
        .execute(&db).await.unwrap();
    let fields = || Fields {
        title: Some("Men in Black".into()),
        premiered: Some("1997-07-02".into()),
        ..Default::default()
    };
    store_answer(&db, "auto-copy", "tmdb", "607", "auto", fields())
        .await
        .unwrap();
    assign_manual(&db, "manual-copy", "tmdb", "607", fields())
        .await
        .unwrap();
    let detail = page(&api, &token, "/api/v1/items/auto-copy").await;
    let overview = page(&api, &token, "/api/v1/items?q=Men%20in%20Black").await;
    assert_eq!(detail["match_confidence"], "auto");
    assert_eq!(
        detail["match_confidence"],
        overview["items"][0]["match_confidence"]
    );
    assert_eq!(detail["matched_title"], "Men in Black");
    let confidence = |detail: &serde_json::Value, id: &str| {
        detail["copies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|copy| copy["id"] == id)
            .unwrap()["match_confidence"]
            .clone()
    };
    assert_eq!(confidence(&detail, "auto-copy"), "auto");
    assert_eq!(confidence(&detail, "manual-copy"), "manual");

    store_answer(&db, "auto-copy", "tmdb", "607", "weak", fields())
        .await
        .unwrap();
    let detail = page(&api, &token, "/api/v1/items/auto-copy").await;
    assert_eq!(confidence(&detail, "auto-copy"), "weak");
    assert_eq!(confidence(&detail, "manual-copy"), "manual");
    sqlx::query("DELETE FROM provider_metadata WHERE item_id='auto-copy'")
        .execute(&db)
        .await
        .unwrap();
    let detail = page(&api, &token, "/api/v1/items/auto-copy").await;
    assert!(confidence(&detail, "auto-copy").is_null());
    assert_eq!(confidence(&detail, "manual-copy"), "manual");
}

#[tokio::test]
async fn subtitle_bodies_keep_the_selected_tracks_source_and_timing() {
    use kahawai_media::subtitles::{Cue, Extracted};
    let (api, token, db, _, artwork) = harness().await;
    let subtitles =
        kahawai_hub::subtitles::Subtitles::new(artwork.parent().unwrap().join("subtitles"));
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES('host','movies','root','/movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    let mut tracks = Vec::new();
    // The larger 25 fps release wins the old collection-level source lookup.
    for (path, size, fps, start, text) in [
        ("Dune-1080.mkv", 2000, [25, 1], 1000, "25 fps release"),
        ("Dune-4k.mkv", 1000, [24000, 1001], 1043, "4K release"),
    ] {
        let info = serde_json::json!({"video":[{"codec":"h264","width":1920,"height":1080,"fps":fps,"interlaced":false}],"subtitles":[{"format":"ass","language":"en"}]});
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
            VALUES('host','movies',(SELECT id FROM collection_roots WHERE root_token='root'),?, ?,1,0,0,0,?) RETURNING id")
            .bind(path).bind(size).bind(info.to_string()).fetch_one(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file, "copy")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let track: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,'embedded',0,'ass','en') RETURNING id")
            .bind(file).fetch_one(&db).await.unwrap();
        let ass = format!("[Script Info]\n; {text}\n");
        subtitles
            .store_extracted(
                "host",
                "movies",
                "root",
                path,
                "e0",
                &Extracted {
                    cues: vec![Cue {
                        start_ms: start,
                        end_ms: start + 1000,
                        text: text.into(),
                    }],
                    ass: Some(ass.clone()),
                },
            )
            .unwrap();
        tracks.push((track, start, text, ass));
    }
    for (track, start, text, ass) in tracks.into_iter().rev() {
        for extension in ["vtt", "ass"] {
            let response = api
                .clone()
                .oneshot(
                    Request::get(format!("/api/v1/items/copy/subtitles/{track}.{extension}"))
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(body.contains(text), "track {track}: {body}");
            if extension == "vtt" {
                assert!(
                    body.contains(&format!("00:00:01.{:03}", start % 1000)),
                    "{body}"
                );
            } else {
                assert_eq!(body, ass);
            }
        }
    }
}

#[tokio::test]
async fn uncached_embedded_and_sidecar_text_read_the_tracks_exact_file() {
    use kahawai_hub::subtitles::{AssBody, Subtitles};
    let (_, _, db, registry, _) = harness().await;
    let media = tempfile::tempdir().unwrap();
    let sessions = kahawai_hub::sessions::Sessions::new(media.path().join("sessions"));
    let media_root = media.path().to_path_buf();
    sessions.set_local_source("host", move |_, _, path| Ok(media_root.join(path)));
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES('host','movies','root','/movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    const HEADER: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n\
        [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Bold\n\
        Style: Default,Arial,48,&H00FFFFFF,0\n\n\
        [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
    let mut selected = Vec::new();
    for (path, text) in [
        ("1080.mkv", "Other release"),
        ("4k.mkv", "Selected release"),
    ] {
        let file_path = media.path().join(path);
        kahawai_media::testutil::render_h264_ass_mkv(
            &file_path,
            HEADER,
            &[(500, 1500, format!("0,0,Default,,0,0,0,,{text}"))],
        );
        let mut info =
            kahawai_media::discover(&file_path, std::time::Duration::from_secs(5)).unwrap();
        let sidecar = format!("{path}.ass");
        std::fs::write(
            media.path().join(&sidecar),
            format!("{HEADER}Dialogue: 0,0:00:00.50,0:00:01.50,Default,,0,0,0,,{text}\n"),
        )
        .unwrap();
        info.external_subtitles
            .push(kahawai_core::media::SidecarSubtitle {
                path_rel: sidecar,
                format: "ass".into(),
                language: Some("en".into()),
                ..Default::default()
            });
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
            VALUES('host','movies',(SELECT id FROM collection_roots WHERE root_token='root'),?, ?,1,0,0,0,?) RETURNING id")
            .bind(path).bind(std::fs::metadata(file_path).unwrap().len() as i64).bind(serde_json::to_string(&info).unwrap()).fetch_one(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file, "copy")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        for origin in ["embedded", "sidecar"] {
            let id: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,?,0,'ass','en') RETURNING id")
                .bind(file).bind(origin).fetch_one(&db).await.unwrap();
            if path == "4k.mkv" {
                selected.push(
                    kahawai_hub::tracks::get_for_item(&db, "copy", id)
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
        }
    }
    for track in selected {
        for format in ["vtt", "ass", "burn"] {
            // Fresh cache for each delivery exercises the actual lease/sidecar
            // read, rather than a body materialized by the previous assertion.
            let cache = tempfile::tempdir().unwrap();
            let subtitles = Arc::new(Subtitles::new(cache.path().into()));
            let text = match format {
                "vtt" => subtitles
                    .vtt(&registry, &sessions, &track, 0)
                    .await
                    .unwrap(),
                "ass" => match subtitles
                    .ass_body(&registry, &sessions, &track)
                    .await
                    .unwrap()
                {
                    AssBody::Full(text) => text,
                    AssBody::Stream(mut rx) => {
                        let mut text = String::new();
                        while let Some(chunk) = rx.recv().await {
                            text.push_str(&chunk);
                        }
                        text
                    }
                },
                _ => subtitles
                    .ass_for_burn(&registry, &sessions, &track)
                    .await
                    .unwrap(),
            };
            assert!(
                text.contains("Selected release"),
                "{} {format}: {text}",
                track.origin
            );
            assert!(!text.contains("Other release"), "{text}");
        }
    }
}

#[tokio::test]
async fn font_extraction_reads_the_selected_rendition() {
    let (_, _, db, registry, _) = harness().await;
    registry.connected("host", "mediahost", "host", "fp", "test");
    let media = tempfile::tempdir().unwrap();
    let sessions = kahawai_hub::sessions::Sessions::new(media.path().join("sessions"));
    let root = media.path().to_path_buf();
    let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
    let reads = opened.clone();
    sessions.set_local_source("host", move |_, _, path| {
        reads.lock().unwrap().push(path.to_owned());
        Ok(root.join(path))
    });
    let subtitles = kahawai_hub::subtitles::Subtitles::new(media.path().join("subtitles"));
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES('host','movies','root','/movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    let mut sources = Vec::new();
    for (path, size, offset, font) in [
        ("default.mkv", 1000, 32, "default-font"),
        ("selected.mkv", 500, 64, "selected-font"),
    ] {
        let mut bytes = vec![b'x'; size];
        bytes[offset..offset + font.len()].copy_from_slice(font.as_bytes());
        std::fs::write(media.path().join(path), bytes).unwrap();
        let info = serde_json::json!({"video":[{"codec":"h264","width":1920,"height":1080,"fps":[24,1],"interlaced":false}],
            "attachments":[{"file_name":"font.ttf","mime_type":"font/ttf","offset":offset,"size":font.len()}]});
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
            VALUES('host','movies',(SELECT id FROM collection_roots WHERE root_token='root'),?,?,1,0,0,0,?) RETURNING id")
            .bind(path).bind(size as i64).bind(info.to_string()).fetch_one(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file, "copy")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let source: i64 = sqlx::query_scalar(
            "SELECT playable_source_id FROM playable_source_parts WHERE file_id=?",
        )
        .bind(file)
        .fetch_one(&db)
        .await
        .unwrap();
        sources.push((source, path, font));
    }
    for &(source, _, font) in sources.iter().rev() {
        let expected = vec![("font.ttf".to_owned(), font.as_bytes().to_vec())];
        for _ in 0..2 {
            assert_eq!(
                subtitles
                    .fonts(&registry, &sessions, "copy", Some(source))
                    .await
                    .unwrap(),
                expected,
                "font bytes and the cached result must belong to source {source}"
            );
        }
    }

    // Older mediahosts have no attachment declarations. Exercise the real
    // demux fallback with valid media and verify which physical file it opens.
    let template = media.path().join("template.mkv");
    kahawai_media::testutil::render_h264_aac_mkv(&template);
    let bytes = std::fs::read(template).unwrap();
    for &(_, path, _) in &sources {
        std::fs::write(media.path().join(path), &bytes).unwrap();
    }
    sqlx::query("UPDATE files SET size=?,streams_json=json_remove(streams_json,'$.attachments')")
        .bind(bytes.len() as i64)
        .execute(&db)
        .await
        .unwrap();
    let subtitles = kahawai_hub::subtitles::Subtitles::new(media.path().join("font-demux"));
    for &(source, path, _) in sources.iter().rev() {
        opened.lock().unwrap().clear();
        assert!(
            subtitles
                .fonts(&registry, &sessions, "copy", Some(source))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(*opened.lock().unwrap(), vec![path.to_owned()]);
    }
}

#[tokio::test]
async fn parent_correction_discards_inherited_episode_metadata_and_repairs_old_titles() {
    use kahawai_hub::{
        library,
        providers::{self, Fields},
    };
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','old','series'),('host','right','series');
        INSERT INTO libraries(id,name,media_type) VALUES('old-library','Old files','series'),('right-library','Right files','series');
        INSERT INTO library_collections VALUES('old-library','host','old'),('right-library','host','right');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id)
        VALUES('old-show','show','Old show','old show',2000,'host','old'),('right-show','show','Right show','right show',2010,'host','right');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
        VALUES('z-old-episode','episode','Episode 1','episode 1','old-show',1,1,'host','old'),
              ('a-right-episode','episode','Episode 1','episode 1','right-show',1,1,'host','right');")
        .execute(&db).await.unwrap();
    for (show, show_id, year, episode, episode_id, title, overview, projection) in [
        (
            "old-show",
            "11",
            "2000-01-01",
            "z-old-episode",
            "111",
            "Old episode title",
            "Old show overview",
            7,
        ),
        (
            "right-show",
            "22",
            "2010-01-01",
            "a-right-episode",
            "222",
            "Correct episode title",
            "Correct show overview",
            2,
        ),
    ] {
        providers::store_answer(
            &db,
            show,
            "tmdb",
            show_id,
            "auto",
            Fields {
                title: Some(
                    if show == "old-show" {
                        "Old show"
                    } else {
                        "Right show"
                    }
                    .into(),
                ),
                premiered: Some(year.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        providers::store_answer(
            &db,
            episode,
            "tmdb",
            episode_id,
            "auto",
            Fields {
                title: Some(title.into()),
                overview: Some(overview.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE provider_metadata SET proj_season=?,proj_episode=3 WHERE item_id=? AND provider='tmdb'")
            .bind(projection).bind(episode).execute(&db).await.unwrap();
    }
    let original = page(&api, &token, "/api/v1/items/a-right-episode").await;
    assert_eq!(original["title"], "Correct episode title");
    db.transaction("correct a show's work", |c| {
        Box::pin(async move { library::assign(c, "old-show", &["right-show".into()]).await })
    })
    .await
    .unwrap();

    // First exercise the live correction, then the migration on a committed
    // pre-fix shape. The ineligible copy sorts last: its detected fallback must
    // not overwrite the valid sibling's title during the repair pass either.
    for deployed_shape in [false, true] {
        if deployed_shape {
            db.write("pre-fix inherited metadata contamination", |c| Box::pin(async move {
                sqlx::raw_sql("UPDATE library_items SET title='Old episode title',norm_title='old episode title',sort_title='old episode title' WHERE id='a-right-episode';
                    UPDATE collection_items SET metadata_eligible=1 WHERE id='z-old-episode';
                    DELETE FROM library_pending;")
                    .execute(&mut *c).await?;
                sqlx::raw_sql(include_str!("../migrations/0082_public_ids_and_inherited_metadata.sql"))
                    .execute(c).await?;
                Ok(())
            })).await.unwrap();
            library::initialize(&db).await.unwrap();
        }
        assert_eq!(sqlx::query_as::<_, (String, bool)>("SELECT a.library_item_id,i.metadata_eligible FROM collection_items i JOIN collection_item_library_items a ON a.collection_item_id=i.id WHERE i.id='z-old-episode'")
            .fetch_one(&db).await.unwrap(), ("a-right-episode".into(), false));
        let detail = page(&api, &token, "/api/v1/items/a-right-episode").await;
        assert_eq!(detail["title"], "Correct episode title", "{detail}");
        assert_eq!(
            detail["metadata"]["overview"], "Correct show overview",
            "{detail}"
        );
        assert_eq!(detail["metadata"]["tmdb_id"], 22, "{detail}");
        assert_eq!(detail["metadata"]["proj_season"], 2, "{detail}");
        assert_eq!(detail["metadata"]["proj_episode"], 3, "{detail}");
    }

    // A viewer with only the corrected source cannot borrow the other copy's
    // provider answer or the previous show's IntroDB lookup identity.
    sqlx::raw_sql(
        "UPDATE users SET is_admin=0,all_libraries=0 WHERE username='pager';
        INSERT INTO user_libraries SELECT id,'old-library' FROM users WHERE username='pager';",
    )
    .execute(&db)
    .await
    .unwrap();
    let detail = page(&api, &token, "/api/v1/items/a-right-episode").await;
    assert!(detail["metadata"].is_null(), "{detail}");
    assert!(detail["proj_season"].is_null(), "{detail}");
    assert!(detail["proj_episode"].is_null(), "{detail}");

    // A deliberate child provider choice is independent of the source parent's
    // old match. Its description survives, while that parent's IDs stay hidden.
    providers::assign_manual(
        &db,
        "z-old-episode",
        "tmdb",
        "333",
        Fields {
            title: Some("Explicit episode choice".into()),
            overview: Some("Explicit child overview".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='z-old-episode'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
    let detail = page(&api, &token, "/api/v1/items/a-right-episode").await;
    assert_eq!(
        detail["metadata"]["overview"], "Explicit child overview",
        "{detail}"
    );
    assert!(detail["metadata"]["tmdb_id"].is_null(), "{detail}");
    assert!(detail["metadata"]["tvdb_id"].is_null(), "{detail}");
    assert!(detail["metadata"]["proj_season"].is_null(), "{detail}");
    assert!(detail["metadata"]["proj_episode"].is_null(), "{detail}");
}

#[tokio::test]
async fn review_queue_includes_library_ambiguity_and_child_conflicts_despite_confident_metadata() {
    let (api, token, db, _, _) = harness().await;
    let mut tx = db.begin().await.unwrap();
    for _ in 0..2 {
        kahawai_hub::library::create(
            &mut tx,
            kahawai_hub::library::NewItem {
                kind: "movie".into(),
                title: "Ambiguous".into(),
                year: Some(2000),
                artist: None,
                parent_id: None,
                season: None,
                episode: None,
                edition: None,
            },
        )
        .await
        .unwrap();
    }
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('review-host','mediahost','host','review-fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('review-host','movies','movies'),('review-host','series','series');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
          ('ambiguous','movie','Ambiguous','ambiguous',2000,'review-host','movies'),
          ('clear','movie','Clear','clear',2001,'review-host','movies'),
          ('manual','movie','Manual','manual',NULL,'review-host','movies'),
          ('parent','show','Parent','parent',2000,'review-host','series'),
          ('different-parent','show','Different','different',2001,'review-host','series');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES
          ('child','episode','Episode 1','episode 1','parent',1,1,'review-host','series'),
          ('different-child','episode','Episode 1','episode 1','different-parent',1,1,'review-host','series');")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for (id, title, year) in [
        ("ambiguous", "Ambiguous", 2000),
        ("clear", "Clear", 2001),
        ("parent", "Parent", 2000),
        ("different-parent", "Different", 2001),
    ] {
        kahawai_hub::providers::store_answer(
            &db,
            id,
            "tmdb",
            id,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                premiered: Some(format!("{year}-01-01")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let mut tx = db.begin().await.unwrap();
    kahawai_hub::library::assign(&mut tx, "manual", &["clear".into()])
        .await
        .unwrap();
    kahawai_hub::library::assign(&mut tx, "child", &["different-child".into()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let state: (String, String) = sqlx::query_as("SELECT c.match_mode,m.confidence FROM collection_items c JOIN resolved_metadata m ON m.item_id=c.id WHERE c.id='ambiguous'")
        .fetch_one(&db).await.unwrap();
    assert_eq!(state, ("unmatched".into(), "auto".into()));
    let entries = page(&api, &token, "/admin/v1/enrich/review").await;
    let mut copies: Vec<_> = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["collection_item_id"].as_str().unwrap())
        .collect();
    copies.sort_unstable();
    assert_eq!(copies, vec!["ambiguous", "child"], "{entries}");
}

#[tokio::test]
async fn newest_item_diagnostics_use_stable_ids_without_legacy_rows() {
    let (api, token, _, _, artwork) = harness().await;
    let item = "01M2CKKCV4232EDSV45SPXYFSC";
    let child = format!("child1:{item}:e:1:2");
    kahawai_hub::sessionlog::store(
        artwork.parent().unwrap(),
        &child,
        "session",
        "episode diagnostics",
    );
    for id in [item, child.as_str()] {
        let response = api
            .clone()
            .oneshot(
                Request::get(format!("/admin/v1/items/{id}/log"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"episode diagnostics");
    }
}

async fn create_library_child_via_api(
    api: &axum::Router,
    token: &str,
    db: &kahawai_hub::library::Database,
    copy: &str,
    definition: serde_json::Value,
) -> String {
    let revision: i64 =
        sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id=?")
            .bind(copy)
            .fetch_one(db)
            .await
            .unwrap();
    let response = api.clone().oneshot(Request::post(format!("/admin/v1/collection-items/{copy}/match"))
        .header("authorization", format!("Bearer {token}")).header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"action":"new","expected_revision":revision,"new_item":definition}).to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    body["library_item_ids"][0].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn creating_a_song_keeps_the_requested_album_position_for_later_copies() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','music'),('host','two','music');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES
          ('album-a','album','First album','first album',2000,'Artist','host','one'),
          ('album-b','album','Second album','second album',2001,'Artist','host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
          VALUES('source','track','Old title','old title','album-a',1,1,'host','one');")
        .execute(&db).await.unwrap();
    let song = create_library_child_via_api(
        &api,
        &token,
        &db,
        "source",
        serde_json::json!({
            "kind":"song", "title":"Chosen Song", "parent_id":"album-b", "season":2, "episode":7,
        }),
    )
    .await;
    let positions: Vec<(String,i64,i64)> = sqlx::query_as("SELECT album_id,disc_number,track_number FROM album_tracks WHERE song_id=? ORDER BY album_id")
        .bind(&song).fetch_all(&db).await.unwrap();
    assert!(
        positions.contains(&("album-b".into(), 2, 7)),
        "requested position missing: {positions:?}"
    );
    sqlx::raw_sql("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
        VALUES('later','track','Track 07','track 07','album-b',2,7,'host','two');").execute(&db).await.unwrap();
    let later: String = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='later'")
        .fetch_one(&db).await.unwrap();
    assert_eq!(later, song);
}

#[tokio::test]
async fn song_creation_keeps_optional_positions_and_validates_supplied_numbers() {
    let (_, _, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','music','music');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id)
          VALUES('album','album','Record','record',2000,'Artist','host','music');")
        .execute(&db).await.unwrap();
    for (disc, track, valid) in [
        (None, Some(7), true),
        (None, None, true),
        (Some(2), None, true),
        (Some(0), Some(7), false),
        (Some(-1), Some(7), false),
        (None, Some(0), false),
        (None, Some(-1), false),
    ] {
        let mut tx = db.begin().await.unwrap();
        let result = kahawai_hub::library::create(
            &mut tx,
            kahawai_hub::library::NewItem {
                kind: "song".into(),
                title: "Chosen Song".into(),
                year: None,
                artist: None,
                parent_id: Some("album".into()),
                season: disc,
                episode: track,
                edition: None,
            },
        )
        .await;
        if valid {
            let id = result.unwrap();
            let position: Option<(String, i64, i64)> = sqlx::query_as(
                "SELECT album_id,disc_number,track_number FROM album_tracks WHERE song_id=?",
            )
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
            assert_eq!(
                position,
                track.map(|track| ("album".into(), disc.unwrap_or(1), track))
            );
            tx.commit().await.unwrap();
        } else {
            assert!(result.is_err(), "disc={disc:?}, track={track:?}");
            tx.rollback().await.unwrap();
        }
    }
}

#[tokio::test]
async fn parent_provider_correction_cannot_donate_the_previous_shows_episode_answer() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','series'),('host','two','series');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
          ('parent-a','show','Series A','series a',2000,'host','one'),('parent-b','show','Series B','series b',2001,'host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES
          ('episode-a','episode','Episode 1','episode 1','parent-a',1,1,'host','one'),
          ('episode-b','episode','Episode 1','episode 1','parent-b',1,1,'host','two');")
        .execute(&db).await.unwrap();
    for (parent, title, year, pid) in [
        ("parent-a", "Series A", 2000, "1"),
        ("parent-b", "Series B", 2001, "2"),
    ] {
        kahawai_hub::providers::assign_manual(
            &db,
            parent,
            "tmdb",
            pid,
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                premiered: Some(format!("{year}-01-01")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    for (child, title, pid) in [
        ("episode-b", "Correct B Episode", "21"),
        ("episode-a", "Old A Episode", "11"),
    ] {
        kahawai_hub::providers::store_answer(
            &db,
            child,
            "tmdb",
            pid,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                overview: Some(format!("{title} overview")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let destination:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='episode-b'").fetch_one(&db).await.unwrap();
    apply_library_match(
        &api,
        &token,
        &db,
        "parent-a",
        serde_json::json!({"action":"pick","provider":"tmdb","candidate":{
            "id":2,"title":"Series B","release_date":"2001-01-01"
        }}),
    )
    .await;
    let corrected:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='episode-a'").fetch_one(&db).await.unwrap();
    assert_eq!(corrected, destination);
    let title: (String, String, String) =
        sqlx::query_as("SELECT title,norm_title,sort_title FROM library_items WHERE id=?")
            .bind(&destination)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        title,
        (
            "Correct B Episode".into(),
            "correct b episode".into(),
            "correct b episode".into()
        )
    );
    let stale: Option<String> =
        sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id='episode-a'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_ne!(stale.as_deref(), Some("Old A Episode"));
    let stored: String = sqlx::query_scalar(
        "SELECT title FROM provider_metadata WHERE item_id='episode-a' AND provider='tmdb'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        stored, "Old A Episode",
        "invalidating context must retain the answer itself"
    );
}

#[tokio::test]
async fn confirming_child_identity_does_not_freeze_eligible_descriptive_titles() {
    for (kind, parent_kind, media_type, provider) in [
        ("episode", "show", "series", "tmdb"),
        ("track", "album", "music", "musicbrainz"),
    ] {
        let (api, token, db, _, _) = harness().await;
        sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp')").execute(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one',?)",
        )
        .bind(media_type)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES('parent',?,'Parent','parent',2000,'Artist','host','one')").bind(parent_kind).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES('child',?,'Initial title','initial title','parent',1,1,'host','one')").bind(kind).execute(&db).await.unwrap();
        kahawai_hub::providers::assign_manual(
            &db,
            "parent",
            provider,
            "1",
            kahawai_hub::providers::Fields {
                title: Some("Parent".into()),
                premiered: Some("2000-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let before:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='child'").fetch_one(&db).await.unwrap();
        apply_library_match(
            &api,
            &token,
            &db,
            "child",
            serde_json::json!({"action":"confirm"}),
        )
        .await;
        kahawai_hub::providers::store_answer(
            &db,
            "child",
            provider,
            "11",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Improved title".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let after:(String,bool)=sqlx::query_as("SELECT a.library_item_id,ci.assignment_manual FROM collection_item_library_items a JOIN collection_items ci ON ci.id=a.collection_item_id WHERE ci.id='child'").fetch_one(&db).await.unwrap();
        assert_eq!(after, (before.clone(), true));
        let title: (String, String, String) =
            sqlx::query_as("SELECT title,norm_title,sort_title FROM library_items WHERE id=?")
                .bind(before)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            title,
            (
                "Improved title".into(),
                "improved title".into(),
                "improved title".into()
            ),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn parent_record_correction_invalidates_descriptions_when_library_identity_stays() {
    for (year, correction_provider) in [(Some(2000), "tmdb"), (None, "tmdb"), (Some(2000), "tvdb")]
    {
        let (api, token, db, _, _) = harness().await;
        sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
            INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','series');
            INSERT INTO collection_items(id,kind,title,norm_title,module_id,collection_id) VALUES('parent','show','Same title','same title','host','one');
            INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES
              ('child','episode','Episode 1','episode 1','parent',1,1,'host','one'),
              ('pinned-child','episode','Episode 2','episode 2','parent',1,2,'host','one');")
            .execute(&db).await.unwrap();
        kahawai_hub::providers::assign_manual(
            &db,
            "parent",
            "tmdb",
            "1",
            kahawai_hub::providers::Fields {
                title: Some("Same title".into()),
                premiered: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        kahawai_hub::providers::store_answer(
            &db,
            "child",
            "tmdb",
            "11",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Previous record's child".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        kahawai_hub::providers::assign_manual(
            &db,
            "pinned-child",
            "tmdb",
            "12",
            kahawai_hub::providers::Fields {
                title: Some("Independent child choice".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE provider_metadata SET proj_season=9,proj_episode=9 WHERE item_id='child'",
        )
        .execute(&db)
        .await
        .unwrap();
        // Existing installations cannot reconstruct historical context. Their
        // retained NULL rows must become bound at the next actual correction.
        sqlx::query("UPDATE provider_metadata SET identity_revision=NULL,parent_library_item_id=NULL WHERE item_id IN ('child','pinned-child')")
            .execute(&db).await.unwrap();
        let before:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='parent'").fetch_one(&db).await.unwrap();
        apply_library_match(
            &api,
            &token,
            &db,
            "parent",
            serde_json::json!({"action":"pick","provider":correction_provider,"candidate":{
                "id":2,"title":"Same title","release_date":year.map(|y|format!("{y}-01-01"))
            }}),
        )
        .await;
        let after:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='parent'").fetch_one(&db).await.unwrap();
        assert_eq!(before, after);
        let stale: Option<String> =
            sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id='child'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stale, None);
        let child_target:String=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='child'").fetch_one(&db).await.unwrap();
        let detail = page(&api, &token, &format!("/api/v1/items/{child_target}")).await;
        assert!(detail["metadata"]["proj_season"].is_null(), "{detail}");
        assert!(detail["metadata"]["proj_episode"].is_null(), "{detail}");
        assert_eq!(
            detail["metadata"][format!("{correction_provider}_id")],
            2,
            "{detail}"
        );
        if correction_provider == "tvdb" {
            assert!(
                detail["metadata"]["tmdb_id"].is_null(),
                "the departing parent record must not key external lookups: {detail}"
            );
        }
        let pinned: Option<String> =
            sqlx::query_scalar("SELECT title FROM resolved_metadata WHERE item_id='pinned-child'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pinned.as_deref(), Some("Independent child choice"));
        let stored: (String, i64) = sqlx::query_as(
            "SELECT title,identity_revision FROM provider_metadata WHERE item_id='child'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stored, ("Previous record's child".into(), 0));
        // An unchanged provider backfill is a refresh, never another correction.
        kahawai_hub::providers::store_answer(
            &db,
            "parent",
            correction_provider,
            "2",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Same title".into()),
                premiered: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT provider_identity_revision FROM collection_items WHERE id='parent'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn retained_combined_episode_reuses_ranked_answers_for_its_primary_title_only() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','series');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('parent','show','Parent','parent',2000,'host','one');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,episode_end,module_id,collection_id) VALUES('child','episode','Episode 1','episode 1','parent',1,1,2,'host','one');")
        .execute(&db).await.unwrap();
    for (provider, title) in [("tmdb", "TMDB title"), ("tvdb", "TVDB title")] {
        kahawai_hub::providers::store_answer(
            &db,
            "parent",
            provider,
            "1",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some("Parent".into()),
                premiered: Some("2000-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        kahawai_hub::providers::store_answer(
            &db,
            "child",
            provider,
            "11",
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    apply_library_match(
        &api,
        &token,
        &db,
        "child",
        serde_json::json!({"action":"confirm"}),
    )
    .await;
    let targets:Vec<String>=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='child' ORDER BY ordinal").fetch_all(&db).await.unwrap();
    assert_eq!(targets.len(), 2);
    kahawai_hub::providers::set_chain(&db, "series", &["tvdb".into(), "tmdb".into()])
        .await
        .unwrap();
    let titles:Vec<String>=sqlx::query_scalar("SELECT li.title FROM collection_item_library_items a JOIN library_items li ON li.id=a.library_item_id WHERE a.collection_item_id='child' ORDER BY ordinal").fetch_all(&db).await.unwrap();
    assert_eq!(titles, ["TVDB title", "Episode 2"]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT provider_identity_revision FROM collection_items WHERE id='parent'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM answer_priority")
            .fetch_one(&db)
            .await
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn album_children_keep_distinct_positions_and_require_a_copy_in_that_album() {
    let (api, token, db, _, artwork_dir) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','public','music'),('host','private','music');
        INSERT INTO libraries(id,name,media_type) VALUES('A','Public','music'),('B','Private','music');
        INSERT INTO library_collections VALUES('A','host','public'),('B','host','private');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES
          ('album-a','album','Shared album','shared album',2000,'Artist','host','public'),
          ('album-copy','album','Shared album','shared album',2000,'Artist','host','private'),
          ('other-album','album','Other album','other album',2001,'Artist','host','public');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES
          ('song-one','track','Recording','recording','album-a',1,1,'host','public'),
          ('song-copy','track','Recording','recording','album-copy',1,1,'host','private'),
          ('song-repeat','track','Recording','recording','album-a',2,3,'host','public'),
          ('hidden-track','track','Hidden position','hidden position','album-copy',1,2,'host','private'),
          ('elsewhere','track','Hidden position','hidden position','other-album',1,4,'host','public'),
          ('orphan-song','track','Other recording','other recording','other-album',1,6,'host','public');")
        .execute(&db).await.unwrap();
    let recording: String = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='song-one'")
        .fetch_one(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    kahawai_hub::library::assign(&mut tx, "song-repeat", std::slice::from_ref(&recording))
        .await
        .unwrap();
    kahawai_hub::library::assign(&mut tx, "elsewhere", &["hidden-track".into()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Durable queue positions may remain without any physical copy. Such an
    // association alone must not turn into a displayed album child.
    sqlx::query("INSERT INTO album_tracks(album_id,song_id,disc_number,track_number) VALUES('album-a','orphan-song',9,9)")
        .execute(&db).await.unwrap();
    let positions = |response: &serde_json::Value| -> Vec<(String, i64, i64)> {
        response["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["id"].as_str().unwrap().to_owned(),
                    row["season"].as_i64().unwrap(),
                    row["episode"].as_i64().unwrap(),
                )
            })
            .collect()
    };
    let all = page(&api, &token, "/api/v1/items/album-a/children").await;
    assert_eq!(
        positions(&all),
        vec![
            (recording.clone(), 1, 1),
            ("hidden-track".into(), 1, 2),
            (recording.clone(), 2, 3)
        ]
    );
    let repeated = all["children"].as_array().unwrap();
    assert_ne!(
        repeated[0]["album_track_id"], repeated[2]["album_track_id"],
        "one recording can retain distinct album positions"
    );

    let auth = kahawai_hub::auth::Auth::new(db.clone(), artwork_dir.parent().unwrap())
        .await
        .unwrap();
    let viewer = auth
        .create_user("album-viewer", "hunter22222hunter", false)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET all_libraries=0 WHERE id=?")
        .bind(&viewer)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_libraries(user_id,library_id) VALUES(?,'A')")
        .bind(&viewer)
        .execute(&db)
        .await
        .unwrap();
    let restricted = auth
        .login("album-viewer", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    // The hidden song is visible through another album; the requested album's
    // position must still require an authorized physical copy of its own.
    assert_eq!(
        page(&api, &restricted, "/api/v1/items/hidden-track").await["id"],
        "hidden-track"
    );
    let granted = page(&api, &restricted, "/api/v1/items/album-a/children").await;
    assert_eq!(
        positions(&granted),
        vec![(recording.clone(), 1, 1), (recording, 2, 3)]
    );
    assert!(
        granted["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["parent_id"] == "album-a")
    );
}

#[tokio::test]
async fn combined_anime_projects_each_native_episode_across_a_season_boundary() {
    let (api, token, db, _, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','anime','anime');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('combined-show','show','Combined anime','combined anime',2000,'host','anime');
      INSERT INTO collection_items(id,kind,title,norm_title,parent_id,episode,episode_end,module_id,collection_id) VALUES('combined-copy','episode','Episodes 25-26','episodes 25-26','combined-show',25,26,'host','anime');")
        .execute(&db).await.unwrap();
    for (copy, provider, record, title) in [
        ("combined-show", "tmdb", "100", "Combined anime"),
        ("combined-copy", "tmdb", "125", "First covered episode"),
    ] {
        kahawai_hub::providers::store_answer(
            &db,
            copy,
            provider,
            record,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(title.into()),
                overview: Some("Primary-only description".into()),
                premiered: Some("2000-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE provider_metadata SET proj_season=2,proj_episode=12,covered_episode_projections=? WHERE item_id='combined-copy' AND provider='tmdb'")
        .bind(r#"{"25":[2,12],"26":[3,1]}"#).execute(&db).await.unwrap();
    let children = page(&api, &token, "/api/v1/items/combined-show/children").await;
    let rows = children["children"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (child, native, season, episode) in [(&rows[0], 25, 2, 12), (&rows[1], 26, 3, 1)] {
        assert!(child["season"].is_null());
        assert_eq!(child["episode"], native);
        assert_eq!(child["proj_season"], season, "native episode {native}");
        assert_eq!(child["proj_episode"], episode);
        let detail = page(
            &api,
            &token,
            &format!("/api/v1/items/{}", child["id"].as_str().unwrap()),
        )
        .await;
        assert_eq!(detail["episode"], native);
        assert_eq!(detail["metadata"]["tmdb_id"], 100);
        assert_eq!(detail["metadata"]["proj_season"], season);
        assert_eq!(detail["metadata"]["proj_episode"], episode);
        if native == 26 {
            assert!(detail["metadata"]["overview"].is_null());
            assert!(detail["matched_title"].is_null());
            assert!(
                detail["metadata"]["provider"].is_null(),
                "no primary episode provider record is claimed"
            );
        }
    }
    assert_eq!(
        rows[1]["title"], "Episode 26",
        "the first episode's description is not reused"
    );
    let second_id = rows[1]["id"].as_str().unwrap().to_string();
    for (copy, record) in [("combined-show", "200"), ("combined-copy", "225")] {
        kahawai_hub::providers::store_answer(
            &db,
            copy,
            "tvdb",
            record,
            "auto",
            kahawai_hub::providers::Fields {
                title: Some(
                    if copy == "combined-show" {
                        "Combined anime"
                    } else {
                        "TVDB first"
                    }
                    .into(),
                ),
                premiered: Some("2000-01-01".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE provider_metadata SET proj_season=7,proj_episode=6,covered_episode_projections=? WHERE item_id='combined-copy' AND provider='tvdb'")
        .bind(r#"{"25":[7,6],"26":[8,1]}"#).execute(&db).await.unwrap();
    kahawai_hub::providers::set_chain(
        &db,
        "anime",
        &["tvdb".into(), "tmdb".into(), "anime".into()],
    )
    .await
    .unwrap();
    let reranked = page(&api, &token, "/api/v1/items/combined-show/children").await;
    assert_eq!(
        reranked["children"][1]["proj_season"], 8,
        "provider rank reuses its stored projection"
    );
    let detail = page(&api, &token, &format!("/api/v1/items/{second_id}")).await;
    assert_eq!(
        detail["proj_season"], 8,
        "display follows describing provider order"
    );
    assert_eq!(detail["metadata"]["tmdb_id"], 100);
    assert_eq!(
        detail["metadata"]["proj_season"], 3,
        "external lookup keeps TMDB's coherent pair"
    );
    assert!(detail["metadata"]["overview"].is_null());
    kahawai_hub::providers::assign_manual(
        &db,
        "combined-copy",
        "tmdb",
        "125",
        kahawai_hub::providers::Fields {
            title: Some("First covered episode".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let pinned = page(&api, &token, "/api/v1/items/combined-show/children").await;
    assert_eq!(
        pinned["children"][1]["proj_season"], 3,
        "same-record independent pin retains the complete map"
    );
    // A single-episode supplementary copy remains a valid projection donor
    // after the combined copy stops covering that native episode.
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,episode,module_id,collection_id) VALUES('supplement','episode','Episode 26','episode 26','combined-show',26,'host','anime')").execute(&db).await.unwrap();
    kahawai_hub::providers::store_answer(
        &db,
        "supplement",
        "tvdb",
        "226",
        "auto",
        kahawai_hub::providers::Fields::default(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE provider_metadata SET proj_season=8,proj_episode=1 WHERE item_id='supplement'",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET episode_end=25 WHERE id='combined-copy'")
        .execute(&db)
        .await
        .unwrap();
    let supplemented = page(&api, &token, &format!("/api/v1/items/{second_id}")).await;
    assert_eq!(
        supplemented["proj_season"], 8,
        "removed combined coverage cannot donate its old map"
    );
    assert_eq!(supplemented["metadata"]["tvdb_id"], 200);
    assert_eq!(supplemented["metadata"]["proj_season"], 8);
    kahawai_hub::providers::assign_manual(
        &db,
        "combined-copy",
        "tmdb",
        "999",
        kahawai_hub::providers::Fields::default(),
    )
    .await
    .unwrap();
    let old_map: Option<String> = sqlx::query_scalar("SELECT covered_episode_projections FROM provider_metadata WHERE item_id='combined-copy' AND provider='tmdb'").fetch_one(&db).await.unwrap();
    assert!(
        old_map.is_none(),
        "a different provider record cannot keep the previous projection map"
    );
}
