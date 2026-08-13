//! Opt-in proof against a disposable copy of a real level-52 data directory.
//!
//! ```text
//! KAHAWAI_REAL_MIGRATION_DIR=$HOME/kahawai-v52-copy \
//!   cargo test -p kahawai-hub --test real_catalogue_migration -- --ignored
//! ```

#[tokio::test]
#[ignore = "requires KAHAWAI_REAL_MIGRATION_DIR pointing at a disposable level-52 copy"]
async fn embedded_migrator_upgrades_a_real_catalogue_copy() {
    let dir = std::env::var_os("KAHAWAI_REAL_MIGRATION_DIR")
        .expect("KAHAWAI_REAL_MIGRATION_DIR is required");
    let dir = std::path::PathBuf::from(dir);
    assert!(
        dir.join("hub.db").exists(),
        "no hub.db in {}",
        dir.display()
    );
    let db = kahawai_hub::db::open(&dir).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(&db)
            .await
            .unwrap(),
        55
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pragma_foreign_key_check")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA quick_check")
            .fetch_one(&db)
            .await
            .unwrap(),
        "ok"
    );
    let invalid: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM items i JOIN items p ON p.id=i.parent_id
                  WHERE (i.module_id,i.collection_id)!=(p.module_id,p.collection_id))
              +(SELECT count(*) FROM files f JOIN items i ON i.id=f.item_id
                  WHERE (f.module_id,f.collection_id)!=(i.module_id,i.collection_id))
              +(SELECT count(*) FROM subtitle_tracks
                  WHERE (item_id IS NULL)=(source_id IS NULL))
              +(SELECT count(*) FROM subtitle_tracks t JOIN subtitle_tracks p
                  ON p.id=t.derived_from
                  WHERE t.item_id IS NOT p.item_id OR t.source_id IS NOT p.source_id)",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(invalid, 0);
}
