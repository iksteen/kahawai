//! Existing playback resources retain their captured copy across a rematch.
mod common;
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    h: &common::Harness,
    bearer: &str,
    method: &str,
    path: &str,
    body: Value,
) -> axum::response::Response {
    h.api
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", bearer)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn public_listings_resolve_their_track_urls(context: &str) {
    use kahawai_media::subtitles::{Cue, Extracted};
    let h = common::harness(
        "Listing (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    let (source, file, copy, module, collection, root, path): (
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT ps.id,f.id,ps.item_id,f.module_id,f.collection_id,r.root_token,f.path_rel
            FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id
            JOIN files f ON f.id=p.file_id JOIN collection_roots r ON r.id=f.root_id",
    )
    .fetch_one(&h.db)
    .await
    .unwrap();
    sqlx::query("UPDATE files SET streams_json=json_set(streams_json,'$.subtitles',json_array(json_object('format','ass','language','en'))) WHERE id=?")
        .bind(file).execute(&h.db).await.unwrap();
    let track: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,'embedded',0,'ass','en') RETURNING id")
        .bind(file).fetch_one(&h.db).await.unwrap();
    let ass = "[Script Info]\n; public listing subtitle\n";
    h.subtitles
        .store_extracted(
            &module,
            &collection,
            &root,
            &path,
            "e0",
            &Extracted {
                cues: vec![Cue {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: "public listing subtitle".into(),
                }],
                ass: Some(ass.into()),
            },
        )
        .unwrap();
    let kind = match context {
        "album" => "song",
        "combined" => "episode",
        _ => "movie",
    };
    let parent = if kind == "movie" {
        None
    } else {
        let mut tx = h.db.begin().await.unwrap();
        let parent_kind = if kind == "song" { "album" } else { "show" };
        sqlx::query("INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('parent-copy',?,'Parent','parent',2000,?,?)")
            .bind(parent_kind).bind(&module).bind(&collection).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE collection_items SET kind=?,parent_id='parent-copy',season=1,episode=1,episode_end=? WHERE id=?")
            .bind(if kind == "song" { "track" } else { "episode" })
            .bind(if kind == "episode" { Some(2_i64) } else { None }).bind(&copy).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        Some(sqlx::query_scalar::<_, String>("SELECT library_item_id FROM collection_item_library_items WHERE collection_item_id='parent-copy' AND ordinal=1")
            .fetch_one(&h.db).await.unwrap())
    };
    let mut tx = h.db.begin().await.unwrap();
    let mut targets = Vec::new();
    for episode in 1..=if kind == "episode" { 2 } else { 1 } {
        targets.push(
            kahawai_hub::library::create(
                &mut tx,
                kahawai_hub::library::NewItem {
                    kind: kind.into(),
                    title: format!("Correct work {episode}"),
                    year: Some(2001),
                    artist: None,
                    parent_id: parent.clone(),
                    season: (kind != "movie").then_some(1),
                    episode: (kind != "movie").then_some(episode),
                    edition: None,
                },
            )
            .await
            .unwrap(),
        );
    }
    kahawai_hub::library::assign(&mut tx, &copy, &targets)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let public_id = targets.last().unwrap();
    assert_ne!(public_id, &copy);
    let album_track: Option<i64> =
        sqlx::query_scalar("SELECT album_track_id FROM collection_items WHERE id=?")
            .bind(&copy)
            .fetch_one(&h.db)
            .await
            .unwrap();
    if kind == "song" {
        assert!(album_track.is_some());
    }
    let queried = request(
        &h,
        &h.bearer,
        "QUERY",
        &format!("/api/v1/items/{public_id}"),
        json!({"source_id":source}),
    )
    .await;
    assert_eq!(queried.status(), 200);
    let queried = common::json_body(queried).await;
    assert_eq!(queried["id"], public_id.as_str());
    let started = request(&h, &h.bearer, "POST", "/api/v1/playback/sessions",
        json!({"item_id":public_id,"mode":"direct","source_id":source,"album_track_id":album_track})).await;
    assert_eq!(started.status(), 201);
    let started = common::json_body(started).await;
    let session = h
        .sessions
        .get(started["session_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        session.collection_item_id, copy,
        "the physical owner stays internal"
    );
    if kind == "episode" {
        assert_eq!(session.library_item_ids, targets);
    }
    let internal_track = kahawai_hub::tracks::get_for_item(&h.db, &copy, track)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(internal_track.item_id, copy);
    let mut returned_ids = Vec::new();
    let mut statuses = Vec::new();
    for listing in [
        &queried["negotiated"]["subtitles"],
        &started["subtitle_listing"],
    ] {
        let listed = listing.as_array().expect("a public subtitle listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], track);
        let item_id = listed[0]["item_id"].as_str().unwrap();
        returned_ids.push(item_id.to_owned());
        let response = request(
            &h,
            &h.bearer,
            "GET",
            &format!("/api/v1/items/{item_id}/subtitles/{track}.ass"),
            Value::Null,
        )
        .await;
        statuses.push(response.status().as_u16());
        if response.status().is_success() {
            assert_eq!(common::body_bytes(response).await, ass.as_bytes());
        }
    }
    assert_eq!(
        statuses,
        vec![200, 200],
        "{context}: QUERY and start listing URLs must resolve; listed IDs: {returned_ids:?}"
    );
    assert_eq!(returned_ids, vec![public_id.clone(), public_id.clone()]);
}

#[tokio::test]
async fn rematched_movie_listings_publish_the_public_item_id() {
    public_listings_resolve_their_track_urls("movie").await;
}

#[tokio::test]
async fn album_song_listings_publish_the_public_song_id() {
    public_listings_resolve_their_track_urls("album").await;
}

#[tokio::test]
async fn combined_episode_listings_publish_the_requested_member_id() {
    public_listings_resolve_their_track_urls("combined").await;
}

#[tokio::test]
async fn active_session_text_and_fonts_survive_rematching_only_for_the_owner_with_access() {
    use kahawai_media::subtitles::{Cue, Extracted};
    let h = common::harness(
        "Captured (2000).mp4",
        kahawai_media::testutil::render_h264_aac_mp4,
    )
    .await;
    sqlx::raw_sql(
        "INSERT INTO users(id,username,password_hash,is_admin,all_libraries)
        SELECT 'viewer','viewer',password_hash,0,0 FROM users LIMIT 1;
        INSERT INTO libraries(id,name,media_type) VALUES('allowed','Allowed','movies');
        INSERT INTO library_collections SELECT 'allowed',module_id,collection_id FROM collections;
        INSERT INTO user_libraries VALUES('viewer','allowed');",
    )
    .execute(&h.db)
    .await
    .unwrap();
    let login = request(
        &h,
        &h.bearer,
        "POST",
        "/api/v1/auth/token",
        json!({"client":"api","username":"viewer","password":"password-123"}),
    )
    .await;
    assert_eq!(login.status(), 200);
    let login = common::json_body(login).await;
    let viewer = format!("Bearer {}", login["access_token"].as_str().unwrap());
    let (source, file, copy, module, collection, root, path): (
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT ps.id,f.id,ps.item_id,f.module_id,f.collection_id,r.root_token,f.path_rel
            FROM playable_sources ps JOIN playable_source_parts p ON p.playable_source_id=ps.id
            JOIN files f ON f.id=p.file_id JOIN collection_roots r ON r.id=f.root_id",
    )
    .fetch_one(&h.db)
    .await
    .unwrap();
    sqlx::query("UPDATE files SET streams_json=json_set(streams_json,'$.subtitles',json_array(json_object('format','ass','language','en')),'$.attachments',json_array(json_object('file_name','font.ttf','mime_type','font/ttf','offset',0,'size',16))) WHERE id=?")
        .bind(file).execute(&h.db).await.unwrap();
    let track: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,'embedded',0,'ass','en') RETURNING id")
        .bind(file).fetch_one(&h.db).await.unwrap();
    h.subtitles
        .store_extracted(
            &module,
            &collection,
            &root,
            &path,
            "e0",
            &Extracted {
                cues: vec![Cue {
                    start_ms: 1000,
                    end_ms: 2000,
                    text: "captured subtitle".into(),
                }],
                ass: Some("[Script Info]\n; captured subtitle\n".into()),
            },
        )
        .unwrap();
    let started = request(
        &h,
        &viewer,
        "POST",
        "/api/v1/playback/sessions",
        json!({"item_id":h.item_id,"mode":"direct","source_id":source}),
    )
    .await;
    assert_eq!(started.status(), 201);
    let started = common::json_body(started).await;
    let sid = started["session_id"].as_str().unwrap();
    let routes = [
        format!("/api/v1/items/{}/subtitles/{track}.vtt", h.item_id),
        format!("/api/v1/items/{}/subtitles/{track}.ass", h.item_id),
        format!("/api/v1/items/{}/fonts?source_id={source}", h.item_id),
        format!("/api/v1/items/{}/fonts/0?source_id={source}", h.item_id),
    ];
    let mut expected = Vec::new();
    for route in &routes {
        let response = request(&h, &viewer, "GET", route, Value::Null).await;
        assert_eq!(response.status(), 200, "before rematch: {route}");
        expected.push(common::body_bytes(response).await);
    }
    let mut tx = h.db.begin().await.unwrap();
    let corrected = kahawai_hub::library::create(
        &mut tx,
        kahawai_hub::library::NewItem {
            kind: "movie".into(),
            title: "Correct work".into(),
            year: Some(2001),
            artist: None,
            parent_id: None,
            season: None,
            episode: None,
            edition: None,
        },
    )
    .await
    .unwrap();
    kahawai_hub::library::assign(&mut tx, &copy, std::slice::from_ref(&corrected))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(h.sessions.get(sid).unwrap().collection_item_id, copy);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM collection_item_library_items WHERE library_item_id=?"
        )
        .bind(&h.item_id)
        .fetch_one(&h.db)
        .await
        .unwrap(),
        0
    );

    let mut statuses = Vec::new();
    for (route, bytes) in routes.iter().zip(&expected) {
        let response = request(&h, &viewer, "GET", route, Value::Null).await;
        statuses.push(response.status().as_u16());
        if response.status().is_success() {
            assert_eq!(&common::body_bytes(response).await, bytes, "{route}");
        }
    }
    assert_eq!(
        statuses,
        vec![200; 4],
        "a rematch must preserve resources for the active session"
    );

    // Another rendition on the same captured copy remains outside this session.
    let mut tx = h.db.begin().await.unwrap();
    let other_file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
        SELECT module_id,collection_id,root_id,'uncaptured.mp4',size,mtime_unix,head_xxh3+1,tail_xxh3,oshash,streams_json FROM files WHERE id=? RETURNING id")
        .bind(file).fetch_one(&mut *tx).await.unwrap();
    kahawai_hub::registry::bind_file_to_item(&mut tx, other_file, &copy)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let other_source: i64 =
        sqlx::query_scalar("SELECT playable_source_id FROM playable_source_parts WHERE file_id=?")
            .bind(other_file)
            .fetch_one(&h.db)
            .await
            .unwrap();
    let other_track: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,'embedded',0,'ass','en') RETURNING id")
        .bind(other_file).fetch_one(&h.db).await.unwrap();

    // An administrator has broad library access, but cannot borrow the viewer's session.
    for route in &routes {
        assert_eq!(
            request(&h, &h.bearer, "GET", route, Value::Null)
                .await
                .status(),
            404,
            "foreign session: {route}"
        );
    }
    for route in [
        format!("/api/v1/items/unrelated/subtitles/{track}.vtt"),
        format!("/api/v1/items/{}/fonts?source_id={other_source}", h.item_id),
        format!(
            "/api/v1/items/{}/fonts/0?source_id={other_source}",
            h.item_id
        ),
        format!("/api/v1/items/{}/subtitles/{other_track}.vtt", h.item_id),
    ] {
        assert_eq!(
            request(&h, &viewer, "GET", &route, Value::Null)
                .await
                .status(),
            404,
            "{route}"
        );
    }
    sqlx::query("DELETE FROM user_libraries WHERE user_id='viewer'")
        .execute(&h.db)
        .await
        .unwrap();
    for route in &routes {
        assert_eq!(
            request(&h, &viewer, "GET", route, Value::Null)
                .await
                .status(),
            404,
            "revoked physical collection: {route}"
        );
    }
    sqlx::query("INSERT INTO user_libraries VALUES('viewer','allowed')")
        .execute(&h.db)
        .await
        .unwrap();
    assert_eq!(
        request(&h, &viewer, "GET", &routes[0], Value::Null)
            .await
            .status(),
        200
    );
    assert_eq!(
        request(
            &h,
            &viewer,
            "DELETE",
            &format!("/api/v1/playback/sessions/{sid}"),
            Value::Null
        )
        .await
        .status(),
        204
    );
    for route in &routes {
        assert_eq!(
            request(&h, &viewer, "GET", route, Value::Null)
                .await
                .status(),
            404,
            "ended session: {route}"
        );
    }
}
