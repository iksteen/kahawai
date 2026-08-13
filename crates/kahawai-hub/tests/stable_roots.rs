//! DATA-1..6: deterministic exact-root identity and lossless adoption.

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use sqlx::Row;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

fn root(path: &str) -> String {
    kahawai_core::media::root_token(std::path::Path::new(path))
}

fn record(root_token: &str, path_rel: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        root_token: root_token.into(),
        path_rel: path_rel.into(),
        size,
        mtime_unix: 7,
        head_xxh3: size + 1,
        tail_xxh3: size + 2,
        oshash: size + 3,
        streams_json: r#"{"container":"matroska"}"#.into(),
    }
}

#[test]
fn exact_read_uses_the_token_not_root_order() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::write(a.path().join("same.mkv"), b"root-a").unwrap();
    std::fs::write(b.path().join("same.mkv"), b"root-b").unwrap();
    let collection = kahawai_core::media::CollectionConfig {
        name: "movies".into(),
        media_type: "movies".into(),
        roots: vec![b.path().into(), a.path().into()],
    };
    let a_path = kahawai_mediahost::serve::resolve_rel(
        std::slice::from_ref(&collection),
        "movies",
        &root(a.path().to_str().unwrap()),
        "same.mkv",
    )
    .unwrap();
    let b_path = kahawai_mediahost::serve::resolve_rel(
        &[collection],
        "movies",
        &root(b.path().to_str().unwrap()),
        "same.mkv",
    )
    .unwrap();
    assert_eq!(std::fs::read(a_path).unwrap(), b"root-a");
    assert_eq!(std::fs::read(b_path).unwrap(), b"root-b");
}

#[tokio::test]
async fn persisted_root_token_path_mismatches_are_rejected() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    let token = root("/media/a");
    registry
        .announce_collection("host", "movies", "movies", &["/media/a".into()])
        .await
        .unwrap();
    sqlx::query("UPDATE collection_roots SET normalized_path='/different' WHERE root_token=?")
        .bind(&token)
        .execute(&db)
        .await
        .unwrap();
    let error = registry
        .announce_collection("host", "movies", "movies", &["/media/a".into()])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("previously stored"), "{error:#}");
}

#[tokio::test]
async fn identical_relative_paths_persist_as_distinct_sources() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    let a = root("/media/a");
    let b = root("/media/b");
    registry
        .announce_collection(
            "host",
            "movies",
            "movies",
            &["/media/b".into(), "/media/a".into()],
        )
        .await
        .unwrap();
    registry
        .upsert_files(
            "host",
            "movies",
            vec![record(&a, "same.mkv", 10), record(&b, "same.mkv", 20)],
        )
        .await
        .unwrap();
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT f.id,r.root_token,f.path_rel FROM files f
         JOIN collection_roots r ON r.id=f.root_id ORDER BY r.root_token",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(_, _, path)| path == "same.mkv"));
    assert_ne!(rows[0].0, rows[1].0);
    assert_ne!(rows[0].1, rows[1].1);
}

async fn level52_fixture() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.path().join("hub.db"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    MIGRATOR.run_to(52, &db).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
           VALUES('host','mediahost','host','cert');
         INSERT INTO collections(module_id,collection_id,media_type,roots_json,sync_version)
           VALUES('host','movies','movies','[\"/media/only\"]',91);
         INSERT INTO libraries(id,name,media_type) VALUES('lib','Movies','movies');
         INSERT INTO library_collections(library_id,module_id,collection_id)
           VALUES('lib','host','movies');
         INSERT INTO users(id,username,password_hash,is_admin) VALUES('user','u','x',0);
         INSERT INTO user_libraries(user_id,library_id) VALUES('user','lib');
         INSERT INTO items(id,kind,title,norm_title) VALUES('item','movie','Same','same');
         INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           VALUES('host','movies','same.mkv',10,7,11,12,13,'{}');
         INSERT INTO item_sources(module_id,collection_id,path_rel,item_id)
           VALUES('host','movies','same.mkv','item');
         INSERT INTO provider_metadata(item_id,provider,provider_id,title,confidence,updated_at)
           VALUES('item','tmdb','42','Same','auto',1);
         INSERT INTO provider_queries(item_id,provider,query_type,query,rev,asked_at)
           VALUES('item','tmdb','title','Same',1,1);
         INSERT INTO manual_match(item_id,provider,provider_id,pinned_at)
           VALUES('item','tmdb','42',1);
         INSERT INTO enrichment_queue(item_id,provider,due_at,attempts,reason)
           VALUES('item','tvdb',9,2,'retry');
         INSERT INTO item_relations(from_item,kind,target_anilist,target_title)
           VALUES('item','sequel',99,'Next');
         INSERT INTO watch_state(user_id,item_id,position_ms,play_count)
           VALUES('user','item',1234,3);
         INSERT INTO subtitle_tracks(id,item_id,origin,module_id,collection_id,path_rel,
                                     stream_index,format)
           VALUES(44,'item','embedded','host','movies','same.mkv',0,'srt');
         INSERT INTO image_set_failures(module_id,collection_id,path_rel,sub_index,error,at)
           VALUES('host','movies','same.mkv',0,'no index',1);",
    )
    .execute(&db)
    .await
    .unwrap();
    (dir, db)
}

#[tokio::test]
async fn migration_and_single_root_adoption_preserve_durable_state() {
    let (_dir, db) = level52_fixture().await;
    MIGRATOR.run(&db).await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .announce_collection("host", "movies", "movies", &["/media/only".into()])
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT f.id,f.item_id,r.root_token,f.path_rel FROM files f
         JOIN collection_roots r ON r.id=f.root_id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let source_id = row.get::<i64, _>("id");
    assert_eq!(row.get::<String, _>("item_id"), "item");
    assert_eq!(row.get::<String, _>("root_token"), root("/media/only"));
    assert_eq!(row.get::<String, _>("path_rel"), "same.mkv");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT source_id FROM subtitle_tracks WHERE id=44")
            .fetch_one(&db)
            .await
            .unwrap(),
        source_id
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT source_id FROM image_set_failures")
            .fetch_one(&db)
            .await
            .unwrap(),
        source_id
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sync_version FROM collections")
            .fetch_one(&db)
            .await
            .unwrap(),
        91
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT (SELECT COUNT(*) FROM provider_metadata)
                  +(SELECT COUNT(*) FROM provider_queries)
                  +(SELECT COUNT(*) FROM manual_match)
                  +(SELECT COUNT(*) FROM enrichment_queue)
                  +(SELECT COUNT(*) FROM item_relations)
                  +(SELECT COUNT(*) FROM watch_state)
                  +(SELECT COUNT(*) FROM user_libraries)"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        7
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pragma_foreign_key_check")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn single_root_announcement_adopts_legacy_state_without_rescan() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .announce_collection("host", "movies", "movies", &[])
        .await
        .unwrap();
    sqlx::query("UPDATE collections SET sync_version=91 WHERE module_id='host'")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('item','movie','Same','same','host','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('host','movies','same.mkv','item',10,7,11,12,13,'{}') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO subtitle_tracks(id,source_id,origin,stream_index,format)
         VALUES(44,?,'embedded',0,'srt')",
    )
    .bind(source_id)
    .execute(&db)
    .await
    .unwrap();

    registry
        .announce_collection("host", "movies", "movies", &["/media/only".into()])
        .await
        .unwrap();
    assert_eq!(
        registry
            .source_id("host", "movies", &root("/media/only"), "same.mkv")
            .await
            .unwrap(),
        Some(source_id)
    );
    assert!(
        registry
            .root_adoption_pending("host", "movies")
            .await
            .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sync_version FROM collections")
            .fetch_one(&db)
            .await
            .unwrap(),
        91
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT source_id FROM subtitle_tracks WHERE id=44")
            .fetch_one(&db)
            .await
            .unwrap(),
        source_id
    );
}

#[tokio::test]
async fn multi_root_legacy_state_is_never_guessed() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry
        .announce_collection("host", "movies", "movies", &[])
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('item','movie','Same','same','host','movies')",
    )
    .execute(&db)
    .await
    .unwrap();
    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('host','movies','same.mkv','item',10,7,11,12,13,'{}') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();

    registry
        .announce_collection(
            "host",
            "movies",
            "movies",
            &["/media/a".into(), "/media/b".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        registry
            .unresolved_legacy_sources("host", "movies")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>("SELECT root_id FROM files WHERE id=?")
            .bind(source_id)
            .fetch_one(&db)
            .await
            .unwrap(),
        None
    );
    let token = root("/media/b");
    registry
        .adopt_legacy_source("host", "movies", &token, "same.mkv")
        .await
        .unwrap();
    assert!(
        registry
            .unresolved_legacy_sources("host", "movies")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        registry
            .source_id("host", "movies", &token, "same.mkv")
            .await
            .unwrap(),
        Some(source_id)
    );
}
