//! HUB-30/30a: a file's ED2K hash states which episode it IS, and on
//! disagreement with the filename, the hash wins.
//!
//! The AniDB lookups are network and tested live; the BINDER is pure
//! database work, so everything it may move — sources, watch state,
//! ghost rows — is pinned here, hashes seeded as the resolver would
//! have cached them.

use kahawai_hub::enrich::Enricher;
use sqlx::SqlitePool;

const AID: u32 = 1234;

async fn harness() -> (Enricher, SqlitePool, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let enricher = Enricher::new(dir.path().to_path_buf());
    sqlx::query(
        "INSERT INTO satellites (module_id, module_type, name, cert_fingerprint, enrolled_at, disabled)
         VALUES ('m','mediahost','m','',unixepoch(),0)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO collections (module_id, collection_id, media_type, roots_json, sync_version)
         VALUES ('m','c','anime','[\"/m\"]',1)",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES ('show','show','X','x')")
        .execute(&db)
        .await
        .unwrap();
    (enricher, db, dir)
}

/// An episode item with one hashed file bound to it, and the cached
/// AniDB answer for that hash.
async fn episode(
    db: &SqlitePool,
    id: &str,
    season: Option<i64>,
    ep: i64,
    file_aid: u32,
    epno: &str,
) {
    sqlx::query(
        "INSERT INTO items (id, kind, title, norm_title, parent_id, season, episode)
         VALUES (?, 'episode', ?, ?, 'show', ?, ?)",
    )
    .bind(id)
    .bind(format!("title {id}"))
    .bind(format!("title {id}"))
    .bind(season)
    .bind(ep)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                            head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted, ed2k)
         VALUES ('m','c', ? || '.mkv', 700, 1, 0, 0, 0, '{}', 0, 'hash-' || ?)",
    )
    .bind(id)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
         VALUES (?, 'm', 'c', ? || '.mkv')",
    )
    .bind(id)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT OR REPLACE INTO ed2k_aid (ed2k, aid, eid, epno, gid, group_name, updated_at)
         VALUES ('hash-' || ?, ?, 9, ?, 7, 'Grp', unixepoch())",
    )
    .bind(id)
    .bind(file_aid)
    .bind(epno)
    .execute(db)
    .await
    .unwrap();
}

async fn slot_of(db: &SqlitePool, path: &str) -> (Option<i64>, i64, String) {
    sqlx::query_as::<_, (Option<i64>, i64, String)>(
        "SELECT i.season, i.episode, i.id FROM item_sources s JOIN items i ON i.id = s.item_id
          WHERE s.path_rel = ?",
    )
    .bind(path)
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_hash_wins_over_the_filename() {
    let (enricher, db, _dir) = harness().await;
    // Filename said 6; AniDB says the file IS episode 5, which exists.
    episode(&db, "e5", None, 5, AID, "05").await;
    episode(&db, "e6", None, 6, AID, "5").await; // misnumbered rip of ep 5
    // The user watched it under the wrong number.
    sqlx::query("INSERT INTO users (id, username, password_hash, is_admin, created_at)
                 VALUES ('u','u','x',0,unixepoch())")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO watch_state (user_id, item_id, position_ms, duration_ms, played, play_count, updated_at)
         VALUES ('u','e6',120000,1200000,1,1,unixepoch())",
    )
    .execute(&db)
    .await
    .unwrap();

    let moves = enricher.bind_hashed_episodes(&db, "show", AID).await.unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!((moves[0].from, moves[0].to), ((None, 6), (None, 5)));

    // Both files now back episode 5 — a second source, exactly HUB-3.
    assert_eq!(slot_of(&db, "e5.mkv").await.0, None);
    assert_eq!(slot_of(&db, "e6.mkv").await, (None, 5, "e5".into()));
    // The misnumbered item is a ghost and is gone; the watch state moved
    // with the content.
    let ghost: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id='e6'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(ghost, 0, "a sourceless misnumbered episode must not linger");
    let watched: String =
        sqlx::query_scalar("SELECT item_id FROM watch_state WHERE user_id='u'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(watched, "e5");

    // Idempotent: nothing left to move.
    let again = enricher.bind_hashed_episodes(&db, "show", AID).await.unwrap();
    assert!(again.is_empty(), "{again:?}");
}

#[tokio::test]
async fn specials_land_in_season_zero_and_the_rest_is_left_alone() {
    let (enricher, db, _dir) = harness().await;
    // Parsed as absolute episode 0 — the classic misfiled special.
    episode(&db, "sp", None, 0, AID, "S2").await;
    // A credits file squatting on an episode slot: an artifact of the
    // numbering, moved into season 0's credits band.
    episode(&db, "op", None, 90, AID, "C1").await;
    // A file from a DIFFERENT AniDB entry (per-season split): left alone.
    episode(&db, "other", None, 40, AID + 1, "3").await;
    // Season-keyed episode: AniDB numbering is not this space; left.
    episode(&db, "skeyed", Some(2), 3, AID, "4").await;

    let moves = enricher.bind_hashed_episodes(&db, "show", AID).await.unwrap();
    assert_eq!(moves.len(), 2, "{moves:?}");

    let sp = slot_of(&db, "sp.mkv").await;
    assert_eq!((sp.0, sp.1), (Some(0), 2), "special bound into season 0");
    let op = slot_of(&db, "op.mkv").await;
    assert_eq!((op.0, op.1), (Some(0), 101), "credits into season 0's C band");
    assert_eq!(slot_of(&db, "other.mkv").await, (None, 40, "other".into()), "cross-aid stays put");
    assert_eq!(slot_of(&db, "skeyed.mkv").await, (Some(2), 3, "skeyed".into()));

    // The created season-0 item carried the file's own title.
    let title: String =
        sqlx::query_scalar("SELECT title FROM items WHERE parent_id='show' AND season=0 AND episode=2")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(title, "title sp");
}
