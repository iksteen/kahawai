//! The supervised pipeline run, shared by the hub's local remux and the
//! transcoder's dispatched sessions (§1.1, AR-10, TC-4, TC-5).
//!
//! A pipeline runs in a child process — `<exe> remux-worker …` — so a
//! GStreamer crash kills one session and never the process that
//! supervises it (observed on a real library, twice, via hlssink3 panics).
//! The supervisor's job is what this module does: give the run a
//! directory, serve it source bytes over one Unix socket per part, wait
//! until its playlist has enough runway to hand to a client, watch it die,
//! and keep the evidence when it does. Tests without a worker binary run
//! the same pipeline in-process instead.
//!
//! # The run directory
//!
//! Every run gets `<scratch_root>/<session_id>/r<N>`, fresh per run and
//! never shared: a seek-restart's new pipeline must not write into the
//! directory the old one is still draining (two hlssink instances on one
//! directory abort). What the worker and the supervisor put there:
//!
//! * `master.m3u8` — the playlist; `#EXT-X-ENDLIST` means the run finished.
//! * `segment%05d.ts`, or `init.mp4` + `segment%05d.m4s` — the media.
//! * `start.pos` — the absolute ms the first segment actually starts at.
//! * `viewer.pos` — written by the supervisor from the client's progress
//!   pings; the worker paces itself against it (§4.6).
//! * `pace.json` — written once by the worker when its throttle first
//!   engages; renamed to `pace.taken.json` by whoever harvests it, so the
//!   file's absence is the "already taken" flag and the number stays
//!   readable by hand.
//! * `facts.jsonl` — the worker's honest-degradation facts (AR-13).
//! * `subs-e{n}.*` — subtitle side channels tapped out of the pipeline.
//! * `worker.log` — the child's stdout AND stderr. Both: `tracing` writes
//!   to stdout, GStreamer and panics to stderr, and capturing only the
//!   latter once left a hung session's log empty for its whole life.
//! * `burn-sets.bin`, `burn.ass` — burn payloads the executor materialised.
//!
//! Sockets are NOT in the run directory: `SUN_LEN` caps a Unix socket path
//! at roughly 108 bytes, and macOS's ordinary temp root plus a session
//! ULID already exceeds it, so they live under a short `/tmp/kahawai-XXXX`
//! that the run owns until it ends.
//!
//! # The byte source
//!
//! Where bytes come from is the one thing the two supervisors genuinely
//! differ on: the hub reads a mediahost lease, a transcoder round-trips
//! each read over its link. Both implement [`ByteSource`]; the socket
//! protocol (16 bytes of `offset, len` in, `len` then bytes out, 0 = EOF)
//! is served here once.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use kahawai_media::facts::Fact;
use kahawai_media::remux::{RemuxJob, RemuxSource, start_parts};
use kahawai_media::worker::MAX_READ;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::bundle;
use crate::job::{ArgvLayout, Job, Payload};
use crate::playlist::{playlist_finished, playlist_ready};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Random-access bytes for one part, served to the worker over its
/// socket (or to an in-process pipeline through [`BlockingSource`]).
pub trait ByteSource: Send + Sync + 'static {
    fn size(&self) -> u64;
    /// Up to `len` bytes at `offset`. A short or empty answer is EOF at
    /// that offset; an error closes the worker's socket, so the pipeline
    /// errors out instead of hanging on a read that will never come.
    fn read(&self, offset: u64, len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>>;
}

/// How long a start may take before it is declared failed. Long enough
/// for a remote source's preroll; short enough that a client still
/// waiting on it has not given up.
pub const START_DEADLINE: Duration = Duration::from_secs(30);

/// How long `end` waits for a killed worker before leaving its directory
/// in place rather than deleting files from under a draining pipeline
/// (which trips C-level asserts in splitmuxsink — observed as an abort).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// The short socket directory. See the module doc on `SUN_LEN`.
pub fn short_socket_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("kahawai-")
        .tempdir_in("/tmp")
        .context("creating short worker socket directory")
}

/// Serve one part's reads to a worker over its socket. Wire format: see
/// `kahawai_media::worker`. Returns when the worker closes the socket
/// (EOS or teardown); an error from the source ends the connection.
pub async fn serve_reads(
    mut conn: tokio::net::UnixStream,
    source: Arc<dyn ByteSource>,
) -> Result<()> {
    let size = source.size();
    let mut req = [0u8; 16];
    loop {
        if conn.read_exact(&mut req).await.is_err() {
            return Ok(());
        }
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
        let len = u64::from_le_bytes(req[8..].try_into().unwrap()).min(MAX_READ);
        let want = if offset >= size {
            0
        } else {
            len.min(size - offset)
        };
        let mut data = if want > 0 {
            source
                .read(offset, want)
                .await
                .with_context(|| format!("reading {want} bytes at {offset}"))?
        } else {
            Vec::new()
        };
        data.truncate(want as usize);
        conn.write_all(&(data.len() as u64).to_le_bytes()).await?;
        conn.write_all(&data).await?;
    }
}

/// A blocking `RemuxSource` over an async [`ByteSource`], for pipelines
/// that run in-process: the remux feeder thread bridges into the runtime
/// for every read.
pub struct BlockingSource {
    source: Arc<dyn ByteSource>,
    handle: tokio::runtime::Handle,
    /// Reads served, for the log line that says whether a stalled
    /// consumer ever got its first byte.
    reads: u64,
}

impl BlockingSource {
    pub fn new(source: Arc<dyn ByteSource>, handle: tokio::runtime::Handle) -> Self {
        Self {
            source,
            handle,
            reads: 0,
        }
    }
}

impl RemuxSource for BlockingSource {
    fn size(&self) -> u64 {
        self.source.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = self.source.size();
        if offset >= size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(size - offset);
        self.reads += 1;
        let started = std::time::Instant::now();
        if self.reads % 64 == 1 {
            tracing::debug!(offset, len, reads = self.reads, "source read");
        }
        let _guard = self.handle.enter();
        let outcome = self.handle.block_on(self.source.read(offset, len));
        // A read that takes seconds is the byte plane, not the pipeline; a
        // read that never returns does not reach this line at all, which
        // is the distinction worth having in a log.
        if started.elapsed() > Duration::from_secs(5) {
            tracing::warn!(
                offset,
                len,
                seconds = started.elapsed().as_secs_f64(),
                ok = outcome.is_ok(),
                "slow source read"
            );
        }
        let data = outcome?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

/// Runs pipelines under a scratch root, one directory per run.
pub struct Executor {
    scratch_root: PathBuf,
    /// The binary to spawn as the pipeline worker (the supervisor passes
    /// its own executable; the worker is a hidden subcommand). `None`
    /// runs pipelines in-process, which is for tests without a binary.
    worker_exe: Option<PathBuf>,
    run_seq: AtomicU64,
}

impl Executor {
    /// Wipes `scratch_root`: runs never survive a restart, and their
    /// directories are garbage the moment the supervisor that owned them
    /// is gone. A supervisor that wants their diagnostics gathers them
    /// BEFORE constructing this.
    pub fn new(scratch_root: PathBuf, worker_exe: Option<PathBuf>) -> Self {
        let _ = std::fs::remove_dir_all(&scratch_root);
        Self {
            scratch_root,
            worker_exe,
            run_seq: AtomicU64::new(1),
        }
    }

    pub fn scratch_root(&self) -> &Path {
        &self.scratch_root
    }

    pub fn worker_exe(&self) -> Option<&Path> {
        self.worker_exe.as_deref()
    }

    /// ONE attempt at running `job`: a fresh run directory, one socket per
    /// part, the worker spawned (or the pipeline started in-process), and
    /// a wait for the playlist to have its runway. Retries — the TC-6
    /// sink fallback — are the caller's, because the caller holds whatever
    /// placement reservation the retry has to give back.
    pub async fn start(
        &self,
        session_id: &str,
        job: Job,
        sources: Vec<Arc<dyn ByteSource>>,
    ) -> Result<Started, StartFailure> {
        let attempt = self.begin(session_id, &job, &sources).await;
        let run = match attempt {
            Ok(run) => run,
            Err(error) => {
                return Err(StartFailure {
                    error,
                    worker_log: String::new(),
                    bundle: String::new(),
                });
            }
        };
        if let Err(error) = run.await_ready(&job).await {
            return Err(run.fail_start(error).await);
        }
        let facts = kahawai_media::facts::read(run.dir());
        run.watch();
        Ok(Started { run, facts })
    }

    async fn begin(
        &self,
        session_id: &str,
        job: &Job,
        sources: &[Arc<dyn ByteSource>],
    ) -> Result<Run> {
        anyhow::ensure!(
            !job.part_sizes.is_empty(),
            "no source parts for the session"
        );
        anyhow::ensure!(
            sources.len() == job.part_sizes.len(),
            "{} sources for {} parts",
            sources.len(),
            job.part_sizes.len()
        );
        let run_no = self.run_seq.fetch_add(1, Ordering::Relaxed);
        let dir = self
            .scratch_root
            .join(session_id)
            .join(format!("r{run_no}"));
        // ALWAYS from a clean dir: a crashed first attempt leaves a stale
        // playlist the readiness check would mistake for output.
        let _ = std::fs::remove_dir_all(&dir);
        kahawai_core::private::create_dir(&dir)
            .with_context(|| format!("creating private run dir {}", dir.display()))?;

        let burn_sets = materialise(&dir, "burn-sets.bin", job.burn_sets.as_ref())?;
        let burn_ass = materialise(&dir, "burn.ass", job.burn_ass.as_ref())?;

        let socket_dir = short_socket_dir()?;
        let mut sockets = Vec::with_capacity(sources.len());
        let mut serving = Vec::with_capacity(sources.len());
        for (n, source) in sources.iter().enumerate() {
            let sock = if n == 0 {
                socket_dir.path().join("worker.sock")
            } else {
                socket_dir.path().join(format!("worker{n}.sock"))
            };
            let listener = tokio::net::UnixListener::bind(&sock)
                .with_context(|| format!("binding {}", sock.display()))?;
            // Serve source reads for the run's life; the task ends when
            // the worker closes its socket. Spawned before the child so
            // the accept is already pending when it connects.
            let source = source.clone();
            serving.push(tokio::spawn(async move {
                match listener.accept().await {
                    Ok((conn, _)) => {
                        if let Err(e) = serve_reads(conn, source).await {
                            tracing::debug!(error = format!("{e:#}"), "worker read channel closed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "worker never connected"),
                }
            }));
            sockets.push(sock);
        }

        let worker = match &self.worker_exe {
            Some(exe) => {
                let argv = job.to_argv(&ArgvLayout {
                    sockets: &sockets,
                    out_dir: &dir,
                    burn_sets: burn_sets.as_deref(),
                    burn_ass: burn_ass.as_deref(),
                    supervisor_pid: Some(std::process::id()),
                })?;
                let log = std::fs::File::create(dir.join("worker.log"))?;
                let child = tokio::process::Command::new(exe)
                    .args(&argv)
                    .stdout(std::process::Stdio::from(log.try_clone()?))
                    .stderr(std::process::Stdio::from(log))
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning worker {}", exe.display()))?;
                tracing::info!(
                    session = session_id,
                    pid = child.id(),
                    "pipeline worker spawned"
                );
                Worker::Child(child)
            }
            None => {
                // In-process: the pipeline pulls (and seeks — MP4
                // moov-at-end needs it) from the sources via a blocking
                // adapter on the remux feeder thread. `start_parts` blocks
                // while prerolling for an offset seek — off the async
                // runtime with it, or the preroll's own reads can never
                // be driven (single-thread runtimes deadlock outright).
                let handle = tokio::runtime::Handle::current();
                let adapters: Vec<Box<dyn RemuxSource>> = sources
                    .iter()
                    .map(|source| {
                        Box::new(BlockingSource::new(source.clone(), handle.clone()))
                            as Box<dyn RemuxSource>
                    })
                    .collect();
                let (plan, start_ms, sink) = (job.plan, job.start_ms, job.sink.clone());
                let (dir2, sets, ass) = (dir.clone(), burn_sets.clone(), burn_ass.clone());
                let pipeline = tokio::task::spawn_blocking(move || {
                    start_parts(
                        &dir2,
                        plan,
                        adapters,
                        start_ms,
                        sink.as_deref(),
                        None,
                        sets.as_deref(),
                        ass.as_deref(),
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("pipeline task panicked: {e}"))??;
                Worker::InProcess(Arc::new(pipeline))
            }
        };
        let (died, _) = tokio::sync::watch::channel(None);
        Ok(Run {
            inner: Arc::new(Inner {
                session_id: session_id.to_string(),
                dir,
                worker: Mutex::new(Some(worker)),
                _socket_dir: socket_dir,
                serving: Mutex::new(serving),
                died,
            }),
            watcher: Mutex::new(None),
        })
    }
}

fn materialise(dir: &Path, name: &str, payload: Option<&Payload>) -> Result<Option<PathBuf>> {
    Ok(match payload {
        None => None,
        Some(Payload::Path(path)) => Some(path.clone()),
        Some(Payload::Bytes(bytes)) => {
            let path = dir.join(name);
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            Some(path)
        }
    })
}

/// A run that reached readiness, with the facts its preroll reported
/// (AR-13).
pub struct Started {
    pub run: Run,
    pub facts: Vec<Fact>,
}

/// Why a start did not reach readiness, with the evidence kept before
/// the run directory went away.
#[derive(Debug)]
pub struct StartFailure {
    pub error: anyhow::Error,
    /// The whole `worker.log`: a panic's message names the file and line,
    /// and the four lines the error quotes are the frames after it, which
    /// name nothing.
    pub worker_log: String,
    /// [`bundle::gather`] of the run directory as it was.
    pub bundle: String,
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for StartFailure {}

impl StartFailure {
    /// The error alone, once the evidence has been stored.
    pub fn into_error(self) -> anyhow::Error {
        self.error
    }
}

/// How a run ended on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Death {
    /// The playlist carries ENDLIST: the pipeline reached EOS.
    Finished,
    /// The worker is gone and the playlist is not finished.
    Crashed { error: String, worker_log: String },
}

enum Worker {
    Child(tokio::process::Child),
    InProcess(Arc<RemuxJob>),
}

impl Worker {
    fn stop(&mut self) {
        match self {
            Worker::Child(child) => {
                let _ = child.start_kill();
            }
            Worker::InProcess(job) => job.stop(),
        }
    }

    /// Has the worker exited (child) or the pipeline reached its end or
    /// an error (in-process)?
    fn gone(&mut self) -> bool {
        match self {
            Worker::Child(child) => matches!(child.try_wait(), Ok(Some(_)) | Err(_)),
            Worker::InProcess(job) => job.finished() || job.failed().is_some(),
        }
    }
}

struct Inner {
    session_id: String,
    dir: PathBuf,
    /// `None` once ended.
    worker: Mutex<Option<Worker>>,
    /// Owns the short socket directory until the run is gone.
    _socket_dir: tempfile::TempDir,
    serving: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    died: tokio::sync::watch::Sender<Option<Death>>,
}

/// One running pipeline. Dropping it kills the worker (`kill_on_drop`),
/// but the orderly way out is [`Run::end`], which waits and keeps the
/// evidence.
pub struct Run {
    inner: Arc<Inner>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// A `Run` that is dropped rather than ended was abandoned: a start whose
/// caller went away after readiness, or a seek-restart whose future was
/// cancelled between taking the old run and ending it. The worker must not
/// outlive it. `kill_on_drop` alone cannot guarantee that, because the
/// watcher task holds the same `Inner` that owns the child and only exits
/// once the child is gone — so the child was kept alive by the very task
/// meant to notice its death. Stop the worker here, and take the watcher
/// and the socket servers down with it; the run directory is left for the
/// executor's next sweep, since a killed worker may still be writing.
impl Drop for Run {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.lock().unwrap().take() {
            watcher.abort();
        }
        for task in self.inner.serving.lock().unwrap().drain(..) {
            task.abort();
        }
        if let Some(worker) = self.inner.worker.lock().unwrap().as_mut() {
            tracing::warn!(session = %self.inner.session_id, "run dropped without end; killing its worker");
            worker.stop();
        }
    }
}

impl Run {
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// Pacing (§4.6): where the viewer is, for the worker to throttle
    /// against. Fire-and-forget — a missed update only delays a
    /// pause/resume by one ping.
    pub fn viewer_position(&self, position_ms: u64) {
        let _ = std::fs::write(self.inner.dir.join("viewer.pos"), position_ms.to_string());
    }

    /// Take this run's pace sample if the worker has written one (HUB-36).
    /// Renamed rather than read-and-remembered: the file's absence is the
    /// "already taken" flag, which costs no state and survives whoever
    /// polls being replaced.
    pub fn take_pace_sample(&self) -> Option<f32> {
        take_pace_sample(&self.inner.session_id, &self.inner.dir)
    }

    /// Resolves to `Some` once the worker is gone. Diagnostics and AR-6
    /// hang off this: a transcoder reports a crash to the hub, the hub
    /// stores the evidence.
    pub fn died(&self) -> tokio::sync::watch::Receiver<Option<Death>> {
        self.inner.died.subscribe()
    }

    /// The playlist carries ENDLIST.
    pub fn is_finished(&self) -> bool {
        playlist_finished(&self.inner.dir.join("master.m3u8"))
    }

    /// Kill without waiting.
    pub fn stop(&self) {
        if let Some(worker) = self.inner.worker.lock().unwrap().as_mut() {
            worker.stop();
        }
    }

    /// Stop and wait until the pipeline is really gone, up to `grace`.
    /// True when it is. A seek-restart needs this: a still-dying worker
    /// writing into a directory a new run is about to reuse corrupts it.
    pub async fn stop_and_wait(&self, grace: Duration) -> bool {
        self.stop();
        let deadline = std::time::Instant::now() + grace;
        loop {
            let gone = match self.inner.worker.lock().unwrap().as_mut() {
                // An in-process pipeline stops synchronously at `stop`.
                Some(Worker::InProcess(_)) | None => true,
                Some(worker) => worker.gone(),
            };
            if gone {
                return true;
            }
            if std::time::Instant::now() > deadline {
                tracing::warn!(session = %self.inner.session_id, "worker did not stop in time");
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The diagnostics bundle of this run as it is now (OPS-10).
    pub fn bundle(&self, label: &str) -> String {
        bundle::gather(label, &self.inner.session_id, &self.inner.dir)
    }

    /// Stop, wait [`STOP_GRACE`], gather the bundle and the final pace
    /// sample, and remove the run directory — unless the worker is still
    /// alive, in which case the directory is left for the next
    /// [`Executor::new`] sweep rather than yanked from under it.
    pub async fn end(self, label: &str) -> Ended {
        if let Some(watcher) = self.watcher.lock().unwrap().take() {
            watcher.abort();
        }
        let stopped = self.stop_and_wait(STOP_GRACE).await;
        for task in self.inner.serving.lock().unwrap().drain(..) {
            task.abort();
        }
        // Gather BEFORE the dir goes: there is no later moment to ask,
        // which is exactly why a hung session used to leave nothing.
        let pace = self.take_pace_sample();
        let bundle = self.bundle(label);
        let worker_log =
            std::fs::read_to_string(self.inner.dir.join("worker.log")).unwrap_or_default();
        *self.inner.worker.lock().unwrap() = None;
        if stopped {
            let _ = std::fs::remove_dir_all(&self.inner.dir);
        } else {
            tracing::warn!(session = %self.inner.session_id, "leaving scratch for a live worker");
        }
        tracing::info!(session = %self.inner.session_id, "session run ended");
        Ended {
            bundle,
            worker_log,
            stopped,
            pace,
        }
    }

    /// Wait for the playlist to have its runway, or for the worker to
    /// die trying.
    async fn await_ready(&self, job: &Job) -> Result<()> {
        let playlist = self.inner.dir.join("master.m3u8");
        let target = job.target_duration_secs;
        let deadline = std::time::Instant::now() + START_DEADLINE;
        loop {
            {
                let mut guard = self.inner.worker.lock().unwrap();
                let worker = guard.as_mut().context("run already ended")?;
                match worker {
                    Worker::InProcess(pipeline) => {
                        if let Some(e) = pipeline.failed() {
                            anyhow::bail!("remux failed to start: {e}");
                        }
                    }
                    Worker::Child(child) => {
                        if let Some(status) = child.try_wait()? {
                            // A CLEAN exit is a pipeline that FINISHED: an
                            // all-copy remux of short content completes in
                            // under a second — faster than this poll. The
                            // playlist (with ENDLIST) is the product. Only
                            // a non-zero exit, or a clean exit with nothing
                            // produced, is failure.
                            if !(status.success() && playlist_ready(&playlist, target)) {
                                let log =
                                    std::fs::read_to_string(self.inner.dir.join("worker.log"))
                                        .unwrap_or_default();
                                let tail: String =
                                    log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                                anyhow::bail!("pipeline worker exited at start ({status}): {tail}");
                            }
                        }
                    }
                }
            }
            if playlist_ready(&playlist, target) {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!("remux produced no playlist in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Turn a failed start into evidence: stop what is running, keep the
    /// log and the bundle, and clear the directory if the worker is gone.
    async fn fail_start(self, error: anyhow::Error) -> StartFailure {
        let stopped = self.stop_and_wait(Duration::from_secs(2)).await;
        for task in self.inner.serving.lock().unwrap().drain(..) {
            task.abort();
        }
        let worker_log =
            std::fs::read_to_string(self.inner.dir.join("worker.log")).unwrap_or_default();
        let bundle = self.bundle("failed start");
        *self.inner.worker.lock().unwrap() = None;
        if stopped {
            let _ = std::fs::remove_dir_all(&self.inner.dir);
        }
        StartFailure {
            error,
            worker_log,
            bundle,
        }
    }

    /// Post-ready supervision: publish the worker's death on `died`.
    fn watch(&self) {
        let inner = self.inner.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let gone = match inner.worker.lock().unwrap().as_mut() {
                    None => return,
                    Some(worker) => worker.gone(),
                };
                if !gone {
                    continue;
                }
                // EOS is also "gone" — only a playlist that never
                // finalized is a death worth reporting.
                let death = if playlist_finished(&inner.dir.join("master.m3u8")) {
                    Death::Finished
                } else {
                    tracing::warn!(session = %inner.session_id, "worker died mid-session");
                    let worker_log = match inner.worker.lock().unwrap().as_ref() {
                        Some(Worker::InProcess(pipeline)) => pipeline.failed().unwrap_or_default(),
                        _ => std::fs::read_to_string(inner.dir.join("worker.log"))
                            .unwrap_or_default(),
                    };
                    Death::Crashed {
                        error: "worker died mid-session".into(),
                        worker_log,
                    }
                };
                let _ = inner.died.send(Some(death));
                return;
            }
        });
        *self.watcher.lock().unwrap() = Some(task);
    }
}

/// What [`Run::end`] leaves behind.
#[derive(Debug, Clone)]
pub struct Ended {
    pub bundle: String,
    pub worker_log: String,
    /// The worker was gone when the directory was removed. False means
    /// the directory was left in place.
    pub stopped: bool,
    /// A pace sample the worker had written and nobody had taken yet.
    pub pace: Option<f32>,
}

/// `pace.json` → `pace.taken.json`, parsed. `{"multiple":3.42}` —
/// hand-rolled rather than pulling serde in for one field, and a torn
/// read simply yields no sample.
pub fn take_pace_sample(session_id: &str, dir: &Path) -> Option<f32> {
    let path = dir.join("pace.json");
    let body = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::rename(&path, dir.join("pace.taken.json"));
    let Some(v) = body
        .split_once(':')
        .and_then(|(_, rest)| rest.trim_matches(['}', ' ', '\n']).parse::<f32>().ok())
    else {
        tracing::debug!(session = %session_id, body = %body, "unparseable pace sample");
        return None;
    };
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    tracing::info!(session = %session_id, multiple = %format!("{v:.2}"), "pace sample harvested");
    Some(v)
}
