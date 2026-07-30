//! TranscoderLink wire test (M3): an enrolled transcoder connects,
//! registers its dry-run-verified capability report, and the admin
//! overview exposes it while connected; a mediahost certificate is
//! refused on the transcoder link (type confusion, SEC-5 spirit).

use std::sync::Arc;
use std::time::Duration;

use kahawai_hub::pki::HubCa;
use kahawai_hub::registry::{PlacementNeed, Registry};
use kahawai_hub::transcoder_link::TranscoderLinkService;
use kahawai_proto::v1::transcoder_link_client::TranscoderLinkClient;
use kahawai_proto::v1::{CapabilityReport, EncoderCap, Hello, TcToHub, tc_to_hub};
use kahawai_transport::identity::SatelliteIdentity;
use kahawai_transport::mtls::AllowedCerts;

struct Hub {
    addr: String,
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
    let svc = TranscoderLinkService::new(registry.clone(), sessions);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });
    Hub {
        addr,
        registry,
        allowed,
        ca,
        _pki: pki,
    }
}

fn enroll(hub: &Hub, module_type: &str, module_id: &str, name: &str) -> SatelliteIdentity {
    let bundle = kahawai_core::pki::new_satellite_csr(module_type, module_id, name).unwrap();
    let signed = hub.ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();
    let id = SatelliteIdentity {
        module_id: module_id.to_string(),
        key_pem: bundle.key_pem,
        cert_pem: signed.cert_pem,
        ca_pem: hub.ca.ca_cert_pem().to_string(),
    };
    hub.allowed
        .insert(&kahawai_transport::mtls::cert_fingerprint_pem(&id.cert_pem).unwrap());
    id
}

/// Poll the satellites overview until `check` passes (or panic).
async fn wait_overview(
    registry: &Arc<Registry>,
    check: impl Fn(&[serde_json::Value]) -> bool,
    what: &str,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sats = registry.satellites_overview().await.unwrap();
            if check(&sats) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn open_link(
    hub_addr: &str,
    id: &SatelliteIdentity,
    name: &str,
) -> (
    tokio::sync::mpsc::Sender<TcToHub>,
    tonic::Streaming<kahawai_proto::v1::HubToTc>,
) {
    let tls = kahawai_transport::mtls::mtls_client_config(id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls)
        .await
        .unwrap();
    let mut client = TranscoderLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Hello(Hello {
            protocol_major: kahawai_proto::PROTOCOL_MAJOR,
            protocol_minor: kahawai_proto::PROTOCOL_MINOR,
            name: name.into(),
        })),
    })
    .await
    .unwrap();
    let inbound = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    (tx, inbound)
}

#[tokio::test]
async fn transcoder_registers_capabilities_and_clears_on_disconnect() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "transcoder", "01TC", "gpu-box");
    // The overview reads the satellites table: record enrollment like the
    // real approval path does.
    hub.registry
        .record_satellite("01TC", "transcoder", "gpu-box", "fp-tc")
        .await
        .unwrap();

    let (tx, mut inbound) = open_link(&hub.addr, &id, "gpu-box").await;
    let first = inbound.message().await.unwrap().unwrap();
    assert!(matches!(
        first.msg,
        Some(kahawai_proto::v1::hub_to_tc::Msg::HelloAck(_))
    ));

    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Capabilities(CapabilityReport {
            encoders: vec![
                EncoderCap {
                    codec: "h264".into(),
                    element: "x264enc".into(),
                    hardware: false,
                },
                EncoderCap {
                    codec: "aac".into(),
                    element: "fdkaacenc".into(),
                    hardware: false,
                },
            ],
            max_sessions: 2,
            decode_caps: vec!["video/x-av1".into(), "audio/x-flac".into()],
            tonemap: false,
        })),
    })
    .await
    .unwrap();

    wait_overview(
        &hub.registry,
        |sats| {
            sats.iter().any(|s| {
                s["module_id"] == "01TC"
                    && s["connected"] == true
                    && s["capabilities"]["encoders"][0]["codec"] == "h264"
                    && s["capabilities"]["max_sessions"] == 2
            })
        },
        "capability report in overview",
    )
    .await;

    // Placement finds it — until the admin disables it.
    let need = |video_caps: &[&str]| PlacementNeed {
        encode_video: true,
        encode_audio: true,
        video_caps: video_caps.iter().map(|s| s.to_string()).collect(),
        audio_caps: vec!["audio/x-flac".into()],
        needs_tonemap: false,
    };
    let av1 = need(&["video/x-av1"]);
    assert_eq!(hub.registry.pick_transcoder(&av1).as_deref(), Some("01TC"));
    // Decode fit: a source this box cannot decode is not placeable here.
    assert_eq!(
        hub.registry.pick_transcoder(&need(&["video/x-daala"])),
        None
    );
    // Capacity: max_sessions = 2 is a hard cap.
    hub.registry.tc_session_started("01TC");
    hub.registry.tc_session_started("01TC");
    assert_eq!(hub.registry.pick_transcoder(&av1), None, "at capacity");
    hub.registry.tc_session_ended("01TC");
    assert_eq!(hub.registry.pick_transcoder(&av1).as_deref(), Some("01TC"));

    hub.registry.set_disabled("01TC", true).await.unwrap();
    assert_eq!(hub.registry.pick_transcoder(&av1), None);

    // The drain survives a hub restart: a fresh registry over the same
    // database loads the flag.
    let reborn = Registry::new(hub.registry.db().clone(), Default::default());
    reborn.load_allowlist().await.unwrap();
    let sats = reborn.satellites_overview().await.unwrap();
    assert!(
        sats.iter()
            .any(|s| s["module_id"] == "01TC" && s["disabled"] == true),
        "disabled flag not persisted: {sats:?}"
    );

    hub.registry.set_disabled("01TC", false).await.unwrap();
    assert_eq!(hub.registry.pick_transcoder(&av1).as_deref(), Some("01TC"));

    // Disconnect: capabilities must not outlive the link.
    drop(tx);
    wait_overview(
        &hub.registry,
        |sats| {
            sats.iter().any(|s| {
                s["module_id"] == "01TC" && s["connected"] == false && s["capabilities"].is_null()
            })
        },
        "capabilities cleared on disconnect",
    )
    .await;
}

#[tokio::test]
async fn mediahost_cert_is_refused_on_transcoder_link() {
    let hub = spawn_hub().await;
    let id = enroll(&hub, "mediahost", "01MH", "nas");

    let tls = kahawai_transport::mtls::mtls_client_config(&id).unwrap();
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.addr, tls)
        .await
        .unwrap();
    let mut client = TranscoderLinkClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Hello(Hello {
            protocol_major: kahawai_proto::PROTOCOL_MAJOR,
            protocol_minor: kahawai_proto::PROTOCOL_MINOR,
            name: "nas".into(),
        })),
    })
    .await
    .unwrap();
    let result = client
        .link(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await;
    match result {
        Err(status) => assert_eq!(status.code(), tonic::Code::PermissionDenied),
        Ok(mut stream) => {
            // Some stacks surface the refusal on first read instead.
            let err = stream.get_mut().message().await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied);
        }
    }
}
