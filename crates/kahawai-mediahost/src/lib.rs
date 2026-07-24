//! Mediahost module: enroll once, then keep a control link to the hub
//! (AR-3: always dials out) and scan collections up to it.

pub mod scan;
pub mod serve;

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
    rescan_minutes: u64,
) -> Result<()> {
    let id = kahawai_transport::enroll::ensure_identity(hub_addr, state_dir, "mediahost", name)
        .await?;
    let tls = kahawai_transport::mtls::mtls_client_config(&id)?;

    loop {
        match link_once(hub_addr, tls.clone(), name, &collections, rescan_minutes).await {
            Ok(()) => tracing::warn!("hub closed the link; reconnecting"),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "link failed; reconnecting"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
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
            slot.get_or_insert_with(ScanTrigger::default).force_dirs.extend(t.force_dirs);
            drop(slot);
            // Wake the orchestrator if space appeared meanwhile; if the
            // queue is still full, its items already guarantee a wake.
            let _ = self.tx.try_send(ScanTrigger::default());
        }
    }
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
) -> Result<()> {
    let channel = kahawai_transport::tls::grpc_channel_with(hub_addr, tls).await?;
    let mut client = MediahostLinkClient::new(channel.clone());

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
            _ => bail!("hub did not open with HelloAck"),
        },
        None => bail!("hub closed the link before HelloAck"),
    }

    // Manifest responses are routed to the scan task of their collection.
    let manifest_waiters: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::mpsc::Sender<kahawai_proto::v1::Manifest>>,
        >,
    > = Default::default();
    let mut guards: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut triggers: std::collections::HashMap<String, TriggerSink> = Default::default();
    for c in collections {
        let (ttx, mut trx) = tokio::sync::mpsc::channel::<ScanTrigger>(8);
        let sink = TriggerSink { tx: ttx, overflow: Default::default() };
        triggers.insert(c.name.clone(), sink.clone());
        let overflow = sink.overflow.clone();
        let (c, tx, waiters) = (c.clone(), tx.clone(), manifest_waiters.clone());
        guards.push(tokio::spawn(async move {
            while let Some(mut trig) = trx.recv().await {
                // Coalesce queued triggers + the overflow slot into one cycle.
                while let Ok(more) = trx.try_recv() {
                    trig.force_dirs.extend(more.force_dirs);
                }
                if let Some(o) = overflow.lock().unwrap().take() {
                    trig.force_dirs.extend(o.force_dirs);
                }
                if let Err(e) = scan_cycle(&c, &tx, &waiters, trig.force_dirs).await {
                    tracing::warn!(collection = %c.name, error = format!("{e:#}"), "scan cycle failed");
                    return; // link is gone; the session restart rescans
                }
            }
        }));
        sink.send(ScanTrigger::default()); // startup scan
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
            Ok(mut watcher) => {
                use notify::Watcher as _;
                for c in collections {
                    for root in &c.roots {
                        if let Err(e) = watcher.watch(root, notify::RecursiveMode::Recursive) {
                            tracing::warn!(root = %root.display(), error = %e, "watch failed (sweeps still cover this root)");
                        }
                    }
                }
                let roots: Vec<(String, std::path::PathBuf)> = collections
                    .iter()
                    .flat_map(|c| c.roots.iter().map(|r| (c.name.clone(), r.clone())))
                    .collect();
                let triggers2 = triggers.clone();
                guards.push(tokio::spawn(async move {
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
                                if let Some((cname, _)) =
                                    roots.iter().find(|(_, r)| path.starts_with(r))
                                {
                                    let dir = path.parent().unwrap_or(&path).to_path_buf();
                                    let e = dirty
                                        .entry(cname.clone())
                                        .or_insert_with(|| (Default::default(), tokio::time::Instant::now()));
                                    e.0.insert(dir);
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
                                        t.send(ScanTrigger { force_dirs: dirs });
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
    let _guard = AbortOnDrop(guards);

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
                    Ok(Some(m)) => {
                        if let Some(hub_to_host::Msg::RescanRequest(r)) = &m.msg {
                            for (name, t) in &triggers {
                                if r.collection_id.is_empty() || *name == r.collection_id {
                                    t.send(ScanTrigger::default());
                                }
                            }
                            continue;
                        }
                        if let Some(hub_to_host::Msg::Manifest(m)) = &m.msg {
                            let sender =
                                manifest_waiters.lock().unwrap().get(&m.collection_id).cloned();
                            if let Some(s) = sender {
                                let _ = s.try_send(m.clone());
                            }
                            continue;
                        }
                        if let Some(hub_to_host::Msg::OpenRead(req)) = m.msg {
                            let path = serve::resolve_path(collections, &req);
                            if let Err(e) = &path {
                                tracing::warn!(error = format!("{e:#}"), "refusing OpenRead");
                            }
                            let ch = channel.clone();
                            tokio::spawn(async move {
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
/// for reconciliation), fetch a fresh manifest, scan.
async fn scan_cycle(
    c: &CollectionConfig,
    tx: &tokio::sync::mpsc::Sender<HostToHub>,
    waiters: &std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::mpsc::Sender<kahawai_proto::v1::Manifest>>,
    >,
    force_dirs: std::collections::HashSet<std::path::PathBuf>,
) -> Result<()> {
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::AnnounceCollection(AnnounceCollection {
            id: c.name.clone(),
            media_type: c.media_type.clone(),
            roots: c.roots.iter().map(|r| r.display().to_string()).collect(),
        })),
    })
    .await
    .context("link closed before announce")?;
    let (mtx, mrx) = tokio::sync::mpsc::channel(16);
    waiters.lock().unwrap().insert(c.name.clone(), mtx);
    tx.send(HostToHub {
        msg: Some(host_to_hub::Msg::ManifestRequest(kahawai_proto::v1::ManifestRequest {
            collection_id: c.name.clone(),
        })),
    })
    .await
    .context("link closed before manifest request")?;
    scan::scan_collection(c.clone(), tx.clone(), mrx, force_dirs).await
}
