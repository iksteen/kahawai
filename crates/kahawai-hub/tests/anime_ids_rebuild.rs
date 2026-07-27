//! `anime_ids` is derived state and must be reconstructible from the
//! answers already on disk.
//!
//! It used to be written only at match time, so losing it lost it: the
//! provider answers are still there, never-ask-twice skips the provider,
//! and the id never returns. Found by wiping AniDB state deliberately —
//! three items came back without their `anidb_id` and stayed that way.

use kahawai_hub::enrich::Enricher;
use sqlx::Row;

async fn seed(db: &sqlx::SqlitePool) {
    // One anime show whose first file carries an ed2k AniDB identified,
    // and an AniList answer whose provider_id IS the AniList media id.
    sqlx::query("INSERT INTO items (id, kind, title, norm_title) VALUES ('show1','show','Lain','lain')")
        .execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO files (module_id, collection_id, path_rel, size, mtime_unix,
                            head_xxh3, tail_xxh3, oshash, streams_json, subs_extracted, ed2k)
         VALUES ('m','c','Lain/ep01.mkv', 1, 1, 0, 0, 0, '{}', 0, 'deadbeef')",
    )
    .execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO item_sources (item_id, module_id, collection_id, path_rel)
         VALUES ('show1','m','c','Lain/ep01.mkv')",
    )
    .execute(db).await.unwrap();
    sqlx::query("INSERT INTO ed2k_aid (ed2k, aid, updated_at) VALUES ('deadbeef', 2129, unixepoch())")
        .execute(db).await.unwrap();
    sqlx::query(
        "INSERT INTO provider_metadata (item_id, provider, provider_id, title, confidence, updated_at)
         VALUES ('show1','anilist','1211','Lain','auto', unixepoch())",
    )
    .execute(db).await.unwrap();
}

#[tokio::test]
async fn bridge_ids_are_rebuilt_from_stored_answers() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    seed(&db).await;

    // Nothing recorded yet: both ids come purely from what is on disk.
    let n = Enricher::rebuild_anime_ids(&db).await.unwrap();
    assert_eq!(n, 1, "one item should have been rebuilt");
    let row = sqlx::query("SELECT anidb_id, anilist_id FROM anime_ids WHERE item_id = 'show1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(row.get::<Option<i64>, _>("anidb_id"), Some(2129), "aid from ed2k_aid");
    assert_eq!(row.get::<Option<i64>, _>("anilist_id"), Some(1211), "anilist id from its answer");

    // The case that started this: the id is wiped while every provider
    // answer survives, so no provider would ever be asked again.
    sqlx::query("UPDATE anime_ids SET anidb_id = NULL").execute(&db).await.unwrap();
    Enricher::rebuild_anime_ids(&db).await.unwrap();
    let back: Option<i64> =
        sqlx::query_scalar("SELECT anidb_id FROM anime_ids WHERE item_id = 'show1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(back, Some(2129), "a wiped id must come back from disk");

    // It fills holes and never overwrites: a correction outranks a
    // reconstruction, which is the whole reason this is safe to run on
    // every pass.
    sqlx::query("UPDATE anime_ids SET anidb_id = 9999").execute(&db).await.unwrap();
    Enricher::rebuild_anime_ids(&db).await.unwrap();
    let kept: Option<i64> =
        sqlx::query_scalar("SELECT anidb_id FROM anime_ids WHERE item_id = 'show1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(kept, Some(9999), "an existing id must survive a rebuild");
}
