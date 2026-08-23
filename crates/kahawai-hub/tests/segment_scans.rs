//! HUB-37: which seasons the detector still has to look at.
//!
//! The subject is the scan record's meaning. It says "these BYTES have been
//! analysed", not "this episode has been analysed" — replace the file and the
//! question is open again, which is the whole reason a re-download of a
//! truncated episode is worth anything.

use sqlx::Row;

async fn library() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO collections(module_id,collection_id,media_type)
           VALUES('host','series','series');
         INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
           VALUES('show','show','Show','show','show','host','series');
         INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,parent_id,season,episode)
           VALUES('e1','episode','One','one','one','host','series','show',1,1),
                 ('e2','episode','Two','two','two','host','series','show',1,2);
         INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           VALUES('host','series','e1.mkv',10,1000,1,2,3,'{\"duration_ms\":1200000}'),
                 ('host','series','e2.mkv',10,1000,1,2,3,'{\"duration_ms\":1200000}');
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('host','series','e1',NULL,'file:e1',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'host','series',1,id FROM files WHERE path_rel='e1.mkv';
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('host','series','e2',NULL,'file:e2',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'host','series',1,id FROM files WHERE path_rel='e2.mkv';",
    )
    .execute(&db)
    .await
    .unwrap();
    (dir, db)
}

/// Record a finished scan the way `analyze_season` does.
async fn scanned(db: &sqlx::SqlitePool, item: &str, mtime: Option<i64>) {
    sqlx::query(
        "INSERT INTO media_segment_scans(item_id,scanned_at,detector,mtime_unix)
         VALUES(?, unixepoch(), ?, ?)
         ON CONFLICT(item_id) DO UPDATE SET mtime_unix = excluded.mtime_unix",
    )
    .bind(item)
    .bind(kahawai_core::segments::DETECTOR_GENERATION)
    .bind(mtime)
    .execute(db)
    .await
    .unwrap();
}

async fn pending(db: &sqlx::SqlitePool) -> i64 {
    kahawai_hub::segments::pending_seasons(db)
        .await
        .unwrap()
        .first()
        .map(|s| s.pending)
        .unwrap_or(0)
}

#[tokio::test]
async fn a_season_is_pending_until_its_episodes_are_scanned() {
    let (_dir, db) = library().await;
    assert_eq!(pending(&db).await, 2, "nothing looked at yet");

    scanned(&db, "e1", Some(1000)).await;
    assert_eq!(pending(&db).await, 1);

    scanned(&db, "e2", Some(1000)).await;
    assert_eq!(pending(&db).await, 0, "both episodes answered for");
}

#[tokio::test]
async fn replacing_the_file_asks_again() {
    // The case this exists for: an episode whose download was truncated is
    // analysed, finds nothing because the bytes are broken, and is fetched
    // again. Keyed on the item alone, that row would say "done" for ever.
    let (_dir, db) = library().await;
    scanned(&db, "e1", Some(1000)).await;
    scanned(&db, "e2", Some(1000)).await;
    assert_eq!(pending(&db).await, 0);

    sqlx::query("UPDATE files SET mtime_unix = 2000 WHERE path_rel = 'e2.mkv'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(pending(&db).await, 1, "the replaced episode is open again");
}

#[tokio::test]
async fn a_scan_from_before_the_column_is_still_a_scan() {
    // Rows written before the identity was recorded carry NULL. Treating that
    // as a mismatch would re-analyse a whole library on upgrade, for nothing.
    let (_dir, db) = library().await;
    scanned(&db, "e1", None).await;
    scanned(&db, "e2", None).await;
    assert_eq!(pending(&db).await, 0);
}

#[tokio::test]
async fn an_episode_with_no_running_time_keeps_its_season_pending() {
    // The analyzer needs a running time before it reads a byte, so an episode
    // without one is skipped and never gets a scan row — while the query goes
    // on counting it. The sweep has to notice that a season it just worked on
    // came back unchanged, or it re-reads that season from the mediahost for
    // ever.
    let (_dir, db) = library().await;
    sqlx::raw_sql(
        "INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,parent_id,season,episode)
           VALUES('e3','episode','Three','three','three','host','series','show',1,3);
         INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           VALUES('host','series','e3.mkv',10,1000,1,2,3,'{}');
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('host','series','e3',NULL,'file:e3',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'host','series',1,id FROM files WHERE path_rel='e3.mkv';",
    )
    .execute(&db)
    .await
    .unwrap();

    scanned(&db, "e1", Some(1000)).await;
    scanned(&db, "e2", Some(1000)).await;
    assert_eq!(pending(&db).await, 1, "and no pass will ever cross it off");
}

#[tokio::test]
async fn a_second_viewer_does_not_double_the_season() {
    // `watch_state` is keyed on (user, item). Joined rather than subselected,
    // it counted an episode once per person who had touched it — and both the
    // episode count and the PENDING count were counts of that fan-out. Three
    // users on the real hub was enough to invent three episodes.
    //
    // The pending count is not just a display: the sweep compares it between
    // looks to notice a season it cannot finish, so a number that moves when
    // somebody presses play is a season re-read from the mediahost in a loop.
    let (_dir, db) = library().await;
    sqlx::raw_sql(
        "INSERT INTO users(id,username,password_hash,created_at)
           VALUES('u1','one','x',1),('u2','two','x',1);
         INSERT INTO watch_state(user_id,item_id,position_ms,played,updated_at)
           VALUES('u1','e1',60000,1,100),('u2','e1',30000,0,200);",
    )
    .execute(&db)
    .await
    .unwrap();

    let seasons = kahawai_hub::segments::pending_seasons(&db).await.unwrap();
    let season = seasons.first().expect("the season is pending");
    assert_eq!(
        season.episodes, 2,
        "two episodes, however many people watched"
    );
    assert_eq!(season.pending, 2);
}

#[tokio::test]
async fn the_season_someone_is_watching_comes_first() {
    // The ordering is the whole point of the sweep: skip buttons should land
    // on the show somebody is halfway through. `watched_at` is a bare
    // per-episode subselect inside a GROUP BY, and without an outer MAX()
    // SQLite takes it from an ARBITRARY row of the group — so a season whose
    // FIRST episode was just watched could sort by another episode's zero.
    let (_dir, db) = library().await;
    sqlx::raw_sql(
        "INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id)
           VALUES('ztouched','show','Ztouched','ztouched','ztouched','host','series');
         INSERT INTO items(id,kind,title,norm_title,sort_title,module_id,collection_id,parent_id,season,episode)
           VALUES('z1','episode','One','one','one','host','series','ztouched',1,1),
                 ('z2','episode','Two','two','two','host','series','ztouched',1,2);
         INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
           VALUES('host','series','z1.mkv',10,1000,1,2,3,'{\"duration_ms\":1200000}'),
                 ('host','series','z2.mkv',10,1000,1,2,3,'{\"duration_ms\":1200000}');
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('host','series','z1',NULL,'file:z1',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'host','series',1,id FROM files WHERE path_rel='z1.mkv';
         INSERT INTO playable_sources(module_id,collection_id,item_id,root_id,family_key,expected_parts)
           VALUES('host','series','z2',NULL,'file:z2',1);
         INSERT INTO playable_source_parts(playable_source_id,module_id,collection_id,ordinal,file_id)
           SELECT last_insert_rowid(),'host','series',1,id FROM files WHERE path_rel='z2.mkv';
         INSERT INTO users(id,username,password_hash,created_at) VALUES('u','u','x',1);
         -- The watch on z2, the SECOND episode: an arbitrary-row pick takes
         -- the group's first row, so a watch on z1 passed even without the
         -- outer MAX() — the fixture has to put the signal where the
         -- arbitrary pick is not looking. The base library's e1-watch below
         -- covers the first-episode case at a lower rank.
         INSERT INTO watch_state(user_id,item_id,position_ms,played,updated_at)
           VALUES('u','z2',60000,0,9999),
                 ('u','e1',60000,0,5000);",
    )
    .execute(&db)
    .await
    .unwrap();

    let seasons = kahawai_hub::segments::pending_seasons(&db).await.unwrap();
    let order: Vec<&str> = seasons.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        order.first(),
        Some(&"Ztouched"),
        "the watched season outranks, whatever episode carried the watch: {order:?}"
    );
    assert!(
        order.len() >= 2 && order[1] != "Ztouched",
        "and the first-episode watch ranks its season second: {order:?}"
    );
}

/// A detector generation is the other thing that invalidates a scan, and it
/// invalidates every one of them at once.
#[tokio::test]
async fn a_newer_detector_asks_the_whole_season_again() {
    let (_dir, db) = library().await;
    scanned(&db, "e1", Some(1000)).await;
    scanned(&db, "e2", Some(1000)).await;
    assert_eq!(pending(&db).await, 0);

    sqlx::query("UPDATE media_segment_scans SET detector = detector - 1")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(pending(&db).await, 2);
    // And the rows are still there, so nothing had to be deleted to ask again.
    let rows: i64 = sqlx::query("SELECT COUNT(*) AS n FROM media_segment_scans")
        .fetch_one(&db)
        .await
        .unwrap()
        .get("n");
    assert_eq!(rows, 2);
}
