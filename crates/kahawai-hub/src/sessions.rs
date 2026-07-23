//! Play sessions (HUB-18 minimal): direct play and in-hub remux (AR-10).
//!
//! ponytail: sessions are in-memory (lost on hub restart, clients reopen);
//! idle timeout and per-user concurrency limits land with HUB-18 proper.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use kahawai_proto::v1::{hub_to_host, HubToHost, OpenRead};
use sqlx::Row;

use crate::leases::{new_lease_token, Lease, Leases};
use crate::registry::Registry;

pub enum Mode {
    Direct { lease: Lease },
    Remux { dir: PathBuf, runner: RemuxRunner },
    /// Dispatched to a transcoder module; artifacts proxied on demand.
    Transcode { transcoder: String },
}

/// How a remux/transcode pipeline runs. The hub always spawns the
/// supervised worker (§1.1: a GStreamer crash kills one session, not the
/// process — observed twice on a real library via hlssink3 panics).
/// ponytail: in-process kept for tests, which have no worker binary.
pub enum RemuxRunner {
    InProcess(Arc<kahawai_media::remux::RemuxJob>),
    Worker(Mutex<tokio::process::Child>),
}

impl RemuxRunner {
    fn stop(&self) {
        match self {
            RemuxRunner::InProcess(job) => job.stop(),
            RemuxRunner::Worker(child) => {
                let _ = child.lock().unwrap().start_kill();
            }
        }
    }
}

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub module_id: String,
    pub size: u64,
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    pub mode: Mode,
    /// Per-kind stream verdict (remux sessions): what happened to video
    /// and audio, for the player's playback-info overlay.
    pub verdict: Option<(String, String)>,
    touched: Mutex<std::time::Instant>,
}

impl Session {
    /// Any client activity (stream chunks, playlist/segment fetches,
    /// progress pings) keeps the session alive (HUB-18).
    pub fn touch(&self) {
        *self.touched.lock().unwrap() = std::time::Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.touched.lock().unwrap().elapsed()
    }
}

/// Adapts a mediahost read lease to the remuxer's random-access source
/// trait; runs on the remux feeder thread, bridging into the runtime.
struct LeaseSource {
    lease: Lease,
    size: u64,
    handle: tokio::runtime::Handle,
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
        let _guard = self.handle.enter();
        let mut stream = self.lease.read_range(offset, len).into_inner();
        self.handle.block_on(async {
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
        })
    }
}

/// Serve RemuxSource reads to a worker over its Unix socket.
/// Wire format: see kahawai_media::worker.
async fn serve_reads(mut conn: tokio::net::UnixStream, lease: Lease, size: u64) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = [0u8; 16];
    loop {
        if conn.read_exact(&mut req).await.is_err() {
            return Ok(()); // worker closed the socket (EOS or teardown)
        }
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
        let len = u64::from_le_bytes(req[8..].try_into().unwrap())
            .min(kahawai_media::worker::MAX_READ);
        let want = if offset >= size { 0 } else { len.min(size - offset) };
        let mut buf = Vec::with_capacity(want as usize);
        if want > 0 {
            let mut stream = lease.read_range(offset, want).into_inner();
            while (buf.len() as u64) < want {
                match stream.recv().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => anyhow::bail!("lease read failed: {e}"),
                    None => break,
                }
            }
            buf.truncate(want as usize);
        }
        conn.write_all(&(buf.len() as u64).to_le_bytes()).await?;
        conn.write_all(&buf).await?;
    }
}

pub struct Sessions {
    pub leases: Leases,
    /// Scratch space for remux sessions (`<data_dir>/sessions`).
    scratch_root: PathBuf,
    max_per_user: usize,
    idle_timeout: Duration,
    /// The binary to spawn as the per-session pipeline worker (the hub
    /// passes its own executable; the worker is a hidden subcommand).
    /// None → pipelines run in-process (tests only).
    worker_exe: Option<PathBuf>,
    active: Mutex<HashMap<String, Arc<Session>>>,
    /// Source leases for dispatched sessions (the transcoder pulls bytes
    /// over its link; lives from dispatch to session end).
    tc_leases: Mutex<HashMap<String, (Lease, u64)>>,
    /// Sessions awaiting the transcoder's ready/error verdict.
    pending_ready: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Result<(), String>>>>,
    /// In-flight artifact fetches, keyed by (session, name).
    artifact_waiting:
        Mutex<HashMap<(String, String), tokio::sync::mpsc::Sender<kahawai_proto::v1::ArtifactData>>>,
    /// Registry handle for teardown messages from the sync `end()` path
    /// (set once at startup; None only in tests without dispatch).
    registry_for_teardown: Mutex<Option<Arc<Registry>>>,
}

impl Sessions {
    pub fn new(scratch_root: PathBuf) -> Self {
        Self::with_limits(scratch_root, 4, Duration::from_secs(90))
    }

    pub fn with_limits(scratch_root: PathBuf, max_per_user: usize, idle_timeout: Duration) -> Self {
        // Sessions never survive a restart; stale scratch is garbage.
        let _ = std::fs::remove_dir_all(&scratch_root);
        Self {
            leases: Leases::default(),
            scratch_root,
            max_per_user,
            idle_timeout,
            worker_exe: None,
            active: Mutex::new(HashMap::new()),
            tc_leases: Mutex::new(HashMap::new()),
            pending_ready: Mutex::new(HashMap::new()),
            artifact_waiting: Mutex::new(HashMap::new()),
            registry_for_teardown: Mutex::new(None),
        }
    }

    /// Give the sync teardown path a registry handle for EndSession
    /// notifications to transcoders.
    pub fn attach_registry(&self, registry: Arc<Registry>) {
        *self.registry_for_teardown.lock().unwrap() = Some(registry);
    }

    /// Run pipelines in a supervised child process (crash isolation).
    pub fn with_worker_exe(mut self, exe: Option<PathBuf>) -> Self {
        self.worker_exe = exe;
        self
    }

    /// Reap idle sessions (HUB-18: no fetch or progress ping → teardown).
    pub fn spawn_janitor(self: &Arc<Self>) {
        let sessions = self.clone();
        // Check at half the timeout (tests use tiny timeouts), capped at 15 s.
        let period = (sessions.idle_timeout / 2)
            .clamp(Duration::from_millis(50), Duration::from_secs(15));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let idle: Vec<String> = sessions
                    .active
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|s| s.idle_for() > sessions.idle_timeout)
                    .map(|s| s.id.clone())
                    .collect();
                for id in idle {
                    tracing::info!(session = %id, "ending idle session");
                    sessions.end(&id);
                }
            }
        });
    }

    /// Start a session for an item: pick the best available source, open a
    /// read lease on its mediahost, and for remux start the in-hub pipeline.
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        user_id: &str,
        item_id: &str,
        mode: &str,
    ) -> Result<Arc<Session>> {
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ? ORDER BY f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let user_active = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.user_id == user_id)
            .count();
        if user_active >= self.max_per_user {
            bail!("too many concurrent streams ({user_active}); close one first");
        }
        let source = rows
            .iter()
            .find(|r| registry.is_connected(&r.get::<String, _>("module_id")))
            .context("no source is currently available (mediahost offline)")?;

        let module_id: String = source.get("module_id");
        let collection_id: String = source.get("collection_id");
        let path_rel: String = source.get("path_rel");
        let size = source.get::<i64, _>("size") as u64;
        let info: kahawai_core::media::MediaInfo =
            serde_json::from_str(source.get::<String, _>("streams_json").as_str())
                .unwrap_or_default();

        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id,
                path_rel: path_rel.clone(),
            })),
        };
        let lease = self
            .leases
            .establish(&token, registry.send_to_host(&module_id, msg))
            .await?;

        let id = ulid::Ulid::new().to_string();
        let mut verdict = None;
        let session_mode = match mode {
            "direct" => Mode::Direct { lease },
            "remux" => {
                // The muxer stalls on unfed pads, so only claim what the
                // plan will actually feed — decided by the muxer's own
                // templates and the installed decoders/encoders (single
                // source of truth with the pipeline's link logic).
                // ponytail: every remux client gets the web target profile; real
                // per-client capability probes (HUB-14) select profiles later.
                let plan = kahawai_media::remux::plan_streams(&info, &kahawai_media::remux::WEB_TARGET);
                if !plan.playable() {
                    bail!("no playable streams — this source needs the video transcoder");
                }
                verdict = Some(kahawai_media::remux::plan_summary(&info, &plan));
                use kahawai_media::remux::StreamMode;
                let needs_encode =
                    plan.video == StreamMode::Encode || plan.audio == StreamMode::Encode;
                // Encode work goes to the fleet when one is available
                // (§4.5); pure remux — and encode with no fleet — stays
                // in the local supervised worker.
                let placed = if needs_encode {
                    registry.pick_transcoder(
                        plan.video == StreamMode::Encode,
                        plan.audio == StreamMode::Encode,
                    )
                } else {
                    None
                };
                match placed {
                    Some(tc) => {
                        self.start_transcode(registry, &tc, &id, plan, lease, size).await?;
                        Mode::Transcode { transcoder: tc }
                    }
                    None => Mode::Remux {
                        runner: self.start_remux(&id, plan, lease, size).await?,
                        dir: self.scratch_root.join(&id),
                    },
                }
            }
            other => bail!("unknown mode {other:?} (direct|remux)"),
        };

        let session = Arc::new(Session {
            id,
            user_id: user_id.to_string(),
            item_id: item_id.to_string(),
            module_id,
            size,
            container: info.container.clone(),
            duration_ms: info.duration_ms,
            mode: session_mode,
            verdict,
            touched: Mutex::new(std::time::Instant::now()),
        });
        self.active.lock().unwrap().insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        Ok(session)
    }

    /// Spin up the remux/transcode pipeline — in a supervised worker
    /// process when configured — feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    async fn start_remux(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        lease: Lease,
        size: u64,
    ) -> Result<RemuxRunner> {
        let dir = self.scratch_root.join(session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let runner = match &self.worker_exe {
            Some(exe) => {
                let sock = dir.join("worker.sock");
                let listener = tokio::net::UnixListener::bind(&sock)
                    .with_context(|| format!("binding {}", sock.display()))?;
                // Serve source reads to the worker for the session's life;
                // the task ends when the worker closes its socket.
                tokio::spawn(async move {
                    match listener.accept().await {
                        Ok((conn, _)) => {
                            if let Err(e) = serve_reads(conn, lease, size).await {
                                tracing::debug!(error = %e, "worker read channel closed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "worker never connected"),
                    }
                });
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let child = tokio::process::Command::new(exe)
                    .arg("remux-worker")
                    .arg(&sock)
                    .arg(&dir)
                    .arg(size.to_string())
                    .args(["--video", kahawai_media::worker::mode_arg(plan.video)])
                    .args(["--audio", kahawai_media::worker::mode_arg(plan.audio)])
                    .stderr(std::process::Stdio::from(log))
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning worker {}", exe.display()))?;
                tracing::info!(session = session_id, pid = child.id(), "pipeline worker spawned");
                RemuxRunner::Worker(Mutex::new(child))
            }
            None => {
                // In-process (tests): the pipeline pulls (and seeks —
                // MP4 moov-at-end needs it) from the lease via a blocking
                // adapter on the remux feeder thread.
                let source = LeaseSource { lease, size, handle: tokio::runtime::Handle::current() };
                RemuxRunner::InProcess(Arc::new(kahawai_media::remux::start(&dir, plan, Box::new(source))?))
            }
        };

        // Return once the playlist exists (or the pipeline died).
        let playlist = dir.join("master.m3u8");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match &runner {
                RemuxRunner::InProcess(job) => {
                    if let Some(e) = job.failed() {
                        bail!("remux failed to start: {e}");
                    }
                }
                RemuxRunner::Worker(child) => {
                    if let Some(status) = child.lock().unwrap().try_wait()? {
                        let log = std::fs::read_to_string(dir.join("worker.log")).unwrap_or_default();
                        let tail: String = log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                        bail!("pipeline worker exited at start ({status}): {tail}");
                    }
                }
            }
            if playlist.exists() {
                return Ok(runner);
            }
            if std::time::Instant::now() > deadline {
                runner.stop();
                bail!("remux produced no playlist in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Dispatch a session to a transcoder and wait for its playlist.
    async fn start_transcode(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        lease: Lease,
        size: u64,
    ) -> Result<()> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        self.tc_leases.lock().unwrap().insert(session_id.to_string(), (lease, size));
        self.pending_ready.lock().unwrap().insert(session_id.to_string(), ready_tx);

        let start = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::StartSession(
                kahawai_proto::v1::StartSession {
                    session_id: session_id.to_string(),
                    size,
                    video: kahawai_media::worker::mode_arg(plan.video).into(),
                    audio: kahawai_media::worker::mode_arg(plan.audio).into(),
                },
            )),
        };
        let cleanup = |sessions: &Self| {
            sessions.tc_leases.lock().unwrap().remove(session_id);
            sessions.pending_ready.lock().unwrap().remove(session_id);
        };
        if let Err(e) = registry.send_to_tc(transcoder, start).await {
            cleanup(self);
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(40), ready_rx).await {
            Ok(Ok(Ok(()))) => {
                registry.tc_session_started(transcoder);
                tracing::info!(session = session_id, transcoder, "session dispatched");
                Ok(())
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
    pub fn transcode_verdict(&self, session_id: &str, result: Result<(), String>) {
        if let Some(tx) = self.pending_ready.lock().unwrap().remove(session_id) {
            let _ = tx.send(result);
        }
    }

    /// Link-facing: serve one source read for a dispatched session.
    pub async fn source_read(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        offset: u64,
        len: u64,
    ) {
        let Some((lease, size)) = self.tc_leases.lock().unwrap().get(session_id).cloned() else {
            tracing::debug!(session = session_id, "source read for unknown session");
            return;
        };
        let len = len.min(kahawai_media::worker::MAX_READ);
        let want = if offset >= size { 0 } else { len.min(size - offset) };
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
                },
            )),
        };
        if let Err(e) = registry.send_to_tc(transcoder, msg).await {
            tracing::debug!(session = session_id, error = format!("{e:#}"), "source data undeliverable");
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
        let key = (session.id.clone(), name.to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        self.artifact_waiting.lock().unwrap().insert(key.clone(), tx);
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

    /// End every session dispatched to a given transcoder (link loss —
    /// AR-6 minimal: sessions fail, clients restart; reschedule later).
    pub fn end_for_transcoder(&self, transcoder: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| matches!(&s.mode, Mode::Transcode { transcoder: t } if t == transcoder))
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id);
        }
        n
    }

    /// Active sessions for the admin dashboard (HUB-18).
    pub fn list(&self) -> Vec<Arc<Session>> {
        let mut v: Vec<_> = self.active.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// End every session backed by a given mediahost (satellite deletion).
    pub fn end_for_module(&self, module_id: &str) -> usize {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.module_id == module_id)
            .map(|s| s.id.clone())
            .collect();
        let n = ids.len();
        for id in ids {
            self.end(&id);
        }
        n
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.active.lock().unwrap().get(id).cloned()
    }

    /// Remove a session: direct leases drop (closing the byte channel);
    /// remux pipelines stop and their scratch dir is deleted.
    pub fn end(&self, id: &str) -> bool {
        let Some(session) = self.active.lock().unwrap().remove(id) else {
            return false;
        };
        match &session.mode {
            Mode::Remux { dir, runner } => {
                runner.stop();
                let _ = std::fs::remove_dir_all(dir);
            }
            Mode::Transcode { transcoder } => {
                self.tc_leases.lock().unwrap().remove(id);
                self.pending_ready.lock().unwrap().remove(id);
                if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
                    registry.tc_session_ended(transcoder);
                    let transcoder = transcoder.clone();
                    let sid = id.to_string();
                    tokio::spawn(async move {
                        let _ = registry
                            .send_to_tc(
                                &transcoder,
                                kahawai_proto::v1::HubToTc {
                                    msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                        kahawai_proto::v1::EndSession { session_id: sid },
                                    )),
                                },
                            )
                            .await;
                    });
                }
            }
            Mode::Direct { .. } => {}
        }
        tracing::info!(session = id, "session ended");
        true
    }
}
