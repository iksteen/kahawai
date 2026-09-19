use kahawai_hub::{
    db,
    watch::{self, Progress},
};

#[tokio::test]
async fn progress_preserves_zero_reports_and_track_completion_without_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = db::open(dir.path()).await.unwrap();
    sqlx::query("INSERT INTO users(id,username,password_hash) VALUES('user','user','unused')")
        .execute(&db)
        .await
        .unwrap();
    for (id, track) in [("movie", false), ("child1:album:t:1:1", true)] {
        let ids = vec![id.to_string()];
        let report = |position| Progress {
            id: id.into(),
            parent: if track { "album" } else { id }.into(),
            position,
            duration: Some(10000),
            track,
        };
        watch::progress(&db, "user", &[report(5000)]).await.unwrap();
        let state = watch::read(&db, "user", &ids)
            .await
            .unwrap()
            .remove(id)
            .unwrap();
        assert!(!state.played);
        assert_eq!(
            state.resume_position_ms,
            if track { None } else { Some(5000) }
        );
        watch::progress(&db, "user", &[report(9000)]).await.unwrap();
        sqlx::query("UPDATE catalogue_watch_state SET updated_at=42 WHERE item_id=?")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        watch::progress(&db, "user", &[report(0)]).await.unwrap();
        let state = watch::read(&db, "user", &ids)
            .await
            .unwrap()
            .remove(id)
            .unwrap();
        assert!(state.played);
        assert_eq!(state.resume_position_ms, None);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT updated_at FROM catalogue_watch_state WHERE item_id=?"
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap(),
            42
        );
        watch::progress(&db, "user", &[report(1)]).await.unwrap();
        assert!(!watch::read(&db, "user", &ids).await.unwrap()[id].played);
        watch::mark(&db, "user", if track { "album" } else { id }, &ids, true)
            .await
            .unwrap();
        watch::mark(&db, "user", if track { "album" } else { id }, &ids, false)
            .await
            .unwrap();
        let state = watch::read(&db, "user", &ids)
            .await
            .unwrap()
            .remove(id)
            .unwrap();
        assert!(!state.played);
        assert_eq!(state.resume_position_ms, None);
        assert!(
            serde_json::to_value(state)
                .unwrap()
                .get("play_count")
                .is_none()
        );
    }
}
