//! Satellite-side enrollment (SEC-2/4, §7.2): generate a keypair locally,
//! submit the CSR, print the console code, poll until an admin approves.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kahawai_core::enroll::enrollment_code;
use kahawai_core::pki::new_satellite_csr;
use kahawai_proto::v1::enrollment_client::EnrollmentClient;
use kahawai_proto::v1::{StatusRequest, SubmitRequest, status_response};

use crate::identity::{self, SatelliteIdentity};

/// Tolerated |hub - local| clock difference before we warn (OPS-4).
const CLOCK_SKEW_WARN: i64 = 5 * 60;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Return the persisted identity, or run the enrollment flow until an
/// administrator approves us. Blocks (polling) as long as it takes.
pub async fn ensure_identity(
    hub_addr: &str,
    state_dir: &Path,
    module_type: &str,
    name: &str,
) -> Result<SatelliteIdentity> {
    if let Some(id) = identity::load(state_dir)? {
        tracing::info!(module_id = %id.module_id, "using enrolled identity");
        return Ok(id);
    }

    let module_id = ulid::Ulid::generate().to_string();
    let bundle = new_satellite_csr(module_type, &module_id, name)?;
    let code = enrollment_code(&bundle.csr_der);
    // The one thing the human at this console must see (SEC-2):
    println!(
        "\n  Enrollment code: {code}\n  Enter this code on the hub to approve this {module_type}.\n"
    );

    let channel = crate::tls::grpc_channel_unverified(hub_addr).await?;
    let mut client = EnrollmentClient::new(channel);

    loop {
        match client
            .submit(SubmitRequest {
                csr_der: bundle.csr_der.clone(),
            })
            .await
        {
            Ok(_) => {}
            // Already pending (e.g. we polled, hub still has it) — fine.
            Err(s) if s.code() == tonic::Code::AlreadyExists => {}
            Err(s) => bail!("enrollment submit failed: {s}"),
        }

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp = client
                .status(StatusRequest {
                    csr_fingerprint: kahawai_core::pki::cert_fingerprint(&bundle.csr_der),
                })
                .await
                .context("polling enrollment status")?
                .into_inner();

            let local = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let skew = local - resp.hub_unix_time;
            if skew.abs() > CLOCK_SKEW_WARN {
                tracing::warn!(
                    "clock skew: local is {} min {} the hub — fix NTP or certificate validation may fail",
                    skew.abs() / 60,
                    if skew > 0 { "ahead of" } else { "behind" }
                );
            }

            match resp.state {
                Some(status_response::State::Approved(a)) => {
                    let id = SatelliteIdentity {
                        module_id,
                        key_pem: bundle.key_pem,
                        cert_pem: a.cert_pem,
                        ca_pem: a.ca_cert_pem,
                    };
                    identity::store(state_dir, &id)?;
                    tracing::info!(module_id = %id.module_id, "enrollment approved; identity persisted");
                    return Ok(id);
                }
                Some(status_response::State::Pending(_)) => continue,
                // Expired or rejected: resubmit the same CSR — the code on
                // this console stays valid, and only an admin typing it can
                // ever approve us.
                Some(status_response::State::Rejected(r)) => {
                    tracing::warn!(reason = %r.reason, "enrollment not pending; resubmitting");
                    break;
                }
                None => bail!("hub sent an empty enrollment status"),
            }
        }
    }
}
