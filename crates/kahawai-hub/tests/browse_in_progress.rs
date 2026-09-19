//! `GET /api/v1/items?in_progress=true` — the continue-watching row.
//!
//! Three things have to hold, and each has a way of quietly not holding:
//! only started-and-unfinished items appear, the order is when you last
//! watched them (not the catalogue order every other browse uses), and a
//! restricted account sees only its own libraries' items in it.
//!
//! The rows are seeded with explicit `updated_at` values rather than by
//! marking them through the API in sequence: `unixepoch()` has
//! one-second resolution, so three marks in a row would tie and the
//! ordering assertion would be testing the tiebreaker instead of the
//! sort.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

async fn harness() -> (
    axum::Router,
    Arc<kahawai_hub::auth::Auth>,
    kahawai_hub::library::Database,
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
        tempfile::tempdir().unwrap().keep(),
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
    let api = kahawai_hub::api::legacy_router_fixture(
        registry,
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    );
    std::mem::forget(dir);
    (api, auth, db)
}

async fn browse(api: &axum::Router, token: &str, uri: &str) -> serde_json::Value {
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

fn titles(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect()
}

/// An item, optionally in a library, optionally with watch state as
/// `(position_ms, duration_ms, played, watched_at)`. `watched_at` is an
/// offset in seconds from now — bigger is older.
async fn seed(
    db: &kahawai_hub::library::Database,
    id: &str,
    kind: &str,
    title: &str,
    library: Option<&str>,
    watch: Option<(i64, i64, i64, i64)>,
) {
    sqlx::query(
        "INSERT OR IGNORE INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('fixture','mediahost','fixture','fp')",
    )
    .execute(db)
    .await
    .unwrap();
    let collection = library.unwrap_or("unattached");
    sqlx::query(
        "INSERT OR IGNORE INTO collections(module_id,collection_id,media_type)
                 VALUES('fixture',?,'movies')",
    )
    .bind(collection)
    .execute(db)
    .await
    .unwrap();
    if let Some(lib) = library {
        sqlx::query(
            "INSERT OR IGNORE INTO library_collections(library_id,module_id,collection_id)
                     VALUES(?,'fixture',?)",
        )
        .bind(lib)
        .bind(collection)
        .execute(db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO collection_items(id,kind,title,norm_title,sort_title,module_id,collection_id)
                 VALUES(?,?,?,?,?,'fixture',?)",
    )
    .bind(id)
    .bind(kind)
    .bind(title)
    .bind(title.to_lowercase())
    .bind(title.to_lowercase())
    .bind(collection)
    .execute(db)
    .await
    .unwrap();
    if let Some((position_ms, duration_ms, played, age_secs)) = watch {
        sqlx::query(
            "INSERT INTO user_item_state
                    (user_id, item_id, position_ms, duration_ms, played, updated_at)
             SELECT id, ?, ?, ?, ?, unixepoch() - ? FROM users WHERE username = 'owner'",
        )
        .bind(id)
        .bind(position_ms)
        .bind(duration_ms)
        .bind(played)
        .bind(age_secs)
        .execute(db)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn continue_watching_is_meaningfully_started_unfinished_most_recent_first() {
    let (api, auth, db) = harness().await;
    auth.complete_setup("owner", "hunter22222hunter")
        .await
        .unwrap();
    let token = auth
        .login("owner", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;

    // Meaningfully started at different times. Oldest first in the seed, so a
    // query that forgot to order would return them the wrong way round.
    seed(
        &db,
        "i1",
        "movie",
        "Oldest",
        None,
        Some((60_000, 1_200_000, 0, 300)),
    )
    .await;
    seed(
        &db,
        "i2",
        "episode",
        "Middle",
        None,
        Some((70_000, 1_200_000, 0, 200)),
    )
    .await;
    seed(
        &db,
        "i3",
        "movie",
        "Newest",
        None,
        Some((80_000, 1_200_000, 0, 100)),
    )
    .await;
    // Excluded, each for its own reason.
    seed(
        &db,
        "i4",
        "movie",
        "Finished",
        None,
        Some((9000, 1_200_000, 1, 50)),
    )
    .await;
    seed(&db, "i5", "movie", "Never started", None, None).await;
    seed(
        &db,
        "i6",
        "movie",
        "Position zero",
        None,
        Some((0, 1_200_000, 0, 10)),
    )
    .await;
    seed(
        &db,
        "i7",
        "track",
        "A song",
        None,
        Some((120_000, 180_000, 0, 5)),
    )
    .await;
    seed(
        &db,
        "i8",
        "movie",
        "Under one minute",
        None,
        Some((59_999, 1_200_000, 0, 4)),
    )
    .await;
    seed(
        &db,
        "i9",
        "movie",
        "Under one percent",
        None,
        Some((60_000, 10_800_000, 0, 3)),
    )
    .await;
    seed(
        &db,
        "i10",
        "movie",
        "At one percent",
        None,
        Some((108_000, 10_800_000, 0, 150)),
    )
    .await;

    let page = browse(&api, &token, "/api/v1/items?in_progress=true").await;
    assert_eq!(
        titles(&page),
        vec!["Newest", "At one percent", "Middle", "Oldest"],
        "both progress thresholds are inclusive, accidental starts stay out, \
         and episodes belong here"
    );
    assert_eq!(
        page["total"], 4,
        "the total counts the same rows it returns"
    );

    // The plain browse is untouched by any of this: it still lists
    // top-level items in title order, finished or not.
    let plain = browse(&api, &token, "/api/v1/items").await;
    assert!(
        titles(&plain).contains(&"Finished".to_string()),
        "a finished film is still in the library"
    );
    assert!(
        !titles(&plain).contains(&"A song".to_string()),
        "tracks stay out of the unscoped browse, as before"
    );
}

#[tokio::test]
async fn continue_watching_respects_library_grants() {
    let (api, auth, db) = harness().await;
    auth.complete_setup("owner", "hunter22222hunter")
        .await
        .unwrap();
    let admin_token = auth
        .login("owner", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;

    for (id, name) in [("LA", "granted"), ("LB", "withheld")] {
        sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES (?,?,'movies')")
            .bind(id)
            .bind(name)
            .execute(&db)
            .await
            .unwrap();
    }
    seed(
        &db,
        "a1",
        "movie",
        "In granted",
        Some("LA"),
        Some((60_000, 1_200_000, 0, 20)),
    )
    .await;
    seed(
        &db,
        "b1",
        "movie",
        "In withheld",
        Some("LB"),
        Some((60_000, 1_200_000, 0, 10)),
    )
    .await;

    // The owner is an admin, so grants do not bind it: both appear.
    let page = browse(&api, &admin_token, "/api/v1/items?in_progress=true").await;
    assert_eq!(titles(&page).len(), 2, "an admin is not bound by grants");

    // A restricted account holding only LA, with a position in BOTH —
    // so the only thing that can keep the withheld one out is the grant.
    let uid = auth
        .create_user("viewer", "hunter22222hunter", false)
        .await
        .unwrap();
    // A new account holds `all_libraries = 1` — every library until an
    // admin says otherwise (0049). Clearing it is what makes the account
    // restricted at all, so without this the test would pass by seeing
    // everything legitimately and prove nothing.
    sqlx::query("UPDATE users SET all_libraries = 0 WHERE id = ?")
        .bind(&uid)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_libraries (user_id, library_id) VALUES (?,'LA')")
        .bind(&uid)
        .execute(&db)
        .await
        .unwrap();
    for (item, age) in [("a1", 20), ("b1", 10)] {
        sqlx::query(
            "INSERT INTO user_item_state
                    (user_id, item_id, position_ms, duration_ms, played, updated_at)
             VALUES (?, ?, 60000, 1200000, 0, unixepoch() - ?)",
        )
        .bind(&uid)
        .bind(item)
        .bind(age)
        .execute(&db)
        .await
        .unwrap();
    }

    let viewer = auth
        .login("viewer", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    let page = browse(&api, &viewer, "/api/v1/items?in_progress=true").await;
    assert_eq!(
        titles(&page),
        vec!["In granted"],
        "a withheld library's item must not leak into continue watching, \
         even though this account has a position in it"
    );
    assert_eq!(page["total"], 1, "and the total must agree with the rows");
    // The row carries a library, because an item page's URL lives under
    // one and a cross-library browse has no other way to know it.
    assert_eq!(
        page["items"][0]["library_id"], "LA",
        "a browse row names a library it is in"
    );
}

#[tokio::test]
async fn continue_watching_paginates_canonical_candidates_with_all_filters() {
    let (api, auth, db) = harness().await;
    auth.complete_setup("owner", "hunter22222hunter")
        .await
        .unwrap();
    let token = auth
        .login("owner", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    for library in ["LA", "LB"] {
        sqlx::query("INSERT INTO libraries(id,name,media_type) VALUES(?,?,'movies')")
            .bind(library)
            .bind(library)
            .execute(&db)
            .await
            .unwrap();
    }
    seed(
        &db,
        "alias",
        "movie",
        "Detected name",
        Some("LA"),
        Some((60_000, 120_000, 0, 0)),
    )
    .await;
    seed(
        &db,
        "b",
        "movie",
        "Bravo",
        Some("LA"),
        Some((60_000, 120_000, 0, 0)),
    )
    .await;
    seed(
        &db,
        "z",
        "movie",
        "Zulu",
        Some("LB"),
        Some((60_000, 120_000, 0, 0)),
    )
    .await;
    seed(
        &db,
        "finished",
        "movie",
        "Finished",
        Some("LA"),
        Some((60_000, 120_000, 1, 0)),
    )
    .await;
    seed(
        &db,
        "other",
        "movie",
        "Another user's film",
        Some("LA"),
        None,
    )
    .await;
    seed(&db, "duplicate", "movie", "Another copy", Some("LA"), None).await;
    let mut tx = db.begin().await.unwrap();
    let target = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "movie".into(),
            title: "Corrected title".into(),
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
    kahawai_hub::library::assign(&mut tx, "alias", std::slice::from_ref(&target))
        .await
        .unwrap();
    kahawai_hub::library::assign(&mut tx, "duplicate", std::slice::from_ref(&target))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, "alias")
            .await
            .unwrap(),
        target
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_item_state WHERE item_id='alias'")
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "promotion moves history onto the canonical work"
    );
    sqlx::query(
        "UPDATE user_item_state SET updated_at=CASE item_id WHEN 'z' THEN 600 ELSE 500 END",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("UPDATE user_item_state SET duration_ms=NULL WHERE item_id=?")
        .bind(&target)
        .execute(&db)
        .await
        .unwrap();
    let viewer = auth
        .create_user("viewer", "hunter22222hunter", false)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET all_libraries=0 WHERE id=?")
        .bind(&viewer)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_libraries(user_id,library_id) VALUES(?,'LA')")
        .bind(&viewer)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,updated_at)
        VALUES(?,'other',60000,120000,700)",
    )
    .bind(&viewer)
    .execute(&db)
    .await
    .unwrap();

    let ids = |response: &serde_json::Value| -> Vec<String> {
        response["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let mut tied = [target.clone(), "b".to_string()];
    tied.sort_by(|a, b| b.cmp(a));
    let expected = vec!["z".to_string(), tied[0].clone(), tied[1].clone()];
    let complete = browse(&api, &token, "/api/v1/items?in_progress=true&sort=title").await;
    assert_eq!(
        ids(&complete),
        expected,
        "watch recency and ID ties override ordinary browse sorting"
    );
    assert_eq!(
        complete["total"], 3,
        "copies do not duplicate a canonical watch candidate and another user's state stays private"
    );
    for offset in 0..=3 {
        let response = browse(
            &api,
            &token,
            &format!("/api/v1/items?in_progress=true&limit=1&offset={offset}"),
        )
        .await;
        assert_eq!(response["total"], 3);
        assert_eq!(
            ids(&response),
            expected
                .iter()
                .skip(offset)
                .take(1)
                .cloned()
                .collect::<Vec<_>>()
        );
    }
    let scoped = browse(
        &api,
        &token,
        "/api/v1/items?in_progress=true&library=LA&limit=1&offset=1",
    )
    .await;
    assert_eq!(scoped["total"], 2);
    assert_eq!(ids(&scoped), vec![tied[1].clone()]);
    for needle in ["Corrected", "Detected"] {
        let searched = browse(
            &api,
            &token,
            &format!("/api/v1/items?in_progress=true&library=LA&q={needle}&limit=1"),
        )
        .await;
        assert_eq!(searched["total"], 1);
        assert_eq!(
            ids(&searched),
            vec![target.clone()],
            "both public title and authorized detected title remain searchable"
        );
    }
    let viewer_token = auth
        .login("viewer", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    let own = browse(
        &api,
        &viewer_token,
        "/api/v1/items?in_progress=true&limit=1",
    )
    .await;
    assert_eq!(ids(&own), vec!["other".to_string()]);
    assert_eq!(own["total"], 1);
}
