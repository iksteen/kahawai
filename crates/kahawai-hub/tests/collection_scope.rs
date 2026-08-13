//! Collection and subtitle ownership are storage invariants, not query conventions.

#[tokio::test]
async fn cross_collection_references_are_rejected_and_source_tracks_follow_the_source() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
           VALUES('m','mediahost','m','fp');
         INSERT INTO collections(module_id,collection_id,media_type) VALUES
           ('m','a','movies'),('m','b','movies');
         INSERT INTO items(id,kind,title,norm_title,module_id,collection_id) VALUES
           ('a1','movie','A1','a1','m','a'),
           ('a2','movie','A2','a2','m','a'),
           ('b1','movie','B1','b1','m','b');
         INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES
           ('m','a','ra','/a'),('m','b','rb','/b');",
    )
    .execute(&db)
    .await
    .unwrap();

    for sql in [
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('bad','movie','Bad','bad','m','missing')",
        "INSERT INTO items(id,kind,title,norm_title,parent_id,module_id,collection_id)
         VALUES('child','episode','Child','child','b1','m','a')",
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('m','a',(SELECT id FROM collection_roots WHERE root_token='rb'),
                'bad-root.mkv','a1',1,1,0,0,0,'{}')",
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('m','a',(SELECT id FROM collection_roots WHERE root_token='ra'),
                'bad-item.mkv','b1',1,1,0,0,0,'{}')",
    ] {
        assert!(
            sqlx::query(sql).execute(&db).await.is_err(),
            "accepted: {sql}"
        );
    }

    let source_id: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,item_id,size,mtime_unix,
                           head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('m','a',(SELECT id FROM collection_roots WHERE root_token='ra'),
                'a.mkv','a1',1,1,0,0,0,'{}') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();

    // Exactly one direct owner: physical tracks name only the source and
    // independently acquired tracks name only the item.
    assert!(
        sqlx::query(
            "INSERT INTO subtitle_tracks(item_id,source_id,origin,stream_index,format)
             VALUES('a1',?,'embedded',0,'srt')",
        )
        .bind(source_id)
        .execute(&db)
        .await
        .is_err(),
        "track accepted two owners"
    );
    let track_id: i64 = sqlx::query_scalar(
        "INSERT INTO subtitle_tracks(source_id,origin,stream_index,format)
         VALUES(?,'embedded',0,'srt') RETURNING id",
    )
    .bind(source_id)
    .fetch_one(&db)
    .await
    .unwrap();
    let downloaded: i64 = sqlx::query_scalar(
        "INSERT INTO subtitle_tracks(item_id,origin,format)
         VALUES('a1','downloaded','srt') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();

    assert!(
        kahawai_hub::tracks::get_for_item(&db, "a1", track_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        kahawai_hub::tracks::get_for_item(&db, "a2", track_id)
            .await
            .unwrap()
            .is_none()
    );

    // Rebinding the physical source changes its catalogue context without
    // rewriting the source-owned track.
    sqlx::query("UPDATE files SET item_id='a2' WHERE id=?")
        .bind(source_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        kahawai_hub::tracks::get_for_item(&db, "a1", track_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        kahawai_hub::tracks::get_for_item(&db, "a2", track_id)
            .await
            .unwrap()
            .unwrap()
            .source_id,
        Some(source_id)
    );

    // A derivative inherits that same direct owner; another owner is rejected.
    let ocr: i64 = sqlx::query_scalar(
        "INSERT INTO subtitle_tracks(source_id,origin,format,derived_from)
         VALUES(?,'ocr','srt',?) RETURNING id",
    )
    .bind(source_id)
    .bind(track_id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO subtitle_tracks(item_id,origin,format,derived_from)
             VALUES('a2','ocr','srt',?)",
        )
        .bind(track_id)
        .execute(&db)
        .await
        .is_err(),
        "derivative accepted another owner"
    );
    assert!(
        sqlx::query("UPDATE subtitle_tracks SET source_id=NULL,item_id='a2' WHERE id=?")
            .bind(track_id)
            .execute(&db)
            .await
            .is_err(),
        "parent owner changed without its derivative"
    );

    // Bare physical facts survive temporary catalogue unbinding but disappear
    // from item-scoped lookup. Item-owned downloads are unaffected.
    sqlx::query("UPDATE files SET item_id=NULL WHERE id=?")
        .bind(source_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        kahawai_hub::tracks::get_for_item(&db, "a2", track_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subtitle_tracks WHERE id IN (?,?)")
            .bind(track_id)
            .bind(ocr)
            .fetch_one(&db)
            .await
            .unwrap(),
        2
    );

    // Source removal evicts physical streams and reproducible derivatives;
    // independently acquired item tracks remain.
    sqlx::query("DELETE FROM files WHERE id=?")
        .bind(source_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subtitle_tracks WHERE id IN (?,?)")
            .bind(track_id)
            .bind(ocr)
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM subtitle_tracks WHERE id=?")
            .bind(downloaded)
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}
