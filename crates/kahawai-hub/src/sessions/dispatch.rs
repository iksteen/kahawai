use super::*;

impl Sessions {
    /// One dispatch attempt plus TC-6's single sink fallback, as one
    /// fallible unit — so the caller holding the box's reservation has
    /// exactly one place to give it back. Returns the preroll facts and
    /// the sink that actually worked.
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub(super) async fn dispatch_to(
        self: &Arc<Self>,
        registry: &Registry,
        tc: &str,
        id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        target_duration_secs: u32,
        parts: &[PartSource],
        start_idx: usize,
        local_ms: u64,
        sets_bytes: &[u8],
        ass_bytes: &[u8],
    ) -> Result<(Vec<kahawai_media::facts::Fact>, String)> {
        let leases = self.open_part_sources(registry, parts, start_idx).await?;
        let first = match self
            .start_transcode(
                registry,
                tc,
                id,
                plan,
                target_duration_secs,
                leases,
                start_idx,
                local_ms,
                "",
                sets_bytes.to_vec(),
                ass_bytes.to_vec(),
            )
            .await
        {
            Ok(f) => return Ok((f, String::new())),
            // fmp4 has no sink fallback.
            Err(e) if plan.segment_format != kahawai_media::remux::SegmentFormat::Ts => {
                return Err(e);
            }
            Err(e) => e,
        };
        // TC-6: one retry on the fallback HLS sink — two library files
        // crash hlssink3 but mux fine on hlssink2 (upstream fix pending).
        tracing::warn!(session = %id, error = format!("{first:#}"),
            "start failed; retrying with fallback sink");
        let leases = self.open_part_sources(registry, parts, start_idx).await?;
        let f = self
            .start_transcode(
                registry,
                tc,
                id,
                plan,
                target_duration_secs,
                leases,
                start_idx,
                local_ms,
                "hlssink2",
                sets_bytes.to_vec(),
                ass_bytes.to_vec(),
            )
            .await
            .with_context(|| format!("first attempt: {first:#}"))?;
        Ok((f, "hlssink2".into()))
    }

    /// Dispatch a session to a transcoder and wait for its playlist.
    #[allow(clippy::too_many_arguments)] // private plumbing, one call site per mode
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub(super) async fn start_transcode(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        // What the hub's playlist declares; the transcoder's readiness
        // runway follows it.
        target_duration_secs: u32,
        parts: Vec<Arc<dyn ByteSource>>,
        part_idx: usize,
        start_ms: u64,
        sink: &str,
        // HUB-32b: a dispatched worker can no more walk the source
        // index than the hub can, so the display sets ride along.
        burn_sets: Vec<u8>,
        // HUB-32a: and neither can it read the media's neighbourhood,
        // so a sidecar `.ass` rides along the same way. Empty for an
        // embedded burn, which comes off the demuxer's own pad.
        burn_ass_file: Vec<u8>,
    ) -> Result<Vec<kahawai_media::facts::Fact>> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        anyhow::ensure!(!parts.is_empty(), "no source parts to dispatch");
        // The job, spelled for the wire by the one codec that knows how
        // (`kahawai_playback::job`): part sizes, sink override, burn
        // payloads and the declared target duration.
        let job = kahawai_playback::job::Job {
            plan,
            start_ms,
            sink: (!sink.is_empty()).then(|| sink.to_string()),
            burn_sets: (!burn_sets.is_empty())
                .then_some(kahawai_playback::job::Payload::Bytes(burn_sets)),
            burn_ass: (!burn_ass_file.is_empty())
                .then_some(kahawai_playback::job::Payload::Bytes(burn_ass_file)),
            target_duration_secs: Some(target_duration_secs),
        };
        let (grants, descriptors) = GrantOwner::new(self.source_grants.clone(), transcoder, &parts);
        let message = kahawai_playback::job::Dispatch {
            job,
            sources: descriptors,
        }
        .to_start_session(session_id)?;
        let start = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::StartSession(message)),
        };
        self.pending_ready
            .lock()
            .unwrap()
            .insert(session_id.to_string(), ready_tx);
        let mut pending = PendingReady {
            wait: ready_rx,
            registry: &self.pending_ready,
            id: session_id,
        };
        registry.send_to_tc(transcoder, start).await?;
        match tokio::time::timeout(Duration::from_secs(40), &mut pending.wait).await {
            Ok(Ok(Ok(facts))) => {
                self.dispatched_sources.lock().unwrap().insert(
                    session_id.to_string(),
                    DispatchedSources {
                        parts,
                        part_idx,
                        _grants: grants,
                    },
                );
                // No increment here: the slot has been held since the
                // pick. Counting again would double it.
                tracing::info!(session = session_id, transcoder, "session dispatched");
                Ok(facts)
            }
            Ok(Ok(Err(e))) => {
                bail!("transcoder rejected session: {e}");
            }
            Ok(Err(_)) | Err(_) => {
                let _ = registry
                    .send_to_tc(
                        transcoder,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                kahawai_proto::v1::EndSession {
                                    session_id: session_id.to_string(),
                                },
                            )),
                        },
                    )
                    .await;
                bail!("transcoder produced no playlist in time");
            }
        }
    }

    /// Link-facing: the transcoder reported the session ready or failed.
    /// Returns whether a pending start consumed the verdict (false → the
    /// session was already running; the caller may reschedule).
    pub fn transcode_verdict(&self, session_id: &str, result: ReadyVerdict) -> bool {
        if let Some(tx) = self.pending_ready.lock().unwrap().remove(session_id) {
            let _ = tx.send(result);
            true
        } else {
            false
        }
    }

    /// Link-facing: a chunk of a requested artifact arrived.
    pub fn artifact_chunk(&self, data: kahawai_proto::v1::ArtifactData) {
        let key = (data.session_id.clone(), data.name.clone());
        let tx = self.artifact_waiting.lock().unwrap().get(&key).cloned();
        if let Some(tx) = tx {
            let _ = tx.try_send(data);
        }
    }

    /// Fetch one artifact (playlist/segment) from the session's
    /// transcoder. ponytail: no cache — playlist polls and one-shot
    /// segment fetches are cheap on a LAN; add LRU when profiling says.
    pub async fn fetch_artifact(
        &self,
        registry: &Registry,
        session: &Session,
        name: &str,
    ) -> Result<Vec<u8>> {
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a transcode session");
        };
        let transcoder = transcoder.lock().unwrap().clone();
        let transcoder = transcoder.as_str();
        let key = (session.id.clone(), name.to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        self.artifact_waiting
            .lock()
            .unwrap()
            .insert(key.clone(), tx);
        let cleanup = || {
            self.artifact_waiting.lock().unwrap().remove(&key);
        };
        let req = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::FetchArtifact(
                kahawai_proto::v1::FetchArtifact {
                    session_id: session.id.clone(),
                    name: name.to_string(),
                },
            )),
        };
        if let Err(e) = registry.send_to_tc(transcoder, req).await {
            cleanup();
            return Err(e);
        }
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(chunk)) => {
                    if !chunk.error.is_empty() {
                        cleanup();
                        bail!("{}", chunk.error);
                    }
                    out.extend_from_slice(&chunk.data);
                    if chunk.eof {
                        cleanup();
                        return Ok(out);
                    }
                }
                Ok(None) | Err(_) => {
                    cleanup();
                    bail!("artifact fetch timed out");
                }
            }
        }
    }
}

/// A cancelled startup removes its own readiness waiter, without touching a
/// newer waiter installed under the same session ID.
struct PendingReady<'a> {
    wait: tokio::sync::oneshot::Receiver<ReadyVerdict>,
    registry: &'a Mutex<HashMap<String, tokio::sync::oneshot::Sender<ReadyVerdict>>>,
    id: &'a str,
}
impl Drop for PendingReady<'_> {
    fn drop(&mut self) {
        self.wait.close();
        let mut pending = self.registry.lock().unwrap();
        if pending
            .get(self.id)
            .is_some_and(|sender| sender.is_closed())
        {
            pending.remove(self.id);
        }
    }
}

/// Lifetime of one run/part capability. Dropping the sender stops an already
/// bound channel too; consuming a token alone would not revoke active readers.
pub(super) struct SourceGrant {
    peer: String,
    source: Arc<dyn ByteSource>,
    claimed: Arc<std::sync::atomic::AtomicBool>,
    alive: tokio::sync::watch::Sender<()>,
}
/// Owns exactly one run's grants, including while its start is pending.
/// Dropping it revokes active readers without touching a replacement run.
pub(super) struct GrantOwner {
    registry: Arc<Mutex<HashMap<String, SourceGrant>>>,
    tokens: Vec<String>,
}
impl GrantOwner {
    fn new(
        registry: Arc<Mutex<HashMap<String, SourceGrant>>>,
        peer: &str,
        parts: &[Arc<dyn ByteSource>],
    ) -> (Self, Vec<kahawai_proto::v1::SourceDescriptor>) {
        let mut owner = Self {
            registry,
            tokens: Vec::with_capacity(parts.len()),
        };
        let mut descriptors = Vec::with_capacity(parts.len());
        {
            let mut grants = owner.registry.lock().unwrap();
            for source in parts {
                let token = crate::leases::new_lease_token();
                let (alive, _) = tokio::sync::watch::channel(());
                grants.insert(
                    token.clone(),
                    SourceGrant {
                        peer: peer.into(),
                        source: source.clone(),
                        claimed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                        alive,
                    },
                );
                owner.tokens.push(token.clone());
                descriptors.push(kahawai_proto::v1::SourceDescriptor {
                    size: source.size(),
                    source_token: token,
                });
            }
        }
        (owner, descriptors)
    }
}
impl Drop for GrantOwner {
    fn drop(&mut self) {
        let mut grants = self.registry.lock().unwrap();
        for token in &self.tokens {
            grants.remove(token);
        }
    }
}
/// Only the live byte channel owns this claim. A disconnected channel may
/// release it for the same peer; revocation still removes the grant entirely.
pub(crate) struct SourceClaim(Arc<std::sync::atomic::AtomicBool>);
impl Drop for SourceClaim {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}
type ClaimedSource = (
    Arc<dyn ByteSource>,
    tokio::sync::watch::Receiver<()>,
    SourceClaim,
);

impl Sessions {
    pub(crate) fn claim_source(
        &self,
        token: &str,
        peer: &str,
    ) -> Result<ClaimedSource, tonic::Status> {
        let mut grants = self.source_grants.lock().unwrap();
        let grant = grants
            .get_mut(token)
            .filter(|g| g.peer == peer)
            .ok_or_else(|| tonic::Status::permission_denied("unknown or foreign source token"))?;
        grant
            .claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| tonic::Status::already_exists("source channel still active"))?;
        Ok((
            grant.source.clone(),
            grant.alive.subscribe(),
            SourceClaim(grant.claimed.clone()),
        ))
    }
}

#[cfg(test)]
mod source_token_tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_start_revokes_only_its_grants_and_preserves_reused_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, b"example").unwrap();
        let sessions = Sessions::new(dir.path().join("runs"));
        let source: Arc<dyn ByteSource> =
            Arc::new(crate::leases::LeaseTransport::local_guarded(path, None, None).buffered(7));
        let (pending, old) = GrantOwner::new(
            sessions.source_grants.clone(),
            "tc",
            std::slice::from_ref(&source),
        );
        let (replacement, new) = GrantOwner::new(sessions.source_grants.clone(), "tc", &[source]);
        let (_, mut live, claim) = sessions.claim_source(&old[0].source_token, "tc").unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let start = tokio::spawn(async move {
            let _pending = pending;
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        start.abort();
        assert!(start.await.unwrap_err().is_cancelled());
        assert!(live.changed().await.is_err());
        drop(claim);
        assert!(sessions.claim_source(&old[0].source_token, "tc").is_err());
        let (source, mut live, claim) = sessions.claim_source(&new[0].source_token, "tc").unwrap();
        assert_eq!(source.read(0, 7).await.unwrap(), b"example");
        drop(replacement);
        assert!(live.changed().await.is_err());
        drop(claim);
        assert!(sessions.source_grants.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_capability_rebinds_only_after_release_and_never_after_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, b"example").unwrap();
        let sessions = Sessions::new(dir.path().join("runs"));
        let (alive, _) = tokio::sync::watch::channel(());
        sessions.source_grants.lock().unwrap().insert(
            "old-token".into(),
            SourceGrant {
                peer: "assigned-tc".into(),
                source: Arc::new(
                    crate::leases::LeaseTransport::local_guarded(path, None, None).buffered(7),
                ),
                claimed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                alive,
            },
        );
        assert!(sessions.claim_source("old-token", "other-tc").is_err());
        let (_, _, claim) = sessions.claim_source("old-token", "assigned-tc").unwrap();
        assert!(sessions.claim_source("old-token", "assigned-tc").is_err());
        drop(claim);
        assert!(sessions.claim_source("old-token", "other-tc").is_err());
        let (_, mut live, claim) = sessions.claim_source("old-token", "assigned-tc").unwrap();
        drop(GrantOwner {
            registry: sessions.source_grants.clone(),
            tokens: vec!["old-token".into()],
        });
        assert!(live.changed().await.is_err());
        drop(claim);
        assert!(sessions.claim_source("old-token", "assigned-tc").is_err());
    }
}
