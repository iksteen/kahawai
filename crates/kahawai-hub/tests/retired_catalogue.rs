//! The catalogue removal must work on upgrades and preserve current hub state.
use sqlx::Connection;

#[tokio::test]
async fn upgrade_drops_catalogue_and_preserves_current_state_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("hub.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let mut connection = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    let migrations = sqlx::migrate!("./migrations");
    sqlx::migrate::Migrator::with_migrations(
        migrations
            .iter()
            .filter(|m| m.version < 89)
            .cloned()
            .collect(),
    )
    .run(&mut connection)
    .await
    .unwrap();
    sqlx::raw_sql("INSERT INTO users(id,username,password_hash) VALUES('u','viewer','hash');
        INSERT INTO libraries(id,name,media_type) VALUES('old-library','Old','movies');
        INSERT INTO library_items(id,kind,title,norm_title,sort_title,added_id) VALUES('old-item','movie','Old','old','old','old');
        INSERT INTO user_item_state(user_id,item_id,position_ms,played,play_count) VALUES('u','old-item',3000,1,4);
        INSERT INTO user_libraries(user_id,library_id) VALUES('u','mediadb-library');
        INSERT INTO catalogue_watch_state(user_id,item_id,parent_id,position_ms,played,updated_at) VALUES('u','new-item','new-item',1234,0,42);
        INSERT INTO settings(key,value) VALUES('fixture','preserve');")
        .execute(&mut connection).await.unwrap();
    connection.close().await.unwrap();
    for _ in 0..2 {
        let db = kahawai_hub::db::open(dir.path()).await.unwrap();
        for statement in include_str!("../migrations/0089_drop_retired_catalogue.sql").lines() {
            if let Some(table) = statement
                .strip_prefix("DROP TABLE IF EXISTS ")
                .and_then(|s| s.strip_suffix(';'))
            {
                let error = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT 1 FROM {table}")))
                    .fetch_all(&db)
                    .await
                    .unwrap_err();
                assert!(
                    error.to_string().contains("no such table"),
                    "{table}: {error}"
                );
            }
        }
        let state: (i64, i64, i64) = sqlx::query_as("SELECT position_ms,played,updated_at FROM catalogue_watch_state WHERE user_id='u' AND item_id='new-item'").fetch_one(&db).await.unwrap();
        assert_eq!(state, (1234, 0, 42));
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT library_id FROM user_libraries WHERE user_id='u'"
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            "mediadb-library"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key='fixture'")
                .fetch_one(&db)
                .await
                .unwrap(),
            "preserve"
        );
        // This is a still-consumed provider cache, not retired catalogue storage.
        sqlx::query("SELECT 1 FROM ed2k_aid")
            .fetch_all(&db)
            .await
            .unwrap();
        db.close().await;
    }
}
