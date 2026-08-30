//! Mediahost module: enroll once, then keep a control link to the hub
//! (AR-3: always dials out) and scan collections up to it.

pub mod catalog;
pub mod ed2k;
pub mod hasher;
pub mod loudness;
pub mod scan;
pub mod scheduler;
pub mod segments;
pub mod serve;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{
    AnnounceCollection, CatalogDelta, CatalogOffer, Heartbeat, Hello, HostToHub, HubToHost,
    host_to_hub, hub_to_host,
};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use prost::Message as _;
use scan::CollectionConfig;
use tokio_stream::wrappers::ReceiverStream;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const RECONNECT_DELAY: Duration = Duration::from_millis(10);
const LINK_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Full-file/season GStreamer jobs create short-lived decoder threads whose
/// glibc arenas can retain hundreds of MiB after every pipeline has reached
/// NULL and dropped. The allocation is no longer live; ask glibc to return
/// whole free pages at that lifecycle boundary. `malloc_trim` is MT-safe and,
/// since glibc 2.8, covers every arena.
/// Source: https://man7.org/linux/man-pages/man3/malloc_trim.3.html
pub fn release_background_memory(job: &'static str) -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let released = {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: malloc_trim(3) accepts every size_t, defines zero as retaining
        // at most one top page, and is documented MT-Safe.
        unsafe { malloc_trim(0) != 0 }
    };
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let released = false;

    if released {
        tracing::debug!(job, "released free background-job memory");
    }
    released
}

#[derive(Clone)]
pub(crate) struct BackgroundMemoryTrimmer {
    next: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    interval: std::time::Duration,
}

impl BackgroundMemoryTrimmer {
    pub(crate) fn every(interval: std::time::Duration) -> Self {
        Self {
            next: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now() + interval)),
            interval,
        }
    }

    /// Streaming decoders can fill freed arenas long before a multi-hour file
    /// reaches its lifecycle boundary. Only one callback wins each interval;
    /// the others avoid even waiting on this bookkeeping lock.
    pub(crate) fn checkpoint(&self, job: &'static str) {
        let Ok(mut next) = self.next.try_lock() else {
            return;
        };
        let now = std::time::Instant::now();
        if now < *next {
            return;
        }
        *next = now + self.interval;
        drop(next);
        release_background_memory(job);
    }
}

/// One independently enrolled hub. `legacy_identity` preserves the original
/// single-hub credentials directly in the mediahost state directory.
#[derive(Debug, Clone)]
pub struct HubTarget {
    pub id: String,
    pub address: String,
    pub collections: Vec<String>,
    pub legacy_identity: bool,
}

/// Recreates the in-process transport after either side rejects a message.
/// The local adapter follows the same fail-and-replay lifecycle as an mTLS
/// link; keeping the factory here lets one `LocalRuntime` survive reconnects.
pub type LocalLinkFactory = std::sync::Arc<
    dyn Fn() -> (
            tokio::sync::mpsc::Sender<HostToHub>,
            tokio::sync::mpsc::Receiver<Result<HubToHost, tonic::Status>>,
        ) + Send
        + Sync,
>;

type CatalogVersionState = std::collections::HashMap<String, (String, u64, bool)>;

/// The process-local half of protocol 4. It owns every filesystem watcher and
/// scan task; links only request work through these coalescing triggers.
#[derive(Clone)]
struct LocalRuntime {
    catalog: catalog::Catalog,
    collections: Vec<CollectionConfig>,
    triggers: std::collections::HashMap<String, TriggerSink>,
    scheduler: scheduler::Scheduler,
    discovery_wake: std::sync::Arc<tokio::sync::Notify>,
    catalog_versions: std::sync::Arc<std::sync::RwLock<CatalogVersionState>>,
    discovery_status: std::sync::Arc<
        std::sync::RwLock<std::collections::HashMap<String, kahawai_proto::v1::DiscoveryStatus>>,
    >,
    _guards: std::sync::Arc<AbortOnDrop>,
}

impl LocalRuntime {
    async fn start(
        state_dir: &Path,
        collections: Vec<CollectionConfig>,
        rescan_minutes: u64,
        scheduler_config: kahawai_core::media::MediahostSchedulerConfig,
        detect_segments: bool,
    ) -> Result<Self> {
        let scheduler = scheduler::Scheduler::new(&collections, &scheduler_config)?;
        Self::start_with_scheduler(
            state_dir,
            collections,
            rescan_minutes,
            scheduler,
            detect_segments,
        )
        .await
    }

    async fn start_with_scheduler(
        state_dir: &Path,
        collections: Vec<CollectionConfig>,
        rescan_minutes: u64,
        scheduler: scheduler::Scheduler,
        detect_segments: bool,
    ) -> Result<Self> {
        let catalog =
            catalog::Catalog::open_with_segment_detection(state_dir, &collections, detect_segments)
                .await?;
        let mut triggers = std::collections::HashMap::new();
        let mut guards = Vec::new();
        let catalog_versions = std::sync::Arc::new(std::sync::RwLock::new(Default::default()));
        let version_catalog = catalog.clone();
        let version_state = catalog_versions.clone();
        guards.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                match version_catalog.version_states().await {
                    Ok(versions) => {
                        *version_state.write().unwrap() = versions;
                    }
                    Err(error) => tracing::warn!(
                        error = format!("{error:#}"),
                        "reading local catalogue versions failed"
                    ),
                }
            }
        }));
        let discovery_status: std::sync::Arc<
            std::sync::RwLock<
                std::collections::HashMap<String, kahawai_proto::v1::DiscoveryStatus>,
            >,
        > = std::sync::Arc::new(std::sync::RwLock::new(Default::default()));
        let status_catalog = catalog.clone();
        let status_collections = collections.clone();
        let status_state = discovery_status.clone();
        guards.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                for collection in &status_collections {
                    match status_catalog.discovery_status(&collection.name).await {
                        Ok(mut status) => {
                            if !detect_segments {
                                status.pending_segments = 0;
                            }
                            status_state
                                .write()
                                .unwrap()
                                .insert(collection.name.clone(), status);
                        }
                        Err(error) => tracing::warn!(
                            collection = %collection.name,
                            error = format!("{error:#}"),
                            "reading local discovery status failed"
                        ),
                    }
                }
            }
        }));
        for collection in &collections {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ScanTrigger>(8);
            let sink = TriggerSink {
                tx,
                overflow: Default::default(),
            };
            triggers.insert(collection.name.clone(), sink.clone());
            let collection = collection.clone();
            let catalog = catalog.clone();
            let scan_scheduler = scheduler.clone();
            let overflow = sink.overflow.clone();
            guards.push(tokio::spawn(async move {
                while let Some(mut trigger) = rx.recv().await {
                    while let Ok(more) = rx.try_recv() {
                        trigger.force_dirs.extend(more.force_dirs);
                        trigger.demand |= more.demand;
                    }
                    if let Some(more) = overflow.lock().unwrap().take() {
                        trigger.force_dirs.extend(more.force_dirs);
                        trigger.demand |= more.demand;
                    }
                    let root_tokens = collection
                        .resolved_roots()
                        .map(|root| root.token)
                        .collect::<Vec<_>>();
                    let resources =
                        scan_scheduler.resources(root_tokens.iter().map(String::as_str), true);
                    let priority = if trigger.demand {
                        scheduler::Priority::Demand
                    } else {
                        scheduler::Priority::CatalogFreshness
                    };
                    let permit = match scan_scheduler
                        .acquire(
                            priority,
                            resources,
                            None,
                            format!("catalog scan {}", collection.name),
                        )
                        .await
                    {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    if let Err(error) = scan::scan_local_collection(
                        collection.clone(),
                        catalog.clone(),
                        trigger.force_dirs,
                        permit,
                    )
                    .await
                    {
                        tracing::warn!(collection = %collection.name,
                            error = format!("{error:#}"), "local catalogue scan failed");
                    }
                }
            }));
            sink.send(ScanTrigger {
                initial: true,
                ..Default::default()
            });
        }

        if rescan_minutes > 0 {
            let periodic = triggers.clone();
            guards.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(rescan_minutes * 60));
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    for trigger in periodic.values() {
                        trigger.send(ScanTrigger::default());
                    }
                }
            }));
        }

        // Watch once for the whole process. Link churn never installs another
        // recursive watch or repeats its potentially expensive mount walk.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            use notify::event::{EventKind, ModifyKind};
            if !matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
            ) {
                return;
            }
            for path in event.paths {
                let _ = event_tx.send(path);
            }
        }) {
            Ok(watcher) => {
                let routes: Vec<(String, std::path::PathBuf)> = collections
                    .iter()
                    .flat_map(|collection| {
                        collection
                            .roots
                            .iter()
                            .map(|root| (collection.name.clone(), root.clone()))
                    })
                    .collect();
                let watch_tokens = collections
                    .iter()
                    .flat_map(CollectionConfig::resolved_roots)
                    .map(|root| root.token)
                    .collect::<Vec<_>>();
                let watch_scheduler = scheduler.clone();
                let watch_roots = routes.clone();
                let watch_triggers = triggers.clone();
                guards.push(tokio::spawn(async move {
                    // Installing recursive watches walks the mount. Keep that
                    // network-filesystem latency away from catalogue startup
                    // and every independently supervised hub link.
                    let permit = match watch_scheduler
                        .acquire(
                            scheduler::Priority::CatalogFreshness,
                            watch_scheduler.resources(
                                watch_tokens.iter().map(String::as_str),
                                false,
                            ),
                            None,
                            "filesystem watch installation",
                        )
                        .await
                    {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let watcher = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        use notify::Watcher as _;
                        let mut watcher = watcher;
                        for (_, root) in &watch_roots {
                            if let Err(error) =
                                watcher.watch(root, notify::RecursiveMode::Recursive)
                            {
                                tracing::warn!(root = %root.display(), %error,
                                    "watch failed; periodic scan still covers this root");
                            }
                        }
                        tracing::info!(roots = watch_roots.len(), "filesystem watches installed");
                        watcher
                    })
                    .await;
                    let Ok(watcher) = watcher else { return };
                    let _watcher = watcher;
                    let mut dirty: std::collections::HashMap<
                        String,
                        (
                            std::collections::HashSet<std::path::PathBuf>,
                            tokio::time::Instant,
                        ),
                    > = Default::default();
                    let mut tick = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            event = event_rx.recv() => {
                                let Some(path) = event else { return };
                                let directory = path.parent().unwrap_or(&path).to_path_buf();
                                for (collection, _) in routes.iter().filter(|(_, root)| path.starts_with(root)) {
                                    let entry = dirty.entry(collection.clone()).or_insert_with(|| {
                                        (Default::default(), tokio::time::Instant::now())
                                    });
                                    entry.0.insert(directory.clone());
                                    entry.1 = tokio::time::Instant::now();
                                }
                            }
                            _ = tick.tick() => {
                                let quiet = Duration::from_secs(3);
                                let ready: Vec<String> = dirty.iter()
                                    .filter(|(_, (_, changed))| changed.elapsed() >= quiet)
                                    .map(|(collection, _)| collection.clone())
                                    .collect();
                                for collection in ready {
                                    if let Some((force_dirs, _)) = dirty.remove(&collection)
                                        && let Some(trigger) = watch_triggers.get(&collection)
                                    {
                                        tracing::info!(%collection, dirs = force_dirs.len(),
                                            "watcher triggered rescan");
                                        trigger.send(ScanTrigger {
                                            force_dirs,
                                            initial: false,
                                            demand: false,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }));
            }
            Err(error) => tracing::warn!(%error, "no filesystem watcher; using periodic scans"),
        }

        // Source-owned discovery workers publish into the catalogue, never to
        // a particular hub. Every link later observes the same versioned fact.
        let (fact_tx, mut fact_rx) = tokio::sync::mpsc::channel::<HostToHub>(32);
        let (local_hash_tx, local_hash_rx) = tokio::sync::mpsc::channel(32);
        let (retry_tx, mut retry_rx) = tokio::sync::mpsc::unbounded_channel();
        guards.push(tokio::spawn(hasher::run(
            local_hash_rx,
            fact_tx.clone(),
            collections.clone(),
            scheduler.clone(),
            None,
            Some(retry_tx),
        )));
        let (local_loudness_tx, local_loudness_rx) = tokio::sync::mpsc::unbounded_channel();
        guards.push(tokio::spawn(loudness::run(
            local_loudness_rx,
            fact_tx,
            collections.clone(),
            scheduler.clone(),
            None,
        )));
        let segment_inflight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fact_catalog = catalog.clone();
        guards.push(tokio::spawn(async move {
            while let Some(message) = fact_rx.recv().await {
                loop {
                    match fact_catalog.store_fact(message.clone()).await {
                        Ok(()) => break,
                        Err(error) if catalog::is_stale_fact(&error) => {
                            tracing::warn!(
                                error = format!("{error:#}"),
                                "discarding stale local discovery result"
                            );
                            break;
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = format!("{error:#}"),
                                "persisting local discovery result failed; retrying"
                            );
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        }));
        let retry_catalog = catalog.clone();
        guards.push(tokio::spawn(async move {
            while let Some(retry) = retry_rx.recv().await {
                if let Err(error) = retry_catalog
                    .release_claims(
                        &retry.collection_id,
                        retry.kind,
                        std::slice::from_ref(&retry.source),
                    )
                    .await
                {
                    tracing::warn!(collection = %retry.collection_id, kind = retry.kind,
                        error = format!("{error:#}"),
                        "releasing retryable local discovery claim failed");
                }
            }
        }));
        let local_segment_tx = if detect_segments {
            let (segment_tx, segment_rx) = tokio::sync::mpsc::unbounded_channel();
            let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<HostToHub>(32);
            let catalog_tx = catalog.clone();
            let completed = segment_inflight.clone();
            guards.push(tokio::spawn(async move {
                while let Some(message) = result_rx.recv().await {
                    let done = matches!(
                        message.msg,
                        Some(host_to_hub::Msg::SegmentDetectionResult(_))
                    );
                    loop {
                        match catalog_tx.store_fact(message.clone()).await {
                            Ok(()) => break,
                            Err(error) if catalog::is_stale_fact(&error) => {
                                tracing::warn!(
                                    error = format!("{error:#}"),
                                    "discarding stale local segment result"
                                );
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    error = format!("{error:#}"),
                                    "persisting local segment result failed; retrying"
                                );
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                    if done {
                        completed.store(false, std::sync::atomic::Ordering::Release);
                    }
                }
            }));
            guards.push(tokio::spawn(segments::run(
                segment_rx,
                result_tx,
                collections.clone(),
                scheduler.clone(),
                None,
            )));
            Some(segment_tx)
        } else {
            None
        };
        let schedule_catalog = catalog.clone();
        let schedule_collections = collections.clone();
        let schedule_segment = segment_inflight;
        let discovery_wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let scheduler_wake = discovery_wake.clone();
        guards.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            let mut segment_exhausted = std::collections::HashMap::<String, i64>::new();
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = scheduler_wake.notified() => {}
                }
                for collection in &schedule_collections {
                    if collection.media_type == "anime" {
                        match schedule_catalog
                            .claim_sources(&collection.name, "file_hashes", 256)
                            .await
                        {
                            Ok(sources) if !sources.is_empty() => {
                                let job = hasher::JobMsg::Hashlist(kahawai_proto::v1::Hashlist {
                                    collection_id: collection.name.clone(),
                                    sources: sources.clone(),
                                });
                                if local_hash_tx.send(job).await.is_err() {
                                    if let Err(error) = schedule_catalog.release_claims(
                                        &collection.name, "file_hashes", &sources
                                    ).await {
                                        tracing::warn!(error = format!("{error:#}"),
                                            "releasing abandoned hash claims failed");
                                    }
                                    tracing::error!("local hash worker stopped; discovery scheduler exiting");
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => tracing::warn!(collection = %collection.name,
                                error = format!("{error:#}"), "claiming local hash work failed"),
                        }
                    }
                    for (kind, limit) in [
                        ("file_attachments", 64),
                        ("file_keyframe", 64),
                        ("file_geometry", 8),
                    ] {
                        match schedule_catalog
                            .claim_cheap_sources(&collection.name, kind, limit)
                            .await
                        {
                            Ok(sources) if !sources.is_empty() => {
                                let job = match kind {
                                    "file_attachments" => hasher::JobMsg::AttachmentsWorklist(
                                        kahawai_proto::v1::AttachmentsWorklist {
                                            collection_id: collection.name.clone(),
                                            sources: sources.clone(),
                                        },
                                    ),
                                    "file_keyframe" => hasher::JobMsg::KeyframeWorklist(
                                        kahawai_proto::v1::KeyframeWorklist {
                                            collection_id: collection.name.clone(),
                                            sources: sources.clone(),
                                        },
                                    ),
                                    "file_geometry" => hasher::JobMsg::VideoGeometryWorklist(
                                        kahawai_proto::v1::VideoGeometryWorklist {
                                            collection_id: collection.name.clone(),
                                            sources: sources.clone(),
                                        },
                                    ),
                                    _ => unreachable!("fixed cheap discovery kind"),
                                };
                                if local_hash_tx.send(job).await.is_err() {
                                    if let Err(error) = schedule_catalog
                                        .release_claims(&collection.name, kind, &sources)
                                        .await
                                    {
                                        tracing::warn!(error = format!("{error:#}"),
                                            "releasing abandoned cheap discovery claims failed");
                                    }
                                    tracing::error!(kind,
                                        "local discovery worker stopped; scheduler exiting");
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => tracing::warn!(collection = %collection.name, kind,
                                error = format!("{error:#}"),
                                "claiming local cheap discovery work failed"),
                        }
                    }
                    if collection.media_type != "music" {
                        match schedule_catalog
                            .claim_sources(&collection.name, "file_loudness", 128)
                            .await
                        {
                            Ok(sources) if !sources.is_empty() => {
                                let work = kahawai_proto::v1::LoudnessWorklist {
                                    collection_id: collection.name.clone(),
                                    analyzer: kahawai_media::loudness::ANALYZER,
                                    sources: sources.clone(),
                                };
                                if local_loudness_tx.send(work).is_err() {
                                    if let Err(error) = schedule_catalog.release_claims(
                                        &collection.name, "file_loudness", &sources
                                    ).await {
                                        tracing::warn!(error = format!("{error:#}"),
                                            "releasing abandoned loudness claims failed");
                                    }
                                    tracing::error!("local loudness worker stopped; discovery scheduler exiting");
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(error) => tracing::warn!(collection = %collection.name,
                                error = format!("{error:#}"), "claiming local loudness work failed"),
                        }
                    }
                    if matches!(collection.media_type.as_str(), "series" | "anime")
                        && let Some(segment_tx) = &local_segment_tx
                        && !schedule_segment.load(std::sync::atomic::Ordering::Acquire)
                    {
                        let (marker, scanning) = match schedule_catalog
                            .segment_scan_state(&collection.name)
                            .await
                        {
                            Ok(state) => state,
                            Err(error) => {
                                tracing::warn!(collection = %collection.name,
                                    error = format!("{error:#}"),
                                    "reading segment scan generation failed");
                                continue;
                            }
                        };
                        if !scanning && segment_exhausted.get(&collection.name) == Some(&marker) {
                            continue;
                        }
                        match schedule_catalog
                            .pending_sources(&collection.name, "file_segments", 1)
                            .await
                        {
                            Ok(pending) if !pending.is_empty() => {
                                match schedule_catalog
                                    .next_segment_job(&collection.name, &collection.media_type)
                                    .await
                                {
                                    Ok(Some(job))
                                        if schedule_segment
                                            .compare_exchange(
                                                false,
                                                true,
                                                std::sync::atomic::Ordering::AcqRel,
                                                std::sync::atomic::Ordering::Acquire,
                                            )
                                            .is_ok() =>
                                    {
                                        segment_exhausted.remove(&collection.name);
                                        if segment_tx.send(job).is_err() {
                                            schedule_segment.store(
                                                false,
                                                std::sync::atomic::Ordering::Release,
                                            );
                                            tracing::error!(
                                                "local segment worker stopped; segment discovery disabled"
                                            );
                                        }
                                    }
                                    Ok(None) => {
                                        if !scanning {
                                            segment_exhausted
                                                .insert(collection.name.clone(), marker);
                                        }
                                    }
                                    Ok(Some(_)) => {}
                                    Err(error) => tracing::warn!(collection = %collection.name,
                                        error = format!("{error:#}"),
                                        "selecting local segment work failed"),
                                }
                            }
                            Ok(_) => {
                                if !scanning {
                                    segment_exhausted.insert(collection.name.clone(), marker);
                                }
                            }
                            Err(error) => tracing::warn!(collection = %collection.name,
                                error = format!("{error:#}"),
                                "checking local segment work failed"),
                        }
                    }
                }
            }
        }));

        Ok(Self {
            catalog,
            collections,
            triggers,
            scheduler,
            discovery_wake,
            catalog_versions,
            discovery_status,
            _guards: std::sync::Arc::new(AbortOnDrop(guards)),
        })
    }

    fn selected(&self, names: &[String]) -> Vec<CollectionConfig> {
        self.collections
            .iter()
            .filter(|collection| names.contains(&collection.name))
            .cloned()
            .collect()
    }

    fn rescan(&self, collection: &str) {
        for (name, trigger) in &self.triggers {
            if collection.is_empty() || name == collection {
                trigger.send(ScanTrigger {
                    demand: true,
                    ..Default::default()
                });
            }
        }
    }
}

async fn ready_catalog_offer(
    runtime: &LocalRuntime,
    selected: &[CollectionConfig],
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
) -> Result<CatalogOffer> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // Consume Tokio's immediate first tick. Hello itself establishes
    // liveness; subsequent ticks keep a genuinely long first scan alive.
    heartbeat.tick().await;
    loop {
        let ready = {
            let versions = runtime.catalog_versions.read().unwrap();
            selected.iter().all(|collection| {
                versions
                    .get(&collection.name)
                    .is_some_and(|(_, _, complete)| *complete)
            })
        };
        if ready {
            return Ok(CatalogOffer {
                collections: runtime.catalog.offers(selected).await?,
            });
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = heartbeat.tick() => {
                send_link_message(tx, HostToHub {
                    msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})),
                }).await?;
            }
        }
    }
}

fn ensure_scoped_rescan(collections: &[CollectionConfig], requested: &str) -> Result<()> {
    anyhow::ensure!(
        !requested.is_empty(),
        "hub requested an unscoped collection scan"
    );
    anyhow::ensure!(
        collections
            .iter()
            .any(|collection| collection.name == requested),
        "hub requested an unshared collection scan"
    );
    Ok(())
}

/// Protocol-4 standalone entrypoint: one local source engine, independently
/// supervised outbound links. A pending first-time enrollment cannot prevent
/// an already enrolled hub from receiving updates.
pub async fn run_multi(
    state_dir: &Path,
    name: &str,
    collections: Vec<CollectionConfig>,
    rescan_minutes: u64,
    scheduler_config: kahawai_core::media::MediahostSchedulerConfig,
    hubs: Vec<HubTarget>,
    detect_segments: bool,
) -> Result<()> {
    let runtime = LocalRuntime::start(
        state_dir,
        collections,
        rescan_minutes,
        scheduler_config,
        detect_segments,
    )
    .await?;
    supervise_hubs(runtime, state_dir, name, hubs).await
}

async fn supervise_hubs(
    runtime: LocalRuntime,
    state_dir: &Path,
    name: &str,
    hubs: Vec<HubTarget>,
) -> Result<()> {
    let mut links = tokio::task::JoinSet::new();
    for hub in hubs {
        let runtime = runtime.clone();
        let state_dir = state_dir.to_path_buf();
        let name = name.to_string();
        links.spawn(async move {
            let identity_dir = if hub.legacy_identity {
                state_dir
            } else {
                state_dir.join("hubs").join(&hub.id)
            };
            loop {
                if let Err(error) = run_hub(
                    runtime.clone(),
                    hub.clone(),
                    identity_dir.clone(),
                    name.clone(),
                )
                .await
                {
                    tracing::warn!(hub = %hub.id, error = format!("{error:#}"),
                        "hub supervisor failed; restarting independently");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
        });
    }
    match links.join_next().await {
        Some(Ok(())) => anyhow::bail!("mediahost hub supervisor exited unexpectedly"),
        Some(Err(error)) => anyhow::bail!("mediahost hub supervisor panicked: {error}"),
        None => anyhow::bail!("mediahost has no hub supervisors"),
    }
}

async fn run_hub(
    runtime: LocalRuntime,
    hub: HubTarget,
    identity_dir: std::path::PathBuf,
    name: String,
) -> Result<()> {
    loop {
        let mut identity = match kahawai_transport::enroll::ensure_identity(
            &hub.address,
            &identity_dir,
            "mediahost",
            &name,
        )
        .await
        {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(hub = %hub.id, error = format!("{error:#}"),
                    "hub enrollment unavailable; retrying independently");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        match kahawai_transport::renew::maybe_renew(&hub.address, &identity_dir, "mediahost", &name)
            .await
        {
            Ok(Some(renewed)) => identity = renewed,
            Ok(None) => {}
            Err(error) => tracing::warn!(hub = %hub.id, error = format!("{error:#}"),
                "certificate renewal failed; retrying later"),
        }
        let tls = kahawai_transport::mtls::mtls_client_config(&identity)?;
        let renewal_due = kahawai_transport::renew::seconds_until_renewal_due(&identity.cert_pem)
            .unwrap_or(i64::MAX)
            .max(3600) as u64;
        tokio::select! {
            result = link_once_v4(&runtime, &hub, tls, &name) => match result {
                Ok(()) => tracing::warn!(hub = %hub.id, "hub closed the link; reconnecting"),
                Err(error) => tracing::warn!(hub = %hub.id, error = format!("{error:#}"),
                    "hub link failed; reconnecting"),
            },
            _ = tokio::time::sleep(Duration::from_secs(renewal_due)) => {
                tracing::info!(hub = %hub.id, "certificate renewal due; cycling the link");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn link_once_v4(
    runtime: &LocalRuntime,
    hub: &HubTarget,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
) -> Result<()> {
    let scheduler_owner = format!("hub:{}", hub.id);
    let _scheduler_owner = SchedulerOwnerGuard {
        scheduler: runtime.scheduler.clone(),
        owner: scheduler_owner.clone(),
    };
    let channel = kahawai_transport::tls::grpc_channel_with(&hub.address, tls.clone()).await?;
    let byte_channel = kahawai_transport::tls::grpc_channel_with(&hub.address, tls).await?;
    let mut client = MediahostLinkClient::new(channel)
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024);
    let (tx, rx) = tokio::sync::mpsc::channel::<HostToHub>(32);
    send_link_message(
        &tx,
        HostToHub {
            msg: Some(host_to_hub::Msg::Hello(Hello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                name: name.to_string(),
                build: kahawai_core::build_stamp().into(),
                segment_detector_generation: kahawai_core::segments::DETECTOR_GENERATION,
            })),
        },
    )
    .await?;
    let mut inbound = client
        .link(ReceiverStream::new(rx))
        .await
        .context("opening mediahost link")?
        .into_inner();
    match inbound.message().await.context("awaiting HelloAck")? {
        Some(HubToHost {
            msg: Some(hub_to_host::Msg::HelloAck(ack)),
        }) if ack.protocol_major == PROTOCOL_MAJOR => {}
        Some(HubToHost {
            msg: Some(hub_to_host::Msg::HelloAck(ack)),
        }) => anyhow::bail!(
            "incompatible hub protocol {}.{}; this mediahost requires {}.{}",
            ack.protocol_major,
            ack.protocol_minor,
            PROTOCOL_MAJOR,
            PROTOCOL_MINOR
        ),
        _ => anyhow::bail!("hub did not open with HelloAck"),
    }

    let selected = runtime.selected(&hub.collections);
    send_link_message(
        &tx,
        HostToHub {
            msg: Some(host_to_hub::Msg::CatalogOffer(
                ready_catalog_offer(runtime, &selected, &tx).await?,
            )),
        },
    )
    .await?;

    // Extraction remains requester-owned. This worker receives only subtitle
    // jobs on protocol 4; all catalogue discovery has one process-local owner.
    let (job_tx, job_rx) = tokio::sync::mpsc::channel(32);
    let _job_guard = AbortOnDrop(vec![tokio::spawn(hasher::run(
        job_rx,
        tx.clone(),
        selected.clone(),
        runtime.scheduler.clone(),
        Some(scheduler_owner.clone()),
        None,
    ))]);
    let sent: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, (String, u64)>>> =
        Default::default();
    let mut syncing = std::collections::HashSet::new();
    let mut syncs = tokio::task::JoinSet::new();
    let link_syncs = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut changes = tokio::time::interval(Duration::from_secs(1));
    let mut status = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            completed = syncs.join_next(), if !syncs.is_empty() => {
                let (collection, result) = completed
                    .context("catalogue sync task disappeared")?
                    .context("catalogue sync task panicked")?;
                syncing.remove(&collection);
                let (epoch, through) = result?;
                sent.lock().await.insert(collection, (epoch, through));
            }
            _ = heartbeat.tick() => {
                send_link_message(&tx, HostToHub {
                    msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})),
                }).await?;
            }
            _ = changes.tick() => {
                let states = sent.lock().await.clone();
                let advertised = runtime.catalog_versions.read().unwrap().clone();
                for collection in &selected {
                    let Some((epoch, cursor)) = states.get(&collection.name) else { continue };
                    if !advertised.get(&collection.name).is_some_and(
                        |(current_epoch, current, _)| current_epoch == epoch && current > cursor
                    ) {
                        continue;
                    }
                    if !syncing.insert(collection.name.clone()) {
                        continue;
                    }
                    let catalog = runtime.catalog.clone();
                    let outbound = tx.clone();
                    let link_limit = link_syncs.clone();
                    let collection_id = collection.name.clone();
                    let cursor = *cursor;
                    let expected_epoch = epoch.clone();
                    syncs.spawn(async move {
                        let result = send_catalog_pages(
                            &catalog, &outbound, &collection_id, cursor, false, link_limit
                        ).await.and_then(|(epoch, through)| {
                            anyhow::ensure!(epoch == expected_epoch,
                                "catalogue epoch changed under a live link");
                            Ok((epoch, through))
                        });
                        (collection_id, result)
                    });
                }
            }
            _ = status.tick() => {
                let statuses = runtime.discovery_status.read().unwrap().clone();
                for collection in &selected {
                    if let Some(status) = statuses.get(&collection.name) {
                        send_link_message(&tx, HostToHub {
                            msg: Some(host_to_hub::Msg::DiscoveryStatus(status.clone())),
                        }).await?;
                    }
                }
            }
            message = inbound.message() => match message? {
                Some(HubToHost { msg: Some(hub_to_host::Msg::CatalogCursor(cursor)) }) => {
                    let config = selected.iter()
                        .find(|collection| collection.name == cursor.collection_id)
                        .with_context(|| format!("hub requested unshared collection {:?}", cursor.collection_id))?;
                    let offer = runtime.catalog.offers(std::slice::from_ref(config)).await?
                        .remove(0);
                    let snapshot = cursor.snapshot
                        || cursor.epoch != offer.epoch
                        || cursor.version < offer.oldest_replayable_version
                        || cursor.version > offer.current_version;
                    let start = if snapshot { 0 } else { cursor.version };
                    anyhow::ensure!(syncing.insert(cursor.collection_id.clone()),
                        "hub requested a second concurrent catalogue sync");
                    let catalog = runtime.catalog.clone();
                    let outbound = tx.clone();
                    let link_limit = link_syncs.clone();
                    let collection_id = cursor.collection_id;
                    syncs.spawn(async move {
                        let result = send_catalog_pages(
                            &catalog, &outbound, &collection_id, start, snapshot, link_limit
                        ).await;
                        (collection_id, result)
                    });
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::CatalogAck(ack)) }) => {
                    runtime.catalog.acknowledge(&hub.id, &ack.collection_id, &ack.epoch, ack.version).await?;
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::RescanRequest(request)) }) => {
                    ensure_scoped_rescan(&selected, &request.collection_id)?;
                    runtime.rescan(&request.collection_id);
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::ExtractSubs(request)) }) => {
                    job_tx.try_send(hasher::JobMsg::Urgent(request))
                        .context("hub overran the extraction queue")?;
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::ExtractImageSubs(request)) }) => {
                    job_tx.try_send(hasher::JobMsg::UrgentImage(request))
                        .context("hub overran the image extraction queue")?;
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::SubsWorklist(request)) }) => {
                    job_tx.try_send(hasher::JobMsg::SubsWorklist(request))
                        .context("hub overran the subtitle work queue")?;
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::OpenRead(request)) }) => {
                    let channel = byte_channel.clone();
                    let scheduler = runtime.scheduler.clone();
                    let owner = Some(format!("hub:{}", hub.id));
                    let collections = selected.clone();
                    tokio::spawn(async move {
                        if let Err(error) = serve::serve_request_scheduled(
                            channel,
                            request,
                            collections,
                            scheduler,
                            owner,
                        ).await {
                            tracing::warn!(error = format!("{error:#}"), "byte channel failed");
                        }
                    });
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::DiscoveryWake(wake)) }) => {
                    if wake.kind == "segments" {
                        runtime.discovery_wake.notify_one();
                    }
                }
                Some(HubToHost { msg: Some(hub_to_host::Msg::DiscoveryPriorityHint(hint)) }) => {
                    if hint.kind == "segments"
                        && let Some(source) = hint.source
                    {
                        runtime.scheduler.hint_segment(
                            &scheduler_owner,
                            &hint.collection_id,
                            &source.root_token,
                            &source.path_rel,
                            Duration::from_secs(u64::from(hint.ttl_seconds.max(1))),
                        );
                        runtime.discovery_wake.notify_one();
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
    }
    Ok(())
}

async fn send_catalog_delta(
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    collection: &str,
    delta: catalog::Delta,
    first_snapshot_message: bool,
    snapshot_stream: bool,
) -> Result<u64> {
    const RECORDS_PER_MESSAGE: usize = 256;
    const PAYLOAD_BYTES_PER_MESSAGE: usize = 4 * 1024 * 1024;
    if delta.records.is_empty() {
        let through = if delta.done {
            delta.current_version
        } else if snapshot_stream {
            0
        } else {
            delta.current_version
        };
        send_link_message(
            tx,
            HostToHub {
                msg: Some(host_to_hub::Msg::CatalogDelta(CatalogDelta {
                    collection_id: collection.to_string(),
                    epoch: delta.epoch,
                    records: Vec::new(),
                    through_version: through,
                    snapshot: first_snapshot_message,
                    done: delta.done,
                })),
            },
        )
        .await?;
        return Ok(through);
    }
    anyhow::ensure!(
        delta
            .records
            .iter()
            .all(|record| record.encoded_len() <= PAYLOAD_BYTES_PER_MESSAGE),
        "one catalogue record exceeds the safe gRPC message budget"
    );
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < delta.records.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < delta.records.len() && end - start < RECORDS_PER_MESSAGE {
            let next = delta.records[end].encoded_len();
            if end > start && bytes + next > PAYLOAD_BYTES_PER_MESSAGE {
                break;
            }
            bytes += next;
            end += 1;
        }
        chunks.push(&delta.records[start..end]);
        start = end;
    }
    let mut chunks = chunks.into_iter().peekable();
    let mut first = true;
    let mut sent_through = 0;
    while let Some(chunk) = chunks.next() {
        let last_chunk = chunks.peek().is_none();
        let done = delta.done && last_chunk;
        let through = if done {
            delta.current_version
        } else if snapshot_stream {
            0
        } else {
            chunk.last().map_or(0, |record| record.version)
        };
        sent_through = through;
        send_link_message(
            tx,
            HostToHub {
                msg: Some(host_to_hub::Msg::CatalogDelta(CatalogDelta {
                    collection_id: collection.to_string(),
                    epoch: delta.epoch.clone(),
                    records: chunk.to_vec(),
                    through_version: through,
                    snapshot: first_snapshot_message && first,
                    done,
                })),
            },
        )
        .await?;
        first = false;
    }
    Ok(sent_through)
}

pub(crate) async fn send_link_message(
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    message: HostToHub,
) -> Result<()> {
    send_link_message_with_timeout(tx, message, LINK_SEND_TIMEOUT).await
}

async fn send_link_message_with_timeout(
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    message: HostToHub,
    timeout: Duration,
) -> Result<()> {
    match tokio::time::timeout(timeout, tx.send(message)).await {
        Ok(result) => result.context("mediahost link outbound channel closed"),
        Err(_) => bail!("mediahost link outbound channel stalled for {timeout:?}"),
    }
}

async fn send_catalog_pages(
    catalog: &catalog::Catalog,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    collection: &str,
    cursor: u64,
    snapshot: bool,
    link_limit: std::sync::Arc<tokio::sync::Semaphore>,
) -> Result<(String, u64)> {
    let _link_permit = link_limit.acquire_owned().await?;
    let mut pages = catalog.delta_pages(collection, cursor, snapshot).await?;
    let mut first = true;
    let mut epoch = String::new();
    let mut through = cursor;
    while let Some(page) = pages.recv().await {
        let page = page?;
        epoch.clone_from(&page.epoch);
        if !snapshot
            && first
            && page.done
            && page.records.is_empty()
            && page.current_version == cursor
        {
            return Ok((epoch, cursor));
        }
        through = send_catalog_delta(tx, collection, page, snapshot && first, snapshot).await?;
        first = false;
    }
    anyhow::ensure!(!first, "catalogue page producer stopped without output");
    Ok((epoch, through))
}
/// Enroll (or load identity) and keep the hub link up forever.
pub async fn run(
    hub_addr: &str,
    state_dir: &Path,
    name: &str,
    collections: Vec<CollectionConfig>,
    rescan_minutes: u64,
) -> Result<()> {
    let selected = collections
        .iter()
        .map(|collection| collection.name.clone())
        .collect();
    run_multi(
        state_dir,
        name,
        collections,
        rescan_minutes,
        Default::default(),
        vec![HubTarget {
            id: "default".into(),
            address: hub_addr.to_string(),
            collections: selected,
            legacy_identity: true,
        }],
        true,
    )
    .await
}

/// AR-5 all-in-one: run the mediahost engine against in-process
/// channels — no gRPC, no TLS, no enrollment. OpenRead never arrives
/// (the hub short-circuits the byte plane to direct file reads).
pub async fn run_local(
    collections: Vec<scan::CollectionConfig>,
    rescan_minutes: u64,
    state_dir: &Path,
    scheduler_config: kahawai_core::media::MediahostSchedulerConfig,
    detect_segments: bool,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    rx: tokio::sync::mpsc::Receiver<Result<HubToHost, tonic::Status>>,
) -> Result<()> {
    let scheduler = scheduler::Scheduler::new(&collections, &scheduler_config)?;
    let runtime = LocalRuntime::start_with_scheduler(
        state_dir,
        collections.clone(),
        rescan_minutes,
        scheduler,
        detect_segments,
    )
    .await?;
    run_local_link(runtime, collections, tx, rx).await
}

/// All-in-one with additional remote hubs: one catalogue/watcher/discovery
/// engine feeds both the intrinsic local link and independently supervised
/// mTLS links. The caller passes only explicitly configured external hubs.
#[allow(clippy::too_many_arguments)]
pub async fn run_local_multi(
    collections: Vec<scan::CollectionConfig>,
    rescan_minutes: u64,
    state_dir: &Path,
    name: &str,
    scheduler: scheduler::Scheduler,
    detect_segments: bool,
    hubs: Vec<HubTarget>,
    local_link: LocalLinkFactory,
) -> Result<()> {
    let runtime = LocalRuntime::start_with_scheduler(
        state_dir,
        collections.clone(),
        rescan_minutes,
        scheduler,
        detect_segments,
    )
    .await?;
    let local_runtime = runtime.clone();
    let local_supervisor = tokio::spawn(async move {
        loop {
            let (tx, rx) = local_link();
            if let Err(error) =
                run_local_link(local_runtime.clone(), collections.clone(), tx, rx).await
            {
                tracing::error!(
                    error = format!("{error:#}"),
                    "in-process hub link failed; reconnecting independently"
                );
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    });
    if hubs.is_empty() {
        local_supervisor
            .await
            .context("in-process hub link supervisor panicked")?;
        return Ok(());
    }
    let result = supervise_hubs(runtime, state_dir, name, hubs).await;
    local_supervisor.abort();
    result
}

async fn run_local_link(
    runtime: LocalRuntime,
    collections: Vec<scan::CollectionConfig>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    mut rx: tokio::sync::mpsc::Receiver<Result<HubToHost, tonic::Status>>,
) -> Result<()> {
    let scheduler_owner = "hub:local".to_string();
    let _scheduler_owner = SchedulerOwnerGuard {
        scheduler: runtime.scheduler.clone(),
        owner: scheduler_owner.clone(),
    };
    send_link_message(
        &tx,
        HostToHub {
            msg: Some(host_to_hub::Msg::CatalogOffer(
                ready_catalog_offer(&runtime, &collections, &tx).await?,
            )),
        },
    )
    .await
    .context("local link closed before catalogue offer")?;
    let (job_tx, job_rx) = tokio::sync::mpsc::channel(32);
    let _job_guard = AbortOnDrop(vec![tokio::spawn(hasher::run(
        job_rx,
        tx.clone(),
        collections,
        runtime.scheduler.clone(),
        Some(scheduler_owner.clone()),
        None,
    ))]);
    let mut sent: std::collections::HashMap<String, (String, u64)> = Default::default();
    let mut syncing = std::collections::HashSet::new();
    let mut syncs = tokio::task::JoinSet::new();
    let link_syncs = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    let mut changes = tokio::time::interval(Duration::from_secs(1));
    let mut status = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            completed = syncs.join_next(), if !syncs.is_empty() => {
                let (collection, result) = completed
                    .context("local catalogue sync task disappeared")?
                    .context("local catalogue sync task panicked")?;
                syncing.remove(&collection);
                let (epoch, through) = result?;
                sent.insert(collection, (epoch, through));
            }
            _ = ticker.tick() => {
                send_link_message(&tx, HostToHub {
                    msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})),
                }).await.context("local link heartbeat failed")?;
            }
            _ = changes.tick() => {
                let states = sent.clone();
                let advertised = runtime.catalog_versions.read().unwrap().clone();
                for (collection, (epoch, cursor)) in states {
                    if !advertised.get(&collection).is_some_and(
                        |(current_epoch, current, _)| {
                            current_epoch == &epoch && *current > cursor
                        }
                    ) {
                        continue;
                    }
                    if !syncing.insert(collection.clone()) {
                        continue;
                    }
                    let catalog = runtime.catalog.clone();
                    let outbound = tx.clone();
                    let link_limit = link_syncs.clone();
                    syncs.spawn(async move {
                        let result = send_catalog_pages(
                            &catalog, &outbound, &collection, cursor, false, link_limit
                        ).await.and_then(|(current_epoch, through)| {
                            anyhow::ensure!(current_epoch == epoch,
                                "local catalogue epoch changed under a live link");
                            Ok((current_epoch, through))
                        });
                        (collection, result)
                    });
                }
            }
            _ = status.tick() => {
                let statuses = runtime.discovery_status.read().unwrap().clone();
                for collection in &runtime.collections {
                    if let Some(status) = statuses.get(&collection.name) {
                        send_link_message(&tx, HostToHub {
                            msg: Some(host_to_hub::Msg::DiscoveryStatus(status.clone())),
                        }).await?;
                    }
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::CatalogCursor(cursor)) })) => {
                        let config = runtime.collections.iter()
                            .find(|collection| collection.name == cursor.collection_id)
                            .context("local hub requested unknown collection")?;
                        let offer = runtime.catalog.offers(std::slice::from_ref(config)).await?
                            .remove(0);
                        let snapshot = cursor.snapshot || cursor.epoch != offer.epoch
                            || cursor.version < offer.oldest_replayable_version
                            || cursor.version > offer.current_version;
                        anyhow::ensure!(syncing.insert(cursor.collection_id.clone()),
                            "local hub requested a second concurrent catalogue sync");
                        let catalog = runtime.catalog.clone();
                        let outbound = tx.clone();
                        let link_limit = link_syncs.clone();
                        let collection = cursor.collection_id;
                        let start = if snapshot { 0 } else { cursor.version };
                        syncs.spawn(async move {
                            let result = send_catalog_pages(
                                &catalog, &outbound, &collection, start, snapshot, link_limit
                            ).await;
                            (collection, result)
                        });
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::CatalogAck(ack)) })) => {
                        runtime.catalog.acknowledge("local", &ack.collection_id, &ack.epoch, ack.version).await?;
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::RescanRequest(request)) })) => {
                        ensure_scoped_rescan(&runtime.collections, &request.collection_id)?;
                        runtime.rescan(&request.collection_id);
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::ExtractSubs(request)) })) => {
                        job_tx.try_send(hasher::JobMsg::Urgent(request))
                            .context("local hub overran the extraction queue")?;
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::ExtractImageSubs(request)) })) => {
                        job_tx.try_send(hasher::JobMsg::UrgentImage(request))
                            .context("local hub overran the image extraction queue")?;
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::SubsWorklist(request)) })) => {
                        job_tx.try_send(hasher::JobMsg::SubsWorklist(request))
                            .context("local hub overran the subtitle work queue")?;
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::OpenRead(request)) })) => {
                        tracing::warn!(token = %request.lease_token,
                            "unexpected OpenRead on the in-process link (hub reads directly)");
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::DiscoveryWake(wake)) })) => {
                        if wake.kind == "segments" {
                            runtime.discovery_wake.notify_one();
                        }
                    }
                    Some(Ok(HubToHost { msg: Some(hub_to_host::Msg::DiscoveryPriorityHint(hint)) })) => {
                        if hint.kind == "segments"
                            && let Some(source) = hint.source
                        {
                            runtime.scheduler.hint_segment(
                                &scheduler_owner,
                                &hint.collection_id,
                                &source.root_token,
                                &source.path_rel,
                                Duration::from_secs(u64::from(hint.ttl_seconds.max(1))),
                            );
                            runtime.discovery_wake.notify_one();
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => bail!("local link closed"),
                }
            }
        }
    }
}

/// Aborts its tasks when the link session ends.
struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

struct SchedulerOwnerGuard {
    scheduler: scheduler::Scheduler,
    owner: String,
}

impl Drop for SchedulerOwnerGuard {
    fn drop(&mut self) {
        self.scheduler.cancel_owner(&self.owner);
    }
}

/// A request for one incremental scan cycle. `force_dirs` bypasses the
/// unchanged-skip for media files in those directories — how sidecar
/// subtitle/artwork changes get noticed (the media file's own
/// size/mtime doesn't change when a cover.jpg appears next to it).
#[derive(Default)]
struct ScanTrigger {
    force_dirs: std::collections::HashSet<std::path::PathBuf>,
    /// Startup trigger: eligible for the sync-version handshake (skip
    /// the scan when the hub already reflects our last completed one).
    /// Watcher/sweep/manual triggers always scan — they carry intent.
    initial: bool,
    /// Explicit hub/admin requests are interactive demand; periodic and
    /// watcher freshness remains the normal catalogue class.
    demand: bool,
}

/// Trigger sender that never drops: when the queue is full, the trigger
/// merges into an overflow slot the orchestrator drains before each
/// cycle, so a busy scan can't lose change notifications.
#[derive(Clone)]
struct TriggerSink {
    tx: tokio::sync::mpsc::Sender<ScanTrigger>,
    overflow: std::sync::Arc<std::sync::Mutex<Option<ScanTrigger>>>,
}

impl TriggerSink {
    fn send(&self, t: ScanTrigger) {
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(t)) = self.tx.try_send(t) {
            tracing::debug!("trigger queue full; merging into overflow");
            let mut slot = self.overflow.lock().unwrap();
            slot.get_or_insert_with(ScanTrigger::default)
                .force_dirs
                .extend(t.force_dirs);
            if t.demand
                && let Some(merged) = slot.as_mut()
            {
                merged.demand = true;
            }
            drop(slot);
            // Wake the orchestrator if space appeared meanwhile; if the
            // queue is still full, its items already guarantee a wake.
            let _ = self.tx.try_send(ScanTrigger::default());
        }
    }
}

/// Everything a running mediahost is, minus the transport (AR-5): the
/// scan orchestrators, filesystem watcher, backup sweep and scheduled job
/// workers, fed by a HostToHub sender and driven by dispatch(). Both the
/// gRPC link and the all-in-one in-process link wrap this.
pub struct Engine {
    triggers: std::collections::HashMap<String, TriggerSink>,
    manifest_waiters: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                tokio::sync::mpsc::Sender<kahawai_proto::v1::Manifest>,
            >,
        >,
    >,
    hash_tx: tokio::sync::mpsc::Sender<hasher::JobMsg>,
    segment_tx: tokio::sync::mpsc::UnboundedSender<kahawai_proto::v1::DetectSegments>,
    loudness_tx: tokio::sync::mpsc::UnboundedSender<kahawai_proto::v1::LoudnessWorklist>,
    collections: Vec<CollectionConfig>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    scheduler: scheduler::Scheduler,
    _guards: AbortOnDrop,
}

impl Engine {
    pub fn start(
        collections: &[scan::CollectionConfig],
        rescan_minutes: u64,
        state_dir: &Path,
        tx: tokio::sync::mpsc::Sender<HostToHub>,
    ) -> Engine {
        Self::start_with_runtime(collections, rescan_minutes, state_dir, tx)
    }

    fn start_with_runtime(
        collections: &[scan::CollectionConfig],
        rescan_minutes: u64,
        state_dir: &Path,
        tx: tokio::sync::mpsc::Sender<HostToHub>,
    ) -> Engine {
        // Manifest responses are routed to the scan task of their collection.
        let manifest_waiters: std::sync::Arc<
            std::sync::Mutex<
                std::collections::HashMap<
                    String,
                    tokio::sync::mpsc::Sender<kahawai_proto::v1::Manifest>,
                >,
            >,
        > = Default::default();
        let mut guards: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut triggers: std::collections::HashMap<String, TriggerSink> = Default::default();
        let scheduler = scheduler::Scheduler::new(collections, &Default::default())
            .expect("valid default mediahost scheduler");
        // ED2K hasher (MH-9): consumes hub Hashlists at its fixed scheduler
        // priority, below local metadata and above subtitle prewarm.
        let (hash_tx, hash_rx) = tokio::sync::mpsc::channel::<hasher::JobMsg>(32);
        guards.push(tokio::spawn(hasher::run(
            hash_rx,
            tx.clone(),
            collections.to_vec(),
            scheduler.clone(),
            Some("legacy-hub".into()),
            None,
        )));
        let (segment_tx, segment_rx) = tokio::sync::mpsc::unbounded_channel();
        guards.push(tokio::spawn(segments::run(
            segment_rx,
            tx.clone(),
            collections.to_vec(),
            scheduler.clone(),
            Some("legacy-hub".into()),
        )));
        let (loudness_tx, loudness_rx) = tokio::sync::mpsc::unbounded_channel();
        guards.push(tokio::spawn(loudness::run(
            loudness_rx,
            tx.clone(),
            collections.to_vec(),
            scheduler.clone(),
            Some("legacy-hub".into()),
        )));
        for c in collections {
            let (ttx, mut trx) = tokio::sync::mpsc::channel::<ScanTrigger>(8);
            let sink = TriggerSink {
                tx: ttx,
                overflow: Default::default(),
            };
            triggers.insert(c.name.clone(), sink.clone());
            let overflow = sink.overflow.clone();
            let (c, tx, waiters) = (c.clone(), tx.clone(), manifest_waiters.clone());
            let scan_scheduler = scheduler.clone();
            let ver_path = state_dir.join("sync").join(format!("{}.ver", c.name));
            guards.push(tokio::spawn(async move {
            // The persisted scan generation: bumped after every
            // completed cycle, compared by the hub on reconnect.
            let mut version: u64 = std::fs::read_to_string(&ver_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            while let Some(mut trig) = trx.recv().await {
                // Coalesce queued triggers + the overflow slot into one cycle.
                while let Ok(more) = trx.try_recv() {
                    trig.force_dirs.extend(more.force_dirs);
                    trig.initial &= more.initial;
                    trig.demand |= more.demand;
                }
                if let Some(o) = overflow.lock().unwrap().take() {
                    trig.force_dirs.extend(o.force_dirs);
                    trig.initial &= o.initial;
                    trig.demand |= o.demand;
                }
                let handshake = if trig.initial { version } else { 0 };
                let next = version + 1;
                let force_dirs = trig.force_dirs;
                let roots = c.resolved_roots().map(|root| root.token).collect::<Vec<_>>();
                let resources = scan_scheduler.resources(roots.iter().map(String::as_str), true);
                let priority = if trig.demand {
                    scheduler::Priority::Demand
                } else {
                    scheduler::Priority::CatalogFreshness
                };
                let permit = match scan_scheduler
                    .acquire(
                        priority,
                        resources,
                        Some("legacy-hub".into()),
                        format!("catalog scan {}", c.name),
                    )
                    .await
                {
                    Ok(permit) => permit,
                    Err(_) => return,
                };
                loop {
                    match scan_cycle(
                        &c,
                        &tx,
                        &waiters,
                        force_dirs.clone(),
                        handshake,
                        next,
                        permit.clone(),
                    )
                    .await
                    {
                        Ok(scan::ScanOutcome::Completed) => {
                            version = next;
                            if let Some(dir) = ver_path.parent() {
                                let _ = std::fs::create_dir_all(dir);
                            }
                            if let Err(e) = std::fs::write(&ver_path, version.to_string()) {
                                tracing::warn!(collection = %c.name, error = %e, "persisting sync version failed");
                            }
                            break;
                        }
                        Ok(scan::ScanOutcome::InSync) => break,
                        // Adoption consumed this trigger only to establish the
                        // new source identity safely. Once acknowledged, retry
                        // it immediately: a restored hub can have a genuinely
                        // older catalogue generation than this mediahost.
                        Ok(scan::ScanOutcome::RootAdoptionAcknowledged) => continue,
                        Err(e) => {
                            tracing::warn!(collection = %c.name, error = format!("{e:#}"), "scan cycle failed");
                            return; // link is gone; the session restart rescans
                        }
                    }
                }
            }
        }));
            sink.send(ScanTrigger {
                initial: true,
                ..Default::default()
            }); // startup scan
        }

        // Filesystem watcher → debounced per-collection triggers. Watcher
        // failures degrade to sweep-only operation, never fatal.
        {
            let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();
            let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(ev) = res else { return };
                // Only content changes. Access and metadata events fire for
                // MERE READS on fuse mounts — forwarding those made the
                // scanner's own discovery reads re-trigger scans forever
                // (77 self-inflicted rescans before this filter existed).
                // Unknown/Any kinds are dropped too: the periodic sweep is
                // the safety net, a feedback loop has none.
                use notify::event::{EventKind, ModifyKind};
                let relevant = matches!(
                    ev.kind,
                    EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
                );
                if !relevant {
                    return;
                }
                for p in ev.paths {
                    let _ = etx.send(p);
                }
            });
            match watcher {
                Ok(watcher) => {
                    let roots: Vec<(String, std::path::PathBuf)> = collections
                        .iter()
                        .flat_map(|c| c.roots.iter().map(|r| (c.name.clone(), r.clone())))
                        .collect();
                    let watch_roots = roots.clone();
                    let watch_tokens = collections
                        .iter()
                        .flat_map(CollectionConfig::resolved_roots)
                        .map(|root| root.token)
                        .collect::<Vec<_>>();
                    let watch_scheduler = scheduler.clone();
                    let triggers2 = triggers.clone();
                    guards.push(tokio::spawn(async move {
                    // Installing recursive watches walks every directory
                    // — minutes over sshfs. Keep it off the link's critical
                    // path so startup manifests and in-sync replies are routed
                    // promptly; protocol 3 waits rather than turning hub
                    // latency into a full scan.
                    let permit = match watch_scheduler
                        .acquire(
                            scheduler::Priority::CatalogFreshness,
                            watch_scheduler.resources(
                                watch_tokens.iter().map(String::as_str),
                                false,
                            ),
                            Some("legacy-hub".into()),
                            "filesystem watch installation",
                        )
                        .await
                    {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let watcher = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        use notify::Watcher as _;
                        let mut watcher = watcher;
                        for (_, root) in &watch_roots {
                            if let Err(e) = watcher.watch(root, notify::RecursiveMode::Recursive) {
                                tracing::warn!(root = %root.display(), error = %e, "watch failed (sweeps still cover this root)");
                            }
                        }
                        tracing::info!(roots = watch_roots.len(), "filesystem watches installed");
                        watcher
                    })
                    .await;
                    let Ok(watcher) = watcher else { return };
                    let _watcher = watcher; // dies with this task
                    let mut dirty: std::collections::HashMap<
                        String,
                        (std::collections::HashSet<std::path::PathBuf>, tokio::time::Instant),
                    > = Default::default();
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            ev = erx.recv() => {
                                let Some(path) = ev else { return };
                                // Cross-collection overlap is deliberate and
                                // valid: one file can feed separate presentation
                                // namespaces. Route an event to every matching
                                // collection, never just root-list order's first.
                                let dir = path.parent().unwrap_or(&path).to_path_buf();
                                for (cname, _) in roots.iter().filter(|(_, r)| path.starts_with(r)) {
                                    let e = dirty
                                        .entry(cname.clone())
                                        .or_insert_with(|| (Default::default(), tokio::time::Instant::now()));
                                    e.0.insert(dir.clone());
                                    e.1 = tokio::time::Instant::now();
                                }
                            }
                            _ = tick.tick() => {
                                let quiet = std::time::Duration::from_secs(3);
                                let ready: Vec<String> = dirty
                                    .iter()
                                    .filter(|(_, (_, at))| at.elapsed() >= quiet)
                                    .map(|(k, _)| k.clone())
                                    .collect();
                                for k in ready {
                                    if let Some((dirs, _)) = dirty.remove(&k)
                                        && let Some(t) = triggers2.get(&k)
                                    {
                                        tracing::info!(collection = %k, dirs = dirs.len(), "watcher triggered rescan");
                                        t.send(ScanTrigger {
                                            force_dirs: dirs,
                                            initial: false,
                                            demand: false,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }));
                }
                Err(e) => tracing::warn!(error = %e, "no filesystem watcher; relying on sweeps"),
            }
        }

        // Backup sweep (periodic full incremental pass).
        if rescan_minutes > 0 {
            let triggers2 = triggers.clone();
            guards.push(tokio::spawn(async move {
                let mut t =
                    tokio::time::interval(std::time::Duration::from_secs(rescan_minutes * 60));
                t.tick().await; // the startup scan already covered "now"
                loop {
                    t.tick().await;
                    for tt in triggers2.values() {
                        tt.send(ScanTrigger::default());
                    }
                }
            }));
        }
        Engine {
            triggers,
            manifest_waiters,
            hash_tx,
            segment_tx,
            loudness_tx,
            collections: collections.to_vec(),
            tx,
            scheduler,
            _guards: AbortOnDrop(guards),
        }
    }

    /// Route one hub→host message. OpenRead is returned to the caller:
    /// the byte plane is transport-specific (gRPC channel on the wire,
    /// a direct file read in all-in-one — AR-11 short-circuit).
    pub fn dispatch(&self, m: HubToHost) -> Result<Option<kahawai_proto::v1::OpenRead>> {
        let Some(message) = m.msg else {
            return Ok(None);
        };
        let validate_sources = |kind: &str, sources: &[kahawai_proto::v1::SourcePath]| {
            anyhow::ensure!(
                sources.iter().all(|source| !source.root_token.is_empty()),
                "protocol-3 {kind} contains an empty root token"
            );
            Ok(())
        };
        match message {
            hub_to_host::Msg::RescanRequest(r) => {
                for (name, t) in &self.triggers {
                    if r.collection_id.is_empty() || *name == r.collection_id {
                        t.send(ScanTrigger {
                            demand: true,
                            ..Default::default()
                        });
                    }
                }
                Ok(None)
            }
            hub_to_host::Msg::Manifest(m) => {
                let sender = self
                    .manifest_waiters
                    .lock()
                    .unwrap()
                    .get(&m.collection_id)
                    .cloned();
                if let Some(s) = sender {
                    let _ = s.try_send(m);
                }
                Ok(None)
            }
            hub_to_host::Msg::Hashlist(h) => {
                validate_sources("Hashlist", &h.sources)?;
                let _ = self.hash_tx.try_send(hasher::JobMsg::Hashlist(h));
                Ok(None)
            }
            hub_to_host::Msg::KeyframeWorklist(w) => {
                validate_sources("KeyframeWorklist", &w.sources)?;
                let _ = self.hash_tx.try_send(hasher::JobMsg::KeyframeWorklist(w));
                Ok(None)
            }
            hub_to_host::Msg::VideoGeometryWorklist(w) => {
                validate_sources("VideoGeometryWorklist", &w.sources)?;
                let _ = self
                    .hash_tx
                    .try_send(hasher::JobMsg::VideoGeometryWorklist(w));
                Ok(None)
            }
            hub_to_host::Msg::AttachmentsWorklist(w) => {
                validate_sources("AttachmentsWorklist", &w.sources)?;
                let _ = self
                    .hash_tx
                    .try_send(hasher::JobMsg::AttachmentsWorklist(w));
                Ok(None)
            }
            hub_to_host::Msg::SubsWorklist(w) => {
                validate_sources("SubsWorklist", &w.sources)?;
                let _ = self.hash_tx.try_send(hasher::JobMsg::SubsWorklist(w));
                Ok(None)
            }
            hub_to_host::Msg::LoudnessWorklist(work) => {
                anyhow::ensure!(
                    work.sources
                        .iter()
                        .all(|source| !source.root_token.is_empty()),
                    "LoudnessWorklist contains an empty root token"
                );
                self.loudness_tx
                    .send(work)
                    .map_err(|_| anyhow::anyhow!("loudness worker stopped"))?;
                Ok(None)
            }
            hub_to_host::Msg::ExtractSubs(e) => {
                let source = e
                    .source
                    .as_ref()
                    .context("ExtractSubs missing exact source")?;
                anyhow::ensure!(
                    !source.root_token.is_empty(),
                    "ExtractSubs has empty root token"
                );
                let _ = self.hash_tx.try_send(hasher::JobMsg::Urgent(e));
                Ok(None)
            }
            // HUB-32b: an image subtitle track's display sets, walked
            // from the container index here — on local disk it costs
            // milliseconds, and over the hub's byte plane it would not
            // finish inside a session start at all.
            hub_to_host::Msg::ExtractImageSubs(e) => {
                let source = e
                    .source
                    .as_ref()
                    .context("ExtractImageSubs missing exact source")?;
                anyhow::ensure!(
                    !source.root_token.is_empty(),
                    "ExtractImageSubs has empty root token"
                );
                let _ = self.hash_tx.try_send(hasher::JobMsg::UrgentImage(e));
                Ok(None)
            }
            hub_to_host::Msg::RootResolutionWorklist(work) => {
                let collections = self.collections.clone();
                let tx = self.tx.clone();
                let scheduler = self.scheduler.clone();
                tokio::spawn(async move {
                    resolve_legacy_roots(&collections, work, &tx, scheduler).await;
                });
                Ok(None)
            }
            hub_to_host::Msg::DetectSegments(job) => {
                anyhow::ensure!(
                    !job.request_id.is_empty(),
                    "DetectSegments has no request id"
                );
                anyhow::ensure!(
                    job.episodes.iter().all(|episode| episode
                        .source
                        .as_ref()
                        .is_some_and(|source| !source.root_token.is_empty())),
                    "DetectSegments contains a missing or empty exact source"
                );
                self.segment_tx
                    .send(job)
                    .map_err(|_| anyhow::anyhow!("segment worker stopped"))?;
                Ok(None)
            }
            hub_to_host::Msg::OpenRead(req) => {
                let source = req
                    .source
                    .as_ref()
                    .context("OpenRead missing exact source")?;
                anyhow::ensure!(
                    !source.root_token.is_empty(),
                    "OpenRead has empty root token"
                );
                Ok(Some(req))
            }
            _ => Ok(None),
        }
    }
}

/// Resolve only the legacy source rows the hub named. This is not a scan: no
/// directory walk, discovery, sidecar reconciliation or generation update.
async fn resolve_legacy_roots(
    collections: &[CollectionConfig],
    work: kahawai_proto::v1::RootResolutionWorklist,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    scheduler: scheduler::Scheduler,
) {
    let Some(collection) = collections.iter().find(|c| c.name == work.collection_id) else {
        return;
    };
    let roots = collection.resolved_roots().collect::<Vec<_>>();
    let resources = scheduler.resources(roots.iter().map(|root| root.token.as_str()), false);
    let Ok(permit) = scheduler
        .acquire(
            scheduler::Priority::LocalMetadata,
            resources,
            Some("legacy-hub".into()),
            format!("root resolution {}", work.collection_id),
        )
        .await
    else {
        return;
    };
    let sources = work.sources;
    let Ok(Ok(resolutions)) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut resolutions = Vec::with_capacity(sources.len());
        for source in sources {
            permit.checkpoint_blocking()?;
            let mut matches = Vec::new();
            for root in &roots {
                let path = root.path.join(&source.path_rel);
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if meta.len() != source.size {
                    continue;
                }
                let Ok((head, tail, oshash)) = scan::identity_hashes(&path, source.size) else {
                    continue;
                };
                if (head, tail, oshash) == (source.head_xxh3, source.tail_xxh3, source.oshash) {
                    matches.push(root.token.clone());
                }
            }
            let (root_token, error) = match matches.as_slice() {
                [token] => (token.clone(), String::new()),
                [] => (String::new(), "missing".into()),
                _ => (String::new(), "ambiguous".into()),
            };
            resolutions.push(kahawai_proto::v1::RootResolution {
                path_rel: source.path_rel.clone(),
                source: (!root_token.is_empty()).then_some(kahawai_proto::v1::SourcePath {
                    root_token,
                    path_rel: source.path_rel,
                }),
                error,
            });
        }
        Ok(resolutions)
    })
    .await
    else {
        return;
    };
    let _ = tx
        .send(HostToHub {
            msg: Some(host_to_hub::Msg::RootResolutions(
                kahawai_proto::v1::RootResolutions {
                    collection_id: work.collection_id,
                    resolutions,
                },
            )),
        })
        .await;
}

/// One link session: Hello/HelloAck, then per-collection scan
/// orchestrators fed by three triggers — the filesystem watcher
/// (primary; useless over sshfs where inotify never fires), the
/// periodic backup sweep, and hub-sent RescanRequests (admin button).
#[allow(dead_code)] // retained only for protocol-3 regression fixtures
async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    collections: &[CollectionConfig],
    rescan_minutes: u64,
    state_dir: &Path,
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls.clone()).await?;
    // The byte plane gets its OWN connection: lease streams pushing (or
    // stalling on) megabytes must never exhaust the control link's h2
    // connection window — that froze heartbeats for 40 s at a time and
    // the hub declared the link dead mid-scan.
    let byte_channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    // Mirror the hub's raised limit: worklists and manifests can pass
    // tonic's 4 MB default on large collections.
    let mut client = MediahostLinkClient::new(channel.clone())
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024);

    let (tx, rx) = tokio::sync::mpsc::channel::<HostToHub>(16);
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::Hello(Hello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            name: name.to_string(),
            build: kahawai_core::build_stamp().into(),
            segment_detector_generation: kahawai_core::segments::DETECTOR_GENERATION,
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
            Some(hub_to_host::Msg::HelloAck(ack)) => {
                anyhow::ensure!(
                    ack.protocol_major == PROTOCOL_MAJOR,
                    "incompatible hub protocol {}.{}; this mediahost requires {}.{}",
                    ack.protocol_major,
                    ack.protocol_minor,
                    PROTOCOL_MAJOR,
                    PROTOCOL_MINOR
                );
                tracing::info!(
                    hub_protocol = format!("{}.{}", ack.protocol_major, ack.protocol_minor),
                    "link established"
                );
            }
            _ => bail!("hub did not open with HelloAck"),
        },
        None => bail!("hub closed the link before HelloAck"),
    };

    let engine = Engine::start_with_runtime(collections, rescan_minutes, state_dir, tx.clone());

    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let queued = tokio::time::Instant::now();
                if tx.send(HostToHub { msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    bail!("link sender closed");
                }
                let waited = queued.elapsed();
                if waited > std::time::Duration::from_secs(2) {
                    tracing::warn!(?waited, "heartbeat send was delayed by a full link channel");
                } else {
                    tracing::debug!("heartbeat queued");
                }
            }
            msg = inbound.message() => {
                match msg {
                    Ok(Some(m)) => {
                        if let Some(req) = engine.dispatch(m)? {
                            let ch = byte_channel.clone();
                            // A background lease is served like any other and
                            // does not make this box busy: the hub's own sweeps
                            // must not shut the gate on the work only this box
                            // can do.
                            let foreground = !req.background;
                            let collection_id = req.collection_id.clone();
                            let path_rel = req
                                .source
                                .as_ref()
                                .map(|source| source.path_rel.clone())
                                .unwrap_or_default();
                            if foreground {
                                tracing::info!(
                                    collection = %collection_id,
                                    path = %path_rel,
                                    "foreground read lease opened"
                                );
                            }
                            let scheduler = engine.scheduler.clone();
                            let collections = collections.to_vec();
                            tokio::spawn(async move {
                                let result = serve::serve_request_scheduled(
                                    ch,
                                    req,
                                    collections,
                                    scheduler,
                                    Some("legacy-hub".into()),
                                )
                                .await;
                                if foreground {
                                    tracing::info!(
                                        collection = %collection_id,
                                        path = %path_rel,
                                        "foreground read lease closed"
                                    );
                                }
                                if let Err(e) = result {
                                    tracing::warn!(error = format!("{e:#}"), "byte channel failed");
                                }
                            });
                        }
                    }
                    Ok(None) => return Ok(()),
                    Err(e) => bail!("link stream error: {e}"),
                }
            }
        }
    }
}

/// One incremental scan cycle: re-announce (resets the hub's seen-set
/// for reconciliation), fetch a fresh manifest, scan. Returns false
/// when the hub answered in_sync (handshake matched; nothing scanned).
async fn scan_cycle(
    c: &CollectionConfig,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    waiters: &std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::mpsc::Sender<kahawai_proto::v1::Manifest>>,
    >,
    force_dirs: std::collections::HashSet<std::path::PathBuf>,
    handshake_version: u64,
    report_version: u64,
    permit: scheduler::JobPermit,
) -> Result<scan::ScanOutcome> {
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::AnnounceCollection(AnnounceCollection {
            id: c.name.clone(),
            media_type: c.media_type.clone(),
            roots: c
                .resolved_roots()
                .map(|root| kahawai_proto::v1::CollectionRoot {
                    root_token: root.token,
                    normalized_path: root.path.display().to_string(),
                })
                .collect(),
        })),
    })
    .await
    .context("link closed before announce")?;
    let (mtx, mrx) = tokio::sync::mpsc::channel(16);
    waiters.lock().unwrap().insert(c.name.clone(), mtx);
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::ManifestRequest(
            kahawai_proto::v1::ManifestRequest {
                collection_id: c.name.clone(),
                sync_version: handshake_version,
            },
        )),
    })
    .await
    .context("link closed before manifest request")?;
    scan::scan_collection(
        c.clone(),
        tx.clone(),
        mrx,
        force_dirs,
        report_version,
        permit,
    )
    .await
}

#[cfg(test)]
mod scheduler_integration_tests {
    use super::{
        CollectionConfig, HubTarget, LocalLinkFactory, ensure_scoped_rescan, run_local_multi,
        scheduler, send_catalog_pages, send_link_message_with_timeout,
    };

    #[test]
    fn hub_rescans_name_exactly_one_shared_collection() {
        let collections = [CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: Vec::new(),
        }];
        assert!(ensure_scoped_rescan(&collections, "movies").is_ok());
        assert!(ensure_scoped_rescan(&collections, "").is_err());
        assert!(ensure_scoped_rescan(&collections, "private").is_err());
    }

    #[tokio::test]
    async fn a_stalled_outbound_link_send_is_bounded() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.send(kahawai_proto::v1::HostToHub::default())
            .await
            .unwrap();
        let error = send_link_message_with_timeout(
            &tx,
            kahawai_proto::v1::HostToHub::default(),
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("link outbound channel stalled"));
    }

    #[tokio::test]
    async fn two_stalled_catalog_links_do_not_hold_a_third_link() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let collection = CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![root.path().to_path_buf()],
        };
        let catalog =
            super::catalog::Catalog::open(state.path(), std::slice::from_ref(&collection))
                .await
                .unwrap();
        let generation = catalog.begin_scan("movies").await.unwrap();
        // Two records larger than half the wire chunk force two sends. A
        // capacity-one outbound channel therefore pins each slow link on its
        // second send without relying on timing inside the page producer.
        let large_metadata = "x".repeat(3 * 1024 * 1024);
        for path_rel in ["one.mkv", "two.mkv"] {
            catalog
                .upsert_file(
                    "movies",
                    &kahawai_proto::v1::FileRecord {
                        source: Some(kahawai_proto::v1::SourcePath::new(
                            kahawai_core::media::root_token(root.path()),
                            path_rel,
                        )),
                        size: 1,
                        mtime_unix: 1,
                        streams_json: large_metadata.clone(),
                        ..Default::default()
                    },
                    generation,
                )
                .await
                .unwrap();
        }
        catalog
            .finish_scan("movies", generation, &Default::default())
            .await
            .unwrap();

        let mut stalled = Vec::new();
        for _ in 0..2 {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            let catalog = catalog.clone();
            stalled.push(tokio::spawn(async move {
                send_catalog_pages(
                    &catalog,
                    &tx,
                    "movies",
                    0,
                    true,
                    std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
                )
                .await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (healthy_tx, mut healthy_rx) = tokio::sync::mpsc::channel(1);
        let drain = tokio::spawn(async move { while healthy_rx.recv().await.is_some() {} });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send_catalog_pages(
                &catalog,
                &healthy_tx,
                "movies",
                0,
                true,
                std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            ),
        )
        .await
        .expect("two slow hubs delayed an independent catalogue link")
        .unwrap();
        drop(healthy_tx);
        drain.await.unwrap();
        for task in stalled {
            task.abort();
        }
    }

    #[tokio::test]
    async fn an_intrinsic_link_failure_keeps_external_hubs_supervised() {
        let dir = tempfile::tempdir().unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_attempts = attempts.clone();
        let local_link: LocalLinkFactory = std::sync::Arc::new(move || {
            factory_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (outbound, _outbound_rx) = tokio::sync::mpsc::channel(4);
            let (inbound_tx, inbound) = tokio::sync::mpsc::channel(1);
            drop(inbound_tx);
            (outbound, inbound)
        });
        let run = run_local_multi(
            Vec::new(),
            60,
            dir.path(),
            "test-host",
            scheduler::Scheduler::new(&[], &Default::default()).unwrap(),
            false,
            vec![HubTarget {
                id: "unavailable".into(),
                address: "https://127.0.0.1:9".into(),
                collections: Vec::new(),
                legacy_identity: false,
            }],
            local_link,
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), run)
                .await
                .is_err(),
            "the intrinsic link failure stopped external supervision"
        );
        assert!(
            attempts.load(std::sync::atomic::Ordering::Relaxed) >= 2,
            "the intrinsic link was not recreated after failure"
        );
    }
}

#[cfg(test)]
mod root_resolution_tests {
    use super::*;

    async fn recv_host_message(
        rx: &mut tokio::sync::mpsc::Receiver<HostToHub>,
    ) -> host_to_hub::Msg {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("mediahost message timed out")
            .expect("mediahost message channel closed")
            .msg
            .expect("empty mediahost message")
    }

    #[tokio::test]
    async fn adoption_acknowledgement_immediately_retries_the_startup_scan() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("sync")).unwrap();
        std::fs::write(state.path().join("sync/movies.ver"), "99").unwrap();
        let collections = vec![CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![root.path().into()],
        }];
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let engine = Engine::start(&collections, 0, state.path(), tx);

        assert!(matches!(
            recv_host_message(&mut rx).await,
            host_to_hub::Msg::AnnounceCollection(_)
        ));
        let host_to_hub::Msg::ManifestRequest(first_request) = recv_host_message(&mut rx).await
        else {
            panic!("startup did not request a manifest")
        };
        assert_eq!(first_request.sync_version, 99);

        engine
            .dispatch(HubToHost {
                msg: Some(hub_to_host::Msg::Manifest(kahawai_proto::v1::Manifest {
                    collection_id: "movies".into(),
                    entries: Vec::new(),
                    done: true,
                    in_sync: true,
                    sidecars_compared: true,
                    root_adoption: true,
                })),
            })
            .unwrap();
        assert!(matches!(
            recv_host_message(&mut rx).await,
            host_to_hub::Msg::RootAdoptionAck(_)
        ));
        assert!(matches!(
            recv_host_message(&mut rx).await,
            host_to_hub::Msg::AnnounceCollection(_)
        ));
        let host_to_hub::Msg::ManifestRequest(retry_request) = recv_host_message(&mut rx).await
        else {
            panic!("adoption acknowledgement did not retry the manifest")
        };
        assert_eq!(
            retry_request.sync_version, 99,
            "the deferred startup trigger must retain the host generation"
        );

        engine
            .dispatch(HubToHost {
                msg: Some(hub_to_host::Msg::Manifest(kahawai_proto::v1::Manifest {
                    collection_id: "movies".into(),
                    entries: Vec::new(),
                    done: true,
                    in_sync: false,
                    sidecars_compared: true,
                    root_adoption: false,
                })),
            })
            .unwrap();
        loop {
            if let host_to_hub::Msg::ScanProgress(progress) = recv_host_message(&mut rx).await
                && progress.complete
            {
                assert_eq!(progress.sync_version, 100);
                break;
            }
        }
        for _ in 0..20 {
            if std::fs::read_to_string(state.path().join("sync/movies.ver"))
                .is_ok_and(|version| version == "100")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("completed retry did not persist generation 100");
    }

    #[tokio::test]
    async fn targeted_resolution_never_guesses_between_identical_candidates() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [&a, &b] {
            std::fs::write(dir.path().join("same.mkv"), b"identical bytes").unwrap();
        }
        let size = std::fs::metadata(a.path().join("same.mkv")).unwrap().len();
        let (head_xxh3, tail_xxh3, oshash) =
            crate::scan::identity_hashes(&a.path().join("same.mkv"), size).unwrap();
        let collections = vec![CollectionConfig {
            name: "movies".into(),
            media_type: "movies".into(),
            roots: vec![a.path().into(), b.path().into()],
        }];
        let scheduler = scheduler::Scheduler::new(&collections, &Default::default()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        resolve_legacy_roots(
            &collections,
            kahawai_proto::v1::RootResolutionWorklist {
                collection_id: "movies".into(),
                sources: vec![kahawai_proto::v1::LegacySource {
                    path_rel: "same.mkv".into(),
                    size,
                    head_xxh3,
                    tail_xxh3,
                    oshash,
                }],
            },
            &tx,
            scheduler,
        )
        .await;
        let message = rx.recv().await.unwrap();
        let host_to_hub::Msg::RootResolutions(result) = message.msg.unwrap() else {
            panic!("wrong result kind")
        };
        assert_eq!(result.resolutions.len(), 1);
        assert!(result.resolutions[0].source.is_none());
        assert_eq!(result.resolutions[0].error, "ambiguous");
    }
}
