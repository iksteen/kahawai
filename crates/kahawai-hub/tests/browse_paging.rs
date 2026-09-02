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

async fn harness() -> (
    axum::Router,
    String,
    kahawai_sqlite::Database,
    Arc<kahawai_hub::registry::Registry>,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
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
    let artwork_dir = dir.path().join("artwork");
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
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

#[derive(Debug, PartialEq)]
struct CompositionState {
    item_count: i64,
    source_id: i64,
    provider_id: String,
    manual_id: String,
    watch: (i64, i64),
    generation: i64,
}

async fn composition_state(db: &kahawai_sqlite::Database) -> CompositionState {
    CompositionState {
        item_count: sqlx::query_scalar("SELECT count(*) FROM items WHERE id='composed-item'")
            .fetch_one(db)
            .await
            .unwrap(),
        source_id: sqlx::query_scalar("SELECT file_id FROM file_bindings WHERE item_id='composed-item'")
            .fetch_one(db)
            .await
            .unwrap(),
        provider_id: sqlx::query_scalar(
            "SELECT provider_id FROM provider_metadata WHERE item_id='composed-item'",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        manual_id: sqlx::query_scalar(
            "SELECT provider_id FROM manual_match WHERE item_id='composed-item'",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        watch: sqlx::query_as(
            "SELECT position_ms,play_count FROM watch_state WHERE item_id='composed-item'",
        )
        .fetch_one(db)
        .await
        .unwrap(),
        generation: sqlx::query_scalar(
            "SELECT sync_version FROM collections WHERE module_id='compose-host' AND collection_id='films'",
        )
        .fetch_one(db)
        .await
        .unwrap(),
    }
}

fn item_ids(v: &serde_json::Value) -> Vec<&str> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect()
}

/// Composition is a live visibility edge, not a materialized presentation.
/// Attaching or detaching a populated collection must neither wait for a later
/// file upsert nor rewrite any durable catalogue/user state.
#[tokio::test]
async fn attach_and_detach_change_only_visibility() {
    let (api, token, db, registry, _artwork_dir) = harness().await;
    sqlx::raw_sql(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
           VALUES('compose-host','mediahost','compose-host','fp');
         INSERT INTO collections(module_id,collection_id,media_type,sync_version)
           VALUES('compose-host','films','movies',91);
         INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
           VALUES('composed-item','movie','Composed','composed','compose-host','films');
         INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           VALUES('compose-host','films','composed.mkv',100,1,2,3,4,'{}');
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('compose-host','films','composed-item',NULL,'file:composed',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'compose-host','films',1,id FROM files WHERE path_rel='composed.mkv';
         INSERT INTO provider_metadata(item_id,provider,provider_id,title,confidence,updated_at)
           VALUES('composed-item','tmdb','42','Composed','auto',1);
         INSERT INTO manual_match(item_id,provider,provider_id,pinned_at)
           VALUES('composed-item','tmdb','42',1);",
    )
    .execute(&db)
    .await
    .unwrap();
    let user_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username='pager'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO watch_state(user_id,item_id,position_ms,play_count)
         VALUES(?,'composed-item',1234,3)",
    )
    .bind(user_id)
    .execute(&db)
    .await
    .unwrap();

    let first = registry
        .create_library("composition-one", "movies")
        .await
        .unwrap();
    let second = registry
        .create_library("composition-two", "movies")
        .await
        .unwrap();
    registry
        .attach_collection(&first, "compose-host", "films")
        .await
        .unwrap();
    let baseline = composition_state(&db).await;

    assert_eq!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={first}")).await),
        ["composed-item"]
    );
    assert!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={second}")).await).is_empty()
    );

    registry
        .attach_collection(&second, "compose-host", "films")
        .await
        .unwrap();
    assert_eq!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={second}")).await),
        ["composed-item"],
        "an already populated collection was not immediately visible"
    );
    assert_eq!(composition_state(&db).await, baseline);

    assert!(
        registry
            .detach_collection(&second, "compose-host", "films")
            .await
            .unwrap()
    );
    assert!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={second}")).await).is_empty()
    );
    assert_eq!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={first}")).await),
        ["composed-item"],
        "detaching another library hid the shared collection"
    );
    assert_eq!(composition_state(&db).await, baseline);

    assert!(
        registry
            .detach_collection(&first, "compose-host", "films")
            .await
            .unwrap()
    );
    assert!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={first}")).await).is_empty()
    );
    assert_eq!(composition_state(&db).await, baseline);

    registry
        .attach_collection(&second, "compose-host", "films")
        .await
        .unwrap();
    assert_eq!(
        item_ids(&page(&api, &token, &format!("/api/v1/items?library={second}")).await),
        ["composed-item"]
    );
    assert_eq!(composition_state(&db).await, baseline);
}

#[tokio::test]
async fn pages_partition_a_library_even_across_ties() {
    let (api, token, db, _registry, _artwork_dir) = harness().await;

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
            "INSERT INTO items(id,kind,title,norm_title,year,module_id,collection_id)
             VALUES(?,'movie','Same','same',2020,'m','c')",
        )
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
    let (api, token, db, _registry, _artwork_dir) = harness().await;
    let q = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql)
                .execute(&db)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
        }
    };
    q("INSERT INTO libraries (id, name, media_type) VALUES ('L','l','series')").await;
    q("INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
       VALUES ('m','mediahost','m','',unixepoch(),0)").await;
    q(
        "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
       VALUES ('m','c','series','[\"/m\"]',1)",
    )
    .await;
    q("INSERT INTO library_collections (library_id, module_id, collection_id) VALUES ('L','m','c')").await;

    // An album by an accented artist. norm_artist is what the write
    // sites store; here it is set the way the registry would.
    q(
        "INSERT INTO items(id,kind,title,norm_title,artist,norm_artist,module_id,collection_id)
       VALUES('alb','album','Ace of Spades','ace of spades','Motörhead','motorhead','m','c')",
    )
    .await;

    // A show whose episode has a projected title that is NOT in the
    // filename — the 19% case.
    q(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
       VALUES('sh','show','A Show','a show','m','c')",
    )
    .await;
    q(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
       VALUES('ep','episode','S03E14','s03e14','sh',3,14,'m','c')",
    )
    .await;

    kahawai_hub::providers::store_answer(
        &db,
        "sh",
        "tvdb",
        "81189",
        "auto",
        kahawai_hub::providers::Fields {
            title: Some("A Show".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    q("INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
       VALUES ('ep','tvdb','81189-e14','Ozymandias','auto',unixepoch())").await;

    // The artist, typed without the umlaut.
    let v = page(&api, &token, "/api/v1/items?q=motorhead").await;
    let ids: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["alb"], "folded artist search must find the album");

    // The episode, by a title that appears in no filename — scoped to
    // the library it reaches through its PARENT's membership.
    let v = page(&api, &token, "/api/v1/items?library=L&q=ozymandias").await;
    let hits = v["items"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "episode must be found in its show's library");
    assert_eq!(hits[0]["id"], "ep");
    assert_eq!(
        hits[0]["title"], "Ozymandias",
        "resolved title, not the filename"
    );
    assert_eq!(
        hits[0]["parent_title"], "A Show",
        "a hit named like 8 others needs its show"
    );

    // Tracks match by TITLE, never by artist: "motorhead" must not bury
    // the album under a row per song, but the song itself is findable.
    q(
        "INSERT INTO items(id,kind,title,norm_title,parent_id,artist,norm_artist,module_id,collection_id)
       VALUES('trk','track','Overkill','overkill','alb','Motörhead','motorhead','m','c')",
    )
    .await;
    let v = page(&api, &token, "/api/v1/items?q=motorhead").await;
    assert_eq!(
        v["items"].as_array().unwrap().len(),
        1,
        "still just the album"
    );

    let v = page(&api, &token, "/api/v1/items?q=overkill").await;
    let hits = v["items"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "the track is findable by its title");
    assert_eq!(hits[0]["id"], "trk");
    assert_eq!(
        hits[0]["parent_id"], "alb",
        "a track hit carries the album to open"
    );
    assert_eq!(hits[0]["parent_title"], "Ace of Spades");
}

#[tokio::test]
async fn artist_browse_groups_before_paging_and_albums_are_chronological() {
    let (api, token, db, _registry, artwork_dir) = harness().await;
    sqlx::query("INSERT INTO libraries(id,name,media_type) VALUES('M','Music','music')")
        .execute(&db)
        .await
        .unwrap();
    for (module, collection) in [("m1", "c1"), ("m2", "c2")] {
        sqlx::query(
            "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint,enrolled_at,disabled)
             VALUES(?,'mediahost',?,'',unixepoch(),0)",
        )
        .bind(module)
        .bind(module)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO collections(module_id,collection_id,media_type,roots_json,sync_version)
             VALUES(?,?,'music','[]',1)",
        )
        .bind(module)
        .bind(collection)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO library_collections(library_id,module_id,collection_id)
             VALUES('M',?,?)",
        )
        .bind(module)
        .bind(collection)
        .execute(&db)
        .await
        .unwrap();
    }
    for (id, title, year, artist, norm, artist_key, module, collection) in [
        (
            "a1",
            "Enriched Earlier",
            2005,
            "Various Artists",
            "various artists",
            "artist-dmFyaW91cyBhcnRpc3Rz",
            "m1",
            "c1",
        ),
        (
            "a2",
            "Tagged Later",
            1999,
            "Various Artists",
            "various artists",
            "artist-dmFyaW91cyBhcnRpc3Rz",
            "m2",
            "c2",
        ),
        (
            "a3",
            "Solo",
            2001,
            "Björk",
            "bjork",
            "artist-Ympvcms",
            "m1",
            "c1",
        ),
    ] {
        sqlx::query(
            "INSERT INTO items
               (id,kind,title,norm_title,sort_title,year,artist,norm_artist,artist_key,module_id,collection_id)
             VALUES(?,'album',?,?,?,?,?,?,?,?,?)",
        )
        .bind(id)
        .bind(title)
        .bind(title.to_ascii_lowercase())
        .bind(title.to_ascii_lowercase())
        .bind(year)
        .bind(artist)
        .bind(norm)
        .bind(artist_key)
        .bind(module)
        .bind(collection)
        .execute(&db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO items
           (id,kind,title,norm_title,sort_title,parent_id,episode,artist,norm_artist,module_id,collection_id)
         VALUES('t','track','Hidden Gem','hidden gem','hidden gem','a1',1,'Guest','guest','m1','c1')",
    )
    .execute(&db)
    .await
    .unwrap();

    // The filename/tag supplied no usable date for a1. MusicBrainz did, and
    // the list must order by the same resolved year its card displays. Leaving
    // this as a raw-year fixture let the broken query pass unnoticed.
    sqlx::query("UPDATE items SET year=NULL WHERE id='a1'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata
           (item_id,provider,provider_id,premiered,confidence,updated_at)
         VALUES('a1','musicbrainz','rg-a1','1990-01-01','auto',unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();

    let first = page(&api, &token, "/api/v1/artists?library=M&limit=1").await;
    assert_eq!(first["total"], 2, "the group count must precede paging");
    assert_eq!(first["artists"][0]["name"], "Björk");
    assert!(first["artists"][0]["art_version"].is_null());
    let second = page(&api, &token, "/api/v1/artists?library=M&limit=1&offset=1").await;
    assert_eq!(second["artists"][0]["name"], "Various Artists");
    assert_eq!(second["artists"][0]["album_count"], 2);

    const ARTIST_URL: &str = "https://assets.fanart.tv/a.jpg";
    sqlx::query(
        "INSERT INTO artist_artwork
           (artist_key,artist_name,musicbrainz_id,image_id,image_url,outcome,source_revision,updated_at)
         VALUES('artist-Ympvcms','Björk','mbid','image','https://assets.fanart.tv/a.jpg',
                'ready','revision',1234)",
    )
    .execute(&db)
    .await
    .unwrap();
    let with_art = page(&api, &token, "/api/v1/artists?library=M&limit=1").await;
    assert_eq!(with_art["artists"][0]["art_version"], 1234);

    let cache_key = format!(
        "fanart-{:016x}",
        xxhash_rust::xxh3::xxh3_64(ARTIST_URL.as_bytes())
    );
    let derivative = artwork_dir.join("size-card-480").join(cache_key);
    std::fs::create_dir_all(derivative.parent().unwrap()).unwrap();
    std::fs::write(&derivative, b"cached-artist-jpeg").unwrap();
    let response = api
        .clone()
        .oneshot(
            Request::get("/api/v1/artists/artist-Ympvcms/artwork?library=M&size=card&v=1234")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/jpeg");
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=86400"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"cached-artist-jpeg");

    let albums = page(
        &api,
        &token,
        "/api/v1/artists/artist-dmFyaW91cyBhcnRpc3Rz/albums?library=M",
    )
    .await;
    let titles: Vec<&str> = albums["albums"]
        .as_array()
        .unwrap()
        .iter()
        .map(|album| album["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Enriched Earlier", "Tagged Later"]);

    let found = page(
        &api,
        &token,
        "/api/v1/artists/artist-dmFyaW91cyBhcnRpc3Rz/albums?library=M&q=hidden",
    )
    .await;
    assert_eq!(found["total"], 1);
    assert_eq!(found["albums"][0]["id"], "a1");

    // Search folding intentionally considers number spellings equivalent,
    // but artist identity must not. Punctuation-only credits also need a
    // non-empty route key.
    sqlx::query(
        "INSERT INTO items
           (id,kind,title,norm_title,sort_title,artist,norm_artist,artist_key,module_id,collection_id)
         VALUES('word','album','Word','word','word','One','1','artist-b25l','m1','c1'),
               ('digit','album','Digit','digit','digit','1','1','artist-MQ','m1','c1'),
               ('punct','album','Punctuation','punctuation','punctuation','!!!','','artist-ISEh','m1','c1')",
    )
    .execute(&db)
    .await
    .unwrap();
    let numeric = page(&api, &token, "/api/v1/artists?library=M&q=one").await;
    assert_eq!(numeric["total"], 2, "fuzzy search merged artist identities");
    let keys: std::collections::BTreeSet<&str> = numeric["artists"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|artist| artist["key"].as_str())
        .collect();
    assert_eq!(keys, ["artist-MQ", "artist-b25l"].into_iter().collect());
    let punctuation = page(&api, &token, "/api/v1/artists/artist-ISEh/albums?library=M").await;
    assert_eq!(punctuation["albums"][0]["id"], "punct");
}

/// Unification: capability never filters the list — it changes each
/// track's computed delivery. A no-overlay client still sees the PGS
/// track; with no burn-capable host it reads `delivery: none`.
#[tokio::test]
async fn capability_changes_delivery_not_existence() {
    // No router needed: this one asks `Subtitles::list` directly.
    let (_api, _token, db, _registry, _artwork_dir) = harness().await;
    let q = |sql: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query(sql)
                .execute(&db)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
        }
    };
    q("INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
       VALUES ('m2','mediahost','m2','',unixepoch(),0)").await;
    q(
        "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
       VALUES ('m2','c2','movies','[\"/m\"]',1)",
    )
    .await;
    q(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
       VALUES('subs-item','movie','Subbed','subbed','m2','c2')",
    )
    .await;
    q(
        "INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path)
       VALUES('m2','c2','root','/m')",
    )
    .await;
    let source_id: i64 = sqlx::query_scalar(
        r#"INSERT INTO files
              (module_id,collection_id,root_id,path_rel,size,mtime_unix,
               head_xxh3,tail_xxh3,oshash,subs_extracted,streams_json)
       VALUES ('m2','c2',(SELECT id FROM collection_roots WHERE module_id='m2'),
               'subbed.mkv',700,1,0,0,0,0,
               '{"container":"matroska",
                 "subtitles":[{"format":"srt","language":"en"},
                              {"format":"ass","language":"en"},
                              {"format":"pgs","language":"en"}]}') RETURNING id"#,
    )
    .fetch_one(&db)
    .await
    .unwrap();
    kahawai_hub::registry::bind_file_to_item(
        &mut db.acquire().await.unwrap(),
        source_id,
        "subs-item",
    )
    .await
    .unwrap();
    // What a scan would materialize (tests seed by SQL, so no
    // sync_source_tracks ran).
    q(
        "INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language)
       VALUES((SELECT file_id FROM file_bindings WHERE item_id='subs-item'),'embedded',0,'srt','en'),
             ((SELECT file_id FROM file_bindings WHERE item_id='subs-item'),'embedded',1,'ass','en'),
             ((SELECT file_id FROM file_bindings WHERE item_id='subs-item'),'embedded',2,'pgs','en')",
    )
    .await;

    // Straight at `Subtitles::list`, not over HTTP: the listing endpoint
    // is gone, and QUERY — which replaced it — cannot pose this
    // question. It negotiates a source first, so "the file is known but
    // its host is not connected", which is exactly the state that makes
    // burn unreachable below, never reaches the listing there.
    let reg = std::sync::Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
    ));
    let subs = kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep());
    let list = |ass_render: bool, overlay: bool| {
        let (reg, subs, db) = (reg.clone(), &subs, db.clone());
        async move {
            let ass = kahawai_hub::tracks::ass_policy_for_user(&db, "u", false).await;
            subs.list(
                &reg,
                "subs-item",
                &kahawai_core::media::CapabilityProfile {
                    ass_render,
                    graphics_overlay: overlay,
                    ..Default::default()
                },
                &ass,
                "u",
                false,
                ("m2", "c2", "root", "subbed.mkv"),
            )
            .await
            .unwrap()
        }
    };
    use kahawai_hub::tracks::Delivery;
    let delivery_of = |v: &[kahawai_hub::subtitles::TrackListing], format: &str| -> Delivery {
        v.iter()
            .find(|s| s.track.format == format)
            .unwrap_or_else(|| panic!("{format} must be listed"))
            .delivery
    };

    let all = list(true, true).await;
    assert_eq!(all.len(), 3);
    assert_eq!(delivery_of(&all, "pgs"), Delivery::Overlay);
    assert_eq!(delivery_of(&all, "ass"), Delivery::Ass);
    assert_eq!(delivery_of(&all, "srt"), Delivery::Text);

    let gated = list(false, false).await;
    assert_eq!(gated.len(), 3, "capability must not filter");
    // m2 is enrolled but not CONNECTED, so no burn either: none.
    assert_eq!(delivery_of(&gated, "pgs"), Delivery::None);
    assert_eq!(delivery_of(&gated, "ass"), Delivery::Text);
}

/// Unification materialization: a rescan preserves row ids while a
/// stream keeps its position (preferences pin ids, so churn would
/// orphan them); a vanished stream deletes its reproducible derivatives.
#[tokio::test]
async fn scan_sync_preserves_track_ids() {
    let (_api, _token, db, _registry, _artwork_dir) = harness().await;
    sqlx::query(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
                 VALUES('sync','mediahost','sync','fp')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections(module_id,collection_id,media_type)
                 VALUES('sync','c','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
                 VALUES('it','movie','M','m','sync','c')",
    )
    .execute(&db)
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('sync','c','m.mkv',1,1,0,0,0,'{}') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    kahawai_hub::registry::bind_file_to_item(&mut db.acquire().await.unwrap(), source_id, "it")
        .await
        .unwrap();
    let info: kahawai_core::media::MediaInfo = serde_json::from_str(
        r#"{"subtitles":[{"format":"srt","language":"en"},{"format":"pgs","language":"nl"}],
            "external_subtitles":[{"path_rel":"m.idx","format":"vobsub","language":"en","track":0}]}"#,
    )
    .unwrap();
    let sync = |info: kahawai_core::media::MediaInfo| {
        let db = db.clone();
        async move {
            let mut tx = db.begin().await.unwrap();
            kahawai_hub::tracks::sync_source_tracks(&mut tx, source_id, &info)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    sync(info.clone()).await;
    let ids = |origin: &'static str| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM subtitle_tracks WHERE origin = ? ORDER BY stream_index",
            )
            .bind(origin)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    let first = ids("embedded").await;
    assert_eq!(first.len(), 2);
    assert_eq!(ids("sidecar").await.len(), 1);

    // An OCR row derived from the PGS track.
    let ocr: i64 = sqlx::query_scalar(
        "INSERT INTO subtitle_tracks (source_id, origin, format, machine, derived_from)
         VALUES (?,'ocr','srt',1,?) RETURNING id",
    )
    .bind(source_id)
    .bind(first[1])
    .fetch_one(&db)
    .await
    .unwrap();

    // Rescan, same streams: ids survive.
    sync(info.clone()).await;
    assert_eq!(ids("embedded").await, first, "rescan must preserve ids");

    // The PGS stream vanishes: its row and reproducible OCR derivative go
    // together. A later scan can regenerate it from the physical stream.
    let mut less = info.clone();
    less.subtitles.truncate(1);
    sync(less).await;
    assert_eq!(ids("embedded").await, vec![first[0]]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subtitle_tracks WHERE id = ?")
            .bind(ocr)
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "source derivative survived its parent stream"
    );
}
