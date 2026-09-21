//! Dispatched session execution (§6, TC-3..5): each session runs on the
//! same supervised executor the hub uses locally
//! (`kahawai_playback::executor`), fed over its Unix socket by a byte
//! source that pulls each read from the hub over the control link.
//! Artifacts (playlist/segments) are read from the run directory and
//! streamed back on request; the run's diagnostics bundle goes back at
//! session end and whenever the hub asks (OPS-10).
//!
//! What is the transcoder's own here, as opposed to the executor's: the
//! link byte source and its rate estimate (HUB-36), the pace report that
//! rides the heartbeat, the SessionReady/SessionError/SessionLogs
//! messages, and the start-generation bookkeeping that keeps a seek
//! restart's EndSession→StartSession ordering honest when the old start
//! has not finished yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kahawai_playback::executor::{BoxFuture, ByteSource, Death, Executor, Run};
use kahawai_playback::job::Job;
use kahawai_proto::v1::{
    ArtifactData, PaceReport, PaceSample, SessionError, SessionFact, SessionReady, StartSession,
    TcToHub, tc_to_hub,
};
use tokio::sync::{mpsc, oneshot};

const ARTIFACT_CHUNK: usize = 256 * 1024;

/// Reads below this measure latency, not bandwidth: the round trip
/// dominates and the resulting figure says more about the hub's event
/// loop than about the link.
const LINK_MIN_READ: usize = 1024 * 1024;

/// Link-rate EWMA weight. Lower than the hub's pace weight because a
/// single read races against whatever else shares the wire; the rate
/// should drift toward the sustained truth rather than chase spikes.
const LINK_ALPHA: f64 = 0.2;

/// How long one source read may wait on the hub before the worker is
/// told the bytes are not coming. If the hub dropped the read (lease
/// gone, session torn down) the worker must error out, not hang.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The link as a byte plane: shared by every part of every session.
struct LinkReads {
    link: mpsc::Sender<TcToHub>,
    /// In-flight source reads keyed by request id — NEVER by session:
    /// seek-restarts reuse the session id, and a stale response from the
    /// previous worker must not satisfy the new worker's read.
    pending: Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>,
    next_req: AtomicU64,
    /// Bytes/sec the source plane sustains, EWMA over LARGE reads only
    /// (see `LINK_MIN_READ`). None until one is seen — a box that has
    /// only ever served small reads has no measured bandwidth, which is
    /// not the same as having none.
    link_rate: Mutex<Option<f64>>,
}

impl LinkReads {
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
}

/// One part of one session, read through the hub.
struct LinkByteSource {
    reads: Arc<LinkReads>,
    session_id: String,
    part: u32,
    size: u64,
}

impl ByteSource for LinkByteSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, offset: u64, len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(async move {
            let req_id = self.reads.next_req.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.reads.pending.lock().unwrap().insert(req_id, tx);
            let sent = self
                .reads
                .link
                .send(TcToHub {
                    msg: Some(tc_to_hub::Msg::SourceRead(kahawai_proto::v1::SourceRead {
                        session_id: self.session_id.clone(),
                        offset,
                        len,
                        req: req_id,
                        part: self.part,
                    })),
                })
                .await;
            if sent.is_err() {
                self.reads.pending.lock().unwrap().remove(&req_id);
                return Err(std::io::Error::other("link closed"));
            }
            let started = std::time::Instant::now();
            match tokio::time::timeout(READ_TIMEOUT, rx).await {
                Ok(Ok(data)) => {
                    // Timed at the LEASE round trip, not the local write:
                    // this is what the source plane sustains for this box
                    // (HUB-36).
                    self.reads.fold_link_rate(data.len(), started.elapsed());
                    Ok(data)
                }
                Ok(Err(_)) | Err(_) => {
                    self.reads.pending.lock().unwrap().remove(&req_id);
                    Err(std::io::Error::other(format!(
                        "source read {req_id} unanswered"
                    )))
                }
            }
        })
    }
}

struct Session {
    run: Run,
    /// Reports a mid-session death to the hub (AR-6); aborted on end.
    death_watch: tokio::task::JoinHandle<()>,
}

/// Which starts of a session id are still wanted.
///
/// A seek restart is `EndSession` then `StartSession` for the same id,
/// and the hub does not wait for the old run to be gone. A start that is
/// still prerolling when its end arrives — or when a newer start for the
/// same id arrives — must not register itself when it finally becomes
/// ready, and must not answer the hub with a verdict the hub will read
/// against the NEWER start. Generations make that decidable: every start
/// takes the next number after it has ended its predecessor, every end
/// records the latest number it saw, and a start whose number is at or
/// below the last end is stale.
#[derive(Default, Clone, Copy)]
struct Generations {
    latest: u64,
    ended: u64,
}

#[derive(Default)]
struct Starting {
    /// Set by the link teardown; no start is admitted after it.
    closed: bool,
    pending: HashMap<String, (tokio::task::Id, tokio::task::AbortHandle)>,
}

/// All state for one hub link's dispatched sessions.
pub struct Runner {
    executor: Executor,
    link: mpsc::Sender<TcToHub>,
    reads: Arc<LinkReads>,
    sessions: Mutex<HashMap<String, Session>>,
    generations: Mutex<HashMap<String, Generations>>,
    /// Start admission. A start registers in `sessions` only once its
    /// playlist is ready, so an EndSession or a link teardown that arrives
    /// while it prerolls has to find it here to cancel it — otherwise the
    /// generation keeps running, registers after the teardown and answers
    /// a hub that is gone. The task id tells `end` apart from the start it
    /// is running inside of.
    ///
    /// One lock for both halves on purpose: `end_all` closes admission and
    /// drains the pending starts under it, and `start` admits — spawns and
    /// records — under it, so a `StartSession` the link loop had already
    /// received when the link died can never slip between the two.
    starting: Mutex<Starting>,
    /// HUB-36: pace samples harvested from ended runs, waiting for the
    /// next heartbeat tick to carry them.
    pending_pace: Mutex<Vec<PaceSample>>,
}

impl Runner {
    pub fn new(
        scratch_root: PathBuf,
        worker_exe: Option<PathBuf>,
        link: mpsc::Sender<TcToHub>,
    ) -> Arc<Self> {
        Arc::new(Self {
            // Sessions never survive a link; stale scratch is garbage,
            // and the executor sweeps it.
            executor: Executor::new(scratch_root, worker_exe),
            link: link.clone(),
            reads: Arc::new(LinkReads {
                link,
                pending: Mutex::new(HashMap::new()),
                next_req: AtomicU64::new(1),
                link_rate: Mutex::new(None),
            }),
            sessions: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            starting: Mutex::new(Starting::default()),
            pending_pace: Mutex::new(Vec::new()),
        })
    }

    /// Run a `StartSession`: answer with `SessionReady` and the preroll's
    /// facts (AR-13), or `SessionError` with the worker's log — or nothing
    /// at all when the start is cancelled by an end or a teardown before it
    /// is ready, since the hub that asked has already forgotten it.
    ///
    /// Synchronous: the start is spawned AND recorded before this returns,
    /// under the admission lock, so a teardown either sees it in the table
    /// or has closed admission before it got here. There is no window in
    /// which a start exists that `end_all` cannot reach.
    pub fn start(self: &Arc<Self>, msg: StartSession) {
        let session_id = msg.session_id.clone();
        let mut starting = self.starting.lock().unwrap();
        if starting.closed {
            tracing::info!(session = %session_id, "start refused: the link is being torn down");
            return;
        }
        let runner = self.clone();
        let task = tokio::spawn(async move {
            runner.start_inner(msg).await;
            // Forget ourselves, unless a newer start for the same id has
            // taken the slot meanwhile.
            let own = tokio::task::try_id();
            let mut starting = runner.starting.lock().unwrap();
            starting.pending.retain(|_, (task, _)| Some(*task) != own);
        });
        starting
            .pending
            .insert(session_id, (task.id(), task.abort_handle()));
    }

    async fn start_inner(self: &Arc<Self>, msg: StartSession) {
        let session_id = msg.session_id.clone();
        let job = match Job::from_start_session(&msg) {
            Ok(job) => job,
            Err(e) => {
                let _ = self
                    .link
                    .send(session_error(&session_id, format!("{e:#}"), String::new()))
                    .await;
                return;
            }
        };
        // Replace any previous run first (seek-restart reuses the id),
        // then take the generation that outlives that end.
        self.end(&session_id).await;
        let generation = {
            let mut generations = self.generations.lock().unwrap();
            let g = generations.entry(session_id.clone()).or_default();
            g.latest += 1;
            g.latest
        };
        let sources: Vec<Arc<dyn ByteSource>> = job
            .part_sizes
            .iter()
            .enumerate()
            .map(|(part, size)| {
                Arc::new(LinkByteSource {
                    reads: self.reads.clone(),
                    session_id: session_id.clone(),
                    part: part as u32,
                    size: *size,
                }) as Arc<dyn ByteSource>
            })
            .collect();
        let started = match self.executor.start(&session_id, job, sources).await {
            Ok(started) => started,
            Err(failure) => {
                tracing::warn!(session = %session_id, error = %failure, "session failed");
                let _ = self
                    .link
                    .send(session_error(
                        &session_id,
                        failure.to_string(),
                        failure.worker_log,
                    ))
                    .await;
                return;
            }
        };
        let stale = self
            .generations
            .lock()
            .unwrap()
            .get(&session_id)
            .is_some_and(|g| g.ended >= generation);
        if stale {
            // The hub ended this start — or replaced it — while it was
            // prerolling. Say nothing: any verdict now would be read
            // against the newer start.
            tracing::info!(session = %session_id, "start superseded before it was ready");
            let ended = started.run.end("satellite").await;
            self.keep_pace(&session_id, ended.pace);
            return;
        }
        let facts: Vec<SessionFact> = started
            .facts
            .into_iter()
            .map(|f| SessionFact {
                kind: f.kind,
                detail: f.detail,
            })
            .collect();
        let death_watch = self.watch_death(&session_id, started.run.died());
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Session {
                run: started.run,
                death_watch,
            },
        );
        tracing::info!(session = %session_id, facts = facts.len(), "session ready");
        let _ = self
            .link
            .send(TcToHub {
                msg: Some(tc_to_hub::Msg::SessionReady(SessionReady {
                    session_id,
                    facts,
                })),
            })
            .await;
    }

    /// Post-ready supervision: a worker dying mid-session becomes a
    /// SessionError so the hub can reschedule (AR-6). EOS is not a death.
    fn watch_death(
        &self,
        session_id: &str,
        mut died: tokio::sync::watch::Receiver<Option<Death>>,
    ) -> tokio::task::JoinHandle<()> {
        let link = self.link.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            loop {
                let death = died.borrow().clone();
                match death {
                    Some(Death::Finished) => return,
                    Some(Death::Crashed { error, worker_log }) => {
                        tracing::warn!(session = %session_id, "worker died mid-session");
                        let _ = link
                            .send(session_error(&session_id, error, worker_log))
                            .await;
                        return;
                    }
                    None => {
                        if died.changed().await.is_err() {
                            return; // the run is gone: ended by the hub
                        }
                    }
                }
            }
        })
    }

    fn keep_pace(&self, session_id: &str, sample: Option<f32>) {
        if let Some(multiple) = sample {
            self.pending_pace.lock().unwrap().push(PaceSample {
                session_id: session_id.to_string(),
                multiple,
            });
        }
    }

    /// Drain what the next heartbeat should carry: every live run's pace
    /// sample if it has written one, the samples ended runs left behind,
    /// and the link rate. None when there is nothing to say — an idle box
    /// should not add traffic to its own keepalive.
    pub fn take_pace_report(&self) -> Option<PaceReport> {
        let mut samples = std::mem::take(&mut *self.pending_pace.lock().unwrap());
        for (id, session) in self.sessions.lock().unwrap().iter() {
            if let Some(multiple) = session.run.take_pace_sample() {
                samples.push(PaceSample {
                    session_id: id.clone(),
                    multiple,
                });
            }
        }
        let rate = *self.reads.link_rate.lock().unwrap();
        if samples.is_empty() && rate.is_none() {
            return None;
        }
        Some(PaceReport {
            samples,
            link_bytes_per_sec: rate.unwrap_or(0.0) as u64,
        })
    }

    /// Hub answered a source read. Stale responses (request id no
    /// longer pending — e.g. from a worker a seek-restart replaced) are
    /// dropped on the floor.
    pub fn source_data(&self, req: u64, data: Vec<u8>) {
        if let Some(tx) = self.reads.pending.lock().unwrap().remove(&req) {
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
            .map(|s| s.run.dir().join(name));
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

    /// Pacing: persist the viewer's position where this session's worker
    /// polls it.
    pub fn viewer_position(&self, session_id: &str, position_ms: u64) {
        if let Some(s) = self.sessions.lock().unwrap().get(session_id) {
            s.run.viewer_position(position_ms);
        }
    }

    /// End one run, WAIT for its pipeline to be truly gone, ship its
    /// bundle (OPS-10) and remove its scratch. A start of this id that has
    /// not registered yet is marked stale and ends itself when it is.
    pub async fn end(&self, session_id: &str) {
        {
            let mut generations = self.generations.lock().unwrap();
            if let Some(g) = generations.get_mut(session_id) {
                g.ended = g.latest;
            }
        }
        // A start still prerolling is cancelled outright: dropping its
        // future drops the run, and the run kills its worker. Not when the
        // caller IS that start — it ends its predecessor before it begins.
        let pending = {
            let mut starting = self.starting.lock().unwrap();
            match starting.pending.get(session_id) {
                Some((task, _)) if Some(*task) == tokio::task::try_id() => None,
                Some(_) => starting.pending.remove(session_id),
                None => None,
            }
        };
        if let Some((_, handle)) = pending {
            handle.abort();
            settle(&handle).await;
        }
        let Some(session) = self.sessions.lock().unwrap().remove(session_id) else {
            return;
        };
        session.death_watch.abort();
        let ended = session.run.end("satellite").await;
        self.keep_pace(session_id, ended.pace);
        let _ = self
            .link
            .send(TcToHub {
                msg: Some(tc_to_hub::Msg::SessionLogs(
                    kahawai_proto::v1::SessionLogs {
                        session_id: session_id.to_string(),
                        body: ended.bundle,
                    },
                )),
            })
            .await;
    }

    /// OPS-10: this running session's diagnostics, on request. Returns
    /// None when the session is not ours — an ended one is already
    /// stored hub-side, so there is nothing useful to answer with.
    pub fn collect_logs(&self, session_id: &str) -> Option<String> {
        let dir = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id)?.run.dir().to_path_buf()
        };
        Some(kahawai_playback::bundle::gather(
            "satellite",
            session_id,
            &dir,
        ))
    }

    /// Link died: every session dies with it (the hub reschedules, AR-6),
    /// the ones still prerolling included — they are cancelled and waited
    /// for, so the reconnect that follows can sweep the scratch root with
    /// nothing still writing into it.
    pub async fn end_all(&self) {
        let pending: Vec<_> = {
            let mut starting = self.starting.lock().unwrap();
            starting.closed = true;
            starting.pending.drain().collect()
        };
        for (_, (task, handle)) in pending {
            if Some(task) != tokio::task::try_id() {
                handle.abort();
                settle(&handle).await;
            }
        }
        for g in self.generations.lock().unwrap().values_mut() {
            g.ended = g.latest;
        }
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.end(&id).await;
        }
    }
}

/// Wait, briefly, for an aborted start to actually be dropped: abort only
/// schedules the cancellation, and the run's worker dies when the future
/// does. Bounded, because a start blocked on a blocking pipeline call
/// cannot be interrupted until that call returns.
async fn settle(handle: &tokio::task::AbortHandle) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn session_error(session_id: &str, error: String, worker_log: String) -> TcToHub {
    TcToHub {
        msg: Some(tc_to_hub::Msg::SessionError(SessionError {
            session_id: session_id.to_string(),
            error,
            worker_log,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads() -> (Arc<LinkReads>, mpsc::Receiver<TcToHub>) {
        let (tx, rx) = mpsc::channel(4);
        (
            Arc::new(LinkReads {
                link: tx,
                pending: Mutex::new(HashMap::new()),
                next_req: AtomicU64::new(1),
                link_rate: Mutex::new(None),
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn a_read_the_hub_drops_fails_the_worker_instead_of_hanging() {
        // The hub tore the session down (lease gone) and will never answer
        // this request: the read must come back as an error so the pipeline
        // errors out rather than waiting for ever.
        let (reads, mut rx) = reads();
        let source = LinkByteSource {
            reads: reads.clone(),
            session_id: "s".into(),
            part: 0,
            size: 100,
        };
        let read = tokio::spawn(async move { source.read(0, 16).await });
        let sent = rx.recv().await.unwrap();
        let Some(tc_to_hub::Msg::SourceRead(req)) = sent.msg else {
            panic!("expected a SourceRead");
        };
        // Dropping the pending sender is what a torn-down session looks
        // like from here: nobody will ever call `source_data` for it.
        reads.pending.lock().unwrap().remove(&req.req);
        assert!(read.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_closed_link_fails_the_read_and_releases_its_slot() {
        let (reads, rx) = reads();
        drop(rx);
        let source = LinkByteSource {
            reads: reads.clone(),
            session_id: "s".into(),
            part: 0,
            size: 100,
        };
        assert!(source.read(0, 16).await.is_err());
        assert!(
            reads.pending.lock().unwrap().is_empty(),
            "the pending slot is released"
        );
    }

    #[tokio::test]
    async fn an_answered_read_returns_the_hubs_bytes() {
        let (reads, mut rx) = reads();
        let source = LinkByteSource {
            reads: reads.clone(),
            session_id: "s".into(),
            part: 2,
            size: 100,
        };
        let read = tokio::spawn(async move { source.read(10, 4).await });
        let sent = rx.recv().await.unwrap();
        let Some(tc_to_hub::Msg::SourceRead(req)) = sent.msg else {
            panic!("expected a SourceRead");
        };
        assert_eq!((req.offset, req.len, req.part), (10, 4, 2));
        let tx = reads.pending.lock().unwrap().remove(&req.req).unwrap();
        tx.send(vec![1, 2, 3, 4]).unwrap();
        assert_eq!(read.await.unwrap().unwrap(), vec![1, 2, 3, 4]);
    }

    /// A stand-in for the `remux-worker` child that never produces a
    /// playlist: the start stays in preroll until something ends it.
    fn stalled_worker(dir: &std::path::Path) -> PathBuf {
        let script = dir.join("stalled-worker.sh");
        std::fs::write(&script, "#!/bin/sh\necho $$ > \"$3/pid\"\nexec sleep 300\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn worker_pid(scratch: &std::path::Path) -> u32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_dir(scratch)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|session| std::fs::read_dir(session.path()).ok())
                .flatten()
                .flatten()
                .find_map(|run| std::fs::read_to_string(run.path().join("pid")).ok())
            {
                return pid.trim().parse().unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fake worker never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn stalled_session() -> StartSession {
        StartSession {
            session_id: "s".into(),
            size: 1,
            video: "copy".into(),
            audio: "copy".into(),
            ..Default::default()
        }
    }

    async fn stalled_start(dir: &std::path::Path) -> (Arc<Runner>, mpsc::Receiver<TcToHub>, u32) {
        let (tx, rx) = mpsc::channel(8);
        let runner = Runner::new(dir.join("sessions"), Some(stalled_worker(dir)), tx);
        runner.start(stalled_session());
        let pid = worker_pid(&dir.join("sessions")).await;
        assert!(alive(pid));
        (runner, rx, pid)
    }

    async fn wait_dead(pid: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!alive(pid), "the worker of a cancelled start must die");
    }

    /// The regression an external review found: a link teardown ended only
    /// registered sessions, and registration happens after readiness, so a
    /// start still prerolling survived `end_all`, registered afterwards and
    /// answered a hub that was gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_all_cancels_a_start_still_prerolling() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, mut rx, pid) = stalled_start(dir.path()).await;
        runner.end_all().await;
        wait_dead(pid).await;
        assert!(runner.sessions.lock().unwrap().is_empty());
        assert!(runner.starting.lock().unwrap().pending.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            rx.try_recv().is_err(),
            "a cancelled start answers nobody: the hub that asked is gone"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_end_during_preroll_cancels_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, mut rx, pid) = stalled_start(dir.path()).await;
        runner.end("s").await;
        wait_dead(pid).await;
        assert!(runner.starting.lock().unwrap().pending.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            rx.try_recv().is_err(),
            "no verdict for a start the hub ended"
        );
    }

    /// The second finding of the same review: a `StartSession` the link
    /// loop had received but not yet acted on when the link died used to
    /// be spawned after `end_all`, register, and launch a worker for a hub
    /// that was gone. Admission closes with the teardown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_arriving_after_teardown_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let runner = Runner::new(
            dir.path().join("sessions"),
            Some(stalled_worker(dir.path())),
            tx,
        );
        runner.end_all().await;
        runner.start(stalled_session());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(runner.starting.lock().unwrap().pending.is_empty());
        assert!(runner.sessions.lock().unwrap().is_empty());
        let spawned = std::fs::read_dir(dir.path().join("sessions"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(spawned, 0, "no run directory: the worker was never spawned");
        assert!(
            rx.try_recv().is_err(),
            "nothing is said to a hub that is gone"
        );
    }

    #[test]
    fn small_reads_do_not_move_the_link_rate() {
        let (tx, _rx) = mpsc::channel(1);
        let reads = LinkReads {
            link: tx,
            pending: Mutex::new(HashMap::new()),
            next_req: AtomicU64::new(1),
            link_rate: Mutex::new(None),
        };
        reads.fold_link_rate(1024, std::time::Duration::from_millis(1));
        assert_eq!(*reads.link_rate.lock().unwrap(), None);
        reads.fold_link_rate(LINK_MIN_READ, std::time::Duration::from_secs(1));
        assert_eq!(*reads.link_rate.lock().unwrap(), Some(LINK_MIN_READ as f64));
        reads.fold_link_rate(LINK_MIN_READ, std::time::Duration::from_millis(500));
        let blended = reads.link_rate.lock().unwrap().unwrap();
        assert!(blended > LINK_MIN_READ as f64 && blended < 2.0 * LINK_MIN_READ as f64);
    }
}
