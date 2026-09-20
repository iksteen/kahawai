//! MediahostLink: the long-lived control stream from an enrolled mediahost.
//! Identity comes exclusively from the client certificate (§3) — an
//! unauthenticated connection can reach enrollment, never a link.

use std::sync::Arc;

use anyhow::Context as _;

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

const LINK_LIVENESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
const WORK_QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// AR-6, the mediahost half. A transcoder that drops has its sessions moved
/// to another box (`reschedule_for_transcoder`); a mediahost has nothing to
/// be moved to, because the bytes are on it. Its leases are already dead, so
/// the sessions reading them cannot recover — left alive they stall, and the
/// client waits on a stream that will never produce another byte.
///
/// Ending them turns that silence into 404 on the next request, which is the
/// one signal the recovery contract defines. What the client does with it is
/// its own business: start again, and find out from THAT whether the host is
/// back.
async fn end_sessions_on(sessions: &Sessions, module_id: &str) {
    let ended = sessions.end_for_module(module_id).await;
    if ended > 0 {
        tracing::warn!(%module_id, ended, "mediahost lost; its sessions ended");
    }
}

/// Forget a lost mediahost: both maps, then its sessions.
///
/// One function because the three teardown paths were writing the same three
/// calls in two different orders, and the order is the whole point.
///
/// `links` (the send side) and `connected` (what `is_connected` answers, and so
/// whether this host's files are offered at all) must never be left
/// disagreeing. Split across a drain, they were: a host that reconnected in
/// between kept `connected: true` while the late `unregister_link` deleted its
/// fresh sender, so its files were still offered, `open_lease` failed with a
/// plain error, and the client was told 409 — give up on this item — about a
/// host that was healthy. It also never got another manifest, so it never
/// scanned again.
///
/// Called before any drain, so nothing after it touches registry state and a
/// reconnect cannot be clobbered.
pub(crate) async fn forget_link(
    registry: &Registry,
    sessions: &Sessions,
    module_id: &str,
    _generation: u64,
    tx: &tokio::sync::mpsc::Sender<Result<HubToHost, Status>>,
) {
    // Remove routing first, then retire this generation's work. A dispatcher
    // that raced before removal is cancelled below; one that races after sees
    // the generation is no longer current and cancels its own waiter.
    let current = registry.unregister_link_if_current(module_id, tx);

    if !current {
        tracing::info!(%module_id, "link already replaced; leaving the new one alone");
        return;
    }
    end_sessions_on(sessions, module_id).await;
}

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

fn register_host_link(
    registry: &Registry,
    module_id: &str,
    tx: tokio::sync::mpsc::Sender<Result<HubToHost, Status>>,
    protocol_minor: u32,
    segment_detector_generation: i64,
) -> u64 {
    let (generation, _) =
        registry.register_link(module_id, tx, protocol_minor, segment_detector_generation);
    generation
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
    let registered_tx = hub_tx.clone();
    let module_id = module_id.to_string();
    let name = name.to_string();
    tokio::spawn(async move {
        let gate = registry.catalog_apply_lock(&module_id);
        let guard = gate.lock().await;
        registry.connected(
            &module_id,
            "mediahost",
            &name,
            "in-process",
            kahawai_core::build_stamp(),
        );
        let generation = register_host_link(
            &registry,
            &module_id,
            hub_tx,
            PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );
        drop(guard);
        let mut partial = Default::default();
        while let Some(HostToHub { msg }) = host_rx.recv().await {
            let Some(msg) = msg else { continue };
            if matches!(msg, host_to_hub::Msg::Heartbeat(_)) {
                registry.seen(&module_id);
                continue;
            }
            if let Err(error) = validate_exact_host_msg(&msg) {
                let _ = registered_tx.try_send(Err(Status::failed_precondition(error.to_string())));
                break;
            }
            let _guard = gate.lock().await;
            if !registry.host_link_is_current(&module_id, generation) {
                break;
            }
            if let Err(e) = handle_host_msg(
                &registry,
                &subtitles,
                &enricher,
                &mut partial,
                &module_id,
                generation,
                msg,
            )
            .await
            {
                tracing::error!(%module_id, error = format!("{e:#}"), "handling local link message");
                let _ = registered_tx.try_send(Err(Status::failed_precondition(e.to_string())));
                break;
            }
        }
        // Not `forget_link`: this path has no `Sessions` handle. Sessions are
        // deliberately left alone because local playback does not use the link
        // byte plane. The all-in-one local-link supervisor recreates this
        // adapter after an error, replaying from the durable catalogue cursor.
        registry.unregister_link_if_current(&module_id, &registered_tx);
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
                "incompatible protocol {}.{} (hub speaks {}.{}); upgrade this mediahost to protocol {}",
                hello.protocol_major,
                hello.protocol_minor,
                PROTOCOL_MAJOR,
                PROTOCOL_MINOR,
                PROTOCOL_MAJOR
            )));
        }
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let registry = self.registry.clone();

        let sessions = self.sessions.clone();
        let subtitles = self.subtitles.clone();
        let enricher = self.enricher.clone();
        let module_id = peer.module_id.clone();
        // Sender first, and only then "present". The reverse order left this
        // host offered as a playback source across the renewal settlement's DB
        // work below — SELECT, sometimes an UPDATE with an audit row — with no
        // way to reach it, which is answered 409 rather than 503.
        let gate = registry.catalog_apply_lock(&module_id);
        let guard = gate.lock().await;
        let admitted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM satellites WHERE module_id=? AND (cert_fingerprint=? OR pending_fingerprint=?))")
            .bind(&module_id).bind(&peer.fingerprint).bind(&peer.fingerprint).fetch_one(registry.db()).await.map_err(|_| Status::internal("checking enrollment"))?;
        if !admitted {
            return Err(Status::unauthenticated("mediahost enrollment was revoked"));
        }
        let generation = register_host_link(
            &registry,
            &module_id,
            tx.clone(),
            hello.protocol_minor,
            hello.segment_detector_generation,
        );
        registry.connected(
            &module_id,
            &peer.module_type,
            &hello.name,
            &peer.fingerprint,
            &hello.build,
        );
        if let Err(e) = registry.settle_renewal(&module_id, &peer.fingerprint).await {
            tracing::warn!(%module_id, error = format!("{e:#}"), "renewal settlement failed");
        }

        drop(guard);
        tokio::spawn(async move {
            let ack = HubToHost {
                msg: Some(hub_to_host::Msg::HelloAck(HelloAck {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                })),
            };
            if tx.send(Ok(ack)).await.is_err() {
                forget_link(&registry, &sessions, &module_id, generation, &tx).await;
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
                let error_tx = tx.clone();
                tokio::spawn(async move {
                    let mut partial = Default::default();
                    while let Some(msg) = work_rx.recv().await {
                        let catalog_message = matches!(
                            msg,
                            host_to_hub::Msg::CatalogOffer(_)
                                | host_to_hub::Msg::CatalogDelta(_)
                                | host_to_hub::Msg::DiscoveryStatus(_)
                        );
                        let _catalog_guard = if catalog_message {
                            Some(registry.catalog_apply_lock(&module_id).lock_owned().await)
                        } else {
                            None
                        };
                        if !registry.host_link_is_current(&module_id, generation) {
                            continue;
                        }
                        if let Err(e) = handle_host_msg(
                            &registry,
                            &subtitles,
                            &enricher,
                            &mut partial,
                            &module_id,
                            generation,
                            msg,
                        )
                        .await
                        {
                            tracing::error!(%module_id, error = format!("{e:#}"), "handling link message");
                            let _ = error_tx
                                .send(Err(Status::failed_precondition(e.to_string())))
                                .await;
                            break;
                        }
                    }
                })
            };
            // Heartbeats arrive every 10 s; three missed = dead link.
            loop {
                let msg = tokio::time::timeout(LINK_LIVENESS_TIMEOUT, inbound.message()).await;
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::warn!(%module_id, "no heartbeat in 35s; declaring link dead");
                        break;
                    }
                };
                match msg {
                    Ok(Some(HostToHub { msg: Some(msg) })) => {
                        if let Err(error) = validate_exact_host_msg(&msg) {
                            let _ = tx
                                .send(Err(Status::failed_precondition(error.to_string())))
                                .await;
                            break;
                        }
                        // Liveness and segment correlation are synchronous:
                        // neither may wait behind scan/database work.
                        if matches!(msg, host_to_hub::Msg::Heartbeat(_)) {
                            tracing::debug!(%module_id, "heartbeat read");
                            registry.seen(&module_id);
                        } else {
                            let kind = kind_name(&msg);
                            tracing::debug!(%module_id, kind, "link msg read");
                            let queued = tokio::time::Instant::now();
                            match tokio::time::timeout(WORK_QUEUE_TIMEOUT, work_tx.send(msg)).await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    // A protocol-4 sender can otherwise pin
                                    // its catalogue read transaction and the
                                    // mediahost WAL forever while this queue
                                    // is full. Closing this generation makes
                                    // it replay from the durable cursor.
                                    tracing::warn!(%module_id, kind,
                                        waited = ?queued.elapsed(),
                                        "projection queue stalled; cycling link");
                                    break;
                                }
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
            // Protocol 4 can replay everything after the hub's durable cursor.
            // Do not drain a dead generation: a reconnect may already be
            // projecting a newer snapshot while this queue still contains an
            // old reconciliation. Cancellation drops/rolls back the current
            // SQL future; already committed records replay idempotently.
            worker.abort();
            let _ = worker.await;
            forget_link(&registry, &sessions, &module_id, generation, &tx).await;
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

/// Reject legacy mutation messages before they reach the catalogue writer.
fn validate_exact_host_msg(m: &host_to_hub::Msg) -> anyhow::Result<()> {
    let valid = |source: &Option<kahawai_proto::v1::SourcePath>, kind: &str| {
        let source = source
            .as_ref()
            .with_context(|| format!("{kind} missing exact source"))?;
        anyhow::ensure!(
            !source.root_token.is_empty(),
            "{kind} has an empty root token"
        );
        Ok(())
    };
    match m {
        host_to_hub::Msg::AnnounceCollection(_)
        | host_to_hub::Msg::FileUpsert(_)
        | host_to_hub::Msg::FileError(_)
        | host_to_hub::Msg::ScanProgress(_)
        | host_to_hub::Msg::ManifestRequest(_)
        | host_to_hub::Msg::FilesSeen(_)
        | host_to_hub::Msg::FileHashes(_)
        | host_to_hub::Msg::FileAttachments(_)
        | host_to_hub::Msg::FileKeyframeInterval(_)
        | host_to_hub::Msg::RootResolutions(_)
        | host_to_hub::Msg::RootAdoptionAck(_)
        | host_to_hub::Msg::FileVideoGeometry(_)
        | host_to_hub::Msg::SegmentDetectionAccepted(_)
        | host_to_hub::Msg::SegmentDetectionResult(_)
        | host_to_hub::Msg::FileLoudness(_) => {
            anyhow::bail!("protocol 4 rejects legacy catalogue mutation messages")
        }
        host_to_hub::Msg::FileSubtitles(message) => valid(&message.source, "FileSubtitles")?,
        host_to_hub::Msg::ImageSubtitles(message) => valid(&message.source, "ImageSubtitles")?,
        host_to_hub::Msg::CatalogOffer(offer) => {
            for collection in &offer.collections {
                anyhow::ensure!(
                    !collection.id.is_empty(),
                    "CatalogOffer has an empty collection id"
                );
                anyhow::ensure!(
                    !collection.epoch.is_empty(),
                    "CatalogOffer has an empty epoch"
                );
                anyhow::ensure!(
                    !collection.roots.is_empty(),
                    "CatalogOffer collection has no roots"
                );
            }
        }
        host_to_hub::Msg::CatalogDelta(delta) => {
            anyhow::ensure!(
                !delta.collection_id.is_empty(),
                "CatalogDelta has no collection"
            );
            anyhow::ensure!(!delta.epoch.is_empty(), "CatalogDelta has no epoch");
        }
        _ => {}
    }
    Ok(())
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
        host_to_hub::Msg::FileSubtitles(_) => "filesubtitles",
        host_to_hub::Msg::FileAttachments(_) => "file_attachments",
        host_to_hub::Msg::FileKeyframeInterval(_) => "file_keyframe_interval",
        host_to_hub::Msg::FileVideoGeometry(_) => "file_video_geometry",
        host_to_hub::Msg::FileLoudness(_) => "file_loudness",
        host_to_hub::Msg::ImageSubtitles(_) => "imagesubtitles",
        host_to_hub::Msg::RootResolutions(_) => "root_resolutions",
        host_to_hub::Msg::RootAdoptionAck(_) => "root_adoption_ack",
        host_to_hub::Msg::SegmentDetectionAccepted(_) => "segment_detection_accepted",
        host_to_hub::Msg::SegmentDetectionResult(_) => "segment_detection_result",
        host_to_hub::Msg::CatalogOffer(_) => "catalog_offer",
        host_to_hub::Msg::CatalogDelta(_) => "catalog_delta",
        host_to_hub::Msg::DiscoveryStatus(_) => "discovery_status",
    }
}

/// The transport owns connection generations; Store owns catalogue state.
/// Blocks of one track's display sets, gathered from the messages that
/// carry them. Per connection: a dropped link drops the partial with it,
/// and the next request starts the transfer again.
struct PartialSets {
    bytes: usize,
    blocks: Vec<kahawai_proto::v1::ImageSubBlock>,
}

/// Ceiling on one track's transfer. Not the wire limit — that is per
/// message and now unreachable — but a guard against a sender that
/// never marks the end.
const MAX_SETS_BYTES: usize = 512 * 1024 * 1024;

/// What a chunk of display sets means for the transfer it belongs to.
enum Chunk {
    /// Held; the sender has not said done yet.
    More,
    /// The last chunk: `m.blocks` now holds the whole track, and the
    /// value is what it weighed.
    Complete(usize),
    /// The sender never said done and went past the cap.
    TooBig(usize),
}

/// Gather one message into its transfer, and say whether that completes
/// it. On completion the message's own `blocks` are replaced by every
/// block of the track, so the caller stores one thing.
///
/// A message with no `done` marker at all is an older mediahost sending
/// the whole track at once — complete by definition, which is why the
/// field has presence rather than defaulting to false.
fn accept_chunk(
    partial: &mut std::collections::HashMap<(String, String, String, u32, String), PartialSets>,
    m: &mut kahawai_proto::v1::ImageSubtitles,
) -> Chunk {
    let source = m.source.as_ref().expect("validated exact image source");
    let key = (
        m.collection_id.clone(),
        source.root_token.clone(),
        source.path_rel.clone(),
        m.sub_index,
        m.source_revision.clone(),
    );
    let last = m.done.unwrap_or(true);
    let held = partial.entry(key.clone()).or_insert_with(|| PartialSets {
        bytes: 0,
        blocks: Vec::new(),
    });
    held.bytes += m.blocks.iter().map(|b| b.payload.len()).sum::<usize>();
    held.blocks.append(&mut m.blocks);
    if held.bytes > MAX_SETS_BYTES {
        let bytes = held.bytes;
        partial.remove(&key);
        return Chunk::TooBig(bytes);
    }
    if !last {
        return Chunk::More;
    }
    let held = partial.remove(&key).expect("just inserted");
    m.blocks = held.blocks;
    Chunk::Complete(held.bytes)
}

async fn handle_host_msg(
    registry: &Arc<Registry>,
    subtitles: &crate::subtitles::Subtitles,
    enricher: &crate::enrich::Enricher,
    partial: &mut std::collections::HashMap<(String, String, String, u32, String), PartialSets>,
    module_id: &str,
    generation: u64,
    msg: host_to_hub::Msg,
) -> anyhow::Result<()> {
    match msg {
        host_to_hub::Msg::Heartbeat(_) => registry.seen(module_id),
        host_to_hub::Msg::Hello(_) => {}
        host_to_hub::Msg::CatalogOffer(offer) => {
            for collection in &offer.collections {
                for root in &collection.roots {
                    let path = std::path::Path::new(&root.normalized_path);
                    anyhow::ensure!(
                        path.is_absolute()
                            && kahawai_core::media::root_token(path) == root.root_token,
                        "collection {} announced invalid root token/path binding",
                        collection.id
                    );
                }
            }
            let name: String = sqlx::query_scalar("SELECT name FROM satellites WHERE module_id=?")
                .bind(module_id)
                .fetch_one(registry.db())
                .await?;
            let cursors = registry
                .catalogue()
                .offer_catalogue(module_id, &name, &offer)
                .await?;
            for cursor in cursors {
                registry
                    .send_to_host_generation(
                        module_id,
                        generation,
                        HubToHost {
                            msg: Some(hub_to_host::Msg::CatalogCursor(cursor)),
                        },
                    )
                    .await?;
            }
        }
        host_to_hub::Msg::CatalogDelta(delta) => {
            let ack = registry
                .catalogue()
                .apply_catalogue(module_id, &delta)
                .await?;
            // The commit created or reset queue rows; the queues look now
            // rather than at their next tick.
            enricher.wake();
            subtitles.wake();
            if let Some(ack) = ack {
                registry
                    .send_to_host_generation(
                        module_id,
                        generation,
                        HubToHost {
                            msg: Some(hub_to_host::Msg::CatalogAck(ack)),
                        },
                    )
                    .await?;
            }
        }
        host_to_hub::Msg::FileSubtitles(message) => {
            let source = message.source.context("missing subtitle source")?;
            // Both gates below drop the mediahost's work silently, and a drop
            // is indistinguishable from never having extracted: the sweep
            // re-offers the same file on its next round, forever. Say so.
            let known = registry
                .catalogue()
                .source_exists(
                    module_id,
                    &message.collection_id,
                    &source,
                    Some(message.size),
                )
                .await?;
            if !message.error.is_empty() || !known || message.source_revision.is_empty() {
                tracing::warn!(
                    module_id,
                    collection = %message.collection_id,
                    path = %source.path_rel,
                    size = message.size,
                    tracks = message.tracks.len(),
                    error = %message.error,
                    known,
                    revisioned = !message.source_revision.is_empty(),
                    "extracted subtitles discarded"
                );
            }
            if !message.error.is_empty() && known {
                // The mediahost's verdict on this file releases the queue
                // row with a retry instead of leaving it leased.
                registry
                    .catalogue()
                    .fail_subtitle_source(
                        module_id,
                        &message.collection_id,
                        &source,
                        "text",
                        crate::queue::now() + crate::subtitles::work::HOST_ERROR_RETRY_SECS,
                        &message.error,
                    )
                    .await?;
                subtitles.wake();
            }
            if message.error.is_empty() && known {
                let mut keys = vec![];
                for track in message.tracks {
                    let extracted = kahawai_media::subtitles::Extracted {
                        cues: serde_json::from_str(&track.cues_json)?,
                        ass: (!track.ass.is_empty()).then_some(track.ass),
                    };
                    subtitles.store_extracted(
                        module_id,
                        &message.collection_id,
                        &source.root_token,
                        &source.path_rel,
                        &track.key,
                        &message.source_revision,
                        &extracted,
                    )?;
                    keys.push(track.key);
                }
                // The keys are what makes a stored entry findable again: the
                // sweep asks for `e<stream_index>`, and anything else caches
                // under a name it will never look for.
                tracing::info!(
                    module_id,
                    collection = %message.collection_id,
                    path = %source.path_rel,
                    keys = %keys.join(","),
                    "extracted subtitles stored"
                );
                registry
                    .catalogue()
                    .finish_subtitle_source(module_id, &message.collection_id, &source, "text")
                    .await?;
                subtitles.wake();
            }
        }
        host_to_hub::Msg::ImageSubtitles(mut message) => {
            let source = message.source.as_ref().context("missing image source")?;
            // Resolved through the catalogue, so a sidecar's `.idx` path
            // lands on the media file that lists it instead of being
            // dropped for not being a file row.
            let file = registry
                .catalogue()
                .source_file(module_id, &message.collection_id, source)
                .await?;
            if !message.error.is_empty() || file.is_none() {
                partial.remove(&(
                    message.collection_id.clone(),
                    source.root_token.clone(),
                    source.path_rel.clone(),
                    message.sub_index,
                    message.source_revision.clone(),
                ));
                if file.is_some() {
                    // The host's verdict on this track releases the file's
                    // `sets` row with a retry instead of leaving it leased.
                    registry
                        .catalogue()
                        .fail_subtitle_source(
                            module_id,
                            &message.collection_id,
                            source,
                            "sets",
                            crate::queue::now() + crate::subtitles::work::HOST_ERROR_RETRY_SECS,
                            &message.error,
                        )
                        .await?;
                    subtitles.wake();
                } else {
                    tracing::warn!(module_id, collection = %message.collection_id,
                        path = %source.path_rel, "image subtitle sets discarded: unknown source");
                }
                return Ok(());
            }
            match accept_chunk(partial, &mut message) {
                Chunk::Complete(bytes) => {
                    subtitles.store_image_sets(module_id, &message).await?;
                    tracing::debug!(bytes, "image subtitle transfer complete");
                    if let Some(file) = file {
                        // Settles the `sets` row once every image track has
                        // landed, and hands the file to OCR.
                        subtitles.sets_landed(registry, file).await?;
                    }
                }
                Chunk::TooBig(bytes) => {
                    tracing::warn!(bytes, "image subtitle transfer exceeded existing limit");
                    let source = message.source.as_ref().context("missing image source")?;
                    registry
                        .catalogue()
                        .fail_subtitle_source(
                            module_id,
                            &message.collection_id,
                            source,
                            "sets",
                            crate::queue::now() + crate::subtitles::work::HOST_ERROR_RETRY_SECS,
                            "display sets exceed the transfer limit",
                        )
                        .await?;
                    subtitles.wake();
                }
                Chunk::More => {}
            }
        }
        host_to_hub::Msg::DiscoveryStatus(status) => {
            registry.update_scan_progress(
                module_id,
                &status.collection_id,
                status.scanned,
                status.failed,
                status.skipped,
                !status.scanning,
            );
            registry.report_discovery(module_id, generation, status);
        }
        _ => anyhow::bail!("this mediahost operation is unavailable during catalogue integration"),
    }
    Ok(())
}

#[cfg(test)]
mod forget_link_tests {
    use super::forget_link;
    use crate::registry::Registry;
    use crate::sessions::Sessions;
    use std::sync::Arc;

    /// A fence, not a proof: it cannot fail for the ordering bug it was written
    /// after, because that bug lived in the CALLER — three sites writing these
    /// calls in two orders, one of them split across a drain. What it pins is
    /// that the two maps are cleared by one call, so a future edit cannot put a
    /// drain between them again without deleting this.
    #[tokio::test]
    async fn both_maps_are_forgotten_together() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = Arc::new(Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        ));
        let sessions = Arc::new(Sessions::new(dir.path().join("scratch")));

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        registry.connected("01HOST", "mediahost", "nas", "fp", "test");
        let generation = registry
            .register_link(
                "01HOST",
                tx.clone(),
                kahawai_proto::PROTOCOL_MINOR,
                kahawai_core::segments::DETECTOR_GENERATION,
            )
            .0;
        assert!(registry.is_connected("01HOST"));

        forget_link(&registry, &sessions, "01HOST", generation, &tx).await;

        assert!(
            !registry.is_connected("01HOST"),
            "the state map must say gone"
        );
        assert!(
            registry
                .send_to_host("01HOST", Default::default())
                .await
                .is_err(),
            "and the send side must be gone with it — a host reported present \
             with no way to reach it is answered 409 instead of 503"
        );
    }

    /// A dying link must not clear a live one's state.
    ///
    /// A drop without a FIN leaves the old task waiting out its heartbeat
    /// window while the box comes back and registers afresh. Clearing by module
    /// id then wiped the NEW link, and nothing puts it back — the host goes on
    /// heartbeating into a hub that thinks it is gone.
    #[tokio::test]
    async fn a_replaced_link_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = Arc::new(Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        ));
        let sessions = Arc::new(Sessions::new(dir.path().join("scratch")));

        let (old_tx, _old_rx) = tokio::sync::mpsc::channel(1);
        registry.connected("01HOST", "mediahost", "nas", "fp", "test");
        let (old_generation, _) = registry.register_link(
            "01HOST",
            old_tx.clone(),
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );

        // The box reconnects before the old task notices.
        let (new_tx, _new_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link(
            "01HOST",
            new_tx.clone(),
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );
        registry.connected("01HOST", "mediahost", "nas", "fp", "test");

        // Now the old task times out and tears down.
        forget_link(&registry, &sessions, "01HOST", old_generation, &old_tx).await;

        assert!(
            registry.is_connected("01HOST"),
            "the live connection must survive its predecessor's teardown"
        );
        assert!(
            registry
                .send_to_host("01HOST", Default::default())
                .await
                .is_ok(),
            "and its sender must still be reachable"
        );
    }

    #[tokio::test]
    async fn protocol_four_baseline_opens_discovery_but_detector_generation_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let (baseline_tx, _baseline_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link(
            "host",
            baseline_tx,
            0,
            kahawai_core::segments::DETECTOR_GENERATION,
        );
        assert!(registry.host_supports_segment_detection("host"));
        assert!(registry.host_supports_loudness_analysis("host"));

        let (mismatch_tx, _mismatch_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link(
            "host",
            mismatch_tx,
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION - 1,
        );
        assert!(!registry.host_supports_segment_detection("host"));
        assert!(registry.host_supports_loudness_analysis("host"));

        let (new_tx, _new_rx) = tokio::sync::mpsc::channel(1);
        registry.register_link(
            "host",
            new_tx,
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );
        assert!(registry.host_supports_segment_detection("host"));
        assert!(registry.host_supports_loudness_analysis("host"));
    }
}

#[cfg(test)]
mod subtitle_revision_tests {
    use super::*;

    #[test]
    fn interleaved_revisions_never_share_partial_image_sets() {
        let mut partial = std::collections::HashMap::new();
        let message = |revision: &str, last, byte| kahawai_proto::v1::ImageSubtitles {
            collection_id: "movies".into(),
            source: Some(kahawai_proto::v1::SourcePath::new("root", "film.mkv")),
            source_revision: revision.into(),
            done: Some(last),
            blocks: vec![kahawai_proto::v1::ImageSubBlock {
                payload: vec![byte],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(matches!(
            accept_chunk(&mut partial, &mut message("old", false, 1)),
            Chunk::More
        ));
        let mut new = message("new", true, 2);
        assert!(matches!(
            accept_chunk(&mut partial, &mut new),
            Chunk::Complete(1)
        ));
        assert_eq!(new.blocks[0].payload, vec![2]);
        let mut old = message("old", true, 3);
        assert!(matches!(
            accept_chunk(&mut partial, &mut old),
            Chunk::Complete(2)
        ));
        assert_eq!(
            old.blocks
                .iter()
                .flat_map(|b| b.payload.clone())
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert!(partial.is_empty());
    }
}
