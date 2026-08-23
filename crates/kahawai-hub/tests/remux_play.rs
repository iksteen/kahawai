//! End-to-end in-hub remux (AR-10, M2): a real MKV is served by the real
//! mediahost code, repackaged to HLS/TS inside the hub with no transcoder,
//! and the playlist + segments come out of the session API.

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

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn remux_to_hls_end_to_end() {
    if !kahawai_media::testutil::require_h264_aac_fixture() {
        return;
    }
    // A real MKV (h264 + AAC) in a collection root.
    let root = tempfile::tempdir().unwrap();
    let mkv = root.path().join("Heat (1995).mkv");
    let mkv2 = mkv.clone();
    tokio::task::spawn_blocking(move || kahawai_media::testutil::render_h264_aac_mkv(&mkv2))
        .await
        .unwrap();
    let size = std::fs::metadata(&mkv).unwrap().len();
    let info = tokio::task::spawn_blocking({
        let mkv = mkv.clone();
        move || kahawai_media::discover(&mkv, Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    let streams_json = serde_json::to_string(&info).unwrap();
    let collections = vec![CollectionConfig {
        name: "movies".into(),
        media_type: "movies".into(),
        roots: vec![root.path().to_path_buf()],
    }];

    // Hub.
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
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
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

    // Fake-host: link, announce, upsert, answer OpenReads with real code.
    let bundle = kahawai_core::pki::new_satellite_csr("mediahost", "01HOST", "nas").unwrap();
    let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    allowed.insert(&signed.fingerprint);
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
    for msg in [pb::host_to_hub::Msg::Hello(pb::Hello {
        protocol_major: kahawai_proto::PROTOCOL_MAJOR,
        protocol_minor: kahawai_proto::PROTOCOL_MINOR,
        name: "nas".into(),
        build: String::new(),
        segment_detector_generation: 0,
    })] {
        tx.send(pb::HostToHub { msg: Some(msg) }).await.unwrap();
    }
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap(); // HelloAck
    for msg in [
        pb::host_to_hub::Msg::AnnounceCollection(pb::AnnounceCollection {
            id: "movies".into(),
            media_type: "movies".into(),
            roots: vec![pb::CollectionRoot::new(
                kahawai_core::media::root_token(root.path()),
                root.path().display().to_string(),
            )],
        }),
        pb::host_to_hub::Msg::FileUpsert(pb::FileUpsert {
            collection_id: "movies".into(),
            files: vec![pb::FileRecord {
                source: Some(pb::SourcePath::new(
                    kahawai_core::media::root_token(root.path()),
                    "Heat (1995).mkv",
                )),
                size,
                mtime_unix: 1,
                head_xxh3: 1,
                tail_xxh3: 2,
                oshash: 3,
                streams_json,
            }],
        }),
    ] {
        tx.send(pb::HostToHub { msg: Some(msg) }).await.unwrap();
    }
    let serve_channel = channel.clone();
    tokio::spawn(async move {
        while let Ok(Some(m)) = inbound.message().await {
            if let Some(pb::hub_to_host::Msg::OpenRead(req)) = m.msg {
                let path = kahawai_mediahost::serve::resolve_path(&collections, &req);
                let ch = serve_channel.clone();
                tokio::spawn(kahawai_mediahost::serve::serve_lease(
                    ch,
                    req.lease_token,
                    path,
                ));
            }
        }
    });

    // API with auth.
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

    // Wait for item, then start a REMUX session.
    let item_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let resp = api
                .clone()
                .oneshot(get("/api/v1/items".into()))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
            if let Some(id) = v["items"].get(0).and_then(|i| i["id"].as_str()) {
                return id.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("item never resolved");

    let resp = api
        .clone()
        .oneshot(
            Request::post("/api/v1/playback/sessions")
                .header("authorization", bearer.clone())
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    "{{\"item_id\":\"{item_id}\",\"mode\":\"remux\"}}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["mode"], "remux");
    assert_eq!(v["content_type"], "application/vnd.apple.mpegurl");
    let stream_url = v["stream_url"].as_str().unwrap().to_string();
    let session_id = v["session_id"].as_str().unwrap().to_string();
    assert!(stream_url.ends_with("/master.m3u8"));

    // The playlist finalizes quickly for a 10 s source; poll for ENDLIST.
    let playlist = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let resp = api.clone().oneshot(get(stream_url.clone())).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let text = String::from_utf8(body_bytes(resp).await).unwrap();
            if text.contains("#EXT-X-ENDLIST") {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("playlist never finalized");
    assert!(
        playlist.contains("segment00000.ts"),
        "playlist:\n{playlist}"
    );

    // Fetch the first segment: real TS bytes.
    let resp = api
        .clone()
        .oneshot(get(format!(
            "/api/v1/playback/sessions/{session_id}/segment00000.ts"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "video/mp2t");
    let seg = body_bytes(resp).await;
    assert!(seg.len() > 10_000, "segment too small: {}", seg.len());
    assert_eq!(seg[0], 0x47, "TS sync byte");

    // Traversal and junk names are rejected.
    let resp = api
        .clone()
        .oneshot(get(format!(
            "/api/v1/playback/sessions/{session_id}/..%2Fescape.ts"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A SCRUB: four concurrent seeks on one session. They serialize on
    // the session's seek lock — interleaved restarts used to wipe each
    // other's scratch dir mid-bind (intermittent 409s in the wild).
    let seek_to = |ms: u64| {
        let api = api.clone();
        let bearer = bearer.clone();
        let session_id = session_id.clone();
        async move {
            api.oneshot(
                Request::post(format!("/api/v1/playback/sessions/{session_id}/seek"))
                    .header("authorization", bearer)
                    .header("content-type", "application/json")
                    .body(Body::from(format!("{{\"position_ms\":{ms}}}")))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    let statuses = tokio::join!(seek_to(1000), seek_to(2500), seek_to(4000), seek_to(6000));
    let statuses = [statuses.0, statuses.1, statuses.2, statuses.3];
    assert!(
        statuses.iter().all(|s| *s == StatusCode::OK),
        "every concurrent seek must succeed: {statuses:?}"
    );
    let tail_playlist = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let resp = api
                .clone()
                .oneshot(get(format!(
                    "/api/v1/playback/sessions/{session_id}/master.m3u8"
                )))
                .await
                .unwrap();
            if resp.status() == StatusCode::OK {
                let text = String::from_utf8(body_bytes(resp).await).unwrap();
                if text.contains("#EXT-X-ENDLIST") {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("post-seek playlist never finalized");
    let total: f64 = tail_playlist
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .sum();
    assert!(
        total > 2.0 && total < 6.5,
        "post-seek playlist should cover ~4s tail, got {total}s:\n{tail_playlist}"
    );

    // Teardown removes the scratch files.
    let resp = api
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/playback/sessions/{session_id}"))
                .header("authorization", bearer.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = api.clone().oneshot(get(stream_url)).await.unwrap();
    // The playlist of an ended session is 404: hls.js reads the status off
    // the failed load and the player restarts.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
