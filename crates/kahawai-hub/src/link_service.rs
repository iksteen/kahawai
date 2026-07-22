//! MediahostLink: the long-lived control stream from an enrolled mediahost.
//! Identity comes exclusively from the client certificate (§3) — an
//! unauthenticated connection can reach enrollment, never a link.

use std::sync::Arc;

use kahawai_proto::v1::mediahost_link_server::{MediahostLink, MediahostLinkServer};
use kahawai_proto::v1::{
    host_to_hub, hub_to_host, ByteChunk, HelloAck, HostToHub, HubToHost, ReadRequest,
};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use kahawai_transport::mtls::peer_identity;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::registry::Registry;
use crate::sessions::Sessions;

pub struct MediahostLinkService {
    registry: Arc<Registry>,
    sessions: Arc<Sessions>,
}

impl MediahostLinkService {
    pub fn new(registry: Arc<Registry>, sessions: Arc<Sessions>) -> Self {
        Self { registry, sessions }
    }

    pub fn into_server(self) -> MediahostLinkServer<Self> {
        MediahostLinkServer::new(self)
    }
}

#[tonic::async_trait]
impl MediahostLink for MediahostLinkService {
    type LinkStream = ReceiverStream<Result<HubToHost, Status>>;

    async fn link(
        &self,
        request: Request<Streaming<HostToHub>>,
    ) -> Result<Response<Self::LinkStream>, Status> {
        let peer = peer_identity(&request)
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        if peer.module_type != "mediahost" {
            return Err(Status::permission_denied("not a mediahost certificate"));
        }

        let mut inbound = request.into_inner();
        // First message must be Hello (AR-7).
        let hello = match inbound.message().await? {
            Some(HostToHub { msg: Some(host_to_hub::Msg::Hello(h)) }) => h,
            _ => return Err(Status::failed_precondition("first message must be Hello")),
        };
        if hello.protocol_major != PROTOCOL_MAJOR {
            return Err(Status::failed_precondition(format!(
                "incompatible protocol {}.{} (hub speaks {}.{})",
                hello.protocol_major, hello.protocol_minor, PROTOCOL_MAJOR, PROTOCOL_MINOR
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let registry = self.registry.clone();
        let module_id = peer.module_id.clone();
        registry.connected(&module_id, &peer.module_type, &hello.name, &peer.fingerprint);
        registry.register_link(&module_id, tx.clone());

        tokio::spawn(async move {
            let ack = HubToHost {
                msg: Some(hub_to_host::Msg::HelloAck(HelloAck {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                })),
            };
            if tx.send(Ok(ack)).await.is_err() {
                registry.unregister_link(&module_id);
                registry.disconnected(&module_id);
                return;
            }
            loop {
                match inbound.message().await {
                    Ok(Some(HostToHub { msg: Some(msg) })) => {
                        if let Err(e) = handle_host_msg(&registry, &module_id, msg).await {
                            tracing::error!(%module_id, error = format!("{e:#}"), "handling link message");
                        }
                    }
                    Ok(Some(HostToHub { msg: None })) => {} // newer kind: ignore (OPS-7)
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(%module_id, error = %e, "link stream error");
                        break;
                    }
                }
            }
            registry.unregister_link(&module_id);
            registry.disconnected(&module_id);
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type ByteChannelStream = ReceiverStream<Result<ReadRequest, Status>>;

    async fn byte_channel(
        &self,
        request: Request<Streaming<ByteChunk>>,
    ) -> Result<Response<Self::ByteChannelStream>, Status> {
        let peer = peer_identity(&request)
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        if peer.module_type != "mediahost" {
            return Err(Status::permission_denied("not a mediahost certificate"));
        }
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty byte channel"))?;
        let (req_stream, chunk_tx) = self
            .sessions
            .leases
            .fulfill(&first.lease_token)
            .ok_or_else(|| Status::not_found("unknown or expired lease token"))?;

        tokio::spawn(async move {
            while let Ok(Some(chunk)) = inbound.message().await {
                if chunk_tx.send(chunk).await.is_err() {
                    break; // lease dropped
                }
            }
        });
        Ok(Response::new(req_stream))
    }
}

async fn handle_host_msg(
    registry: &Registry,
    module_id: &str,
    msg: host_to_hub::Msg,
) -> anyhow::Result<()> {
    use crate::registry::FileUpsertRecord;
    match msg {
        host_to_hub::Msg::Heartbeat(_) => registry.seen(module_id),
        host_to_hub::Msg::AnnounceCollection(a) => {
            registry
                .announce_collection(module_id, &a.id, &a.media_type, &a.roots)
                .await?
        }
        host_to_hub::Msg::FileUpsert(u) => {
            let files = u
                .files
                .into_iter()
                .map(|f| FileUpsertRecord {
                    path_rel: f.path_rel,
                    size: f.size,
                    mtime_unix: f.mtime_unix,
                    head_xxh3: f.head_xxh3,
                    tail_xxh3: f.tail_xxh3,
                    oshash: f.oshash,
                    streams_json: f.streams_json,
                })
                .collect();
            let n = registry.upsert_files(module_id, &u.collection_id, files).await?;
            tracing::debug!(%module_id, collection = %u.collection_id, files = n, "file upsert");
        }
        host_to_hub::Msg::FileError(e) => {
            // MH-8: reported, not silently skipped.
            tracing::warn!(%module_id, collection = %e.collection_id, path = %e.path_rel,
                error = %e.error, "mediahost reported unreadable file");
        }
        host_to_hub::Msg::ScanProgress(p) if p.complete => {
            tracing::info!(%module_id, collection = %p.collection_id,
                scanned = p.scanned, failed = p.failed, "scan complete");
        }
        host_to_hub::Msg::ScanProgress(_) | host_to_hub::Msg::Hello(_) => {}
    }
    Ok(())
}
