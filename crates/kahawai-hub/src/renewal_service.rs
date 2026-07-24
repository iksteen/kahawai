//! SEC-7: certificate renewal over the already-authenticated mTLS channel.
//!
//! The connection's client certificate is the authorization — a satellite
//! can only renew the identity it presented. The new fingerprint is
//! admitted (DB + live allowlist) before the certificate is returned, so a
//! renewal can never lock its satellite out.

use std::sync::Arc;

use kahawai_proto::v1::renewal_server::{Renewal, RenewalServer};
use kahawai_proto::v1::{RenewRequest, RenewResponse};
use kahawai_transport::mtls::peer_identity;
use tonic::{Request, Response, Status};

use crate::pki::HubCa;
use crate::registry::Registry;

pub struct RenewalService {
    ca: Arc<HubCa>,
    registry: Arc<Registry>,
    cert_days: u32,
}

impl RenewalService {
    pub fn new(ca: Arc<HubCa>, registry: Arc<Registry>, cert_days: u32) -> Self {
        Self { ca, registry, cert_days }
    }

    pub fn into_server(self) -> RenewalServer<Self> {
        RenewalServer::new(self)
    }
}

#[tonic::async_trait]
impl Renewal for RenewalService {
    async fn renew(
        &self,
        request: Request<RenewRequest>,
    ) -> Result<Response<RenewResponse>, Status> {
        let peer = peer_identity(&request)
            .ok_or_else(|| Status::unauthenticated("renewal requires a client certificate"))?;
        let csr = request.into_inner().csr_der;
        let claimed = crate::pki::csr_module_uri(&csr)
            .map_err(|e| Status::invalid_argument(format!("bad CSR: {e:#}")))?;
        let expected = kahawai_core::pki::module_uri(&peer.module_type, &peer.module_id);
        if claimed.as_deref() != Some(expected.as_str()) {
            tracing::warn!(module_id = %peer.module_id, ?claimed, "renewal CSR identity mismatch");
            return Err(Status::permission_denied("CSR identity does not match the connection"));
        }
        let signed = self
            .ca
            .sign_satellite_csr(&csr, self.cert_days)
            .map_err(|e| Status::internal(format!("signing renewal: {e:#}")))?;
        self.registry
            .record_renewal(&peer.module_id, &signed.fingerprint)
            .await
            .map_err(|e| Status::internal(format!("recording renewal: {e:#}")))?;
        tracing::info!(module_id = %peer.module_id, fingerprint = %signed.fingerprint, "renewed satellite certificate issued");
        Ok(Response::new(RenewResponse { cert_pem: signed.cert_pem }))
    }
}
