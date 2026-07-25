//! End-to-end direct play (AR-10, M2): a mediahost serves file bytes over
//! the ByteChannel and the hub proxies them to an authenticated client with
//! byte-range support. The mediahost side runs the real `serve` code.

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

const FILE_LEN: usize = 5 * 1024 * 1024 + 123;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap().to_vec()
}

#[tokio::test]
async fn direct_play_ranges_end_to_end() {
    // Media root with one known file.
    let root = tempfile::tempdir().unwrap();
    let content = pattern(FILE_LEN);
    std::fs::write(root.path().join("Heat (1995).mkv"), &content).unwrap();
    let collections = vec![CollectionConfig {
        name: "movies".into(),
        media_type: "movies".into(),
        roots: vec![root.path().to_path_buf()],
    }];

    // Hub: link service + sessions + API.
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
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep()));
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

    // Fake-host side: enroll, link, announce, upsert the real file's record,
    // then answer OpenReads with the REAL serve code.
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
    let channel = kahawai_transport::tls::grpc_channel_with(&hub_addr, client_tls).await.unwrap();
    let mut client = pb::mediahost_link_client::MediahostLinkClient::new(channel.clone());
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let send = |msg: pb::host_to_hub::Msg| {
        let tx = tx.clone();
        async move { tx.send(pb::HostToHub { msg: Some(msg) }).await.unwrap() }
    };
    send(pb::host_to_hub::Msg::Hello(pb::Hello {
        protocol_major: kahawai_proto::PROTOCOL_MAJOR,
        protocol_minor: kahawai_proto::PROTOCOL_MINOR,
        name: "nas".into(),
    }))
    .await;
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap(); // HelloAck
    send(pb::host_to_hub::Msg::AnnounceCollection(pb::AnnounceCollection {
        id: "movies".into(),
        media_type: "movies".into(),
        roots: vec![root.path().display().to_string()],
    }))
    .await;
    send(pb::host_to_hub::Msg::FileUpsert(pb::FileUpsert {
        collection_id: "movies".into(),
        files: vec![pb::FileRecord {
            path_rel: "Heat (1995).mkv".into(),
            size: FILE_LEN as u64,
            mtime_unix: 1,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: r#"{"container":"matroska"}"#.into(),
        }],
    }))
    .await;
    // OpenRead responder — the real mediahost serving path.
    let serve_channel = channel.clone();
    let serve_collections = collections.clone();
    tokio::spawn(async move {
        while let Ok(Some(m)) = inbound.message().await {
            if let Some(pb::hub_to_host::Msg::OpenRead(req)) = m.msg {
                let path = kahawai_mediahost::serve::resolve_path(&serve_collections, &req);
                let ch = serve_channel.clone();
                tokio::spawn(kahawai_mediahost::serve::serve_lease(ch, req.lease_token, path));
            }
        }
    });

    // API with auth completed.
    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), pki.path()).await.unwrap(),
    );
    let pair = auth
        .complete_setup(&auth.setup_token().unwrap(), "admin", "password-123")
        .await
        .unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let api = test_router(registry.clone(), auth, sessions.clone());

    // Wait for the item to resolve.
    let item_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let resp = api
                .clone()
                .oneshot(
                    Request::get("/api/v1/items")
                        .header("authorization", bearer.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let v: serde_json::Value =
                serde_json::from_slice(&body_bytes(resp).await).unwrap();
            if let Some(id) = v["items"].get(0).and_then(|i| i["id"].as_str()) {
                return id.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("item never resolved");

    // Start a session.
    let resp = api
        .clone()
        .oneshot(
            Request::post("/api/v1/playback/sessions")
                .header("authorization", bearer.clone())
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"item_id\":\"{item_id}\"}}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(v["mode"], "direct");
    assert_eq!(v["content_type"], "video/x-matroska");
    assert_eq!(v["size"], FILE_LEN as u64);
    let stream_url = v["stream_url"].as_str().unwrap().to_string();
    let session_id = v["session_id"].as_str().unwrap().to_string();

    let get = |range: Option<&str>| {
        let mut req = Request::get(&stream_url).header("authorization", bearer.clone());
        if let Some(r) = range {
            req = req.header("range", r);
        }
        api.clone().oneshot(req.body(Body::empty()).unwrap())
    };

    // Full body.
    let resp = get(None).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    assert_eq!(resp.headers()["content-type"], "video/x-matroska");
    assert_eq!(body_bytes(resp).await, content);

    // Interior range crossing the 4 MiB block boundary.
    let resp = get(Some("bytes=4194000-4194999")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers()["content-range"],
        format!("bytes 4194000-4194999/{FILE_LEN}")
    );
    assert_eq!(body_bytes(resp).await, content[4194000..4195000]);

    // Open-ended and suffix ranges (what players actually send).
    let resp = get(Some(&format!("bytes={}-", FILE_LEN - 100))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(resp).await, content[FILE_LEN - 100..]);
    let resp = get(Some("bytes=-123")).await.unwrap();
    assert_eq!(body_bytes(resp).await, content[FILE_LEN - 123..]);

    // Unsatisfiable.
    let resp = get(Some(&format!("bytes={FILE_LEN}-"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // End the session; the stream endpoint forgets it.
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
    let resp = get(None).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Kill the link: new sessions must fail with "no source available".
    drop(tx);
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.is_connected("01HOST") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let resp = api
        .clone()
        .oneshot(
            Request::post("/api/v1/playback/sessions")
                .header("authorization", bearer)
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"item_id\":\"{item_id}\"}}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
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
    kahawai_hub::api::router(registry, auth, sessions, enrollments, Arc::new(kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep())), Arc::new(kahawai_hub::artwork::Artwork::new(tempfile::tempdir().unwrap().keep(), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())))), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())), kahawai_hub::api::NetOptions::default())
}
