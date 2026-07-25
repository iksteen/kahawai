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

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
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
    tx.send(pb::HostToHub {
        msg: Some(pb::host_to_hub::Msg::Hello(pb::Hello {
            protocol_major: kahawai_proto::PROTOCOL_MAJOR,
            protocol_minor: kahawai_proto::PROTOCOL_MINOR,
            name: "nas".into(),
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
    for msg in [
        pb::host_to_hub::Msg::AnnounceCollection(pb::AnnounceCollection {
            id: "movies".into(),
            media_type: "movies".into(),
            roots: vec![root.path().display().to_string()],
        }),
        pb::host_to_hub::Msg::FileUpsert(pb::FileUpsert {
            collection_id: "movies".into(),
            files: vec![pb::FileRecord {
                path_rel: "Heat (1995).mkv".into(),
                size: 65536,
                mtime_unix: 1,
                head_xxh3: 1,
                tail_xxh3: 2,
                oshash: 3,
                streams_json: r#"{"container":"matroska","duration_ms":100000}"#.into(),
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
                tokio::spawn(kahawai_mediahost::serve::serve_lease(
                    serve_channel.clone(),
                    req.lease_token,
                    path,
                ));
            }
        }
    });

    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), pki.path()).await.unwrap());
    let pair = auth
        .complete_setup(&auth.setup_token().unwrap(), "admin", "password-123")
        .await
        .unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let api = test_router(registry.clone(), auth, sessions.clone());
    let get = |uri: String| {
        Request::get(uri).header("authorization", bearer.clone()).body(Body::empty()).unwrap()
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
            let v = body_json(api.clone().oneshot(get("/api/v1/items".into())).await.unwrap()).await;
            if let Some(id) = v["items"].get(0).and_then(|i| i["id"].as_str()) {
                return id.to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();

    // Fresh item: no watch state.
    let v = body_json(api.clone().oneshot(get("/api/v1/items".into())).await.unwrap()).await;
    assert_eq!(v["items"][0]["resume_position_ms"], serde_json::Value::Null);
    assert_eq!(v["items"][0]["played"], false);

    let start_session = || {
        api.clone().oneshot(post(
            "/api/v1/playback/sessions".into(),
            serde_json::json!({"item_id": item_id}),
        ))
    };
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let s1 = body_json(resp).await["session_id"].as_str().unwrap().to_string();

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
    let v = body_json(api.clone().oneshot(get("/api/v1/items".into())).await.unwrap()).await;
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
    let v = body_json(api.clone().oneshot(get(format!("/api/v1/items/{item_id}"))).await.unwrap())
        .await;
    assert_eq!(v["played"], true);
    assert_eq!(v["play_count"], 1);
    assert_eq!(v["resume_position_ms"], 97_000);

    // Per-user cap: second session fine, third refused (limit 2).
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "cap of 2 sessions per user");

    // Idle reaping: stop touching; the janitor ends both within ~2 s.
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
            // NB: polling the stream endpoint touches the session, so
            // back off beyond the idle timeout between checks.
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
    })
    .await
    .expect("idle session was never reaped");

    // Room again after reaping.
    let resp = start_session().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
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
    kahawai_hub::api::router(registry, auth, sessions, enrollments, Arc::new(kahawai_hub::subtitles::Subtitles::new(tempfile::tempdir().unwrap().keep())), Arc::new(kahawai_hub::artwork::Artwork::new(tempfile::tempdir().unwrap().keep(), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())))), Arc::new(kahawai_hub::enrich::Enricher::new(tempfile::tempdir().unwrap().keep())))
}
