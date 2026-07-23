//! Transcoder module (§6, M3): enroll once, keep a control link to the
//! hub (AR-3: always dials out), and register what this box can encode —
//! every element dry-run-verified at startup (TC-1), so a broken driver
//! is discovered at registration, not mid-session.
//!
//! ponytail: this slice is registration only; session dispatch and the
//! video pipeline land next.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::transcoder_link_client::TranscoderLinkClient;
use kahawai_proto::v1::{hub_to_tc, tc_to_hub, CapabilityReport, EncoderCap, Heartbeat, Hello, TcToHub};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use tokio_stream::wrappers::ReceiverStream;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Enroll (or load identity) and keep the hub link up forever.
pub async fn run(hub_addr: &str, state_dir: &Path, name: &str, max_sessions: u32) -> Result<()> {
    let id =
        kahawai_transport::enroll::ensure_identity(hub_addr, state_dir, "transcoder", name).await?;
    let tls = kahawai_transport::mtls::mtls_client_config(&id)?;

    let capabilities = probe_capabilities(max_sessions)?;

    loop {
        match link_once(hub_addr, tls.clone(), name, capabilities.clone()).await {
            Ok(()) => tracing::warn!("hub closed the link; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed; reconnecting"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// TC-1 capability probe: what this machine can verifiably encode.
fn probe_capabilities(max_sessions: u32) -> Result<CapabilityReport> {
    let encoders: Vec<EncoderCap> = kahawai_media::remux::encoder_capabilities()
        .into_iter()
        .map(|(codec, element)| {
            tracing::info!(codec, element, "encoder verified");
            EncoderCap { codec: codec.into(), element: element.into() }
        })
        .collect();
    if encoders.is_empty() {
        bail!("no working encoders on this machine — see `kahawai doctor`");
    }
    Ok(CapabilityReport { encoders, max_sessions })
}

/// One link session: Hello/HelloAck, capability registration, then
/// heartbeats until the stream dies.
async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    capabilities: CapabilityReport,
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    let mut client = TranscoderLinkClient::new(channel);

    let (tx, rx) = tokio::sync::mpsc::channel::<TcToHub>(16);
    tx.send(TcToHub {
        msg: Some(tc_to_hub::Msg::Hello(Hello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            name: name.to_string(),
        })),
    })
    .await
    .ok();

    let mut inbound = client
        .link(ReceiverStream::new(rx))
        .await
        .context("opening link")?
        .into_inner();

    match inbound.message().await.context("awaiting HelloAck")? {
        Some(m) => match m.msg {
            Some(hub_to_tc::Msg::HelloAck(ack)) => {
                tracing::info!(
                    hub_protocol = format!("{}.{}", ack.protocol_major, ack.protocol_minor),
                    "link established"
                );
            }
            _ => bail!("hub did not open with HelloAck"),
        },
        None => bail!("hub closed the link before HelloAck"),
    }

    tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Capabilities(capabilities)) })
        .await
        .context("link closed before capability report")?;

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if tx.send(TcToHub { msg: Some(tc_to_hub::Msg::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    bail!("link sender closed");
                }
            }
            msg = inbound.message() => {
                match msg {
                    Ok(Some(_)) => {} // session dispatch: next slice
                    Ok(None) => return Ok(()),
                    Err(e) => bail!("link stream error: {e}"),
                }
            }
        }
    }
}
