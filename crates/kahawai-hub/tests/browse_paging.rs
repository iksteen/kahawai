//! Paging a library must PARTITION it: every item on exactly one page,
//! whatever the page size and wherever the boundaries land.
//!
//! The trap is ties. Rows equal under the sort order used to come out in
//! whatever order the plan produced, so a tie straddling a page boundary
//! could show a row twice or not at all across two requests. The
//! membership-driven page (0040) ends its ORDER BY in `item_id`, inside
//! the covering index, making the order total by construction — this is
//! the test that says so, on a library built to tie.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

async fn harness() -> (axum::Router, String, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db.clone(), Default::default()));
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), dir.path()).await.unwrap());
    let sessions =
        Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep()));
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
    let api = kahawai_hub::api::router(
        registry,
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep())),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        kahawai_hub::api::NetOptions::default(),
    );
    let token = auth
        .complete_setup(&auth.setup_token().unwrap(), "pager", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    std::mem::forget(dir);
    (api, token, db)
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
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn pages_partition_a_library_even_across_ties() {
    let (api, token, db) = harness().await;

    sqlx::query("INSERT INTO libraries (id, name, media_type) VALUES ('L','l','movies')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
         VALUES ('m','mediahost','m','',unixepoch(),0)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
         VALUES ('m','c','movies','[\"/m\"]',1)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO library_collections (library_id, module_id, collection_id) VALUES ('L','m','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    // Eleven items, ALL tying on (sort_title, year) — the sort has
    // nothing to go on except the tiebreaker.
    for n in 0..11 {
        let id = format!("i{n:02}");
        sqlx::query(
            "INSERT INTO items (id, kind, title, norm_title, year) VALUES (?, 'movie', 'Same', 'same', 2020)",
        )
        .bind(&id)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
             VALUES (?, 'm', 'c', ? || '.mkv')",
        )
        .bind(&id)
        .bind(&id)
        .execute(&db)
        .await
        .unwrap();
    }

    for sort in ["title", "-title", "year", "-year", "added", "-added"] {
        let mut seen = Vec::new();
        let mut offset = 0;
        loop {
            let v = page(
                &api,
                &token,
                &format!("/api/v1/items?library=L&sort={sort}&limit=3&offset={offset}"),
            )
            .await;
            assert_eq!(v["total"], 11, "sort={sort}");
            let ids: Vec<String> = v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect();
            if ids.is_empty() {
                break;
            }
            offset += ids.len();
            seen.extend(ids);
            if offset >= 11 {
                break;
            }
        }
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            11,
            "sort={sort}: pages must partition the library, got {seen:?}"
        );
    }

    // And a search that underfills its page reports the exact total
    // without a second scan being possible to get wrong.
    let v = page(&api, &token, "/api/v1/items?library=L&q=same&limit=100").await;
    assert_eq!(v["total"], 11);
    assert_eq!(v["items"].as_array().unwrap().len(), 11);
    let v = page(&api, &token, "/api/v1/items?q=same&limit=3").await;
    assert_eq!(v["total"], 11, "unscoped search still counts everything");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
}

/// HUB-12: albums are findable by artist and episodes by their resolved
/// titles — through the real router, folded like a person types.
#[tokio::test]
async fn search_finds_artists_and_episode_titles() {
    let (api, token, db) = harness().await;
    let q = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql).execute(&db).await.unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
        }
    };
    q("INSERT INTO libraries (id, name, media_type) VALUES ('L','l','series')").await;
    q("INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
       VALUES ('m','mediahost','m','',unixepoch(),0)").await;
    q("INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
       VALUES ('m','c','series','[\"/m\"]',1)").await;
    q("INSERT INTO library_collections (library_id, module_id, collection_id) VALUES ('L','m','c')").await;

    // An album by an accented artist. norm_artist is what the write
    // sites store; here it is set the way the registry would.
    q("INSERT INTO items (id, kind, title, norm_title, artist, norm_artist)
       VALUES ('alb','album','Ace of Spades','ace of spades','Motörhead','motorhead')").await;

    // A show whose episode has a projected title that is NOT in the
    // filename — the 19% case.
    q("INSERT INTO items (id, kind, title, norm_title) VALUES ('sh','show','A Show','a show')").await;
    q("INSERT INTO items (id, kind, title, norm_title, parent_id, season, episode)
       VALUES ('ep','episode','S03E14','s03e14','sh',3,14)").await;
    q("INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
       VALUES ('ep','m','c','s03e14.mkv')").await;
    kahawai_hub::providers::store_answer(
        &db,
        "sh",
        "tvdb",
        "81189",
        "auto",
        kahawai_hub::providers::Fields { title: Some("A Show".into()), ..Default::default() },
    )
    .await
    .unwrap();
    q("INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
       VALUES ('ep','tvdb','81189-e14','Ozymandias','auto',unixepoch())").await;

    // The artist, typed without the umlaut.
    let v = page(&api, &token, "/api/v1/items?q=motorhead").await;
    let ids: Vec<&str> =
        v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["alb"], "folded artist search must find the album");

    // The episode, by a title that appears in no filename — scoped to
    // the library it reaches through its PARENT's membership.
    let v = page(&api, &token, "/api/v1/items?library=L&q=ozymandias").await;
    let hits = v["items"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "episode must be found in its show's library");
    assert_eq!(hits[0]["id"], "ep");
    assert_eq!(hits[0]["title"], "Ozymandias", "resolved title, not the filename");
    assert_eq!(hits[0]["parent_title"], "A Show", "a hit named like 8 others needs its show");

    // Tracks stay out: "motorhead" must not bury the album under tracks.
    q("INSERT INTO items (id, kind, title, norm_title, parent_id, artist, norm_artist)
       VALUES ('trk','track','Overkill','overkill','alb','Motörhead','motorhead')").await;
    let v = page(&api, &token, "/api/v1/items?q=motorhead").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "still just the album");
}
