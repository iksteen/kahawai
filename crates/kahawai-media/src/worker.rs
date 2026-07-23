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
pub fn run(
    socket: &Path,
    out_dir: &Path,
    size: u64,
    video: StreamMode,
    audio: StreamMode,
    start_ms: u64,
) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let plan = RemuxPlan { video, audio };
    let job = remux::start_at(out_dir, plan, Box::new(SocketSource { stream, size }), start_ms)?;
    while !job.finished() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Some(e) = job.failed() {
        anyhow::bail!("{e}");
    }
    Ok(())
}
