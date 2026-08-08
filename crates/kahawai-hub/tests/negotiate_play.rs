//! HUB-14/HUB-16: the hub negotiates when the client sends no mode. A
//! two-source item (mp4 + mkv, same movie, real files served by the
//! real mediahost code) proves the cheapest sufficient path wins, the
//! fallback profile preserves the historical behavior, and the user's
//! bandwidth cap bites.

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
        kahawai_hub::api::NetOptions::default(),
    )
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec()
}

/// An API error is `(StatusCode, String)`, not JSON. Parsing it with
/// `unwrap` reports "expected ident" and throws the server's actual
/// message away — which is the whole diagnosis.
fn json(bytes: Vec<u8>) -> serde_json::Value {
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        panic!(
            "server did not answer JSON: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

#[tokio::test]
async fn negotiation_picks_cheapest_source_and_honors_caps() {
    if !kahawai_media::testutil::require_h264_aac_fixture() {
        return;
    }
    // Two real files of the same movie: an MSE-friendly mp4 and an MKV.
    let root = tempfile::tempdir().unwrap();
    let mp4 = root.path().join("Heat (1995).mp4");
    let mkv = root.path().join("Heat (1995).mkv");
    {
        let (mp4, mkv) = (mp4.clone(), mkv.clone());
        tokio::task::spawn_blocking(move || {
            kahawai_media::testutil::render_h264_aac_mp4(&mp4);
            kahawai_media::testutil::render_h264_aac_mkv(&mkv);
        })
        .await
        .unwrap();
    }
    let file_facts = |p: &std::path::Path| {
        let size = std::fs::metadata(p).unwrap().len();
        let info = kahawai_media::discover(p, Duration::from_secs(15)).unwrap();
        (size, serde_json::to_string(&info).unwrap())
    };
    let (mp4_facts, mkv_facts) = {
        let (mp4, mkv) = (mp4.clone(), mkv.clone());
        tokio::task::spawn_blocking(move || (file_facts(&mp4), file_facts(&mkv)))
            .await
            .unwrap()
    };
    let collections = vec![CollectionConfig {
        name: "movies".into(),
        media_type: "movies".into(),
        roots: vec![root.path().to_path_buf()],
    }];

    // Hub + fake host link (the remux_play harness shape).
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
        Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        Arc::new(kahawai_hub::enrich::Enricher::new(
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
            files: vec![
                pb::FileRecord {
                    path_rel: "Heat (1995).mp4".into(),
                    size: mp4_facts.0,
                    mtime_unix: 1,
                    head_xxh3: 1,
                    tail_xxh3: 2,
                    oshash: 3,
                    streams_json: mp4_facts.1.clone(),
                },
                pb::FileRecord {
                    path_rel: "Heat (1995).mkv".into(),
                    size: mkv_facts.0,
                    mtime_unix: 1,
                    head_xxh3: 4,
                    tail_xxh3: 5,
                    oshash: 6,
                    streams_json: mkv_facts.1.clone(),
                },
            ],
        }),
    ] {
        tx.send(pb::HostToHub { msg: Some(msg) }).await.unwrap();
    }
    // Heartbeats, like a real mediahost. Without them the hub's 35 s
    // liveness timeout fires mid-test and every later play answers
    // "no source is currently available (mediahost offline)" — a
    // latent bug that only shows once the test runs longer than 35 s,
    // which it now does (it performs a real AV1 encode first).
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                if tx
                    .send(pb::HostToHub {
                        msg: Some(pb::host_to_hub::Msg::Heartbeat(pb::Heartbeat {})),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
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
    let api = test_router(registry.clone(), auth, sessions.clone());

    // Wait for the ONE item with BOTH sources.
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
            let v: serde_json::Value = json(body_bytes(resp).await);
            if let Some(item) = v["items"].get(0)
                && item["sources"] == 2
            {
                return item["id"].as_str().unwrap().to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("two-source item never resolved");

    let start = |body: serde_json::Value| {
        let api = api.clone();
        let bearer = bearer.clone();
        async move {
            let resp = api
                .oneshot(
                    Request::post("/api/v1/playback/sessions")
                        .header("authorization", bearer)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let v: serde_json::Value = json(body_bytes(resp).await);
            (status, v)
        }
    };
    let end = |v: &serde_json::Value| {
        let api = api.clone();
        let bearer = bearer.clone();
        let sid = v["session_id"].as_str().unwrap().to_string();
        async move {
            api.oneshot(
                Request::delete(format!("/api/v1/playback/sessions/{sid}"))
                    .header("authorization", bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        }
    };

    // 1. No mode, no profile: the fallback negotiates — the mp4 DIRECT
    //    beats the rank-preferred MKV's remux (HUB-16 source choice).
    let (status, v) = start(serde_json::json!({ "item_id": item_id })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["mode"], "direct", "cheapest source must win: {v}");
    assert_eq!(
        v["content_type"], "video/mp4",
        "the mp4 source was the direct one"
    );
    end(&v).await;

    // 2. A profile that also demuxes matroska: the MKV direct-plays too,
    //    and rank (height/revision/size) breaks the all-direct tie.
    let (status, v) = start(serde_json::json!({
        "item_id": item_id,
        "profile": {
            "containers": ["mp4", "webm", "matroska"],
            "video": [{"codec": "h264"}],
            "audio": ["aac", "mp3"],
            "target_duration": {"mode": "ignore"}
        }
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["mode"], "direct", "{v}");
    end(&v).await;

    // 3. A standing user bandwidth cap kills direct and clamps the
    //    encode; the verdict says why.
    sqlx::query(
        "INSERT INTO user_prefs (user_id, scope, key, value)
         SELECT id, '', 'bandwidth_kbps', '1' FROM users",
    )
    .execute(&db)
    .await
    .unwrap();
    let (status, v) = start(serde_json::json!({ "item_id": item_id })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["mode"], "remux", "cap must forbid direct: {v}");
    let video_verdict = v["streams"]["video"].as_str().unwrap();
    assert!(
        video_verdict.contains("bandwidth cap"),
        "verdict: {video_verdict}"
    );
    assert!(
        v["streams"]["subtitles"].is_array(),
        "negotiated sessions carry sub verdicts"
    );
    end(&v).await;
}
