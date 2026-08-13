//! A hub with a REAL mediahost on the other end of a real mTLS link,
//! serving a real media file.
//!
//! Session tests need this and nothing less: starting a session opens a
//! lease and reads bytes, so a registry row claiming a host is
//! connected is not enough. Declaration-only fixtures (see
//! `item_query.rs`) can answer questions ABOUT playing; only this can
//! actually play.
//!
//! Lifted from `remux_play.rs`, which still carries its own inline
//! copy: rewriting a working 300-line end-to-end test to route through
//! here buys nothing but tidiness and risks the one test that proves
//! the whole remux path. Next test that needs a live mediahost uses
//! this; `remux_play` moves over when it is being changed anyway.

#![allow(dead_code)] // each test binary uses its own subset

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use kahawai_hub::link_service::MediahostLinkService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::Registry;
use kahawai_mediahost::scan::CollectionConfig;
use kahawai_proto::v1 as pb;
use kahawai_transport::identity::SatelliteIdentity;
use tower::ServiceExt;

pub struct Harness {
    pub api: axum::Router,
    pub bearer: String,
    pub item_id: String,
    pub registry: Arc<Registry>,
    pub sessions: Arc<kahawai_hub::sessions::Sessions>,
    pub db: sqlx::SqlitePool,
    /// Held so the collection root and PKI outlive the test.
    _root: tempfile::TempDir,
    _pki: tempfile::TempDir,
}

pub async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec()
}

pub async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

/// `render` writes the media file; the probe that follows is the real
/// one, so the declarations the hub negotiates against describe the
/// bytes that are actually there.
pub async fn harness(file_name: &str, render: fn(&Path)) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join(file_name);
    let media2 = media.clone();
    tokio::task::spawn_blocking(move || render(&media2))
        .await
        .unwrap();
    let size = std::fs::metadata(&media).unwrap().len();
    let info = tokio::task::spawn_blocking({
        let media = media.clone();
        move || kahawai_media::discover(&media, Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    let streams_json = serde_json::to_string(&info).unwrap();
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
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let subtitles = Arc::new(kahawai_hub::subtitles::Subtitles::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let enricher = Arc::new(kahawai_hub::enrich::Enricher::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let link_svc = MediahostLinkService::new(
        registry.clone(),
        sessions.clone(),
        subtitles.clone(),
        enricher.clone(),
    );
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(link_svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });

    // The mediahost side: enroll, link, announce, upsert, and answer
    // OpenRead with the real serve code.
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
    tx.send(pb::HostToHub {
        msg: Some(pb::host_to_hub::Msg::Hello(pb::Hello {
            protocol_major: kahawai_proto::PROTOCOL_MAJOR,
            protocol_minor: kahawai_proto::PROTOCOL_MINOR,
            name: "nas".into(),
            build: String::new(),
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
                root_token: String::new(),
                path_rel: file_name.into(),
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
    // The sender must outlive the link or the stream closes under it.
    std::mem::forget(tx);

    let auth = Arc::new(
        kahawai_hub::auth::Auth::new(db.clone(), pki.path())
            .await
            .unwrap(),
    );
    let pair = auth
        .complete_setup(&auth.setup_token().unwrap(), "admin", "password-123")
        .await
        .unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let enrollments = Arc::new(kahawai_hub::enrollment_service::EnrollmentService::new(
        ca,
        registry.clone(),
        Duration::from_secs(900),
        90,
    ));
    let api = kahawai_hub::api::router(
        registry.clone(),
        auth,
        sessions.clone(),
        enrollments,
        subtitles,
        Arc::new(kahawai_hub::artwork::Artwork::new(
            tempfile::tempdir().unwrap().keep(),
            enricher.clone(),
        )),
        enricher,
        kahawai_hub::api::NetOptions::default(),
    );

    // The upsert crosses the link asynchronously; wait for the item.
    let item_id = tokio::time::timeout(Duration::from_secs(10), {
        let api = api.clone();
        let bearer = bearer.clone();
        async move {
            loop {
                let resp = api
                    .clone()
                    .oneshot(
                        Request::get("/api/v1/items")
                            .header("authorization", &bearer)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let v = json_body(resp).await;
                if let Some(id) = v["items"].get(0).and_then(|i| i["id"].as_str()) {
                    return id.to_string();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    })
    .await
    .expect("item never resolved");

    Harness {
        api,
        bearer,
        item_id,
        registry,
        sessions,
        db,
        _root: root,
        _pki: pki,
    }
}
