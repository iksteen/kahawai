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

        let mut seen: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
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
            // Heartbeats arrive every 10 s; three missed = dead link.
            loop {
                let msg = tokio::time::timeout(
                    std::time::Duration::from_secs(35),
                    inbound.message(),
                )
                .await;
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::warn!(%module_id, "no heartbeat in 35s; declaring link dead");
                        break;
                    }
                };
                match msg {
                    Ok(Some(HostToHub { msg: Some(msg) })) => {
                        if let Err(e) = handle_host_msg(&registry, &module_id, msg, &mut seen).await
                        {
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

/// `seen` accumulates upserted paths per collection between its announce
/// and its scan-complete, at which point files missing from the scan are
/// reconciled away (deletions on disk propagate on every rescan).
async fn handle_host_msg(
    registry: &Registry,
    module_id: &str,
    msg: host_to_hub::Msg,
    seen: &mut std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> anyhow::Result<()> {
    use crate::registry::FileUpsertRecord;
    match msg {
        host_to_hub::Msg::Heartbeat(_) => registry.seen(module_id),
        host_to_hub::Msg::AnnounceCollection(a) => {
            seen.insert(a.id.clone(), Default::default());
            registry
                .announce_collection(module_id, &a.id, &a.media_type, &a.roots)
                .await?
        }
        host_to_hub::Msg::FileUpsert(u) => {
            if let Some(paths) = seen.get_mut(&u.collection_id) {
                paths.extend(u.files.iter().map(|f| f.path_rel.clone()));
            }
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
            // MH-8: reported, not silently skipped. The file stays known
            // (it exists on disk), so keep it out of reconciliation.
            if let Some(paths) = seen.get_mut(&e.collection_id) {
                paths.insert(e.path_rel.clone());
            }
            tracing::warn!(%module_id, collection = %e.collection_id, path = %e.path_rel,
                error = %e.error, "mediahost reported unreadable file");
        }
        host_to_hub::Msg::ManifestRequest(r) => {
            // Incremental rescan (MH-5): what we already know, so the
            // host can skip re-inspecting unchanged files.
            let entries = registry.file_stats(module_id, &r.collection_id).await?;
            const CHUNK: usize = 8000;
            let total = entries.len();
            let mut sent = 0;
            let mut chunks = entries.chunks(CHUNK).peekable();
            loop {
                let chunk = chunks.next().unwrap_or(&[]);
                let done = chunks.peek().is_none();
                let msg = kahawai_proto::v1::HubToHost {
                    msg: Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(
                        kahawai_proto::v1::Manifest {
                            collection_id: r.collection_id.clone(),
                            entries: chunk.to_vec(),
                            done,
                        },
                    )),
                };
                registry.send_to_host(module_id, msg).await?;
                sent += chunk.len();
                if done {
                    break;
                }
            }
            tracing::debug!(%module_id, collection = %r.collection_id, files = total, sent, "manifest sent");
        }
        host_to_hub::Msg::FilesSeen(s) => {
            if let Some(paths) = seen.get_mut(&s.collection_id) {
                paths.extend(s.path_rel);
            }
        }
        host_to_hub::Msg::ScanProgress(p) if p.complete => {
            tracing::info!(%module_id, collection = %p.collection_id,
                scanned = p.scanned, failed = p.failed, skipped = p.skipped, "scan complete");
            if let Some(paths) = seen.remove(&p.collection_id) {
                registry.reconcile_files(module_id, &p.collection_id, &paths).await?;
            }
        }
        host_to_hub::Msg::ScanProgress(_) | host_to_hub::Msg::Hello(_) => {}
    }
    Ok(())
}
