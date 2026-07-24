//! Satellite-side certificate renewal (SEC-7): when less than
//! [`RENEW_WINDOW_DAYS`] of validity remain, submit a fresh CSR (new
//! keypair) over the authenticated channel and atomically persist the
//! renewed identity. The hub admits the new fingerprint before returning
//! the certificate, so the reconnect that follows can never be refused.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use kahawai_core::pki::new_satellite_csr;
use kahawai_proto::v1::renewal_client::RenewalClient;
use kahawai_proto::v1::RenewRequest;

use crate::identity::{self, SatelliteIdentity};

pub const RENEW_WINDOW_DAYS: i64 = 30;

/// Seconds until this certificate enters the renewal window (negative:
/// already inside it).
pub fn seconds_until_renewal_due(cert_pem: &str) -> Result<i64> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow::anyhow!("decoding certificate PEM: {e}"))?;
    let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents)
        .map_err(|e| anyhow::anyhow!("parsing certificate: {e}"))?;
    let not_after = cert.validity().not_after.timestamp();
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    Ok(not_after - RENEW_WINDOW_DAYS * 86_400 - now)
}

/// Renew if the persisted certificate is inside the renewal window.
/// Returns the new identity when a renewal happened.
pub async fn maybe_renew(
    hub_addr: &str,
    state_dir: &Path,
    module_type: &str,
    name: &str,
) -> Result<Option<SatelliteIdentity>> {
    let id = identity::load(state_dir)?.context("not enrolled")?;
    let due_in = seconds_until_renewal_due(&id.cert_pem)?;
    // KAHAWAI_FORCE_RENEW: ops lever — renew now regardless of the window
    // (planned migrations, drills). The satellite consumes it every start.
    if due_in > 0 && std::env::var_os("KAHAWAI_FORCE_RENEW").is_none() {
        return Ok(None);
    }
    tracing::info!(module_id = %id.module_id, "certificate inside renewal window; renewing");

    let bundle = new_satellite_csr(module_type, &id.module_id, name)?;
    let tls = crate::mtls::mtls_client_config(&id)?;
    let channel = crate::tls::grpc_channel_with(hub_addr, tls).await?;
    let resp = RenewalClient::new(channel)
        .renew(RenewRequest { csr_der: bundle.csr_der })
        .await
        .context("renewal request")?
        .into_inner();

    let renewed = SatelliteIdentity {
        module_id: id.module_id,
        key_pem: bundle.key_pem,
        cert_pem: resp.cert_pem,
        ca_pem: id.ca_pem,
    };
    identity::store_renewal(state_dir, &renewed)?;
    tracing::info!(module_id = %renewed.module_id, "renewed certificate persisted");
    Ok(Some(renewed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert_expiring_in(days: i64) -> String {
        use rcgen::{CertificateParams, KeyPair};
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["test".into()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(days);
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn renewal_window_boundary() {
        // 60 days left: due in ~30 days.
        let due = seconds_until_renewal_due(&cert_expiring_in(60)).unwrap();
        assert!(due > 29 * 86_400 && due < 31 * 86_400, "due={due}");
        // 10 days left: overdue.
        let due = seconds_until_renewal_due(&cert_expiring_in(10)).unwrap();
        assert!(due < 0, "due={due}");
    }
}
