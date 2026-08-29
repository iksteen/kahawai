//! Watch state + session lifecycle (HUB-10/18): progress → resume →
//! played/play-count, per-user session caps, idle-session reaping.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::link_service::MediahostLinkService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::Registry;
use kahawai_mediahost::scan::CollectionConfig;
use kahawai_proto::v1 as pb;
use kahawai_transport::identity::SatelliteIdentity;
use tower::ServiceExt;

#[path = "common/catalog.rs"]
mod catalog_fixture;

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn progress_resume_played_caps_and_idle() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Heat (1995).mkv"), vec![7u8; 65536]).unwrap();
    let collections = vec![CollectionConfig {
        name: "movies".into(),
        media_type: "movies".into(),
        roots: vec![root.path().to_path_buf()],
    }];

    let pki = tempfile::tempdir().unwrap();
    let ca = Arc::new(HubCa::load_or_create(pki.path()).unwrap());
    let (cert_pem, key_pem) = ca.issue_server_cert(&["localhost".into()]).unwrap();
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    let tls = kahawai_transport::mtls::mtls_server_config(
        &cert_pem,
        &key_pem,
        ca.ca_cert_pem(),
        allowed.clone(),
    )
    .unwrap();
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), allowed.clone()));
    // Tight limits so this test can see them: 2 sessions/user, 700 ms idle.
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::with_limits(
        tempfile::tempdir().unwrap().keep(),
        2,
        Duration::from_millis(700),
    ));
    sessions.spawn_janitor();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let link_svc = MediahostLinkService::new(
        registry.clone(),
        sessions.clone(),
        std::sync::Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        std::sync::Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(link_svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });

    // Fake host: link, announce, upsert (with duration for the played
    // threshold), serve OpenReads with the real code.
    let bundle = kahawai_core::pki::new_satellite_csr("mediahost", "01HOST", "nas").unwrap();
    let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    allowed.insert(&signed.fingerprint);
    registry
        .record_satellite("01HOST", "mediahost", "nas", &signed.fingerprint)
        .await
        .unwrap();
    let id = SatelliteIdentity {
        module_id: "01HOST".into(),
        key_pem: bundle.key_pem,
        cert_pem: signed.cert_pem,
        ca_pem: ca.ca_cert_pem().to_string(),
    };
    let client_tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub_addr, client_tls)
        .await
        .unwrap();
    let mut client = pb::mediahost_link_client::MediahostLinkClient::new(channel.clone());
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tx.send(pb::HostToHub {
        msg: Some(pb::host_to_hub::Msg::Hello(pb::Hello {
            protocol_major: kahawai_proto::PROTOCOL_MAJOR,
            protocol_minor: kahawai_proto::PROTOCOL_MINOR,
            name: "nas".into(),
            build: String::new(),
            segment_detector_generation: 0,
        })),
    })
    .await
    .unwrap();
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap(); // HelloAck
    catalog_fixture::project_files(
        &tx,
        &mut inbound,
        "movies",
        "movies",
        vec![pb::CollectionRoot::new(
            kahawai_core::media::root_token(root.path()),
            root.path().display().to_string(),
        )],
        vec![pb::FileRecord {
            source: Some(pb::SourcePath::new(
                kahawai_core::media::root_token(root.path()),
                "Heat (1995).mkv",
            )),
            size: 65536,
            mtime_unix: 1,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: r#"{"container":"matroska","duration_ms":100000}"#.into(),
        }],
    )
    .await;
    let serve_channel = channel.clone();
    tokio::spawn(async move {
        while let Ok(Some(m)) = inbound.message().await {
            if let Some(pb::hub_to_host::Msg::OpenRead(req)) = m.msg {
                let path = kahawai_mediahost::serve::resolve_path(&collections, &req);
                tokio::spawn(kahawai_mediahost::serve::serve_lease(
                    serve_channel.clone(),
                    req.lease_token,
                    path,
                ));
            }
        }
    });

    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), pki.path())
            .await
            .unwrap(),
    );
    auth.complete_setup("admin", "password-123").await.unwrap();
    let pair = auth.login("admin", "password-123").await.unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let api = test_router(registry.clone(), auth, sessions.clone());
    let get = |uri: String| {
        Request::get(uri)
            .header("authorization", bearer.clone())
            .body(Body::empty())
            .unwrap()
    };
    let post = |uri: String, body: serde_json::Value| {
        Request::post(uri)
            .header("authorization", bearer.clone())
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Wait for the item.
    let item_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v = body_json(
                api.clone()
                    .oneshot(get("/api/v1/items".into()))
                    .await
                    .unwrap(),
            )
            .await;
            if let Some(id) = v["items"].get(0).and_then(|i| i["id"].as_str()) {
                return id.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();

    // Fresh item: no watch state.
    let v = body_json(
        api.clone()
            .oneshot(get("/api/v1/items".into()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["items"][0]["resume_position_ms"], serde_json::Value::Null);
    assert_eq!(v["items"][0]["played"], false);

    let start_session = || {
        api.clone().oneshot(post(
            "/api/v1/playback/sessions".into(),
            serde_json::json!({"item_id": item_id, "mode": "direct"}),
        ))
    };
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let s1 = body_json(resp).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Progress at 50%: resume position stored, not played.
    let resp = api
        .clone()
        .oneshot(post(
            format!("/api/v1/playback/sessions/{s1}/progress"),
            serde_json::json!({"position_ms": 50_000}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(
        api.clone()
            .oneshot(get("/api/v1/items".into()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["items"][0]["resume_position_ms"], 50_000);
    assert_eq!(v["items"][0]["played"], false);

    // Crossing 90%: played, count 1 — and it doesn't double-count.
    for pos in [95_000, 97_000] {
        let resp = api
            .clone()
            .oneshot(post(
                format!("/api/v1/playback/sessions/{s1}/progress"),
                serde_json::json!({"position_ms": pos}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let v = body_json(
        api.clone()
            .oneshot(get(format!("/api/v1/items/{item_id}")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(v["played"], true);
    assert_eq!(
        v["play_count"], 0,
        "crossing the line is not finishing: the play lands when the watch stops"
    );
    // A finished item answers with no resume position. That is what makes
    // the next Play start it at the start, with no client-side rule about
    // ignoring a position that is nine tenths of the way in.
    assert_eq!(v["resume_position_ms"], serde_json::Value::Null);
    // The hub still knows where the viewer was, though — a re-dispatched
    // transcode resumes the stream from it (AR-6).
    let stored: i64 = sqlx::query_scalar("SELECT position_ms FROM watch_state WHERE item_id = ?")
        .bind(&item_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        stored, 97_000,
        "the playhead is kept, it is just not offered"
    );

    let progress = |pos: i64| {
        api.clone().oneshot(post(
            format!("/api/v1/playback/sessions/{s1}/progress"),
            serde_json::json!({ "position_ms": pos }),
        ))
    };

    // A report at ZERO is not a statement that this has not been seen, and
    // one arrives for things nobody has touched: the audio queue pings
    // zero every ten seconds for the track it has preloaded, and the video
    // player answers with the resume position — absent on a played item —
    // until the element has its metadata. Clearing the mark on those wiped
    // the seen ticks a row ahead of the playhead through an album already
    // heard.
    sqlx::query("UPDATE watch_state SET updated_at = 123 WHERE item_id = ?")
        .bind(&item_id)
        .execute(&db)
        .await
        .unwrap();
    let resp = progress(0).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(
        v["played"], true,
        "a ping from a standing start does not unwatch anything"
    );
    let watched_at: i64 =
        sqlx::query_scalar("SELECT updated_at FROM watch_state WHERE item_id = ?")
            .bind(&item_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        watched_at, 123,
        "a zero ping must not make an old finish look newly watched"
    );

    // Scrubbing back over the line and forward again. `played` is a
    // boolean, not a high-water mark, so it follows the playhead both
    // ways once the playhead has actually moved — and none of it counts a
    // play, because the watch has not stopped. Counting the crossing made
    // this one viewing count twice, which is why the count moved to
    // teardown.
    let resp = progress(1_000).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(
        v["played"], false,
        "starting it again is not having seen it"
    );
    assert_eq!(v["play_count"], 0, "and nothing has been counted yet");

    // Back past the line: played again, still nothing counted.
    let resp = progress(96_000).await.unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["played"], true);
    assert_eq!(
        v["play_count"], 0,
        "one watch, however often the line moves"
    );

    // Per-user cap: second session fine, third refused (limit 2).
    //
    // 429 and `session_cap`, not the 409 every other refusal from this
    // endpoint carries. The cap clears the moment a session ends and the
    // item's own refusals never do; both used to arrive as 409 with the
    // difference in the prose, so a client playing a queue had to guess.
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = start_session().await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "cap of 2 sessions per user"
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    let refusal: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(refusal["code"], "session_cap");

    // A live session still answers 404 for a sub-resource that does not
    // exist. AUTH-11 deliberately spends that distinction: session absence
    // must be indistinguishable from a foreign live id, so clients treat a
    // missing session resource as a reason to renegotiate.
    let resp = api
        .clone()
        .oneshot(get(format!(
            "/api/v1/playback/sessions/{s1}/subs-999999.ass"
        )))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a missing session sub-resource is hidden"
    );

    // Idle reaping: stop touching; the janitor ends both within ~2 s.
    // The reaped session answers 404 — the one signal a client needs to
    // know it should start a new session at its current position.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let resp = api
                .clone()
                .oneshot(get(format!("/api/v1/playback/sessions/{s1}/stream")))
                .await
                .unwrap();
            if resp.status() == StatusCode::NOT_FOUND {
                return;
            }
            assert_ne!(
                resp.status(),
                StatusCode::GONE,
                "session absence must not reveal a distinct gone state"
            );
            // NB: polling the stream endpoint touches the session, so
            // back off beyond the idle timeout between checks.
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
    })
    .await
    .expect("idle session was never reaped");

    // Stopping is what counts the play, and being reaped IS stopping —
    // it is what a closed laptop looks like from here. ONE play, though
    // the 90 percent line was crossed twice. The janitor owns this teardown,
    // so observe its durable write rather than only the session disappearing.
    let count = || {
        let api = api.clone();
        let item_id = item_id.clone();
        async move {
            body_json(
                api.oneshot(get(format!("/api/v1/items/{item_id}")))
                    .await
                    .unwrap(),
            )
            .await["play_count"]
                .clone()
        }
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = count().await;
            if n == 1 {
                return;
            }
            assert_eq!(n, 0, "a watch counts once, at its end");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the finished watch was never counted");

    // Room again after reaping.
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // And the OTHER session reaped above — the one from the cap check,
    // on this same item, which never reported a position — added
    // nothing. A session that played nothing must not count the play a
    // previous watch left marked.
    assert_eq!(
        count().await,
        1,
        "a session that watched nothing is not a play"
    );

    // Two more finished watches. Ended in this order deliberately: the
    // one that must NOT count goes last, so anything it writes has to
    // appear after a total that is already settled.
    let del = |uri: String| {
        api.clone().oneshot(
            Request::delete(uri)
                .header("authorization", bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
    };
    let finish = |id: &str| {
        let id = id.to_string();
        let api = api.clone();
        let bearer = bearer.clone();
        async move {
            let resp = api
                .oneshot(
                    Request::post(format!("/api/v1/playback/sessions/{id}/progress"))
                        .header("authorization", bearer)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({ "position_ms": 97_000 }).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(body_json(resp).await["played"], true);
        }
    };
    let session_id = |v: &serde_json::Value| v["session_id"].as_str().unwrap().to_string();

    // A watch that takes the item past the line itself, stopped by the
    // viewer: a play, exactly as the reaped one was.
    let watched = session_id(&body_json(resp).await);
    finish(&watched).await;
    assert_eq!(
        del(format!("/api/v1/playback/sessions/{watched}"))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        count().await,
        2,
        "the DELETE response waits until the finished watch is durable"
    );

    // A session that OPENS past the line is a continuation, not a watch.
    // It is what the client starts after losing one — `recovery.ts` picks
    // the position back up — and it ends past the line like the session
    // before it, so counting on that alone counted one sitting twice.
    let resp = api
        .clone()
        .oneshot(post(
            "/api/v1/playback/sessions".into(),
            serde_json::json!({"item_id": item_id, "mode": "direct", "start_ms": 96_000}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resumed = session_id(&body_json(resp).await);
    finish(&resumed).await;
    assert_eq!(
        del(format!("/api/v1/playback/sessions/{resumed}"))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    // A bounded wait, because what is being asserted is an absence — but
    // a tight one: the two increments above are written by the same
    // machinery and both landed well inside it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        count().await,
        2,
        "a session that never crossed the line must not count the play the one that did already earned"
    );
}

/// Router with default admin plumbing for tests that don't exercise it.
fn test_router(
    registry: std::sync::Arc<kahawai_hub::registry::Registry>,
    auth: std::sync::Arc<kahawai_hub::auth::Auth>,
    sessions: std::sync::Arc<kahawai_hub::sessions::Sessions>,
) -> axum::Router {
    let ca = std::sync::Arc::new(
        kahawai_hub::pki::HubCa::load_or_create(tempfile::tempdir().unwrap().keep().as_path())
            .unwrap(),
    );
    let enrollments = std::sync::Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        std::time::Duration::from_secs(900),
        90,
    ));
    kahawai_hub::api::router(
        registry,
        auth,
        sessions,
        enrollments,
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            Arc::new(kahawai_hub::enrich::Enricher::new(
                tempfile::tempdir().unwrap().keep(),
            )),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::segments::Detector::new()),
        kahawai_hub::api::NetOptions::default(),
    )
}
