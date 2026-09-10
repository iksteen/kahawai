use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

async fn harness() -> (
    axum::Router,
    String,
    kahawai_hub::library::Database,
    Arc<kahawai_hub::registry::Registry>,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(
        db.clone(),
        Default::default(),
    ));
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), dir.path())
            .await
            .unwrap(),
    );
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let ca = Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(dir.path().to_path_buf()));
    let artwork_dir = dir.path().join("artwork");
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth.clone(),
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            dir.path().join("subtitles"),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            artwork_dir.clone(),
            enricher.clone(),
        )),
        enricher,
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    );
    auth.complete_setup("pager", "hunter22222hunter")
        .await
        .unwrap();
    let token = auth
        .login("pager", "hunter22222hunter")
        .await
        .unwrap()
        .access_token;
    std::mem::forget(dir);
    (api, token, db, registry, artwork_dir)
}

async fn page(api: &axum::Router, token: &str, uri: &str) -> serde_json::Value {
    let resp = api
        .clone()
        .oneshot(
            Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{uri} -> {}", resp.status());
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
async fn browse_and_matching_use_library_identity_and_guard_copy_revision() {
    let (api, token, db, registry, _) = harness().await;
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
      INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','one','movies'),('host','two','movies');
      INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES
      ('bare','movie','X-Men','x-men',NULL,'host','one'),('dated','movie','X-Men','x-men',2000,'host','two');")
      .execute(&db).await.unwrap();
    let library = registry.create_library("Movies", "movies").await.unwrap();
    registry
        .attach_collection(&library, "host", "one")
        .await
        .unwrap();
    registry
        .attach_collection(&library, "host", "two")
        .await
        .unwrap();
    assert_eq!(
        page(&api, &token, &format!("/api/v1/items?library={library}")).await["total"],
        2
    );
    let detail = page(&api, &token, "/api/v1/items/bare").await;
    let revision = detail["copies"][0]["assignment"]["revision"]
        .as_i64()
        .unwrap();
    let request = serde_json::json!({"action":"pick","expected_revision":revision,"provider":"tmdb","candidate":{"id":36657,"title":"X-Men","release_date":"2000-01-01"}});
    async fn apply(
        api: &axum::Router,
        token: &str,
        body: &serde_json::Value,
    ) -> axum::response::Response {
        api.clone()
            .oneshot(
                Request::post("/admin/v1/collection-items/bare/match")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    let response = apply(&api, &token, &request).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let assignment: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(assignment["library_item_ids"], serde_json::json!(["dated"]));
    assert_eq!(apply(&api, &token, &request).await.status(), 409);
    let result = page(&api, &token, &format!("/api/v1/items?library={library}")).await;
    assert_eq!(result["total"], 1);
    assert_eq!(result["items"][0]["id"], "dated");
    let detail = page(&api, &token, "/api/v1/items/dated").await;
    assert_eq!(detail["copies"].as_array().unwrap().len(), 2);
    let metadata = serde_json::json!({"expected_revision":detail["library_revision"],"fields":{"overview":"Shared work description"}});
    let response = api
        .clone()
        .oneshot(
            Request::put("/admin/v1/library-items/dated/metadata")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(metadata.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        page(&api, &token, "/api/v1/items/dated").await["metadata"]["overview"],
        "Shared work description"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM provider_metadata WHERE overview='Shared work description'"
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "work override must leave provider evidence untouched"
    );

    let reject = serde_json::json!({"action":"reject","expected_revision":assignment["revision"]});
    let response = apply(&api, &token, &reject).await;
    assert_eq!(response.status(), 200);
    let result = page(&api, &token, &format!("/api/v1/items?library={library}")).await;
    assert_eq!(result["total"], 2);
}

#[tokio::test]
async fn subtitle_bodies_keep_the_selected_tracks_source_and_timing() {
    use kahawai_media::subtitles::{Cue, Extracted};
    let (api, token, db, _, artwork) = harness().await;
    let subtitles =
        kahawai_hub::subtitles::Subtitles::new(artwork.parent().unwrap().join("subtitles"));
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES('host','movies','root','/movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    let mut tracks = Vec::new();
    // The larger 25 fps release wins the old collection-level source lookup.
    for (path, size, fps, start, text) in [
        ("Dune-1080.mkv", 2000, [25, 1], 1000, "25 fps release"),
        ("Dune-4k.mkv", 1000, [24000, 1001], 1043, "4K release"),
    ] {
        let info = serde_json::json!({"video":[{"codec":"h264","width":1920,"height":1080,"fps":fps,"interlaced":false}],"subtitles":[{"format":"ass","language":"en"}]});
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
            VALUES('host','movies',(SELECT id FROM collection_roots WHERE root_token='root'),?, ?,1,0,0,0,?) RETURNING id")
            .bind(path).bind(size).bind(info.to_string()).fetch_one(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file, "copy")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let track: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,'embedded',0,'ass','en') RETURNING id")
            .bind(file).fetch_one(&db).await.unwrap();
        let ass = format!("[Script Info]\n; {text}\n");
        subtitles
            .store_extracted(
                "host",
                "movies",
                "root",
                path,
                "e0",
                &Extracted {
                    cues: vec![Cue {
                        start_ms: start,
                        end_ms: start + 1000,
                        text: text.into(),
                    }],
                    ass: Some(ass.clone()),
                },
            )
            .unwrap();
        tracks.push((track, start, text, ass));
    }
    for (track, start, text, ass) in tracks.into_iter().rev() {
        for extension in ["vtt", "ass"] {
            let response = api
                .clone()
                .oneshot(
                    Request::get(format!("/api/v1/items/copy/subtitles/{track}.{extension}"))
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(body.contains(text), "track {track}: {body}");
            if extension == "vtt" {
                assert!(
                    body.contains(&format!("00:00:01.{:03}", start % 1000)),
                    "{body}"
                );
            } else {
                assert_eq!(body, ass);
            }
        }
    }
}

#[tokio::test]
async fn uncached_embedded_and_sidecar_text_read_the_tracks_exact_file() {
    use kahawai_hub::subtitles::{AssBody, Subtitles};
    let (_, _, db, registry, _) = harness().await;
    let media = tempfile::tempdir().unwrap();
    let sessions = kahawai_hub::sessions::Sessions::new(media.path().join("sessions"));
    let media_root = media.path().to_path_buf();
    sessions.set_local_source("host", move |_, _, path| Ok(media_root.join(path)));
    sqlx::raw_sql("INSERT INTO satellites(module_id,module_type,name,cert_fingerprint) VALUES('host','mediahost','host','fp');
        INSERT INTO collections(module_id,collection_id,media_type) VALUES('host','movies','movies');
        INSERT INTO collection_roots(module_id,collection_id,root_token,normalized_path) VALUES('host','movies','root','/movies');
        INSERT INTO collection_items(id,kind,title,norm_title,year,module_id,collection_id) VALUES('copy','movie','Dune','dune',2021,'host','movies');")
        .execute(&db).await.unwrap();
    const HEADER: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n\
        [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Bold\n\
        Style: Default,Arial,48,&H00FFFFFF,0\n\n\
        [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
    let mut selected = Vec::new();
    for (path, text) in [
        ("1080.mkv", "Other release"),
        ("4k.mkv", "Selected release"),
    ] {
        let file_path = media.path().join(path);
        kahawai_media::testutil::render_h264_ass_mkv(
            &file_path,
            HEADER,
            &[(500, 1500, format!("0,0,Default,,0,0,0,,{text}"))],
        );
        let mut info =
            kahawai_media::discover(&file_path, std::time::Duration::from_secs(5)).unwrap();
        let sidecar = format!("{path}.ass");
        std::fs::write(
            media.path().join(&sidecar),
            format!("{HEADER}Dialogue: 0,0:00:00.50,0:00:01.50,Default,,0,0,0,,{text}\n"),
        )
        .unwrap();
        info.external_subtitles
            .push(kahawai_core::media::SidecarSubtitle {
                path_rel: sidecar,
                format: "ass".into(),
                language: Some("en".into()),
                ..Default::default()
            });
        let file: i64 = sqlx::query_scalar("INSERT INTO files(module_id,collection_id,root_id,path_rel,size,mtime_unix,head_xxh3,tail_xxh3,oshash,streams_json)
            VALUES('host','movies',(SELECT id FROM collection_roots WHERE root_token='root'),?, ?,1,0,0,0,?) RETURNING id")
            .bind(path).bind(std::fs::metadata(file_path).unwrap().len() as i64).bind(serde_json::to_string(&info).unwrap()).fetch_one(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        kahawai_hub::registry::bind_file_to_item(&mut tx, file, "copy")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        for origin in ["embedded", "sidecar"] {
            let id: i64 = sqlx::query_scalar("INSERT INTO subtitle_tracks(source_id,origin,stream_index,format,language) VALUES(?,?,0,'ass','en') RETURNING id")
                .bind(file).bind(origin).fetch_one(&db).await.unwrap();
            if path == "4k.mkv" {
                selected.push(
                    kahawai_hub::tracks::get_for_item(&db, "copy", id)
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
        }
    }
    for track in selected {
        for format in ["vtt", "ass", "burn"] {
            // Fresh cache for each delivery exercises the actual lease/sidecar
            // read, rather than a body materialized by the previous assertion.
            let cache = tempfile::tempdir().unwrap();
            let subtitles = Arc::new(Subtitles::new(cache.path().into()));
            let text = match format {
                "vtt" => subtitles
                    .vtt(&registry, &sessions, &track, 0)
                    .await
                    .unwrap(),
                "ass" => match subtitles
                    .ass_body(&registry, &sessions, &track)
                    .await
                    .unwrap()
                {
                    AssBody::Full(text) => text,
                    AssBody::Stream(mut rx) => {
                        let mut text = String::new();
                        while let Some(chunk) = rx.recv().await {
                            text.push_str(&chunk);
                        }
                        text
                    }
                },
                _ => subtitles
                    .ass_for_burn(&registry, &sessions, &track)
                    .await
                    .unwrap(),
            };
            assert!(
                text.contains("Selected release"),
                "{} {format}: {text}",
                track.origin
            );
            assert!(!text.contains("Other release"), "{text}");
        }
    }
}
