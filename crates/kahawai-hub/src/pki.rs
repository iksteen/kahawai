//! Hub-internal certificate authority (SEC-1, §7.1–7.3).
//!
//! Generated on first start under `<data_dir>/pki/`; `ca.key` never leaves
//! this directory. Satellite leaf certs are backdated 24 h (OPS-4) and carry
//! clientAuth+serverAuth EKUs (serverAuth reserved for delegated delivery,
//! AR-8).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use time::{Duration, OffsetDateTime};

pub struct HubCa {
    issuer: Issuer<'static, KeyPair>,
    ca_cert_pem: String,
    ca_fingerprint: String,
}

const CA_VALIDITY_YEARS: i64 = 10;
/// Leaf notBefore backdate for RTC-less satellites (OPS-4).
const LEAF_BACKDATE: Duration = Duration::hours(24);

impl HubCa {
    /// Load the CA from `pki_dir`, creating and persisting it on first start.
    pub fn load_or_create(pki_dir: &Path) -> Result<Self> {
        let key_path = pki_dir.join("ca.key");
        let cert_path = pki_dir.join("ca.crt");
        if !(key_path.exists() && cert_path.exists()) {
            create(pki_dir, &key_path, &cert_path)?;
        }
        let key_pem = fs::read_to_string(&key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        let ca_cert_pem = fs::read_to_string(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key = KeyPair::from_pem(&key_pem).context("parsing ca.key")?;
        let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, key).context("parsing ca.crt")?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(ca_cert_pem.as_bytes())
            .map_err(|e| anyhow::anyhow!("decoding ca.crt PEM: {e}"))?;
        let ca_fingerprint = kahawai_core::pki::cert_fingerprint(&pem.contents);
        tracing::info!(ca = %ca_fingerprint, "hub CA ready");
        Ok(Self { issuer, ca_cert_pem, ca_fingerprint })
    }

    /// The CA certificate satellites pin (SEC-4).
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn ca_fingerprint(&self) -> &str {
        &self.ca_fingerprint
    }

    /// Issue the hub's own leaf server certificate (§7.1), used on the
    /// enrollment/control/byte listeners. Ephemeral per boot — satellites
    /// validate it against the pinned CA, not by fingerprint.
    pub fn issue_server_cert(&self, hostnames: &[String]) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(hostnames.to_vec())?;
        params.distinguished_name.push(DnType::CommonName, "Kahawai Hub");
        let now = OffsetDateTime::now_utc();
        params.not_before = now - LEAF_BACKDATE;
        params.not_after = now + Duration::days(90);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &self.issuer)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Sign an approved satellite CSR (SEC-4). Identity (CN, URI SAN) is taken
    /// from the CSR; validity and EKUs are imposed here — the satellite does
    /// not get a say in them.
    pub fn sign_satellite_csr(&self, csr_der: &[u8], validity_days: u32) -> Result<SignedCert> {
        let mut csr = CertificateSigningRequestParams::from_der(&csr_der.into())
            .context("parsing satellite CSR")?;
        let now = OffsetDateTime::now_utc();
        csr.params.not_before = now - LEAF_BACKDATE;
        csr.params.not_after = now + Duration::days(i64::from(validity_days));
        csr.params.is_ca = IsCa::ExplicitNoCa;
        csr.params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let cert = csr.signed_by(&self.issuer)?;
        Ok(SignedCert {
            fingerprint: kahawai_core::pki::cert_fingerprint(cert.der()),
            cert_pem: cert.pem(),
        })
    }
}

/// Generate the CA keypair + self-signed cert and persist them (first start).
fn create(pki_dir: &Path, key_path: &Path, cert_path: &Path) -> Result<()> {
    fs::create_dir_all(pki_dir).with_context(|| format!("creating {}", pki_dir.display()))?;
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(Vec::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "Kahawai Hub CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let now = OffsetDateTime::now_utc();
    params.not_before = now - LEAF_BACKDATE;
    params.not_after = now + Duration::days(365 * CA_VALIDITY_YEARS);
    let ca = CertifiedIssuer::self_signed(params, key)?;
    write_private(key_path, ca.key().serialize_pem().as_bytes())?;
    fs::write(cert_path, ca.pem()).with_context(|| format!("writing {}", cert_path.display()))?;
    tracing::info!(ca = %kahawai_core::pki::cert_fingerprint(ca.der()), "generated hub CA");
    Ok(())
}

pub struct SignedCert {
    pub cert_pem: String,
    /// sha256(cert_DER) hex — recorded on the satellite row, checked against
    /// the revocation blocklist on every connection (SEC-5/6).
    pub fingerprint: String,
}

/// Write a key file with 0600 permissions (SEC-1).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Convenience: the hub's PKI directory under its data dir.
pub fn pki_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("pki")
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::*;

    #[test]
    fn ca_creates_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let ca = HubCa::load_or_create(dir.path()).unwrap();
        let fp = ca.ca_fingerprint().to_string();
        assert!(dir.path().join("ca.key").exists());
        assert!(dir.path().join("ca.crt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("ca.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Reload gives the same CA, not a new one.
        let reloaded = HubCa::load_or_create(dir.path()).unwrap();
        assert_eq!(reloaded.ca_fingerprint(), fp);
    }

    #[test]
    fn signs_satellite_csr_with_imposed_policy() {
        let dir = tempfile::tempdir().unwrap();
        let ca = HubCa::load_or_create(dir.path()).unwrap();
        let bundle = kahawai_core::pki::new_satellite_csr("mediahost", "01H", "nas").unwrap();
        let signed = ca.sign_satellite_csr(&bundle.csr_der, 90).unwrap();

        let (_, pem) = x509_parser::pem::parse_x509_pem(signed.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&pem.contents).unwrap();
        assert!(cert.issuer().to_string().contains("Kahawai Hub CA"));
        // URI SAN carried over from the CSR.
        let san = cert.subject_alternative_name().unwrap().unwrap();
        assert!(san.value.general_names.iter().any(|n| matches!(
            n, GeneralName::URI(u) if *u == "kahawai://mediahost/01H"
        )));
        // Backdated notBefore (OPS-4) and bounded validity (SEC-7).
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let nb = cert.validity().not_before.timestamp();
        let na = cert.validity().not_after.timestamp();
        assert!(nb <= now - 23 * 3600, "notBefore should be backdated ~24h");
        assert!(na <= now + 91 * 86400 && na >= now + 89 * 86400);
        assert_eq!(signed.fingerprint.len(), 64);
    }

    #[test]
    fn rejects_garbage_csr() {
        let dir = tempfile::tempdir().unwrap();
        let ca = HubCa::load_or_create(dir.path()).unwrap();
        assert!(ca.sign_satellite_csr(b"not a csr", 90).is_err());
    }
}
