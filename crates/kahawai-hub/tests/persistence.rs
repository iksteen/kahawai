//! HUB-13/NFR-3: state survives a hub restart; movie resolution dedups
//! sources onto one item (HUB-3/4); the browse API serves it all.

use std::sync::Arc;

use kahawai_hub::registry::{FileUpsertRecord, Registry};
use sqlx::Row;
use tower::ServiceExt;

fn rec(path: &str, size: u64) -> FileUpsertRecord {
    FileUpsertRecord {
        path_rel: path.into(),
        size,
        mtime_unix: 1,
        head_xxh3: 1,
        tail_xxh3: 2,
        oshash: 3,
        streams_json: r#"{"container":"matroska"}"#.into(),
    }
}

#[tokio::test]
async fn files_and_items_survive_restart() {
    let dir = tempfile::tempdir().unwrap();

    {
        let db = kahawai_hub::db::open(dir.path()).await.unwrap();
        let reg = Registry::new(db.clone(), Default::default());
        reg.announce_collection("01H", "movies", "movies", &[]).await.unwrap();
        reg.upsert_files(
            "01H",
            "movies",
            vec![
                // Same movie, two qualities → one item, two sources.
                rec("Heat (1995)/Heat.1995.2160p.mkv", 100),
                rec("Heat.1995.1080p.BluRay.x264-GRP.mkv", 50),
                rec("Ronin (1998).mkv", 60),
            ],
        )
        .await
        .unwrap();
        db.close().await;
    }

    // "Restart": fresh pool over the same directory.
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Arc::new(Registry::new(db.clone(), Default::default()));

    // The DB (password hashes, sessions) must not be world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("hub.db")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "hub.db must be 0600");
    }

    let cols = reg.collections().await.unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].file_count, 3);
    assert!(!cols[0].available, "no mediahost connected after restart");

    let titles: Vec<(String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT i.title, i.year, COUNT(s.item_id) FROM items i
         JOIN item_sources s ON s.item_id = i.id
         GROUP BY i.id ORDER BY i.title",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        titles,
        vec![("Heat".into(), Some(1995), 2), ("Ronin".into(), Some(1998), 1)]
    );

    // Browse API over the same state (setup + login first).
    let auth = Arc::new(kahawai_hub::auth::Auth::new(db.clone(), dir.path()).await.unwrap());
    let token = auth.setup_token().unwrap();
    let pair = auth.complete_setup(&token, "admin", "password-123").await.unwrap();
    let bearer = format!("Bearer {}", pair.access_token);
    let get = |uri: String| {
        axum::http::Request::get(uri)
            .header("authorization", bearer.clone())
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let api = test_router(reg.clone(), auth, Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep())));
    let resp = api
        .clone()
        .oneshot(get("/api/v1/items".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Heat");
    assert_eq!(items[0]["sources"], 2);

    // Detail includes sources with parsed stream info and availability.
    let id = items[0]["id"].as_str().unwrap();
    let resp = api
        .clone()
        .oneshot(get(format!("/api/v1/items/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sources = json["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["size"], 100, "sources ranked by size");
    assert_eq!(sources[0]["streams"]["container"], "matroska");
    assert_eq!(sources[0]["available"], false);

    // Unknown item → 404.
    let resp = api.oneshot(get("/api/v1/items/nope".into())).await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reconcile_drops_files_missing_from_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = kahawai_hub::db::open(dir.path()).await.unwrap();
    let reg = Registry::new(db.clone(), Default::default());
    reg.announce_collection("01H", "movies", "movies", &[]).await.unwrap();
    reg.upsert_files(
        "01H",
        "movies",
        vec![rec("Heat (1995).mkv", 100), rec("Ronin (1998).mkv", 50)],
    )
    .await
    .unwrap();

    // A user watched Ronin; its state must die with the item.
    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ('u1','u','x')")
        .execute(&db)
        .await
        .unwrap();
    let ronin: String = sqlx::query_scalar("SELECT id FROM items WHERE title = 'Ronin'")
        .fetch_one(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO watch_state (user_id, item_id, position_ms) VALUES ('u1', ?, 1234)")
        .bind(&ronin)
        .execute(&db)
        .await
        .unwrap();

    // Rescan saw only Heat.
    let seen: std::collections::HashSet<String> = ["Heat (1995).mkv".to_string()].into();
    let removed = reg.reconcile_files("01H", "movies", &seen).await.unwrap();
    assert_eq!(removed, 1);

    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files").fetch_one(&db).await.unwrap();
    let items: Vec<String> = sqlx::query_scalar("SELECT title FROM items").fetch_all(&db).await.unwrap();
    let watch: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_state").fetch_one(&db).await.unwrap();
    assert_eq!(files, 1);
    assert_eq!(items, vec!["Heat".to_string()]);
    assert_eq!(watch, 0, "watch state cascades with the removed item");

    // Idempotent when nothing changed.
    assert_eq!(reg.reconcile_files("01H", "movies", &seen).await.unwrap(), 0);
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

/// Incremental rescan (MH-5): a second scan that reports unchanged files
/// via FilesSeen (no re-upsert) must NOT get them reconciled away, and
/// the hub must answer ManifestRequest with what it knows.
#[tokio::test]
async fn manifest_and_files_seen_survive_rescan() {
    use kahawai_proto::v1 as pb;
    let pki = tempfile::tempdir().unwrap();
    let ca = Arc::new(kahawai_hub::pki::HubCa::load_or_create(pki.path()).unwrap());
    let allowed = kahawai_transport::mtls::AllowedCerts::default();
    let (cert_pem, key_pem) = ca.issue_server_cert(&["localhost".into()]).unwrap();
    let tls = kahawai_transport::mtls::mtls_server_config(
        &cert_pem,
        &key_pem,
        ca.ca_cert_pem(),
        allowed.clone(),
    )
    .unwrap();
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Arc::new(Registry::new(db.clone(), allowed.clone()));
    let sessions =
        Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let link_svc =
        kahawai_hub::link_service::MediahostLinkService::new(
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

    let bundle = kahawai_core::pki::new_satellite_csr("mediahost", "01HOST", "nas").unwrap();
    let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    allowed.insert(&signed.fingerprint);
    let id = kahawai_transport::identity::SatelliteIdentity {
        module_id: "01HOST".into(),
        key_pem: bundle.key_pem,
        cert_pem: signed.cert_pem,
        ca_pem: ca.ca_cert_pem().to_string(),
    };
    let client_tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();

    let scan = |round: u32| {
        let hub_addr = hub_addr.clone();
        let client_tls = client_tls.clone();
        async move {
            let channel =
                kahawai_transport::tls::grpc_channel_with(&hub_addr, client_tls).await.unwrap();
            let mut client = pb::mediahost_link_client::MediahostLinkClient::new(channel);
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
            tx.send(pb::HostToHub {
                msg: Some(pb::host_to_hub::Msg::AnnounceCollection(pb::AnnounceCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    roots: vec!["/srv/movies".into()],
                })),
            })
            .await
            .unwrap();
            // Round 3 handshakes with the version stored in round 2:
            // the hub must answer in_sync and send no entries.
            tx.send(pb::HostToHub {
                msg: Some(pb::host_to_hub::Msg::ManifestRequest(pb::ManifestRequest {
                    collection_id: "movies".into(),
                    sync_version: if round == 3 { 7 } else { 0 },
                })),
            })
            .await
            .unwrap();
            // Collect the manifest.
            let mut entries = vec![];
            let mut in_sync = false;
            loop {
                match inbound.message().await.unwrap().unwrap().msg {
                    Some(pb::hub_to_host::Msg::Manifest(m)) => {
                        entries.extend(m.entries);
                        in_sync |= m.in_sync;
                        if m.done {
                            break;
                        }
                    }
                    other => panic!("expected manifest, got {other:?}"),
                }
            }
            if round == 3 {
                assert!(in_sync, "matching sync_version must short-circuit");
                assert!(entries.is_empty());
                return; // handshake skips the scan entirely
            }
            assert!(!in_sync);
            if round == 1 {
                assert!(entries.is_empty(), "fresh hub knows nothing: {entries:?}");
                tx.send(pb::HostToHub {
                    msg: Some(pb::host_to_hub::Msg::FileUpsert(pb::FileUpsert {
                        collection_id: "movies".into(),
                        files: vec![pb::FileRecord {
                            path_rel: "Heat (1995).mkv".into(),
                            size: 100,
                            mtime_unix: 42,
                            head_xxh3: 1,
                            tail_xxh3: 2,
                            oshash: 3,
                            streams_json: "{}".into(),
                        }],
                    })),
                })
                .await
                .unwrap();
            } else {
                // Round 2: hub knows the file; report it seen, upsert nothing.
                assert_eq!(entries.len(), 1, "{entries:?}");
                assert_eq!(entries[0].path_rel, "Heat (1995).mkv");
                assert_eq!(entries[0].size, 100);
                assert_eq!(entries[0].mtime_unix, 42);
                tx.send(pb::HostToHub {
                    msg: Some(pb::host_to_hub::Msg::FilesSeen(pb::FilesSeen {
                        collection_id: "movies".into(),
                        path_rel: vec!["Heat (1995).mkv".into()],
                    })),
                })
                .await
                .unwrap();
            }
            tx.send(pb::HostToHub {
                msg: Some(pb::host_to_hub::Msg::ScanProgress(pb::ScanProgress {
                    collection_id: "movies".into(),
                    scanned: (round == 1) as u32,
                    failed: 0,
                    complete: true,
                    skipped: (round == 2) as u32,
                    sync_version: 7, // the generation both rounds report
                })),
            })
            .await
            .unwrap();
            // Give the hub a beat to reconcile before the link drops.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    };

    scan(1).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1);

    scan(2).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1, "FilesSeen must protect unchanged files from reconciliation");
    let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(items, 1, "resolved item survives the incremental rescan");
    // Round 3: reconnect with the stored generation → in_sync short-circuit.
    scan(3).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 1, "in-sync reconnect must leave state untouched");
}

#[tokio::test]
async fn multipart_movies_group_into_one_item() {
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Registry::new(db.clone(), Default::default());
    registry.record_satellite("01HOST", "mediahost", "nas", "fp").await.unwrap();
    registry
        .announce_collection("01HOST", "movies", "movies", &["/srv/movies".into()])
        .await
        .unwrap();
    registry
        .upsert_files(
            "01HOST",
            "movies",
            vec![
                rec("12 Monkeys - CD1.avi", 700),
                rec("12 Monkeys - CD2.avi", 701),
                rec("Heat (1995).mkv", 800),
            ],
        )
        .await
        .unwrap();

    let items: Vec<String> =
        sqlx::query_scalar("SELECT title FROM items WHERE kind='movie' ORDER BY title")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(items, ["12 Monkeys", "Heat"], "{items:?}");

    let parts: Vec<(String, Option<i64>)> = sqlx::query(
        "SELECT s.path_rel, s.part FROM item_sources s
         JOIN items i ON i.id = s.item_id WHERE i.title = '12 Monkeys'
         ORDER BY s.part",
    )
    .fetch_all(&db)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get::<String, _>("path_rel"), r.get("part")))
    .collect();
    assert_eq!(
        parts,
        vec![
            ("12 Monkeys - CD1.avi".to_string(), Some(1)),
            ("12 Monkeys - CD2.avi".to_string(), Some(2)),
        ]
    );
    let heat_part: Option<i64> = sqlx::query_scalar(
        "SELECT s.part FROM item_sources s JOIN items i ON i.id = s.item_id
         WHERE i.title = 'Heat'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(heat_part, None);
}
