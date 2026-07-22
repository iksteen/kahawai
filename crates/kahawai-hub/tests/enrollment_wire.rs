//! End-to-end enrollment over real TLS sockets (SEC-2..4, §7.2):
//! satellite generates a key locally, submits its CSR, the "admin" enters the
//! console code, the satellite persists cert + pinned CA.

use std::sync::Arc;
use std::time::Duration;

use kahawai_core::enroll::enrollment_code;
use kahawai_hub::enrollment_service::EnrollmentService;
use kahawai_hub::pki::HubCa;

async fn spawn_hub(svc: EnrollmentService, ca: &HubCa) -> std::net::SocketAddr {
    let (cert_pem, key_pem) = ca.issue_server_cert(&["localhost".into()]).unwrap();
    let tls = kahawai_transport::tls::server_config(&cert_pem, &key_pem).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(svc.into_server())
            .serve_with_incoming(kahawai_transport::tls::tls_incoming(listener, tls))
            .await
            .unwrap();
    });
    addr
}

/// Bounded wait — a broken flow must fail the test, never hang it.
async fn wait_for_pending(svc: &EnrollmentService) -> Vec<kahawai_hub::enrollment_service::PendingInfo> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let p = svc.pending();
            if !p.is_empty() {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no enrollment arrived within 10s")
}

#[tokio::test]
async fn full_enrollment_flow() {
    let pki = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let ca = Arc::new(HubCa::load_or_create(pki.path()).unwrap());
    let db = kahawai_hub::db::open_in_memory().await.unwrap();
    let registry = Arc::new(kahawai_hub::registry::Registry::new(db));
    let svc = EnrollmentService::new(ca.clone(), registry.clone(), Duration::from_secs(900), 90);
    let addr = spawn_hub(svc.clone(), &ca).await;

    // Satellite enrolls in the background.
    let state_dir = state.path().to_path_buf();
    let hub_addr = format!("localhost:{}", addr.port());
    let satellite = tokio::spawn(async move {
        kahawai_transport::enroll::ensure_identity(&hub_addr, &state_dir, "mediahost", "nas")
            .await
            .unwrap()
    });

    // "Admin": wait for the CSR to arrive, read the code off the (simulated)
    // satellite console — derived from the same CSR — and approve it.
    let pending = wait_for_pending(&svc).await;
    assert_eq!(pending[0].module_type, "mediahost");
    assert_eq!(pending[0].name, "nas");

    // A wrong code approves nothing and (sole pending) rejects the CSR…
    assert!(svc.approve("AAAA-AAAA").await.is_err());
    // …after which the satellite resubmits the same CSR on its own.
    let pending = wait_for_pending(&svc).await;
    let code = enrollment_code(&pending[0].csr_der);
    svc.approve(&code).await.unwrap();
    // Approval records the satellite row (SEC-4 bookkeeping).
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM satellites WHERE module_type = 'mediahost'")
        .fetch_one(registry.db())
        .await
        .unwrap();
    assert_eq!(n, 1);

    let id = tokio::time::timeout(Duration::from_secs(15), satellite)
        .await
        .expect("satellite should enroll before timeout")
        .unwrap();

    // Identity persisted: key stays local (0600), CA pinned.
    assert_eq!(id.ca_pem, ca.ca_cert_pem());
    assert!(id.cert_pem.contains("BEGIN CERTIFICATE"));
    let reloaded = kahawai_transport::identity::load(state.path()).unwrap().unwrap();
    assert_eq!(reloaded.module_id, id.module_id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(state.path().join("sat.key")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Second start: no network, identity comes from disk.
    let id2 = kahawai_transport::enroll::ensure_identity(
        "localhost:1", // unreachable on purpose
        state.path(),
        "mediahost",
        "nas",
    )
    .await
    .unwrap();
    assert_eq!(id2.module_id, id.module_id);
}
