//! Mediahost module: enroll once, then keep a control link to the hub
//! (AR-3: always dials out) and scan collections up to it.

pub mod ed2k;
pub mod hasher;
pub mod scan;
pub mod segments;
pub mod serve;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use kahawai_proto::v1::mediahost_link_client::MediahostLinkClient;
use kahawai_proto::v1::{
    AnnounceCollection, Heartbeat, Hello, HostToHub, HubToHost, host_to_hub, hub_to_host,
};
use kahawai_proto::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
use scan::CollectionConfig;
use tokio_stream::wrappers::ReceiverStream;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// What the ED2K hasher must yield to (MH-9): scans and lease serving.
/// Busy while either count is nonzero; the hasher pauses between chunks.
#[derive(Clone, Default)]
pub struct Activity {
    scans: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    leases: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    urgent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    background: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub struct ActivityGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}
pub(crate) type BlockingActivityGuard = std::sync::Arc<ActivityGuard>;

pub struct BackgroundGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for BackgroundGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Activity {
    fn enter(counter: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> ActivityGuard {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ActivityGuard(counter.clone())
    }
    pub fn scan(&self) -> ActivityGuard {
        Self::enter(&self.scans)
    }
    pub fn lease(&self) -> ActivityGuard {
        Self::enter(&self.leases)
    }
    pub fn urgent(&self) -> ActivityGuard {
        Self::enter(&self.urgent)
    }
    pub fn try_background(&self) -> Option<BackgroundGuard> {
        self.background
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Acquire,
                std::sync::atomic::Ordering::Relaxed,
            )
            .ok()
            .map(|_| BackgroundGuard(self.background.clone()))
    }
    pub fn busy(&self) -> bool {
        self.scans.load(std::sync::atomic::Ordering::Relaxed) != 0
            || self.leases.load(std::sync::atomic::Ordering::Relaxed) != 0
            || self.urgent.load(std::sync::atomic::Ordering::Relaxed) != 0
    }
}

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

#[derive(Clone, Default)]
struct JobRuntime {
    activity: Activity,
    segment_serial: std::sync::Arc<tokio::sync::Mutex<()>>,
}
/// Enroll (or load identity) and keep the hub link up forever.
pub async fn run(
    hub_addr: &str,
    state_dir: &Path,
    name: &str,
    collections: Vec<CollectionConfig>,
    rescan_minutes: u64,
) -> Result<()> {
    let mut id =
        kahawai_transport::enroll::ensure_identity(hub_addr, state_dir, "mediahost", name).await?;
    // Survives link engines: cancelled spawn_blocking work retains this permit
    let jobs = JobRuntime::default();
    loop {
        // SEC-7: renew before (re)connecting when inside the window, and
        // bound the link's lifetime so a long-lived link still renews.
        match kahawai_transport::renew::maybe_renew(hub_addr, state_dir, "mediahost", name).await {
            Ok(Some(renewed)) => id = renewed,
            Ok(None) => {}
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "certificate renewal failed; retrying later"
            ),
        }
        let tls = kahawai_transport::mtls::mtls_client_config(&id)?;
        let renewal_due = kahawai_transport::renew::seconds_until_renewal_due(&id.cert_pem)
            .unwrap_or(i64::MAX)
            .max(3600) as u64;
        tokio::select! {
            r = link_once(
                hub_addr,
                tls.clone(),
                name,
                &collections,
                rescan_minutes,
                state_dir,
                jobs.clone(),
            ) => match r {
                Ok(()) => tracing::warn!("hub closed the link; reconnecting"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed; reconnecting"),
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(renewal_due)) => {
                tracing::info!("certificate renewal due; cycling the link");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// AR-5 all-in-one: run the mediahost engine against in-process
/// channels — no gRPC, no TLS, no enrollment. OpenRead never arrives
/// (the hub short-circuits the byte plane to direct file reads).
pub async fn run_local(
    collections: Vec<scan::CollectionConfig>,
    rescan_minutes: u64,
    state_dir: &Path,
    activity: Activity,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    mut rx: tokio::sync::mpsc::Receiver<Result<HubToHost, tonic::Status>>,
) -> Result<()> {
    let engine = Engine::start_with_activity(
        &collections,
        rescan_minutes,
        state_dir,
        tx.clone(),
        activity,
    );
    let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if tx.send(HostToHub { msg: Some(host_to_hub::Msg::Heartbeat(Heartbeat {})) })
                    .await
                    .is_err()
                {
                    bail!("local link closed");
                }
            }
            msg = rx.recv() => {
                match msg {
                    Some(Ok(m)) => {
                        if let Some(req) = engine.dispatch(m)? {
                            tracing::warn!(token = %req.lease_token,
                                "unexpected OpenRead on the in-process link (hub reads directly)");
                        }
                    }
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
            drop(slot);
            // Wake the orchestrator if space appeared meanwhile; if the
            // queue is still full, its items already guarantee a wake.
            let _ = self.tx.try_send(ScanTrigger::default());
        }
    }
}

/// Everything a running mediahost is, minus the transport (AR-5): the
/// scan orchestrators, filesystem watcher, backup sweep and idle job
/// worker, fed by a HostToHub sender and driven by dispatch(). Both the
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
    collections: Vec<CollectionConfig>,
    tx: tokio::sync::mpsc::Sender<HostToHub>,
    pub activity: Activity,
    _guards: AbortOnDrop,
}

impl Engine {
    pub fn start(
        collections: &[scan::CollectionConfig],
        rescan_minutes: u64,
        state_dir: &Path,
        tx: tokio::sync::mpsc::Sender<HostToHub>,
    ) -> Engine {
        Self::start_with_activity(
            collections,
            rescan_minutes,
            state_dir,
            tx,
            Activity::default(),
        )
    }

    pub fn start_with_activity(
        collections: &[scan::CollectionConfig],
        rescan_minutes: u64,
        state_dir: &Path,
        tx: tokio::sync::mpsc::Sender<HostToHub>,
        activity: Activity,
    ) -> Engine {
        Self::start_with_runtime(
            collections,
            rescan_minutes,
            state_dir,
            tx,
            activity,
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        )
    }

    fn start_with_runtime(
        collections: &[scan::CollectionConfig],
        rescan_minutes: u64,
        state_dir: &Path,
        tx: tokio::sync::mpsc::Sender<HostToHub>,
        activity: Activity,
        segment_serial: std::sync::Arc<tokio::sync::Mutex<()>>,
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
        // ED2K hasher (MH-9): consumes hub Hashlists, chugs only when idle.
        let (hash_tx, hash_rx) = tokio::sync::mpsc::channel::<hasher::JobMsg>(32);
        guards.push(tokio::spawn(hasher::run(
            hash_rx,
            tx.clone(),
            collections.to_vec(),
            activity.clone(),
        )));
        let (segment_tx, segment_rx) = tokio::sync::mpsc::unbounded_channel();
        guards.push(tokio::spawn(segments::run(
            segment_rx,
            tx.clone(),
            collections.to_vec(),
            activity.clone(),
            segment_serial,
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
            let activity = activity.clone();
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
                }
                if let Some(o) = overflow.lock().unwrap().take() {
                    trig.force_dirs.extend(o.force_dirs);
                    trig.initial &= o.initial;
                }
                let handshake = if trig.initial { version } else { 0 };
                let next = version + 1;
                let force_dirs = trig.force_dirs;
                let busy: BlockingActivityGuard = std::sync::Arc::new(activity.scan());
                loop {
                    match scan_cycle(
                        &c,
                        &tx,
                        &waiters,
                        force_dirs.clone(),
                        handshake,
                        next,
                        busy.clone(),
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
                    let triggers2 = triggers.clone();
                    guards.push(tokio::spawn(async move {
                    // Installing recursive watches walks every directory
                    // — minutes over sshfs. Keep it off the link's critical
                    // path so startup manifests and in-sync replies are routed
                    // promptly; protocol 3 waits rather than turning hub
                    // latency into a full scan.
                    let watcher = tokio::task::spawn_blocking(move || {
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
                                        t.send(ScanTrigger { force_dirs: dirs, initial: false });
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
            collections: collections.to_vec(),
            tx,
            activity,
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
                        t.send(ScanTrigger::default());
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
                tokio::spawn(async move {
                    resolve_legacy_roots(&collections, work, &tx).await;
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
) {
    let Some(collection) = collections.iter().find(|c| c.name == work.collection_id) else {
        return;
    };
    let mut resolutions = Vec::with_capacity(work.sources.len());
    for source in work.sources {
        let mut matches = Vec::new();
        for root in collection.resolved_roots() {
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
                matches.push(root.token);
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
async fn link_once(
    hub_addr: &str,
    tls: std::sync::Arc<rustls::ClientConfig>,
    name: &str,
    collections: &[CollectionConfig],
    rescan_minutes: u64,
    state_dir: &Path,
    jobs: JobRuntime,
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
    }

    let engine = Engine::start_with_runtime(
        collections,
        rescan_minutes,
        state_dir,
        tx.clone(),
        jobs.activity,
        jobs.segment_serial,
    );

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
                            let path = serve::resolve_path(collections, &req);
                            if let Err(e) = &path {
                                tracing::warn!(error = format!("{e:#}"), "refusing OpenRead");
                            }
                            let ch = byte_channel.clone();
                            // A background lease is served like any other and
                            // does not make this box busy: the hub's own sweeps
                            // must not shut the gate on the work only this box
                            // can do.
                            let busy = (!req.background).then(|| engine.activity.lease());
                            tokio::spawn(async move {
                                let _busy = busy;
                                if let Err(e) =
                                    serve::serve_lease(ch, req.lease_token, path).await
                                {
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
    blocking_guard: BlockingActivityGuard,
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
        blocking_guard,
    )
    .await
}

#[cfg(test)]
mod activity_tests {
    use super::Activity;

    #[test]
    fn urgent_work_is_foreground_and_background_is_exclusive() {
        let activity = Activity::default();
        assert!(!activity.busy());
        let urgent = activity.urgent();
        assert!(activity.busy());
        drop(urgent);
        assert!(!activity.busy());

        let background = activity.try_background().expect("first background job");
        assert!(
            activity.try_background().is_none(),
            "two heavy background jobs entered together"
        );
        drop(background);
        assert!(activity.try_background().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_work_retains_background_permit_after_async_abort() {
        let activity = Activity::default();
        let permit: std::sync::Arc<dyn Send + Sync> =
            std::sync::Arc::new(activity.try_background().unwrap());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            let blocking = permit.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = blocking;
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            })
            .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(
            activity.try_background().is_none(),
            "abort released permit while blocking work was live"
        );

        release_tx.send(()).unwrap();
        for _ in 0..100 {
            if activity.try_background().is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("blocking work did not release its permit");
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
