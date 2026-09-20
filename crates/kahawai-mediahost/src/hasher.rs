//! Exact-source hashing, sparse declarations and subtitle extraction.
//!
//! Per-kind queues preserve useful ordering and deduplication; the universal
//! scheduler, not this worker, decides admission and interruption. Urgent
//! extraction registers immediate demand because a viewer is waiting.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use kahawai_proto::v1::{
    AttachmentsWorklist, ExtractSubs, FileAttachments, FileHash, FileHashes, FileKeyframeInterval,
    FileSubtitles, FileVideoGeometry, Hashlist, HostToHub, SubTrack, SubsWorklist, host_to_hub,
};

type BlockingGuard = Arc<dyn Send + Sync>;
use crate::ed2k::{self, CHUNK, Ed2k};
use crate::scan::CollectionConfig;
use crate::scheduler::{JobPermit, Priority, Scheduler};

/// Pause between chunks even when idle: bounds the read rate (~95 MB/s)
/// so the hasher never monopolizes the disk it shares with everything else.
const CHUNK_PACE: Duration = Duration::from_millis(100);

/// Work arriving from the hub via the link dispatch loop.
pub enum JobMsg {
    Hashlist(Hashlist),
    SubsWorklist(SubsWorklist),
    ImageSubsWorklist(kahawai_proto::v1::ImageSubsWorklist),
    AttachmentsWorklist(AttachmentsWorklist),
    KeyframeWorklist(kahawai_proto::v1::KeyframeWorklist),
    VideoGeometryWorklist(kahawai_proto::v1::VideoGeometryWorklist),
    Urgent(ExtractSubs),
    UrgentImage(kahawai_proto::v1::ExtractImageSubs),
}

/// A local, retryable discovery failure. This never crosses a hub link: the
/// catalogue releases the still-running exact-source claim before scheduling
/// the path again.
pub struct RetryClaim {
    pub collection_id: String,
    pub kind: &'static str,
    pub source: kahawai_proto::v1::SourcePath,
}

/// One deduped work queue (collection, root token, path_rel) per tier.
#[derive(Default)]
struct Tier {
    q: VecDeque<(String, String, String)>,
    seen: HashSet<(String, String, String)>,
    /// Only the subtitle tier has these: the hub's opaque revision for each
    /// queued file, which the reply must echo or the hub cannot key the cues
    /// to the bytes it asked about and throws them away, and the rank it gave
    /// that file in the round's backlog.
    meta: HashMap<(String, String, String), SubsWork>,
}

fn source(root_token: &str, path_rel: &str) -> Option<kahawai_proto::v1::SourcePath> {
    Some(kahawai_proto::v1::SourcePath {
        root_token: root_token.to_string(),
        path_rel: path_rel.to_string(),
    })
}

/// What the hub said about one queued subtitle file, beyond its identity.
#[derive(Clone, Default)]
struct SubsWork {
    revision: String,
    rank: u32,
}

impl Tier {
    fn push(&mut self, collection_id: &str, sources: Vec<kahawai_proto::v1::SourcePath>) {
        for source in sources {
            let key = (
                collection_id.to_string(),
                source.root_token,
                source.path_rel,
            );
            if self.seen.insert(key.clone()) {
                self.q.push_back(key);
            }
        }
    }

    fn push_revisioned(
        &mut self,
        collection_id: &str,
        items: Vec<kahawai_proto::v1::SubsWorkItem>,
    ) {
        for item in items {
            let Some(source) = item.source else { continue };
            let key = (
                collection_id.to_string(),
                source.root_token,
                source.path_rel,
            );
            // A re-offer of a file already queued still refreshes both:
            // the newer worklist saw the newer bytes, and ranked the file
            // against a backlog that has moved on since.
            self.meta.insert(
                key.clone(),
                SubsWork {
                    revision: item.source_revision.clone(),
                    rank: item.rank,
                },
            );
            if self.seen.insert(key.clone()) {
                self.q.push_back(key);
            }
        }
    }

    /// Put the queue in the hub's ranked order.
    ///
    /// A worklist names one collection, so a round arrives as several
    /// messages and arrival order says nothing about what is most wanted.
    /// The rank does, across the whole round.
    fn sort_by_rank(&mut self) {
        let rank = |key: &(String, String, String)| {
            self.meta.get(key).map(|work| work.rank).unwrap_or(u32::MAX)
        };
        let mut queued: Vec<_> = self.q.drain(..).collect();
        queued.sort_by_key(rank);
        self.q.extend(queued);
    }
}

#[derive(Default)]
struct Queues {
    urgent: VecDeque<(String, String, String, String)>,
    urgent_image: VecDeque<kahawai_proto::v1::ExtractImageSubs>,
    /// The same walk, requested by a sweep rather than a viewer: admitted
    /// through the scheduler at prewarm priority instead of entering as
    /// interactive.
    background_image: VecDeque<kahawai_proto::v1::ExtractImageSubs>,
    /// What [Self::background_image] already holds. A sweep re-asks for work
    /// it has not seen land yet, so without this a track queued behind a
    /// large backlog is enqueued again every round it fails to surface.
    background_image_seen: HashSet<(String, String, String, u32, String)>,
    ed2k: Tier,
    subs: Tier,
    atts: Tier,
    keys: Tier,
    geometry: Tier,
}

/// Identity of one queued background image walk: which track of which
/// revision of which file. A request naming all four is the same work as one
/// already waiting.
fn background_image_key(
    e: &kahawai_proto::v1::ExtractImageSubs,
) -> Option<(String, String, String, u32, String)> {
    let source = e.source.as_ref()?;
    Some((
        e.collection_id.clone(),
        source.root_token.clone(),
        source.path_rel.clone(),
        e.sub_index,
        e.source_revision.clone(),
    ))
}

/// Route one message into its tier; returns true for urgent work.
fn intake(msg: JobMsg, queues: &mut Queues) -> bool {
    match msg {
        // A viewer waiting to start a burn-in session is urgent; the hub's
        // idle sweep warming the same cache is not, and says so.
        JobMsg::UrgentImage(e) => {
            let urgent = !e.background;
            if urgent {
                queues.urgent_image.push_back(e);
            } else if background_image_key(&e)
                .is_some_and(|key| queues.background_image_seen.insert(key))
            {
                queues.background_image.push_back(e);
            }
            urgent
        }
        JobMsg::Urgent(e) => {
            if let Some(source) = e.source {
                queues.urgent.push_back((
                    e.collection_id,
                    source.root_token,
                    source.path_rel,
                    e.source_revision,
                ));
            }
            true
        }
        JobMsg::Hashlist(h) => {
            queues.ed2k.push(&h.collection_id, h.sources);
            false
        }
        // A batch of background image walks: each item takes the same
        // deduped path a single background ExtractImageSubs does.
        JobMsg::ImageSubsWorklist(w) => {
            tracing::info!(collection = %w.collection_id, tracks = w.items.len(),
                "image subtitle worklist received");
            for item in w.items {
                let e = kahawai_proto::v1::ExtractImageSubs {
                    collection_id: w.collection_id.clone(),
                    source: item.source,
                    sub_index: item.sub_index,
                    source_revision: item.source_revision,
                    background: true,
                };
                if background_image_key(&e)
                    .is_some_and(|key| queues.background_image_seen.insert(key))
                {
                    queues.background_image.push_back(e);
                }
            }
            false
        }
        JobMsg::SubsWorklist(w) => {
            // Logged on receipt, mirroring the hub's "sending" line: a
            // worklist that never arrives is one grep rather than a guess.
            tracing::info!(collection = %w.collection_id, files = w.items.len(),
                "subtitle worklist received");
            queues.subs.push_revisioned(&w.collection_id, w.items);
            false
        }
        JobMsg::AttachmentsWorklist(w) => {
            queues.atts.push(&w.collection_id, w.sources);
            false
        }
        JobMsg::KeyframeWorklist(w) => {
            // Logged on receipt, mirroring the hub's "sending" line:
            // between them, a worklist that never arrives is one grep
            // rather than a guess.
            tracing::info!(collection = %w.collection_id, files = w.sources.len(),
                "keyframe worklist received");
            queues.keys.push(&w.collection_id, w.sources);
            false
        }
        JobMsg::VideoGeometryWorklist(w) => {
            tracing::info!(collection = %w.collection_id, files = w.sources.len(),
                "video geometry worklist received");
            queues.geometry.push(&w.collection_id, w.sources);
            false
        }
    }
}

/// Which background tier a drained job came from — three of them now,
/// and a bool cannot say.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Bg {
    Ed2k,
    Atts,
    Keyframe,
    Geometry,
    Subs,
}

impl Bg {
    fn priority(self) -> Priority {
        match self {
            Self::Atts => Priority::Attachments,
            Self::Keyframe => Priority::Keyframes,
            Self::Geometry => Priority::Geometry,
            Self::Ed2k => Priority::Ed2k,
            Self::Subs => Priority::SubtitlePrewarm,
        }
    }

    fn cpu_heavy(self) -> bool {
        // Geometry uses the same bounded discovery as a scan. ED2K hashes
        // every byte and therefore needs sustained CPU admission as well.
        matches!(self, Self::Ed2k)
    }
}

pub async fn run(
    mut rx: tokio::sync::mpsc::Receiver<JobMsg>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    collections: Vec<CollectionConfig>,
    scheduler: Scheduler,
    owner: Option<String>,
    retry_tx: Option<tokio::sync::mpsc::UnboundedSender<RetryClaim>>,
) {
    let mut queues = Queues::default();
    let mut jobs = tokio::task::JoinSet::new();
    let mut background_jobs = HashMap::new();
    let mut intake_open = true;
    loop {
        while let Ok(message) = rx.try_recv() {
            intake(message, &mut queues);
        }

        while let Some((collection_id, root_token, path_rel, revision)) = queues.urgent.pop_front()
        {
            let scheduler = scheduler.clone();
            let collections = collections.clone();
            let tx = tx.clone();
            jobs.spawn(async move {
                // Text subtitle extraction demuxes but does not decode.
                let resources = scheduler.resources([root_token.as_str()], false);
                let urgent: BlockingGuard = Arc::new(scheduler.enter_interactive(
                    resources,
                    format!("urgent subtitles {collection_id}/{path_rel}"),
                ));
                extract_and_send(
                    &collections,
                    &collection_id,
                    &root_token,
                    &path_rel,
                    &revision,
                    Some(urgent),
                    None,
                    &tx,
                )
                .await;
            });
        }
        while let Some(e) = queues.urgent_image.pop_front() {
            if let Some(source) = e.source {
                let scheduler = scheduler.clone();
                let collections = collections.clone();
                let tx = tx.clone();
                jobs.spawn(async move {
                    // Sparse image-track extraction walks the container index;
                    // decoding remains hub-owned OCR work.
                    let resources = scheduler.resources([source.root_token.as_str()], false);
                    let urgent: BlockingGuard = Arc::new(scheduler.enter_interactive(
                        resources,
                        format!(
                            "urgent image subtitles {}/{}",
                            e.collection_id, source.path_rel
                        ),
                    ));
                    extract_image_and_send(
                        &collections,
                        &e.collection_id,
                        &source.root_token,
                        &source.path_rel,
                        e.sub_index,
                        &e.source_revision,
                        urgent,
                        None,
                        &tx,
                    )
                    .await;
                });
            }
        }

        while let Some(e) = queues.background_image.pop_front() {
            if let Some(key) = background_image_key(&e) {
                queues.background_image_seen.remove(&key);
            }
            if let Some(source) = e.source {
                let scheduler = scheduler.clone();
                let collections = collections.clone();
                let tx = tx.clone();
                let owner = owner.clone();
                jobs.spawn(async move {
                    // I/O only, like the text extraction it sits beside:
                    // walking the container index reads, it does not decode.
                    // Decoding stays hub-owned OCR work.
                    let resources = scheduler.resources([source.root_token.as_str()], false);
                    let label = format!(
                        "image subtitle prewarm {}/{}",
                        e.collection_id, source.path_rel
                    );
                    let Ok(permit) = scheduler
                        .acquire(Priority::SubtitlePrewarm, resources, owner, label)
                        .await
                    else {
                        return;
                    };
                    let guard: BlockingGuard = Arc::new(permit.clone());
                    extract_image_and_send(
                        &collections,
                        &e.collection_id,
                        &source.root_token,
                        &source.path_rel,
                        e.sub_index,
                        &e.source_revision,
                        guard,
                        Some(permit),
                        &tx,
                    )
                    .await;
                });
            }
        }

        for which in [Bg::Atts, Bg::Keyframe, Bg::Geometry, Bg::Ed2k, Bg::Subs] {
            let tier = match which {
                Bg::Ed2k => &mut queues.ed2k,
                Bg::Subs => &mut queues.subs,
                Bg::Atts => &mut queues.atts,
                Bg::Keyframe => &mut queues.keys,
                Bg::Geometry => &mut queues.geometry,
            };
            if which == Bg::Subs {
                tier.sort_by_rank();
            }
            while let Some(key) = tier.q.pop_front() {
                let (collection_id, root_token, path_rel) = key.clone();
                // Empty for every tier but Subs, and harmless there: the
                // other kinds of work carry no revision to echo.
                let revision = tier
                    .meta
                    .get(&key)
                    .map(|work| work.revision.clone())
                    .unwrap_or_default();
                let scheduler = scheduler.clone();
                let collections = collections.clone();
                let tx = tx.clone();
                let retry_tx = retry_tx.clone();
                let owner = owner.clone();
                let task = jobs.spawn(async move {
                    let resources = scheduler.resources([root_token.as_str()], which.cpu_heavy());
                    let permit = scheduler
                        .acquire(
                            which.priority(),
                            resources,
                            owner,
                            format!("{which:?} {collection_id}/{path_rel}"),
                        )
                        .await;
                    if let Ok(permit) = permit {
                        run_background_job(
                            which,
                            &collections,
                            &collection_id,
                            &root_token,
                            &path_rel,
                            &revision,
                            permit,
                            &tx,
                            retry_tx.as_ref(),
                        )
                        .await;
                    }
                });
                background_jobs.insert(task.id(), (which, key));
            }
        }

        if !intake_open && jobs.is_empty() {
            return;
        }
        tokio::select! {
            message = rx.recv(), if intake_open => match message {
                Some(message) => { intake(message, &mut queues); }
                None => intake_open = false,
            },
            completed = jobs.join_next_with_id(), if !jobs.is_empty() => {
                let completed_id = match completed {
                    Some(Ok((id, ()))) => Some(id),
                    Some(Err(error)) => {
                        let id = error.id();
                        tracing::warn!(%error, "mediahost work task failed");
                        Some(id)
                    }
                    None => None,
                };
                if let Some((which, key)) = completed_id
                    .and_then(|id| background_jobs.remove(&id))
                {
                    let tier = match which {
                        Bg::Ed2k => &mut queues.ed2k,
                        Bg::Subs => &mut queues.subs,
                        Bg::Atts => &mut queues.atts,
                        Bg::Keyframe => &mut queues.keys,
                        Bg::Geometry => &mut queues.geometry,
                    };
                    tier.seen.remove(&key);
                    tier.meta.remove(&key);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_background_job(
    which: Bg,
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    // Only `Bg::Subs` has one; empty everywhere else.
    source_revision: &str,
    permit: JobPermit,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    retry_tx: Option<&tokio::sync::mpsc::UnboundedSender<RetryClaim>>,
) {
    match which {
        Bg::Ed2k => {
            let result = hash_one(collections, collection_id, root_token, path_rel, permit).await;
            let hash = match result {
                Ok(mut hash) => {
                    hash.source = source(root_token, path_rel);
                    tracing::info!(collection = %collection_id, path = %path_rel,
                        ed2k = %hash.ed2k_hex, crc_ok = hash.crc_ok || !hash.crc_checked,
                        "ed2k computed");
                    hash
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::warn!(collection = %collection_id, path = %path_rel,
                        %error, "ed2k failed for exact source revision");
                    FileHash {
                        source: source(root_token, path_rel),
                        error,
                        ..Default::default()
                    }
                }
            };
            let _ = crate::send_link_message(
                tx,
                HostToHub {
                    msg: Some(host_to_hub::Msg::FileHashes(FileHashes {
                        collection_id: collection_id.to_string(),
                        hashes: vec![hash],
                    })),
                },
            )
            .await;
        }
        Bg::Subs => {
            let background: BlockingGuard = Arc::new(permit.clone());
            extract_and_send(
                collections,
                collection_id,
                root_token,
                path_rel,
                source_revision,
                Some(background),
                Some(permit),
                tx,
            )
            .await;
        }
        Bg::Atts => {
            let background: BlockingGuard = Arc::new(permit);
            declare_and_send(
                collections,
                collection_id,
                root_token,
                path_rel,
                background,
                tx,
                retry_tx,
            )
            .await;
        }
        Bg::Keyframe => {
            let background: BlockingGuard = Arc::new(permit);
            measure_keyframes_and_send(
                collections,
                collection_id,
                root_token,
                path_rel,
                background,
                tx,
                retry_tx,
            )
            .await;
        }
        Bg::Geometry => {
            let background: BlockingGuard = Arc::new(permit);
            probe_geometry_and_send(
                collections,
                collection_id,
                root_token,
                path_rel,
                background,
                tx,
            )
            .await;
        }
    }
}

/// MH-4 backfill: declare one file's attachments and its chapters —
/// sparse header reads, ~0.3 s even over a network mount — and ship them
/// to the hub. An error here sends NOTHING, on purpose: the reader settles
/// deterministic shape problems as "[]" itself, so what escapes is
/// weather (I/O on the mount), and the file stays listed to be retried.
async fn declare_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    background: BlockingGuard,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    retry_tx: Option<&tokio::sync::mpsc::UnboundedSender<RetryClaim>>,
) {
    let result: anyhow::Result<(u64, String, String)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let blocking = background.clone();
        let (atts, chapters) = tokio::task::spawn_blocking(move || {
            let _background = blocking;
            kahawai_media::subindex::declare_container(&path)
        })
        .await??;
        Ok((
            size,
            serde_json::to_string(&atts)?,
            serde_json::to_string(&chapters)?,
        ))
    }
    .await;
    let (size, attachments_json, chapters_json) = match result {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "attachment declaration failed");
            retry_local_claim(
                retry_tx,
                collection_id,
                "file_attachments",
                root_token,
                path_rel,
            );
            return;
        }
    };
    // Say which, because the one investigation this line shows up in is
    // "why has this file no chapters", and a message that claims both when
    // only the fonts were found answers it wrongly.
    if attachments_json != "[]" || chapters_json != "[]" {
        tracing::info!(
            collection = %collection_id, path = %path_rel,
            attachments = attachments_json != "[]",
            chapters = chapters_json != "[]",
            "container header declared"
        );
    }
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileAttachments(FileAttachments {
            collection_id: collection_id.to_string(),
            source: source(root_token, path_rel),
            size,
            attachments_json,
            chapters_json: Some(chapters_json),
        })),
    };
    drop(background);
    let _ = crate::send_link_message(tx, msg).await;
}

/// HUB-17 backfill: measure one file's longest keyframe gap from the
/// container index and ship it. An UNKNOWN result is reported too —
/// silence would leave the file in the worklist forever, and "we looked
/// and this container has no index we can read" is a real answer.
async fn measure_keyframes_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    background: BlockingGuard,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    retry_tx: Option<&tokio::sync::mpsc::UnboundedSender<RetryClaim>>,
) {
    let result: anyhow::Result<(u64, Option<u32>)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let blocking = background.clone();
        let ms = tokio::task::spawn_blocking(move || {
            let _background = blocking;
            kahawai_media::subindex::max_keyframe_interval_ms(&path)
        })
        .await?
        .unwrap_or(None);
        Ok((size, ms))
    }
    .await;
    let (size, ms) = match result {
        Ok(v) => v,
        // Vanished or unreadable: say nothing and let the next scan
        // reconcile, rather than recording a measurement we did not make.
        Err(e) => {
            tracing::debug!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "keyframe measurement failed");
            retry_local_claim(
                retry_tx,
                collection_id,
                "file_keyframe",
                root_token,
                path_rel,
            );
            return;
        }
    };
    tracing::debug!(collection = %collection_id, path = %path_rel, ms = ?ms,
        "keyframe interval measured");
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileKeyframeInterval(
            FileKeyframeInterval {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                size,
                max_keyframe_interval_ms: ms,
            },
        )),
    };
    drop(background);
    let _ = crate::send_link_message(tx, msg).await;
}

fn retry_local_claim(
    retry_tx: Option<&tokio::sync::mpsc::UnboundedSender<RetryClaim>>,
    collection_id: &str,
    kind: &'static str,
    root_token: &str,
    path_rel: &str,
) {
    if let Some(retry_tx) = retry_tx {
        let _ = retry_tx.send(RetryClaim {
            collection_id: collection_id.to_string(),
            kind,
            source: kahawai_proto::v1::SourcePath {
                root_token: root_token.to_string(),
                path_rel: path_rel.to_string(),
            },
        });
    }
}

/// Source-owned PAR/orientation/display dimensions for one exact file. This is
/// deliberately a targeted probe: it opens only the named source and does no
/// directory walk, hash, sidecar inspection, reconciliation or generation work.
async fn probe_geometry_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    background: BlockingGuard,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let path = match crate::serve::resolve_rel(collections, collection_id, root_token, path_rel) {
        Ok(path) => path,
        Err(e) => {
            drop(background);
            let _ = crate::send_link_message(
                tx,
                HostToHub {
                    msg: Some(host_to_hub::Msg::FileVideoGeometry(FileVideoGeometry {
                        collection_id: collection_id.to_string(),
                        source: source(root_token, path_rel),
                        size: 0,
                        geometry_json: String::new(),
                        error: format!("{e:#}"),
                    })),
                },
            )
            .await;
            return;
        }
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let blocking = background.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _background = blocking;
        kahawai_media::probe_video_geometry(&path, Duration::from_secs(30))
    })
    .await;
    let (geometry_json, error) = match result {
        Ok(Ok(geometry)) => (
            serde_json::to_string(&geometry).unwrap_or_default(),
            String::new(),
        ),
        Ok(Err(e)) => (String::new(), format!("{e:#}")),
        Err(e) => (String::new(), format!("geometry probe task failed: {e}")),
    };
    let msg = HostToHub {
        msg: Some(host_to_hub::Msg::FileVideoGeometry(FileVideoGeometry {
            collection_id: collection_id.to_string(),
            source: source(root_token, path_rel),
            size,
            geometry_json,
            error,
        })),
    };
    drop(background);
    let _ = crate::send_link_message(tx, msg).await;
}

/// HUB-32b: one image subtitle track's raw display-set blocks, read
/// through the container's own index. Undecoded on purpose — the
/// payloads are compact this way and the pipeline worker owns the
/// decoders.
#[allow(clippy::too_many_arguments)] // exact source/revision plus scheduler ownership and transport
async fn extract_image_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    sub_index: u32,
    source_revision: &str,
    blocking_guard: BlockingGuard,
    // Present for a background walk: the permit playback pauses and
    // cancels. A walk without one reads to completion whatever else the
    // mediahost is being asked to do.
    permit: Option<JobPermit>,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let started = std::time::Instant::now();
    let (collections2, cid, token, prel) = (
        collections.to_vec(),
        collection_id.to_string(),
        root_token.to_string(),
        path_rel.to_string(),
    );
    let blocking_guard2 = blocking_guard.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _blocking_guard = blocking_guard2;
        let path = crate::serve::resolve_rel(&collections2, &cid, &token, &prel)?;
        // A `.idx` path means a VobSub sidecar pair, not a container:
        // `sub_index` is the track index INSIDE the idx, and the result
        // is shaped exactly like a demuxed S_VOBSUB track (idx text as
        // codec_private), so nothing downstream tells them apart.
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("idx"))
        {
            let idx = std::fs::read_to_string(&path)?;
            let sub = std::fs::read(path.with_extension("sub"))?;
            let blocks = kahawai_media::vobsub_file::extract_track(&idx, &sub, sub_index)?;
            return Ok(Some(kahawai_media::subindex::ImageTrack {
                codec: "S_VOBSUB".into(),
                codec_private: Some(idx.into_bytes()),
                blocks,
            }));
        }
        let mut src = kahawai_media::remux::FileSource::open(&path)?;
        // Local disk: no budget needed, and a header walk is still
        // only a few percent of the file.
        match permit {
            Some(permit) => kahawai_media::subindex::extract_image_track_interruptible(
                &mut src,
                sub_index as usize,
                std::time::Duration::from_secs(120),
                move || permit.checkpoint_blocking(),
            ),
            None => kahawai_media::subindex::extract_image_track(
                &mut src,
                sub_index as usize,
                std::time::Duration::from_secs(120),
            ),
        }
    })
    .await;

    let msg = match result {
        Ok(Ok(Some(track))) => {
            tracing::info!(
                collection = collection_id,
                path = path_rel,
                track = sub_index,
                blocks = track.blocks.len(),
                ms = started.elapsed().as_millis(),
                "image display sets extracted"
            );
            // Chunked, because a track is not a message: a PGS stream
            // of a whole film runs to tens of MiB and the largest that
            // ever crossed intact was 63.8 MiB against a 64 MiB limit.
            // Over it the shared link stream resets, taking scans and
            // leases with it.
            let blocks: Vec<kahawai_proto::v1::ImageSubBlock> = track
                .blocks
                .into_iter()
                .map(
                    |(start_ms, dur, payload)| kahawai_proto::v1::ImageSubBlock {
                        start_ms,
                        duration_ms: dur.unwrap_or(0),
                        payload,
                    },
                )
                .collect();
            drop(blocking_guard);
            send_chunked(
                tx,
                collection_id,
                root_token,
                path_rel,
                sub_index,
                source_revision,
                track.codec,
                track.codec_private.unwrap_or_default(),
                blocks,
            )
            .await;
            return;
        }
        other => {
            let error = match other {
                Ok(Ok(None)) => "no such image track, or the container has no usable index".into(),
                Ok(Err(e)) => format!("{e:#}"),
                Err(e) => format!("{e}"),
                Ok(Ok(Some(_))) => unreachable!("handled above"),
            };
            tracing::warn!(collection = collection_id, path = path_rel, track = sub_index,
                %error, "image display-set extraction failed");
            kahawai_proto::v1::ImageSubtitles {
                collection_id: collection_id.into(),
                source: source(root_token, path_rel),
                source_revision: source_revision.into(),
                sub_index,
                error,
                // One message, and it is the last one.
                done: Some(true),
                ..Default::default()
            }
        }
    };
    drop(blocking_guard);
    let _ = crate::send_link_message(
        tx,
        HostToHub {
            msg: Some(host_to_hub::Msg::ImageSubtitles(msg)),
        },
    )
    .await;
}

/// Extract every text subtitle track of one local file (single demux
/// pass at disk speed) and ship the results to the hub.
#[allow(clippy::too_many_arguments)] // exact source/revision plus scheduler ownership and transport
async fn extract_and_send(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    source_revision: &str,
    background: Option<BlockingGuard>,
    permit: Option<JobPermit>,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) {
    let started = std::time::Instant::now();
    let result: anyhow::Result<(u64, Vec<(usize, kahawai_media::subtitles::Extracted)>)> = async {
        let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
        let size = std::fs::metadata(&path)?.len();
        let blocking = background.clone();
        let checkpoint = permit.clone();
        let tracks = tokio::task::spawn_blocking(move || {
            let _background = blocking;
            // Sparse first (index-driven reads, no demux); trust it
            // only when it actually produced events — a parser gap
            // must never look like "no subtitles".
            match kahawai_media::subindex::extract_sparse(&path) {
                Ok(Some(tracks)) if tracks.iter().any(|(_, ex)| !ex.cues.is_empty()) => {
                    tracing::debug!(path = %path.display(), "sparse extraction");
                    Ok(tracks)
                }
                _ => {
                    tracing::debug!(path = %path.display(), "sequential extraction");
                    let source = kahawai_media::remux::FileSource::open(&path)?;
                    match checkpoint {
                        Some(permit) => {
                            kahawai_media::subtitles::extract_embedded_all_interruptible(
                                Box::new(source),
                                move || permit.checkpoint_blocking(),
                            )
                        }
                        None => kahawai_media::subtitles::extract_embedded_all(Box::new(source)),
                    }
                }
            }
        })
        .await??;
        Ok((size, tracks))
    }
    .await;

    let msg = match result {
        Ok((size, tracks)) => {
            tracing::info!(collection = %collection_id, path = %path_rel,
                tracks = tracks.len(), elapsed = ?started.elapsed(), "subtitles extracted");
            FileSubtitles {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                source_revision: source_revision.into(),
                size,
                tracks: tracks
                    .into_iter()
                    .map(|(idx, ex)| SubTrack {
                        key: format!("e{idx}"),
                        ass: ex.ass.unwrap_or_default(),
                        cues_json: serde_json::to_string(&ex.cues).unwrap_or_default(),
                    })
                    .collect(),
                error: String::new(),
            }
        }
        Err(e) => {
            tracing::warn!(collection = %collection_id, path = %path_rel,
                error = format!("{e:#}"), "subtitle extraction failed");
            FileSubtitles {
                collection_id: collection_id.to_string(),
                source: source(root_token, path_rel),
                source_revision: source_revision.into(),
                size: 0,
                tracks: vec![],
                error: format!("{e:#}"),
            }
        }
    };
    drop(background);
    let _ = crate::send_link_message(
        tx,
        HostToHub {
            msg: Some(host_to_hub::Msg::FileSubtitles(msg)),
        },
    )
    .await;
}

async fn hash_one(
    collections: &[CollectionConfig],
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    permit: JobPermit,
) -> anyhow::Result<FileHash> {
    let path = crate::serve::resolve_rel(collections, collection_id, root_token, path_rel)?;
    let claimed_crc = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ed2k::filename_crc32);

    let size = std::fs::metadata(&path)?.len();
    let mut file = std::fs::File::open(&path)?;
    let mut ed2k = Ed2k::default();
    let mut crc = crc32fast::Hasher::new();
    let mut remaining = size;

    while remaining > 0 {
        permit.checkpoint().await?;
        let want = remaining.min(CHUNK as u64) as usize;
        let blocking = permit.clone();
        let (f, buf) = tokio::task::spawn_blocking(move || {
            let _background = blocking;
            let mut buf = vec![0u8; want];
            file.read_exact(&mut buf).map(|_| (file, buf))
        })
        .await??;
        file = f;
        ed2k.update(&buf);
        if claimed_crc.is_some() {
            crc.update(&buf);
        }
        remaining -= want as u64;
        tokio::time::sleep(CHUNK_PACE).await;
    }

    let crc_ok = claimed_crc.map(|want| crc.finalize() == want);
    if crc_ok == Some(false) {
        tracing::warn!(path = %path.display(), "filename CRC32 mismatch — file may be corrupt");
    }
    Ok(FileHash {
        source: None, // caller fills
        ed2k_hex: ed2k.finish(),
        size,
        crc_checked: claimed_crc.is_some(),
        crc_ok: crc_ok.unwrap_or(false),
        error: String::new(),
    })
}

/// How much block payload rides in one message.
///
/// Far below the 64 MiB the link allows, because the limit is a cliff
/// rather than a budget: crossing it resets the shared stream, not just
/// this transfer. 4 MiB also keeps the receiver's buffer small and fits
/// inside the 4 MiB default that any less generous peer would impose.
const SETS_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Send one track's display sets as a run of messages, the last marked
/// done. The header fields ride on every chunk so the receiver can key
/// them without remembering an opening message.
#[allow(clippy::too_many_arguments)] // one complete wire source/track identity
async fn send_chunked(
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    collection_id: &str,
    root_token: &str,
    path_rel: &str,
    sub_index: u32,
    source_revision: &str,
    codec: String,
    codec_private: Vec<u8>,
    blocks: Vec<kahawai_proto::v1::ImageSubBlock>,
) {
    let mut chunk: Vec<kahawai_proto::v1::ImageSubBlock> = Vec::new();
    let mut bytes = 0usize;
    let mut sent = 0usize;
    let total = blocks.len();
    let mut iter = blocks.into_iter().peekable();
    while let Some(b) = iter.next() {
        bytes += b.payload.len();
        chunk.push(b);
        let last = iter.peek().is_none();
        if bytes < SETS_CHUNK_BYTES && !last {
            continue;
        }
        sent += chunk.len();
        let msg = kahawai_proto::v1::ImageSubtitles {
            collection_id: collection_id.into(),
            source: source(root_token, path_rel),
            source_revision: source_revision.into(),
            sub_index,
            codec: codec.clone(),
            codec_private: codec_private.clone(),
            blocks: std::mem::take(&mut chunk),
            error: String::new(),
            done: Some(last),
        };
        bytes = 0;
        if crate::send_link_message(
            tx,
            HostToHub {
                msg: Some(host_to_hub::Msg::ImageSubtitles(msg)),
            },
        )
        .await
        .is_err()
        {
            tracing::warn!(
                collection = collection_id,
                path = path_rel,
                track = sub_index,
                sent,
                total,
                "link closed mid-transfer; display sets abandoned"
            );
            return;
        }
    }
    // A track with no blocks at all still needs its one message, or the
    // hub waits for a transfer that never starts.
    if total == 0 {
        let _ = crate::send_link_message(
            tx,
            HostToHub {
                msg: Some(host_to_hub::Msg::ImageSubtitles(
                    kahawai_proto::v1::ImageSubtitles {
                        collection_id: collection_id.into(),
                        source: source(root_token, path_rel),
                        source_revision: source_revision.into(),
                        sub_index,
                        codec,
                        codec_private,
                        done: Some(true),
                        ..Default::default()
                    },
                )),
            },
        )
        .await;
    }
}

#[cfg(test)]
mod subtitle_revision_tests {
    use super::*;

    #[tokio::test]
    async fn chunked_image_replies_echo_the_requested_revision() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        send_chunked(
            &tx,
            "movies",
            "root",
            "film.mkv",
            0,
            "captured-revision",
            "S_HDMV/PGS".into(),
            vec![],
            vec![
                kahawai_proto::v1::ImageSubBlock {
                    payload: vec![1; SETS_CHUNK_BYTES],
                    ..Default::default()
                },
                kahawai_proto::v1::ImageSubBlock {
                    payload: vec![2],
                    ..Default::default()
                },
            ],
        )
        .await;
        for done in [false, true] {
            let Some(host_to_hub::Msg::ImageSubtitles(message)) = rx.recv().await.unwrap().msg
            else {
                panic!("wrong reply")
            };
            assert_eq!(message.source_revision, "captured-revision");
            assert_eq!(message.done, Some(done));
        }
        // Empty tracks still finish and carry the same identity.
        send_chunked(
            &tx,
            "movies",
            "root",
            "film.mkv",
            0,
            "empty-revision",
            "S_HDMV/PGS".into(),
            vec![],
            vec![],
        )
        .await;
        let Some(host_to_hub::Msg::ImageSubtitles(message)) = rx.recv().await.unwrap().msg else {
            panic!("wrong reply")
        };
        assert_eq!(message.source_revision, "empty-revision");
        assert_eq!(message.done, Some(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, rank: u32) -> kahawai_proto::v1::SubsWorkItem {
        kahawai_proto::v1::SubsWorkItem {
            source: Some(kahawai_proto::v1::SourcePath {
                root_token: "root".into(),
                path_rel: path.into(),
            }),
            source_revision: format!("rev-{path}"),
            rank,
        }
    }

    #[test]
    fn the_hubs_ranking_survives_being_split_across_collection_messages() {
        let mut tier = Tier::default();
        // One round, delivered as one message per collection. The series
        // someone is watching is ranked first but arrives last.
        tier.push_revisioned("anime", vec![item("Anime/a.mkv", 2)]);
        tier.push_revisioned("movies", vec![item("Movies/m.mkv", 1)]);
        tier.push_revisioned("series", vec![item("Series/watching.mkv", 0)]);
        tier.sort_by_rank();
        let order: Vec<_> = tier.q.iter().map(|(_, _, path)| path.as_str()).collect();
        assert_eq!(
            order,
            vec!["Series/watching.mkv", "Movies/m.mkv", "Anime/a.mkv"],
            "the round's own order, not the order its messages happened to arrive in"
        );
    }

    #[test]
    fn a_re_offer_updates_rank_and_revision_without_queueing_twice() {
        let mut tier = Tier::default();
        tier.push_revisioned("series", vec![item("Series/e01.mkv", 40)]);
        tier.push_revisioned("series", vec![item("Series/e02.mkv", 5)]);
        // The next round finds e01 in flight and ranks it first.
        let mut promoted = item("Series/e01.mkv", 0);
        promoted.source_revision = "rev-new".into();
        tier.push_revisioned("series", vec![promoted]);
        tier.sort_by_rank();
        let order: Vec<_> = tier.q.iter().map(|(_, _, path)| path.as_str()).collect();
        assert_eq!(order, vec!["Series/e01.mkv", "Series/e02.mkv"]);
        let key = (
            "series".to_string(),
            "root".to_string(),
            "Series/e01.mkv".to_string(),
        );
        assert_eq!(
            tier.meta.get(&key).map(|work| work.revision.as_str()),
            Some("rev-new"),
            "the newer worklist saw the newer bytes"
        );
    }
}
