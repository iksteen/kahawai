//! The hub's byte plane (AR-10, AR-12): how anything on the hub reads a
//! file that lives on a mediahost.
//!
//! A read lease is minted here, announced to the host with `OpenRead`, and
//! fulfilled when the host's byte channel arrives with the matching token
//! (`link_service::byte_channel` → `Leases::fulfill`). Dropping the lease
//! closes the channel. Playback is the largest consumer but not the only
//! one: subtitle extraction, artwork and `.nfo` reads all lease bytes the
//! same way, which is why this lives beside the session manager rather than
//! inside it.
//!
//! All-in-one (AR-5, AR-11): when the mediahost runs in this process its
//! byte plane is a function call. The local hooks registered at startup
//! resolve the path and admit the read through the mediahost's own
//! scheduler, so a viewer holds CPU for the life of the lease while a sweep
//! is admitted as background work.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kahawai_proto::v1::{HubToHost, OpenRead, hub_to_host};

use crate::leases::{Lease, Leases, LocalAdmission, new_lease_token};
use crate::registry::Registry;

/// Who a read lease is for. It travels to the mediahost, which serves both
/// identically and schedules its OWN local work — hashes, declarations,
/// probes, extractions — around whether it is serving a viewer.
///
/// The distinction has to be stated because the host cannot infer it: bytes
/// are bytes. Without it, a sweep reading every episode in the library is
/// indistinguishable from somebody watching all day, and the host's queues
/// never drain — measured, hours of intro detection during which not one
/// file was declared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Reader {
    /// Somebody is waiting on these bytes.
    Viewer,
    /// The hub's own background work, which can be outrun by anything.
    Sweep,
}

/// Every other refusal from a session start is about the item: it has no
/// sources, it cannot be played, you already hold too many streams. This one
/// is about the moment — the bytes exist, on a host that is not answering
/// right now — and the same request may well succeed in a minute.
///
/// A type rather than a sentence, because the caller has to ACT on the
/// difference: it becomes 503 at the API edge, and a client that sees 503
/// stands by and tries again instead of giving up. Matching that distinction
/// out of an error message would break the first time someone rewords it.
#[derive(Debug, Clone, Copy)]
pub struct SourceOffline;

impl std::fmt::Display for SourceOffline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no source is currently available (mediahost offline)")
    }
}

impl std::error::Error for SourceOffline {}

/// Adapts a mediahost read lease to the remuxer's random-access source
/// trait; runs on the remux feeder thread, bridging into the runtime.
pub(crate) struct LeaseSource {
    pub(crate) lease: Lease,
    pub(crate) size: u64,
    pub(crate) handle: tokio::runtime::Handle,
    /// Reads served, for the log line that says whether a stalled consumer ever
    /// got its first byte.
    pub(crate) reads: u64,
}

impl kahawai_media::remux::RemuxSource for LeaseSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(self.size - offset);
        self.reads += 1;
        let started = std::time::Instant::now();
        if self.reads % 64 == 1 {
            tracing::debug!(offset, len, reads = self.reads, "lease read");
        }
        tracing::trace!(offset, len, reads = self.reads, "lease read: asking");
        let _guard = self.handle.enter();
        let mut stream = self.lease.read_range(offset, len).into_inner();
        let outcome = self.handle.block_on(async {
            let mut filled = 0usize;
            while filled < len as usize {
                match stream.recv().await {
                    Some(Ok(bytes)) => {
                        let n = bytes.len().min(buf.len() - filled);
                        buf[filled..filled + n].copy_from_slice(&bytes[..n]);
                        filled += n;
                    }
                    Some(Err(e)) => return Err(std::io::Error::other(e)),
                    None => break,
                }
            }
            Ok(filled)
        });
        // A read that takes seconds is the byte plane, not the analyzer; a read
        // that never returns does not reach this line at all, which is the
        // distinction worth having in a log.
        tracing::trace!(
            offset,
            ok = outcome.is_ok(),
            seconds = started.elapsed().as_secs_f64(),
            "lease read: answered"
        );
        if started.elapsed() > std::time::Duration::from_secs(5) {
            tracing::warn!(
                offset,
                len,
                seconds = started.elapsed().as_secs_f64(),
                ok = outcome.is_ok(),
                "slow lease read"
            );
        }
        outcome
    }
}

type LocalResolver = Arc<dyn Fn(&str, &str, &str) -> Result<PathBuf> + Send + Sync>;
type LocalActivity = Arc<dyn Fn(&str) -> Box<dyn Send + Sync> + Send + Sync>;
type LocalPlayback = Arc<dyn Fn() -> Box<dyn Send + Sync> + Send + Sync>;
type LocalBackground = Arc<dyn Fn(&str) -> LocalAdmission + Send + Sync>;

/// Every open lease on the hub, plus the all-in-one short-circuit.
#[derive(Default)]
pub struct ByteSources {
    pub leases: Leases,
    /// AR-5: the in-process mediahost, if any — (module_id, path
    /// resolver). Its leases are direct file reads, no OpenRead.
    local_source: Mutex<Option<(String, LocalResolver)>>,
    /// Interactive storage admission around individual local read operations.
    local_activity: Mutex<Option<LocalActivity>>,
    /// CPU reservation held throughout an in-process viewer lease.
    local_playback: Mutex<Option<LocalPlayback>>,
    /// Scheduler admission for non-interactive all-in-one reads.
    local_background: Mutex<Option<LocalBackground>>,
}

impl ByteSources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this module's byte plane short-circuited to local reads
    /// (AR-11, all-in-one)? Burn-in's index walk needs it.
    pub fn reads_locally(&self, module_id: &str) -> bool {
        self.local_source
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(m, _)| m == module_id)
    }

    /// AR-5: register the in-process mediahost — leases for its files
    /// bypass OpenRead entirely and read the disk directly.
    pub fn set_local_source(
        &self,
        module_id: &str,
        resolve: impl Fn(&str, &str, &str) -> Result<PathBuf> + Send + Sync + 'static,
    ) {
        *self.local_source.lock().unwrap() = Some((module_id.to_string(), Arc::new(resolve)));
    }

    pub fn set_local_activity(
        &self,
        enter: impl Fn(&str) -> Box<dyn Send + Sync> + Send + Sync + 'static,
    ) {
        *self.local_activity.lock().unwrap() = Some(Arc::new(enter));
    }

    pub fn set_local_background(
        &self,
        admit: impl Fn(&str) -> LocalAdmission + Send + Sync + 'static,
    ) {
        *self.local_background.lock().unwrap() = Some(Arc::new(admit));
    }

    pub fn set_local_playback(
        &self,
        enter: impl Fn() -> Box<dyn Send + Sync> + Send + Sync + 'static,
    ) {
        *self.local_playback.lock().unwrap() = Some(Arc::new(enter));
    }

    /// Open a read lease on an arbitrary path within a collection (also
    /// used for sidecar subtitle files, which are not `files` rows).
    pub(crate) async fn open_lease(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        root_token: &str,
        path_rel: &str,
        reader: Reader,
    ) -> Result<Lease> {
        // AR-5/AR-11: the in-process mediahost's byte plane is a
        // function call — resolve the path and read the disk directly.
        let local = {
            let guard = self.local_source.lock().unwrap();
            guard
                .as_ref()
                .and_then(|(id, resolve)| (id == module_id).then(|| resolve.clone()))
        };
        if let Some(resolve) = local {
            let playback = (reader == Reader::Viewer)
                .then(|| {
                    self.local_playback
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|enter| enter())
                })
                .flatten();
            let foreground_admission = (reader == Reader::Viewer)
                .then(|| {
                    self.local_activity.lock().unwrap().as_ref().map(|enter| {
                        let enter = enter.clone();
                        let root_token = root_token.to_string();
                        Arc::new(move || {
                            let guard = enter(&root_token);
                            Box::pin(async move { Ok(guard) })
                                as std::pin::Pin<
                                    Box<
                                        dyn std::future::Future<
                                                Output = Result<Box<dyn Send + Sync>>,
                                            > + Send,
                                    >,
                                >
                        }) as LocalAdmission
                    })
                })
                .flatten();
            let background_admission = (reader == Reader::Sweep)
                .then(|| {
                    self.local_background
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|admit| admit(root_token))
                })
                .flatten();
            let admission = foreground_admission.or(background_admission);
            let resolution_permit = match &admission {
                Some(admit) => Some(admit().await?),
                None => None,
            };
            let collection_id = collection_id.to_string();
            let root_token_owned = root_token.to_string();
            let path_rel = path_rel.to_string();
            let path = tokio::task::spawn_blocking(move || {
                resolve(&collection_id, &root_token_owned, &path_rel)
            })
            .await
            .context("local media path resolution task failed")?;
            drop(resolution_permit);
            return Ok(Lease::local_guarded(path?, admission, playback));
        }
        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id: collection_id.to_string(),
                source: Some(kahawai_proto::v1::SourcePath {
                    root_token: root_token.to_string(),
                    path_rel: path_rel.to_string(),
                }),
                background: reader == Reader::Sweep,
            })),
        };
        // A send failure here means the host went away between being judged
        // present and being asked for bytes — a window no ordering can close,
        // because candidate selection and this call are separated by DB work.
        // Left as a plain error it reached `session_refusal` as 409, "give up
        // on this item", for a source that is merely offline; the recovery
        // contract's answer to an absent host is 503 and stand by.
        self.leases
            .establish(&token, registry.send_to_host(module_id, msg))
            .await
            .map_err(|e| {
                if registry.is_connected(module_id) {
                    e
                } else {
                    anyhow::Error::new(SourceOffline).context(format!("{e:#}"))
                }
            })
    }
}

#[cfg(test)]
mod lease_purpose_tests {
    use super::{ByteSources, Reader};

    /// The mediahost schedules its own local work — hashes, declarations,
    /// probes, extractions — around whether it is serving somebody. It
    /// cannot tell a sweep from a viewer by looking at the bytes, so the
    /// lease has to say, and this is the only place that says it.
    async fn opened_as(reader: Reader) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        registry.register_link(
            "01MH",
            tx,
            kahawai_proto::PROTOCOL_MINOR,
            kahawai_core::segments::DETECTOR_GENERATION,
        );

        let bytes = std::sync::Arc::new(ByteSources::new());
        // Nobody answers the OpenRead, so the lease never establishes; the
        // message is on the wire either way, which is the whole subject.
        let opening = tokio::spawn(async move {
            let _ = bytes
                .open_lease(&registry, "01MH", "c", "r", "e.mkv", reader)
                .await;
        });
        let sent = rx.recv().await.expect("an OpenRead reaches the host");
        opening.abort();
        match sent.unwrap().msg {
            Some(kahawai_proto::v1::hub_to_host::Msg::OpenRead(open)) => open.background,
            other => panic!("expected an OpenRead, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sweeps_lease_says_so() {
        assert!(opened_as(Reader::Sweep).await);
    }

    #[tokio::test]
    async fn and_a_viewers_does_not() {
        // The default reading of a missing field, so a hub too old to say
        // is taken as a viewer — the safe way round.
        assert!(!opened_as(Reader::Viewer).await);
    }

    #[tokio::test]
    async fn a_local_viewer_holds_cpu_for_the_lease_and_storage_only_for_operations() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("episode.mkv");
        std::fs::write(&source, b"bytes").unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let bytes = ByteSources::new();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entered_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cpu = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let playback_cpu = cpu.clone();
        bytes.set_local_playback(move || {
            playback_cpu.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(Guard(playback_cpu.clone()))
        });
        let resolving_cpu = cpu.clone();
        let resolving = active.clone();
        bytes.set_local_source("local", move |_, _, _| {
            assert_eq!(resolving.load(std::sync::atomic::Ordering::Relaxed), 1);
            assert_eq!(resolving_cpu.load(std::sync::atomic::Ordering::Relaxed), 1);
            Ok(source.clone())
        });
        let entered = active.clone();
        let counted = entered_count.clone();
        bytes.set_local_activity(move |_| {
            entered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(Guard(entered.clone()))
        });

        let lease = bytes
            .open_lease(&registry, "local", "c", "r", "episode.mkv", Reader::Viewer)
            .await
            .unwrap();
        let mut read_stream = lease.read_range(0, 5);
        let mut read = Vec::new();
        use tokio_stream::StreamExt as _;
        while let Some(chunk) = read_stream.next().await {
            read.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(read, b"bytes");
        assert!(
            entered_count.load(std::sync::atomic::Ordering::Relaxed) >= 4,
            "path resolution, open/metadata, seek and read must each be admitted"
        );
        assert_eq!(active.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(cpu.load(std::sync::atomic::Ordering::Relaxed), 1);
        let clone = lease.clone();
        drop(lease);
        assert_eq!(cpu.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(clone);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while cpu.load(std::sync::atomic::Ordering::Relaxed) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closing the local lease did not release CPU");
    }

    #[tokio::test]
    async fn a_local_sweep_is_admitted_before_path_resolution() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("episode.mkv");
        std::fs::write(&source, b"bytes").unwrap();
        let db = crate::db::open(dir.path()).await.unwrap();
        let registry = crate::registry::Registry::new(
            db,
            Default::default(),
            kahawai_mediadb::Store::in_memory().await.unwrap(),
        );
        let bytes = ByteSources::new();
        bytes.set_local_playback(|| panic!("background reads must not reserve playback CPU"));
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resolving = active.clone();
        bytes.set_local_source("local", move |_, _, _| {
            assert_eq!(resolving.load(std::sync::atomic::Ordering::Relaxed), 1);
            Ok(source.clone())
        });
        let admitted = active.clone();
        bytes.set_local_background(move |_| {
            let admitted = admitted.clone();
            std::sync::Arc::new(move || {
                let admitted = admitted.clone();
                Box::pin(async move {
                    admitted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(Box::new(Guard(admitted)) as Box<dyn Send + Sync>)
                })
            })
        });

        let lease = bytes
            .open_lease(&registry, "local", "c", "r", "episode.mkv", Reader::Sweep)
            .await
            .unwrap();
        drop(lease);
    }
}
