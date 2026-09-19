//! Upgrade projection-derived episode keys without losing a work's history.
use kahawai_hub::{db, library::Database};
use sqlx::SqliteConnection;

async fn fixture(native_counterpart: bool) -> Database {
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','anime');
        INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('series','show','Series','series',2000,'host','one');
        INSERT INTO provider_metadata(item_id,provider,provider_id,title,premiered,confidence,updated_at) VALUES('series','tmdb','1','Series','2000-01-01','auto',1);
        INSERT INTO manual_match(item_id,provider,provider_id,pinned_at) VALUES('series','tmdb','1',1);")
        .execute(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    if native_counterpart {
        episode(&mut tx, "native", 13).await;
    }
    episode(&mut tx, "projected", 13).await;
    tx.commit().await.unwrap();
    // Recreate the pre-fix committed shape: provider S2E1 became the work's
    // identity even though the physical copy still has native absolute 13.
    db.write("pre-native-identity fixture",move |c|Box::pin(async move {
        if native_counterpart {
            sqlx::query("INSERT INTO library_items(id,kind,title,norm_title,sort_title,unidentified,added_id) VALUES('old-projection','episode','Old episode','old episode','old episode',0,'old-projection')").execute(&mut *c).await?;
            sqlx::query("INSERT INTO episode_details(item_id,series_id,season,episode,numbering) VALUES('old-projection','series',2,1,'aired')").execute(&mut *c).await?;
            sqlx::query("UPDATE collection_item_library_items SET library_item_id='old-projection' WHERE collection_item_id='projected'").execute(&mut *c).await?;
        } else {
            sqlx::query("UPDATE episode_details SET season=2,episode=1,numbering='aired' WHERE item_id='projected'").execute(&mut *c).await?;
        }
        sqlx::query("INSERT INTO provider_metadata(item_id,provider,provider_id,title,confidence,proj_season,proj_episode,updated_at) VALUES('projected','tmdb','episode','Projected title','auto',2,1,1)").execute(&mut *c).await?;
        let old=if native_counterpart {"old-projection"} else {"projected"};
        sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,played,play_count,updated_at) VALUES('user',?,123456,1000000,0,3,200)").bind(old).execute(&mut *c).await?;
        sqlx::query("INSERT INTO library_overrides VALUES(?,?)").bind(old).bind(r#"{"overview":"Saved description","rating":5}"#).execute(&mut *c).await?;
        if native_counterpart {
            sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,played,play_count,updated_at) VALUES('user','native',12,1000000,1,1,100)").execute(&mut *c).await?;
            sqlx::query("INSERT INTO library_overrides VALUES('native',?)").bind(r#"{"rating":8}"#).execute(&mut *c).await?;
        }
        Ok(())
    })).await.unwrap();
    db
}
async fn episode(c: &mut SqliteConnection, id: &str, number: i64) {
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES(?,'episode','Episode','episode','series',NULL,?,'host','one')")
        .bind(id).bind(number).execute(c).await.unwrap();
}
async fn migration(db: &Database) {
    db.write("native episode migration", |c| {
        Box::pin(async move {
            sqlx::raw_sql(include_str!(
                "../migrations/0081_native_episode_identity.sql"
            ))
            .execute(c)
            .await?;
            Ok(())
        })
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn projected_key_promotes_history_and_overrides_to_existing_native_episode() {
    let db = fixture(true).await;
    migration(&db).await;
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT unidentified FROM library_items WHERE id='old-projection'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
    kahawai_hub::library::initialize(&db).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='projected'").fetch_one(&db).await.unwrap(),"native");
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT merged_into FROM library_items WHERE id='old-projection'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        "native"
    );
    let state:(i64,i64,i64)=sqlx::query_as("SELECT position_ms,played,play_count FROM user_item_state WHERE item_id='native' AND user_id='user'").fetch_one(&db).await.unwrap();
    assert_eq!(state, (123456, 0, 3));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM user_item_state WHERE item_id='old-projection'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
    let overrides: String =
        sqlx::query_scalar("SELECT fields FROM library_overrides WHERE library_item_id='native'")
            .fetch_one(&db)
            .await
            .unwrap();
    let overrides: serde_json::Value = serde_json::from_str(&overrides).unwrap();
    assert_eq!(overrides["overview"], "Saved description");
    assert_eq!(overrides["rating"], 8.0);
}

#[tokio::test]
async fn projected_key_keeps_its_id_when_no_native_counterpart_exists() {
    let db = fixture(false).await;
    migration(&db).await;
    kahawai_hub::library::initialize(&db).await.unwrap();
    let episode: (String, Option<i64>, i64) = sqlx::query_as(
        "SELECT numbering,season,episode FROM episode_details WHERE item_id='projected'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(episode, ("absolute".into(), None, 13));
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='projected'").fetch_one(&db).await.unwrap(),"projected");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT position_ms FROM user_item_state WHERE item_id='projected'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        123456
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT unidentified FROM library_items WHERE id='projected'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn intentional_or_conflicting_copies_prevent_history_promotion() {
    for scenario in ["manual", "conflicting"] {
        let db = fixture(true).await;
        db.write("non-promotable projection fixture",move |c|Box::pin(async move {
            match scenario {
                "manual"=>{
                    episode(c,"intentional",13).await;
                    sqlx::query("UPDATE collection_items SET assignment_manual=1 WHERE id='intentional'").execute(&mut *c).await?;
                    sqlx::query("INSERT INTO collection_item_library_items VALUES('intentional',1,'old-projection')").execute(&mut *c).await?;
                }
                _=>{
                    episode(c,"conflicting",14).await;
                    sqlx::query("INSERT INTO collection_item_library_items VALUES('conflicting',1,'old-projection')").execute(&mut *c).await?;
                    sqlx::query("INSERT INTO provider_metadata(item_id,provider,provider_id,title,confidence,proj_season,proj_episode,updated_at) VALUES('conflicting','tmdb','other-episode','Another episode','auto',2,1,1)").execute(&mut *c).await?;
                }
            }
            Ok(())
        })).await.unwrap();
        migration(&db).await;
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT unidentified FROM library_items WHERE id='old-projection'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            "{scenario}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT position_ms FROM user_item_state WHERE item_id='old-projection'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            123456,
            "{scenario}"
        );
    }
}

async fn public_id_migration(db: &Database) {
    db.write("preserve public collection IDs", |c| {
        Box::pin(async move {
            sqlx::raw_sql(include_str!(
                "../migrations/0082_public_ids_and_inherited_metadata.sql"
            ))
            .execute(c)
            .await?;
            Ok(())
        })
    })
    .await
    .unwrap();
}

async fn duplicate_legacy_movies() -> Database {
    let db = db::open_in_memory().await.unwrap();
    // Recreate the state migration 77 sees, before the startup matcher runs.
    db.write("legacy movies before initialization", |c| Box::pin(async move {
        sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
            INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
            INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');
            INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id)
            VALUES('copy-a','movie','Shared movie','shared movie',2000,'host','one'),
                  ('copy-b','movie','Shared movie','shared movie',2000,'host','two');
            INSERT INTO state_imports(user_id,item_id,position_ms,play_count,updated_at) VALUES('user','copy-b',123456,3,200);")
            .execute(c).await?;
        Ok(())
    })).await.unwrap();
    db
}

#[tokio::test]
async fn initial_upgrade_preserves_both_public_ids_when_copies_coalesce() {
    let db = duplicate_legacy_movies().await;
    public_id_migration(&db).await;
    kahawai_hub::library::initialize(&db).await.unwrap();
    for id in ["copy-a", "copy-b"] {
        assert_eq!(
            kahawai_hub::library::resolve_id(&db, id).await.unwrap(),
            "copy-a"
        );
        assert_eq!(
            kahawai_hub::library::copies(&db, "user", id).await.unwrap(),
            ["copy-a", "copy-b"]
        );
    }
    assert_eq!(sqlx::query_as::<_, (i64, i64)>("SELECT position_ms,play_count FROM user_item_state WHERE user_id='user' AND item_id='copy-a'")
        .fetch_one(&db).await.unwrap(), (123456, 3));
}

#[tokio::test]
async fn forward_alias_repair_stays_permanent_after_a_copy_is_corrected() {
    let db = duplicate_legacy_movies().await;
    kahawai_hub::library::initialize(&db).await.unwrap();
    assert!(
        kahawai_hub::library::copies(&db, "user", "copy-b")
            .await
            .unwrap()
            .is_empty()
    );
    public_id_migration(&db).await;
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, "copy-b")
            .await
            .unwrap(),
        "copy-a"
    );
    let corrected = db
        .transaction("correct a previously collapsed copy", |c| {
            Box::pin(async move {
                let target = kahawai_hub::library::create(
                    c,
                    kahawai_hub::library::NewItem {
                        kind: "movie".into(),
                        title: "Different movie".into(),
                        year: Some(2001),
                        artist: None,
                        parent_id: None,
                        season: None,
                        episode: None,
                        edition: None,
                    },
                )
                .await?;
                kahawai_hub::library::assign(c, "copy-b", std::slice::from_ref(&target)).await?;
                Ok(target)
            })
        })
        .await
        .unwrap();
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, "copy-b")
            .await
            .unwrap(),
        "copy-a"
    );
    assert_eq!(
        kahawai_hub::library::copies(&db, "user", "copy-b")
            .await
            .unwrap(),
        ["copy-a"]
    );
    assert_eq!(
        kahawai_hub::library::copies(&db, "user", &corrected)
            .await
            .unwrap(),
        ["copy-b"]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT position_ms FROM user_item_state WHERE user_id='user' AND item_id='copy-a'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        123456
    );
}

#[tokio::test]
async fn album_correction_gates_inherited_track_answers_but_keeps_own_choices() {
    use kahawai_hub::{
        library,
        providers::{self, Fields},
    };
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','music','music');
        INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id)
        VALUES('old-album','album','Old album','old album',2000,'Artist','host','music'),
              ('right-album','album','Right album','right album',2010,'Artist','host','music');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id)
        VALUES('track','track','Detected track','detected track','old-album',1,1,'host','music');")
        .execute(&db).await.unwrap();
    providers::store_answer(
        &db,
        "old-album",
        "musicbrainz",
        "old-album-id",
        "auto",
        Fields {
            title: Some("Old album".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    providers::store_answer(
        &db,
        "track",
        "musicbrainz",
        "old-track-id",
        "auto",
        Fields {
            title: Some("Inherited track".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.transaction("correct album", |c| {
        Box::pin(async move { library::assign(c, "old-album", &["right-album".into()]).await })
    })
    .await
    .unwrap();
    assert_eq!(sqlx::query_as::<_, (bool, String)>("SELECT i.metadata_eligible,li.title FROM collection_items i JOIN collection_item_library_items a ON a.collection_item_id=i.id JOIN library_items li ON li.id=a.library_item_id WHERE i.id='track'")
        .fetch_one(&db).await.unwrap(), (false, "Detected track".into()));
    providers::assign_manual(
        &db,
        "track",
        "musicbrainz",
        "chosen-track-id",
        Fields {
            title: Some("Chosen track".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sqlx::query_as::<_, (bool, String)>("SELECT i.metadata_eligible,li.title FROM collection_items i JOIN collection_item_library_items a ON a.collection_item_id=i.id JOIN library_items li ON li.id=a.library_item_id WHERE i.id='track'")
        .fetch_one(&db).await.unwrap(), (true, "Chosen track".into()));
}

#[tokio::test]
async fn reopen_repairs_false_recording_ambiguity_without_touching_other_songs() {
    let dir = tempfile::tempdir().unwrap();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(dir.path().join("hub.db"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    migrator.run_to(82, &pool).await.unwrap();
    // A saved pre-fix catalogue: the newly learned tag produced a separate
    // unidentified work despite an existing global recording. Older identified
    // history, explicit choices and unrelated unmatched music must remain intact.
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','music','music');
        INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');
        INSERT INTO collection_items(id,kind,title,norm_title,sort_title,year,artist,module_id,collection_id)
        VALUES('album-a','album','First','first','first',2000,'Artist','host','music'),
              ('album-b','album','Second','second','second',2000,'Artist','host','music');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id,match_mode,assignment_manual)
        VALUES('known-copy','track','Song','song','album-a',1,1,'host','music','automatic',0),
              ('late-copy','track','Song','song','album-b',1,1,'host','music','unmatched',0),
              ('untouched-copy','track','Other','other','album-b',1,2,'host','music','unmatched',0),
              ('manual-copy','track','Manual','manual','album-b',1,3,'host','music','manual',1);
        INSERT INTO library_items(id,kind,title,norm_title,sort_title,year,artist,match_artist,unidentified,added_id)
        VALUES('album-a','album','First','first','first',2000,'Artist','artist',0,'album-a'),
              ('album-b','album','Second','second','second',2000,'Artist','artist',0,'album-b');
        INSERT INTO library_items(id,kind,title,norm_title,sort_title,recording_id,unidentified,added_id)
        VALUES('known','song','Song','song','song','recording-1',0,'known'),
              ('old-position','song','Song','song','song',NULL,0,'old-position'),
              ('false-unmatched','song','Song','song','song','recording-1',1,'late-copy'),
              ('untouched','song','Other','other','other','unknown-recording',1,'untouched-copy'),
              ('manual','song','Manual','manual','manual','recording-1',1,'manual-copy');
        INSERT INTO collection_item_library_items VALUES('album-a',1,'album-a'),('album-b',1,'album-b'),
            ('known-copy',1,'known'),('late-copy',1,'false-unmatched'),('untouched-copy',1,'untouched'),
            ('manual-copy',1,'manual');
        INSERT INTO album_tracks(id,album_id,song_id,disc_number,track_number)
        VALUES(1,'album-a','known',1,1),(2,'album-b','old-position',1,1),(3,'album-b','false-unmatched',1,1);
        UPDATE collection_items SET album_track_id=1 WHERE id='known-copy';
        UPDATE collection_items SET album_track_id=3 WHERE id='late-copy';
        INSERT INTO files(id,module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
        VALUES(1,'host','music','first.flac',1,1,1,1,1,'{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-1\"}}'),
              (2,'host','music','second.flac',2,1,2,2,2,'{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-1\"}}');
        INSERT INTO playable_sources(id,module_id,collection_id,item_id,family_key,expected_parts)
        VALUES(1,'host','music','known-copy','first',1),(2,'host','music','late-copy','second',1);
        INSERT INTO playable_source_parts VALUES(1,'host','music',1,1),(2,'host','music',1,2);
        INSERT INTO user_item_state(user_id,item_id,position_ms,play_count,updated_at)
        VALUES('user','known',10000,1,100),('user','old-position',20000,2,200),('user','false-unmatched',33333,3,300);
        DELETE FROM library_pending;")
        .execute(&pool).await.unwrap();
    migrator.run_to(83, &pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT collection_item_id FROM library_pending ORDER BY collection_item_id"
        )
        .fetch_all(&pool)
        .await
        .unwrap(),
        ["late-copy"]
    );
    pool.close().await;

    let db = db::open_legacy_fixture(dir.path()).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_, String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='late-copy'")
        .fetch_one(&db).await.unwrap(), "known");
    assert_eq!(
        kahawai_hub::library::resolve_id(&db, "false-unmatched")
            .await
            .unwrap(),
        "known"
    );
    assert_eq!(
        sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT item_id,position_ms,play_count FROM user_item_state ORDER BY item_id"
        )
        .fetch_all(&db)
        .await
        .unwrap(),
        [
            ("known".into(), 33333, 3),
            ("old-position".into(), 20000, 2)
        ]
    );
    for (copy, expected) in [("manual-copy", "manual"), ("untouched-copy", "untouched")] {
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=?")
            .bind(copy).fetch_one(&db).await.unwrap(), expected);
    }
    let revision: i64 =
        sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id='late-copy'")
            .fetch_one(&db)
            .await
            .unwrap();
    db.close().await;
    let db = db::open_legacy_fixture(dir.path()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT assignment_revision FROM collection_items WHERE id='late-copy'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        revision,
        "the repair is not repeated on every startup"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM library_pending")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}
