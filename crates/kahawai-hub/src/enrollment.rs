//! Pending-enrollment store and approve-by-code (SEC-2..4, §7.2).
//!
//! CSRs are held unsigned until an administrator types the code printed on
//! the satellite's console. The code is recomputed from the stored CSR — an
//! exact match is required, so a substituted CSR can never be approved with
//! the code the real satellite printed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use kahawai_core::enroll::code_matches;
use kahawai_core::pki::cert_fingerprint;
use x509_parser::prelude::{FromDer, GeneralName, X509CertificationRequest};

use crate::pki::{HubCa, SignedCert};

#[derive(Debug)]
pub struct Pending {
    pub csr_der: Vec<u8>,
    /// sha256(csr_DER) hex — the stable handle shown in the admin UI.
    pub csr_fingerprint: String,
    pub module_type: String,
    pub module_id: String,
    pub name: String,
    submitted_at: Instant,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EnrollError {
    #[error("CSR is not valid or carries no kahawai module identity")]
    InvalidCsr,
    #[error("this CSR is already pending")]
    Duplicate,
    #[error("too many pending enrollments")]
    Full,
    #[error("no pending enrollment matches that code")]
    NoMatch,
}

pub struct Enrollments {
    pending: Vec<Pending>,
    ttl: Duration,
    max_pending: usize,
}

impl Enrollments {
    pub fn new(ttl: Duration) -> Self {
        // ponytail: fixed cap; per-source rate limiting arrives with the
        // network listener (SEC-3 logs/limits are transport concerns).
        Self {
            pending: Vec::new(),
            ttl,
            max_pending: 32,
        }
    }

    /// Store a submitted CSR as pending (SEC-3: never auto-signed).
    pub fn submit(&mut self, csr_der: Vec<u8>) -> Result<&Pending, EnrollError> {
        self.expire();
        let (module_type, module_id, name) =
            parse_identity(&csr_der).ok_or(EnrollError::InvalidCsr)?;
        let csr_fingerprint = cert_fingerprint(&csr_der);
        if self
            .pending
            .iter()
            .any(|p| p.csr_fingerprint == csr_fingerprint)
        {
            return Err(EnrollError::Duplicate);
        }
        if self.pending.len() >= self.max_pending {
            return Err(EnrollError::Full);
        }
        tracing::info!(%module_type, %module_id, %name, fp = %csr_fingerprint, "enrollment submitted");
        self.pending.push(Pending {
            csr_der,
            csr_fingerprint,
            module_type,
            module_id,
            name,
            submitted_at: Instant::now(),
        });
        Ok(self.pending.last().unwrap())
    }

    /// Approve by console code: sign the matching CSR and return the cert
    /// bundle for the satellite. No match → error, nothing signed (SEC-3).
    pub fn approve(&mut self, code: &str, ca: &HubCa, validity_days: u32) -> Result<Approved> {
        self.expire();
        let idx = match self
            .pending
            .iter()
            .position(|p| code_matches(code, &p.csr_der))
        {
            Some(idx) => idx,
            None => {
                // §7.2: a code the admin can't confirm marks the pending CSR
                // as suspect (possible substitution) — remove it. Only when
                // it's unambiguous which one was meant; a typo must not nuke
                // unrelated concurrent enrollments.
                if self.pending.len() == 1 {
                    let p = self.pending.remove(0);
                    tracing::warn!(fp = %p.csr_fingerprint, "enrollment rejected: code mismatch");
                }
                return Err(EnrollError::NoMatch.into());
            }
        };
        let p = self.pending.remove(idx);
        let signed = ca
            .sign_satellite_csr(&p.csr_der, validity_days)
            .context("signing approved CSR")?;
        tracing::info!(module_type = %p.module_type, module_id = %p.module_id,
            cert_fp = %signed.fingerprint, "enrollment approved");
        Ok(Approved { pending: p, signed })
    }

    /// Pending list for the admin UI/CLI, oldest first.
    pub fn list(&mut self) -> &[Pending] {
        self.expire();
        &self.pending
    }

    fn expire(&mut self) {
        let ttl = self.ttl;
        self.pending.retain(|p| p.submitted_at.elapsed() < ttl);
    }
}

pub struct Approved {
    pub pending: Pending,
    pub signed: SignedCert,
}

/// Extract `(module_type, module_id, name)` from the CSR's URI SAN
/// `kahawai://<type>/<id>` and CN. Verifies the CSR's self-signature.
fn parse_identity(csr_der: &[u8]) -> Option<(String, String, String)> {
    let (_, csr) = X509CertificationRequest::from_der(csr_der).ok()?;
    csr.verify_signature().ok()?;
    let info = &csr.certification_request_info;
    let name = info
        .subject
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())?
        .to_string();
    let uri = csr.requested_extensions()?.find_map(|ext| {
        if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) = ext {
            san.general_names.iter().find_map(|n| match n {
                GeneralName::URI(u) => Some(u.to_string()),
                _ => None,
            })
        } else {
            None
        }
    })?;
    let rest = uri.strip_prefix("kahawai://")?;
    let (module_type, module_id) = rest.split_once('/')?;
    if !matches!(module_type, "mediahost" | "transcoder") || module_id.is_empty() {
        return None;
    }
    Some((module_type.to_string(), module_id.to_string(), name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_core::enroll::enrollment_code;
    use kahawai_core::pki::new_satellite_csr;

    fn ca() -> (tempfile::TempDir, HubCa) {
        let dir = tempfile::tempdir().unwrap();
        let ca = HubCa::load_or_create(dir.path()).unwrap();
        (dir, ca)
    }

    #[test]
    fn happy_path_submit_approve() {
        let (_d, ca) = ca();
        let mut e = Enrollments::new(Duration::from_secs(900));
        let bundle = new_satellite_csr("mediahost", "01H", "nas").unwrap();
        let code = enrollment_code(&bundle.csr_der);

        let p = e.submit(bundle.csr_der.clone()).unwrap();
        assert_eq!(
            (
                p.module_type.as_str(),
                p.module_id.as_str(),
                p.name.as_str()
            ),
            ("mediahost", "01H", "nas")
        );

        let approved = e.approve(&code, &ca, 90).unwrap();
        assert!(approved.signed.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(e.list().is_empty());
    }

    #[test]
    fn wrong_code_rejects_and_removes_sole_pending() {
        let (_d, ca) = ca();
        let mut e = Enrollments::new(Duration::from_secs(900));
        let bundle = new_satellite_csr("transcoder", "01T", "gpu-box").unwrap();
        e.submit(bundle.csr_der).unwrap();
        assert!(e.approve("AAAA-AAAA", &ca, 90).is_err());
        assert!(e.list().is_empty(), "rejected enrollment is removed (§7.2)");
    }

    #[test]
    fn wrong_code_with_multiple_pending_removes_nothing() {
        let (_d, ca) = ca();
        let mut e = Enrollments::new(Duration::from_secs(900));
        e.submit(new_satellite_csr("mediahost", "01A", "a").unwrap().csr_der)
            .unwrap();
        e.submit(new_satellite_csr("mediahost", "01B", "b").unwrap().csr_der)
            .unwrap();
        assert!(e.approve("AAAA-AAAA", &ca, 90).is_err());
        assert_eq!(
            e.list().len(),
            2,
            "a typo must not nuke unrelated enrollments"
        );
    }

    #[test]
    fn substituted_csr_cannot_use_original_code() {
        // MITM swaps the CSR in flight: the admin still types the code from
        // the real satellite's console, which cannot match the substitute.
        let (_d, ca) = ca();
        let mut e = Enrollments::new(Duration::from_secs(900));
        let real = new_satellite_csr("mediahost", "01H", "nas").unwrap();
        let evil = new_satellite_csr("mediahost", "01H", "nas").unwrap();
        e.submit(evil.csr_der).unwrap();
        assert!(e.approve(&enrollment_code(&real.csr_der), &ca, 90).is_err());
    }

    #[test]
    fn rejects_foreign_and_duplicate_csrs() {
        let (_d, _ca) = ca();
        let mut e = Enrollments::new(Duration::from_secs(900));
        assert_eq!(
            e.submit(b"garbage".to_vec()).unwrap_err(),
            EnrollError::InvalidCsr
        );

        let bundle = new_satellite_csr("mediahost", "01H", "nas").unwrap();
        e.submit(bundle.csr_der.clone()).unwrap();
        assert_eq!(
            e.submit(bundle.csr_der).unwrap_err(),
            EnrollError::Duplicate
        );
    }

    #[test]
    fn pending_expires() {
        let (_d, _ca) = ca();
        let mut e = Enrollments::new(Duration::ZERO);
        let bundle = new_satellite_csr("mediahost", "01H", "nas").unwrap();
        e.submit(bundle.csr_der).unwrap();
        assert!(e.list().is_empty());
    }
}
