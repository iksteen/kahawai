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
async fn queued_album_position_survives_repeated_album_promotions() {
    let h = common::harness(
        "Queued (2000).mp4",
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
    for (id, year) in [
        ("old-album", None),
        ("middle-album", None),
        ("known-album", Some(2000)),
    ] {
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,artist,module_id,collection_id) SELECT ?,'album',?,?,?,'Artist',module_id,collection_id FROM collection_items WHERE id=?")
            .bind(id).bind(id).bind(id).bind(year).bind(&copy).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("UPDATE collection_items SET kind='track',parent_id='old-album',season=1,episode=1 WHERE id=?")
        .bind(&copy).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let original_queue: (String, i64) = sqlx::query_as("SELECT a.library_item_id,ci.album_track_id FROM collection_items ci JOIN collection_item_library_items a ON a.collection_item_id=ci.id WHERE ci.id=?")
        .bind(&copy).fetch_one(&h.db).await.unwrap();
    let mut tx = h.db.begin().await.unwrap();
    let song = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "song".into(),
            title: "Chosen Song".into(),
            year: None,
            artist: None,
            parent_id: Some("known-album".into()),
            season: Some(1),
            episode: Some(1),
            edition: None,
        },
    )
    .await
    .unwrap();
    // Both destination albums already have this recording at the same slot.
    for album in ["middle-album", "known-album"] {
        sqlx::query("INSERT INTO album_tracks(album_id,song_id,disc_number,track_number) VALUES(?,?,1,1) ON CONFLICT DO NOTHING")
            .bind(album).bind(&song).execute(&mut *tx).await.unwrap();
    }
    kahawai_hub::library::assign(&mut tx, &copy, std::slice::from_ref(&song))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let queued_position: i64 =
        sqlx::query_scalar("SELECT album_track_id FROM collection_items WHERE id=?")
            .bind(&copy)
            .fetch_one(&h.db)
            .await
            .unwrap();
    for (source, target) in [
        ("old-album", "middle-album"),
        ("middle-album", "known-album"),
    ] {
        let mut tx = h.db.begin().await.unwrap();
        kahawai_hub::library::assign(&mut tx, source, &[target.into()])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    for (queued_song, position) in [
        (&song, queued_position),
        (&original_queue.0, original_queue.1),
    ] {
        let started = call(
            &h,
            "POST",
            "/api/v1/playback/sessions",
            json!({"item_id":queued_song,"album_track_id":position,"mode":"direct"}),
        )
        .await;
        let session_id = started["session_id"].as_str().unwrap();
        assert_eq!(h.sessions.get(session_id).unwrap().collection_item_id, copy);
        call(
            &h,
            "DELETE",
            &format!("/api/v1/playback/sessions/{session_id}"),
            json!({}),
        )
        .await;
    }
    let children = call(&h, "GET", "/api/v1/items/old-album/children", json!({})).await;
    assert_eq!(
        children["children"].as_array().unwrap().len(),
        1,
        "retained historical slots cannot create duplicate browse entries"
    );
    assert_eq!(children["children"][0]["id"], song);
    let mut tx = h.db.begin().await.unwrap();
    let other_album = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "album".into(),
            title: "Different album".into(),
            year: Some(2002),
            artist: Some("Artist".into()),
            parent_id: None,
            season: None,
            episode: None,
            edition: None,
        },
    )
    .await
    .unwrap();
    let other_song = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "song".into(),
            title: "Different song".into(),
            year: None,
            artist: None,
            parent_id: Some("known-album".into()),
            season: Some(1),
            episode: Some(1),
            edition: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for (album, recording, disc, track) in [
        ("known-album", song.as_str(), 2, 1),
        ("known-album", song.as_str(), 1, 2),
        (other_album.as_str(), song.as_str(), 1, 1),
        ("known-album", other_song.as_str(), 1, 1),
    ] {
        let wrong_position: i64 = sqlx::query_scalar("INSERT INTO album_tracks(album_id,song_id,disc_number,track_number) VALUES(?,?,?,?) ON CONFLICT(album_id,disc_number,track_number,song_id) DO UPDATE SET song_id=excluded.song_id RETURNING id")
            .bind(album).bind(recording).bind(disc).bind(track).fetch_one(&h.db).await.unwrap();
        let response = h
            .api
            .clone()
            .oneshot(
                Request::post("/api/v1/playback/sessions")
                    .header("authorization", &h.bearer)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"item_id":song,"album_track_id":wrong_position,"mode":"direct"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !response.status().is_success(),
            "position mismatch must not fall back to another slot containing the song"
        );
    }
}

#[tokio::test]
async fn recovery_refuses_a_replaced_version_but_waits_for_a_retained_offline_version() {
    let h = common::harness(
        "Recovery (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    let started = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":h.item_id,"mode":"direct"}),
    )
    .await;
    let sid = started["session_id"].as_str().unwrap();
    let source = started["source_id"].as_i64().unwrap();
    let fingerprint = started["source_fingerprint"].as_str().unwrap();
    let session = h.sessions.get(sid).unwrap();
    let file = session.parts[0].file_id;
    let module = session.parts[0].module_id.clone();
    call(
        &h,
        "DELETE",
        &format!("/api/v1/playback/sessions/{sid}"),
        json!({}),
    )
    .await;
    sqlx::query("UPDATE files SET head_xxh3=head_xxh3+1 WHERE id=?")
        .bind(file)
        .execute(&h.db)
        .await
        .unwrap();
    for selected in [Some(source), None] {
        let response = h.api.clone().oneshot(Request::post("/api/v1/playback/sessions")
            .header("authorization", &h.bearer)
            .header("content-type", "application/json")
            .body(Body::from(json!({"item_id":h.item_id,"mode":"direct","source_id":selected,"resume_source_fingerprint":fingerprint,"start_ms":1000}).to_string())).unwrap()).await.unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::CONFLICT,
            "no retained bytes match the recovery fingerprint (source_id={selected:?})"
        );
    }
    sqlx::query("UPDATE files SET head_xxh3=head_xxh3-1 WHERE id=?")
        .bind(file)
        .execute(&h.db)
        .await
        .unwrap();
    h.registry.unregister_link(&module);
    h.registry.disconnected(&module);
    let response = h.api.clone().oneshot(Request::post("/api/v1/playback/sessions")
        .header("authorization", &h.bearer)
        .header("content-type", "application/json")
        .body(Body::from(json!({"item_id":h.item_id,"mode":"direct","source_id":source,"resume_source_fingerprint":fingerprint,"start_ms":1000}).to_string())).unwrap()).await.unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "an unchanged retained rendition becomes playable when its host returns"
    );
    assert_eq!(common::json_body(response).await["code"], "source_offline");
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
    let removed_choice = h
        .api
        .clone()
        .oneshot(
            Request::post("/api/v1/playback/sessions")
                .header("authorization", &h.bearer)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"item_id":h.item_id,"mode":"direct","source_id":original}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        removed_choice.status(),
        axum::http::StatusCode::CONFLICT,
        "a removed explicit choice needs another choice, not indefinite offline retries"
    );
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

#[tokio::test]
async fn switching_audio_uses_the_selected_renditions_stream_metadata() {
    if !kahawai_media::testutil::require_h264_aac_fixture() {
        return;
    }
    fn render_versions(path: &std::path::Path) {
        let aac = kahawai_media::remux::aac_encoder().unwrap();
        kahawai_media::testutil::render(&format!(
            "videotestsrc num-buffers=100 ! video/x-raw,format=I420,width=320,height=240,framerate=25/1 ! x264enc key-int-max=25 ! h264parse ! matroskamux name=m \
             audiotestsrc num-buffers=172 freq=440 ! audioconvert ! {aac} ! m. \
             audiotestsrc num-buffers=172 freq=880 ! audioconvert ! flacenc ! m. \
             m. ! filesink location=\"{}\"",
            path.display()
        ));
        kahawai_media::testutil::render_h264_aac_mkv(&path.with_file_name("larger.mkv"));
    }
    let h = common::harness("Versions (2000).mkv", render_versions).await;
    let (source, file, copy): (i64, i64, String) = sqlx::query_as(
        "SELECT ps.id,p.file_id,ps.item_id FROM playable_sources ps
         JOIN playable_source_parts p ON p.playable_source_id=ps.id",
    )
    .fetch_one(&h.db)
    .await
    .unwrap();
    let chosen_json: String = sqlx::query_scalar("SELECT streams_json FROM files WHERE id=?")
        .bind(file)
        .fetch_one(&h.db)
        .await
        .unwrap();
    let chosen: kahawai_core::media::MediaInfo = serde_json::from_str(&chosen_json).unwrap();
    assert_eq!(chosen.audio.len(), 2);
    assert_eq!(chosen.audio[1].codec, "flac");
    let root: String = sqlx::query_scalar("SELECT normalized_path FROM collection_roots LIMIT 1")
        .fetch_one(&h.db)
        .await
        .unwrap();
    let larger = std::path::Path::new(&root).join("larger.mkv");
    let size = std::fs::metadata(&larger).unwrap().len();
    let info = tokio::task::spawn_blocking(move || {
        kahawai_media::discover(&larger, std::time::Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(info.audio.len(), 1);
    let chosen_size: i64 = sqlx::query_scalar("SELECT size FROM files WHERE id=?")
        .bind(file)
        .fetch_one(&h.db)
        .await
        .unwrap();
    assert!(
        size > chosen_size as u64,
        "the other rendition must sort first by size"
    );
    let mut tx = h.db.begin().await.unwrap();
    let other: i64 = sqlx::query_scalar(
        "INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
         SELECT module_id,collection_id,root_id,'larger.mkv',?2,1,3,4,5,?3 FROM files WHERE id=?1 RETURNING id",
    )
    .bind(file)
    .bind(size as i64)
    .bind(serde_json::to_string(&info).unwrap())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    kahawai_hub::registry::bind_file_to_item(&mut tx, other, &copy)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let started = call(
        &h,
        "POST",
        "/api/v1/playback/sessions",
        json!({
            "item_id":h.item_id,"source_id":source,"mode":"remux","audio_track":0
        }),
    )
    .await;
    assert_eq!(started["source_id"], source);
    let sid = started["session_id"].as_str().unwrap();
    let switched = call(
        &h,
        "POST",
        &format!("/api/v1/playback/sessions/{sid}/seek"),
        json!({"position_ms":1000,"audio_track":1}),
    )
    .await;
    assert!(
        switched["streams"]["audio"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("flac"),
        "the selected rendition's second track is FLAC, not the larger rendition's sole AAC track: {switched}"
    );
    call(
        &h,
        "DELETE",
        &format!("/api/v1/playback/sessions/{sid}"),
        json!({}),
    )
    .await;
}
