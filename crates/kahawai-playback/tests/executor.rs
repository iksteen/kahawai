//! The executor against a real pipeline, in-process: the run-directory
//! contract, readiness, restarts and endings. Skips where the local
//! GStreamer stack cannot render the fixture.

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kahawai_media::remux::{RemuxPlan, StreamMode};
use kahawai_media::testutil;
use kahawai_playback::executor::{BoxFuture, ByteSource, Death, Executor};
use kahawai_playback::job::Job;

struct FileSource {
    path: PathBuf,
    size: u64,
}

impl FileSource {
    fn open(path: &Path) -> Arc<dyn ByteSource> {
        let size = std::fs::metadata(path).unwrap().len();
        Arc::new(Self {
            path: path.to_path_buf(),
            size,
        })
    }
}

impl ByteSource for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read(&self, offset: u64, len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(async move {
            let file = std::fs::File::open(&self.path)?;
            let mut buf = vec![0u8; len as usize];
            let n = file.read_at(&mut buf, offset)?;
            buf.truncate(n);
            Ok(buf)
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    media: PathBuf,
    scratch: PathBuf,
}

fn fixture() -> Option<Fixture> {
    if !testutil::require_h264_aac_fixture() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("episode.mkv");
    testutil::render_h264_aac_mkv(&media);
    let scratch = dir.path().join("sessions");
    Some(Fixture {
        _dir: dir,
        media,
        scratch,
    })
}

fn copy_job(size: u64) -> Job {
    Job {
        plan: RemuxPlan {
            video: StreamMode::Copy,
            audio: StreamMode::Copy,
            ..RemuxPlan::default()
        },
        part_sizes: vec![size],
        start_ms: 0,
        sink: None,
        burn_sets: None,
        burn_ass: None,
        target_duration_secs: Some(2),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_process_run_becomes_ready_and_writes_the_contract_files() {
    let Some(f) = fixture() else { return };
    let executor = Executor::new(f.scratch.clone(), None);
    let source = FileSource::open(&f.media);
    let started = executor
        .start("s1", copy_job(source.size()), vec![source])
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", e.bundle));
    let run = started.run;
    assert!(run.dir().ends_with("s1/r1"), "{}", run.dir().display());
    for name in ["master.m3u8", "segment00000.ts", "start.pos"] {
        assert!(
            run.dir().join(name).exists(),
            "{name} missing after readiness"
        );
    }
    assert_eq!(run.session_id(), "s1");

    let ended = run.end("test").await;
    assert!(ended.stopped);
    assert!(
        ended.bundle.contains("== test: session s1"),
        "{}",
        ended.bundle
    );
    assert!(
        ended.bundle.contains("== master.m3u8 (tail)"),
        "{}",
        ended.bundle
    );
    assert!(ended.bundle.contains("== start.pos"), "{}", ended.bundle);
    assert!(ended.bundle.contains("first segment: "), "{}", ended.bundle);
    assert!(
        !f.scratch.join("s1/r1").exists(),
        "a stopped run's directory is removed"
    );
    assert!(
        !f.scratch.join("s1").exists(),
        "and the session directory with it, once empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seek_restart_gets_a_fresh_run_dir() {
    let Some(f) = fixture() else { return };
    let executor = Executor::new(f.scratch.clone(), None);
    let source = FileSource::open(&f.media);
    let first = executor
        .start("s1", copy_job(source.size()), vec![source.clone()])
        .await
        .unwrap();
    assert!(first.run.dir().ends_with("r1"));
    first.run.end("test").await;
    let second = executor
        .start("s1", copy_job(source.size()), vec![source])
        .await
        .unwrap();
    assert!(
        second.run.dir().ends_with("r2"),
        "{}",
        second.run.dir().display()
    );
    assert!(!f.scratch.join("s1/r1").exists());
    second.run.end("test").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_position_lands_in_the_run_dir() {
    let Some(f) = fixture() else { return };
    let executor = Executor::new(f.scratch.clone(), None);
    let source = FileSource::open(&f.media);
    let started = executor
        .start("s1", copy_job(source.size()), vec![source])
        .await
        .unwrap();
    started.run.viewer_position(4321);
    assert_eq!(
        std::fs::read_to_string(started.run.dir().join("viewer.pos")).unwrap(),
        "4321"
    );
    started.run.end("test").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_run_reports_its_death_as_finished() {
    let Some(f) = fixture() else { return };
    let executor = Executor::new(f.scratch.clone(), None);
    let source = FileSource::open(&f.media);
    let started = executor
        .start("s1", copy_job(source.size()), vec![source])
        .await
        .unwrap();
    // A ten-second all-copy remux finishes well inside this.
    let mut died = started.run.died();
    let death = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(death) = died.borrow().clone() {
                return death;
            }
            died.changed().await.unwrap();
        }
    })
    .await
    .expect("the watcher reports the end of a finished run");
    assert_eq!(death, Death::Finished);
    assert!(started.run.is_finished());
    started.run.end("test").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_with_no_parts_is_refused_before_anything_is_spawned() {
    let dir = tempfile::tempdir().unwrap();
    let executor = Executor::new(dir.path().join("sessions"), None);
    let mut job = copy_job(1);
    job.part_sizes.clear();
    let failure = match executor.start("s1", job, Vec::new()).await {
        Err(failure) => failure,
        Ok(_) => panic!("a job with no parts started"),
    };
    assert!(failure.to_string().contains("no source parts"), "{failure}");
    assert!(!dir.path().join("sessions/s1").exists());
}

/// Thirty seconds of wall clock, so it runs from the check script rather
/// than every `cargo test`.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_never_answers_fails_the_start_inside_the_deadline() {
    struct Silent;
    impl ByteSource for Silent {
        fn size(&self) -> u64 {
            1 << 20
        }
        fn read(&self, _offset: u64, _len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
            Box::pin(std::future::pending())
        }
    }
    if !testutil::require_h264_aac_fixture() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let executor = Executor::new(dir.path().join("sessions"), None);
    let started = std::time::Instant::now();
    let outcome = executor
        .start("s1", copy_job(1 << 20), vec![Arc::new(Silent)])
        .await;
    assert!(outcome.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "{:?}",
        started.elapsed()
    );
}

/// A stand-in for the `remux-worker` child: records its pid, writes a
/// finished playlist so the start becomes ready, then lives until killed.
fn fake_worker(dir: &Path) -> PathBuf {
    let script = dir.join("fake-worker.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nout=\"$3\"\necho $$ > \"$out/pid\"\nprintf '#EXTM3U\\n#EXTINF:2.0,\\nsegment00000.ts\\n#EXT-X-ENDLIST\\n' > \"$out/master.m3u8\"\nexec sleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

struct OneByte;

impl ByteSource for OneByte {
    fn size(&self) -> u64 {
        1
    }
    fn read(&self, _offset: u64, _len: u64) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        Box::pin(async { Ok(vec![0]) })
    }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The regression an external review found: the watcher held the struct
/// that owns the child, so a dropped `Run` left its worker running until
/// EOS. Here the worker never reaches EOS on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_run_kills_its_worker() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_worker(dir.path());
    let executor = Executor::new(dir.path().join("sessions"), Some(script));
    let started = executor
        .start("s1", copy_job(1), vec![Arc::new(OneByte)])
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", e.bundle));
    let pid: u32 = std::fs::read_to_string(started.run.dir().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid), "the fake worker is running after readiness");

    drop(started);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "dropping the run must kill its worker");
}
