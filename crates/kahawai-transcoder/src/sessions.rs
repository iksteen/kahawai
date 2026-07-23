//! Dispatched session execution (§6): each session runs the same
//! supervised worker the hub uses locally, fed over its Unix socket by a
//! bridge that pulls source bytes from the hub over the control link.
//! Artifacts (playlist/segments) are read from the session scratch dir
//! and streamed back on request.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use kahawai_proto::v1::{tc_to_hub, ArtifactData, SessionError, SessionReady, TcToHub};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

const ARTIFACT_CHUNK: usize = 256 * 1024;

enum Worker {
    Child(tokio::process::Child),
    /// Tests only: worker::run on a blocking thread (no binary to spawn).
    InProcess,
}

struct Session {
    dir: PathBuf,
    worker: Worker,
}

/// All state for one hub link's dispatched sessions.
pub struct Runner {
    scratch_root: PathBuf,
    /// None → run pipelines in-process (tests); Some → worker binary.
    worker_exe: Option<PathBuf>,
    link: mpsc::Sender<TcToHub>,
    sessions: Mutex<HashMap<String, Session>>,
    /// One outstanding source read per session (the worker blocks on
    /// each), fulfilled when the hub's SourceData arrives.
    pending_reads: Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>,
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
        })
    }

    pub async fn start(
        self: &Arc<Self>,
        session_id: String,
        size: u64,
        video: &str,
        audio: &str,
        start_ms: u64,
    ) {
        let result = self.start_inner(&session_id, size, video, audio, start_ms).await;
        let msg = match result {
            Ok(()) => {
                tracing::info!(session = %session_id, "session ready");
                TcToHub {
                    msg: Some(tc_to_hub::Msg::SessionReady(SessionReady {
                        session_id: session_id.clone(),
                    })),
                }
            }
            Err(e) => {
                tracing::warn!(session = %session_id, error = format!("{e:#}"), "session failed");
                self.end(&session_id);
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

    async fn start_inner(
        self: &Arc<Self>,
        session_id: &str,
        size: u64,
        video: &str,
        audio: &str,
        start_ms: u64,
    ) -> Result<()> {
        let dir = self.scratch_root.join(session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let sock = dir.join("worker.sock");
        let listener = tokio::net::UnixListener::bind(&sock)
            .with_context(|| format!("binding {}", sock.display()))?;
        // Bridge: worker's socket reads → SourceRead over the link →
        // SourceData fulfils the pending oneshot → back to the socket.
        let runner = self.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            let Ok((conn, _)) = listener.accept().await else {
                return;
            };
            if let Err(e) = runner.bridge_reads(conn, &sid).await {
                tracing::debug!(session = %sid, error = format!("{e:#}"), "read bridge closed");
            }
        });

        let worker = match &self.worker_exe {
            Some(exe) => {
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let child = tokio::process::Command::new(exe)
                    .arg("remux-worker")
                    .arg(&sock)
                    .arg(&dir)
                    .arg(size.to_string())
                    .args(["--video", video])
                    .args(["--audio", audio])
                    .args(["--start-ms", &start_ms.to_string()])
                    .stderr(std::process::Stdio::from(log))
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning worker {}", exe.display()))?;
                tracing::info!(session = %session_id, pid = child.id(), "worker spawned");
                Worker::Child(child)
            }
            None => {
                let (v, a) = (
                    kahawai_media::worker::parse_mode(video),
                    kahawai_media::worker::parse_mode(audio),
                );
                let (sock, dir) = (sock.clone(), dir.clone());
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = kahawai_media::worker::run(&sock, &dir, size, v, a, start_ms) {
                        tracing::warn!(error = format!("{e:#}"), "in-process worker failed");
                    }
                });
                Worker::InProcess
            }
        };
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string(), Session { dir: dir.clone(), worker });

        // Ready once the playlist exists; failed if the worker dies first.
        let playlist = dir.join("master.m3u8");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if playlist.exists() {
                return Ok(());
            }
            {
                let mut sessions = self.sessions.lock().unwrap();
                let Some(s) = sessions.get_mut(session_id) else {
                    anyhow::bail!("session ended during startup");
                };
                if let Worker::Child(child) = &mut s.worker
                    && let Some(status) = child.try_wait()?
                {
                    let log = std::fs::read_to_string(dir.join("worker.log")).unwrap_or_default();
                    let tail: String = log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                    anyhow::bail!("worker exited at start ({status}): {tail}");
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
    async fn bridge_reads(&self, mut conn: tokio::net::UnixStream, session_id: &str) -> Result<()> {
        let mut req = [0u8; 16];
        loop {
            if conn.read_exact(&mut req).await.is_err() {
                return Ok(()); // worker closed (EOS or teardown)
            }
            let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
            let len = u64::from_le_bytes(req[8..].try_into().unwrap())
                .min(kahawai_media::worker::MAX_READ);
            let (tx, rx) = oneshot::channel();
            self.pending_reads.lock().unwrap().insert(session_id.to_string(), tx);
            self.link
                .send(TcToHub {
                    msg: Some(tc_to_hub::Msg::SourceRead(kahawai_proto::v1::SourceRead {
                        session_id: session_id.to_string(),
                        offset,
                        len,
                    })),
                })
                .await
                .context("link closed")?;
            let data = rx.await.context("link dropped pending read")?;
            conn.write_all(&(data.len() as u64).to_le_bytes()).await?;
            conn.write_all(&data).await?;
        }
    }

    /// Hub answered a source read.
    pub fn source_data(&self, session_id: &str, data: Vec<u8>) {
        if let Some(tx) = self.pending_reads.lock().unwrap().remove(session_id) {
            let _ = tx.send(data);
        }
    }

    /// Hub wants an artifact: stream it back in chunks.
    pub async fn fetch_artifact(&self, session_id: &str, name: &str) {
        // Names come from client URLs upstream; the hub sanitizes, but
        // never trust a path component here either.
        let sane = !name.contains('/') && !name.contains("..");
        let path = self.sessions.lock().unwrap().get(session_id).map(|s| s.dir.join(name));
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

    pub fn end(&self, session_id: &str) {
        if let Some(mut s) = self.sessions.lock().unwrap().remove(session_id) {
            if let Worker::Child(child) = &mut s.worker {
                let _ = child.start_kill();
            }
            // In-process workers die when their scratch and socket vanish.
            let _ = std::fs::remove_dir_all(&s.dir);
            tracing::info!(session = %session_id, "session ended");
        }
        self.pending_reads.lock().unwrap().remove(session_id);
    }

    /// Link died: every session dies with it (the hub reschedules, AR-6).
    pub fn end_all(&self) {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.end(&id);
        }
    }
}
