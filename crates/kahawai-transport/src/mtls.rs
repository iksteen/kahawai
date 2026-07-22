//! Mutual TLS (SEC-5/6, §7.4).
//!
//! One satellite listener serves everything: connections *may* present a
//! client certificate (enrollment needs none); services that require an
//! identity get it from [`peer_identity`]. A certificate that fails chain
//! validation or is on the revocation blocklist kills the handshake itself,
//! so deleted satellites are refused at the TLS layer (SEC-6).
//!
//! Satellite-side pinning needs no custom verifier: the client's root store
//! contains exactly one anchor — the pinned hub CA — so a foreign CA with
//! the same name fails signature validation (SEC-5).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, DistinguishedName, RootCertStore, ServerConfig};

use crate::identity::SatelliteIdentity;

/// Shared revocation blocklist of cert fingerprints (sha256 hex), consulted
/// on every handshake. Will be backed by the hub's `revoked_certs` table.
#[derive(Clone, Default, Debug)]
pub struct RevocationList(Arc<RwLock<HashSet<String>>>);

impl RevocationList {
    pub fn revoke(&self, fingerprint: &str) {
        self.0.write().unwrap().insert(fingerprint.to_string());
    }

    pub fn contains(&self, fingerprint: &str) -> bool {
        self.0.read().unwrap().contains(fingerprint)
    }
}

fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .context("parsing certificate PEM")
}

/// Hub-side listener config: server cert + optional client auth chained to
/// the hub CA, with revocation checked per handshake.
pub fn mtls_server_config(
    cert_pem: &str,
    key_pem: &str,
    ca_pem: &str,
    revoked: RevocationList,
) -> Result<Arc<ServerConfig>> {
    crate::tls::init_crypto();
    let mut roots = RootCertStore::empty();
    for cert in certs_from_pem(ca_pem)? {
        roots.add(cert).context("adding hub CA to root store")?;
    }
    let inner = WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .context("building client cert verifier")?;
    let verifier = Arc::new(RevocationCheckingVerifier { inner, revoked });

    let certs = certs_from_pem(cert_pem)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("parsing server key")?;
    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("building mTLS server config")?;
    Ok(Arc::new(cfg))
}

/// Satellite-side client config: pinned hub CA as the only trust anchor,
/// presenting the satellite's own certificate.
pub fn mtls_client_config(id: &SatelliteIdentity) -> Result<Arc<ClientConfig>> {
    crate::tls::init_crypto();
    let mut roots = RootCertStore::empty();
    for cert in certs_from_pem(&id.ca_pem)? {
        roots.add(cert).context("adding pinned CA")?;
    }
    let certs = certs_from_pem(&id.cert_pem)?;
    let key = PrivateKeyDer::from_pem_slice(id.key_pem.as_bytes()).context("parsing sat.key")?;
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("building mTLS client config")?;
    Ok(Arc::new(cfg))
}

/// Delegates chain validation to webpki, then rejects revoked fingerprints.
#[derive(Debug)]
struct RevocationCheckingVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    revoked: RevocationList,
}

impl ClientCertVerifier for RevocationCheckingVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self.inner.verify_client_cert(end_entity, intermediates, now)?;
        let fp = kahawai_core::pki::cert_fingerprint(end_entity.as_ref());
        if self.revoked.contains(&fp) {
            tracing::warn!(fingerprint = %fp, "rejected revoked certificate");
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Revoked,
            ));
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The authenticated identity of a connected satellite, read from its
/// certificate's URI SAN (`kahawai://<type>/<id>`, §7.3).
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    pub module_type: String,
    pub module_id: String,
    /// sha256 of the presented leaf DER.
    pub fingerprint: String,
}

/// Extract the peer identity from a request's TLS connect info. `None` when
/// the connection presented no (valid) client certificate.
pub fn peer_identity<T>(request: &tonic::Request<T>) -> Option<PeerIdentity> {
    let certs = request.peer_certs()?;
    let leaf = certs.first()?;
    parse_peer_cert(leaf.as_ref())
}

fn parse_peer_cert(der: &[u8]) -> Option<PeerIdentity> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    let uri = san.value.general_names.iter().find_map(|n| match n {
        GeneralName::URI(u) => Some(*u),
        _ => None,
    })?;
    let rest = uri.strip_prefix("kahawai://")?;
    let (module_type, module_id) = rest.split_once('/')?;
    Some(PeerIdentity {
        module_type: module_type.to_string(),
        module_id: module_id.to_string(),
        fingerprint: kahawai_core::pki::cert_fingerprint(der),
    })
}
