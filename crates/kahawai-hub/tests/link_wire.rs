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
    let sessions = Arc::new(kahawai_hub::sessions::Sessions::new(tempfile::tempdir().unwrap().keep()));
    let svc = MediahostLinkService::new(registry.clone(), sessions.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });
    Hub { addr, _sessions: sessions, registry, allowed, ca, _pki: pki }
}

/// Enroll a satellite directly against the CA and admit it on the hub's
/// allowlist (the wire flow has its own test).
fn enroll(hub: &Hub, module_id: &str, name: &str) -> SatelliteIdentity {
    let id = sign_only(&hub.ca, module_id, name);
    hub.allowed.insert(&kahawai_transport::mtls::cert_fingerprint_pem(&id.cert_pem).unwrap());
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
        let channel = kahawai_transport::tls::grpc_channel_with(&addr, tls).await.unwrap();
        let mut client =
            kahawai_proto::v1::mediahost_link_client::MediahostLinkClient::new(channel);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(kahawai_proto::v1::HostToHub {
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(kahawai_proto::v1::Hello {
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
                    roots: vec!["/tank/movies".into()],
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
                        path_rel: "Heat (1995)/Heat.mkv".into(),
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
        || registry.snapshot().iter().any(|(id, s)| id == "01LINK" && s.connected),
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
        || registry.snapshot().iter().any(|(id, s)| id == "01LINK" && !s.connected),
        "mediahost to be marked disconnected",
    )
    .await;
    let cols = hub.registry.collections().await.unwrap();
    let col = cols.iter().find(|c| c.module_id == "01LINK").unwrap();
    assert!(!col.available, "collection must be unavailable after disconnect");
    assert_eq!(col.file_count, 1, "files must survive a disconnect (AR-6)");
}

#[tokio::test]
async fn no_client_cert_cannot_link() {
    let hub = spawn_hub().await;
    // Channel with server-only TLS (the enrollment-style channel).
    let channel = kahawai_transport::tls::grpc_channel_unverified(&hub.addr).await.unwrap();
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
            msg: Some(kahawai_proto::v1::host_to_hub::Msg::Hello(kahawai_proto::v1::Hello {
                protocol_major: kahawai_proto::PROTOCOL_MAJOR,
                protocol_minor: kahawai_proto::PROTOCOL_MINOR,
                name: "probe".into(),
            })),
        })
        .await
        .ok();
        client.link(tokio_stream::wrappers::ReceiverStream::new(rx)).await?;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
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
