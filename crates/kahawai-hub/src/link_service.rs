//! MediahostLink: the long-lived control stream from an enrolled mediahost.
//! Identity comes exclusively from the client certificate (§3) — an
//! unauthenticated connection can reach enrollment, never a link.

use std::sync::Arc;

use kahawai_proto::v1::mediahost_link_server::{MediahostLink, MediahostLinkServer};
use kahawai_proto::v1::{
    ByteChunk, HelloAck, HostToHub, HubToHost, ReadRequest, host_to_hub, hub_to_host,
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
    subtitles: Arc<crate::subtitles::Subtitles>,
    enricher: Arc<crate::enrich::Enricher>,
}

impl MediahostLinkService {
    pub fn new(
        registry: Arc<Registry>,
        sessions: Arc<Sessions>,
        subtitles: Arc<crate::subtitles::Subtitles>,
        enricher: Arc<crate::enrich::Enricher>,
    ) -> Self {
        Self {
            registry,
            sessions,
            subtitles,
            enricher,
        }
    }

    pub fn into_server(self) -> MediahostLinkServer<Self> {
        // A multi-track FileSubtitles (30+ full subtitle texts) can pass
        // tonic's 4 MB default, which kills the whole control link.
        MediahostLinkServer::new(self).max_decoding_message_size(64 * 1024 * 1024)
    }
}

/// AR-5: attach an in-process mediahost. Same message handling as a
/// network link, but the transport is a channel pair — no TLS, no
/// enrollment, no liveness timeout (the peer shares our fate). Returns
/// (host→hub sender for the engine, hub→host receiver for it).
#[allow(clippy::type_complexity)]
pub fn local_link(
    registry: Arc<Registry>,
    subtitles: Arc<crate::subtitles::Subtitles>,
    enricher: Arc<crate::enrich::Enricher>,
    module_id: &str,
    name: &str,
) -> (
    tokio::sync::mpsc::Sender<HostToHub>,
    tokio::sync::mpsc::Receiver<Result<HubToHost, Status>>,
) {
    let (host_tx, mut host_rx) = tokio::sync::mpsc::channel::<HostToHub>(64);
    let (hub_tx, hub_rx) = tokio::sync::mpsc::channel::<Result<HubToHost, Status>>(16);
    registry.connected(module_id, "mediahost", name, "in-process");
    registry.register_link(module_id, hub_tx);
    let module_id = module_id.to_string();
    tokio::spawn(async move {
        let mut seen: std::collections::HashMap<String, std::collections::HashSet<String>> =
            Default::default();
        while let Some(HostToHub { msg }) = host_rx.recv().await {
            let Some(msg) = msg else { continue };
            if matches!(msg, host_to_hub::Msg::Heartbeat(_)) {
                registry.seen(&module_id);
                continue;
            }
            if let Err(e) =
                handle_host_msg(&registry, &subtitles, &enricher, &module_id, msg, &mut seen).await
            {
                tracing::error!(%module_id, error = format!("{e:#}"), "handling local link message");
            }
        }
        registry.unregister_link(&module_id);
        registry.disconnected(&module_id);
    });
    (host_tx, hub_rx)
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
            Some(HostToHub {
                msg: Some(host_to_hub::Msg::Hello(h)),
            }) => h,
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
        let outer_subtitles = self.subtitles.clone();
        let outer_enricher = self.enricher.clone();
        let module_id = peer.module_id.clone();
        registry.connected(
            &module_id,
            &peer.module_type,
            &hello.name,
            &peer.fingerprint,
        );
        if let Err(e) = registry.settle_renewal(&module_id, &peer.fingerprint).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "renewal settlement failed");
        }
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
            // Heavy messages (upserts with resolution, reconciliation)
            // process on an ordered queue so the read loop keeps
            // reading: while it was blocked on DB work, heartbeats sat
            // unread in the stream and the 35 s liveness timeout fired
            // spuriously mid-scan — killing the scan it was serving.
            let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<host_to_hub::Msg>(64);
            let worker = {
                let registry = registry.clone();
                let module_id = module_id.clone();
                let subtitles = outer_subtitles.clone();
                let enricher = outer_enricher.clone();
                tokio::spawn(async move {
                    let mut seen: std::collections::HashMap<
                        String,
                        std::collections::HashSet<String>,
                    > = std::collections::HashMap::new();
                    while let Some(msg) = work_rx.recv().await {
                        if let Err(e) = handle_host_msg(
                            &registry, &subtitles, &enricher, &module_id, msg, &mut seen,
                        )
                        .await
                        {
                            tracing::error!(%module_id, error = format!("{e:#}"), "handling link message");
                        }
                    }
                })
            };
            // Heartbeats arrive every 10 s; three missed = dead link.
            loop {
                let msg =
                    tokio::time::timeout(std::time::Duration::from_secs(35), inbound.message())
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
                        // Liveness inline; everything else in order on
                        // the worker (a full queue applies backpressure
                        // but 64 batches of headroom outlasts any DB
                        // stall shorter than the timeout).
                        if matches!(msg, host_to_hub::Msg::Heartbeat(_)) {
                            tracing::debug!(%module_id, "heartbeat read");
                            registry.seen(&module_id);
                        } else {
                            let kind = kind_name(&msg);
                            tracing::debug!(%module_id, kind, "link msg read");
                            let queued = tokio::time::Instant::now();
                            if work_tx.send(msg).await.is_err() {
                                break;
                            }
                            if queued.elapsed() > std::time::Duration::from_secs(2) {
                                tracing::warn!(%module_id, kind, waited = ?queued.elapsed(),
                                    "read loop stalled on a full work queue");
                            }
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
            drop(work_tx);
            let _ = worker.await; // drain in order before cleanup
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

fn kind_name(m: &host_to_hub::Msg) -> &'static str {
    match m {
        host_to_hub::Msg::Hello(_) => "hello",
        host_to_hub::Msg::Heartbeat(_) => "heartbeat",
        host_to_hub::Msg::AnnounceCollection(_) => "announce",
        host_to_hub::Msg::FileUpsert(_) => "upsert",
        host_to_hub::Msg::FileError(_) => "file_error",
        host_to_hub::Msg::ScanProgress(_) => "scan_progress",
        host_to_hub::Msg::ManifestRequest(_) => "manifest_request",
        host_to_hub::Msg::FilesSeen(_) => "files_seen",
        host_to_hub::Msg::FileHashes(_) => "file_hashes",
        host_to_hub::Msg::FileSubtitles(_) => "file_subtitles",
        host_to_hub::Msg::FileAttachments(_) => "file_attachments",
        host_to_hub::Msg::ImageSubtitles(_) => "image_subtitles",
    }
}

/// `seen` accumulates upserted paths per collection between its announce
/// and its scan-complete, at which point files missing from the scan are
/// reconciled away (deletions on disk propagate on every rescan).
async fn handle_host_msg(
    registry: &Arc<Registry>,
    subtitles: &crate::subtitles::Subtitles,
    enricher: &Arc<crate::enrich::Enricher>,
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
            let n = registry
                .upsert_files(module_id, &u.collection_id, files)
                .await?;
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
            // Deep refresh: answer EMPTY, so every file re-probes
            // (first-scan semantics). Must beat the in-sync gate — a
            // deep refresh of an in-sync collection is the whole point.
            if registry.take_deep_rescan(module_id, &r.collection_id) {
                let msg = kahawai_proto::v1::HubToHost {
                    msg: Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(
                        kahawai_proto::v1::Manifest {
                            sidecars_compared: false,
                            collection_id: r.collection_id.clone(),
                            entries: vec![],
                            done: true,
                            in_sync: false,
                        },
                    )),
                };
                registry.send_to_host(module_id, msg).await?;
                tracing::info!(%module_id, collection = %r.collection_id,
                    "deep refresh: empty manifest sent, full re-probe");
                return Ok(());
            }
            // Reconnect handshake: matching scan generations mean the
            // hub already reflects the host's last completed scan — no
            // manifest, no walk, no reconciliation churn on restart.
            if r.sync_version != 0
                && r.sync_version
                    == registry
                        .collection_sync_version(module_id, &r.collection_id)
                        .await?
            {
                let msg = kahawai_proto::v1::HubToHost {
                    msg: Some(kahawai_proto::v1::hub_to_host::Msg::Manifest(
                        kahawai_proto::v1::Manifest {
                            sidecars_compared: true,
                            collection_id: r.collection_id.clone(),
                            entries: vec![],
                            done: true,
                            in_sync: true,
                        },
                    )),
                };
                registry.send_to_host(module_id, msg).await?;
                tracing::info!(%module_id, collection = %r.collection_id,
                    version = r.sync_version, "collection in sync; scan skipped");
                push_ed2k_worklist(registry, module_id, &r.collection_id).await;
                push_subs_worklist(registry, module_id, &r.collection_id).await;
                push_attachments_worklist(registry, module_id, &r.collection_id).await;
                return Ok(());
            }
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
                            sidecars_compared: true,
                            collection_id: r.collection_id.clone(),
                            entries: chunk.to_vec(),
                            done,
                            in_sync: false,
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
        // HUB-32b: display sets the host walked for us; cached for the
        // burn-in session that asked (and for any later one).
        host_to_hub::Msg::ImageSubtitles(m) => {
            if !m.error.is_empty() {
                tracing::warn!(%module_id, collection = %m.collection_id, path = %m.path_rel,
                    track = m.sub_index, error = %m.error, "image display-set extraction failed");
            } else if let Err(e) = subtitles.store_image_sets(module_id, &m).await {
                tracing::warn!(%module_id, error = format!("{e:#}"), "storing image display sets");
            } else {
                tracing::info!(%module_id, collection = %m.collection_id, path = %m.path_rel,
                    track = m.sub_index, blocks = m.blocks.len(), "image display sets cached");
            }
        }
        host_to_hub::Msg::FilesSeen(s) => {
            if let Some(paths) = seen.get_mut(&s.collection_id) {
                paths.extend(s.path_rel);
            }
        }
        host_to_hub::Msg::ScanProgress(p) if !p.complete => {
            registry.update_scan_progress(
                module_id,
                &p.collection_id,
                p.scanned,
                p.failed,
                p.skipped,
                false,
            );
        }
        host_to_hub::Msg::ScanProgress(p) if p.complete => {
            tracing::info!(%module_id, collection = %p.collection_id,
                scanned = p.scanned, failed = p.failed, skipped = p.skipped, "scan complete");
            if let Some(paths) = seen.remove(&p.collection_id) {
                registry
                    .reconcile_files(module_id, &p.collection_id, &paths)
                    .await?;
            }
            if p.sync_version != 0 {
                registry
                    .set_collection_sync_version(module_id, &p.collection_id, p.sync_version)
                    .await?;
            }
            registry.update_scan_progress(
                module_id,
                &p.collection_id,
                p.scanned,
                p.failed,
                p.skipped,
                true,
            );
            push_ed2k_worklist(registry, module_id, &p.collection_id).await;
            push_subs_worklist(registry, module_id, &p.collection_id).await;
            push_attachments_worklist(registry, module_id, &p.collection_id).await;
        }
        host_to_hub::Msg::FileAttachments(fa) => {
            let stored = registry
                .record_file_attachments(
                    module_id,
                    &fa.collection_id,
                    &fa.path_rel,
                    fa.size,
                    &fa.attachments_json,
                )
                .await?;
            if fa.attachments_json != "[]" {
                tracing::info!(%module_id, collection = %fa.collection_id,
                    path = %fa.path_rel, stored, "attachments declared by mediahost");
            }
        }
        host_to_hub::Msg::FileSubtitles(fs) => {
            if !fs.error.is_empty() {
                tracing::warn!(%module_id, collection = %fs.collection_id, path = %fs.path_rel,
                    error = %fs.error, "mediahost subtitle extraction failed");
                registry
                    .set_subs_extracted(module_id, &fs.collection_id, &fs.path_rel, None)
                    .await?;
                return Ok(());
            }
            for t in &fs.tracks {
                let cues: Vec<kahawai_media::subtitles::Cue> =
                    serde_json::from_str(&t.cues_json).unwrap_or_default();
                let ex = kahawai_media::subtitles::Extracted {
                    cues,
                    ass: (!t.ass.is_empty()).then(|| t.ass.clone()),
                };
                subtitles.store_extracted(
                    module_id,
                    &fs.collection_id,
                    &fs.path_rel,
                    &t.key,
                    &ex,
                )?;
            }
            let stored = registry
                .set_subs_extracted(module_id, &fs.collection_id, &fs.path_rel, Some(fs.size))
                .await?;
            tracing::info!(%module_id, collection = %fs.collection_id, path = %fs.path_rel,
                tracks = fs.tracks.len(), stored, "subtitles cached from mediahost");
        }
        host_to_hub::Msg::FileHashes(fh) => {
            for h in fh.hashes {
                if h.crc_checked && !h.crc_ok {
                    tracing::warn!(%module_id, collection = %fh.collection_id,
                        path = %h.path_rel, "mediahost reports filename CRC32 mismatch");
                }
                let stored = registry
                    .record_ed2k(
                        module_id,
                        &fh.collection_id,
                        &h.path_rel,
                        &h.ed2k_hex,
                        h.size,
                    )
                    .await?;
                if !stored {
                    tracing::debug!(%module_id, path = %h.path_rel,
                        "ed2k result stale (file changed since listing); dropped");
                }
            }
            // A late hash is canonical identity material (HUB-30):
            // debounced re-enrichment re-verifies name-based matches.
            enricher.nudge(registry.clone());
        }
        host_to_hub::Msg::ScanProgress(_) | host_to_hub::Msg::Hello(_) => {}
    }
    Ok(())
}

/// MH-9: send the collection's ED2K worklist (anime only; empty = no-op).
/// Failures are logged, never fatal — hashing is strictly best-effort.
async fn push_ed2k_worklist(
    registry: &crate::registry::Registry,
    module_id: &str,
    collection_id: &str,
) {
    let paths = match registry.ed2k_worklist(module_id, collection_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%module_id, collection = %collection_id,
                error = format!("{e:#}"), "ed2k worklist failed");
            return;
        }
    };
    if paths.is_empty() {
        return;
    }
    tracing::info!(%module_id, collection = %collection_id, files = paths.len(),
        "sending ed2k worklist");
    for chunk in paths.chunks(5000) {
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::Hashlist(
                kahawai_proto::v1::Hashlist {
                    collection_id: collection_id.to_string(),
                    paths: chunk.to_vec(),
                },
            )),
        };
        if let Err(e) = registry.send_to_host(module_id, msg).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "ed2k worklist send failed");
            return;
        }
    }
}

/// Efficiency ladder step 2: send the collection's subtitle pre-warm
/// worklist (video collections; empty = no-op). Best-effort.
async fn push_attachments_worklist(
    registry: &crate::registry::Registry,
    module_id: &str,
    collection_id: &str,
) {
    let paths = match registry
        .attachments_worklist(module_id, collection_id)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%module_id, collection = %collection_id,
                error = format!("{e:#}"), "attachments worklist failed");
            return;
        }
    };
    if paths.is_empty() {
        return;
    }
    tracing::info!(%module_id, collection = %collection_id, files = paths.len(),
        "sending attachments worklist");
    for chunk in paths.chunks(5000) {
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::AttachmentsWorklist(
                kahawai_proto::v1::AttachmentsWorklist {
                    collection_id: collection_id.to_string(),
                    paths: chunk.to_vec(),
                },
            )),
        };
        if let Err(e) = registry.send_to_host(module_id, msg).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "attachments worklist send failed");
            return;
        }
    }
}

async fn push_subs_worklist(
    registry: &crate::registry::Registry,
    module_id: &str,
    collection_id: &str,
) {
    let paths = match registry.subs_worklist(module_id, collection_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%module_id, collection = %collection_id,
                error = format!("{e:#}"), "subs worklist failed");
            return;
        }
    };
    if paths.is_empty() {
        return;
    }
    tracing::info!(%module_id, collection = %collection_id, files = paths.len(),
        "sending subtitle worklist");
    for chunk in paths.chunks(5000) {
        let msg = kahawai_proto::v1::HubToHost {
            msg: Some(kahawai_proto::v1::hub_to_host::Msg::SubsWorklist(
                kahawai_proto::v1::SubsWorklist {
                    collection_id: collection_id.to_string(),
                    paths: chunk.to_vec(),
                },
            )),
        };
        if let Err(e) = registry.send_to_host(module_id, msg).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "subs worklist send failed");
            return;
        }
    }
}
