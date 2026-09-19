//! HUB-36 phase 4: the pace table survives what it must and forgets
//! what it should. The EWMA arithmetic itself is unit-tested in the
//! module; this is about persistence.

use kahawai_hub::pace;

#[tokio::test]
async fn folds_persist_across_restart_and_die_with_the_satellite() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let class = pace::work_class(2160, "hevc", "h264", true);

    // First sample IS the estimate — nothing to blend against.
    assert_eq!(pace::fold(&db, "mod-a", &class, 4.0, 1).await.unwrap(), 4.0);
    // Second folds at ALPHA: 0.3*2.0 + 0.7*4.0 = 3.4.
    let second = pace::fold(&db, "mod-a", &class, 2.0, 2).await.unwrap();
    assert!((second - 3.4).abs() < 1e-9, "got {second}");

    // A different box learning the same class is a SEPARATE row: the
    // whole point is telling boxes apart.
    pace::fold(&db, "mod-b", &class, 0.6, 3).await.unwrap();
    assert_eq!(pace::load_all(&db).await.unwrap().len(), 2);

    // What a restart would read back.
    let rows = pace::load_all(&db).await.unwrap();
    let (_, _, m) = rows.iter().find(|(id, _, _)| id == "mod-a").unwrap();
    assert!((m - second).abs() < 1e-9, "reload changed the estimate");

    // Sample count is kept for diagnosis, not weighting.
    let n: i64 = sqlx::query_scalar("SELECT samples FROM transcoder_pace WHERE module_id = ?")
        .bind("mod-a")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 2);

    // Deleting a satellite forgets what its hardware could do.
    pace::forget(&db, "mod-a").await.unwrap();
    let left = pace::load_all(&db).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].0, "mod-b");
}
