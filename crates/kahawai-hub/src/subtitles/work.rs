//! Subtitle prewarm as claimed work on the shared queue driver, and OCR as
//! what happens when display sets land.
//!
//! **Prewarm.** Rows live in mediadb (`subtitle_jobs`, one per probed file
//! and kind) and are created by the catalogue commit; see the mediadb
//! module doc for their lifecycle. Two kinds share one mechanism: `text`
//! is the embedded text tracks, walked out of the container in one pass
//! and settled by the host's `FileSubtitles`; `sets` is the image tracks'
//! display sets, walked one track at a time and settled once every image
//! track has landed (`ImageSubtitles`). One step claims a ranked batch of
//! each kind per connected mediahost, settles the rows that need nothing
//! (no such track, or everything already cached — one `try_exists` per
//! claimed track, never a walk), and offers the rest as one worklist
//! message per batch carrying the batch rank. A row stays leased until
//! its landing, the host reconnects (its queue died with it), or the
//! lease lapses.
//!
//! **OCR** is a function of one thing: display sets on disk with no
//! `ocr.json` beside them. So it is not a queue. When sets land, the link
//! handler hands the file to one worker, which OCRs it while nobody is
//! watching. At startup the worker is seeded once from the catalogue, for
//! sets that landed while the hub was down. A track that fails leaves a
//! marker beside its answer's place and is skipped until an administrator
//! reruns.
//!
//! Wakes come from catalogue commits, landings and reconnects. The tick is
//! insurance.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use kahawai_mediadb::{SUBTITLE_KINDS, SourceFile};

use super::Subtitles;
use crate::queue::{self, Step};
use crate::registry::Registry;
use crate::sessions::{FileId, PartSource, Sessions};

/// Rows examined per claim and kind. A worklist names one collection, so
/// a batch is split by collection on the way out, and the rank carries
/// the batch's order across that split.
const BATCH: usize = 256;
/// A whole batch has to be walked before its lease matters: the host
/// works the list at its lowest priority, so this is the fallback for a
/// queue that vanished without a reconnect the hub saw.
const LEASE_SECS: i64 = 3600;
/// A failure the mediahost reported for one file retries after this;
/// attempts are counted by claims, so six reports park the row.
pub(crate) const HOST_ERROR_RETRY_SECS: i64 = 3600;
/// Between OCR files, so a large backlog is a background hum rather
/// than a CPU pin.
const OCR_PACE: Duration = Duration::from_secs(10);
/// Lost-event insurance; every real change wakes the driver.
const FALLBACK: Duration = Duration::from_secs(900);
/// Beside a track's `ocr.json` would be: the error that stopped it.
const FAILED_MARKER: &str = "ocr.failed";

/// The OCR worker's own bookkeeping: nothing durable, because the cache
/// directory is the durable state.
#[derive(Default)]
pub(crate) struct OcrState {
    pub queued: AtomicUsize,
    pub busy: AtomicUsize,
    pub done: AtomicUsize,
    pub failed: AtomicUsize,
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<SourceFile>>>,
}

fn part(file: &SourceFile) -> PartSource {
    PartSource {
        file_id: FileId::Catalogue(file.file_id.clone()),
        module_id: file.host.clone(),
        collection_id: file.remote_id.clone(),
        root_token: file.root_token.clone(),
        path_rel: file.path.clone(),
        size: file.size,
        mtime_unix: file.mtime,
        head_xxh3: file.head_hash as i64,
        tail_xxh3: file.tail_hash as i64,
        base_ms: 0,
        duration_ms: file.media.duration_ms.unwrap_or(0),
    }
}

fn tracks(file: &SourceFile) -> Vec<crate::tracks::Track> {
    crate::sessions::catalogue::tracks(
        file.item_id.as_deref().unwrap_or(""),
        &part(file),
        &file.media,
    )
}

/// Cheap pre-check from the probe alone, before any track is built.
fn has_image_tracks(media: &kahawai_core::media::MediaInfo) -> bool {
    media
        .subtitles
        .iter()
        .any(|s| crate::tracks::is_image_format(&s.format))
        || media
            .external_subtitles
            .iter()
            .any(|s| crate::tracks::is_image_format(&s.format))
}

fn connected_mediahosts(registry: &Registry) -> Vec<String> {
    registry
        .snapshot()
        .into_iter()
        .filter(|(_, state)| state.connected && state.module_type == "mediahost")
        .map(|(id, _)| id)
        .collect()
}

/// `derived-v2-<key>-ocr.json` → `derived-v2-<key>-ocr.failed`.
fn marker_name(answer: &std::path::Path) -> String {
    let name = answer.file_name().unwrap_or_default().to_string_lossy();
    match name.strip_suffix("ocr.json") {
        Some(stem) => format!("{stem}{FAILED_MARKER}"),
        None => format!("{name}.{FAILED_MARKER}"),
    }
}

/// What one batch of one kind turns into on the wire.
enum Items {
    Text(Vec<kahawai_proto::v1::SubsWorkItem>),
    Sets(Vec<kahawai_proto::v1::ImageSubsWorkItem>),
}

impl Items {
    fn len(&self) -> usize {
        match self {
            Self::Text(v) => v.len(),
            Self::Sets(v) => v.len(),
        }
    }

    /// Chunked: one message naming every cold file in a large collection
    /// is a needlessly large frame.
    fn messages(self, collection_id: &str) -> Vec<kahawai_proto::v1::HubToHost> {
        use kahawai_proto::v1::hub_to_host::Msg;
        match self {
            Self::Text(items) => items
                .chunks(5000)
                .map(|chunk| kahawai_proto::v1::HubToHost {
                    msg: Some(Msg::SubsWorklist(kahawai_proto::v1::SubsWorklist {
                        collection_id: collection_id.into(),
                        items: chunk.to_vec(),
                    })),
                })
                .collect(),
            Self::Sets(items) => items
                .chunks(5000)
                .map(|chunk| kahawai_proto::v1::HubToHost {
                    msg: Some(Msg::ImageSubsWorklist(
                        kahawai_proto::v1::ImageSubsWorklist {
                            collection_id: collection_id.into(),
                            items: chunk.to_vec(),
                        },
                    )),
                })
                .collect(),
        }
    }
}

impl Subtitles {
    /// Something changed what is missing or who can supply it.
    pub fn wake(&self) {
        self.wake.notify_waiters();
    }

    /// Start the prewarm driver, the reconnect listener and the OCR worker.
    pub fn start_work(self: &Arc<Self>, registry: Arc<Registry>, sessions: Arc<Sessions>) {
        {
            let subs = self.clone();
            let registry = registry.clone();
            queue::spawn("subtitles", self.wake.clone(), FALLBACK, move || {
                let subs = subs.clone();
                let registry = registry.clone();
                async move { subs.step(&registry).await }
            });
        }
        // Subscribed before spawning so a reconnect racing startup is not
        // missed; the driver's tick would catch it, an hour late.
        let mut events = registry.subscribe_events();
        {
            let subs = self.clone();
            let registry = registry.clone();
            tokio::spawn(async move {
                loop {
                    let released = match events.recv().await {
                        Ok(crate::registry::RegistryEvent::Satellite {
                            module_id,
                            connected: true,
                            ..
                        }) if connected_mediahosts(&registry).contains(&module_id) => {
                            subs.release_host(&registry, &module_id).await
                        }
                        Ok(_) => continue,
                        // At least one hint was dropped: reconcile against
                        // authoritative connection state rather than guess.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let mut total = 0;
                            for host in connected_mediahosts(&registry) {
                                total += subs.release_host(&registry, &host).await;
                            }
                            total
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    };
                    if released > 0 {
                        subs.wake();
                    }
                }
            });
        }
        #[cfg(feature = "ocr")]
        {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SourceFile>();
            *self.ocr.tx.lock().unwrap() = Some(tx);
            let subs = self.clone();
            tokio::spawn(async move {
                let mut idle = sessions.idle_watch();
                subs.ocr_seed(&registry).await;
                while let Some(file) = rx.recv().await {
                    subs.ocr.queued.fetch_sub(1, Ordering::Relaxed);
                    // Idle means idle: playback outranks this. A session
                    // ending flips the watch, so waiting here costs nothing.
                    while !*idle.borrow() {
                        if idle.changed().await.is_err() {
                            return;
                        }
                    }
                    subs.ocr.busy.store(1, Ordering::Relaxed);
                    let generated = subs.ocr_file(&registry, &file).await;
                    subs.ocr.busy.store(0, Ordering::Relaxed);
                    if generated > 0 {
                        tracing::info!(path = %file.path, generated, "OCR generated text");
                        tokio::time::sleep(OCR_PACE).await;
                    }
                }
            });
        }
        #[cfg(not(feature = "ocr"))]
        let _ = sessions;
    }

    async fn release_host(&self, registry: &Registry, module_id: &str) -> u64 {
        match registry.catalogue().release_subtitle_host(module_id).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(%module_id, rows = n, "mediahost reconnect released subtitle work");
                }
                n
            }
            Err(error) => {
                tracing::warn!(%module_id, error = format!("{error:#}"),
                    "releasing subtitle work on reconnect failed");
                0
            }
        }
    }

    /// One claimed batch of each kind per connected mediahost.
    pub(crate) async fn step(&self, registry: &Registry) -> Result<Step> {
        let store = registry.catalogue();
        let in_flight: Vec<String> = crate::workorder::in_flight(registry.db())
            .await
            .into_iter()
            .collect();
        let now = queue::now();
        let mut worked = false;
        for host in connected_mediahosts(registry) {
            for kind in SUBTITLE_KINDS {
                if kind == "sets" && !registry.host_supports_image_subs_worklists(&host) {
                    // An older host takes one request per track and would
                    // lose its link to a batch; its rows wait for an
                    // upgrade, visible as pending.
                    continue;
                }
                let jobs = store
                    .claim_subtitle_jobs(kind, &host, &in_flight, now, LEASE_SECS, BATCH)
                    .await?;
                if jobs.is_empty() {
                    continue;
                }
                worked = true;
                let mut by_collection: BTreeMap<String, Items> = Default::default();
                let (mut settled, mut offered) = (0usize, 0usize);
                for (rank, job) in jobs.iter().enumerate() {
                    let rank = rank.try_into().unwrap_or(u32::MAX);
                    let tracks = tracks(&job.file);
                    let batch = by_collection
                        .entry(job.file.remote_id.clone())
                        .or_insert_with(|| match kind {
                            "text" => Items::Text(vec![]),
                            _ => Items::Sets(vec![]),
                        });
                    let before = batch.len();
                    match batch {
                        Items::Text(items) => {
                            if let Some(mut item) = self.text_item(&tracks)? {
                                item.rank = rank;
                                items.push(item);
                            }
                        }
                        Items::Sets(items) => {
                            for mut item in self.sets_items(registry, &tracks).await {
                                item.rank = rank;
                                items.push(item);
                            }
                        }
                    }
                    if batch.len() == before {
                        settled += 1;
                        store.finish_subtitle_job(&job.file.file_id, kind).await?;
                    } else {
                        offered += 1;
                    }
                }
                'send: for (collection_id, items) in by_collection {
                    for msg in items.messages(&collection_id) {
                        if let Err(error) = registry.send_to_host(&host, msg).await {
                            tracing::warn!(module_id = %host, kind, error = format!("{error:#}"),
                                "subtitle worklist send failed; releasing the batch");
                            store.release_subtitle_host(&host).await?;
                            break 'send;
                        }
                    }
                }
                tracing::info!(module_id = %host, kind, offered, settled, "subtitle work claimed");
            }
        }
        if worked {
            return Ok(Step::Worked);
        }
        let mut next_due: Option<i64> = None;
        for kind in SUBTITLE_KINDS {
            if let Some(due) = store.subtitle_jobs_next_due(kind).await? {
                next_due = Some(next_due.map_or(due, |d| d.min(due)));
            }
        }
        Ok(Step::Idle { next_due })
    }

    /// The text worklist item for one file, or `None` when nothing on it
    /// needs the mediahost: no embedded text track, or every one already
    /// cached.
    fn text_item(
        &self,
        tracks: &[crate::tracks::Track],
    ) -> Result<Option<kahawai_proto::v1::SubsWorkItem>> {
        for track in tracks {
            // Sidecars are one small read on demand, with no container to
            // walk; image tracks are the `sets` kind's business.
            if track.origin != "embedded" || crate::tracks::is_image_format(&track.format) {
                continue;
            }
            let (Some(source), Ok(revision)) = (&track.physical, track.source_revision()) else {
                continue;
            };
            let cached = self
                .dir
                .join(format!(
                    "{}.json",
                    super::cache_key(
                        &source.module_id,
                        &source.collection_id,
                        &source.root_token,
                        &source.path_rel,
                        &track.internal_key(),
                        revision,
                    )
                ))
                .try_exists()?;
            if cached {
                continue;
            }
            // Every embedded track of one file shares the file's revision,
            // and the host extracts the whole container in one pass, so the
            // first cold track settles the file.
            return Ok(Some(kahawai_proto::v1::SubsWorkItem {
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: source.root_token.clone(),
                    path_rel: source.path_rel.clone(),
                }),
                source_revision: revision.to_string(),
                rank: 0,
            }));
        }
        Ok(None)
    }

    /// One worklist item per image track that still needs its display
    /// sets: no OCR answer, no failure marker, no sets on disk.
    async fn sets_items(
        &self,
        registry: &Registry,
        tracks: &[crate::tracks::Track],
    ) -> Vec<kahawai_proto::v1::ImageSubsWorkItem> {
        let mut items = vec![];
        for track in tracks {
            if !crate::tracks::is_image_format(&track.format) || self.ocr_answered(track) {
                continue;
            }
            let Ok((host, collection, root, rel, index, _)) =
                self.extract_ref(registry, track).await
            else {
                continue;
            };
            let Ok(revision) = track.source_revision() else {
                continue;
            };
            if self.image_sets_cached(&host, &collection, &root, &rel, index, revision) {
                continue;
            }
            items.push(kahawai_proto::v1::ImageSubsWorkItem {
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: root,
                    path_rel: rel,
                }),
                sub_index: index as u32,
                source_revision: revision.to_string(),
                rank: 0,
            });
        }
        items
    }

    /// An OCR answer or a failure marker: nothing more to ask for.
    fn ocr_answered(&self, track: &crate::tracks::Track) -> bool {
        let Ok(answer) = super::catalogue::path(&self.dir, track, "ocr.json") else {
            return true;
        };
        answer.try_exists().unwrap_or(true)
            || answer
                .with_file_name(marker_name(&answer))
                .try_exists()
                .unwrap_or(true)
    }

    /// Display sets landed for one track of this file: settle the `sets`
    /// row once nothing on the file is missing, and hand the file to OCR.
    pub(crate) async fn sets_landed(&self, registry: &Registry, file: SourceFile) -> Result<()> {
        if self.sets_items(registry, &tracks(&file)).await.is_empty() {
            registry
                .catalogue()
                .finish_subtitle_job(&file.file_id, "sets")
                .await?;
        }
        self.ocr_enqueue(file);
        Ok(())
    }

    /// OCR this file once nobody is watching. Before the worker is up
    /// (tests, `--no-default-features`) this is a no-op; the startup seed
    /// covers what landed meanwhile.
    pub(crate) fn ocr_enqueue(&self, file: SourceFile) {
        if let Some(tx) = self.ocr.tx.lock().unwrap().as_ref()
            && tx.send(file).is_ok()
        {
            self.ocr.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sets that landed while the hub was down: one pass over the
    /// catalogue at startup, filtered on the probe before any track is
    /// built, so it is a read of `files` and not a stat storm.
    #[cfg(feature = "ocr")]
    async fn ocr_seed(&self, registry: &Registry) {
        let store = registry.catalogue();
        let summaries = match store.collection_summaries().await {
            Ok(s) => s,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "OCR seed could not read collections"
                );
                return;
            }
        };
        let mut seeded = 0usize;
        for summary in summaries {
            let c = summary.collection;
            let files = match store.files(&c.id).await {
                Ok(f) => f,
                Err(error) => {
                    tracing::warn!(collection = %c.id, error = format!("{error:#}"),
                        "OCR seed could not read files");
                    continue;
                }
            };
            for f in files {
                let (Some(media), Some(size)) = (f.media, f.size) else {
                    continue;
                };
                if !has_image_tracks(&media) {
                    continue;
                }
                let file = SourceFile {
                    file_id: f.id,
                    host: c.mediahost_id.clone(),
                    collection_id: c.id.clone(),
                    remote_id: c.remote_id.clone(),
                    media_type: c.media_type,
                    root_token: f.root_token,
                    path: f.path,
                    size,
                    mtime: f.mtime.unwrap_or(0),
                    head_hash: f.head_hash.unwrap_or(0),
                    tail_hash: f.tail_hash.unwrap_or(0),
                    media,
                    item_id: f.item_id,
                };
                if self.ocr_pending(registry, &file).await {
                    seeded += 1;
                    self.ocr_enqueue(file);
                }
            }
        }
        tracing::info!(seeded, "OCR seeded from display sets already on disk");
    }

    /// Whether any image track of this file has sets on disk, a model to
    /// read them with, no answer and no failure marker.
    #[cfg(feature = "ocr")]
    async fn ocr_pending(&self, registry: &Registry, file: &SourceFile) -> bool {
        for track in tracks(file) {
            if !crate::tracks::is_image_format(&track.format) || self.ocr_answered(&track) {
                continue;
            }
            if let Ok((host, collection, root, rel, index, language)) =
                self.extract_ref(registry, &track).await
                && crate::ocr::model_for(language.as_deref()).is_some()
                && let Ok(revision) = track.source_revision()
                && self.image_sets_cached(&host, &collection, &root, &rel, index, revision)
            {
                return true;
            }
        }
        false
    }

    /// OCR every image track of one file whose sets are on disk. Returns
    /// how many answers were generated.
    #[cfg(feature = "ocr")]
    async fn ocr_file(&self, registry: &Registry, file: &SourceFile) -> usize {
        let mut generated = 0;
        for track in tracks(file) {
            if !crate::tracks::is_image_format(&track.format) || self.ocr_answered(&track) {
                continue;
            }
            let Ok((host, collection, root, rel, index, language)) =
                self.extract_ref(registry, &track).await
            else {
                continue;
            };
            // No Tesseract model for this language is not a failure of the
            // track: nothing to mark, and a rerun after installing one
            // picks it up.
            if crate::ocr::model_for(language.as_deref()).is_none() {
                continue;
            }
            // Only sets on disk are OCR's business; a track without them is
            // the `sets` row's, and its landing brings the file back here.
            if !track.source_revision().is_ok_and(|rev| {
                self.image_sets_cached(&host, &collection, &root, &rel, index, rev)
            }) {
                continue;
            }
            match self.catalogue_ocr(registry, &track).await {
                Ok(super::OcrGeneration::Generated) => generated += 1,
                // No text is an answer and it is now recorded.
                Ok(super::OcrGeneration::NoText) => {}
                Ok(super::OcrGeneration::RetryLater | super::OcrGeneration::RetryOnReconnect) => {}
                Err(error) => {
                    tracing::warn!(path = %file.path, track = track.id, error = format!("{error:#}"),
                        "OCR failed; marked, skipped until rerun");
                    if let Ok(answer) = super::catalogue::path(&self.dir, &track, "ocr.json")
                        && std::fs::write(
                            answer.with_file_name(marker_name(&answer)),
                            format!("{error:#}"),
                        )
                        .is_ok()
                    {
                        self.ocr.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if generated > 0 {
            self.ocr.done.fetch_add(generated, Ordering::Relaxed);
        }
        generated
    }

    /// Administrator rerun: forget every failure marker and seed again.
    #[cfg(feature = "ocr")]
    pub(crate) async fn ocr_rerun(&self, registry: &Registry) {
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().ends_with(FAILED_MARKER) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        self.ocr.failed.store(0, Ordering::Relaxed);
        self.ocr_seed(registry).await;
    }

    #[cfg(not(feature = "ocr"))]
    pub(crate) async fn ocr_rerun(&self, _registry: &Registry) {}

    pub(crate) fn ocr_counters(&self) -> &OcrState {
        &self.ocr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kahawai_proto::v1 as p;
    use prost::Message;

    async fn fixture(
        files: Vec<(&str, serde_json::Value)>,
    ) -> (tempfile::TempDir, Registry, Subtitles) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let store = crate::db::open_catalogue(dir.path()).await.unwrap();
        store.put_mediahost("host", "Fixture").await.unwrap();
        store
            .offer_collection(
                "host",
                &p::CatalogCollection {
                    id: "series".into(),
                    media_type: "series".into(),
                    epoch: "epoch".into(),
                    current_version: files.len() as u64,
                    roots: vec![p::CollectionRoot::new("root", "/fixture")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let records = files
            .iter()
            .enumerate()
            .map(|(i, (path, streams))| p::CatalogRecord {
                version: i as u64 + 1,
                kind: "file".into(),
                key: format!("root\0{path}").into_bytes(),
                payload: p::FileUpsert {
                    collection_id: "series".into(),
                    files: vec![p::FileRecord {
                        source: Some(p::SourcePath::new("root", *path)),
                        size: 10,
                        mtime_unix: 1,
                        streams_json: streams.to_string(),
                        ..Default::default()
                    }],
                }
                .encode_to_vec(),
                deleted: false,
            })
            .collect();
        store
            .apply_catalogue(
                "host",
                &p::CatalogDelta {
                    collection_id: "series".into(),
                    epoch: "epoch".into(),
                    snapshot: true,
                    done: true,
                    through_version: files.len() as u64,
                    records,
                },
            )
            .await
            .unwrap();
        let registry = Registry::new(db, Default::default(), store);
        let subs = Subtitles::new(dir.path().join("subtitles"));
        (dir, registry, subs)
    }

    async fn states(registry: &Registry, kind: &str) -> Vec<(String, i64)> {
        let mut v: Vec<_> = registry
            .catalogue()
            .subtitle_jobs_status()
            .await
            .unwrap()
            .into_iter()
            .filter(|s| s.kind == kind)
            .map(|s| (s.state, s.count))
            .collect();
        v.sort();
        v
    }

    fn drain(
        rx: &mut tokio::sync::mpsc::Receiver<Result<p::HubToHost, tonic::Status>>,
    ) -> Vec<p::hub_to_host::Msg> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|m| m.unwrap().msg.unwrap())
            .collect()
    }

    /// A file is named once however many text tracks it carries: the
    /// mediahost extracts the whole container in one pass. Image tracks go
    /// out as the `sets` kind's own worklist, and a file with nothing of a
    /// kind is settled without an entry.
    #[tokio::test]
    async fn a_claim_offers_each_kind_as_one_worklist_and_settles_the_rest() {
        let (_dir, registry, subs) = fixture(vec![
            (
                "Many.mkv",
                serde_json::json!({"container":"mkv","subtitles":[
                    {"format":"subrip","language":"eng"},
                    {"format":"ass","language":"nld"},
                    {"format":"subrip","language":"fra"}
                ]}),
            ),
            (
                "Image.mkv",
                serde_json::json!({"container":"mkv","subtitles":[{"format":"pgs","language":"eng"}]}),
            ),
        ])
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        registry.register_link("host", tx, kahawai_proto::PROTOCOL_MINOR, 0);
        registry.connected("host", "mediahost", "Fixture", "fp", "test");

        assert!(matches!(subs.step(&registry).await.unwrap(), Step::Worked));
        let sent = drain(&mut rx);
        assert_eq!(sent.len(), 2, "one worklist per kind: {sent:?}");
        let text = sent
            .iter()
            .find_map(|m| match m {
                p::hub_to_host::Msg::SubsWorklist(l) => Some(l.clone()),
                _ => None,
            })
            .expect("a text worklist");
        let sets = sent
            .iter()
            .find_map(|m| match m {
                p::hub_to_host::Msg::ImageSubsWorklist(l) => Some(l.clone()),
                _ => None,
            })
            .expect("a sets worklist");
        assert_eq!(
            text.items
                .iter()
                .map(|i| i.source.as_ref().unwrap().path_rel.as_str())
                .collect::<Vec<_>>(),
            ["Many.mkv"]
        );
        assert!(
            !text.items[0].source_revision.is_empty(),
            "the reply has to echo a revision"
        );
        assert_eq!(sets.items.len(), 1);
        assert_eq!(sets.items[0].source.as_ref().unwrap().path_rel, "Image.mkv");
        assert_eq!(sets.items[0].sub_index, 0);
        assert_eq!(
            states(&registry, "text").await,
            vec![("done".into(), 1), ("running".into(), 1)]
        );
        assert_eq!(
            states(&registry, "sets").await,
            vec![("done".into(), 1), ("running".into(), 1)]
        );

        // Nothing claimable while the offers are leased, and the driver
        // knows when that changes.
        let Step::Idle { next_due } = subs.step(&registry).await.unwrap() else {
            panic!("leased work is not claimed again");
        };
        assert!(next_due.is_some());

        // Landings settle each row through the address the host reports.
        for key in ["e0", "e1", "e2"] {
            subs.store_extracted(
                "host",
                "series",
                "root",
                "Many.mkv",
                key,
                &text.items[0].source_revision,
                &kahawai_media::subtitles::Extracted {
                    cues: vec![],
                    ass: None,
                },
            )
            .unwrap();
        }
        assert!(
            registry
                .catalogue()
                .finish_subtitle_source(
                    "host",
                    "series",
                    &p::SourcePath::new("root", "Many.mkv"),
                    "text"
                )
                .await
                .unwrap()
        );
        subs.store_image_sets(
            "host",
            &p::ImageSubtitles {
                collection_id: "series".into(),
                source: Some(p::SourcePath::new("root", "Image.mkv")),
                sub_index: 0,
                source_revision: sets.items[0].source_revision.clone(),
                codec: "S_HDMV/PGS".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let image = registry
            .catalogue()
            .source_file("host", "series", &p::SourcePath::new("root", "Image.mkv"))
            .await
            .unwrap()
            .unwrap();
        subs.sets_landed(&registry, image).await.unwrap();
        assert_eq!(states(&registry, "text").await, vec![("done".into(), 2)]);
        assert_eq!(states(&registry, "sets").await, vec![("done".into(), 2)]);

        // A rerun re-examines everything, but a fully cached file is never
        // offered again.
        for kind in SUBTITLE_KINDS {
            assert_eq!(
                registry
                    .catalogue()
                    .rerun_subtitle_jobs(kind)
                    .await
                    .unwrap(),
                2
            );
        }
        assert!(matches!(subs.step(&registry).await.unwrap(), Step::Worked));
        assert!(
            drain(&mut rx).is_empty(),
            "everything was cached, so nothing was sent"
        );
        assert_eq!(states(&registry, "text").await, vec![("done".into(), 2)]);
        assert_eq!(states(&registry, "sets").await, vec![("done".into(), 2)]);
    }

    #[tokio::test]
    async fn an_older_host_is_not_offered_sets_and_a_send_failure_releases_the_batch() {
        let (_dir, registry, subs) = fixture(vec![(
            "Film.mkv",
            serde_json::json!({"container":"mkv","subtitles":[
                {"format":"subrip","language":"eng"},{"format":"pgs","language":"eng"}]}),
        )])
        .await;
        // Protocol 4.3: text worklists yes, image worklists no.
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        registry.register_link("host", tx, 3, 0);
        registry.connected("host", "mediahost", "Fixture", "fp", "test");
        assert!(matches!(subs.step(&registry).await.unwrap(), Step::Worked));
        let sent = drain(&mut rx);
        assert!(
            matches!(sent.as_slice(), [p::hub_to_host::Msg::SubsWorklist(_)]),
            "{sent:?}"
        );
        assert_eq!(
            states(&registry, "sets").await,
            vec![("pending".into(), 1)],
            "waits for an upgrade"
        );
        assert_eq!(states(&registry, "text").await, vec![("running".into(), 1)]);

        // The link is full: the offer cannot be delivered, so the batch
        // must not stay leased for an hour.
        assert_eq!(
            registry
                .catalogue()
                .release_subtitle_host("host")
                .await
                .unwrap(),
            1
        );
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        registry.register_link("host", tx, kahawai_proto::PROTOCOL_MINOR, 0);
        drop(rx);
        assert!(matches!(subs.step(&registry).await.unwrap(), Step::Worked));
        assert_eq!(states(&registry, "text").await, vec![("pending".into(), 1)]);
    }

    /// OCR is driven by sets on disk: a file whose sets are cached is OCR'd
    /// and answered; one whose sets are not is left to the landing. A
    /// failure leaves a marker that a rerun clears.
    #[cfg(feature = "ocr")]
    #[tokio::test]
    async fn ocr_answers_cached_sets_and_marks_failures_until_rerun() {
        let pgs = || serde_json::json!({"container":"mkv","subtitles":[{"format":"pgs","language":"eng"}]});
        let (_dir, registry, subs) = fixture(vec![("A.mkv", pgs()), ("B.mkv", pgs())]).await;
        registry.connected("host", "mediahost", "Fixture", "fp", "test");
        let file = |path: &'static str| {
            let registry = &registry;
            async move {
                registry
                    .catalogue()
                    .source_file("host", "series", &p::SourcePath::new("root", path))
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let a = file("A.mkv").await;
        let track = tracks(&a).remove(0);
        subs.store_image_sets(
            "host",
            &p::ImageSubtitles {
                collection_id: "series".into(),
                source: Some(p::SourcePath::new("root", "A.mkv")),
                source_revision: track.source_revision().unwrap().into(),
                codec: "S_HDMV/PGS".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(subs.ocr_pending(&registry, &a).await);
        assert!(
            !subs.ocr_pending(&registry, &file("B.mkv").await).await,
            "no sets, nothing to do"
        );

        // Empty sets are an answer: no text, recorded.
        assert_eq!(subs.ocr_file(&registry, &a).await, 0);
        assert!(!subs.ocr_pending(&registry, &a).await, "answered");
        let answer = super::super::catalogue::path(subs.cache_dir(), &track, "ocr.json").unwrap();
        assert!(answer.exists());

        // A marker beside the answer's place keeps a failed track out until
        // a rerun forgets it.
        std::fs::remove_file(&answer).unwrap();
        let marker = answer.with_file_name(marker_name(&answer));
        std::fs::write(&marker, "boom").unwrap();
        assert!(!subs.ocr_pending(&registry, &a).await);
        subs.ocr_rerun(&registry).await;
        assert!(!marker.exists());
        assert!(subs.ocr_pending(&registry, &a).await);
    }

    #[test]
    fn a_marker_sits_beside_the_answer_it_replaces() {
        assert_eq!(
            marker_name(std::path::Path::new("/c/derived-v2-abc-ocr.json")),
            "derived-v2-abc-ocr.failed"
        );
    }
}
