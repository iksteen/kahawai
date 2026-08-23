//! mTLS link tests (SEC-5/6, §7.4): an enrolled mediahost connects and is
//! tracked; no cert, a revoked cert, and a foreign-CA cert are all refused.

use std::sync::Arc;
use std::time::Duration;

use kahawai_hub::link_service::MediahostLinkService;
use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::Registry;
use kahawai_transport::identity::SatelliteIdentity;
use kahawai_transport::mtls::AllowedCerts;

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
    let addr = hub.addr.clone();

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
        // Announce a collection and push one file record.
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::AnnounceCollection(
                kahawai_proto::v1::AnnounceCollection {
                    id: "movies".into(),
                    media_type: "movies".into(),
                    roots: vec![kahawai_proto::v1::CollectionRoot::new(
                        kahawai_core::media::root_token(std::path::Path::new("/tank/movies")),
                        "/tank/movies",
                    )],
                },
            )),
        })
        .await
        .unwrap();
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::FileUpsert(
                kahawai_proto::v1::FileUpsert {
                    collection_id: "movies".into(),
                    files: vec![kahawai_proto::v1::FileRecord {
                        source: Some(kahawai_proto::v1::SourcePath::new(
                            kahawai_core::media::root_token(std::path::Path::new("/tank/movies")),
                            "Heat (1995)/Heat.mkv",
                        )),
                        size: 123,
                        mtime_unix: 456,
                        head_xxh3: 1,
                        tail_xxh3: 2,
                        oshash: 3,
                        streams_json: "{}".into(),
                    }],
                },
            )),
        })
        .await
        .unwrap();
        // Keep the link open until the test drops us.
        (tx, inbound)
    });

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

    // Drop the client: AR-6 — satellite and collection marked unavailable,
    // nothing deleted.
    let (tx, inbound) = link.await.unwrap();
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
}

#[tokio::test]
async fn root_adoption_suppresses_only_its_own_generation_mismatch() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "01ADOPT", "adopter");
    hub.registry
        .announce_collection("01ADOPT", "movies", "movies", &[])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE collections SET sync_version = 7
         WHERE module_id = '01ADOPT' AND collection_id = 'movies'",
    )
    .execute(hub.registry.db())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items(id,kind,title,norm_title,module_id,collection_id)
         VALUES('legacy','movie','Legacy','legacy','01ADOPT','movies')",
    )
    .execute(hub.registry.db())
    .await
    .unwrap();
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO files
           (module_id,collection_id,path_rel,size,mtime_unix,
            head_xxh3,tail_xxh3,oshash,streams_json)
         VALUES('01ADOPT','movies','legacy.mkv',10,1,2,3,4,'{}') RETURNING id",
    )
    .fetch_one(hub.registry.db())
    .await
    .unwrap();
    kahawai_hub::registry::bind_file_to_item(
        &mut hub.registry.db().acquire().await.unwrap(),
        file_id,
        "legacy",
    )
    .await
    .unwrap();
    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(
            kahawai_proto::v1::Hello {
                protocol_major: 3,
                protocol_minor: 0,
                name: "adopter".into(),
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

    let root_path = std::path::Path::new("/media/movies");
    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::AnnounceCollection(
            kahawai_proto::v1::AnnounceCollection {
                id: "movies".into(),
                media_type: "movies".into(),
                roots: vec![kahawai_proto::v1::CollectionRoot::new(
                    kahawai_core::media::root_token(root_path),
                    root_path.display().to_string(),
                )],
            },
        )),
    })
    .await
    .unwrap();
    let request = || kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::ManifestRequest(
            kahawai_proto::v1::ManifestRequest {
                collection_id: "movies".into(),
                sync_version: 99,
            },
        )),
    };
    tx.send(request()).await.unwrap();

    let first = inbound.message().await.unwrap().unwrap();
    let Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(first)) = first.msg else {
        panic!("root adoption did not answer with a manifest")
    };
    assert!(first.in_sync);
    assert!(first.entries.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT sync_version FROM collections
             WHERE module_id = '01ADOPT' AND collection_id = 'movies'",
        )
        .fetch_one(hub.registry.db())
        .await
        .unwrap(),
        7,
        "adoption must not hide restore drift by changing the hub generation"
    );
    assert_ne!(
        sqlx::query_scalar::<_, String>(
            "SELECT r.root_token FROM files f JOIN collection_roots r ON r.id=f.root_id
             WHERE f.module_id='01ADOPT' AND f.collection_id='movies'",
        )
        .fetch_one(hub.registry.db())
        .await
        .unwrap(),
        ""
    );
    assert!(first.root_adoption);

    tx.send(request()).await.unwrap();
    let repeated = inbound.message().await.unwrap().unwrap();
    let Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(repeated)) = repeated.msg else {
        panic!("unacknowledged adoption did not repeat its suppression")
    };
    assert!(repeated.in_sync);
    assert!(repeated.root_adoption);
    assert!(repeated.entries.is_empty());

    tx.send(kahawai_proto::v1::HostToHub {
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::RootAdoptionAck(
            kahawai_proto::v1::RootAdoptionAck {
                collection_id: "movies".into(),
            },
        )),
    })
    .await
    .unwrap();

    tx.send(request()).await.unwrap();
    let after_ack = inbound.message().await.unwrap().unwrap();
    let Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(after_ack)) = after_ack.msg else {
        panic!("post-ack request did not receive the normal manifest")
    };
    assert!(!after_ack.in_sync, "acknowledgement must end suppression");
    assert_eq!(after_ack.entries.len(), 1);
    let source = after_ack.entries[0].source.as_ref().unwrap();
    assert!(!source.root_token.is_empty());
    assert_eq!(source.path_rel, "legacy.mkv");
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
async fn protocol_3_rejects_a_missing_exact_source() {
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
                protocol_major: 3,
                protocol_minor: 0,
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
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::FileUpsert(
            kahawai_proto::v1::FileUpsert {
                collection_id: "movies".into(),
                files: vec![kahawai_proto::v1::FileRecord {
                    source: None,
                    ..Default::default()
                }],
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
async fn protocol_3_rejects_an_invalid_root_binding() {
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
                protocol_major: 3,
                protocol_minor: 0,
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
        msg: Some(kahawai_proto::v1::host_to_hub::Msg::AnnounceCollection(
            kahawai_proto::v1::AnnounceCollection {
                id: "movies".into(),
                media_type: "movies".into(),
                roots: vec![kahawai_proto::v1::CollectionRoot::new(
                    "root-sha256-not-the-path-digest",
                    "/media/movies",
                )],
            },
        )),
    })
    .await
    .unwrap();
    let status = inbound.message().await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status.message().contains("invalid root token/path"),
        "{status}"
    );
}

#[tokio::test]
async fn protocol_2_mediahost_is_rejected_during_hello() {
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
                protocol_major: 2,
                protocol_minor: 4,
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
