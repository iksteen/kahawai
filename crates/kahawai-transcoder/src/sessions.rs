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
}

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
        burn_sets: Vec<u8>,
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
                burn_sets,
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
        burn_sets: Vec<u8>,
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
                    tone_map,
                    burn_subtitle: (burn_subtitle > 0).then(|| (burn_subtitle - 1) as usize),
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
            Ok(t) => t.contains("#EXT-X-ENDLIST") || t.matches("#EXTINF").count() >= 3,
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
            let data = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(data)) => data,
                Ok(Err(_)) | Err(_) => {
                    self.pending_reads.lock().unwrap().remove(&req_id);
                    anyhow::bail!("source read {req_id} unanswered");
                }
            };
            conn.write_all(&(data.len() as u64).to_le_bytes()).await?;
            conn.write_all(&data).await?;
        }
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
        if done {
            let _ = std::fs::remove_dir_all(&s.dir);
        } else {
            // Leave the dir for the next Runner::new sweep rather than
            // yanking it from under a live pipeline.
            tracing::warn!(session = %session_id, "worker did not stop in time; leaving scratch");
        }
        tracing::info!(session = %session_id, "session run ended");
    }

    /// Link died: every session dies with it (the hub reschedules, AR-6).
    pub async fn end_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.end(&id).await;
        }
    }
}
