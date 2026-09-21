//! Run one pipeline through the executor and print what it left behind.
//!
//! In-process by default; `--worker-exe <path>` spawns the real
//! `remux-worker` child instead, which is the path every real session
//! takes (short sockets under /tmp, log capture, kill on end).
//!
//!     cargo run -p kahawai-playback --example playback_check
//!     cargo run -p kahawai-playback --example playback_check -- --worker-exe target/debug/kahawai

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use kahawai_media::remux::{RemuxPlan, StreamMode};
use kahawai_playback::executor::{BoxFuture, ByteSource, Executor};
use kahawai_playback::job::Job;

struct FileSource {
    path: PathBuf,
    size: u64,
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

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut worker_exe: Option<PathBuf> = None;
    let mut media: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--worker-exe" => {
                worker_exe = Some(args.next().context("--worker-exe wants a path")?.into())
            }
            "--media" => media = Some(args.next().context("--media wants a path")?.into()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let dir = tempfile::tempdir()?;
    let media = match media {
        Some(path) => path,
        None => {
            if !kahawai_media::testutil::require_h264_aac_fixture() {
                println!(
                    "SKIPPED: no x264enc/AAC encoder to render a fixture; pass --media <file>"
                );
                return Ok(());
            }
            let path = dir.path().join("fixture.mkv");
            kahawai_media::testutil::render_h264_aac_mkv(&path);
            path
        }
    };
    let size = std::fs::metadata(&media)?.len();
    let source: Arc<dyn ByteSource> = Arc::new(FileSource {
        path: media.clone(),
        size,
    });
    let executor = Executor::new(dir.path().join("sessions"), worker_exe.clone());
    let job = Job {
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
    };
    println!(
        "media: {} ({size} bytes); worker: {}",
        media.display(),
        worker_exe
            .as_deref()
            .map(Path::display)
            .map_or("in-process".to_string(), |p| p.to_string())
    );
    let started = match executor.start("check", job, vec![source]).await {
        Ok(started) => started,
        Err(failure) => {
            println!("{}", failure.bundle);
            anyhow::bail!("start failed: {failure}");
        }
    };
    println!("ready in {}", started.run.dir().display());
    for name in ["master.m3u8", "segment00000.ts", "start.pos"] {
        anyhow::ensure!(started.run.dir().join(name).exists(), "{name} missing");
    }
    if worker_exe.is_some() {
        anyhow::ensure!(
            std::fs::metadata(started.run.dir().join("worker.log"))?.len() > 0,
            "worker.log is empty: the child's output is not being captured"
        );
    }
    started.run.viewer_position(1000);
    let ended = started.run.end("playback_check").await;
    anyhow::ensure!(
        ended.stopped,
        "the worker did not stop inside the grace period"
    );
    anyhow::ensure!(
        !dir.path().join("sessions/check/r1").exists(),
        "run dir not removed"
    );
    println!("{}", ended.bundle);
    println!("OK");
    Ok(())
}
