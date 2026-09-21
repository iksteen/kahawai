use super::*;

use kahawai_playback::executor::{BoxFuture, ByteSource, Death, Started};
use kahawai_playback::job::{Job, Payload};

/// A mediahost read lease as the executor's byte source: the hub's
/// worker reads the same lease a direct-play client would stream.
pub(super) struct LeaseByteSource {
    pub(super) lease: Lease,
    pub(super) size: u64,
}

impl ByteSource for LeaseByteSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, offset: u64, len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(async move {
            let mut stream = self.lease.read_range(offset, len).into_inner();
            let mut buf = Vec::with_capacity(len as usize);
            while (buf.len() as u64) < len {
                match stream.recv().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        return Err(std::io::Error::other(format!("lease read failed: {e}")));
                    }
                    None => break,
                }
            }
            buf.truncate(len as usize);
            Ok(buf)
        })
    }
}

impl Sessions {
    /// ONE attempt at running a pipeline on the hub's own supervised
    /// worker over the given part leases. The TC-6 sink fallback is the
    /// caller's: it decides whether a second attempt is worth a second set
    /// of leases.
    ///
    /// A failed start stores its evidence itself (OPS-10): this session is
    /// NOT in `active` — registration happens after start succeeds —
    /// which is why `note_session` ran when the id was minted, and why the
    /// bundle stored here is the only trace it ever existed.
    #[allow(clippy::too_many_arguments)] // one call site per mode
    pub(super) async fn start_local(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        // What the playlist will DECLARE: the readiness gate hands over
        // enough runway for a client reloading at that cadence.
        target_duration_secs: u32,
        parts: Vec<(Lease, u64)>,
        start_ms: u64,
        sink: Option<&str>,
        // HUB-32b: display sets the mediahost walked for us.
        burn_sets: Option<&std::path::Path>,
        // HUB-32a: a sidecar `.ass` script to burn, as TEXT — the executor
        // writes it into the run directory. Embedded ASS needs nothing
        // here: it burns from the demuxer's own pad.
        burn_ass: Option<&str>,
    ) -> Result<Started> {
        let job = Job {
            plan,
            part_sizes: parts.iter().map(|(_, size)| *size).collect(),
            start_ms,
            sink: sink.map(str::to_string),
            burn_sets: burn_sets.map(|p| Payload::Path(p.to_path_buf())),
            burn_ass: burn_ass.map(|text| Payload::Bytes(text.as_bytes().to_vec())),
            target_duration_secs: Some(target_duration_secs),
        };
        let sources: Vec<Arc<dyn ByteSource>> = parts
            .into_iter()
            .map(|(lease, size)| Arc::new(LeaseByteSource { lease, size }) as Arc<dyn ByteSource>)
            .collect();
        match self.executor.start(session_id, job, sources).await {
            Ok(started) => Ok(started),
            Err(failure) => {
                if let Some(data_dir) = self.data_dir() {
                    let (item, header) = self.log_header(session_id);
                    crate::sessionlog::store(
                        data_dir,
                        &item,
                        session_id,
                        &format!("{header}{}", failure.bundle),
                    );
                }
                Err(failure.into_error())
            }
        }
    }

    /// Post-ready supervision of a local run: diagnostics only. A worker
    /// that dies mid-session leaves its bundle under the data dir and an
    /// error line on the session's header; the janitor ends the session
    /// when it goes idle, exactly as before. Nothing here reschedules —
    /// a local session has nowhere else to go.
    pub(super) fn watch_local_death(
        self: &Arc<Self>,
        session_id: &str,
        mut died: tokio::sync::watch::Receiver<Option<Death>>,
    ) {
        let sessions = self.clone();
        let id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                let death = died.borrow().clone();
                match death {
                    Some(Death::Finished) => return,
                    Some(Death::Crashed { error, .. }) => {
                        sessions.note_error(&id, &error);
                        let bundle = sessions.get(&id).and_then(|s| match &s.mode {
                            Mode::Remux { run } => run
                                .lock()
                                .unwrap()
                                .as_ref()
                                .map(|run| run.bundle("hub-local worker")),
                            _ => None,
                        });
                        if let (Some(bundle), Some(data_dir)) = (bundle, sessions.data_dir()) {
                            let (item, header) = sessions.log_header(&id);
                            crate::sessionlog::store(
                                data_dir,
                                &item,
                                &id,
                                &format!("{header}{bundle}"),
                            );
                        }
                        return;
                    }
                    None => {
                        if died.changed().await.is_err() {
                            return; // the run ended the orderly way
                        }
                    }
                }
            }
        });
    }

    /// HUB-36: fold one pace sample from the hub's own worker into what
    /// placement knows about `local`. Before the executor no local run's
    /// `pace.json` was ever read, so `predict_local` had nothing observed
    /// to go on and always fell back to the benchmark.
    pub(super) async fn fold_local_pace(&self, registry: &Registry, class: &str, multiple: f32) {
        if class.is_empty() || !multiple.is_finite() || multiple <= 0.0 {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match crate::pace::fold(
            registry.db(),
            crate::pace::LOCAL,
            class,
            multiple as f64,
            now,
        )
        .await
        {
            Ok(next) => registry.set_pace(crate::pace::LOCAL, class, next),
            Err(e) => tracing::warn!(class, error = format!("{e:#}"), "local pace fold failed"),
        }
    }

    /// Harvest the pace samples live local runs have written. Runs from
    /// the janitor tick; a run that ends between ticks folds its own last
    /// sample on the way out.
    pub(super) async fn harvest_local_pace(&self, registry: &Registry) {
        let samples: Vec<(String, f32)> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| !s.pace_class.is_empty())
            .filter_map(|s| match &s.mode {
                Mode::Remux { run } => run
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|run| run.take_pace_sample())
                    .map(|m| (s.pace_class.clone(), m)),
                _ => None,
            })
            .collect();
        for (class, multiple) in samples {
            self.fold_local_pace(registry, &class, multiple).await;
        }
    }
}
