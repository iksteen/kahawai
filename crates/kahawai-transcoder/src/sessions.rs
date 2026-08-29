//! Dispatched session execution (§6): each session runs the same
//! supervised worker the hub uses locally, fed over its Unix socket by a
//! bridge that pulls source bytes from the hub over the control link.
//! Artifacts (playlist/segments) are read from the session scratch dir
//! and streamed back on request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kahawai_proto::v1::{
    ArtifactData, SessionError, SessionFact, SessionReady, TcToHub, tc_to_hub,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

const ARTIFACT_CHUNK: usize = 256 * 1024;

enum Worker {
    Child(tokio::process::Child),
    /// Tests only: worker::run on a blocking thread (no binary to spawn).
    InProcess(tokio::task::JoinHandle<()>, Arc<Mutex<Option<String>>>),
}

struct Session {
    /// This run's scratch dir: `<scratch>/<session>/r<N>` — per-RUN, so
    /// a seek-restart's new pipeline never shares paths with the old
    /// run's drain (two hlssink instances on one dir corrupt/abort).
    dir: PathBuf,
    worker: Worker,
    /// Source-read bridge for this run; aborted on end so a replaced
    /// run stops pulling bytes promptly.
    bridge: tokio::task::JoinHandle<()>,
}

/// All state for one hub link's dispatched sessions.
pub struct Runner {
    scratch_root: PathBuf,
    /// None → run pipelines in-process (tests); Some → worker binary.
    worker_exe: Option<PathBuf>,
    link: mpsc::Sender<TcToHub>,
    sessions: Mutex<HashMap<String, Session>>,
    run_seq: std::sync::atomic::AtomicU64,
    /// In-flight source reads keyed by request id — NEVER by session:
    /// seek-restarts reuse the session id, and a stale response from the
    /// previous worker must not satisfy the new worker's read.
    pending_reads: Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>,
    next_req: std::sync::atomic::AtomicU64,
    /// Stderr of the most recently failed worker, handed to the hub
    /// with the SessionError so it outlives this scratch dir.
    last_worker_log: Mutex<Option<String>>,
    /// HUB-36 phase 4: pace samples harvested from finished runs,
    /// waiting for the next heartbeat tick to carry them.
    pending_pace: Mutex<Vec<kahawai_proto::v1::PaceSample>>,
    /// Bytes/sec the source plane sustains, EWMA over LARGE reads only
    /// (see `LINK_MIN_READ`). None until one is seen — a box that has
    /// only ever served small reads has no measured bandwidth, which is
    /// not the same as having none.
    link_rate: Mutex<Option<f64>>,
}

/// Reads below this measure latency, not bandwidth: the round trip
/// dominates and the resulting figure says more about the hub's event
/// loop than about the link.
const LINK_MIN_READ: usize = 1024 * 1024;

/// Link-rate EWMA weight. Lower than the hub's pace weight because a
/// single read races against whatever else shares the wire; the rate
/// should drift toward the sustained truth rather than chase spikes.
const LINK_ALPHA: f64 = 0.2;

impl Runner {
    pub fn new(
        scratch_root: PathBuf,
        worker_exe: Option<PathBuf>,
        link: mpsc::Sender<TcToHub>,
    ) -> Arc<Self> {
        // Sessions never survive a link; stale scratch is garbage.
        let _ = std::fs::remove_dir_all(&scratch_root);
        Arc::new(Self {
            scratch_root,
            worker_exe,
            link,
            sessions: Mutex::new(HashMap::new()),
            pending_reads: Mutex::new(HashMap::new()),
            next_req: std::sync::atomic::AtomicU64::new(1),
            run_seq: std::sync::atomic::AtomicU64::new(1),
            last_worker_log: Mutex::new(None),
            pending_pace: Mutex::new(Vec::new()),
            link_rate: Mutex::new(None),
        })
    }

    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    pub async fn start(
        self: &Arc<Self>,
        session_id: String,
        size: u64,
        video: &str,
        audio: &str,
        audio_track: u32,
        video_track: u32,
        start_ms: u64,
        sink: &str,
        tail_sizes: Vec<u64>,
        encode_params: (u32, u32, u32, bool, u32),
        // The source is interlaced; the encode chain deinterlaces.
        deinterlace: bool,
        loudness: (f64, f64, u32),
        // HUB-15b (video_codec, audio_codec, container); empty = legacy.
        targets: (String, String, String),
        burn_sets: Vec<u8>,
        // HUB-32a: (1 + the embedded e{n} to burn, or 0; a sidecar .ass)
        ass_burn: (u32, Vec<u8>),
    ) {
        let result = self
            .start_inner(
                &session_id,
                size,
                video,
                audio,
                audio_track,
                video_track,
                start_ms,
                sink,
                &tail_sizes,
                encode_params,
                deinterlace,
                loudness,
                targets,
                burn_sets,
                ass_burn,
            )
            .await;
        let msg = match result {
            Ok(dir) => {
                // What the worker learned during preroll (AR-13): read
                // once at ready and sent with it, so the hub can fold
                // the facts into the verdict it is about to answer with.
                let facts: Vec<SessionFact> = kahawai_media::facts::read(&dir)
                    .into_iter()
                    .map(|f| SessionFact {
                        kind: f.kind,
                        detail: f.detail,
                    })
                    .collect();
                tracing::info!(session = %session_id, facts = facts.len(), "session ready");
                TcToHub {
                    msg: Some(tc_to_hub::Msg::SessionReady(SessionReady {
                        session_id: session_id.clone(),
                        facts,
                    })),
                }
            }
            Err(e) => {
                tracing::warn!(session = %session_id, error = format!("{e:#}"), "session failed");
                self.end(&session_id).await;
                TcToHub {
                    msg: Some(tc_to_hub::Msg::SessionError(SessionError {
                        session_id: session_id.clone(),
                        error: format!("{e:#}"),
                        worker_log: self
                            .last_worker_log
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap_or_default(),
                    })),
                }
            }
        };
        let _ = self.link.send(msg).await;
    }

    #[allow(clippy::too_many_arguments)] // wire-shaped plumbing
    async fn start_inner(
        self: &Arc<Self>,
        session_id: &str,
        size: u64,
        video: &str,
        audio: &str,
        audio_track: u32,
        video_track: u32,
        start_ms: u64,
        sink: &str,
        tail_sizes: &[u64],
        (video_kbps, max_height, max_channels, tone_map, burn_subtitle): (u32, u32, u32, bool, u32),
        deinterlace: bool,
        (stereo_gain_db, native_gain_db, loudness_source_channels): (f64, f64, u32),
        (video_codec, audio_codec, container): (String, String, String),
        burn_sets: Vec<u8>,
        (burn_ass, burn_ass_file): (u32, Vec<u8>),
    ) -> Result<PathBuf> {
        // Replace any previous run first (seek-restart reuses the id).
        self.end(session_id).await;
        let run = self
            .run_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.scratch_root.join(session_id).join(format!("r{run}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        // One socket per part of a split source (CD1/CD2): the worker
        // joins them with concat into a single pipeline, so the boundary
        // is not a restart. Each bridge tags its reads with the part it
        // serves; the hub holds a lease per part.
        let mut socks: Vec<(std::path::PathBuf, u64)> = Vec::with_capacity(1 + tail_sizes.len());
        let mut bridges = Vec::with_capacity(1 + tail_sizes.len());
        for (part, part_size) in std::iter::once(size)
            .chain(tail_sizes.iter().copied())
            .enumerate()
        {
            let sock = if part == 0 {
                dir.join("worker.sock")
            } else {
                dir.join(format!("worker{part}.sock"))
            };
            let listener = tokio::net::UnixListener::bind(&sock)
                .with_context(|| format!("binding {}", sock.display()))?;
            // Bridge: worker's socket reads → SourceRead over the link →
            // SourceData fulfils the pending oneshot → back to the socket.
            let runner = self.clone();
            let sid = session_id.to_string();
            bridges.push(tokio::spawn(async move {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                if let Err(e) = runner.bridge_reads(conn, &sid, part as u32).await {
                    tracing::debug!(session = %sid, error = format!("{e:#}"), "read bridge closed");
                }
            }));
            socks.push((sock, part_size));
        }
        let sock = socks[0].0.clone();
        let bridge = bridges.remove(0);

        let worker = match &self.worker_exe {
            Some(exe) => {
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let mut cmd = tokio::process::Command::new(exe);
                cmd.arg("remux-worker")
                    .arg(&sock)
                    .arg(&dir)
                    .arg(size.to_string());
                // Who to die with. The worker compares this against its
                // own getppid(); see the guard in run_remux_worker.
                cmd.args(["--supervisor-pid", &std::process::id().to_string()]);
                for (s, n) in &socks[1..] {
                    cmd.args(["--part", &format!("{}:{n}", s.display())]);
                }
                for (flag, v) in [
                    ("--video-kbps", video_kbps),
                    ("--max-height", max_height),
                    ("--max-channels", max_channels),
                ] {
                    if v > 0 {
                        cmd.args([flag, &v.to_string()]);
                    }
                }
                if stereo_gain_db.is_finite() {
                    cmd.args(["--stereo-gain-db", &stereo_gain_db.to_string()]);
                }
                if native_gain_db.is_finite() {
                    cmd.args(["--native-gain-db", &native_gain_db.to_string()]);
                }
                if loudness_source_channels > 0 {
                    cmd.args([
                        "--loudness-source-channels",
                        &loudness_source_channels.to_string(),
                    ]);
                }
                if deinterlace {
                    cmd.arg("--deinterlace");
                }
                if tone_map {
                    cmd.arg("--tone-map");
                }
                if burn_subtitle > 0 {
                    cmd.args(["--burn-sub", &(burn_subtitle - 1).to_string()]);
                }
                if !burn_sets.is_empty() {
                    let p = dir.join("burn-sets.bin");
                    std::fs::write(&p, &burn_sets)
                        .with_context(|| format!("writing {}", p.display()))?;
                    cmd.args(["--burn-sets", &p.to_string_lossy()]);
                }
                if burn_ass > 0 {
                    cmd.args(["--burn-ass", &(burn_ass - 1).to_string()]);
                }
                if !burn_ass_file.is_empty() {
                    let p = dir.join("burn.ass");
                    std::fs::write(&p, &burn_ass_file)
                        .with_context(|| format!("writing {}", p.display()))?;
                    cmd.args(["--burn-ass-file", &p.to_string_lossy()]);
                }
                if !video_codec.is_empty() {
                    cmd.args(["--video-codec", &video_codec]);
                }
                if !audio_codec.is_empty() {
                    cmd.args(["--audio-codec", &audio_codec]);
                }
                if !container.is_empty() {
                    cmd.args(["--container", &container]);
                }
                let child = cmd
                    .args(["--video", video])
                    .args(["--audio", audio])
                    .args(["--audio-track", &audio_track.to_string()])
                    .args(["--video-track", &video_track.to_string()])
                    .args(["--start-ms", &start_ms.to_string()])
                    .args(if sink.is_empty() {
                        vec![]
                    } else {
                        vec!["--sink".into(), sink.to_string()]
                    })
                    // BOTH streams. stderr alone captured GStreamer's
                    // C-side output and Rust panics — which is why crash
                    // capture worked — while every tracing::info! the
                    // worker emits went to stdout and was discarded with
                    // the detached parent's. A hung session therefore
                    // left no trace of what the pipeline thought it was
                    // doing, which cost an evening of guessing.
                    .stdout(std::process::Stdio::from(log.try_clone()?))
                    .stderr(std::process::Stdio::from(log))
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning worker {}", exe.display()))?;
                tracing::info!(session = %session_id, pid = child.id(), "worker spawned");
                Worker::Child(child)
            }
            None => {
                let plan = kahawai_media::remux::RemuxPlan {
                    video: kahawai_media::worker::parse_mode(video),
                    audio: kahawai_media::worker::parse_mode(audio),
                    audio_track: audio_track as usize,
                    video_track: video_track as usize,
                    video_kbps: (video_kbps > 0).then_some(video_kbps),
                    max_height: (max_height > 0).then_some(max_height),
                    max_channels: (max_channels > 0).then_some(max_channels),
                    stereo_gain_db: stereo_gain_db.is_finite().then_some(stereo_gain_db),
                    native_gain_db: native_gain_db.is_finite().then_some(native_gain_db),
                    loudness_source_channels: (loudness_source_channels > 0)
                        .then_some(loudness_source_channels),
                    tone_map,
                    deinterlace,
                    burn_subtitle: (burn_subtitle > 0).then(|| (burn_subtitle - 1) as usize),
                    burn_ass: (burn_ass > 0).then(|| (burn_ass - 1) as usize),
                    video_codec: kahawai_media::remux::VideoTarget::from_str(&video_codec),
                    audio_codec: kahawai_media::remux::AudioTarget::from_str(&audio_codec),
                    segment_format: kahawai_media::remux::SegmentFormat::from_str(&container),
                };
                let (all, dir) = (socks.clone(), dir.clone());
                let sink_owned = (!sink.is_empty()).then(|| sink.to_string());
                let sets_path = if burn_sets.is_empty() {
                    None
                } else {
                    let p = dir.join("burn-sets.bin");
                    std::fs::write(&p, &burn_sets)?;
                    Some(p)
                };
                let ass_path = if burn_ass_file.is_empty() {
                    None
                } else {
                    let p = dir.join("burn.ass");
                    std::fs::write(&p, &burn_ass_file)?;
                    Some(p)
                };
                let err = Arc::new(Mutex::new(None));
                let err2 = err.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    if let Err(e) = kahawai_media::worker::run_parts(
                        &all,
                        &dir,
                        plan,
                        start_ms,
                        sink_owned.as_deref(),
                        sets_path.as_deref(),
                        ass_path.as_deref(),
                    ) {
                        tracing::warn!(error = format!("{e:#}"), "in-process worker failed");
                        *err2.lock().unwrap() = Some(format!("{e:#}"));
                    }
                });
                Worker::InProcess(handle, err)
            }
        };
        self.sessions.lock().unwrap().insert(
            session_id.to_string(),
            Session {
                dir: dir.clone(),
                worker,
                bridge,
            },
        );

        // Post-ready supervision: a worker dying mid-session becomes a
        // SessionError so the hub can reschedule (AR-6).
        {
            let runner = self.clone();
            let sid = session_id.to_string();
            let run_dir = dir.clone();
            tokio::spawn(async move {
                // Poll: the child handle lives in the sessions map.
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    // HUB-36: the worker writes pace.json once, when the
                    // throttle first engages. Take it here — this poll
                    // already watches the run dir, and the dir is gone
                    // the moment the session ends or a seek replaces it.
                    runner.take_pace(&sid, &run_dir);
                    let gone = {
                        let mut sessions = runner.sessions.lock().unwrap();
                        match sessions.get_mut(&sid) {
                            // Replaced or ended: this watcher is obsolete.
                            Some(s) if s.dir != run_dir => return,
                            None => return,
                            Some(s) => match &mut s.worker {
                                Worker::Child(child) => {
                                    matches!(child.try_wait(), Ok(Some(_)) | Err(_))
                                }
                                Worker::InProcess(handle, _) => handle.is_finished(),
                            },
                        }
                    };
                    if gone {
                        // EOS is also "gone" — only report if the playlist
                        // never finalized (ENDLIST = clean finish).
                        let done = std::fs::read_to_string(run_dir.join("master.m3u8"))
                            .map(|p| p.contains("#EXT-X-ENDLIST"))
                            .unwrap_or(false);
                        if !done {
                            tracing::warn!(session = %sid, "worker died mid-session");
                            let _ = runner
                                .link
                                .send(TcToHub {
                                    msg: Some(tc_to_hub::Msg::SessionError(SessionError {
                                        session_id: sid.clone(),
                                        error: "worker died mid-session".into(),
                                        // Mid-session death: same
                                        // evidence, same reason to keep
                                        // it (this dir is about to go).
                                        worker_log: std::fs::read_to_string(
                                            run_dir.join("worker.log"),
                                        )
                                        .unwrap_or_default(),
                                    })),
                                })
                                .await;
                        }
                        return;
                    }
                }
            });
        }

        // Ready once the playlist has runway (≥3 segments or ENDLIST) —
        // a one-segment playlist stalls the client right after segment 0;
        // failed if the worker dies first.
        let playlist = dir.join("master.m3u8");
        let ready = |p: &std::path::Path| match std::fs::read_to_string(p) {
            // Content seconds, not segment count — see the hub's
            // playlist_ready for the measured reasoning.
            Ok(t) => {
                t.contains("#EXT-X-ENDLIST")
                    || t.lines()
                        .filter_map(|l| {
                            l.strip_prefix("#EXTINF:")?
                                .trim_end_matches(',')
                                .parse::<f64>()
                                .ok()
                        })
                        .sum::<f64>()
                        >= 6.5
            }
            Err(_) => false,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if ready(&playlist) {
                return Ok(dir);
            }
            {
                let mut sessions = self.sessions.lock().unwrap();
                let Some(s) = sessions.get_mut(session_id) else {
                    anyhow::bail!("session ended during startup");
                };
                match &mut s.worker {
                    Worker::Child(child) => {
                        if let Some(status) = child.try_wait()? {
                            // A clean exit with a finished playlist is a
                            // pipeline that COMPLETED (short content
                            // remuxes faster than this poll).
                            if status.success() && ready(&playlist) {
                                return Ok(dir);
                            }
                            let log =
                                std::fs::read_to_string(dir.join("worker.log")).unwrap_or_default();
                            let tail: String =
                                log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                            // Keep the WHOLE stderr for the hub: this
                            // dir is wiped by the very next attempt, and
                            // a panic's message — the only line naming
                            // the cause — is above the frames the tail
                            // quotes.
                            *self.last_worker_log.lock().unwrap() = Some(log);
                            anyhow::bail!("worker exited at start ({status}): {tail}");
                        }
                    }
                    Worker::InProcess(handle, err) => {
                        if handle.is_finished() {
                            let detail = err.lock().unwrap().clone();
                            if detail.is_none() && ready(&playlist) {
                                return Ok(dir);
                            }
                            let detail = detail.unwrap_or_default();
                            anyhow::bail!("in-process worker exited before the playlist: {detail}");
                        }
                    }
                }
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!("no playlist within 30s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Serve the worker's Unix-socket read protocol by round-tripping
    /// each request over the hub link.
    async fn bridge_reads(
        &self,
        mut conn: tokio::net::UnixStream,
        session_id: &str,
        part: u32,
    ) -> Result<()> {
        let mut req = [0u8; 16];
        loop {
            if conn.read_exact(&mut req).await.is_err() {
                return Ok(()); // worker closed (EOS or teardown)
            }
            let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
            let len = u64::from_le_bytes(req[8..].try_into().unwrap())
                .min(kahawai_media::worker::MAX_READ);
            let req_id = self
                .next_req
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.pending_reads.lock().unwrap().insert(req_id, tx);
            self.link
                .send(TcToHub {
                    msg: Some(tc_to_hub::Msg::SourceRead(kahawai_proto::v1::SourceRead {
                        session_id: session_id.to_string(),
                        offset,
                        len,
                        req: req_id,
                        part,
                    })),
                })
                .await
                .context("link closed")?;
            // Bounded wait: if the hub dropped the read (lease gone,
            // session torn down) the worker must error out, not hang.
            let started = std::time::Instant::now();
            let data = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(data)) => data,
                Ok(Err(_)) | Err(_) => {
                    self.pending_reads.lock().unwrap().remove(&req_id);
                    anyhow::bail!("source read {req_id} unanswered");
                }
            };
            // Timed at the LEASE round trip, not the local write: this
            // is what the source plane sustains for this box (HUB-36).
            self.fold_link_rate(data.len(), started.elapsed());
            conn.write_all(&(data.len() as u64).to_le_bytes()).await?;
            conn.write_all(&data).await?;
        }
    }

    /// Take this run's pace sample if the worker has written one.
    ///
    /// Renamed rather than read-and-remembered: the file's ABSENCE is
    /// the "already taken" flag, which survives this watcher being
    /// replaced by a seek-restart's and costs no extra state. Keeping
    /// the renamed copy leaves the number visible in the run dir for
    /// anyone reading it by hand.
    fn take_pace(&self, session_id: &str, run_dir: &std::path::Path) {
        let path = run_dir.join("pace.json");
        let Ok(body) = std::fs::read_to_string(&path) else {
            return;
        };
        let _ = std::fs::rename(&path, run_dir.join("pace.taken.json"));
        // {"multiple":3.42} — hand-rolled rather than pulling serde in
        // for one field, and a torn read simply yields no sample.
        let Some(v) = body
            .split_once(':')
            .and_then(|(_, rest)| rest.trim_matches(['}', ' ', '\n']).parse::<f32>().ok())
        else {
            tracing::debug!(session = %session_id, body = %body, "unparseable pace sample");
            return;
        };
        if !v.is_finite() || v <= 0.0 {
            return;
        }
        tracing::info!(session = %session_id, multiple = %format!("{v:.2}"), "pace sample harvested");
        self.pending_pace
            .lock()
            .unwrap()
            .push(kahawai_proto::v1::PaceSample {
                session_id: session_id.to_string(),
                multiple: v,
            });
    }

    /// Drain what the next heartbeat should carry. None when there is
    /// nothing to say — an idle box should not add traffic to its own
    /// keepalive.
    pub fn take_pace_report(&self) -> Option<kahawai_proto::v1::PaceReport> {
        let samples = std::mem::take(&mut *self.pending_pace.lock().unwrap());
        let rate = *self.link_rate.lock().unwrap();
        if samples.is_empty() && rate.is_none() {
            return None;
        }
        Some(kahawai_proto::v1::PaceReport {
            samples,
            link_bytes_per_sec: rate.unwrap_or(0.0) as u64,
        })
    }

    /// Fold one completed source read into the link-rate EWMA. Small
    /// reads are ignored (see `LINK_MIN_READ`).
    fn fold_link_rate(&self, bytes: usize, elapsed: std::time::Duration) {
        if bytes < LINK_MIN_READ || elapsed.is_zero() {
            return;
        }
        let bps = bytes as f64 / elapsed.as_secs_f64();
        let mut cur = self.link_rate.lock().unwrap();
        *cur = Some(match *cur {
            Some(prev) => LINK_ALPHA * bps + (1.0 - LINK_ALPHA) * prev,
            None => bps,
        });
    }

    /// Hub answered a source read. Stale responses (request id no
    /// longer pending — e.g. from a worker a seek-restart replaced) are
    /// dropped on the floor.
    pub fn source_data(&self, req: u64, data: Vec<u8>) {
        if let Some(tx) = self.pending_reads.lock().unwrap().remove(&req) {
            let _ = tx.send(data);
        }
    }

    /// Hub wants an artifact: stream it back in chunks.
    pub async fn fetch_artifact(&self, session_id: &str, name: &str) {
        // Names come from client URLs upstream; the hub sanitizes, but
        // never trust a path component here either.
        let sane = !name.contains('/') && !name.contains("..");
        let path = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|s| s.dir.join(name));
        let data = match (sane, path) {
            (true, Some(p)) => std::fs::read(&p).map_err(|e| e.to_string()),
            (false, _) => Err("invalid artifact name".into()),
            (true, None) => Err("unknown session".into()),
        };
        match data {
            Ok(bytes) => {
                let mut sent = 0usize;
                while sent < bytes.len() || (bytes.is_empty() && sent == 0) {
                    let end = (sent + ARTIFACT_CHUNK).min(bytes.len());
                    let eof = end == bytes.len();
                    let msg = TcToHub {
                        msg: Some(tc_to_hub::Msg::ArtifactData(ArtifactData {
                            session_id: session_id.to_string(),
                            name: name.to_string(),
                            data: bytes[sent..end].to_vec(),
                            eof,
                            error: String::new(),
                        })),
                    };
                    if self.link.send(msg).await.is_err() {
                        return;
                    }
                    if eof {
                        break;
                    }
                    sent = end;
                }
            }
            Err(e) => {
                let _ = self
                    .link
                    .send(TcToHub {
                        msg: Some(tc_to_hub::Msg::ArtifactData(ArtifactData {
                            session_id: session_id.to_string(),
                            name: name.to_string(),
                            data: Vec::new(),
                            eof: true,
                            error: e,
                        })),
                    })
                    .await;
            }
        }
    }

    /// End one run and WAIT for its pipeline to be truly gone before
    /// removing its scratch — deleting files under a draining pipeline
    /// trips C-level asserts in splitmuxsink (observed: abort).
    /// Pacing: persist the viewer's position where this session's
    /// worker polls it.
    pub fn viewer_position(&self, session_id: &str, position_ms: u64) {
        if let Some(s) = self.sessions.lock().unwrap().get(session_id) {
            let _ = std::fs::write(s.dir.join("viewer.pos"), position_ms.to_string());
        }
    }

    pub async fn end(&self, session_id: &str) {
        let Some(mut s) = self.sessions.lock().unwrap().remove(session_id) else {
            return;
        };
        s.bridge.abort();
        let done = match &mut s.worker {
            Worker::Child(child) => {
                let _ = child.start_kill();
                tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                    .await
                    .is_ok()
            }
            Worker::InProcess(handle, _) => {
                // Its source is gone (bridge aborted): the pipeline
                // errors/EOSes and the blocking task returns.
                tokio::time::timeout(std::time::Duration::from_secs(10), handle)
                    .await
                    .is_ok()
            }
        };
        // OPS-10: gather BEFORE the dir goes. remove_dir_all below is
        // synchronous and the hub's EndSession is fire-and-forget, so
        // there is no later moment to ask — which is exactly why a hung
        // session used to leave nothing behind.
        let bundle = gather_bundle(session_id, &s.dir);
        let _ = self
            .link
            .send(TcToHub {
                msg: Some(tc_to_hub::Msg::SessionLogs(
                    kahawai_proto::v1::SessionLogs {
                        session_id: session_id.to_string(),
                        body: bundle,
                    },
                )),
            })
            .await;
        if done {
            let _ = std::fs::remove_dir_all(&s.dir);
        } else {
            // Leave the dir for the next Runner::new sweep rather than
            // yanking it from under a live pipeline.
            tracing::warn!(session = %session_id, "worker did not stop in time; leaving scratch");
        }
        tracing::info!(session = %session_id, "session run ended");
    }

    /// OPS-10: this running session's diagnostics, on request. Returns
    /// None when the session is not ours — an ended one is already
    /// stored hub-side, so there is nothing useful to answer with.
    pub fn collect_logs(&self, session_id: &str) -> Option<String> {
        let dir = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id)?.dir.clone()
        };
        Some(gather_bundle(session_id, &dir))
    }

    /// Link died: every session dies with it (the hub reschedules, AR-6).
    pub async fn end_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.end(&id).await;
        }
    }
}

/// Everything the satellite knows about one run, as text a human can
/// paste into a bug report (OPS-10).
///
/// Best-effort throughout: a missing file is a missing section, never a
/// failure — this runs on the teardown path and must not be able to
/// break it.
fn gather_bundle(session_id: &str, dir: &std::path::Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32 * 1024);
    let _ = writeln!(out, "== satellite: session {session_id}");
    let _ = writeln!(out, "run dir: {}", dir.display());

    let segs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            let mut v: Vec<_> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("segment"))
                })
                .collect();
            v.sort();
            v
        })
        .unwrap_or_default();
    let _ = writeln!(out, "segments: {}", segs.len());
    if let Some(first) = segs.first() {
        let _ = writeln!(out, "first segment: {}", first_segment_summary(first));
    }

    for name in [
        "start.pos",
        "viewer.pos",
        "pace.json",
        "pace.taken.json",
        "facts.jsonl",
    ] {
        if let Ok(body) = std::fs::read_to_string(dir.join(name)) {
            let _ = writeln!(out, "\n== {name}\n{}", body.trim_end());
        }
    }
    if let Ok(pl) = std::fs::read_to_string(dir.join("master.m3u8")) {
        let _ = writeln!(out, "\n== master.m3u8 (tail)\n{}", tail_lines(&pl, 40));
    }
    if let Ok(log) = std::fs::read_to_string(dir.join("worker.log")) {
        let _ = writeln!(out, "\n== worker.log\n{log}");
    }
    out
}

/// Is the first segment independently decodable? One line, and the
/// whole diagnosis of a wedged session: a player handed a segment with
/// no parameter sets stalls there forever while the worker happily
/// produces minutes more behind it.
///
/// H.264-in-TS only, which is what the start codes below assume. Other
/// pipelines say so rather than being silently skipped.
fn first_segment_summary(path: &std::path::Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return "unreadable".into();
    };
    if !path.extension().is_some_and(|e| e == "ts") {
        return format!("{} bytes (not TS; no NAL summary)", bytes.len());
    }
    let (mut sps, mut pps, mut idr) = (false, false, false);
    for w in bytes.windows(4) {
        if w[0] == 0 && w[1] == 0 && w[2] == 1 {
            match w[3] & 0x1f {
                5 => idr = true,
                7 => sps = true,
                8 => pps = true,
                _ => {}
            }
        }
    }
    format!(
        "{} bytes, SPS={sps} PPS={pps} IDR={idr}{}",
        bytes.len(),
        if sps && pps && idr {
            ""
        } else {
            "  <-- NOT independently decodable; a player wedges here"
        }
    )
}

fn tail_lines(body: &str, n: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
