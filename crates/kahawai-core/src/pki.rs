//! Satellite-side PKI: keypair + CSR generation (SEC-2) and cert fingerprinting.
//!
//! The private key is generated here and never leaves the satellite; only the
//! CSR crosses the wire. The hub-side CA lives in `kahawai-hub::pki`.

use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use sha2::{Digest, Sha256};

/// `kahawai://<module_type>/<module_id>` — the URI SAN that *is* the
/// satellite's identity on every future mTLS connection (§7.3).
pub fn module_uri(module_type: &str, module_id: &str) -> String {
    format!("kahawai://{module_type}/{module_id}")
}

pub struct CsrBundle {
    /// PKCS#8 PEM. Persist with restrictive permissions; never transmit.
    pub key_pem: String,
    pub csr_der: Vec<u8>,
    pub csr_pem: String,
}

/// Generate a fresh P-256 keypair and a CSR identifying the module.
pub fn new_satellite_csr(
    module_type: &str,
    module_id: &str,
    name: &str,
) -> Result<CsrBundle, rcgen::Error> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(Vec::new())?;
    params.distinguished_name.push(DnType::CommonName, name);
    params
        .subject_alt_names
        .push(SanType::URI(module_uri(module_type, module_id).try_into()?));
    let csr = params.serialize_request(&key)?;
    Ok(CsrBundle {
        key_pem: key.serialize_pem(),
        csr_der: csr.der().to_vec(),
        csr_pem: csr.pem()?,
    })
}

/// `sha256(cert_DER)` as lowercase hex — the identity used by the revocation
/// blocklist (SEC-5/6) and shown in the admin UI.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    let digest = Sha256::digest(cert_der);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_roundtrip_and_identity() {
        let bundle = new_satellite_csr("mediahost", "01ABCDEF", "nas-basement").unwrap();
        assert!(bundle.key_pem.contains("PRIVATE KEY"));
        assert!(bundle.csr_pem.contains("CERTIFICATE REQUEST"));
        // Two enrollments never share a key or CSR.
        let other = new_satellite_csr("mediahost", "01ABCDEF", "nas-basement").unwrap();
        assert_ne!(bundle.csr_der, other.csr_der);
    }

    #[test]
    fn fingerprint_is_hex_sha256() {
        let fp = cert_fingerprint(b"der-bytes");
        assert_eq!(fp.len(), 64);
        assert_eq!(fp, cert_fingerprint(b"der-bytes"));
        assert_ne!(fp, cert_fingerprint(b"other"));
    }
}
