//! Real mediahost playback keeps library coverage and exact-source resume.
mod common;
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(h: &common::Harness, method: &str, path: &str, body: Value) -> Value {
    let response = h
        .api
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", &h.bearer)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = common::body_bytes(response).await;
    let result: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    assert!(status.is_success(), "{method} {path}: {status} {result}");
    result
}

#[tokio::test]
async fn exact_episode_boundaries_drive_resume_and_history_without_splitting_files() {
    let h = common::harness(
        "Combined (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    let copy: String = sqlx::query_scalar(
        "SELECT collection_item_id FROM collection_item_library_items WHERE library_item_id=?",
    )
    .bind(&h.item_id)
    .fetch_one(&h.db)
    .await
    .unwrap();
    let mut tx = h.db.begin().await.unwrap();
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) SELECT 'parent','show','Series','series',2000,module_id,collection_id FROM collection_items WHERE id=?").bind(&copy).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE collection_items SET kind='episode',parent_id='parent',season=1,episode=1,episode_end=2 WHERE id=?").bind(&copy).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id=? ORDER BY ordinal",
    )
    .bind(&copy)
    .fetch_all(&h.db)
    .await
    .unwrap();
    let (source,file):(i64,i64)=sqlx::query_as("SELECT ps.id,p.file_id FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id WHERE ps.item_id=?").bind(&copy).fetch_one(&h.db).await.unwrap();
    let snapshot = kahawai_hub::library::playback_snapshot(&h.db, &ids[0], file)
        .await
        .unwrap();
    for (ordinal, start, end) in [(1, 0, 3000), (2, 3000, 6000)] {
        sqlx::query("INSERT INTO source_boundaries VALUES(?,?,?,?,?)")
            .bind(source)
            .bind(ordinal)
            .bind(&snapshot.fingerprint)
            .bind(start)
            .bind(end)
            .execute(&h.db)
            .await
            .unwrap();
    }
    let mut mapped = kahawai_hub::library::playback_snapshot(&h.db, &ids[0], file)
        .await
        .unwrap();
    let previous = kahawai_hub::library::resume_fingerprint(&mapped);
    mapped.boundaries[0].library_item_id = ids[1].clone();
    mapped.boundaries[1].library_item_id = ids[0].clone();
    assert!(
        !kahawai_hub::library::same_resume_version(
            &h.db,
            Some(&previous),
            &kahawai_hub::library::resume_fingerprint(&mapped)
        )
        .await
        .unwrap(),
        "moving a member on unchanged bytes invalidates its resume coordinate"
    );
    let zero = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":ids[1],"mode":"direct","source_id":source,"start_ms":0}),
    )
    .await;
    assert_eq!(
        zero["effective_start_ms"], 0,
        "an explicit chapter at physical zero stays zero"
    );
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            zero["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    let started = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":ids[1],"mode":"direct"}),
    )
    .await;
    assert_eq!(started["effective_start_ms"], 3000);
    assert_eq!(started["coverage"].as_array().unwrap().len(), 2);
    let sid = started["session_id"].as_str().unwrap();
    call(
        &h,
        "POST",
        &format!("/api/v1/playback/sessions/{sid}/progress"),
        json!({"position_ms":5900}),
    )
    .await;
    call(
        &h,
        "DELETE",
        &format!("/api/v1/playback/sessions/{sid}"),
        json!({}),
    )
    .await;
    let states: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT item_id,position_ms,played,play_count FROM user_item_state ORDER BY item_id",
    )
    .fetch_all(&h.db)
    .await
    .unwrap();
    assert_eq!(
        states,
        vec![(ids[1].clone(), 2900, 1, 1)],
        "starting the second episode must not mark the first watched"
    );
    sqlx::query("DELETE FROM user_item_state")
        .execute(&h.db)
        .await
        .unwrap();
    let chapter = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":ids[0],"mode":"direct","source_id":source,"start_ms":3500}),
    )
    .await;
    assert_eq!(chapter["source_id"], source);
    let sid = chapter["session_id"].as_str().unwrap();
    call(
        &h,
        "POST",
        &format!("/api/v1/playback/sessions/{sid}/progress"),
        json!({"position_ms":5900}),
    )
    .await;
    call(
        &h,
        "DELETE",
        &format!("/api/v1/playback/sessions/{sid}"),
        json!({}),
    )
    .await;
    let watched: Vec<String> =
        sqlx::query_scalar("SELECT item_id FROM user_item_state WHERE played=1")
            .fetch_all(&h.db)
            .await
            .unwrap();
    assert_eq!(
        watched,
        vec![ids[1].clone()],
        "a chapter in E2 must not mark E1 visited"
    );
    // An initially skipped member remains playable after seeking backward.
    let back = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":ids[1],"mode":"direct"}),
    )
    .await;
    let sid = back["session_id"].as_str().unwrap();
    for position in [1500, 2900] {
        call(
            &h,
            "POST",
            &format!("/api/v1/playback/sessions/{sid}/progress"),
            json!({"position_ms":position}),
        )
        .await;
    }
    call(
        &h,
        "DELETE",
        &format!("/api/v1/playback/sessions/{sid}"),
        json!({}),
    )
    .await;
    let first: (i64, i64, i64) =
        sqlx::query_as("SELECT position_ms,played,play_count FROM user_item_state WHERE item_id=?")
            .bind(&ids[0])
            .fetch_one(&h.db)
            .await
            .unwrap();
    assert_eq!(first, (2900, 1, 1));
    // Altering coordinates without altering the bytes invalidates a saved offset.
    sqlx::query("UPDATE user_item_state SET position_ms=500,played=0 WHERE item_id=?")
        .bind(&ids[1])
        .execute(&h.db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM source_boundaries WHERE playable_source_id=?")
        .bind(source)
        .execute(&h.db)
        .await
        .unwrap();
    let changed = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":ids[1],"mode":"direct","start_ms":500,"resume":true}),
    )
    .await;
    assert_eq!(
        changed["effective_start_ms"], 0,
        "member-relative offset is not source-relative"
    );
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            changed["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    // Restoring the old boundary rows still cannot attach them to replacement bytes.
    for (ordinal, start, end) in [(1, 0, 3000), (2, 3000, 6000)] {
        sqlx::query("INSERT INTO source_boundaries VALUES(?,?,?,?,?)")
            .bind(source)
            .bind(ordinal)
            .bind(&snapshot.fingerprint)
            .bind(start)
            .bind(end)
            .execute(&h.db)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE files SET head_xxh3=head_xxh3+1 WHERE id=?")
        .bind(file)
        .execute(&h.db)
        .await
        .unwrap();
    let changed = kahawai_hub::library::playback_snapshot(&h.db, &ids[1], file)
        .await
        .unwrap();
    assert!(
        changed.boundaries.is_empty(),
        "boundaries from replacement bytes are ineligible"
    );
}

#[tokio::test]
async fn source_choices_stay_bound_and_recovery_can_use_an_identical_copy() {
    let h = common::harness(
        "Versions (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    let initial = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":h.item_id,"mode":"direct"}),
    )
    .await;
    let original = initial["source_id"].as_i64().unwrap();
    let fingerprint = initial["source_fingerprint"].as_str().unwrap().to_string();
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            initial["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    let mut tx = h.db.begin().await.unwrap();
    let file:i64=sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
        SELECT f.module_id,f.collection_id,f.root_id,'alternate.mp4',f.size,f.mtime_unix,f.head_xxh3+1,f.tail_xxh3,f.oshash,json_set(f.streams_json,'$.video[0].height',4320,'$.replay_gain',json_object('album_gain_db',-6)) FROM files f JOIN playable_source_parts p ON p.file_id=f.id WHERE p.playable_source_id=? RETURNING id")
        .bind(original).fetch_one(&mut *tx).await.unwrap();
    let copy: String = sqlx::query_scalar("SELECT item_id FROM playable_sources WHERE id=?")
        .bind(original)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    kahawai_hub::registry::bind_file_to_item(&mut tx, file, &copy)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let alternate: i64 =
        sqlx::query_scalar("SELECT playable_source_id FROM playable_source_parts WHERE file_id=?")
            .bind(file)
            .fetch_one(&h.db)
            .await
            .unwrap();
    let pinned = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":h.item_id,"mode":"direct","source_id":original,"audio_track":0}),
    )
    .await;
    assert_eq!(
        pinned["source_id"], original,
        "a source-specific choice cannot switch versions"
    );
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            pinned["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
    // The original disappears; another source retains exactly the same bytes.
    let mut tx = h.db.begin().await.unwrap();
    sqlx::query("UPDATE files SET head_xxh3=head_xxh3-1 WHERE id=?")
        .bind(file)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM playable_sources WHERE id=?")
        .bind(original)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let recovered=call(&h,"POST","/api/v1/playback/sessions",json!({"item_id":h.item_id,"mode":"direct","source_id":original,"resume_source_fingerprint":fingerprint,"start_ms":1000})).await;
    assert_eq!(recovered["source_id"], alternate);
    assert_eq!(recovered["effective_start_ms"], 1000);
    assert_eq!(
        recovered["replay_gain"]["album_gain_db"], -6.0,
        "gain belongs to the actual selected source"
    );
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            recovered["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn playback_and_source_access_follow_only_first_identification_aliases() {
    let h = common::harness(
        "Versions (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) SELECT 'before-identification','movie','Versions','versions',NULL,module_id,collection_id FROM collection_items WHERE id=?")
        .bind(&h.item_id).execute(&h.db).await.unwrap();
    kahawai_hub::providers::assign_manual(
        &h.db,
        "before-identification",
        "tmdb",
        "versions",
        kahawai_hub::providers::Fields {
            title: Some("Versions".into()),
            premiered: Some("2000-01-01".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let target: String = sqlx::query_scalar(
        "SELECT merged_into FROM library_items WHERE id='before-identification'",
    )
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(target, h.item_id);
    let detail = call(
        &h,
        "GET",
        "/api/v1/items/before-identification",
        Value::Null,
    )
    .await;
    assert_eq!(detail["id"], h.item_id);
    let started = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":"before-identification","mode":"direct"}),
    )
    .await;
    assert!(
        started["library_item_ids"]
            .as_array()
            .unwrap()
            .contains(&json!(h.item_id))
    );
    call(
        &h,
        "DELETE",
        &format!(
            "/api/v1/playback/sessions/{}",
            started["session_id"].as_str().unwrap()
        ),
        json!({}),
    )
    .await;
}
