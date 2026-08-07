//! End-to-end session dispatch (M3, §4.5): a source whose audio needs
//! encoding is placed on a connected transcoder (real kahawai-transcoder
//! link code, in-process worker), source bytes flow hub→transcoder over
//! the control link, and the playlist + AAC segments come back through
//! the hub's artifact proxy.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kahawai_hub::link_service::MediahostLinkService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::Registry;
use kahawai_hub::transcoder_link::TranscoderLinkService;
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

fn enroll(
    ca: &HubCa,
    allowed: &kahawai_transport::mtls::AllowedCerts,
    module_type: &str,
    module_id: &str,
    name: &str,
) -> SatelliteIdentity {
    let bundle = kahawai_core::pki::new_satellite_csr(module_type, module_id, name).unwrap();
    let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    allowed.insert(&signed.fingerprint);
    SatelliteIdentity {
        module_id: module_id.into(),
        key_pem: bundle.key_pem,
        cert_pem: signed.cert_pem,
        ca_pem: ca.ca_cert_pem().to_string(),
    }
}

/// A TCP relay the test can cut, standing in for a transcoder box that
/// dies. `JoinHandle::abort` is not that: it drops the future, but the
/// connection stays open, so the hub sees a silent-but-live stream and
/// waits out its 35 s liveness timeout — a PARTITION, which is a real
/// failure mode but not the one this scenario claims. A process that
/// dies closes its sockets and the hub notices at once. Cutting the
/// relay closes them, which is both faithful and 33 s cheaper on every
/// run of the suite.
///
/// The hub's cert is issued for `localhost`, so relaying through a
/// second `localhost` port still validates.
fn cuttable_relay(target: String) -> (String, tokio::sync::broadcast::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let (cut, _) = tokio::sync::broadcast::channel::<()>(1);
    let cut2 = cut.clone();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        while let Ok((mut inbound, _)) = listener.accept().await {
            let target = target.clone();
            let mut cut = cut2.subscribe();
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else {
                    return;
                };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                    // Both halves drop here: FIN on each, exactly as a
                    // dying process would send.
                    _ = cut.recv() => {}
                }
            });
        }
    });
    (addr, cut)
}

#[tokio::test]
async fn dispatches_encode_session_to_transcoder() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,kahawai_hub=debug")
        .try_init();
    if kahawai_media::remux::aac_encoder().is_none() {
        eprintln!("skipping: no verified AAC encoder");
        return;
    }

    // Fixture: h264 + FLAC — the web target plans audio as Encode.
    let root = tempfile::tempdir().unwrap();
    let mkv = root.path().join("Concert (2020).mkv");
    let mkv2 = mkv.clone();
    tokio::task::spawn_blocking(move || kahawai_media::testutil::render_h264_flac_mkv(&mkv2))
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

    // Hub with both link services.
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
    sessions.attach_registry(registry.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let mh_svc = MediahostLinkService::new(
        registry.clone(),
        sessions.clone(),
        std::sync::Arc::new(kahawai_hub::subtitles::Subtitles::new(
            tempfile::tempdir().unwrap().keep(),
        )),
        std::sync::Arc::new(kahawai_hub::enrich::Enricher::new(
            tempfile::tempdir().unwrap().keep(),
        )),
    );
    let tc_svc = TranscoderLinkService::new(registry.clone(), sessions.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(mh_svc.into_server())
            .add_service(tc_svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });

    // Fake mediahost: link, announce, upsert, serve OpenReads.
    let id = enroll(&ca, &allowed, "mediahost", "01HOST", "nas");
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
                path_rel: "Concert (2020).mkv".into(),
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
    // Heartbeats, like a real mediahost: without them the hub's 35 s
    // liveness timeout drops this link partway through the test.
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

    // Real transcoder link (in-process worker), aac-only capability.
    let tc_id = enroll(&ca, &allowed, "transcoder", "01TC", "encoder-box");
    let tc_tls = kahawai_transport::mtls::mtls_client_config(&tc_id).unwrap();
    let tc_scratch = tempfile::tempdir().unwrap();
    // tc1 reaches the hub through a relay the test can cut, so its
    // "death" later is a real one (see cuttable_relay).
    let (tc1_addr, cut_tc1) =
        cuttable_relay(hub_addr.replace("localhost:", "127.0.0.1:").to_string());
    let hub_addr2 = tc1_addr;
    let scratch_path = tc_scratch.path().join("sessions");
    let tc1_task = tokio::spawn(async move {
        let caps = pb::CapabilityReport {
            encoders: vec![pb::EncoderCap {
                codec: "aac".into(),
                element: "fdkaacenc".into(),
                hardware: false,
                speed_1080: None,
                speed_2160: None,
            }],
            max_sessions: 2,
            decode_caps: vec![], // empty = assume capable (OPS-7)
            tonemap: false,
            tonemap_speed_1080: None,
            tonemap_speed_2160: None,
            ass_burn: false,
        };
        let _ = kahawai_transcoder::link_once(
            &hub_addr2,
            tc_tls,
            "encoder-box",
            tokio::sync::watch::channel(caps).1,
            &scratch_path,
            &None,
        )
        .await;
    });

    // API.
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
    let get = |uri: String| {
        Request::get(uri)
            .header("authorization", bearer.clone())
            .body(Body::empty())
            .unwrap()
    };

    // Wait for the item AND the transcoder registration.
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
    tokio::time::timeout(Duration::from_secs(10), async {
        while registry
            .pick_transcoder(&kahawai_hub::registry::PlacementNeed {
                encode_audio: true,
                audio_caps: vec!["audio/x-flac".into()],
                ..Default::default()
            })
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("transcoder never registered");

    // Start a remux session: audio Encode → dispatched.
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
    assert_eq!(v["mode"], "transcode", "session should dispatch: {v}");
    assert_eq!(v["streams"]["audio"], "flac → aac (transcoded)");
    let stream_url = v["stream_url"].as_str().unwrap().to_string();
    let session_id = v["session_id"].as_str().unwrap().to_string();

    // Playlist arrives through the artifact proxy; wait for ENDLIST.
    let playlist = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let resp = api.clone().oneshot(get(stream_url.clone())).await.unwrap();
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
    .expect("playlist never finalized");
    assert!(
        playlist.contains("segment00000.ts"),
        "playlist:\n{playlist}"
    );

    // First segment: real TS bytes, audio transcoded to AAC.
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
    let seg_path = root.path().join("seg0.ts");
    std::fs::write(&seg_path, &seg).unwrap();
    let seg_info = tokio::task::spawn_blocking(move || {
        kahawai_media::discover(&seg_path, Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        seg_info.audio.first().map(|a| a.codec.as_str()),
        Some("aac"),
        "{seg_info:?}"
    );
    assert_eq!(
        seg_info.video.first().map(|v| v.codec.as_str()),
        Some("h264"),
        "{seg_info:?}"
    );

    // Seek-restart a dispatched session (the stale-read regression:
    // the old worker's in-flight source read must not poison the new
    // worker's header read — correlation is by request id, not session).
    let resp = api
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/playback/sessions/{session_id}/seek"))
                .header("authorization", bearer.clone())
                .header("content-type", "application/json")
                .body(Body::from(r#"{"position_ms":2500}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    if status != StatusCode::OK {
        let body = String::from_utf8_lossy(&body_bytes(resp).await).to_string();
        panic!("seek-restart failed: {status} — {body}");
    }
    let tail_playlist = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let resp = api.clone().oneshot(get(stream_url.clone())).await.unwrap();
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
    // ~5 s fixture sought to 2.5 s: expect roughly the tail.
    assert!(
        total > 1.5 && total < 4.0,
        "post-seek playlist should cover the tail, got {total}s:\n{tail_playlist}"
    );

    // AR-6: a second transcoder joins, the first dies mid-session —
    // the hub reschedules onto the survivor and playback recovers.
    let tc2_id = enroll(&ca, &allowed, "transcoder", "02TC", "backup-box");
    registry
        .record_satellite("02TC", "transcoder", "backup-box", "fp-02tc")
        .await
        .unwrap();
    let tc2_tls = kahawai_transport::mtls::mtls_client_config(&tc2_id).unwrap();
    let tc2_scratch = tempfile::tempdir().unwrap();
    let hub_addr3 = hub_addr.clone();
    let tc2_path = tc2_scratch.path().join("sessions");
    tokio::spawn(async move {
        let caps = pb::CapabilityReport {
            encoders: vec![pb::EncoderCap {
                codec: "aac".into(),
                element: "fdkaacenc".into(),
                hardware: false,
                speed_1080: None,
                speed_2160: None,
            }],
            max_sessions: 2,
            decode_caps: vec![],
            tonemap: false,
            tonemap_speed_1080: None,
            tonemap_speed_2160: None,
            ass_burn: false,
        };
        let _ = kahawai_transcoder::link_once(
            &hub_addr3,
            tc2_tls,
            "backup-box",
            tokio::sync::watch::channel(caps).1,
            &tc2_path,
            &None,
        )
        .await;
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sats = registry.satellites_overview().await.unwrap();
            if sats
                .iter()
                .any(|s| s["module_id"] == "02TC" && s["connected"] == true)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("backup transcoder never connected");

    // The box running the session vanishes: sockets close, and the hub
    // learns from the stream ending rather than from a 35 s timeout.
    let _ = cut_tc1.send(());
    tc1_task.abort();
    let recovered = tokio::time::timeout(Duration::from_secs(70), async {
        loop {
            let resp = api.clone().oneshot(get(stream_url.clone())).await.unwrap();
            if resp.status() == StatusCode::OK {
                let text = String::from_utf8(body_bytes(resp).await).unwrap();
                if text.contains("#EXT-X-ENDLIST") {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("session never recovered on the backup transcoder");
    assert!(
        recovered.contains("segment"),
        "recovered playlist empty:\n{recovered}"
    );
    assert!(
        sessions.get(&session_id).is_some(),
        "session should survive the transcoder loss"
    );

    // Teardown ends the dispatched session.
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
    // GONE: the transcoder's slot went back to the pool with the session,
    // so a client must negotiate a new one rather than retry this URL.
    assert_eq!(resp.status(), StatusCode::GONE);
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
        kahawai_hub::api::NetOptions::default(),
    )
}
