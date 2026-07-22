//! Mediahost module: enroll once, then keep a control link to the hub
//! (AR-3: always dials out) and scan collections up to it.

pub mod scan;

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{host_to_hub, hub_to_host, AnnounceCollection, Heartbeat, Hello, HostToHub};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use scan::CollectionConfig;
use tokio_stream::wrappers::ReceiverStream;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Enroll (or load identity) and keep the hub link up forever.
pub async fn run(
    hub_addr: &str,
    state_dir: &Path,
    name: &str,
    collections: Vec<CollectionConfig>,
) -> Result<()> {
    let id = kahawai_transport::enroll::ensure_identity(hub_addr, state_dir, "mediahost", name)
        .await?;
    let tls = kahawai_transport::mtls::mtls_client_config(&id)?;

    loop {
        match link_once(hub_addr, tls.clone(), name, &collections).await {
            Ok(()) => tracing::warn!("hub closed the link; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed; reconnecting"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// One link session: Hello/HelloAck, announce + scan collections, then
/// heartbeats until the stream dies.
async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    collections: &[CollectionConfig],
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    let mut client = MediahostLinkClient::new(channel);

    let (tx, rx) = tokio::sync::mpsc::channel::<HostToHub>(16);
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::Hello(Hello {
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

    // Hub speaks first: HelloAck with its protocol version (AR-7).
    match inbound.message().await.context("awaiting HelloAck")? {
        Some(m) => match m.msg {
            Some(hub_to_host::Msg::HelloAck(ack)) => {
                tracing::info!(
                    hub_protocol = format!("{}.{}", ack.protocol_major, ack.protocol_minor),
                    "link established"
                );
            }
            None => bail!("hub sent an empty first message"),
        },
        None => bail!("hub closed the link before HelloAck"),
    }

    for c in collections {
        tx.send(HostToHub {
            msg: Some(host_to_hub::Msg::AnnounceCollection(AnnounceCollection {
                id: c.name.clone(),
                media_type: c.media_type.clone(),
                roots: c.roots.iter().map(|r| r.display().to_string()).collect(),
            })),
        })
        .await
        .context("link closed before announce")?;
        // ponytail: rescan on every (re)connect; incremental journal later.
        tokio::spawn(scan::scan_collection(c.clone(), tx.clone()));
    }

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if tx.send(HostToHub { msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    bail!("link sender closed");
                }
            }
            msg = inbound.message() => {
                match msg {
                    Ok(Some(_)) => {} // no hub→host commands defined yet
                    Ok(None) => return Ok(()),
                    Err(e) => bail!("link stream error: {e}"),
                }
            }
        }
    }
}
