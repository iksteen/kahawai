use kahawai_hub::{
    db,
    library::Database,
    providers::{self, Fields},
};

async fn fixture() -> Database {
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
        INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');")
        .execute(&db).await.unwrap();
    db
}
async fn copy(
    db: &Database,
    id: &str,
    kind: &str,
    title: &str,
    year: Option<i64>,
    parent: Option<&str>,
    position: Option<i64>,
) {
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id,parent_id,season,episode)
        VALUES(?,?,?,?,?,'host','one',?,1,?)")
        .bind(id).bind(kind).bind(title).bind(title.to_lowercase()).bind(year).bind(parent).bind(position).execute(db).await.unwrap();
}
async fn target(db: &Database, id: &str) -> String {
    sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal LIMIT 1").bind(id).fetch_one(db).await.unwrap()
}
async fn assign(db: &Database, id: &str, year: &str) {
    providers::assign_manual(
        db,
        id,
        "tmdb",
        year,
        Fields {
            title: Some("X-Men".into()),
            premiered: Some(format!("{year}-01-01")),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn anime_bridge_describes_native_episode_without_moving_identity_or_history() {
    let db = fixture().await;
    sqlx::query("UPDATE collections SET media_type='anime' WHERE collection_id='one'")
        .execute(&db)
        .await
        .unwrap();
    copy(&db, "show", "show", "Anime", Some(2000), None, None).await;
    providers::store_answer(
        &db,
        "show",
        "anilist",
        "1",
        "auto",
        Fields {
            title: Some("Anime".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    copy(
        &db,
        "episode",
        "episode",
        "Episode 27",
        None,
        Some("show"),
        Some(27),
    )
    .await;
    sqlx::query("UPDATE collection_items SET season=NULL WHERE id='episode'")
        .execute(&db)
        .await
        .unwrap();
    let original = target(&db, "episode").await;
    sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,played,play_count) VALUES('user',?,60000,120000,1,3)")
        .bind(&original).execute(&db).await.unwrap();
    providers::store_answer(
        &db,
        "episode",
        "tvdb",
        "201",
        "auto",
        Fields {
            title: Some("The bridge title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE provider_metadata SET proj_season=2,proj_episode=1 WHERE item_id='episode' AND provider='tvdb'").execute(&db).await.unwrap();
    assert_eq!(target(&db, "episode").await, original);
    let row: (String,Option<i64>,Option<i64>) = sqlx::query_as("SELECT li.title,ed.season,ed.episode FROM library_items li JOIN episode_details ed ON ed.item_id=li.id WHERE li.id=?")
        .bind(&original).fetch_one(&db).await.unwrap();
    assert_eq!(row, ("The bridge title".into(), None, Some(27)));
    let state: (i64, i64, i64) =
        sqlx::query_as("SELECT position_ms,played,play_count FROM user_item_state WHERE item_id=?")
            .bind(&original)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(state, (60000, 1, 3));
    providers::assign_manual(
        &db,
        "show",
        "tvdb",
        "10",
        Fields {
            title: Some("Anime".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        target(&db, "episode").await,
        original,
        "choosing the bridge provider for the parent must not move episode history"
    );
    providers::store_answer(
        &db,
        "episode",
        "tmdb",
        "999",
        "weak",
        Fields {
            title: Some("Unchosen weak title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT title FROM library_items WHERE id=?")
            .bind(&original)
            .fetch_one(&db)
            .await
            .unwrap(),
        "The bridge title"
    );
}

#[tokio::test]
async fn collection_reannouncement_keeps_assignments_until_media_type_changes() {
    use kahawai_hub::registry::Registry;
    let db = fixture().await;
    copy(&db, "movie", "movie", "X-Men", Some(2000), None, None).await;
    let registry = Registry::new(
        db.clone(),
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    );
    let revision: i64 =
        sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id='movie'")
            .fetch_one(&db)
            .await
            .unwrap();

    for root in ["/first-root", "/changed-root"] {
        registry
            .announce_collection("host", "one", "movies", &[root.into()])
            .await
            .unwrap();
        let after: i64 =
            sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id='movie'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(after, revision, "reconnect must not rematch the collection");
    }
    let root: String =
        sqlx::query_scalar("SELECT normalized_path FROM collection_roots WHERE configured=1")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(root, "/changed-root");

    registry
        .announce_collection("host", "one", "anime", &[root])
        .await
        .unwrap();
    let anime: bool = sqlx::query_scalar("SELECT anime FROM library_items WHERE id=?")
        .bind(target(&db, "movie").await)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(
        anime,
        "a changed media type must still update library membership"
    );
}

#[tokio::test]
async fn assigned_identity_merges_detected_copy_and_correction_is_reversible() {
    let db = fixture().await;
    copy(&db, "bare", "movie", "X-Men", None, None, None).await;
    copy(&db, "dated", "movie", "X-Men", Some(2000), None, None).await;
    sqlx::query("UPDATE collection_items SET collection_id='two' WHERE id='dated'")
        .execute(&db)
        .await
        .unwrap();
    assert_ne!(target(&db, "bare").await, target(&db, "dated").await);
    sqlx::query("INSERT INTO state_imports(user_id,item_id,position_ms,play_count) VALUES('user','bare',1234,2)").execute(&db).await.unwrap();
    assign(&db, "bare", "2000").await;
    let movie = target(&db, "dated").await;
    assert_eq!(target(&db, "bare").await, movie);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT play_count FROM user_item_state WHERE item_id=?")
            .bind(&movie)
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
    assign(&db, "bare", "1993").await;
    assert_ne!(target(&db, "bare").await, movie);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_item_state WHERE item_id=?")
            .bind(&movie)
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    assign(&db, "bare", "2000").await;
    assert_eq!(target(&db, "bare").await, movie);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM state_imports")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn episode_coverage_is_separate_from_physical_parts() {
    let db = fixture().await;
    copy(&db, "series", "show", "A Series", Some(2001), None, None).await;
    copy(
        &db,
        "double",
        "episode",
        "Episodes 1 and 2",
        None,
        Some("series"),
        Some(1),
    )
    .await;
    sqlx::query("UPDATE collection_items SET episode_end=2 WHERE id='double'")
        .execute(&db)
        .await
        .unwrap();
    copy(
        &db,
        "single",
        "episode",
        "Episode 2",
        None,
        Some("series"),
        Some(2),
    )
    .await;
    let members:Vec<String>=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='double' ORDER BY ordinal").fetch_all(&db).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[1], target(&db, "single").await);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT kind FROM library_items WHERE id='series'")
            .fetch_one(&db)
            .await
            .unwrap(),
        "series"
    );
}

#[tokio::test]
async fn failed_source_transaction_does_not_publish_assignment() {
    let db = fixture().await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,module_id,collection_id) VALUES('rollback','movie','Gone','gone','host','one')").execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM library_items")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn multipart_movie_keeps_files_and_session_identity_when_one_copy_moves() {
    let db = fixture().await;
    copy(&db, "split", "movie", "X-Men", None, None, None).await;
    copy(&db, "single", "movie", "X-Men", Some(2000), None, None).await;
    sqlx::raw_sql("INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
        VALUES('host','one','CD1.avi',100,1,2,3,4,'{}'),('host','one','CD2.avi',200,1,3,4,5,'{}');
        INSERT INTO playable_sources(module_id,collection_id,item_id,family_key,expected_parts) VALUES('host','one','split','split',2);
        INSERT INTO playable_source_parts SELECT ps.id,'host','one',CASE f.path_rel WHEN 'CD1.avi' THEN 1 ELSE 2 END,f.id FROM files f CROSS JOIN playable_sources ps WHERE ps.item_id='split';")
        .execute(&db).await.unwrap();
    let files: Vec<(i64, String)> = sqlx::query_as("SELECT id,path_rel FROM files ORDER BY id")
        .fetch_all(&db)
        .await
        .unwrap();
    assign(&db, "split", "2000").await;
    let snapshot = kahawai_hub::library::playback_snapshot(&db, "single", files[0].0)
        .await
        .unwrap();
    assert_eq!(snapshot.library_item_ids, vec!["single"]);
    assign(&db, "split", "1993").await;
    assert_eq!(snapshot.library_item_ids, vec!["single"]);
    assert_ne!(target(&db, "split").await, "single");
    assert_eq!(
        sqlx::query_as::<_, (i64, String)>("SELECT id,path_rel FROM files ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap(),
        files
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playable_source_parts")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn recording_evidence_joins_albums_but_equal_song_names_do_not() {
    let db = fixture().await;
    for album in ["First", "Second"] {
        copy(&db, album, "album", album, Some(2000), None, None).await;
        sqlx::query("UPDATE collection_items SET artist='Artist' WHERE id=?")
            .bind(album)
            .execute(&db)
            .await
            .unwrap();
    }
    // Scanner inputs land in one transaction, including the embedded recording ID.
    let mut tx = db.begin().await.unwrap();
    for (song, album) in [("one", "First"), ("two", "Second")] {
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES(?,'track','Song','song',?,1,1,'host','one')").bind(song).bind(album).execute(&mut *tx).await.unwrap();
        let file:i64=sqlx::query_scalar("INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json) VALUES('host','one',?,1,1,1,1,1,'{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-1\"}}') RETURNING id").bind(song).fetch_one(&mut *tx).await.unwrap();
        let source:i64=sqlx::query_scalar("INSERT INTO playable_sources(module_id,collection_id,item_id,family_key,expected_parts) VALUES('host','one',?,?,1) RETURNING id").bind(song).bind(song).fetch_one(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO playable_source_parts VALUES(?,'host','one',1,?)")
            .bind(source)
            .bind(file)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(target(&db, "one").await, target(&db, "two").await);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM album_copies")
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );
    sqlx::query("UPDATE files SET streams_json='{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-2\"}}' WHERE path_rel='two'").execute(&db).await.unwrap();
    assert_ne!(
        target(&db, "one").await,
        target(&db, "two").await,
        "same album slot cannot contradict a recording ID"
    );
    let association: String =
        sqlx::query_scalar("SELECT song_id FROM album_copies WHERE collection_item_id='two'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        association,
        target(&db, "two").await,
        "correcting a recording removes the previous album association"
    );
    copy(&db, "third", "track", "Song", None, Some("Second"), Some(2)).await;
    assert_ne!(target(&db, "one").await, target(&db, "third").await);
}

async fn song_waiting_for_recording_id() -> Database {
    let db = fixture().await;
    for album in ["First", "Second"] {
        copy(&db, album, "album", album, Some(2000), None, None).await;
        sqlx::query("UPDATE collection_items SET artist='Artist' WHERE id=?")
            .bind(album)
            .execute(&db)
            .await
            .unwrap();
    }
    let mut tx = db.begin().await.unwrap();
    for (song, album, tags) in [
        (
            "one",
            "First",
            r#"{"tags":{"MUSICBRAINZ_TRACKID":"recording-1"}}"#,
        ),
        ("two", "Second", "{}"),
    ] {
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES(?,'track','Song','song',?,1,1,'host','one')")
            .bind(song).bind(album).execute(&mut *tx).await.unwrap();
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json) VALUES('host','one',?,1,1,1,1,1,?) RETURNING id")
            .bind(song).bind(tags).fetch_one(&mut *tx).await.unwrap();
        let source: i64 = sqlx::query_scalar("INSERT INTO playable_sources(module_id,collection_id,item_id,family_key,expected_parts) VALUES('host','one',?,?,1) RETURNING id")
            .bind(song).bind(song).fetch_one(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO playable_source_parts VALUES(?,'host','one',1,?)")
            .bind(source)
            .bind(file)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    db
}

#[tokio::test]
async fn learning_recording_id_joins_the_known_song_without_moving_established_history() {
    let db = song_waiting_for_recording_id().await;
    assert_eq!(target(&db, "one").await, "one");
    assert_eq!(target(&db, "two").await, "two");
    sqlx::raw_sql("INSERT INTO user_item_state(user_id,item_id,position_ms,played,play_count,updated_at)
        VALUES('user','one',10000,1,3,100),('user','two',20000,0,1,200);
        INSERT INTO library_overrides(library_item_id,fields) VALUES('two','{\"overview\":\"Description of the previously identified work\"}');")
        .execute(&db).await.unwrap();
    // The newly learned ID outranks the old untagged album position, including
    // subsequent reconciliations after that historical position still exists.
    for _ in 0..2 {
        sqlx::query("UPDATE files SET streams_json='{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-1\"}}' WHERE path_rel='two'")
            .execute(&db).await.unwrap();
        assert_eq!(target(&db, "two").await, "one");
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT match_mode FROM collection_items WHERE id='two'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            "automatic"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT song_id FROM album_copies WHERE collection_item_id='two'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            "one"
        );
    }
    assert_eq!(
        sqlx::query_as::<_, (String, i64, i64, i64)>(
            "SELECT item_id,position_ms,played,play_count FROM user_item_state ORDER BY item_id"
        )
        .fetch_all(&db)
        .await
        .unwrap(),
        [("one".into(), 10000, 1, 3), ("two".into(), 20000, 0, 1)]
    );
    assert_eq!(
        sqlx::query_as::<_, (bool, Option<String>, Option<String>)>(
            "SELECT unidentified,merged_into,recording_id FROM library_items WHERE id='two'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        (false, None, None)
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM library_overrides WHERE library_item_id='two')"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM library_overrides WHERE library_item_id='one')"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn recording_match_does_not_bypass_conflicts_or_rejections() {
    for scenario in ["sources", "candidates", "rejected"] {
        let db = song_waiting_for_recording_id().await;
        let mut tx = db.begin().await.unwrap();
        match scenario {
            "sources" => {
                let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json) VALUES('host','one','alternate',2,1,2,2,2,'{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-2\"}}') RETURNING id")
                    .fetch_one(&mut *tx).await.unwrap();
                let source: i64 = sqlx::query_scalar("INSERT INTO playable_sources(module_id,collection_id,item_id,family_key,expected_parts) VALUES('host','one','two','alternate',1) RETURNING id")
                    .fetch_one(&mut *tx).await.unwrap();
                sqlx::query("INSERT INTO playable_source_parts VALUES(?,'host','one',1,?)")
                    .bind(source)
                    .bind(file)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            "candidates" => {
                sqlx::query("INSERT INTO library_items(id,kind,title,norm_title,sort_title,recording_id,unidentified,added_id) VALUES('another','song','Another','another','another','recording-1',0,'another')")
                    .execute(&mut *tx).await.unwrap();
            }
            _ => {
                sqlx::query("INSERT INTO rejected_library_matches VALUES('two','one')")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
        }
        sqlx::query("UPDATE files SET streams_json='{\"tags\":{\"MUSICBRAINZ_TRACKID\":\"recording-1\"}}' WHERE path_rel='two'")
            .execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let selected = target(&db, "two").await;
        assert_ne!(selected, "one", "{scenario}");
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT unidentified FROM library_items WHERE id=?")
                .bind(&selected)
                .fetch_one(&db)
                .await
                .unwrap(),
            "{scenario}"
        );
    }
}

#[tokio::test]
async fn parent_correction_recomputes_children_and_exposes_explicit_pin_conflict() {
    let db = fixture().await;
    copy(&db, "parent", "show", "Original", Some(2000), None, None).await;
    copy(
        &db,
        "auto",
        "episode",
        "First",
        None,
        Some("parent"),
        Some(1),
    )
    .await;
    copy(
        &db,
        "pinned",
        "episode",
        "Second",
        None,
        Some("parent"),
        Some(2),
    )
    .await;
    let original = target(&db, "pinned").await;
    let chosen = original.clone();
    db.transaction("choose episode", |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "pinned", &[chosen]).await })
    })
    .await
    .unwrap();
    providers::assign_manual(
        &db,
        "parent",
        "tmdb",
        "123",
        Fields {
            title: Some("Different".into()),
            premiered: Some("2010-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(target(&db, "pinned").await, original);
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT match_conflict FROM collection_items WHERE id='pinned'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
        .is_some()
    );
    let parent: String =
        sqlx::query_scalar("SELECT series_id FROM episode_details WHERE item_id=?")
            .bind(target(&db, "auto").await)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(parent, target(&db, "parent").await);
}

#[tokio::test]
async fn unidentified_promotion_redirects_other_pins_and_captured_history_ids() {
    let db = fixture().await;
    copy(&db, "bare", "movie", "X-Men", None, None, None).await;
    copy(&db, "other", "movie", "Unknown", None, None, None).await;
    copy(&db, "dated", "movie", "X-Men", Some(2000), None, None).await;
    db.transaction("choose unidentified item", |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "other", &["bare".into()]).await })
    })
    .await
    .unwrap();
    assign(&db, "bare", "2000").await;
    assert_eq!(target(&db, "other").await, "dated");
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        kahawai_hub::library::canonical_id(&mut tx, "bare")
            .await
            .unwrap(),
        "dated"
    );
    tx.rollback().await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM collection_item_library_items a JOIN library_items c ON c.id=a.library_item_id WHERE c.merged_into IS NOT NULL").fetch_one(&db).await.unwrap(),0);
    assign(&db, "bare", "1993").await;
    assert_eq!(
        target(&db, "other").await,
        "dated",
        "a later correction leaves the other pin alone"
    );
}

#[tokio::test]
async fn projected_span_keeps_its_original_numbering_and_length() {
    let db = fixture().await;
    copy(&db, "parent", "show", "Series", Some(2000), None, None).await;
    copy(
        &db,
        "span",
        "episode",
        "Episodes 25-26",
        None,
        Some("parent"),
        Some(25),
    )
    .await;
    sqlx::query("UPDATE collection_items SET season=NULL,episode_end=26 WHERE id='span'")
        .execute(&db)
        .await
        .unwrap();
    providers::assign_manual(
        &db,
        "parent",
        "tmdb",
        "series",
        Fields {
            title: Some("Series".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    providers::store_answer(
        &db,
        "span",
        "tmdb",
        "episode",
        "strong",
        Fields {
            title: Some("First".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE provider_metadata SET proj_season=2,proj_episode=1 WHERE item_id='span' AND provider='tmdb'").execute(&db).await.unwrap();
    let positions:Vec<(String,i64)>=sqlx::query_as("SELECT n.numbering,n.episode FROM collection_item_library_items a JOIN episode_details n ON n.item_id=a.library_item_id WHERE a.collection_item_id='span' ORDER BY a.ordinal").fetch_all(&db).await.unwrap();
    assert_eq!(
        positions,
        vec![("absolute".into(), 25), ("absolute".into(), 26)]
    );
}

#[tokio::test]
async fn compatible_child_enrichment_updates_description_without_changing_identity() {
    let db = fixture().await;
    copy(&db, "parent", "show", "Series", Some(2000), None, None).await;
    copy(
        &db,
        "child",
        "episode",
        "Episode 1",
        None,
        Some("parent"),
        Some(1),
    )
    .await;
    let before = target(&db, "child").await;
    providers::assign_manual(
        &db,
        "parent",
        "tmdb",
        "series",
        Fields {
            title: Some("Series".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    providers::store_answer(
        &db,
        "child",
        "tmdb",
        "episode",
        "strong",
        Fields {
            title: Some("The Beginning".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(target(&db, "child").await, before);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT title FROM library_items WHERE id=?")
            .bind(before)
            .fetch_one(&db)
            .await
            .unwrap(),
        "The Beginning"
    );
}

#[tokio::test]
async fn scanner_keeps_single_episode_and_combined_copy_separate() {
    use kahawai_hub::registry::{FileUpsertRecord, Registry};
    let db = fixture().await;
    let registry = Registry::new(
        db.clone(),
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    );
    let root = "/library-test";
    registry
        .announce_collection("host", "episodes", "series", &[root.into()])
        .await
        .unwrap();
    let records = ["Show (2001).S01E01.mkv", "Show (2001).S01E01-E02.mkv"]
        .into_iter()
        .enumerate()
        .map(|(n, path)| FileUpsertRecord {
            root_token: kahawai_core::media::root_token(std::path::Path::new(root)),
            path_rel: path.into(),
            size: 100 + n as u64,
            mtime_unix: 1,
            head_xxh3: n as u64,
            tail_xxh3: 0,
            oshash: 0,
            streams_json: "{}".into(),
        })
        .collect();
    registry
        .upsert_files("host", "episodes", records)
        .await
        .unwrap();
    let rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT f.path_rel,ci.id,COUNT(a.library_item_id) FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN collection_items ci ON ci.id=fb.item_id JOIN collection_item_library_items a ON a.collection_item_id=ci.id GROUP BY f.id ORDER BY f.path_rel").fetch_all(&db).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(
        rows[0].1, rows[1].1,
        "different episode coverage needs its own collection item"
    );
    assert_eq!(rows.iter().find(|r| r.0.ends_with("E01.mkv")).unwrap().2, 1);
    assert_eq!(rows.iter().find(|r| r.0.ends_with("E02.mkv")).unwrap().2, 2);
    // Simulate the old scanner's shared row, then reopen the library without a
    // mediahost or rescan. Startup must repair the stored physical memberships.
    db.write("legacy coverage fixture", |c| Box::pin(async move {
        sqlx::query("UPDATE playable_sources SET item_id=(SELECT id FROM collection_items WHERE kind='episode' AND episode_end=2 LIMIT 1) WHERE collection_id='episodes'").execute(&mut *c).await?;
        sqlx::query("INSERT INTO library_pending SELECT id FROM collection_items WHERE kind='episode' ON CONFLICT DO NOTHING").execute(&mut *c).await?;
        Ok(())
    })).await.unwrap();
    db.write("legacy subtitles fixture", |c| Box::pin(async move { sqlx::raw_sql("INSERT INTO subtitle_tracks(id,item_id,origin,format,language,created_by) SELECT 100,id,'downloaded','srt','en','user' FROM collection_items WHERE kind='episode' AND episode_end=2 LIMIT 1;
        INSERT INTO subtitle_tracks(id,item_id,origin,format,derived_from,payload_id) SELECT 101,item_id,'raster','pgs',100,201 FROM subtitle_tracks WHERE id=100; INSERT INTO rejected_library_matches SELECT collection_item_id,library_item_id FROM collection_item_library_items WHERE collection_item_id=(SELECT item_id FROM subtitle_tracks WHERE id=100) AND ordinal=1").execute(&mut *c).await?; Ok(()) })).await.unwrap();
    kahawai_hub::library::initialize(&db).await.unwrap();
    let preserved:i64=sqlx::query_scalar("SELECT COUNT(*) FROM playable_sources ps JOIN subtitle_tracks t ON t.item_id=ps.item_id AND t.origin='downloaded' JOIN subtitle_tracks d ON d.derived_from=t.id AND d.item_id=t.item_id WHERE COALESCE(t.payload_id,t.id)=100 AND d.payload_id=201").fetch_one(&db).await.unwrap();
    assert_eq!(
        preserved, 2,
        "both repaired copies retain the download, derivative and immutable payloads"
    );
    let retained:i64=sqlx::query_scalar("SELECT COUNT(*) FROM subtitle_tracks t JOIN rejected_library_matches r ON r.collection_item_id=t.item_id WHERE t.origin='downloaded' AND COALESCE(t.payload_id,t.id)=100").fetch_one(&db).await.unwrap();
    assert_eq!(
        retained, 2,
        "rejected library identities follow each repaired copy"
    );
    let coverage:Vec<(String,i64)>=sqlx::query_as("SELECT f.path_rel,COUNT(a.library_item_id) FROM files f JOIN file_bindings fb ON fb.file_id=f.id JOIN collection_item_library_items a ON a.collection_item_id=fb.item_id GROUP BY f.id ORDER BY f.path_rel").fetch_all(&db).await.unwrap();
    assert_eq!(
        coverage
            .iter()
            .find(|r| r.0.ends_with("E01.mkv"))
            .unwrap()
            .1,
        1
    );
    assert_eq!(
        coverage
            .iter()
            .find(|r| r.0.ends_with("E02.mkv"))
            .unwrap()
            .1,
        2
    );
}

#[tokio::test]
async fn a_combined_copys_first_episode_answer_does_not_rename_the_second() {
    let db = fixture().await;
    copy(&db, "show", "show", "Series", Some(2000), None, None).await;
    copy(
        &db,
        "e2",
        "episode",
        "The Second",
        None,
        Some("show"),
        Some(2),
    )
    .await;
    copy(
        &db,
        "span",
        "episode",
        "Episodes 1-2",
        None,
        Some("show"),
        Some(1),
    )
    .await;
    sqlx::query("UPDATE collection_items SET episode_end=2 WHERE id='span'")
        .execute(&db)
        .await
        .unwrap();
    providers::assign_manual(
        &db,
        "span",
        "tmdb",
        "first",
        Fields {
            title: Some("Pilot".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let second = target(&db, "e2").await;
    let title: String = sqlx::query_scalar("SELECT title FROM library_items WHERE id=?")
        .bind(second)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "The Second");
    let ids:Vec<String>=sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='span' ORDER BY ordinal").fetch_all(&db).await.unwrap();
    db.transaction("confirm combined assignment", move |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "span", &ids).await })
    })
    .await
    .unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='span'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        "confirming a span preserves its first episode description"
    );
}

#[tokio::test]
async fn song_correction_returns_to_its_existing_album_position_and_history() {
    let db = fixture().await;
    sqlx::raw_sql("INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES('a','album','A','a',2000,'Artist','host','one'),('b','album','B','b',2000,'Artist','host','one')").execute(&db).await.unwrap();
    copy(
        &db,
        "song",
        "track",
        "Original title",
        None,
        Some("a"),
        Some(1),
    )
    .await;
    let original = target(&db, "song").await;
    sqlx::query("INSERT INTO user_item_state(user_id,item_id,play_count) VALUES('user',?,3)")
        .bind(&original)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE collection_items SET parent_id='b' WHERE id='song'")
        .execute(&db)
        .await
        .unwrap();
    assert_ne!(target(&db, "song").await, original);
    sqlx::query("UPDATE collection_items SET parent_id='a',title='Correct title' WHERE id='song'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(target(&db, "song").await, original);
    let state:(String,i64)=sqlx::query_as("SELECT i.title,w.play_count FROM library_items i JOIN user_item_state w ON w.item_id=i.id WHERE i.id=?").bind(&original).fetch_one(&db).await.unwrap();
    assert_eq!(state, ("Correct title".into(), 3));
}

#[tokio::test]
async fn first_identification_preserves_custom_descriptions_and_existing_target_choices() {
    let db = fixture().await;
    copy(&db, "bare", "movie", "X-Men", None, None, None).await;
    copy(&db, "dated", "movie", "X-Men", Some(2000), None, None).await;
    sqlx::raw_sql("INSERT INTO library_overrides VALUES('bare','{\"overview\":\"My description\",\"rating\":5}'),('dated','{\"rating\":8}')").execute(&db).await.unwrap();
    assign(&db, "bare", "2000").await;
    let fields: String =
        sqlx::query_scalar("SELECT fields FROM library_overrides WHERE library_item_id='dated'")
            .fetch_one(&db)
            .await
            .unwrap();
    let fields: serde_json::Value = serde_json::from_str(&fields).unwrap();
    assert_eq!(fields["overview"], "My description");
    assert_eq!(fields["rating"].as_f64(), Some(8.0));
}

#[tokio::test]
async fn assigning_an_album_to_another_artist_does_not_donate_the_old_metadata() {
    let db = fixture().await;
    sqlx::raw_sql("INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES('a','album','Greatest Hits','greatest hits',2000,'Artist A','host','one'),('b','album','Greatest Hits','greatest hits',2000,'Artist B','host','one')").execute(&db).await.unwrap();
    db.transaction("correct album", |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "a", &["b".into()]).await })
    })
    .await
    .unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT metadata_eligible FROM collection_items WHERE id='a'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn missing_disc_means_disc_one_and_provider_song_title_survives_poor_copy_tags() {
    let db = fixture().await;
    sqlx::raw_sql("INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) VALUES('album','album','Album','album',2000,'Artist','host','one'),('album2','album','Album','album',2000,'Artist','host','two');
        INSERT INTO collection_items(id,kind,title,norm_title,parent_id,season,episode,module_id,collection_id) VALUES('a','track','Song','song','album',NULL,1,'host','one'),('b','track','Track 01','track 01','album2',1,1,'host','two')").execute(&db).await.unwrap();
    assert_eq!(target(&db, "a").await, target(&db, "b").await);
    let disc: i64 =
        sqlx::query_scalar("SELECT disc_number FROM album_copies WHERE collection_item_id='a'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(disc, 1);
    providers::assign_manual(
        &db,
        "a",
        "musicbrainz",
        "recording",
        Fields {
            title: Some("Provider title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE collection_items SET title='Poor new tag' WHERE id='b'")
        .execute(&db)
        .await
        .unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM library_items WHERE id=?")
        .bind(target(&db, "a").await)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "Provider title");
}

#[tokio::test]
async fn deleting_one_migrated_subtitle_preserves_the_other_copys_payload() {
    let db = fixture().await;
    copy(&db, "a", "movie", "A", Some(2000), None, None).await;
    copy(&db, "b", "movie", "B", Some(2000), None, None).await;
    sqlx::raw_sql("INSERT INTO subtitle_tracks(id,item_id,origin,format,created_by,payload_id) VALUES(100,'a','downloaded','srt','user',NULL),(101,'b','downloaded','srt','user',100)").execute(&db).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("downloaded-100.json");
    std::fs::write(&payload, "{}").unwrap();
    let subs = kahawai_hub::subtitles::Subtitles::new(dir.path().into());
    let registry = kahawai_hub::registry::Registry::new(
        db,
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    );
    assert!(
        subs.delete_track(&registry, 100, "user", false)
            .await
            .unwrap()
    );
    assert!(payload.exists());
    assert_eq!(subs.clean_orphaned_payloads(&registry).await.unwrap(), 0);
    assert!(
        subs.delete_track(&registry, 101, "user", false)
            .await
            .unwrap()
    );
    assert!(!payload.exists());
}

#[tokio::test]
async fn identifying_a_parent_keeps_manually_assigned_children_under_the_same_series() {
    let db = fixture().await;
    copy(&db, "unknown", "show", "Series", None, None, None).await;
    copy(&db, "known", "show", "Series", Some(2000), None, None).await;
    copy(
        &db,
        "episode",
        "episode",
        "Pilot",
        None,
        Some("unknown"),
        Some(1),
    )
    .await;
    let episode = target(&db, "episode").await;
    let chosen = episode.clone();
    db.transaction("confirm episode", move |c| {
        Box::pin(async move { kahawai_hub::library::assign(c, "episode", &[chosen]).await })
    })
    .await
    .unwrap();
    providers::assign_manual(
        &db,
        "unknown",
        "tmdb",
        "series",
        Fields {
            title: Some("Series".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let parent: String =
        sqlx::query_scalar("SELECT series_id FROM episode_details WHERE item_id=?")
            .bind(&episode)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(parent, target(&db, "known").await);
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT match_conflict FROM collection_items WHERE id='episode'"
        )
        .fetch_one(&db)
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn rescanning_corrected_song_tags_refreshes_the_library_title() {
    use kahawai_hub::registry::{FileUpsertRecord, Registry};
    let db = fixture().await;
    let registry = Registry::new(
        db.clone(),
        Default::default(),
        kahawai_mediadb::Store::in_memory().await.unwrap(),
    );
    let root = "/library-tag-test";
    registry
        .announce_collection("host", "music", "music", &[root.into()])
        .await
        .unwrap();
    let make = |title: &str, mtime| {
        FileUpsertRecord{
        root_token:kahawai_core::media::root_token(std::path::Path::new(root)),path_rel:"01.flac".into(),size:100,mtime_unix:mtime,head_xxh3:1,tail_xxh3:2,oshash:0,
        streams_json:serde_json::json!({"container":"flac","duration_ms":1000,"tags":{"title":title,"album":"Album","album_artist":"Artist","artist":"Artist","track_number":"1"}}).to_string(),
    }
    };
    registry
        .upsert_files("host", "music", vec![make("Track 01", 1)])
        .await
        .unwrap();
    let copy: String = sqlx::query_scalar("SELECT id FROM collection_items WHERE kind='track'")
        .fetch_one(&db)
        .await
        .unwrap();
    let original = target(&db, &copy).await;
    registry
        .upsert_files("host", "music", vec![make("Correct title", 2)])
        .await
        .unwrap();
    assert_eq!(target(&db, &copy).await, original);
    let title: String = sqlx::query_scalar("SELECT title FROM library_items WHERE id=?")
        .bind(original)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "Correct title");
}

#[tokio::test]
async fn overlapping_episode_assignments_preserve_retained_items_and_history() {
    for change in ["reorder", "replace", "shrink"] {
        let db = fixture().await;
        copy(&db, "parent", "show", "Unknown series", None, None, None).await;
        copy(
            &db,
            "span",
            "episode",
            "Episode 1",
            None,
            Some("parent"),
            Some(1),
        )
        .await;
        sqlx::query("UPDATE collection_items SET episode_end=2 WHERE id='span'")
            .execute(&db)
            .await
            .unwrap();
        copy(
            &db,
            "third",
            "episode",
            "Episode 3",
            None,
            Some("parent"),
            Some(3),
        )
        .await;
        let old: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='span' ORDER BY ordinal")
            .fetch_all(&db).await.unwrap();
        assert_eq!(old.len(), 2);
        let third = target(&db, "third").await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM library_items WHERE kind='episode' AND unidentified=1"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            3
        );
        for (n, id) in old.iter().enumerate() {
            sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,play_count) VALUES('user',?,?,?)")
                .bind(id).bind((n as i64 + 1) * 111).bind(n as i64 + 1).execute(&db).await.unwrap();
        }
        let next = match change {
            "reorder" => vec![old[1].clone(), old[0].clone()],
            "replace" => vec![old[1].clone(), third.clone()],
            _ => vec![old[1].clone()],
        };
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::library::assign(&mut tx, "span", &next)
            .await
            .unwrap();
        // Inspect before commit: the old implementation creates the cycle here,
        // and only then hangs in the before-commit reconciliation hook.
        let actual: Vec<String> = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='span' ORDER BY ordinal")
            .fetch_all(&mut *tx).await.unwrap();
        assert_eq!(actual, next, "{change}: no retained target can collapse");
        for id in &next {
            let alias: Option<String> =
                sqlx::query_scalar("SELECT merged_into FROM library_items WHERE id=?")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
            assert!(
                alias.is_none(),
                "{change}: retained/new target {id} became an alias"
            );
        }
        tx.commit().await.unwrap();
        let state: (i64, i64) =
            sqlx::query_as("SELECT position_ms,play_count FROM user_item_state WHERE item_id=?")
                .bind(&old[1])
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            state,
            (222, 2),
            "{change}: retained episode keeps its own history"
        );
        let first_target = if change == "replace" { &third } else { &old[0] };
        let state: (i64, i64) =
            sqlx::query_as("SELECT position_ms,play_count FROM user_item_state WHERE item_id=?")
                .bind(first_target)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(state, (111, 1));
        let mut tx = db.begin().await.unwrap();
        assert_eq!(
            kahawai_hub::library::canonical_id(&mut tx, &old[0])
                .await
                .unwrap(),
            *first_target
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_alias_cycles_return_an_error_without_blocking_the_writer() {
    let db = fixture().await;
    copy(&db, "a", "movie", "Unknown a", None, None, None).await;
    copy(&db, "b", "movie", "Unknown b", None, None, None).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE library_items SET merged_into=CASE id WHEN 'a' THEN 'b' ELSE 'a' END WHERE id IN ('a','b')")
        .execute(&mut *tx).await.unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        kahawai_hub::library::canonical_id(&mut tx, "a"),
    )
    .await;
    assert!(result.expect("alias traversal must terminate").is_err());
    tx.rollback().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        kahawai_hub::library::canonical_id(&mut tx, "a")
            .await
            .unwrap(),
        "a"
    );
    tx.commit().await.unwrap();
}
