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
        let leases = self.open_part_leases(registry, parts, start_idx).await?;
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
        let leases = self.open_part_leases(registry, parts, start_idx).await?;
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
        // runway follows it (protocol 4.5).
        target_duration_secs: u32,
        parts: Vec<(Lease, u64)>,
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
            part_sizes: parts.iter().map(|(_, size)| *size).collect(),
            start_ms,
            sink: (!sink.is_empty()).then(|| sink.to_string()),
            burn_sets: (!burn_sets.is_empty())
                .then_some(kahawai_playback::job::Payload::Bytes(burn_sets)),
            burn_ass: (!burn_ass_file.is_empty())
                .then_some(kahawai_playback::job::Payload::Bytes(burn_ass_file)),
            target_duration_secs: Some(target_duration_secs),
        };
        let start = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::StartSession(
                job.to_start_session(session_id)?,
            )),
        };
        self.tc_leases
            .lock()
            .unwrap()
            .insert(session_id.to_string(), (parts, part_idx));
        self.pending_ready
            .lock()
            .unwrap()
            .insert(session_id.to_string(), ready_tx);
        let cleanup = |sessions: &Self| {
            sessions.tc_leases.lock().unwrap().remove(session_id);
            sessions.pending_ready.lock().unwrap().remove(session_id);
        };
        if let Err(e) = registry
            .send_to_tc_requiring(transcoder, start, loudness_protocol_feature(&plan))
            .await
        {
            cleanup(self);
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(40), ready_rx).await {
            Ok(Ok(Ok(facts))) => {
                // No increment here: the slot has been held since the
                // pick. Counting again would double it.
                tracing::info!(session = session_id, transcoder, "session dispatched");
                Ok(facts)
            }
            Ok(Ok(Err(e))) => {
                cleanup(self);
                bail!("transcoder rejected session: {e}");
            }
            Ok(Err(_)) | Err(_) => {
                cleanup(self);
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

    /// Link-facing: serve one source read for a dispatched session.
    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub async fn source_read(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        offset: u64,
        len: u64,
        req: u64,
        part: u32,
    ) {
        let held = self.tc_leases.lock().unwrap().get(session_id).cloned();
        let Some((lease, size)) = held.and_then(|(parts, _)| parts.get(part as usize).cloned())
        else {
            tracing::debug!(
                session = session_id,
                part,
                "source read for unknown session/part"
            );
            return;
        };
        let len = len.min(kahawai_media::worker::MAX_READ);
        let want = if offset >= size {
            0
        } else {
            len.min(size - offset)
        };
        let mut buf = Vec::with_capacity(want as usize);
        if want > 0 {
            let mut stream = lease.read_range(offset, want).into_inner();
            while (buf.len() as u64) < want {
                match stream.recv().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        tracing::warn!(session = session_id, error = %e, "lease read failed");
                        break;
                    }
                    None => break,
                }
            }
            buf.truncate(want as usize);
        }
        let msg = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::SourceData(
                kahawai_proto::v1::SourceData {
                    session_id: session_id.to_string(),
                    offset,
                    data: buf,
                    req,
                    part,
                },
            )),
        };
        if let Err(e) = registry.send_to_tc(transcoder, msg).await {
            tracing::debug!(
                session = session_id,
                error = format!("{e:#}"),
                "source data undeliverable"
            );
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
