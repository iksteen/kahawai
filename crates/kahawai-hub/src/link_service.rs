//! MediahostLink: the long-lived control stream from an enrolled mediahost.
//! Identity comes exclusively from the client certificate (§3) — an
//! unauthenticated connection can reach enrollment, never a link.

use std::sync::Arc;

use kahawai_proto::v1::mediahost_link_server::{MediahostLink, MediahostLinkServer};
use kahawai_proto::v1::{host_to_hub, hub_to_host, HelloAck, HostToHub, HubToHost};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use kahawai_transport::mtls::peer_identity;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::registry::Registry;

pub struct MediahostLinkService {
    registry: Arc<Registry>,
}

impl MediahostLinkService {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
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

        tokio::spawn(async move {
            let ack = HubToHost {
                msg: Some(hub_to_host::Msg::HelloAck(HelloAck {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                })),
            };
            if tx.send(Ok(ack)).await.is_err() {
                registry.disconnected(&module_id);
                return;
            }
            loop {
                match inbound.message().await {
                    Ok(Some(HostToHub { msg: Some(msg) })) => {
                        handle_host_msg(&registry, &module_id, msg)
                    }
                    Ok(Some(HostToHub { msg: None })) => {} // newer kind: ignore (OPS-7)
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(%module_id, error = %e, "link stream error");
                        break;
                    }
                }
            }
            registry.disconnected(&module_id);
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

fn handle_host_msg(registry: &Registry, module_id: &str, msg: host_to_hub::Msg) {
    use crate::registry::FileEntry;
    match msg {
        host_to_hub::Msg::Heartbeat(_) => registry.seen(module_id),
        host_to_hub::Msg::AnnounceCollection(a) => {
            registry.announce_collection(module_id, &a.id, &a.media_type, a.roots)
        }
        host_to_hub::Msg::FileUpsert(u) => {
            let n = registry.upsert_files(
                module_id,
                &u.collection_id,
                u.files.into_iter().map(|f| {
                    (
                        f.path_rel,
                        FileEntry {
                            size: f.size,
                            mtime_unix: f.mtime_unix,
                            head_xxh3: f.head_xxh3,
                            tail_xxh3: f.tail_xxh3,
                            oshash: f.oshash,
                            streams_json: f.streams_json,
                        },
                    )
                }),
            );
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
}
