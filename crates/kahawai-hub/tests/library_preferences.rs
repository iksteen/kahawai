//! Exact track choices retain their copy ownership through import and copy repair.
use kahawai_hub::{db, library::Database};
use sqlx::SqliteConnection;

async fn fixture() -> Database {
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies');
        INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');")
        .execute(&db).await.unwrap();
    db
}
async fn movie(c: &mut SqliteConnection, id: &str) {
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES(?,'movie',?,?,2000,'host','one')")
        .bind(id).bind(id).bind(id).execute(c).await.unwrap();
}
async fn source(c: &mut SqliteConnection, copy: &str, path: &str) -> i64 {
    let file:i64=sqlx::query_scalar("INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json) VALUES('host','one',?,100,1,2,3,4,'{}') RETURNING id")
        .bind(path).fetch_one(&mut *c).await.unwrap();
    let source:i64=sqlx::query_scalar("INSERT INTO playable_sources(module_id,collection_id,item_id,family_key,expected_parts) VALUES('host','one',?,?,1) RETURNING id")
        .bind(copy).bind(path).fetch_one(&mut *c).await.unwrap();
    sqlx::query("INSERT INTO playable_source_parts VALUES(?,'host','one',1,?)")
        .bind(source)
        .bind(file)
        .execute(&mut *c)
        .await
        .unwrap();
    source
}
async fn pref(c: &mut SqliteConnection, scope: &str, key: &str, value: &str) {
    sqlx::query("INSERT INTO user_prefs(user_id,scope,key,value) VALUES('user',?,?,?)")
        .bind(scope)
        .bind(key)
        .bind(value)
        .execute(c)
        .await
        .unwrap();
}
async fn value(db: &Database, scope: &str, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM user_prefs WHERE user_id='user' AND scope=? AND key=?")
        .bind(scope)
        .bind(key)
        .fetch_optional(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn initial_import_qualifies_single_source_choices_and_retains_ambiguous_originals() {
    let db = fixture().await;
    let mut tx = db.begin().await.unwrap();
    movie(&mut tx, "single").await;
    let single = source(&mut tx, "single", "single.mkv").await;
    pref(&mut tx, "single", "audio", "#3").await;
    pref(&mut tx, "single", "subs.track", "41").await;
    pref(&mut tx, &format!("source:{single}"), "audio.track", "#9").await;
    movie(&mut tx, "multiple").await;
    let a = source(&mut tx, "multiple", "a.mkv").await;
    let b = source(&mut tx, "multiple", "b.mkv").await;
    pref(&mut tx, "multiple", "audio.track", "#2").await;
    tx.commit().await.unwrap();
    assert_eq!(
        value(&db, &format!("source:single:{single}"), "audio.track").await,
        Some("#3".into())
    );
    assert_eq!(
        value(&db, &format!("source:single:{single}"), "subs.track").await,
        Some("41".into())
    );
    assert_eq!(value(&db, "single", "audio").await, Some("#3".into()));
    assert_eq!(
        value(&db, &format!("source:{single}"), "audio.track").await,
        Some("#9".into())
    );
    for id in [a, b] {
        assert_eq!(
            value(&db, &format!("source:multiple:{id}"), "audio.track").await,
            None
        );
    }
    assert_eq!(
        value(&db, "multiple", "audio.track").await,
        Some("#2".into())
    );
}

#[tokio::test]
async fn migration_imports_proven_originals_and_keeps_existing_qualified_choices() {
    let db = fixture().await;
    let mut tx = db.begin().await.unwrap();
    movie(&mut tx, "single").await;
    let single = source(&mut tx, "single", "single.mkv").await;
    movie(&mut tx, "multiple").await;
    let a = source(&mut tx, "multiple", "a.mkv").await;
    source(&mut tx, "multiple", "b.mkv").await;
    tx.commit().await.unwrap();
    // These preferences already existed when migration 80 was introduced;
    // initial-assignment import no longer runs for these copies.
    let mut tx = db.begin().await.unwrap();
    pref(&mut tx, "single", "audio", "#2").await;
    pref(&mut tx, "single", "subs.track", "10").await;
    pref(
        &mut tx,
        &format!("source:single:{single}"),
        "audio.track",
        "#4",
    )
    .await;
    pref(&mut tx, "multiple", "audio.track", "#1").await;
    tx.commit().await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0080_copy_qualified_source_preferences.sql"
    ))
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        value(&db, &format!("source:single:{single}"), "audio.track").await,
        Some("#4".into())
    );
    assert_eq!(
        value(&db, &format!("source:single:{single}"), "subs.track").await,
        Some("10".into())
    );
    assert_eq!(
        value(&db, &format!("source:multiple:{a}"), "audio.track").await,
        None
    );
    assert_eq!(value(&db, "single", "audio").await, Some("#2".into()));
    assert_eq!(
        value(&db, "multiple", "audio.track").await,
        Some("#1".into())
    );
}

#[tokio::test]
async fn migration_does_not_give_a_reused_source_id_its_previous_copys_preferences() {
    let db = fixture().await;
    let mut tx = db.begin().await.unwrap();
    movie(&mut tx, "old").await;
    let old = source(&mut tx, "old", "old.mkv").await;
    pref(&mut tx, &format!("source:{old}"), "audio.track", "#7").await;
    pref(&mut tx, &format!("source:{old}"), "subs.track", "99").await;
    tx.commit().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::query("DELETE FROM playable_sources WHERE id=?")
        .bind(old)
        .execute(&mut *tx)
        .await
        .unwrap();
    movie(&mut tx, "new").await;
    let reused = source(&mut tx, "new", "new.mkv").await;
    tx.commit().await.unwrap();
    assert_eq!(old, reused, "fixture must exercise numeric ID reuse");
    sqlx::raw_sql(include_str!(
        "../migrations/0080_copy_qualified_source_preferences.sql"
    ))
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        value(&db, &format!("source:new:{reused}"), "audio.track").await,
        None
    );
    assert_eq!(
        value(&db, &format!("source:new:{reused}"), "subs.track").await,
        None
    );
    assert_eq!(
        value(&db, &format!("source:{old}"), "audio.track").await,
        Some("#7".into())
    );
}

#[tokio::test]
async fn episode_copy_split_moves_qualified_choices_and_remaps_only_its_subtitle() {
    let db = fixture().await;
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('show','show','Show','show',2000,'host','one');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,episode_end,module_id,collection_id) VALUES('legacy','episode','Episodes 1-2','episodes 1-2','show',1,1,2,'host','one');").execute(&mut *tx).await.unwrap();
    source(&mut tx, "legacy", "Show.S01E01-E02.mkv").await;
    let single = source(&mut tx, "legacy", "Show.S01E01.mkv").await;
    sqlx::raw_sql("INSERT INTO subtitle_tracks(id,item_id,origin,format,language,created_by) VALUES(100,'legacy','downloaded','srt','en','user');
        INSERT INTO subtitle_tracks(id,item_id,origin,format,derived_from,payload_id) VALUES(101,'legacy','raster','pgs',100,201);").execute(&mut *tx).await.unwrap();
    let old_scope = format!("source:legacy:{single}");
    pref(&mut tx, &old_scope, "subs.track", "100").await;
    pref(&mut tx, &old_scope, "audio.track", "#3").await;
    pref(&mut tx, &format!("source:{single}"), "subs.track", "777").await;
    tx.commit().await.unwrap();
    db.write("queue legacy coverage repair", |c| {
        Box::pin(async move {
            sqlx::query("INSERT INTO library_pending VALUES('legacy') ON CONFLICT DO NOTHING")
                .execute(c)
                .await?;
            Ok(())
        })
    })
    .await
    .unwrap();
    kahawai_hub::library::initialize(&db).await.unwrap();
    let new_copy: String = sqlx::query_scalar("SELECT item_id FROM playable_sources WHERE id=?")
        .bind(single)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_ne!(new_copy, "legacy");
    let track: i64 = sqlx::query_scalar(
        "SELECT id FROM subtitle_tracks WHERE item_id=? AND origin='downloaded'",
    )
    .bind(&new_copy)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_ne!(track, 100);
    let new_scope = format!("source:{new_copy}:{single}");
    assert_eq!(
        value(&db, &new_scope, "subs.track").await,
        Some(track.to_string())
    );
    assert_eq!(
        value(&db, &new_scope, "audio.track").await,
        Some("#3".into())
    );
    assert_eq!(
        value(&db, &old_scope, "subs.track").await,
        Some("100".into())
    );
    assert_eq!(
        value(&db, &format!("source:{single}"), "subs.track").await,
        Some("777".into())
    );
}
