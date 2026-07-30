//! gRPC surface for enrollment (§7.2): the only service reachable without a
//! client certificate. Accepts nothing but Submit/Status; never signs
//! anything without an admin-entered code.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kahawai_proto::v1::enrollment_server::{Enrollment as EnrollmentRpc, EnrollmentServer};
use kahawai_proto::v1::{
    Approved as ApprovedMsg, Pending as PendingMsg, Rejected, StatusRequest, StatusResponse,
    SubmitRequest, SubmitResponse, status_response,
};
use tonic::{Request, Response, Status};

use crate::enrollment::{EnrollError, Enrollments};
use crate::pki::HubCa;

pub struct PendingInfo {
    pub csr_fingerprint: String,
    pub module_type: String,
    pub module_id: String,
    pub name: String,
    pub csr_der: Vec<u8>,
}

struct Inner {
    enrollments: Enrollments,
    /// Approved certs waiting for the satellite to poll them up, by CSR
    /// fingerprint. Kept until fetched or TTL.
    approved: HashMap<String, (String, Instant)>,
}

#[derive(Clone)]
pub struct EnrollmentService {
    inner: Arc<Mutex<Inner>>,
    ca: Arc<HubCa>,
    registry: Arc<crate::registry::Registry>,
    ttl: Duration,
    validity_days: u32,
}

impl EnrollmentService {
    pub fn new(
        ca: Arc<HubCa>,
        registry: Arc<crate::registry::Registry>,
        ttl: Duration,
        validity_days: u32,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                enrollments: Enrollments::new(ttl),
                approved: HashMap::new(),
            })),
            ca,
            registry,
            ttl,
            validity_days,
        }
    }

    pub fn into_server(self) -> EnrollmentServer<Self> {
        EnrollmentServer::new(self)
    }

    /// Admin entry point (SEC-3): approve by console code. Records the
    /// satellite row (module id, name, cert fingerprint) on success.
    pub async fn approve(&self, code: &str) -> anyhow::Result<String> {
        let (summary, pending, fingerprint) = {
            let mut inner = self.inner.lock().unwrap();
            let approved = inner
                .enrollments
                .approve(code, &self.ca, self.validity_days)?;
            let summary = format!(
                "{} \"{}\" ({}) cert {}",
                approved.pending.module_type,
                approved.pending.name,
                approved.pending.module_id,
                &approved.signed.fingerprint[..16],
            );
            inner.approved.insert(
                approved.pending.csr_fingerprint.clone(),
                (approved.signed.cert_pem, Instant::now()),
            );
            (summary, approved.pending, approved.signed.fingerprint)
        };
        self.registry
            .record_satellite(
                &pending.module_id,
                &pending.module_type,
                &pending.name,
                &fingerprint,
            )
            .await?;
        Ok(summary)
    }

    /// Pending enrollments for the admin surface.
    pub fn pending(&self) -> Vec<PendingInfo> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .enrollments
            .list()
            .iter()
            .map(|p| PendingInfo {
                csr_fingerprint: p.csr_fingerprint.clone(),
                module_type: p.module_type.clone(),
                module_id: p.module_id.clone(),
                name: p.name.clone(),
                csr_der: p.csr_der.clone(),
            })
            .collect()
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tonic::async_trait]
impl EnrollmentRpc for EnrollmentService {
    async fn submit(
        &self,
        request: Request<SubmitRequest>,
    ) -> Result<Response<SubmitResponse>, Status> {
        let csr_der = request.into_inner().csr_der;
        let mut inner = self.inner.lock().unwrap();
        match inner.enrollments.submit(csr_der) {
            Ok(p) => Ok(Response::new(SubmitResponse {
                csr_fingerprint: p.csr_fingerprint.clone(),
                ttl_seconds: self.ttl.as_secs(),
            })),
            Err(EnrollError::Duplicate) => Err(Status::already_exists("already pending")),
            Err(EnrollError::Full) => {
                Err(Status::resource_exhausted("too many pending enrollments"))
            }
            Err(e) => Err(Status::invalid_argument(e.to_string())),
        }
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let fp = request.into_inner().csr_fingerprint;
        let mut inner = self.inner.lock().unwrap();
        let ttl = self.ttl;
        inner.approved.retain(|_, (_, at)| at.elapsed() < ttl);

        let state = if let Some((cert_pem, _)) = inner.approved.get(&fp) {
            status_response::State::Approved(ApprovedMsg {
                cert_pem: cert_pem.clone(),
                ca_cert_pem: self.ca.ca_cert_pem().to_string(),
            })
        } else if inner
            .enrollments
            .list()
            .iter()
            .any(|p| p.csr_fingerprint == fp)
        {
            status_response::State::Pending(PendingMsg {})
        } else {
            // Deliberately vague (SEC-3: never hint how close anything was).
            status_response::State::Rejected(Rejected {
                reason: "unknown, expired, or rejected".into(),
            })
        };
        Ok(Response::new(StatusResponse {
            state: Some(state),
            hub_unix_time: unix_now(),
        }))
    }
}
