//! Supervised pipeline worker (§1.1, §6): the remux/transcode pipeline
//! runs in a child process so a GStreamer crash (hostile input, plugin
//! bug — both observed on a real library) kills one session, not the
//! hub. The parent serves source bytes over a Unix socket.
//!
//! Wire format, child → parent per read: 16 LE bytes (offset u64,
//! len u64); parent replies 8 LE bytes n, then n bytes. n = 0 → EOF.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};

use crate::remux::{self, RemuxPlan, RemuxSource, StreamMode};

/// Cap on a single read request, both sides (sanity, not throughput —
/// the remux feeder already reads in ≤4 MiB chunks).
pub const MAX_READ: u64 = 8 * 1024 * 1024;

struct SocketSource {
    stream: UnixStream,
    size: u64,
}

impl RemuxSource for SocketSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut req = [0u8; 16];
        req[..8].copy_from_slice(&offset.to_le_bytes());
        req[8..].copy_from_slice(&(buf.len() as u64).to_le_bytes());
        self.stream.write_all(&req)?;
        let mut hdr = [0u8; 8];
        self.stream.read_exact(&mut hdr)?;
        let n = u64::from_le_bytes(hdr) as usize;
        if n > buf.len() {
            return Err(std::io::Error::other("oversized read response"));
        }
        self.stream.read_exact(&mut buf[..n])?;
        Ok(n)
    }
}

pub fn parse_mode(s: &str) -> StreamMode {
    match s {
        "copy" => StreamMode::Copy,
        "encode" => StreamMode::Encode,
        _ => StreamMode::Off,
    }
}

pub fn mode_arg(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Copy => "copy",
        StreamMode::Encode => "encode",
        StreamMode::Off => "off",
    }
}

/// Child entry point: connect to the parent's socket, run the pipeline
/// to EOS, exit. Errors (including pipeline errors) return Err — the
/// binary maps that to a non-zero exit the supervisor can see.
#[allow(clippy::too_many_arguments)] // CLI-shaped plumbing
pub fn run(
    socket: &Path,
    out_dir: &Path,
    size: u64,
    plan: RemuxPlan,
    start_ms: u64,
    sink: Option<&str>,
) -> Result<()> {
    run_parts(
        &[(socket.to_path_buf(), size)],
        out_dir,
        plan,
        start_ms,
        sink,
        None,
    )
}

/// Multi-part entry point: one socket per part, in timeline order, joined
/// into a single pipeline (see `remux::start_parts`). `start_ms` applies
/// to the first part; the rest play whole.
#[allow(clippy::too_many_arguments)] // CLI-shaped plumbing
pub fn run_parts(
    parts: &[(std::path::PathBuf, u64)],
    out_dir: &Path,
    plan: RemuxPlan,
    start_ms: u64,
    sink: Option<&str>,
    burn_sets: Option<&Path>,
) -> Result<()> {
    anyhow::ensure!(!parts.is_empty(), "no parts given");
    let mut sources: Vec<Box<dyn RemuxSource>> = Vec::with_capacity(parts.len());
    for (socket, size) in parts {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        sources.push(Box::new(SocketSource {
            stream,
            size: *size,
        }));
    }
    // Pacing window (§4.6): transcode ahead of the viewer, but not the
    // whole film. The supervisor keeps `viewer.pos` fresh (absolute ms,
    // from the client's progress pings); muxer-bound buffers beyond
    // viewer+window block in-band until the viewer catches up.
    // ponytail: fixed window, env-tunable; per-session config later.
    let pace = remux::PaceConfig {
        window_ms: std::env::var("KAHAWAI_PACE_WINDOW_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120_000),
        floor_ms: start_ms,
        viewer_file: out_dir.join("viewer.pos"),
    };
    let job = remux::start_parts(
        out_dir,
        plan,
        sources,
        start_ms,
        sink,
        Some(pace),
        burn_sets,
    )?;
    while !job.finished() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Some(e) = job.failed() {
        anyhow::bail!("{e}");
    }
    Ok(())
}
