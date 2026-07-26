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
    Remux { dir: PathBuf, runner: Mutex<RemuxRunner> },
    /// Dispatched to a transcoder module; artifacts proxied on demand.
    /// The module can change: AR-6 reschedules onto a new box.
    Transcode { transcoder: Mutex<String> },
}

/// How a remux/transcode pipeline runs. The hub always spawns the
/// supervised worker (§1.1: a GStreamer crash kills one session, not the
/// process — observed twice on a real library via hlssink3 panics).
/// ponytail: in-process kept for tests, which have no worker binary.
pub enum RemuxRunner {
    InProcess(Arc<kahawai_media::remux::RemuxJob>),
    Worker(Mutex<tokio::process::Child>),
    /// Placeholder while a seek-restart swaps runners.
    Stopped,
}

impl RemuxRunner {
    fn stop(&self) {
        match self {
            RemuxRunner::InProcess(job) => job.stop(),
            RemuxRunner::Worker(child) => {
                let _ = child.lock().unwrap().start_kill();
            }
            RemuxRunner::Stopped => {}
        }
    }

    /// Stop and wait until the pipeline is really gone — a seek-restart
    /// recreates the scratch dir, and a still-dying worker writing into
    /// it corrupts the new run.
    async fn stop_and_wait(&self) {
        self.stop();
        if let RemuxRunner::Worker(child) = self {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if matches!(child.lock().unwrap().try_wait(), Ok(Some(_)) | Err(_)) {
                    return;
                }
                if std::time::Instant::now() > deadline {
                    tracing::warn!("old worker did not exit in time; proceeding");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

/// One file of a (possibly multi-part) source, with its absolute
/// timeline offset. CD1/CD2-era rips: parts play as one continuous
/// timeline; part boundaries are ordinary seek-restarts.
#[derive(Debug, Clone)]
pub struct PartSource {
    pub module_id: String,
    pub collection_id: String,
    pub path_rel: String,
    pub size: u64,
    pub base_ms: u64,
    pub duration_ms: u64,
}

pub struct Session {
    pub id: String,
    pub user_id: String,
    pub item_id: String,
    pub module_id: String,
    pub size: u64,
    /// All parts in timeline order (len 1 for single-file sources).
    pub parts: Vec<PartSource>,
    pub current_part: std::sync::atomic::AtomicUsize,
    pub container: Option<String>,
    pub duration_ms: Option<u64>,
    pub mode: Mode,
    /// Per-kind stream verdict (remux sessions): what happened to video
    /// and audio, for the player's playback-info overlay.
    pub verdict: Option<(String, String)>,
    /// The negotiated plan (remux/transcode) — reused on seek-restarts;
    /// mutable because audio-track switches re-plan (HUB-27).
    plan: Mutex<Option<kahawai_media::remux::RemuxPlan>>,
    /// Placement requirements — reused when rescheduling (AR-6).
    needs: crate::registry::PlacementNeed,
    touched: Mutex<std::time::Instant>,
}

/// Index of the part containing `abs_ms`.
fn part_index(parts: &[PartSource], abs_ms: u64) -> usize {
    parts
        .iter()
        .rposition(|p| abs_ms >= p.base_ms)
        .unwrap_or(0)
}

impl Session {
    /// Timeline base of the part currently playing (0 for single-file).
    pub fn part_base_ms(&self) -> u64 {
        self.parts
            .get(self.current_part.load(std::sync::atomic::Ordering::SeqCst))
            .map(|p| p.base_ms)
            .unwrap_or(0)
    }

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
pub(crate) struct LeaseSource {
    pub(crate) lease: Lease,
    pub(crate) size: u64,
    pub(crate) handle: tokio::runtime::Handle,
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

/// A playlist is client-ready with ≥3 segments (~10 s of runway) or an
/// ENDLIST (short source: whatever exists is all there is).
fn playlist_ready(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(p) => p.contains("#EXT-X-ENDLIST") || p.matches("#EXTINF").count() >= 3,
        Err(_) => false,
    }
}

type LocalResolver = std::sync::Arc<dyn Fn(&str, &str) -> Result<std::path::PathBuf> + Send + Sync>;

pub struct Sessions {
    pub leases: Leases,
    /// AR-5: the in-process mediahost, if any — (module_id, path
    /// resolver). Its leases are direct file reads, no OpenRead.
    local_source: Mutex<Option<(String, LocalResolver)>>,
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
    /// Hub-held source leases of dispatched sessions: (lease, size,
    /// part index) — reused across restarts within the same part so
    /// recovery works even when the mediahost link is flapping.
    /// Every part from the session's starting part onward, in timeline
    /// order: the transcoder joins them into one pipeline and asks for
    /// each by index. Second element is the starting part's index, so a
    /// seek that stays inside it can reuse these leases.
    tc_leases: Mutex<HashMap<String, (Vec<(Lease, u64)>, usize)>>,
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
            local_source: Mutex::new(None),
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

    /// Pick the best available source for an item and open a read lease
    /// on its mediahost. Used at session start and on seek-restarts of
    /// local remux sessions (whose lease died with the old worker).
    /// Single-part only (subtitle extraction and friends).
    pub(crate) async fn open_source(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<(String, String, u64, kahawai_core::media::MediaInfo, Lease)> {
        let (parts, info) = self.source_parts(registry, item_id).await?;
        let p = &parts[0];
        let lease =
            self.open_lease(registry, &p.module_id, &p.collection_id, &p.path_rel).await?;
        Ok((p.module_id.clone(), p.path_rel.clone(), p.size, info, lease))
    }

    /// All parts of the item's best available source in timeline order:
    /// a complete single-file source wins; otherwise the CD1/CD2-style
    /// part set (with cumulative timeline bases from per-part durations).
    pub(crate) async fn source_parts(
        &self,
        registry: &Registry,
        item_id: &str,
    ) -> Result<(Vec<PartSource>, kahawai_core::media::MediaInfo)> {
        let rows = sqlx::query(
            "SELECT s.module_id, s.collection_id, s.path_rel, s.part, f.size, f.streams_json
             FROM item_sources s
             JOIN files f ON (f.module_id, f.collection_id, f.path_rel)
                           = (s.module_id, s.collection_id, s.path_rel)
             WHERE s.item_id = ? ORDER BY s.part IS NOT NULL, f.size DESC",
        )
        .bind(item_id)
        .fetch_all(registry.db())
        .await?;
        if rows.is_empty() {
            bail!("no sources for item");
        }
        let parse_info = |r: &sqlx::sqlite::SqliteRow| -> kahawai_core::media::MediaInfo {
            serde_json::from_str(r.get::<String, _>("streams_json").as_str()).unwrap_or_default()
        };
        // Complete single file first (query put part-NULL rows up front).
        if let Some(r) = rows.iter().find(|r| {
            r.get::<Option<i64>, _>("part").is_none()
                && registry.is_connected(&r.get::<String, _>("module_id"))
        }) {
            let info = parse_info(r);
            return Ok((
                vec![PartSource {
                    module_id: r.get("module_id"),
                    collection_id: r.get("collection_id"),
                    path_rel: r.get("path_rel"),
                    size: r.get::<i64, _>("size") as u64,
                    base_ms: 0,
                    duration_ms: info.duration_ms.unwrap_or(0),
                }],
                info,
            ));
        }
        // Part set: connected parts, ordered, deduped by part number.
        let mut by_part: std::collections::BTreeMap<i64, &sqlx::sqlite::SqliteRow> =
            Default::default();
        for r in &rows {
            if let Some(part) = r.get::<Option<i64>, _>("part")
                && registry.is_connected(&r.get::<String, _>("module_id"))
            {
                by_part.entry(part).or_insert(r);
            }
        }
        if by_part.is_empty() {
            bail!("no source is currently available (mediahost offline)");
        }
        let mut parts = Vec::new();
        let mut base = 0u64;
        let mut first_info = None;
        for (_, r) in by_part {
            let info = parse_info(r);
            let dur = info
                .duration_ms
                .context("multi-part source with unknown part duration")?;
            parts.push(PartSource {
                module_id: r.get("module_id"),
                collection_id: r.get("collection_id"),
                path_rel: r.get("path_rel"),
                size: r.get::<i64, _>("size") as u64,
                base_ms: base,
                duration_ms: dur,
            });
            base += dur;
            first_info.get_or_insert(info);
        }
        Ok((parts, first_info.unwrap()))
    }

    /// Open a read lease on an arbitrary path within a collection (also
    /// used for sidecar subtitle files, which are not `files` rows).
    /// AR-5: register the in-process mediahost — leases for its files
    /// bypass OpenRead entirely and read the disk directly.
    pub fn set_local_source(
        &self,
        module_id: &str,
        resolve: impl Fn(&str, &str) -> Result<std::path::PathBuf> + Send + Sync + 'static,
    ) {
        *self.local_source.lock().unwrap() =
            Some((module_id.to_string(), std::sync::Arc::new(resolve)));
    }

    pub(crate) async fn open_lease(
        &self,
        registry: &Registry,
        module_id: &str,
        collection_id: &str,
        path_rel: &str,
    ) -> Result<Lease> {
        // AR-5/AR-11: the in-process mediahost's byte plane is a
        // function call — resolve the path and read the disk directly.
        let local = {
            let guard = self.local_source.lock().unwrap();
            guard.as_ref().and_then(|(id, resolve)| {
                (id == module_id).then(|| resolve(collection_id, path_rel))
            })
        };
        if let Some(path) = local {
            return Ok(Lease::local(path?));
        }
        let token = new_lease_token();
        let msg = HubToHost {
            msg: Some(hub_to_host::Msg::OpenRead(OpenRead {
                lease_token: token.clone(),
                collection_id: collection_id.to_string(),
                path_rel: path_rel.to_string(),
            })),
        };
        self.leases.establish(&token, registry.send_to_host(module_id, msg)).await
    }

    /// Start a session for an item: pick the best available source, open a
    /// read lease on its mediahost, and for remux start the in-hub pipeline
    /// (from `start_ms` when resuming into the middle of the file).
    pub async fn start(
        self: &Arc<Self>,
        registry: &Registry,
        user_id: &str,
        item_id: &str,
        mode: &str,
        start_ms: u64,
        audio_track: u32,
        video_track: u32,
    ) -> Result<Arc<Session>> {
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
        let (parts, info) = self.source_parts(registry, item_id).await?;
        if parts.len() > 1 && mode == "direct" {
            bail!("multi-part sources play via remux/transcode, not direct");
        }
        let total_ms: u64 = parts.iter().map(|p| p.duration_ms).sum();
        let start_idx = part_index(&parts, start_ms);
        let part = parts[start_idx].clone();
        let local_ms = start_ms.saturating_sub(part.base_ms);
        let (module_id, path_rel, size) =
            (part.module_id.clone(), part.path_rel.clone(), part.size);
        let lease =
            self.open_lease(registry, &part.module_id, &part.collection_id, &part.path_rel).await?;

        let id = ulid::Ulid::generate().to_string();
        let mut verdict = None;
        let mut session_plan = None;
        let mut session_needs = crate::registry::PlacementNeed::default();
        let session_mode = match mode {
            "direct" => Mode::Direct { lease },
            "remux" => {
                // The muxer stalls on unfed pads, so only claim what the
                // plan will actually feed — decided by the muxer's own
                // templates and the installed decoders/encoders (single
                // source of truth with the pipeline's link logic).
                // ponytail: every remux client gets the web target profile; real
                // per-client capability probes (HUB-14) select profiles later.
                let plan = kahawai_media::remux::plan_streams(
                    &info,
                    &kahawai_media::remux::WEB_TARGET,
                    audio_track as usize,
                    video_track as usize,
                );
                if !plan.playable() {
                    bail!("no playable streams — this source needs the video transcoder");
                }
                verdict = Some(kahawai_media::remux::plan_summary(&info, &plan));
                session_plan = Some(plan);
                use kahawai_media::remux::StreamMode;
                session_needs = crate::registry::PlacementNeed {
                    encode_video: plan.video == StreamMode::Encode,
                    encode_audio: plan.audio == StreamMode::Encode,
                    video_caps: kahawai_media::remux::source_caps_names("video", &info),
                    audio_caps: kahawai_media::remux::source_caps_names("audio", &info),
                };
                // Encode work goes to the fleet when one is available
                // (§4.5); pure remux — and encode with no fleet — stays
                // in the local supervised worker.
                let placed = if session_needs.encode_video || session_needs.encode_audio {
                    registry.pick_transcoder(&session_needs)
                } else {
                    None
                };
                match placed {
                    Some(tc) => {
                        // TC-6: one retry on the fallback HLS sink — two
                        // library files crash hlssink3 but mux fine on
                        // hlssink2 (upstream fix pending).
                        if let Err(first) = self
                            .start_transcode(
                                registry, &tc, &id, plan,
                                self.open_part_leases(registry, &parts, start_idx).await?,
                                start_idx, local_ms, "",
                            )
                            .await
                        {
                            tracing::warn!(session = %id, error = format!("{first:#}"),
                                "start failed; retrying with fallback sink");
                            self.start_transcode(
                                registry, &tc, &id, plan,
                                self.open_part_leases(registry, &parts, start_idx).await?,
                                start_idx, local_ms, "hlssink2",
                            )
                            .await
                            .with_context(|| format!("first attempt: {first:#}"))?;
                        }
                        Mode::Transcode { transcoder: Mutex::new(tc) }
                    }
                    None => {
                        let tail = self.open_part_leases(registry, &parts, start_idx).await?;
                        let runner = match self
                            .start_remux(&id, plan, tail, local_ms, "")
                            .await
                        {
                            Ok(r) => r,
                            Err(first) => {
                                tracing::warn!(session = %id, error = format!("{first:#}"),
                                    "start failed; retrying with fallback sink");
                                let tail =
                                    self.open_part_leases(registry, &parts, start_idx).await?;
                                self.start_remux(&id, plan, tail, local_ms, "hlssink2")
                                    .await
                                    .with_context(|| format!("first attempt: {first:#}"))?
                            }
                        };
                        Mode::Remux {
                            runner: Mutex::new(runner),
                            dir: self.scratch_root.join(&id),
                        }
                    }
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
            duration_ms: if parts.len() > 1 { Some(total_ms) } else { info.duration_ms },
            parts,
            current_part: std::sync::atomic::AtomicUsize::new(start_idx),
            mode: session_mode,
            verdict,
            plan: Mutex::new(session_plan),
            needs: session_needs,
            touched: Mutex::new(std::time::Instant::now()),
        });
        self.active.lock().unwrap().insert(session.id.clone(), session.clone());
        tracing::info!(session = %session.id, item = item_id, path = %path_rel, mode, "session started");
        registry.emit(serde_json::json!({ "kind": "sessions" }));
        Ok(session)
    }

    /// Leases for every part from `from` onward, in timeline order.
    ///
    /// A remux pipeline spans the rest of the source, so it needs them
    /// all up front: concat holds each later part blocked until the one
    /// before it ends, but the branch has to exist before that happens.
    /// Costs one lease per remaining part instead of one per session —
    /// paid once, at the start, rather than as a stall at every boundary.
    async fn open_part_leases(
        &self,
        registry: &Registry,
        parts: &[PartSource],
        from: usize,
    ) -> Result<Vec<(Lease, u64)>> {
        let mut out = Vec::with_capacity(parts.len().saturating_sub(from));
        for part in &parts[from..] {
            let lease = self
                .open_lease(registry, &part.module_id, &part.collection_id, &part.path_rel)
                .await?;
            out.push((lease, part.size));
        }
        Ok(out)
    }

    /// Spin up the remux/transcode pipeline — in a supervised worker
    /// process when configured — feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    async fn start_remux(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        parts: Vec<(Lease, u64)>,
        start_ms: u64,
        sink: &str,
    ) -> Result<RemuxRunner> {
        let dir = self.scratch_root.join(session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        anyhow::ensure!(!parts.is_empty(), "no source parts for the session");

        let runner = match &self.worker_exe {
            Some(exe) => {
                // ponytail: SUN_LEN caps unix socket paths at ~108
                // bytes — a pathologically deep data_dir breaks remux
                // here. Bind under a short tmp dir if it ever bites a
                // real deployment.
                // One socket per part: the worker joins them with concat
                // into a single pipeline, so a CD1->CD2 boundary is not a
                // restart. Part one keeps the historical name and the
                // positional argument; the rest arrive as --part.
                let mut socks = Vec::with_capacity(parts.len());
                for (n, (lease, size)) in parts.iter().enumerate() {
                    let sock = if n == 0 {
                        dir.join("worker.sock")
                    } else {
                        dir.join(format!("worker{n}.sock"))
                    };
                    let listener = tokio::net::UnixListener::bind(&sock)
                        .with_context(|| format!("binding {}", sock.display()))?;
                    // Serve source reads for the session's life; the task
                    // ends when the worker closes its socket.
                    let (lease, size) = (lease.clone(), *size);
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
                    socks.push((sock, size));
                }
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let mut cmd = tokio::process::Command::new(exe);
                cmd.arg("remux-worker")
                    .arg(&socks[0].0)
                    .arg(&dir)
                    .arg(socks[0].1.to_string());
                for (sock, size) in &socks[1..] {
                    cmd.args(["--part", &format!("{}:{size}", sock.display())]);
                }
                let child = cmd
                    .args(["--video", kahawai_media::worker::mode_arg(plan.video)])
                    .args(["--audio", kahawai_media::worker::mode_arg(plan.audio)])
                    .args(["--audio-track", &plan.audio_track.to_string()])
                    .args(["--video-track", &plan.video_track.to_string()])
                    .args(["--start-ms", &start_ms.to_string()])
                    .args(if sink.is_empty() { vec![] } else { vec!["--sink".into(), sink.to_string()] })
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
                let handle = tokio::runtime::Handle::current();
                let sources: Vec<Box<dyn kahawai_media::remux::RemuxSource>> = parts
                    .into_iter()
                    .map(|(lease, size)| {
                        Box::new(LeaseSource { lease, size, handle: handle.clone() })
                            as Box<dyn kahawai_media::remux::RemuxSource>
                    })
                    .collect();
                // start_at blocks while prerolling for an offset seek —
                // off the async runtime with it, or the preroll's own
                // lease reads can never be driven (single-thread runtimes
                // deadlock outright).
                let dir2 = dir.clone();
                let sink_owned = (!sink.is_empty()).then(|| sink.to_string());
                let job = tokio::task::spawn_blocking(move || {
                    kahawai_media::remux::start_parts(
                        &dir2,
                        plan,
                        sources,
                        start_ms,
                        sink_owned.as_deref(),
                        None,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("worker task panicked: {e}"))??;
                RemuxRunner::InProcess(Arc::new(job))
            }
        };

        // Return once the playlist has enough runway (or ended): a
        // playlist handed over with one ~3 s segment guarantees a stall
        // right after it — hls.js only discovers more segments on its
        // next live reload (~target duration later).
        let playlist = dir.join("master.m3u8");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
                RemuxRunner::Stopped => unreachable!("start_remux never yields Stopped"),
            }
            if playlist_ready(&playlist) {
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
    #[allow(clippy::too_many_arguments)] // private plumbing, one call site per mode
    async fn start_transcode(
        &self,
        registry: &Registry,
        transcoder: &str,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        parts: Vec<(Lease, u64)>,
        part_idx: usize,
        start_ms: u64,
        sink: &str,
    ) -> Result<()> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        anyhow::ensure!(!parts.is_empty(), "no source parts to dispatch");
        let size = parts[0].1;
        let tail_sizes: Vec<u64> = parts[1..].iter().map(|(_, n)| *n).collect();
        self.tc_leases.lock().unwrap().insert(session_id.to_string(), (parts, part_idx));
        self.pending_ready.lock().unwrap().insert(session_id.to_string(), ready_tx);

        let start = kahawai_proto::v1::HubToTc {
            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::StartSession(
                kahawai_proto::v1::StartSession {
                    session_id: session_id.to_string(),
                    size,
                    video: kahawai_media::worker::mode_arg(plan.video).into(),
                    audio: kahawai_media::worker::mode_arg(plan.audio).into(),
                    audio_track: plan.audio_track as u32,
                    video_track: plan.video_track as u32,
                    start_ms,
                    sink: sink.into(),
                    tail_sizes,
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

    /// Pacing (§4.6): forward the viewer's position to wherever the
    /// session's worker runs. Fire-and-forget — a missed update only
    /// delays a pause/resume by one ping.
    pub fn viewer_position(self: &Arc<Self>, registry: &Arc<Registry>, id: &str, position_ms: u64) {
        let Some(session) = self.get(id) else { return };
        match &session.mode {
            Mode::Remux { dir, .. } => {
                let _ = std::fs::write(dir.join("viewer.pos"), position_ms.to_string());
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let registry = registry.clone();
                let sid = id.to_string();
                tokio::spawn(async move {
                    let _ = registry
                        .send_to_tc(
                            &tc,
                            kahawai_proto::v1::HubToTc {
                                msg: Some(kahawai_proto::v1::hub_to_tc::Msg::ViewerPosition(
                                    kahawai_proto::v1::ViewerPosition {
                                        session_id: sid,
                                        position_ms,
                                    },
                                )),
                            },
                        )
                        .await;
                });
            }
            Mode::Direct { .. } => {}
        }
    }

    /// Link-facing: the transcoder reported the session ready or failed.
    /// Returns whether a pending start consumed the verdict (false → the
    /// session was already running; the caller may reschedule).
    pub fn transcode_verdict(&self, session_id: &str, result: Result<(), String>) -> bool {
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
            tracing::debug!(session = session_id, part, "source read for unknown session/part");
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
                    req,
                    part,
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
        let transcoder = transcoder.lock().unwrap().clone();
        let transcoder = transcoder.as_str();
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

    /// Seek-restart (§6): tear the session's pipeline down and start it
    /// again at `position_ms` (keyframe-snapped by the demuxer). Same
    /// session id, same URLs — the client re-attaches to a playlist that
    /// now begins at the offset.
    /// Returns the timeline base of the part the restart landed in
    /// (players add it to the pipeline's local start.pos).
    pub async fn seek(
        self: &Arc<Self>,
        registry: &Registry,
        id: &str,
        position_ms: u64,
        audio_track: Option<u32>,
        video_track: Option<u32>,
    ) -> Result<u64> {
        let session = self.get(id).context("no such session")?;
        let mut plan =
            (*session.plan.lock().unwrap()).context("session has no restartable pipeline")?;
        session.touch();
        let want_audio = audio_track.map(|t| t as usize).unwrap_or(plan.audio_track);
        let want_video = video_track.map(|t| t as usize).unwrap_or(plan.video_track);
        if want_audio != plan.audio_track || want_video != plan.video_track {
            // Switching tracks re-plans: the new track's codec decides
            // copy vs encode, not the old one's.
            let (_, _, _, info) =
                crate::subtitles::source_row(registry, &session.item_id).await?;
            plan = kahawai_media::remux::plan_streams(
                &info,
                &kahawai_media::remux::WEB_TARGET,
                want_audio,
                want_video,
            );
            anyhow::ensure!(plan.playable(), "selected track is not playable");
            *session.plan.lock().unwrap() = Some(plan);
        }
        // Map the absolute position onto the right part (single-part
        // sessions: part 0, local == absolute).
        let idx = part_index(&session.parts, position_ms);
        let part = session.parts.get(idx).context("session has no parts")?.clone();
        let local_ms = position_ms.saturating_sub(part.base_ms);
        session.current_part.store(idx, std::sync::atomic::Ordering::SeqCst);
        match &session.mode {
            Mode::Remux { dir, runner } => {
                let old =
                    std::mem::replace(&mut *runner.lock().unwrap(), RemuxRunner::Stopped);
                old.stop_and_wait().await;
                let _ = std::fs::remove_dir_all(dir);
                // The old worker's lease died with it; open a fresh one
                // on whichever part the target lands in.
                // A seek restarts in the target part and spans the rest
                // from there: concat cannot serve the seek itself (it
                // accepts one and then plays from zero — measured), so
                // the restart stays, but it only ever happens for a seek
                // now, never for a boundary.
                let tail = self.open_part_leases(registry, &session.parts, idx).await?;
                let fresh = self.start_remux(&session.id, plan, tail, local_ms, "").await?;
                *runner.lock().unwrap() = fresh;
                Ok(part.base_ms)
            }
            Mode::Transcode { transcoder } => {
                let tc = transcoder.lock().unwrap().clone();
                let _ = registry
                    .send_to_tc(
                        &tc,
                        kahawai_proto::v1::HubToTc {
                            msg: Some(kahawai_proto::v1::hub_to_tc::Msg::EndSession(
                                kahawai_proto::v1::EndSession { session_id: session.id.clone() },
                            )),
                        },
                    )
                    .await;
                registry.tc_session_ended(&tc);
                // The hub-held lease survives restarts; reuse it while
                // the target stays inside the same part (works even when
                // the mediahost link is flapping). Crossing parts needs
                // a lease on the other file.
                let held = self.tc_leases.lock().unwrap().remove(&session.id);
                let parts = match held {
                    Some((parts, held_idx)) if held_idx == idx => parts,
                    _ => self.open_part_leases(registry, &session.parts, idx).await?,
                };
                self.start_transcode(
                    registry, &tc, &session.id, plan, parts, idx, local_ms, "",
                )
                .await?;
                Ok(part.base_ms)
            }
            Mode::Direct { .. } => bail!("direct sessions seek with range requests"),
        }
    }

    /// A transcoder vanished (link loss): reschedule its sessions onto
    /// the remaining fleet at the viewer's last position (AR-6); end the
    /// ones nobody can take.
    pub async fn reschedule_for_transcoder(
        self: &Arc<Self>,
        registry: &Registry,
        transcoder: &str,
    ) -> (usize, usize) {
        let ids: Vec<String> = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                matches!(&s.mode, Mode::Transcode { transcoder: t }
                    if *t.lock().unwrap() == transcoder)
            })
            .map(|s| s.id.clone())
            .collect();
        let (mut moved, mut ended) = (0, 0);
        for id in ids {
            match self.reschedule(registry, &id).await {
                Ok(new_tc) => {
                    tracing::info!(session = %id, from = transcoder, to = %new_tc, "session rescheduled");
                    moved += 1;
                }
                Err(e) => {
                    tracing::warn!(session = %id, error = format!("{e:#}"), "reschedule failed; ending");
                    self.end(&id);
                    ended += 1;
                }
            }
        }
        (moved, ended)
    }

    /// Re-dispatch one session (its transcoder died or its worker
    /// crashed) at the viewer's last reported position.
    pub async fn reschedule(self: &Arc<Self>, registry: &Registry, id: &str) -> Result<String> {
        let session = self.get(id).context("no such session")?;
        let plan = (*session.plan.lock().unwrap()).context("not a pipeline session")?;
        let Mode::Transcode { transcoder } = &session.mode else {
            bail!("not a dispatched session");
        };
        let old_tc = transcoder.lock().unwrap().clone();
        registry.tc_session_ended(&old_tc);
        let new_tc = registry
            .pick_transcoder(&session.needs)
            .context("no capable transcoder left")?;
        // Resume where the viewer was: the player posts progress every
        // 10 s, which is exactly the doc's start_offset for AR-6.
        let position_ms: i64 = sqlx::query_scalar(
            "SELECT position_ms FROM watch_state WHERE user_id = ? AND item_id = ?",
        )
        .bind(&session.user_id)
        .bind(&session.item_id)
        .fetch_optional(registry.db())
        .await?
        .unwrap_or(0);
        let idx = part_index(&session.parts, position_ms.max(0) as u64);
        let part = session.parts.get(idx).context("session has no parts")?.clone();
        let local_ms = (position_ms.max(0) as u64).saturating_sub(part.base_ms);
        session.current_part.store(idx, std::sync::atomic::Ordering::SeqCst);
        // Reuse the hub-held lease when the position is still in its
        // part — the mediahost may be unreachable during a fleet blip.
        let held = self.tc_leases.lock().unwrap().remove(id);
        let parts = match held {
            Some((parts, held_idx)) if held_idx == idx => parts,
            _ => self.open_part_leases(registry, &session.parts, idx).await?,
        };
        self.start_transcode(registry, &new_tc, id, plan, parts, idx, local_ms, "").await?;
        *transcoder.lock().unwrap() = new_tc.clone();
        Ok(new_tc)
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
        if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
            registry.emit(serde_json::json!({ "kind": "sessions" }));
        }
        match &session.mode {
            Mode::Remux { dir, runner } => {
                runner.lock().unwrap().stop();
                let _ = std::fs::remove_dir_all(dir);
            }
            Mode::Transcode { transcoder } => {
                let transcoder = transcoder.lock().unwrap().clone();
                self.tc_leases.lock().unwrap().remove(id);
                self.pending_ready.lock().unwrap().remove(id);
                if let Some(registry) = self.registry_for_teardown.lock().unwrap().clone() {
                    registry.tc_session_ended(&transcoder);
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

#[cfg(test)]
mod tests {
    use super::{part_index, PartSource};

    fn part(base_ms: u64, duration_ms: u64) -> PartSource {
        PartSource {
            module_id: "m".into(),
            collection_id: "c".into(),
            path_rel: format!("CD{}.avi", base_ms),
            size: 1,
            base_ms,
            duration_ms,
        }
    }

    /// The CD1→CD2 hand-off is this function and nothing else: when a
    /// part's playlist ends the client seeks to `end + 250 ms`, and which
    /// file that lands in is decided here. Boundaries measured against
    /// the real two-part rip in the library (part 2 based at 3_752_711).
    #[test]
    fn a_timestamp_lands_in_the_part_that_contains_it() {
        let parts = [part(0, 3_752_711), part(3_752_711, 3_758_967)];

        assert_eq!(part_index(&parts, 0), 0, "start of part one");
        assert_eq!(part_index(&parts, 3_752_710), 0, "last ms of part one");
        // base_ms is inclusive: the boundary itself is already part two,
        // which is why the client's `+250` cannot land back in part one.
        assert_eq!(part_index(&parts, 3_752_711), 1, "the boundary");
        assert_eq!(part_index(&parts, 3_752_961), 1, "end-of-part-one + 250");
        assert_eq!(part_index(&parts, 7_511_677), 1, "last ms of the film");
        // Past the end clamps to the final part rather than panicking:
        // a seek beyond the timeline is a UI rounding error, not a crash.
        assert_eq!(part_index(&parts, u64::MAX), 1, "past the end");

        // Single-file sources take the same path with one part.
        assert_eq!(part_index(&[part(0, 1_000)], 999), 0);
        assert_eq!(part_index(&[part(0, 1_000)], 10_000), 0);
        // No parts at all is not reachable today, but must not panic.
        assert_eq!(part_index(&[], 42), 0);
    }
}
