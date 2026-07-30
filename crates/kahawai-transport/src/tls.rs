//! TLS plumbing shared by hub and satellites.
//!
//! The enrollment channel is deliberately server-unverified (§7.2): the
//! satellite has nothing to pin yet, and the console code — not the channel —
//! is what defeats substitution. Everything after enrollment is mTLS (SEC-5),
//! which lands with the link services.

use std::io;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;

/// Install the ring crypto provider exactly once, before any rustls config is
/// built. Safe to call from every entry point.
pub fn init_crypto() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("crypto provider installed twice");
    });
}

/// Server TLS with no client-cert requirement — the enrollment listener only.
pub fn server_config(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>> {
    init_crypto();
    let certs: Vec<CertificateDer> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .context("parsing server cert PEM")?;
    let key =
        PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("parsing server key PEM")?;
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building server TLS config")?;
    Ok(Arc::new(cfg))
}

/// Accept TCP connections, perform TLS handshakes off the accept loop, and
/// yield established streams — the `serve_with_incoming` input for tonic.
pub fn tls_incoming(
    tcp: TcpListener,
    tls: Arc<ServerConfig>,
) -> ReceiverStream<Result<tokio_rustls::server::TlsStream<TcpStream>, io::Error>> {
    let acceptor = TlsAcceptor::from(tls);
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            let (stream, peer) = match tcp.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        let _ = tx.send(Ok(tls_stream)).await;
                    }
                    Err(e) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                }
            });
        }
    });
    ReceiverStream::new(rx)
}

/// gRPC channel over TLS with **no server verification** — enrollment only.
pub async fn grpc_channel_unverified(addr: &str) -> Result<tonic::transport::Channel> {
    init_crypto();
    let tls = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoServerVerify::new()))
        .with_no_client_auth();
    grpc_channel_with(addr, Arc::new(tls)).await
}

/// gRPC channel over TLS using the given client config (mTLS later).
pub async fn grpc_channel_with(
    addr: &str,
    tls: Arc<ClientConfig>,
) -> Result<tonic::transport::Channel> {
    use hyper_util::rt::TokioIo;
    let addr = addr.to_string();
    let connector = tower::service_fn(move |_uri: tonic::transport::Uri| {
        let addr = addr.clone();
        let tls = tls.clone();
        async move {
            let tcp = TcpStream::connect(&addr).await?;
            let host = addr
                .rsplit_once(':')
                .map(|(h, _)| h)
                .unwrap_or(&addr)
                .to_string();
            let sni = ServerName::try_from(host)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            let stream = tokio_rustls::TlsConnector::from(tls)
                .connect(sni, tcp)
                .await?;
            Ok::<_, io::Error>(TokioIo::new(stream))
        }
    });
    // Placeholder URI — the connector dials the real address and does the
    // TLS itself, so this stays `http` (h2 prior knowledge over our stream).
    let channel = tonic::transport::Endpoint::try_from("http://kahawai.invalid")?
        .connect_with_connector(connector)
        .await
        .context("connecting to hub")?;
    Ok(channel)
}

/// Accepts any server certificate. Used exclusively for the enrollment
/// channel, where the console-code commitment provides the authenticity.
#[derive(Debug)]
struct NoServerVerify {
    schemes: Vec<rustls::SignatureScheme>,
}

impl NoServerVerify {
    fn new() -> Self {
        Self {
            schemes: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for NoServerVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}
