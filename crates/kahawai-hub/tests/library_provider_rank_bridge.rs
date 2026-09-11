//! Provider description order can change while the native parent match stays put.
use kahawai_hub::{
    db,
    providers::{self, Fields},
};

#[tokio::test]
async fn bridge_rank_changes_refresh_title_and_search_without_moving_native_history() {
    let db = db::open_in_memory().await.unwrap();
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','anime','anime');
        INSERT INTO users(id,username,password_hash) VALUES('user','user','unused');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('show','show','Anime','anime',2000,'host','anime');")
        .execute(&db).await.unwrap();
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
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,module_id,collection_id,parent_id,episode) VALUES('episode','episode','Episode 27','episode 27','host','anime','show',27)")
        .execute(&db).await.unwrap();
    for (provider, title) in [("tmdb", "TMDB bridge title"), ("tvdb", "TVDB bridge title")] {
        providers::store_answer(
            &db,
            "episode",
            provider,
            "201",
            "auto",
            Fields {
                title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE provider_metadata SET proj_season=2,proj_episode=1 WHERE item_id='episode'",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,module_id,collection_id,parent_id,episode) VALUES('untouched','episode','Episode 28','episode 28','host','anime','show',28)")
        .execute(&db).await.unwrap();
    let untouched_revision: i64 =
        sqlx::query_scalar("SELECT assignment_revision FROM collection_items WHERE id='untouched'")
            .fetch_one(&db)
            .await
            .unwrap();
    let original: String = sqlx::query_scalar("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='episode'")
        .fetch_one(&db).await.unwrap();
    let parent_match: (String, String, i64) = sqlx::query_as(
        "SELECT provider,provider_id,updated_at FROM item_match WHERE item_id='show'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_item_state(user_id,item_id,position_ms,duration_ms,played,play_count) VALUES('user',?,60000,120000,1,3)")
        .bind(&original).execute(&db).await.unwrap();
    for (order, expected) in [
        (["anime", "tmdb", "tvdb"], "TMDB bridge title"),
        (["anime", "tvdb", "tmdb"], "TVDB bridge title"),
        (["anime", "tmdb", "tvdb"], "TMDB bridge title"),
    ] {
        providers::set_chain(&db, "anime", &order.map(str::to_owned))
            .await
            .unwrap();
        let current_match: (String, String, i64) = sqlx::query_as(
            "SELECT provider,provider_id,updated_at FROM item_match WHERE item_id='show'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            current_match, parent_match,
            "the parent's native provider decision is unchanged"
        );
        let row: (String, String, String, Option<i64>, i64) = sqlx::query_as("SELECT li.id,li.title,li.norm_title,ed.season,ed.episode FROM collection_item_library_items a JOIN library_items li ON li.id=a.library_item_id JOIN episode_details ed ON ed.item_id=li.id WHERE a.collection_item_id='episode'")
            .fetch_one(&db).await.unwrap();
        assert_eq!(
            row,
            (
                original.clone(),
                expected.into(),
                expected.to_lowercase(),
                None,
                27
            )
        );
        let state: (i64, i64, i64) = sqlx::query_as(
            "SELECT position_ms,played,play_count FROM user_item_state WHERE item_id=?",
        )
        .bind(&original)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(state, (60000, 1, 3));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT assignment_revision FROM collection_items WHERE id='untouched'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            untouched_revision,
            "a changed bridge rank need not revisit episodes without its answer"
        );
    }
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE provider_ranks SET rank=rank WHERE media_type='anime'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM library_pending")
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        0
    );
    tx.commit().await.unwrap();
    for (query, expected) in [
        (
            "DELETE FROM provider_ranks WHERE media_type='anime' AND provider='tmdb'",
            "TVDB bridge title",
        ),
        (
            "INSERT INTO provider_ranks(media_type,provider,rank) VALUES('anime','tmdb',1)",
            "TMDB bridge title",
        ),
    ] {
        sqlx::query(query).execute(&db).await.unwrap();
        let row: (String, String, String, Option<i64>, i64) = sqlx::query_as("SELECT li.id,li.title,li.norm_title,ed.season,ed.episode FROM collection_item_library_items a JOIN library_items li ON li.id=a.library_item_id JOIN episode_details ed ON ed.item_id=li.id WHERE a.collection_item_id='episode'")
            .fetch_one(&db).await.unwrap();
        assert_eq!(
            row,
            (
                original.clone(),
                expected.into(),
                expected.to_lowercase(),
                None,
                27
            )
        );
    }
    // The deployed schema is 78: migration 79 must repair a description left
    // stale under its current ranks during that upgrade, before new triggers run.
    sqlx::query("UPDATE library_items SET title='TVDB bridge title',norm_title='tvdb bridge title' WHERE id=?")
        .bind(&original).execute(&db).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0079_episode_bridge_descriptions.sql"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    let queued: Vec<String> = sqlx::query_scalar(
        "SELECT collection_item_id FROM library_pending ORDER BY collection_item_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(queued, vec!["episode"]);
    tx.commit().await.unwrap();
    let repaired: (String, String) =
        sqlx::query_as("SELECT title,norm_title FROM library_items WHERE id=?")
            .bind(&original)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        repaired,
        ("TMDB bridge title".into(), "tmdb bridge title".into())
    );
    let state: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT item_id,position_ms,played,play_count FROM user_item_state WHERE item_id=?",
    )
    .bind(&original)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(state, (original, 60000, 1, 3));
}
