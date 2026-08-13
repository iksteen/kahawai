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
    let api = kahawai_hub::api::router(
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
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
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
    let (api, token, db) = harness().await;
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

/// Unification: capability never filters the list — it changes each
/// track's computed delivery. A no-overlay client still sees the PGS
/// track; with no burn-capable host it reads `delivery: none`.
#[tokio::test]
async fn capability_changes_delivery_not_existence() {
    // No router needed: this one asks `Subtitles::list` directly.
    let (_api, _token, db) = harness().await;
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
    q(r#"INSERT INTO files
              (module_id,collection_id,root_id,path_rel,item_id,size,mtime_unix,
               head_xxh3,tail_xxh3,oshash,subs_extracted,streams_json)
       VALUES ('m2','c2',(SELECT id FROM collection_roots WHERE module_id='m2'),
               'subbed.mkv','subs-item',700,1,0,0,0,0,
               '{"container":"matroska",
                 "subtitles":[{"format":"srt","language":"en"},
                              {"format":"ass","language":"en"},
                              {"format":"pgs","language":"en"}]}')"#)
    .await;
    // What a scan would materialize (tests seed by SQL, so no
    // sync_source_tracks ran).
    q("INSERT INTO subtitle_tracks(item_id,source_id,origin,stream_index,format,language)
       VALUES('subs-item',(SELECT id FROM files WHERE item_id='subs-item'),'embedded',0,'srt','en'),
             ('subs-item',(SELECT id FROM files WHERE item_id='subs-item'),'embedded',1,'ass','en'),
             ('subs-item',(SELECT id FROM files WHERE item_id='subs-item'),'embedded',2,'pgs','en')")
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
/// orphan them); a vanished stream deletes its row and lineage links
/// go NULL rather than dangling.
#[tokio::test]
async fn scan_sync_preserves_track_ids() {
    let (_api, _token, db) = harness().await;
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
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('sync','c','m.mkv','it',1,1,0,0,0,'{}') RETURNING id",
    )
    .fetch_one(&db)
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
            kahawai_hub::tracks::sync_source_tracks(&mut tx, "it", source_id, &info)
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
        "INSERT INTO subtitle_tracks (item_id, origin, format, machine, derived_from)
         VALUES ('it','ocr','srt',1,?) RETURNING id",
    )
    .bind(first[1])
    .fetch_one(&db)
    .await
    .unwrap();

    // Rescan, same streams: ids survive.
    sync(info.clone()).await;
    assert_eq!(ids("embedded").await, first, "rescan must preserve ids");

    // The PGS stream vanishes: its row goes, the OCR row stays but its
    // lineage link nulls (ON DELETE SET NULL).
    let mut less = info.clone();
    less.subtitles.truncate(1);
    sync(less).await;
    assert_eq!(ids("embedded").await, vec![first[0]]);
    let link: Option<i64> =
        sqlx::query_scalar("SELECT derived_from FROM subtitle_tracks WHERE id = ?")
            .bind(ocr)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(link, None, "lineage must null, not dangle");
}
