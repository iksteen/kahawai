//! mTLS link tests (SEC-5/6, §7.4): an enrolled mediahost connects and is
//! tracked; no cert, a revoked cert, and a foreign-CA cert are all refused.

use std::sync::Arc;
use std::time::Duration;

use kahawai_hub::link_service::MediahostLinkService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::Registry;
use kahawai_transport::identity::SatelliteIdentity;
use kahawai_transport::mtls::AllowedCerts;
use prost::Message as _;

struct Hub {
    addr: String,
    _sessions: Arc<kahawai_hub::sessions::Sessions>,
    registry: Arc<Registry>,
    allowed: AllowedCerts,
    ca: Arc<HubCa>,
    _pki: tempfile::TempDir,
}

async fn spawn_hub() -> Hub {
    let pki = tempfile::tempdir().unwrap();
    let ca = Arc::new(HubCa::load_or_create(pki.path()).unwrap());
    let (cert_pem, key_pem) = ca.issue_server_cert(&["localhost".into()]).unwrap();
    let allowed = AllowedCerts::default();
    let tls = kahawai_transport::mtls::mtls_server_config(
        &cert_pem,
        &key_pem,
        ca.ca_cert_pem(),
        allowed.clone(),
    )
    .unwrap();
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Arc::new(Registry::new(db, allowed.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("localhost:{}", listener.local_addr().unwrap().port());
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(
        tempfile::tempdir().unwrap().keep(),
    ));
    let svc = MediahostLinkService::new(
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
            .add_service(svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });
    Hub {
        addr,
        _sessions: sessions,
        registry,
        allowed,
        ca,
        _pki: pki,
    }
}

/// Enroll a satellite directly against the CA and admit it on the hub's
/// allowlist (the wire flow has its own test).
fn enroll(hub: &Hub, module_id: &str, name: &str) -> SatelliteIdentity {
    let id = sign_only(&hub.ca, module_id, name);
    hub.allowed
        .insert(&kahawai_transport::mtls::cert_fingerprint_pem(&id.cert_pem).unwrap());
    id
}

/// Sign a CSR without admitting the cert — chains to the CA but is NOT on
/// the allowlist.
fn sign_only(ca: &HubCa, module_id: &str, name: &str) -> SatelliteIdentity {
    let bundle = kahawai_core::pki::new_satellite_csr("mediahost", module_id, name).unwrap();
    let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    SatelliteIdentity {
        module_id: module_id.to_string(),
        key_pem: bundle.key_pem,
        cert_pem: signed.cert_pem,
        ca_pem: ca.ca_cert_pem().to_string(),
    }
}

async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn enrolled_mediahost_links_and_disconnect_is_tracked() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01LINK", "nas");
    sqlx::query(
        "INSERT INTO satellites(module_id,module_type,name,cert_fingerprint)
         VALUES('01LINK','mediahost','nas',?)",
    )
    .bind(kahawai_transport::mtls::cert_fingerprint_pem(&id.cert_pem).unwrap())
    .execute(hub.registry.db())
    .await
    .unwrap();
    let addr = hub.addr.clone();
    let reconnect_id = id.clone();

    let link = tokio::spawn(async move {
        // run() loops forever; we only need it to connect once.
        let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
        let channel = kahawai_transport::tls::grpc_channel_with(&addr, tls)
            .await
            .unwrap();
        let mut client =
            kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
                kahawai_proto::v1::Hello {
                    protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                    protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                    name: "nas".into(),
                    build: String::new(),
                    segment_detector_generation: 0,
                },
            )),
        })
        .await
        .unwrap();
        let mut inbound = client
            .link(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let first = inbound.message().await.unwrap().unwrap();
        assert!(matches!(
            first.msg,
            Some(kahawai_proto::v1::hub_to_host::Msg::HelloAck(_))
        ));
        // Offer a local catalogue, receive the hub's durable cursor, then
        // project one versioned current record.
        let root_path = std::path::Path::new("/tank/movies");
        let root_token = kahawai_core::media::root_token(root_path);
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogOffer(
                kahawai_proto::v1::CatalogOffer {
                    collections: vec![kahawai_proto::v1::CatalogCollection {
                        id: "movies".into(),
                        media_type: "movies".into(),
                        roots: vec![kahawai_proto::v1::CollectionRoot::new(
                            root_token.clone(),
                            root_path.display().to_string(),
                        )],
                        epoch: "epoch-a".into(),
                        current_version: 1,
                        oldest_replayable_version: 0,
                        scanning: false,
                    }],
                },
            )),
        })
        .await
        .unwrap();
        let cursor = tokio::time::timeout(Duration::from_secs(5), inbound.message())
            .await
            .expect("catalogue cursor timeout")
            .unwrap()
            .unwrap();
        assert!(matches!(
            cursor.msg,
            Some(kahawai_proto::v1::hub_to_host::Msg::CatalogCursor(
                kahawai_proto::v1::CatalogCursor {
                    version: 0,
                    snapshot: true,
                    ..
                }
            ))
        ));
        let source = kahawai_proto::v1::SourcePath::new(root_token.clone(), "Heat (1995)/Heat.mkv");
        let payload = kahawai_proto::v1::FileUpsert {
            collection_id: "movies".into(),
            files: vec![kahawai_proto::v1::FileRecord {
                source: Some(source.clone()),
                size: 123,
                mtime_unix: 456,
                head_xxh3: 1,
                tail_xxh3: 2,
                oshash: 3,
                streams_json: "{}".into(),
            }],
        }
        .encode_to_vec();
        let mut key = root_token.into_bytes();
        key.push(0);
        key.extend_from_slice(source.path_rel.as_bytes());
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogDelta(
                kahawai_proto::v1::CatalogDelta {
                    collection_id: "movies".into(),
                    epoch: "epoch-a".into(),
                    records: vec![kahawai_proto::v1::CatalogRecord {
                        version: 1,
                        kind: "file".into(),
                        key,
                        payload,
                        deleted: false,
                    }],
                    through_version: 1,
                    snapshot: true,
                    done: true,
                },
            )),
        })
        .await
        .unwrap();
        let ack = tokio::time::timeout(Duration::from_secs(5), inbound.message())
            .await
            .expect("catalogue ACK timeout")
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack.msg,
            Some(kahawai_proto::v1::hub_to_host::Msg::CatalogAck(
                kahawai_proto::v1::CatalogAck { version: 1, .. }
            ))
        ));
        // Keep the link open until the test drops us.
        (tx, inbound)
    });
    let (tx, mut inbound) = link.await.unwrap();

    let registry = hub.registry.clone();
    wait_until(
        || {
            registry
                .snapshot()
                .iter()
                .any(|(id, s)| id == "01LINK" && s.connected)
        },
        "mediahost to appear connected",
    )
    .await;

    // Identity came from the certificate, not from any message.
    let snap = hub.registry.snapshot();
    let (_, state) = snap.iter().find(|(id, _)| id == "01LINK").unwrap();
    assert_eq!(state.module_type, "mediahost");
    assert_eq!(state.name, "nas");

    // The announced collection and its file arrive in the registry.
    let registry = hub.registry.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let cols = registry.collections().await.unwrap();
            if cols.iter().any(|c| {
                c.module_id == "01LINK"
                    && c.collection_id == "movies"
                    && c.available
                    && c.file_count == 1
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("collection with one file");

    // A terminal hash result is current catalogue state, not merely a log
    // message. In particular, it must clear a prior exact hash when a file was
    // replaced with different bytes that retained the same stat stamp.
    let root_token = kahawai_core::media::root_token(std::path::Path::new("/tank/movies"));
    let source = kahawai_proto::v1::SourcePath::new(root_token.clone(), "Heat (1995)/Heat.mkv");
    let mut hash_key = root_token.clone().into_bytes();
    hash_key.push(0);
    hash_key.extend_from_slice(source.path_rel.as_bytes());
    for (version, hash) in [
        (
            2,
            kahawai_proto::v1::FileHash {
                source: Some(source.clone()),
                size: 123,
                ed2k_hex: "exact-hash".into(),
                ..Default::default()
            },
        ),
        (
            3,
            kahawai_proto::v1::FileHash {
                source: Some(source.clone()),
                error: "replacement could not be read".into(),
                ..Default::default()
            },
        ),
    ] {
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogDelta(
                kahawai_proto::v1::CatalogDelta {
                    collection_id: "movies".into(),
                    epoch: "epoch-a".into(),
                    records: vec![kahawai_proto::v1::CatalogRecord {
                        version,
                        kind: "file_hashes".into(),
                        key: hash_key.clone(),
                        payload: kahawai_proto::v1::FileHashes {
                            collection_id: "movies".into(),
                            hashes: vec![hash],
                        }
                        .encode_to_vec(),
                        deleted: false,
                    }],
                    through_version: version,
                    snapshot: false,
                    done: true,
                },
            )),
        })
        .await
        .unwrap();
        let ack = tokio::time::timeout(Duration::from_secs(5), inbound.message())
            .await
            .expect("hash catalogue ACK timeout")
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack.msg,
            Some(kahawai_proto::v1::hub_to_host::Msg::CatalogAck(
                kahawai_proto::v1::CatalogAck { version: acked, .. }
            )) if acked == version
        ));
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT ed2k FROM files WHERE module_id='01LINK' AND collection_id='movies'",
        )
        .fetch_one(hub.registry.db())
        .await
        .unwrap();
        assert_eq!(stored.as_deref(), (version == 2).then_some("exact-hash"));
    }
    let ed2k: Option<String> = sqlx::query_scalar(
        "SELECT ed2k FROM files WHERE module_id='01LINK' AND collection_id='movies'",
    )
    .fetch_one(hub.registry.db())
    .await
    .unwrap();
    assert!(
        ed2k.is_none(),
        "terminal current hash state retained stale ED2K"
    );

    // A new catalogue epoch is a physical refresh, not permission to erase
    // hub-owned identity decisions. Stream the same current source through a
    // forced snapshot and verify its stable item row (and manual pin) survive.
    let item_id: String = sqlx::query_scalar(
        "SELECT id FROM items WHERE module_id='01LINK' AND collection_id='movies' AND kind='movie'",
    )
    .fetch_one(hub.registry.db())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO manual_match(item_id,provider,provider_id,pinned_at)
         VALUES(?,'tmdb','123',unixepoch())",
    )
    .bind(&item_id)
    .execute(hub.registry.db())
    .await
    .unwrap();
    let root_path = std::path::Path::new("/tank/movies");
    let root_token = kahawai_core::media::root_token(root_path);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogOffer(
            kahawai_proto::v1::CatalogOffer {
                collections: vec![kahawai_proto::v1::CatalogCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    roots: vec![kahawai_proto::v1::CollectionRoot::new(
                        root_token.clone(),
                        root_path.display().to_string(),
                    )],
                    epoch: "epoch-b".into(),
                    current_version: 2,
                    oldest_replayable_version: 0,
                    scanning: false,
                }],
            },
        )),
    })
    .await
    .unwrap();
    let cursor = tokio::time::timeout(Duration::from_secs(5), inbound.message())
        .await
        .expect("replacement snapshot cursor timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(
        cursor.msg,
        Some(kahawai_proto::v1::hub_to_host::Msg::CatalogCursor(
            kahawai_proto::v1::CatalogCursor {
                version: 0,
                snapshot: true,
                ..
            }
        ))
    ));
    let source = kahawai_proto::v1::SourcePath::new(root_token.clone(), "Heat (1995)/Heat.mkv");
    let payload = kahawai_proto::v1::FileUpsert {
        collection_id: "movies".into(),
        files: vec![kahawai_proto::v1::FileRecord {
            source: Some(source.clone()),
            size: 123,
            mtime_unix: 457,
            head_xxh3: 1,
            tail_xxh3: 2,
            oshash: 3,
            streams_json: "{}".into(),
        }],
    }
    .encode_to_vec();
    let mut key = root_token.into_bytes();
    key.push(0);
    key.extend_from_slice(source.path_rel.as_bytes());
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogDelta(
            kahawai_proto::v1::CatalogDelta {
                collection_id: "movies".into(),
                epoch: "epoch-b".into(),
                records: vec![kahawai_proto::v1::CatalogRecord {
                    version: 1,
                    kind: "file".into(),
                    key,
                    payload,
                    deleted: false,
                }],
                through_version: 0,
                snapshot: true,
                done: false,
            },
        )),
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mtime: i64 = sqlx::query_scalar(
                "SELECT mtime_unix FROM files
                  WHERE module_id='01LINK' AND collection_id='movies'",
            )
            .fetch_one(hub.registry.db())
            .await
            .unwrap();
            if mtime == 457 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replacement snapshot first page was not applied");
    let staged_cursor: i64 = sqlx::query_scalar(
        "SELECT version FROM mediahost_catalog_cursors
          WHERE module_id='01LINK' AND collection_id='movies'",
    )
    .fetch_one(hub.registry.db())
    .await
    .unwrap();
    assert_eq!(
        staged_cursor, 0,
        "an incomplete snapshot became a durable resume point"
    );
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogDelta(
            kahawai_proto::v1::CatalogDelta {
                collection_id: "movies".into(),
                epoch: "epoch-b".into(),
                records: Vec::new(),
                through_version: 2,
                snapshot: false,
                done: true,
            },
        )),
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), inbound.message())
        .await
        .expect("replacement snapshot final ACK timeout")
        .unwrap()
        .unwrap();
    let preserved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manual_match WHERE item_id=? AND provider_id='123'",
    )
    .bind(&item_id)
    .fetch_one(hub.registry.db())
    .await
    .unwrap();
    assert_eq!(preserved, 1, "catalogue replacement erased a manual pin");

    // Drop the client: AR-6 — satellite and collection marked unavailable,
    // nothing deleted.
    drop(tx);
    drop(inbound);
    let registry = hub.registry.clone();
    wait_until(
        || {
            registry
                .snapshot()
                .iter()
                .any(|(id, s)| id == "01LINK" && !s.connected)
        },
        "mediahost to be marked disconnected",
    )
    .await;
    let cols = hub.registry.collections().await.unwrap();
    let col = cols.iter().find(|c| c.module_id == "01LINK").unwrap();
    assert!(
        !col.available,
        "collection must be unavailable after disconnect"
    );
    assert_eq!(col.file_count, 1, "files must survive a disconnect (AR-6)");

    // The hub, not an in-memory ACK observation on the mediahost, supplies
    // the durable resume point after reconnect.
    let tls = kahawai_transport::mtls::mtls_client_config(&reconnect_id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                name: "nas".into(),
                build: String::new(),
                segment_detector_generation: 0,
            },
        )),
    })
    .await
    .unwrap();
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap();
    let root_path = std::path::Path::new("/tank/movies");
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogOffer(
            kahawai_proto::v1::CatalogOffer {
                collections: vec![kahawai_proto::v1::CatalogCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    roots: vec![kahawai_proto::v1::CollectionRoot::new(
                        kahawai_core::media::root_token(root_path),
                        root_path.display().to_string(),
                    )],
                    epoch: "epoch-b".into(),
                    current_version: 2,
                    oldest_replayable_version: 0,
                    scanning: false,
                }],
            },
        )),
    })
    .await
    .unwrap();
    let cursor = tokio::time::timeout(Duration::from_secs(5), inbound.message())
        .await
        .expect("resume cursor timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(
        cursor.msg,
        Some(kahawai_proto::v1::hub_to_host::Msg::CatalogCursor(
            kahawai_proto::v1::CatalogCursor {
                version: 2,
                snapshot: false,
                ..
            }
        ))
    ));
}

#[tokio::test]
async fn protocol_4_rejects_legacy_reconciliation_messages() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01LEGACY", "legacy-v4");
    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                name: "legacy-v4".into(),
                build: String::new(),
                segment_detector_generation: 0,
            },
        )),
    })
    .await
    .unwrap();
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap();

    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::AnnounceCollection(
            kahawai_proto::v1::AnnounceCollection {
                id: "movies".into(),
                media_type: "movies".into(),
                roots: vec![kahawai_proto::v1::CollectionRoot::new(
                    kahawai_core::media::root_token(std::path::Path::new("/media/movies")),
                    "/media/movies",
                )],
            },
        )),
    })
    .await
    .unwrap();
    let status = inbound.message().await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("rejects legacy catalogue"));
}

#[tokio::test]
async fn no_client_cert_cannot_link() {
    let hub = spawn_hub().await;
    // Channel with server-only TLS (the enrollment-style channel).
    let channel = kahawai_transport::tls::grpc_channel_unverified(&hub.addr)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (_tx, rx) = tokio::sync::mpsc::channel::<kahawai_proto::v1::HostToHub>(1);
    let err = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn unlisted_cert_is_refused_fail_closed() {
    // A certificate validly signed by the hub CA grants nothing unless it
    // is on the allowlist (SEC-5 fail-closed).
    let hub = spawn_hub().await;
    let id = sign_only(&hub.ca, "01GHOST", "never-admitted");
    assert!(
        try_link(&hub.addr, &id).await.is_err(),
        "CA-signed but unlisted cert must be refused at the TLS layer"
    );
}

#[tokio::test]
async fn deleted_cert_is_refused_at_tls_layer() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01GONE", "gone");
    // Admitted first: the link works.
    assert!(try_link(&hub.addr, &id).await.is_ok());
    // Deletion removes the fingerprint from the allowlist (SEC-6).
    let fp = kahawai_transport::mtls::cert_fingerprint_pem(&id.cert_pem).unwrap();
    hub.allowed.remove(&fp);
    // TLS 1.3: the client's handshake "succeeds" locally before the server
    // evaluates the client cert, so the refusal surfaces on the first RPC.
    assert!(
        try_link(&hub.addr, &id).await.is_err(),
        "deleted cert must be refused at the TLS layer (SEC-6)"
    );
}

/// Connect + attempt the Link RPC (with a proper Hello, so a successful
/// attempt completes instead of deadlocking on the server's Hello wait);
/// returns Err if any step is refused. Bounded: refusals must be prompt.
async fn try_link(addr: &str, id: &SatelliteIdentity) -> Result<(), Box<dyn std::error::Error>> {
    let attempt = async {
        let tls = kahawai_transport::mtls::mtls_client_config(id)?;
        let channel = kahawai_transport::tls::grpc_channel_with(addr, tls).await?;
        let mut client =
            kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
        let (tx, rx) = tokio::sync::mpsc::channel::<kahawai_proto::v1::HostToHub>(1);
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
                kahawai_proto::v1::Hello {
                    protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                    protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                    name: "probe".into(),
                    build: String::new(),
                    segment_detector_generation: 0,
                },
            )),
        })
        .await
        .ok();
        client
            .link(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await?;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
}

#[tokio::test]
async fn protocol_4_rejects_a_missing_exact_source() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01BADP3", "bad-p3");
    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                name: "bad-p3".into(),
                build: String::new(),
                segment_detector_generation: 0,
            },
        )),
    })
    .await
    .unwrap();
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap();
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::FileSubtitles(
            kahawai_proto::v1::FileSubtitles {
                collection_id: "movies".into(),
                source: None,
                ..Default::default()
            },
        )),
    })
    .await
    .unwrap();
    let status = inbound.message().await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("missing exact source"),
        "{status}"
    );
}

#[tokio::test]
async fn protocol_4_rejects_an_invalid_root_binding() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01BADROOT", "bad-root");
    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                name: "bad-root".into(),
                build: String::new(),
                segment_detector_generation: 0,
            },
        )),
    })
    .await
    .unwrap();
    let mut inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    inbound.message().await.unwrap().unwrap();
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::CatalogOffer(
            kahawai_proto::v1::CatalogOffer {
                collections: vec![kahawai_proto::v1::CatalogCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    roots: vec![kahawai_proto::v1::CollectionRoot::new(
                        "root-sha256-not-the-path-digest",
                        "/media/movies",
                    )],
                    epoch: "epoch".into(),
                    current_version: 0,
                    oldest_replayable_version: 0,
                    scanning: false,
                }],
            },
        )),
    })
    .await
    .unwrap();
    let status = inbound.message().await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("invalid root binding"),
        "{status}"
    );
}

#[tokio::test]
async fn protocol_3_mediahost_is_rejected_during_hello() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01OLD", "protocol-two");
    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: 3,
                protocol_minor: 6,
                name: "old".into(),
                build: String::new(),
                segment_detector_generation: 0,
            },
        )),
    })
    .await
    .unwrap();
    let status = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains(&format!(
            "hub speaks {}.{}",
            kahawai_proto::PROTOCOL_MAJOR,
            kahawai_proto::PROTOCOL_MINOR
        )),
        "{status}"
    );
}

#[tokio::test]
async fn foreign_ca_cert_is_refused() {
    let hub = spawn_hub().await;
    // A different CA signs an otherwise-identical satellite cert.
    let foreign_pki = tempfile::tempdir().unwrap();
    let foreign_ca = HubCa::load_or_create(foreign_pki.path()).unwrap();
    let mut id = sign_only(&foreign_ca, "01FOREIGN", "evil");
    // It pins the real hub CA so the server side passes; only the client
    // cert is foreign.
    id.ca_pem = hub.ca.ca_cert_pem().to_string();

    assert!(
        try_link(&hub.addr, &id).await.is_err(),
        "foreign-CA cert must be refused at the TLS layer (SEC-5)"
    );
}
