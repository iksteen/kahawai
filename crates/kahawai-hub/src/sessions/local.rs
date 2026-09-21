use super::*;

pub(super) fn short_worker_socket_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("kahawai-")
        .tempdir_in("/tmp")
        .context("creating short worker socket directory")
}

/// How a remux/transcode pipeline runs. The hub always spawns the
/// supervised worker (§1.1: a GStreamer crash kills one session, not the
/// process — observed twice on a real library via hlssink3 panics).
/// ponytail: in-process kept for tests, which have no worker binary.
pub enum RemuxRunner {
    InProcess(Arc<kahawai_media::remux::RemuxJob>),
    Worker {
        child: Mutex<tokio::process::Child>,
        /// Owns the short Unix-socket directory until the worker is gone.
        _socket_dir: tempfile::TempDir,
    },
    /// Placeholder while a seek-restart swaps runners.
    Stopped,
}

impl RemuxRunner {
    pub(super) fn stop(&self) {
        match self {
            RemuxRunner::InProcess(job) => job.stop(),
            RemuxRunner::Worker { child, .. } => {
                let _ = child.lock().unwrap().start_kill();
            }
            RemuxRunner::Stopped => {}
        }
    }

    /// Stop and wait until the pipeline is really gone — a seek-restart
    /// recreates the scratch dir, and a still-dying worker writing into
    /// it corrupts the new run.
    pub(super) async fn stop_and_wait(&self) {
        self.stop();
        if let RemuxRunner::Worker { child, .. } = self {
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

/// Serve RemuxSource reads to a worker over its Unix socket.
/// Wire format: see kahawai_media::worker.
pub(super) async fn serve_reads(
    mut conn: tokio::net::UnixStream,
    lease: Lease,
    size: u64,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = [0u8; 16];
    loop {
        if conn.read_exact(&mut req).await.is_err() {
            return Ok(()); // worker closed the socket (EOS or teardown)
        }
        let offset = u64::from_le_bytes(req[..8].try_into().unwrap());
        let len =
            u64::from_le_bytes(req[8..].try_into().unwrap()).min(kahawai_media::worker::MAX_READ);
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
/// What "enough runway" means, measured on real starts (2026-07-31,
/// Firefox): hls.js reveals new segments only on its EVENT-playlist
/// reloads (~every target-duration, 3 s), and production jitters (one
/// heavy GOP took 3.5 s against a ~2 s cadence) — so playback stalls
/// whenever buffered content dips under ~production-gap + reload ≈
/// 6.5 s before the encoder's lead has grown past it. Hand-off with
/// ~4 s of content buffered stalled twice; ~5 s stalled once at 12 s.
/// The gate is therefore CONTENT seconds, not a segment count — three
/// segments can be as little as 4 s when scene-cut keyframes shorten
/// them. ENDLIST (a finished short encode) is always ready.
pub(super) fn playlist_ready(path: &std::path::Path, target_secs: u32) -> bool {
    // The 6.5 s floor was derived against a declared target of 2: a
    // client reloads about every target duration, so the runway it
    // needs is production-gap PLUS one reload interval. Once the
    // declaration follows the source — 12 s for a 10 s-GOP file — a
    // fixed 6.5 s hands over less than the client's own refresh period
    // and stalls on the first gap. §6.3.3 pushes the same way: a client
    // SHOULD NOT start within three target durations of the end.
    //
    // CAPPED so this gate can never be what times a start out. A 66 s
    // GOP would ask for 204 s of content, which is more than the 30 s
    // start deadline can produce however fast the source reads.
    //
    // Past the cap the server can do nothing useful anyway: a client
    // that honours §6.3.3 waits on its own account whatever we hand
    // it, and one that does not is ready now — so waiting longer here
    // only converts the client's wait into our timeout.
    //
    // Not the cure for extreme sources, and it should not be mistaken
    // for one: a file whose keyframes are 66 s apart cannot close a
    // single segment inside the deadline no matter what this returns,
    // because one segment IS one GOP. Those need `short`, which
    // re-encodes and gives them keyframes of our own choosing.
    const MAX_RUNWAY_SECS: f64 = 30.0;
    let need = (6.5f64).max((3.0 * target_secs as f64).min(MAX_RUNWAY_SECS));
    match std::fs::read_to_string(path) {
        Ok(p) => p.contains("#EXT-X-ENDLIST") || playlist_span_secs(&p) >= need,
        Err(_) => false,
    }
}

/// Total seconds of content a playlist advertises (Σ EXTINF).
pub(super) fn playlist_span_secs(playlist: &str) -> f64 {
    playlist
        .lines()
        .filter_map(|l| {
            l.strip_prefix("#EXTINF:")?
                .trim_end_matches(',')
                .parse::<f64>()
                .ok()
        })
        .sum()
}

/// The hub-local worker's half of a session bundle (OPS-10). The
/// satellite's equivalent lives in kahawai-transcoder; this one is
/// deliberately separate rather than shared, because the two read
/// different directory layouts and sharing would mean a crate
/// dependency purely for a string builder.
pub(super) fn local_bundle(dir: &std::path::Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "== hub-local worker\nrun dir: {}", dir.display());
    let segs = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("segment"))
                .count()
        })
        .unwrap_or(0);
    let _ = writeln!(out, "segments: {segs}");
    for name in ["start.pos", "viewer.pos", "pace.json", "facts.jsonl"] {
        if let Ok(body) = std::fs::read_to_string(dir.join(name)) {
            let _ = writeln!(out, "\n== {name}\n{}", body.trim_end());
        }
    }
    if let Ok(log) = std::fs::read_to_string(dir.join("worker.log")) {
        let _ = writeln!(out, "\n== worker.log\n{log}");
    }
    out
}

impl Sessions {
    /// Spin up the remux/transcode pipeline — in a supervised worker
    /// process when configured — feed it from the lease, and wait for the
    /// playlist to materialize so the returned URL is immediately playable.
    #[allow(clippy::too_many_arguments)] // the session's shape, spelled out
    pub(super) async fn start_remux(
        &self,
        session_id: &str,
        plan: kahawai_media::remux::RemuxPlan,
        // What the playlist will DECLARE (not what the sink cuts on):
        // the readiness gate hands over enough runway for a client
        // reloading at that cadence, so it has to know the number.
        target_secs: u32,
        parts: Vec<(Lease, u64)>,
        start_ms: u64,
        sink: &str,
        // HUB-32b: display sets the mediahost walked for us.
        sets: Option<&std::path::Path>,
        // HUB-32a: a sidecar `.ass` script to burn, as TEXT rather than
        // a path — this function wipes and recreates the session dir,
        // so anything written beforehand would not survive. Embedded
        // ASS needs nothing here: it burns from the demuxer's own pad.
        ass: Option<&str>,
    ) -> Result<(RemuxRunner, Vec<kahawai_media::facts::Fact>)> {
        let dir = self.scratch_root.join(session_id);
        // ALWAYS from a clean dir: a crashed first attempt leaves its
        // socket (EADDRINUSE killed the TC-6 fallback) and a stale
        // playlist the readiness check would mistake for output.
        self.keep_local_logs(session_id, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        kahawai_core::private::create_dir(&dir)
            .with_context(|| format!("creating private session dir {}", dir.display()))?;
        anyhow::ensure!(!parts.is_empty(), "no source parts for the session");
        let ass_path = match ass {
            Some(text) => {
                let p = dir.join("burn.ass");
                std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
                Some(p)
            }
            None => None,
        };

        let runner = match &self.worker_exe {
            Some(exe) => {
                // SUN_LEN caps Unix socket paths at roughly 108 bytes. macOS's
                // ordinary temp root already consumes most of that before the
                // session ULID and socket name are added, so keep only these
                // private transport sockets under the short /tmp spelling.
                // Output and diagnostics remain in the configured data dir.
                let socket_dir = short_worker_socket_dir()?;
                // One socket per part: the worker joins them with concat
                // into a single pipeline, so a CD1->CD2 boundary is not a
                // restart. Part one keeps the historical name and the
                // positional argument; the rest arrive as --part.
                let mut socks = Vec::with_capacity(parts.len());
                for (n, (lease, size)) in parts.iter().enumerate() {
                    let sock = if n == 0 {
                        socket_dir.path().join("worker.sock")
                    } else {
                        socket_dir.path().join(format!("worker{n}.sock"))
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
                // Who to die with. The worker compares this against its
                // own getppid(); see the guard in run_remux_worker.
                cmd.args(["--supervisor-pid", &std::process::id().to_string()]);
                for (sock, size) in &socks[1..] {
                    cmd.args(["--part", &format!("{}:{size}", sock.display())]);
                }
                for (flag, v) in [
                    ("--video-kbps", plan.video_kbps),
                    ("--max-height", plan.max_height),
                    ("--max-channels", plan.max_channels),
                ] {
                    if let Some(v) = v {
                        cmd.args([flag, &v.to_string()]);
                    }
                }
                if let Some(gain) = plan.stereo_gain_db {
                    cmd.args(["--stereo-gain-db", &gain.to_string()]);
                }
                if let Some(gain) = plan.native_gain_db {
                    cmd.args(["--native-gain-db", &gain.to_string()]);
                }
                if let Some(channels) = plan.loudness_source_channels {
                    cmd.args(["--loudness-source-channels", &channels.to_string()]);
                }
                if plan.loudness_gains.iter().any(Option::is_some) {
                    let gains = plan
                        .loudness_gains
                        .iter()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>();
                    cmd.args(["--loudness-gains", &serde_json::to_string(&gains)?]);
                }
                if plan.deinterlace {
                    cmd.arg("--deinterlace");
                }
                if plan.tone_map {
                    cmd.arg("--tone-map");
                }
                if let Some(n) = plan.burn_subtitle {
                    cmd.args(["--burn-sub", &n.to_string()]);
                }
                if let Some(p) = &sets {
                    cmd.args(["--burn-sets", &p.to_string_lossy()]);
                }
                if let Some(n) = plan.burn_ass {
                    cmd.args(["--burn-ass", &n.to_string()]);
                }
                if let Some(p) = &ass_path {
                    cmd.args(["--burn-ass-file", &p.to_string_lossy()]);
                }
                let child = cmd
                    .args(["--video", kahawai_media::worker::mode_arg(plan.video)])
                    .args(["--audio", kahawai_media::worker::mode_arg(plan.audio)])
                    .args(["--video-codec", plan.video_codec.as_str()])
                    .args(["--audio-codec", plan.audio_codec.as_str()])
                    .args(["--container", plan.segment_format.as_str()])
                    .args(["--audio-track", &plan.audio_track.to_string()])
                    .args(["--video-track", &plan.video_track.to_string()])
                    .args(["--start-ms", &start_ms.to_string()])
                    .args(if sink.is_empty() {
                        vec![]
                    } else {
                        vec!["--sink".into(), sink.to_string()]
                    })
                    // BOTH streams. The dispatched path (transcoder
                    // sessions.rs) learned this the hard way and this
                    // copy never got the fix: `tracing_subscriber` writes
                    // to STDOUT, so capturing stderr alone kept the
                    // GStreamer C-side output and Rust panics — which is
                    // why crash capture looked fine — while every
                    // `tracing::info!` the worker emitted went to the
                    // detached parent's stdout and was discarded. A
                    // locally-remuxed session's worker.log was 0 bytes
                    // for its whole life, and OPS-10 bundled that.
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
                RemuxRunner::Worker {
                    child: Mutex::new(child),
                    _socket_dir: socket_dir,
                }
            }
            None => {
                // In-process (tests): the pipeline pulls (and seeks —
                // MP4 moov-at-end needs it) from the lease via a blocking
                // adapter on the remux feeder thread.
                let handle = tokio::runtime::Handle::current();
                let sources: Vec<Box<dyn kahawai_media::remux::RemuxSource>> = parts
                    .into_iter()
                    .map(|(lease, size)| {
                        Box::new(LeaseSource {
                            lease,
                            size,
                            handle: handle.clone(),
                            reads: 0,
                        }) as Box<dyn kahawai_media::remux::RemuxSource>
                    })
                    .collect();
                // start_at blocks while prerolling for an offset seek —
                // off the async runtime with it, or the preroll's own
                // lease reads can never be driven (single-thread runtimes
                // deadlock outright).
                let dir2 = dir.clone();
                let sink_owned = (!sink.is_empty()).then(|| sink.to_string());
                let sets_owned = sets.map(|p| p.to_path_buf());
                let ass_owned = ass_path.clone();
                let job = tokio::task::spawn_blocking(move || {
                    kahawai_media::remux::start_parts(
                        &dir2,
                        plan,
                        sources,
                        start_ms,
                        sink_owned.as_deref(),
                        None,
                        sets_owned.as_deref(),
                        ass_owned.as_deref(),
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
                RemuxRunner::Worker { child, .. } => {
                    if let Some(status) = child.lock().unwrap().try_wait()? {
                        // A CLEAN exit is a pipeline that FINISHED: an
                        // all-copy remux of short content completes in
                        // under a second — faster than this poll. The
                        // playlist (with ENDLIST) is the product; fall
                        // through to the ready-check below instead of
                        // declaring death. Only a non-zero exit, or a
                        // clean exit with nothing produced, is failure.
                        if !(status.success() && playlist_ready(&playlist, target_secs)) {
                            let log =
                                std::fs::read_to_string(dir.join("worker.log")).unwrap_or_default();
                            let tail: String =
                                log.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                            // Keep the stderr BEFORE the retry wipes this
                            // dir: a panic's message names the file and
                            // line, and the four lines quoted below are
                            // the frames after it, which name nothing.
                            if let Some(data_dir) = self.scratch_root.parent() {
                                // OPS-10: this session is NOT in `active`
                                // — registration happens after start
                                // succeeds — which is why note_session
                                // ran when the id was minted, and why
                                // this bundle is the only trace it ever
                                // existed.
                                let _ = &log;
                                let (item, header) = self.log_header(session_id);
                                let body = format!("{header}{}", local_bundle(&dir));
                                crate::sessionlog::store(data_dir, &item, session_id, &body);
                            }
                            bail!("pipeline worker exited at start ({status}): {tail}");
                        }
                    }
                }
                RemuxRunner::Stopped => unreachable!("start_remux never yields Stopped"),
            }
            if playlist_ready(&playlist, target_secs) {
                return Ok((runner, kahawai_media::facts::read(&dir)));
            }
            if std::time::Instant::now() > deadline {
                runner.stop();
                bail!("remux produced no playlist in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
mod worker_socket_tests {
    use std::os::unix::ffi::OsStrExt;

    use super::short_worker_socket_dir;

    #[test]
    fn worker_socket_directory_leaves_sun_len_headroom() {
        let dir = short_worker_socket_dir().unwrap();
        let socket = dir.path().join("worker999.sock");
        assert!(socket.as_os_str().as_bytes().len() <= 64, "{socket:?}");
    }
}
